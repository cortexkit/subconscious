#![forbid(unsafe_code)]

use cortexkit_test_support::ScratchDir;
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    fs, io,
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, OnceLock,
    },
    time::Duration,
};

use serde_json::{json, Value};
use subc_client_rs::{
    async_trait, serve_with_handle, BindDecision, CallError, CallOptions, CatalogUpdateError,
    CloseRouteOptions, ConsumerIdentity, ConsumerOptions, HandlerOutcome, ModuleHandle,
    ModuleHandler, OutcomeUnknownCause, PolicyResolveError, PolicyResolver, PolicyResolverConfig,
    PolicyVerdict, ProjectRef, RequestCtx, RetryBackoff, ReverseRequestRegistrationError,
    RouteBindRequest, RouteCloseDisposition, RouteCloseReason, RouteEndReason, RouteHandle,
    ScopeCallError, ScopeDescribeReply, ScopeSelector, SubcConsumer, SubcModuleError, Subject,
    SubscribeOptions,
};
use subc_control::{ClientControlRequest, ClientControlResponse, PollKind};
use subc_protocol::{
    manifest::{
        CapabilityDeclarations, CapabilityNeed, CapabilityRequirement, Concurrency, ExecutionMode,
        IdentityScope, ManagementOperation, ManagementOperationKind, ModuleManifest, ProviderRole,
        Tool,
    },
    scope::{ScopeKind, ScopeStamp, ScopeStatus},
    session::HealthStatus,
    BindIdentity, ErrorBody, Flags, Frame, FrameType, Principal, Priority, RouteTarget,
};
use subc_transport::{authenticate_client, read_frame, write_frame};
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
    time::{sleep, timeout, Instant},
};

const MODULE_ID: &str = "subc-client-rs-echo";
const READ_TIMEOUT: Duration = Duration::from_secs(3);
const START_TIMEOUT: Duration = Duration::from_secs(10);
const EVENT_TIMEOUT: Duration = Duration::from_secs(5);
const AUTH_DEADLINE: Duration = Duration::from_secs(2);
const CONSUMER_MODULE_A: &str = "subc-client-rs-consumer-a";
const CONSUMER_MODULE_B: &str = "subc-client-rs-consumer-b";
const CATALOG_FAKE_AFT_MODULE_ID: &str = "subc-client-rs-catalog-fake-aft";
const PUSH_MODULE_ID: &str = "subc-client-rs-push";
const POLICY_MODULE_ID: &str = "subc-client-rs-policy-resolver";

struct LiveDaemon {
    child: Child,
    runtime_dir: PathBuf,
    config_dir: PathBuf,
    connection_file: PathBuf,
}

struct ProviderProcess {
    child: Child,
}

impl Drop for ProviderProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for LiveDaemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl LiveDaemon {
    fn kill_and_wait(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_file(&self.connection_file);
    }

    fn restart(&mut self, daemon_bin: &Path) {
        self.child = spawn_daemon_child(daemon_bin, &self.runtime_dir, &self.config_dir);
    }
}

struct EchoModuleHandler;

#[async_trait]
impl ModuleHandler for EchoModuleHandler {
    async fn handle(&self, _ctx: RequestCtx, body: Vec<u8>) -> HandlerOutcome {
        HandlerOutcome::Response(body)
    }
}

#[derive(Clone, Default)]
struct PushModuleHandler {
    routes: Arc<Mutex<HashMap<u16, RouteHandle>>>,
}

impl PushModuleHandler {
    fn route(&self, channel: u16) -> Option<RouteHandle> {
        self.routes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&channel)
            .copied()
    }
}

#[async_trait]
impl ModuleHandler for PushModuleHandler {
    async fn handle(&self, _ctx: RequestCtx, body: Vec<u8>) -> HandlerOutcome {
        HandlerOutcome::Response(body)
    }

    async fn on_bound(&self, handle: &RouteHandle) {
        self.routes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(handle.channel, *handle);
    }

    async fn on_route_gone(&self, handle: &RouteHandle) {
        let mut routes = self
            .routes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if routes.get(&handle.channel) == Some(handle) {
            routes.remove(&handle.channel);
        }
    }
}

#[derive(Clone)]
enum PolicyScript {
    Reply {
        verdict: &'static str,
        revision: u64,
        ttl_ms: u64,
    },
    Stall {
        duration: Duration,
    },
}

#[derive(Clone)]
struct PolicyModuleHandler {
    scripts: Arc<Mutex<VecDeque<PolicyScript>>>,
    calls: Arc<AtomicU64>,
    routes: Arc<Mutex<HashMap<u16, RouteHandle>>>,
    /// Queued revision bumps served over the HELD policy.subscribe stream --
    /// the live resolver's lane (StreamData on a held-open request), which the
    /// first cut faked as spontaneous route pushes its own helper accepted.
    bump_rx: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<u64>>>>,
    /// Shared across handler clones (serve holds one), so close_bumps() can
    /// end the held stream from the harness side: serve waits on the stream,
    /// stop waits on serve, and a per-clone sender would deadlock the pair.
    bump_tx: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<u64>>>>,
}

impl PolicyModuleHandler {
    fn new(scripts: impl IntoIterator<Item = PolicyScript>) -> Self {
        let (bump_tx, bump_rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            scripts: Arc::new(Mutex::new(scripts.into_iter().collect())),
            calls: Arc::new(AtomicU64::new(0)),
            routes: Arc::new(Mutex::new(HashMap::new())),
            bump_rx: Arc::new(Mutex::new(Some(bump_rx))),
            bump_tx: Arc::new(Mutex::new(Some(bump_tx))),
        }
    }

    /// Count of policy.resolve calls only; the held subscription is not a call.
    fn calls(&self) -> u64 {
        self.calls.load(Ordering::SeqCst)
    }

    /// The call count once it reaches `expected`, or whatever it is at the
    /// deadline. The counter increments when a request ARRIVES at this fake,
    /// which is a hop behind the resolver's own timeout: a resolve that faults
    /// at 200 ms against a stalling script has often not yet been counted when
    /// the caller reads `calls()`, and on a loaded runner "often" became
    /// "always" (Windows CI, 2026-09-19: read 1, expected 2). Waiting keeps
    /// the mutation fence intact -- a branch that never sends leaves the count
    /// short forever and the deadline reds -- without asserting on arrival
    /// latency the test does not control.
    async fn calls_reaching(&self, expected: u64, deadline: Duration) -> u64 {
        let started = Instant::now();
        loop {
            let calls = self.calls();
            if calls >= expected || started.elapsed() >= deadline {
                return calls;
            }
            sleep(Duration::from_millis(10)).await;
        }
    }

    fn send_bump(&self, revision: u64) {
        self.bump_tx
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .expect("bump channel open")
            .send(revision)
            .expect("bump channel open");
    }

    /// Close the bump lane so the held policy.subscribe stream ends; without
    /// this, harness.stop() deadlocks on a serve task waiting for a stream
    /// whose sender serve's own handler clone keeps alive.
    fn close_bumps(&self) {
        self.bump_tx
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
    }
}

#[async_trait]
impl ModuleHandler for PolicyModuleHandler {
    async fn handle(&self, ctx: RequestCtx, body: Vec<u8>) -> HandlerOutcome {
        let request: Value = serde_json::from_slice(&body).expect("policy request must be JSON");
        // The managed-call convention: {method, params} out, {result} back.
        // The fake mirrors the CONVENTION rather than the helper draft -- the
        // first cut of this handler accepted a flat body its own helper
        // invented, and the real resolver refused every live call.
        if request["method"] == "policy.subscribe" {
            // Serve the held bump stream until the test closes the channel.
            let Some(mut rx) = self
                .bump_rx
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take()
            else {
                return HandlerOutcome::Error {
                    code: "already_subscribed".to_string(),
                    message: "one held stream per harness".to_string(),
                };
            };
            loop {
                tokio::select! {
                    // Graceful stop cancels in-flight handlers; a held stream
                    // that ignores cancellation deadlocks harness.stop().
                    _ = ctx.cancelled() => break,
                    maybe = rx.recv() => {
                        let Some(revision) = maybe else { break };
                        // NESTED framing, pinned by the producer's push_event
                        // fixture entry (the flat form was the eighth drift).
                        let event = serde_json::to_vec(&json!({
                            "op": "policy.revision_bump",
                            "body": { "revision": revision },
                        }))
                        .unwrap();
                        if ctx.emit(event).await.is_err() {
                            break;
                        }
                    }
                }
            }
            return HandlerOutcome::Streamed;
        }
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(request["method"], "policy.resolve");
        let params = request.get("params").expect("params envelope");
        assert!(params.get("domain").is_some());
        assert!(params.get("gate_id").is_some());
        assert!(params.get("subject").is_some());
        assert!(params.get("project_root").is_some());

        let script = self
            .scripts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .pop_front()
            .expect("policy test received an unexpected wire call");
        match script {
            PolicyScript::Reply {
                verdict,
                revision,
                ttl_ms,
            } => HandlerOutcome::Response(
                serde_json::to_vec(&json!({
                    "result": {
                        "verdict": verdict,
                        "revision": revision,
                        "ttl_ms": ttl_ms,
                    }
                }))
                .unwrap(),
            ),
            PolicyScript::Stall { duration } => {
                sleep(duration).await;
                HandlerOutcome::Response(
                    serde_json::to_vec(&json!({
                        "verdict": "allow",
                        "revision": 1,
                        "ttl_ms": 1_000,
                    }))
                    .unwrap(),
                )
            }
        }
    }

    async fn on_bound(&self, handle: &RouteHandle) {
        self.routes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(handle.channel, *handle);
    }

    async fn on_route_gone(&self, handle: &RouteHandle) {
        let mut routes = self
            .routes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if routes.get(&handle.channel) == Some(handle) {
            routes.remove(&handle.channel);
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subc_consumer_catalog_list_reads_tool_provider_without_open_routes() {
    let workspace = workspace_root();
    let daemon_bin = ensure_binary(
        &workspace,
        &[
            "build",
            "--locked",
            "-p",
            "subc-core",
            "--bin",
            "ck-subc",
            "--message-format=json",
        ],
    );
    let fake_aft_bin = ensure_binary(
        &workspace,
        &[
            "build",
            "--locked",
            "-p",
            "subc-core",
            "--bin",
            "fake-aft-stub",
            "--message-format=json",
        ],
    );

    let temp_dir = unique_temp_dir("subc-client-rs-catalog-list");
    let runtime_dir = temp_dir.join("runtime");
    let config_dir = temp_dir.join("config");
    fs::create_dir_all(&runtime_dir).unwrap();
    fs::create_dir_all(config_dir.join("cortexkit")).unwrap();
    fs::write(
        config_dir.join("cortexkit").join("subc.jsonc"),
        fake_aft_stub_config_doc(&fake_aft_bin, CATALOG_FAKE_AFT_MODULE_ID),
    )
    .unwrap();

    let mut daemon = spawn_daemon(&daemon_bin, &runtime_dir, &config_dir);
    wait_for_connection_file(&daemon.connection_file, START_TIMEOUT).await;
    wait_for_catalog_module(
        &daemon.connection_file,
        CATALOG_FAKE_AFT_MODULE_ID,
        START_TIMEOUT,
    )
    .await;

    let consumer = SubcConsumer::connect(&daemon.connection_file, fast_consumer_options())
        .await
        .unwrap();
    let catalog = consumer.catalog_list().await.unwrap();
    let module = catalog
        .modules
        .iter()
        .find(|module| module.module_id == CATALOG_FAKE_AFT_MODULE_ID)
        .expect("catalog.list must include the fake-aft-stub module");
    let tools = module
        .roles
        .iter()
        .find_map(|role| match role {
            ProviderRole::ToolProvider { tools, .. } => Some(tools),
            _ => None,
        })
        .expect("fake-aft-stub must advertise a tool_provider role");
    let tool = tools
        .first()
        .expect("fake-aft-stub must advertise at least one tool");
    assert!(!tool.name.is_empty());
    assert!(tool.schema.is_object());

    daemon.kill_and_wait();
}

/// A route.open the daemon refuses surfaces its refusal body, `detail`
/// included, through `CallError::route_open_refusal`. Two paths: a terminal
/// refusal returns at once with its own body, and a retryable one that
/// outlasts the retry deadline returns the LAST refusal rather than a bare
/// "deadline elapsed", so a caller can tell a target that stayed warming from a
/// connection that failed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn route_open_refusal_keeps_the_daemons_code_and_detail() {
    let workspace = workspace_root();
    let daemon_bin = ensure_binary(
        &workspace,
        &[
            "build",
            "--locked",
            "-p",
            "subc-core",
            "--bin",
            "ck-subc",
            "--message-format=json",
        ],
    );
    let temp_dir = unique_temp_dir("subc-client-rs-route-open-refusal");
    let runtime_dir = temp_dir.join("runtime");
    let config_dir = temp_dir.join("config");
    fs::create_dir_all(&runtime_dir).unwrap();
    write_empty_config(&config_dir);

    let mut daemon = spawn_daemon(&daemon_bin, &runtime_dir, &config_dir);
    wait_for_connection_file(&daemon.connection_file, START_TIMEOUT).await;

    // Held not-ready by the daemon for as long as the test runs: it requires a
    // capability no registered module provides.
    let mut held = inline_module_manifest("refusal-held", &["capability.resolve"]);
    held.capabilities = Some(CapabilityDeclarations {
        provides: Vec::new(),
        requires: vec![CapabilityRequirement {
            capability: "refusal-nobody/v1".to_string(),
            need: CapabilityNeed::Required,
        }],
        must_never_reach: Vec::new(),
    });
    let (_held_module, held_task) = spawn_inline_module(&daemon.connection_file, held).await;
    wait_for_catalog_module(&daemon.connection_file, "refusal-held", START_TIMEOUT).await;

    let consumer = SubcConsumer::connect(&daemon.connection_file, fast_consumer_options())
        .await
        .unwrap();
    let identity = consumer_identity("route-open-refusal");

    let warming = consumer
        .call(
            tool_target("refusal-held"),
            identity.clone(),
            br#"{"kind":"unary","value":"held"}"#.to_vec(),
            // The deadline has to outlast at least one route.open round trip,
            // or no refusal arrives to be kept: at 600 ms a loaded Windows
            // runner timed out the first request itself. Every retry is refused
            // module_warming, so a longer deadline still ends on that refusal.
            CallOptions {
                route_retry_deadline: Duration::from_secs(3),
                ..fast_call_options()
            },
        )
        .await
        .unwrap_err();
    assert!(
        matches!(warming, CallError::NotSent(_)),
        "a refused route.open must stay NotSent, got {warming:?}"
    );
    let refusal = warming
        .route_open_refusal()
        .unwrap_or_else(|| panic!("the last refusal must survive the deadline: {warming}"));
    assert_eq!(refusal.code, "module_warming");
    assert_eq!(
        refusal.detail,
        Some(serde_json::json!({
            "reason": "required_capability_unprovided",
            "capability": "refusal-nobody/v1",
        }))
    );

    let absent = consumer
        .call(
            tool_target("refusal-never-registers"),
            identity,
            br#"{"kind":"unary","value":"missing"}"#.to_vec(),
            fast_call_options(),
        )
        .await
        .unwrap_err();
    assert!(matches!(absent, CallError::NotSent(_)), "got {absent:?}");
    assert_eq!(
        absent.route_open_refusal().map(|body| body.code.as_str()),
        Some("unknown_module")
    );

    held_task.abort();
    daemon.kill_and_wait();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn capability_resolver_does_not_fall_back_to_module_id_equality_and_orders_claimants() {
    let workspace = workspace_root();
    let daemon_bin = ensure_binary(
        &workspace,
        &[
            "build",
            "--locked",
            "-p",
            "subc-core",
            "--bin",
            "ck-subc",
            "--message-format=json",
        ],
    );
    let temp_dir = unique_temp_dir("subc-client-rs-capability-resolvers");
    let runtime_dir = temp_dir.join("runtime");
    let config_dir = temp_dir.join("config");
    fs::create_dir_all(&runtime_dir).unwrap();
    write_empty_config(&config_dir);

    let mut daemon = spawn_daemon(&daemon_bin, &runtime_dir, &config_dir);
    wait_for_connection_file(&daemon.connection_file, START_TIMEOUT).await;

    let (_fallback_module, fallback_task) = spawn_inline_module(
        &daemon.connection_file,
        inline_capability_module_manifest("fallback-only", None),
    )
    .await;
    let (_single_module, single_task) = spawn_inline_module(
        &daemon.connection_file,
        inline_capability_module_manifest("single-provider", Some("single-provider/v1")),
    )
    .await;
    let (_z_module, z_task) = spawn_inline_module(
        &daemon.connection_file,
        inline_capability_module_manifest("z-provider", Some("many-provider/v1")),
    )
    .await;
    let (_a_module, a_task) = spawn_inline_module(
        &daemon.connection_file,
        inline_capability_module_manifest("a-provider", Some("many-provider/v1")),
    )
    .await;
    for module_id in [
        "fallback-only",
        "single-provider",
        "z-provider",
        "a-provider",
    ] {
        wait_for_catalog_module(&daemon.connection_file, module_id, START_TIMEOUT).await;
    }
    let consumer = SubcConsumer::connect(&daemon.connection_file, fast_consumer_options())
        .await
        .unwrap();
    assert_eq!(
        consumer
            .resolve_providers("missing-provider/v1")
            .await
            .unwrap(),
        Vec::<String>::new()
    );
    let missing = consumer
        .resolve_provider("missing-provider/v1")
        .await
        .unwrap_err();
    assert_eq!(missing.code(), Some("capability_unprovided"));
    assert!(matches!(
        missing,
        CallError::CapabilityUnprovided { capability } if capability == "missing-provider/v1"
    ));
    assert_eq!(
        consumer
            .resolve_provider("single-provider/v1")
            .await
            .unwrap(),
        "single-provider"
    );
    assert_eq!(
        consumer
            .resolve_providers("many-provider/v1")
            .await
            .unwrap(),
        vec!["a-provider".to_string(), "z-provider".to_string()]
    );
    let ambiguous = consumer
        .resolve_provider("many-provider/v1")
        .await
        .unwrap_err();
    assert_eq!(ambiguous.code(), Some("capability_ambiguous"));
    assert!(matches!(
        ambiguous,
        CallError::CapabilityAmbiguous { capability, claimants }
            if capability == "many-provider/v1"
                && claimants == vec!["a-provider".to_string(), "z-provider".to_string()]
    ));
    // The module id equals the capability name but the manifest does not claim it.
    // This load-bearing negative prevents name-addressed fallback from returning it.
    assert_eq!(
        consumer
            .resolve_providers("fallback-only/v1")
            .await
            .unwrap(),
        Vec::<String>::new()
    );
    let fallback = consumer
        .resolve_provider("fallback-only/v1")
        .await
        .unwrap_err();
    assert_eq!(fallback.code(), Some("capability_unprovided"));
    assert!(matches!(
        fallback,
        CallError::CapabilityUnprovided { capability } if capability == "fallback-only/v1"
    ));
    let malformed = consumer
        .resolve_providers("Malformed/v1")
        .await
        .unwrap_err();
    assert_eq!(malformed.code(), Some("invalid_capability_identifier"));
    assert!(matches!(
        malformed,
        CallError::InvalidCapabilityIdentifier { capability } if capability == "Malformed/v1"
    ));

    consumer.close().await;
    daemon.kill_and_wait();
    assert!(fallback_task.await.unwrap().is_ok());
    assert!(single_task.await.unwrap().is_ok());
    assert!(z_task.await.unwrap().is_ok());
    assert!(a_task.await.unwrap().is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clean_subc_client_rs_serves_through_real_daemon() {
    let workspace = workspace_root();
    let daemon_bin = ensure_binary(
        &workspace,
        &[
            "build",
            "--locked",
            "-p",
            "subc-core",
            "--bin",
            "ck-subc",
            "--message-format=json",
        ],
    );
    let module_bin = ensure_binary(
        &workspace,
        &[
            "build",
            "--locked",
            "-p",
            "subc-client-rs",
            "--example",
            "echo-module",
            "--message-format=json",
        ],
    );

    let temp_dir = unique_temp_dir("subc-client-rs-real-daemon");
    let runtime_dir = temp_dir.join("runtime");
    let config_dir = temp_dir.join("config");
    let events_path = temp_dir.join("events.jsonl");
    fs::create_dir_all(&runtime_dir).unwrap();
    fs::create_dir_all(config_dir.join("cortexkit")).unwrap();
    fs::write(
        config_dir.join("cortexkit").join("subc.jsonc"),
        config_doc(&module_bin, &events_path),
    )
    .unwrap();

    let mut daemon = spawn_daemon(&daemon_bin, &runtime_dir, &config_dir);
    wait_for_connection_file(&daemon.connection_file, START_TIMEOUT).await;
    wait_for_catalog_module(&daemon.connection_file, MODULE_ID, START_TIMEOUT).await;

    let mut client = connect_authed_client(&daemon.connection_file)
        .await
        .unwrap();
    let health = control_rpc_on_stream(
        &mut client,
        99,
        serde_json::to_value(ClientControlRequest::SupervisorHealthProbe {
            module_id: MODULE_ID.to_string(),
        })
        .unwrap(),
    )
    .await;
    let health: ClientControlResponse = serde_json::from_value(health).unwrap();
    match health {
        ClientControlResponse::SupervisorHealthProbe {
            module_id,
            status,
            detail,
            metrics,
        } => {
            assert_eq!(module_id, MODULE_ID);
            assert_eq!(status, HealthStatus::Ok);
            // This module implements no health(), so it inherits the trait
            // default -- which now identifies itself rather than being byte
            // identical to a measured all-clear. Asserting the marker END TO END
            // proves the daemon carries detail verbatim from the module to the
            // control plane, which is the property that makes the marker
            // reachable by an operator at all.
            assert!(
                detail
                    .as_deref()
                    .is_some_and(|d| d.contains("no health implementation")),
                "expected the inherited-default marker, got {detail:?}"
            );
            assert_eq!(metrics, None);
        }
        other => panic!("unexpected health response: {other:?}"),
    }

    let (route_channel, route_epoch) = open_route(&mut client, MODULE_ID, 100).await;
    wait_for_event(&events_path, EVENT_TIMEOUT, |event| {
        event["kind"] == "bind" && event["route_channel"].as_u64() == Some(u64::from(route_channel))
    })
    .await;

    write_frame(
        &mut client,
        &data_request(
            route_channel,
            route_epoch,
            101,
            br#"{"kind":"unary","value":42}"#,
        ),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();
    let unary = read_frame_timeout(&mut client).await;
    assert_eq!(unary.header.ty, FrameType::Response);
    assert_eq!(unary.header.channel, route_channel);
    assert_eq!(unary.header.corr, 101);
    let unary_body: Value = serde_json::from_slice(&unary.body).unwrap();
    assert_eq!(unary_body["ok"], true);
    assert_eq!(unary_body["echo"]["value"], 42);

    write_frame(
        &mut client,
        &data_request(route_channel, route_epoch, 102, br#"{"kind":"error"}"#),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();
    let error = read_frame_timeout(&mut client).await;
    assert_eq!(error.header.ty, FrameType::Error);
    assert_eq!(error.header.channel, route_channel);
    assert_eq!(error.header.corr, 102);
    let error_body: ErrorBody = serde_json::from_slice(&error.body).unwrap();
    assert_eq!(error_body.code, "example_error");

    write_frame(
        &mut client,
        &data_request(route_channel, route_epoch, 103, br#"{"kind":"stream"}"#),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();
    let stream_data = read_frame_timeout(&mut client).await;
    assert_eq!(stream_data.header.ty, FrameType::StreamData);
    assert_eq!(stream_data.header.channel, route_channel);
    assert_eq!(stream_data.header.corr, 103);
    assert_eq!(stream_data.body, b"stream-event");
    let stream_end = read_frame_timeout(&mut client).await;
    assert_eq!(stream_end.header.ty, FrameType::StreamEnd);
    assert_eq!(stream_end.header.channel, route_channel);
    assert_eq!(stream_end.header.corr, 103);

    write_frame(
        &mut client,
        &data_request(route_channel, route_epoch, 104, br#"{"kind":"cancel"}"#),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();
    wait_for_event(&events_path, EVENT_TIMEOUT, |event| {
        event["kind"] == "cancel_waiting" && event["corr"].as_u64() == Some(104)
    })
    .await;
    write_frame(&mut client, &cancel_frame(route_channel, route_epoch, 104))
        .await
        .unwrap();
    client.flush().await.unwrap();
    wait_for_event(&events_path, EVENT_TIMEOUT, |event| {
        event["kind"] == "cancelled" && event["corr"].as_u64() == Some(104)
    })
    .await;
    let cancelled = read_frame_timeout(&mut client).await;
    assert_eq!(cancelled.header.ty, FrameType::Error);
    let cancelled_body: ErrorBody = serde_json::from_slice(&cancelled.body).unwrap();
    assert_eq!(cancelled_body.code, "cancelled");

    write_frame(&mut client, &goodbye_frame(route_channel, route_epoch, 105))
        .await
        .unwrap();
    client.flush().await.unwrap();
    wait_for_event(&events_path, EVENT_TIMEOUT, |event| {
        event["kind"] == "route_gone"
            && event["route_channel"].as_u64() == Some(u64::from(route_channel))
    })
    .await;

    let _ = daemon.child.kill();
}

/// The daemon hands a spawned module its launch nonce on descriptor 3, the
/// module's SDK reads it there, and its HELLO carries it: the module is
/// declared `reserved`, so the daemon admits it only with its exact spawn
/// nonce, and its provenance reports that the nonce came from `fd`. The
/// daemon still sets the environment copy too, which is why the source is
/// what tells the two apart.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_supervised_module_reads_its_launch_nonce_from_the_descriptor_and_hello_carries_it() {
    let workspace = workspace_root();
    let daemon_bin = ensure_binary(
        &workspace,
        &[
            "build",
            "--locked",
            "-p",
            "subc-core",
            "--bin",
            "ck-subc",
            "--message-format=json",
        ],
    );
    let module_bin = ensure_binary(
        &workspace,
        &[
            "build",
            "--locked",
            "-p",
            "subc-client-rs",
            "--example",
            "echo-module",
            "--message-format=json",
        ],
    );

    let temp_dir = unique_temp_dir("subc-client-rs-launch-nonce-fd");
    let runtime_dir = temp_dir.join("runtime");
    let config_dir = temp_dir.join("config");
    let events_path = temp_dir.join("events.jsonl");
    fs::create_dir_all(&runtime_dir).unwrap();
    fs::create_dir_all(config_dir.join("cortexkit")).unwrap();
    let env = BTreeMap::from([
        (
            "SUBC_MODULE_ECHO_EVENTS".to_string(),
            events_path.to_string_lossy().into_owned(),
        ),
        ("SUBC_MODULE_ECHO_PROVENANCE".to_string(), "1".to_string()),
    ]);
    fs::write(
        config_dir.join("cortexkit").join("subc.jsonc"),
        serde_json::to_string_pretty(&json!({
            "version": 1,
            "modules": {
                MODULE_ID: {
                    "program": module_bin.to_string_lossy(),
                    "args": [],
                    "env": env,
                    "enabled": true,
                    "reserved": true,
                    "launch_nonce_env": true,
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let mut daemon = spawn_daemon(&daemon_bin, &runtime_dir, &config_dir);
    wait_for_connection_file(&daemon.connection_file, START_TIMEOUT).await;
    let expected_source = if cfg!(unix) { "fd" } else { "env" };
    let started = wait_for_event(&events_path, START_TIMEOUT, |event| {
        event["kind"] == "launch_nonce"
    })
    .await;
    assert_eq!(started["source"], expected_source, "{started}");
    assert_eq!(started["env_present"], !cfg!(unix), "{started}");
    assert_eq!(started["fd_env_present"], cfg!(unix), "{started}");
    wait_for_catalog_module(&daemon.connection_file, MODULE_ID, START_TIMEOUT).await;

    let mut client = connect_authed_client(&daemon.connection_file)
        .await
        .unwrap();
    let response = control_rpc_on_stream(
        &mut client,
        7,
        serde_json::to_value(ClientControlRequest::SupervisorProvenance {
            module_id: Some(MODULE_ID.to_string()),
        })
        .unwrap(),
    )
    .await;
    let modules = &response["modules"];
    assert_eq!(
        modules[0]["module_declared"]["build"]["launch_nonce_source"], expected_source,
        "{response}"
    );
    daemon.kill_and_wait();
    // The warning goes to the daemon's own log (the file an operator reads), not
    // to stderr, which launchd and systemd usually discard. spawn_daemon_child
    // puts the data home at <temp>/data, so the log is under its run/logs.
    let logs_dir = temp_dir
        .join("data")
        .join("cortexkit")
        .join("run")
        .join("logs");
    let log = fs::read_dir(&logs_dir)
        .unwrap_or_else(|error| panic!("read {}: {error}", logs_dir.display()))
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("subc."))
        .map(|entry| fs::read_to_string(entry.path()).unwrap_or_default())
        .collect::<String>();
    assert!(
        log.lines().any(
            |line| line.contains("launch_nonce_env is deprecated and ignored")
                && line.contains(MODULE_ID)
        ),
        "missing module-named deprecation warning in {}: {log}",
        logs_dir.display()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn module_handle_catalog_update_refreshes_catalog_without_dropping_open_routes() {
    let workspace = workspace_root();
    let daemon_bin = ensure_binary(
        &workspace,
        &[
            "build",
            "--locked",
            "-p",
            "subc-core",
            "--bin",
            "ck-subc",
            "--message-format=json",
        ],
    );

    let temp_dir = unique_temp_dir("subc-client-rs-module-handle-update");
    let runtime_dir = temp_dir.join("runtime");
    let config_dir = temp_dir.join("config");
    fs::create_dir_all(&runtime_dir).unwrap();
    write_empty_config(&config_dir);

    let mut daemon = spawn_daemon(&daemon_bin, &runtime_dir, &config_dir);
    wait_for_connection_file(&daemon.connection_file, START_TIMEOUT).await;
    let module_id = "subc-client-rs-handle-catalog-update";
    let (handle, serve_task) = spawn_inline_module(
        &daemon.connection_file,
        inline_module_manifest(module_id, &["a", "b"]),
    )
    .await;
    wait_for_catalog_module(&daemon.connection_file, module_id, START_TIMEOUT).await;

    let initial_modules = catalog_modules(&daemon.connection_file, Some(module_id), 10_001).await;
    assert_eq!(module_tool_names(&initial_modules[0]), vec!["a", "b"]);

    let mut client = connect_authed_client(&daemon.connection_file)
        .await
        .unwrap();
    let (route_channel, route_epoch) = open_route(&mut client, module_id, 10_002).await;

    handle
        .catalog_update(vec![tool_provider_role(&["a", "c"])])
        .await
        .unwrap();

    let updated_modules = catalog_modules(&daemon.connection_file, Some(module_id), 10_003).await;
    assert_eq!(module_tool_names(&updated_modules[0]), vec!["a", "c"]);

    write_frame(
        &mut client,
        &data_request(route_channel, route_epoch, 10_004, b"after-update"),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();
    let response = read_frame_timeout(&mut client).await;
    assert_eq!(response.header.ty, FrameType::Response);
    assert_eq!(response.header.channel, route_channel);
    assert_eq!(response.header.corr, 10_004);
    assert_eq!(response.body, b"after-update");

    daemon.kill_and_wait();
    assert!(serve_task.await.unwrap().is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_registering_module_receives_the_daemons_machine_id_in_hello_ack() {
    let workspace = workspace_root();
    let daemon_bin = ensure_binary(
        &workspace,
        &[
            "build",
            "--locked",
            "-p",
            "subc-core",
            "--bin",
            "ck-subc",
            "--message-format=json",
        ],
    );

    let temp_dir = unique_temp_dir("subc-client-rs-machine-id");
    let runtime_dir = temp_dir.join("runtime");
    let config_dir = temp_dir.join("config");
    fs::create_dir_all(&runtime_dir).unwrap();
    write_empty_config(&config_dir);

    let mut daemon = spawn_daemon(&daemon_bin, &runtime_dir, &config_dir);
    wait_for_connection_file(&daemon.connection_file, START_TIMEOUT).await;
    let (handle, serve_task) = spawn_inline_module(
        &daemon.connection_file,
        inline_module_manifest("subc-client-rs-machine-id", &["a"]),
    )
    .await;

    // `spawn_daemon_child` points XDG_DATA_HOME at `<temp>/data`, so this is
    // the file this daemon minted, not the operator's.
    let stored = fs::read_to_string(temp_dir.join("data").join("cortexkit").join("machine-id"))
        .expect("the daemon minted its machine id before accepting the module");
    assert_eq!(
        handle.machine_id().map(|id| id.as_str()),
        Some(stored.trim_end_matches('\n')),
        "the module handle carries the id from HELLO_ACK, equal to the daemon's file"
    );

    daemon.kill_and_wait();
    assert!(serve_task.await.unwrap().is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn module_handle_catalog_update_surfaces_frozen_field_rejections() {
    let workspace = workspace_root();
    let daemon_bin = ensure_binary(
        &workspace,
        &[
            "build",
            "--locked",
            "-p",
            "subc-core",
            "--bin",
            "ck-subc",
            "--message-format=json",
        ],
    );

    let temp_dir = unique_temp_dir("subc-client-rs-module-handle-frozen");
    let runtime_dir = temp_dir.join("runtime");
    let config_dir = temp_dir.join("config");
    fs::create_dir_all(&runtime_dir).unwrap();
    write_empty_config(&config_dir);

    let mut daemon = spawn_daemon(&daemon_bin, &runtime_dir, &config_dir);
    wait_for_connection_file(&daemon.connection_file, START_TIMEOUT).await;
    let module_id = "subc-client-rs-handle-frozen-field";
    let (handle, serve_task) = spawn_inline_module(
        &daemon.connection_file,
        inline_module_manifest(module_id, &["a"]),
    )
    .await;
    wait_for_catalog_module(&daemon.connection_file, module_id, START_TIMEOUT).await;

    let error = handle.catalog_update(Vec::new()).await.unwrap_err();
    assert!(matches!(error, CatalogUpdateError::FrozenField(_)));

    daemon.kill_and_wait();
    assert!(serve_task.await.unwrap().is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn module_handle_catalog_update_fails_fast_after_connection_death() {
    let workspace = workspace_root();
    let daemon_bin = ensure_binary(
        &workspace,
        &[
            "build",
            "--locked",
            "-p",
            "subc-core",
            "--bin",
            "ck-subc",
            "--message-format=json",
        ],
    );

    let temp_dir = unique_temp_dir("subc-client-rs-module-handle-death");
    let runtime_dir = temp_dir.join("runtime");
    let config_dir = temp_dir.join("config");
    fs::create_dir_all(&runtime_dir).unwrap();
    write_empty_config(&config_dir);

    let mut daemon = spawn_daemon(&daemon_bin, &runtime_dir, &config_dir);
    wait_for_connection_file(&daemon.connection_file, START_TIMEOUT).await;
    let module_id = "subc-client-rs-handle-connection-death";
    let (handle, serve_task) = spawn_inline_module(
        &daemon.connection_file,
        inline_module_manifest(module_id, &["a"]),
    )
    .await;
    wait_for_catalog_module(&daemon.connection_file, module_id, START_TIMEOUT).await;

    daemon.kill_and_wait();
    assert!(serve_task.await.unwrap().is_ok());

    let result = timeout(
        Duration::from_secs(1),
        handle.catalog_update(vec![tool_provider_role(&["a"])]),
    )
    .await
    .unwrap();
    assert!(matches!(result, Err(CatalogUpdateError::ConnectionClosed)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subc_consumer_reports_outcome_unknown_mid_call_then_reopens_after_restart() {
    let workspace = workspace_root();
    let daemon_bin = ensure_binary(
        &workspace,
        &[
            "build",
            "--locked",
            "-p",
            "subc-core",
            "--bin",
            "ck-subc",
            "--message-format=json",
        ],
    );
    let module_bin = ensure_binary(
        &workspace,
        &[
            "build",
            "--locked",
            "-p",
            "subc-client-rs",
            "--example",
            "echo-module",
            "--message-format=json",
        ],
    );
    let temp_dir = unique_temp_dir("subc-client-rs-consumer-midcall");
    let runtime_dir = temp_dir.join("runtime");
    let config_dir = temp_dir.join("config");
    fs::create_dir_all(&runtime_dir).unwrap();
    write_empty_config(&config_dir);

    let mut daemon = spawn_daemon(&daemon_bin, &runtime_dir, &config_dir);
    wait_for_connection_file(&daemon.connection_file, START_TIMEOUT).await;
    let events_path = temp_dir.join("provider-a.jsonl");
    let provider = spawn_provider(
        &module_bin,
        &daemon.connection_file,
        CONSUMER_MODULE_A,
        &events_path,
    );
    wait_for_catalog_module(&daemon.connection_file, CONSUMER_MODULE_A, START_TIMEOUT).await;

    let consumer = Arc::new(
        SubcConsumer::connect(&daemon.connection_file, fast_consumer_options())
            .await
            .unwrap(),
    );
    let identity = consumer_identity("midcall");
    let first = consumer
        .call(
            tool_target(CONSUMER_MODULE_A),
            identity.clone(),
            br#"{"kind":"unary","value":1}"#.to_vec(),
            fast_call_options(),
        )
        .await
        .unwrap();
    assert_eq!(json_body(&first)["echo"]["value"], 1);

    let in_flight = {
        let consumer = Arc::clone(&consumer);
        let identity = identity.clone();
        tokio::spawn(async move {
            consumer
                .call(
                    tool_target(CONSUMER_MODULE_A),
                    identity,
                    br#"{"kind":"sleep","ms":5000}"#.to_vec(),
                    fast_call_options(),
                )
                .await
        })
    };
    wait_for_event(&events_path, EVENT_TIMEOUT, |event| {
        event["kind"] == "sleep_started"
    })
    .await;
    daemon.kill_and_wait();
    let mid_call = in_flight.await.unwrap();
    assert!(
        matches!(mid_call, Err(CallError::OutcomeUnknown(_))),
        "accepted mid-call must surface OutcomeUnknown, got {mid_call:?}"
    );
    assert_eq!(
        read_events(&events_path)
            .into_iter()
            .filter(|event| event["kind"] == "sleep_started")
            .count(),
        1,
        "OutcomeUnknown calls must not be auto-retried"
    );
    drop(provider);

    daemon.restart(&daemon_bin);
    wait_for_connection_file(&daemon.connection_file, START_TIMEOUT).await;
    let events_path_restarted = temp_dir.join("provider-a-restarted.jsonl");
    let _provider = spawn_provider(
        &module_bin,
        &daemon.connection_file,
        CONSUMER_MODULE_A,
        &events_path_restarted,
    );
    wait_for_catalog_module(&daemon.connection_file, CONSUMER_MODULE_A, START_TIMEOUT).await;

    let reopened = consumer
        .call(
            tool_target(CONSUMER_MODULE_A),
            identity,
            br#"{"kind":"unary","value":2}"#.to_vec(),
            fast_call_options(),
        )
        .await
        .unwrap();
    assert_eq!(json_body(&reopened)["echo"]["value"], 2);
    assert!(
        consumer.current_epoch() >= 2,
        "consumer epoch should advance after reconnect"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subc_consumer_recovers_after_daemon_restart_and_refuses_unknown_module_terminally() {
    let workspace = workspace_root();
    let daemon_bin = ensure_binary(
        &workspace,
        &[
            "build",
            "--locked",
            "-p",
            "subc-core",
            "--bin",
            "ck-subc",
            "--message-format=json",
        ],
    );
    let module_bin = ensure_binary(
        &workspace,
        &[
            "build",
            "--locked",
            "-p",
            "subc-client-rs",
            "--example",
            "echo-module",
            "--message-format=json",
        ],
    );
    let temp_dir = unique_temp_dir("subc-client-rs-consumer-race");
    let runtime_dir = temp_dir.join("runtime");
    let config_dir = temp_dir.join("config");
    fs::create_dir_all(&runtime_dir).unwrap();
    write_empty_config(&config_dir);

    let mut daemon = spawn_daemon(&daemon_bin, &runtime_dir, &config_dir);
    wait_for_connection_file(&daemon.connection_file, START_TIMEOUT).await;
    let initial_events = temp_dir.join("initial-provider.jsonl");
    let initial_provider = spawn_provider(
        &module_bin,
        &daemon.connection_file,
        CONSUMER_MODULE_A,
        &initial_events,
    );
    wait_for_catalog_module(&daemon.connection_file, CONSUMER_MODULE_A, START_TIMEOUT).await;

    let consumer = Arc::new(
        SubcConsumer::connect(&daemon.connection_file, fast_consumer_options())
            .await
            .unwrap(),
    );
    let identity = consumer_identity("race");
    let warm = consumer
        .call(
            tool_target(CONSUMER_MODULE_A),
            identity.clone(),
            br#"{"kind":"unary","value":"warm"}"#.to_vec(),
            fast_call_options(),
        )
        .await
        .unwrap();
    assert_eq!(json_body(&warm)["echo"]["value"], "warm");

    daemon.kill_and_wait();
    drop(initial_provider);
    daemon.restart(&daemon_bin);
    wait_for_connection_file(&daemon.connection_file, START_TIMEOUT).await;

    // unknown_module is a terminal route.open refusal: a call that races a
    // provider's registration now fails instead of retrying in place, so the
    // provider's HELLO must land before the call. wait_for_catalog_module is
    // that registration barrier.
    let restarted_events = temp_dir.join("restarted-provider.jsonl");
    let _provider = spawn_provider(
        &module_bin,
        &daemon.connection_file,
        CONSUMER_MODULE_A,
        &restarted_events,
    );
    wait_for_catalog_module(&daemon.connection_file, CONSUMER_MODULE_A, START_TIMEOUT).await;
    let raced = consumer
        .call(
            tool_target(CONSUMER_MODULE_A),
            identity.clone(),
            br#"{"kind":"unary","value":"after-register"}"#.to_vec(),
            fast_call_options(),
        )
        .await
        .unwrap();
    assert_eq!(json_body(&raced)["echo"]["value"], "after-register");

    // A terminal refusal must also not burn the retry budget: the call fails
    // after one route.open, well before the first retry backoff would fire.
    // terminal_absence_options makes that backoff two seconds, so a consumer
    // that retried unknown_module anyway would blow past this bound.
    let started = Instant::now();
    let absent = consumer
        .call(
            tool_target("subc-client-rs-never-registers"),
            identity,
            br#"{"kind":"unary","value":"missing"}"#.to_vec(),
            terminal_absence_options(),
        )
        .await;
    let absent_elapsed = started.elapsed();
    assert!(
        matches!(absent, Err(CallError::NotSent(_))),
        "target absence should terminate as NotSent, got {absent:?}"
    );
    assert!(
        absent_elapsed < Duration::from_secs(1),
        "unknown_module is terminal: the refusal took {absent_elapsed:?}, at or past the \
         one-second bound a single unretried route.open must stay well under"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subc_consumer_multiplexes_targets_and_classifies_reconnect_in_flight() {
    let workspace = workspace_root();
    let daemon_bin = ensure_binary(
        &workspace,
        &[
            "build",
            "--locked",
            "-p",
            "subc-core",
            "--bin",
            "ck-subc",
            "--message-format=json",
        ],
    );
    let module_bin = ensure_binary(
        &workspace,
        &[
            "build",
            "--locked",
            "-p",
            "subc-client-rs",
            "--example",
            "echo-module",
            "--message-format=json",
        ],
    );
    let temp_dir = unique_temp_dir("subc-client-rs-consumer-mux");
    let runtime_dir = temp_dir.join("runtime");
    let config_dir = temp_dir.join("config");
    fs::create_dir_all(&runtime_dir).unwrap();
    write_empty_config(&config_dir);
    let mut daemon = spawn_daemon(&daemon_bin, &runtime_dir, &config_dir);
    wait_for_connection_file(&daemon.connection_file, START_TIMEOUT).await;
    let events_a = temp_dir.join("a.jsonl");
    let events_b = temp_dir.join("b.jsonl");
    let provider_a = spawn_provider(
        &module_bin,
        &daemon.connection_file,
        CONSUMER_MODULE_A,
        &events_a,
    );
    let provider_b = spawn_provider(
        &module_bin,
        &daemon.connection_file,
        CONSUMER_MODULE_B,
        &events_b,
    );
    wait_for_catalog_module(&daemon.connection_file, CONSUMER_MODULE_A, START_TIMEOUT).await;
    wait_for_catalog_module(&daemon.connection_file, CONSUMER_MODULE_B, START_TIMEOUT).await;

    let consumer = Arc::new(
        SubcConsumer::connect(&daemon.connection_file, fast_consumer_options())
            .await
            .unwrap(),
    );
    let identity = consumer_identity("multiplex");
    let mut handles = Vec::new();
    for i in 0..20u64 {
        let consumer = Arc::clone(&consumer);
        let identity = identity.clone();
        let module_id = if i % 2 == 0 {
            CONSUMER_MODULE_A
        } else {
            CONSUMER_MODULE_B
        };
        handles.push(tokio::spawn(async move {
            let body = serde_json::to_vec(&json!({ "kind": "unary", "value": i })).unwrap();
            let bytes = consumer
                .call(tool_target(module_id), identity, body, fast_call_options())
                .await
                .unwrap();
            (i, json_body(&bytes))
        }));
    }
    for handle in handles {
        let (i, response) = handle.await.unwrap();
        assert_eq!(response["echo"]["value"], i);
    }

    let mut sleepy = Vec::new();
    for module_id in [
        CONSUMER_MODULE_A,
        CONSUMER_MODULE_B,
        CONSUMER_MODULE_A,
        CONSUMER_MODULE_B,
    ] {
        let consumer = Arc::clone(&consumer);
        let identity = identity.clone();
        sleepy.push(tokio::spawn(async move {
            consumer
                .call(
                    tool_target(module_id),
                    identity,
                    br#"{"kind":"sleep","ms":5000}"#.to_vec(),
                    fast_call_options(),
                )
                .await
        }));
    }
    wait_for_event(&events_a, EVENT_TIMEOUT, |event| {
        event["kind"] == "sleep_started"
    })
    .await;
    wait_for_event(&events_b, EVENT_TIMEOUT, |event| {
        event["kind"] == "sleep_started"
    })
    .await;
    daemon.kill_and_wait();
    for handle in sleepy {
        let result = handle.await.unwrap();
        assert!(
            matches!(result, Err(CallError::OutcomeUnknown(_))),
            "accepted multiplexed sleep should be OutcomeUnknown, got {result:?}"
        );
    }
    drop(provider_a);
    drop(provider_b);

    daemon.restart(&daemon_bin);
    wait_for_connection_file(&daemon.connection_file, START_TIMEOUT).await;
    let _provider_a = spawn_provider(
        &module_bin,
        &daemon.connection_file,
        CONSUMER_MODULE_A,
        &temp_dir.join("a-restarted.jsonl"),
    );
    let _provider_b = spawn_provider(
        &module_bin,
        &daemon.connection_file,
        CONSUMER_MODULE_B,
        &temp_dir.join("b-restarted.jsonl"),
    );
    wait_for_catalog_module(&daemon.connection_file, CONSUMER_MODULE_A, START_TIMEOUT).await;
    wait_for_catalog_module(&daemon.connection_file, CONSUMER_MODULE_B, START_TIMEOUT).await;
    let after = consumer
        .call(
            tool_target(CONSUMER_MODULE_B),
            identity,
            br#"{"kind":"unary","value":"after"}"#.to_vec(),
            fast_call_options(),
        )
        .await
        .unwrap();
    assert_eq!(json_body(&after)["echo"]["value"], "after");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subc_consumer_close_route_releases_the_route_and_reopens_fresh() {
    let workspace = workspace_root();
    let daemon_bin = ensure_binary(
        &workspace,
        &[
            "build",
            "--locked",
            "-p",
            "subc-core",
            "--bin",
            "ck-subc",
            "--message-format=json",
        ],
    );
    let module_bin = ensure_binary(
        &workspace,
        &[
            "build",
            "--locked",
            "-p",
            "subc-client-rs",
            "--example",
            "echo-module",
            "--message-format=json",
        ],
    );
    let temp_dir = unique_temp_dir("subc-client-rs-close-route");
    let runtime_dir = temp_dir.join("runtime");
    let config_dir = temp_dir.join("config");
    fs::create_dir_all(&runtime_dir).unwrap();
    write_empty_config(&config_dir);

    let mut daemon = spawn_daemon(&daemon_bin, &runtime_dir, &config_dir);
    wait_for_connection_file(&daemon.connection_file, START_TIMEOUT).await;
    let events_path = temp_dir.join("provider.jsonl");
    let _provider = spawn_provider(
        &module_bin,
        &daemon.connection_file,
        CONSUMER_MODULE_A,
        &events_path,
    );
    wait_for_catalog_module(&daemon.connection_file, CONSUMER_MODULE_A, START_TIMEOUT).await;

    let consumer = Arc::new(
        SubcConsumer::connect(&daemon.connection_file, fast_consumer_options())
            .await
            .unwrap(),
    );
    let identity = consumer_identity("close-route");

    // Open a route via a call (the echo-module logs a `bind` event with route_channel).
    let warm = consumer
        .call(
            tool_target(CONSUMER_MODULE_A),
            identity.clone(),
            br#"{"kind":"unary","value":"warm"}"#.to_vec(),
            fast_call_options(),
        )
        .await
        .unwrap();
    assert_eq!(json_body(&warm)["echo"]["value"], "warm");
    let bind = wait_for_event(&events_path, EVENT_TIMEOUT, |event| event["kind"] == "bind").await;
    let first_channel = bind["route_channel"].as_u64().unwrap();

    // close_route: the module must observe a route-gone GOODBYE for that channel.
    consumer
        .close_route(
            tool_target(CONSUMER_MODULE_A),
            identity.clone(),
            CloseRouteOptions::default(),
        )
        .await;
    wait_for_event(&events_path, EVENT_TIMEOUT, |event| {
        event["kind"] == "route_gone" && event["route_channel"].as_u64() == Some(first_channel)
    })
    .await;

    // A later call for the SAME key opens a FRESH route (not a tombstone): the module
    // logs a NEW bind on a different route_channel and the call succeeds.
    let after = consumer
        .call(
            tool_target(CONSUMER_MODULE_A),
            identity,
            br#"{"kind":"unary","value":"after"}"#.to_vec(),
            fast_call_options(),
        )
        .await
        .unwrap();
    assert_eq!(json_body(&after)["echo"]["value"], "after");
    wait_for_event(&events_path, EVENT_TIMEOUT, |event| {
        event["kind"] == "bind" && event["route_channel"].as_u64() != Some(first_channel)
    })
    .await;

    // close_route is idempotent: closing an already-closed / never-opened route is a no-op.
    consumer
        .close_route(
            tool_target("subc-client-rs-never-opened"),
            consumer_identity("close-route-absent"),
            CloseRouteOptions::default(),
        )
        .await;

    daemon.kill_and_wait();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subc_consumer_push_events_delivers_registered_push() {
    let workspace = workspace_root();
    let daemon_bin = ensure_binary(
        &workspace,
        &[
            "build",
            "--locked",
            "-p",
            "subc-core",
            "--bin",
            "ck-subc",
            "--message-format=json",
        ],
    );
    let temp_dir = unique_temp_dir("subc-client-rs-push-delivery");
    let runtime_dir = temp_dir.join("runtime");
    let config_dir = temp_dir.join("config");
    fs::create_dir_all(&runtime_dir).unwrap();
    write_empty_config(&config_dir);

    let mut daemon = spawn_daemon(&daemon_bin, &runtime_dir, &config_dir);
    wait_for_connection_file(&daemon.connection_file, START_TIMEOUT).await;
    let handler = PushModuleHandler::default();
    let (module, serve_task) = spawn_inline_module_with_handler(
        &daemon.connection_file,
        inline_push_module_manifest(PUSH_MODULE_ID, &["push"]),
        handler.clone(),
    )
    .await;
    wait_for_catalog_module(&daemon.connection_file, PUSH_MODULE_ID, START_TIMEOUT).await;

    let consumer = SubcConsumer::connect(&daemon.connection_file, fast_consumer_options())
        .await
        .unwrap();
    let route = consumer
        .open_route(
            tool_target(PUSH_MODULE_ID),
            consumer_identity("push-delivery"),
            fast_call_options(),
        )
        .await
        .unwrap();
    let module_route = wait_for_module_route(&handler, route.channel).await;
    let mut events = consumer.push_events(&route).unwrap();

    module
        .push(&module_route, b"registered-push".to_vec(), None)
        .await
        .unwrap();
    let event = timeout(EVENT_TIMEOUT, events.recv())
        .await
        .expect("registered push should arrive")
        .expect("registered push receiver should remain open");
    assert_eq!(event.handle, route);
    assert_eq!(event.body, b"registered-push");

    daemon.kill_and_wait();
    assert!(serve_task.await.unwrap().is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subc_consumer_counts_pushes_dropped_without_receiver() {
    let workspace = workspace_root();
    let daemon_bin = ensure_binary(
        &workspace,
        &[
            "build",
            "--locked",
            "-p",
            "subc-core",
            "--bin",
            "ck-subc",
            "--message-format=json",
        ],
    );
    let temp_dir = unique_temp_dir("subc-client-rs-push-drop");
    let runtime_dir = temp_dir.join("runtime");
    let config_dir = temp_dir.join("config");
    fs::create_dir_all(&runtime_dir).unwrap();
    write_empty_config(&config_dir);

    let mut daemon = spawn_daemon(&daemon_bin, &runtime_dir, &config_dir);
    wait_for_connection_file(&daemon.connection_file, START_TIMEOUT).await;
    let handler = PushModuleHandler::default();
    let (module, serve_task) = spawn_inline_module_with_handler(
        &daemon.connection_file,
        inline_push_module_manifest(PUSH_MODULE_ID, &["push"]),
        handler.clone(),
    )
    .await;
    wait_for_catalog_module(&daemon.connection_file, PUSH_MODULE_ID, START_TIMEOUT).await;

    let consumer = SubcConsumer::connect(&daemon.connection_file, fast_consumer_options())
        .await
        .unwrap();
    let route = consumer
        .open_route(
            tool_target(PUSH_MODULE_ID),
            consumer_identity("push-drop"),
            fast_call_options(),
        )
        .await
        .unwrap();
    let module_route = wait_for_module_route(&handler, route.channel).await;
    let before = consumer.pushes_dropped_no_receiver();

    module
        .push(&module_route, b"unregistered-push".to_vec(), None)
        .await
        .unwrap();
    wait_for_push_drop_count(&consumer, before + 1).await;
    assert!(
        consumer.pushes_dropped_no_receiver() > before,
        "an unregistered Push must increment pushes_dropped_no_receiver"
    );

    let mut events = consumer.push_events(&route).unwrap();
    assert!(
        timeout(Duration::from_millis(100), events.recv())
            .await
            .is_err(),
        "a Push dropped before registration must not be delivered later"
    );

    daemon.kill_and_wait();
    assert!(serve_task.await.unwrap().is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subc_consumer_push_events_end_when_route_closes() {
    let workspace = workspace_root();
    let daemon_bin = ensure_binary(
        &workspace,
        &[
            "build",
            "--locked",
            "-p",
            "subc-core",
            "--bin",
            "ck-subc",
            "--message-format=json",
        ],
    );
    let temp_dir = unique_temp_dir("subc-client-rs-push-teardown");
    let runtime_dir = temp_dir.join("runtime");
    let config_dir = temp_dir.join("config");
    fs::create_dir_all(&runtime_dir).unwrap();
    write_empty_config(&config_dir);

    let mut daemon = spawn_daemon(&daemon_bin, &runtime_dir, &config_dir);
    wait_for_connection_file(&daemon.connection_file, START_TIMEOUT).await;
    let handler = PushModuleHandler::default();
    let (module, serve_task) = spawn_inline_module_with_handler(
        &daemon.connection_file,
        inline_push_module_manifest(PUSH_MODULE_ID, &["push"]),
        handler.clone(),
    )
    .await;
    wait_for_catalog_module(&daemon.connection_file, PUSH_MODULE_ID, START_TIMEOUT).await;

    let consumer = SubcConsumer::connect(&daemon.connection_file, fast_consumer_options())
        .await
        .unwrap();
    let route = consumer
        .open_route(
            tool_target(PUSH_MODULE_ID),
            consumer_identity("push-teardown"),
            fast_call_options(),
        )
        .await
        .unwrap();
    let module_route = wait_for_module_route(&handler, route.channel).await;
    let mut events = consumer.push_events(&route).unwrap();

    consumer
        .close_handle(&route, CloseRouteOptions::default())
        .await
        .unwrap();
    assert!(
        timeout(EVENT_TIMEOUT, events.recv())
            .await
            .expect("route close should settle the push receiver")
            .is_none(),
        "route close must end the push receiver"
    );
    wait_for_module_route_gone(&handler, route.channel).await;
    let push_after_close = module
        .push(&module_route, b"dead-route-push".to_vec(), None)
        .await;
    assert!(
        matches!(push_after_close, Err(SubcModuleError::StaleRouteHandle(_))),
        "the module-side handle must reject Push after route teardown, got {push_after_close:?}"
    );

    daemon.kill_and_wait();
    assert!(serve_task.await.unwrap().is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subc_consumer_subscribe_streaming_contract() {
    let workspace = workspace_root();
    let daemon_bin = ensure_binary(
        &workspace,
        &[
            "build",
            "--locked",
            "-p",
            "subc-core",
            "--bin",
            "ck-subc",
            "--message-format=json",
        ],
    );
    let module_bin = ensure_binary(
        &workspace,
        &[
            "build",
            "--locked",
            "-p",
            "subc-client-rs",
            "--example",
            "echo-module",
            "--message-format=json",
        ],
    );
    let temp_dir = unique_temp_dir("subc-client-rs-subscribe");
    let runtime_dir = temp_dir.join("runtime");
    let config_dir = temp_dir.join("config");
    fs::create_dir_all(&runtime_dir).unwrap();
    write_empty_config(&config_dir);

    let mut daemon = spawn_daemon(&daemon_bin, &runtime_dir, &config_dir);
    wait_for_connection_file(&daemon.connection_file, START_TIMEOUT).await;
    let events_path = temp_dir.join("provider.jsonl");
    let provider = spawn_provider(
        &module_bin,
        &daemon.connection_file,
        CONSUMER_MODULE_A,
        &events_path,
    );
    wait_for_catalog_module(&daemon.connection_file, CONSUMER_MODULE_A, START_TIMEOUT).await;

    let consumer = SubcConsumer::connect(&daemon.connection_file, fast_consumer_options())
        .await
        .unwrap();
    let identity = consumer_identity("subscribe");

    let mut stream = consumer
        .subscribe(
            tool_target(CONSUMER_MODULE_A),
            identity.clone(),
            br#"{"kind":"stream_many","count":3}"#.to_vec(),
            fast_subscribe_options(),
        )
        .await
        .unwrap();
    let mut events = Vec::new();
    for _ in 0..3 {
        events.push(
            timeout(EVENT_TIMEOUT, stream.events().recv())
                .await
                .expect("stream event should arrive")
                .expect("stream event channel should remain open"),
        );
    }
    assert_eq!(
        events,
        vec![
            b"stream-event-0".to_vec(),
            b"stream-event-1".to_vec(),
            b"stream-event-2".to_vec(),
        ]
    );
    assert!(
        timeout(EVENT_TIMEOUT, stream.closed())
            .await
            .expect("stream should close")
            .is_ok(),
        "StreamEnd should resolve subscription.closed() successfully"
    );

    let mut error_stream = consumer
        .subscribe(
            tool_target(CONSUMER_MODULE_A),
            identity.clone(),
            br#"{"kind":"error"}"#.to_vec(),
            fast_subscribe_options(),
        )
        .await
        .unwrap();
    match timeout(EVENT_TIMEOUT, error_stream.closed())
        .await
        .expect("error terminal should close")
    {
        Err(CallError::Module(body)) => assert_eq!(body.code, "example_error"),
        other => panic!("Error terminal should reject closed() with ErrorBody, got {other:?}"),
    }

    let mut cancellable = consumer
        .subscribe(
            tool_target(CONSUMER_MODULE_A),
            identity.clone(),
            br#"{"kind":"cancel","tag":"unsubscribe"}"#.to_vec(),
            fast_subscribe_options(),
        )
        .await
        .unwrap();
    wait_for_event(&events_path, EVENT_TIMEOUT, |event| {
        event["kind"] == "cancel_waiting" && event["tag"] == "unsubscribe"
    })
    .await;
    cancellable.unsubscribe().unwrap();
    assert!(
        timeout(EVENT_TIMEOUT, cancellable.closed())
            .await
            .expect("unsubscribe should settle promptly")
            .is_ok(),
        "unsubscribe should settle closed() as a local clean close"
    );
    wait_for_event(&events_path, EVENT_TIMEOUT, |event| {
        event["kind"] == "cancelled" && event["tag"] == "unsubscribe"
    })
    .await;

    let mut overflow_opts = fast_subscribe_options();
    overflow_opts.event_buffer = 1;
    let mut overflow = consumer
        .subscribe(
            tool_target(CONSUMER_MODULE_A),
            identity.clone(),
            br#"{"kind":"stream_many","count":4}"#.to_vec(),
            overflow_opts,
        )
        .await
        .unwrap();
    let overflow_closed = timeout(EVENT_TIMEOUT, overflow.closed())
        .await
        .expect("overflow should close the subscription");
    assert!(
        matches!(overflow_closed, Err(CallError::SubscriptionBackpressure(_))),
        "full event channel should close with SubscriptionBackpressure, got {overflow_closed:?}"
    );

    let mut route_gone = consumer
        .subscribe(
            tool_target(CONSUMER_MODULE_A),
            identity.clone(),
            br#"{"kind":"cancel","tag":"route-goodbye"}"#.to_vec(),
            fast_subscribe_options(),
        )
        .await
        .unwrap();
    wait_for_event(&events_path, EVENT_TIMEOUT, |event| {
        event["kind"] == "cancel_waiting" && event["tag"] == "route-goodbye"
    })
    .await;
    drop(provider);
    let route_gone_closed = timeout(EVENT_TIMEOUT, route_gone.closed())
        .await
        .expect("provider route teardown should close the subscription");
    assert!(
        matches!(route_gone_closed, Err(CallError::OutcomeUnknown(_))),
        "route GOODBYE should reject closed(), got {route_gone_closed:?}"
    );

    let restarted_events = temp_dir.join("provider-restarted.jsonl");
    let _restarted_provider = spawn_provider(
        &module_bin,
        &daemon.connection_file,
        CONSUMER_MODULE_A,
        &restarted_events,
    );
    wait_for_catalog_module(&daemon.connection_file, CONSUMER_MODULE_A, START_TIMEOUT).await;
    let after = consumer
        .call(
            tool_target(CONSUMER_MODULE_A),
            identity,
            br#"{"kind":"unary","value":"after-route-goodbye"}"#.to_vec(),
            fast_call_options(),
        )
        .await
        .unwrap();
    assert_eq!(json_body(&after)["echo"]["value"], "after-route-goodbye");

    daemon.kill_and_wait();
}

async fn spawn_inline_module(
    connection_file: &Path,
    manifest: ModuleManifest,
) -> (
    ModuleHandle,
    tokio::task::JoinHandle<Result<(), SubcModuleError>>,
) {
    spawn_inline_module_with_handler(connection_file, manifest, EchoModuleHandler).await
}

async fn spawn_inline_module_with_handler<H>(
    connection_file: &Path,
    manifest: ModuleManifest,
    handler: H,
) -> (
    ModuleHandle,
    tokio::task::JoinHandle<Result<(), SubcModuleError>>,
)
where
    H: ModuleHandler,
{
    let (handle, serve_future) = serve_with_handle(connection_file, manifest, handler)
        .await
        .unwrap();
    (handle, tokio::spawn(serve_future))
}

fn inline_module_manifest(module_id: &str, tool_names: &[&str]) -> ModuleManifest {
    ModuleManifest::builder(module_id, env!("CARGO_PKG_VERSION"))
        .provides(vec![tool_provider_role(tool_names)])
        .build()
}

fn inline_capability_module_manifest(module_id: &str, capability: Option<&str>) -> ModuleManifest {
    let mut manifest = inline_module_manifest(module_id, &["capability.resolve"]);
    manifest.capabilities = capability.map(|capability| CapabilityDeclarations {
        provides: vec![capability.to_string()],
        requires: Vec::new(),
        must_never_reach: Vec::new(),
    });
    manifest
}

fn inline_push_module_manifest(module_id: &str, tool_names: &[&str]) -> ModuleManifest {
    let mut manifest = inline_module_manifest(module_id, tool_names);
    let Some(ProviderRole::ToolProvider { emits_push, .. }) = manifest.provides.first_mut() else {
        unreachable!("push test manifest must have a tool provider role");
    };
    *emits_push = true;
    manifest
}

fn tool_provider_role(tool_names: &[&str]) -> ProviderRole {
    ProviderRole::ToolProvider {
        tools: tool_names
            .iter()
            .map(|name| Tool {
                name: (*name).to_string(),
                description: None,
                execution_mode: ExecutionMode::Pure,
                schema: json!({ "type": "object" }),
            })
            .collect(),
        identity_scope: vec![IdentityScope::Project, IdentityScope::Session],
        concurrency: Concurrency::ModuleManaged,
        emits_push: false,
        sub_supervises: false,
    }
}

fn module_tool_names(module: &Value) -> Vec<&str> {
    module["roles"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|role| role["tools"].as_array().unwrap().iter())
        .map(|tool| tool["name"].as_str().unwrap())
        .collect()
}

fn spawn_daemon(daemon_bin: &Path, runtime_dir: &Path, config_dir: &Path) -> LiveDaemon {
    let child = spawn_daemon_child(daemon_bin, runtime_dir, config_dir);
    LiveDaemon {
        child,
        runtime_dir: runtime_dir.to_path_buf(),
        config_dir: config_dir.to_path_buf(),
        connection_file: runtime_dir.join("subc-connection.json"),
    }
}

fn spawn_daemon_child(daemon_bin: &Path, runtime_dir: &Path, config_dir: &Path) -> Child {
    // The spawned daemon is scrubbed here, but the CONSUMER in these tests runs
    // in this process and reads the same two variables through
    // `consumer_identity_from_env`. So a shell that already carries a supervised
    // module's identity -- an agent session, or any terminal launched under the
    // supervisor -- makes the client attest as that module, and route.open is
    // refused with `bad_consumer_identity` naming a module nobody mentioned.
    //
    // The failure reads as a code defect and is a property of the terminal. There
    // is no in-process scrub because `std::env::remove_var` is unsafe in a
    // multi-threaded process and these tests are threaded; run the suite with
    // `env -u SUBC_MODULE_ID -u SUBC_LAUNCH_NONCE` instead, which is what CI does
    // by virtue of not running under the supervisor.
    // XDG_DATA_HOME IS LOAD-BEARING AND WAS MISSING, and runtime+config are not a
    // substitute for it. The supervisor's child stdout/stderr capture directory is
    // `daemon_run_dir()/logs`, and `daemon_run_dir()` derives from the DATA home
    // (daemon_config.rs: `default_data_home().join("cortexkit").join("run")`) --
    // not from XDG_RUNTIME_DIR, which is what this test was setting.
    //
    // So every run of this suite supervised modules named `subc-client-rs-echo`
    // and `subc-client-rs-catalog-fake-aft` and created capture files for them in
    // the OPERATOR'S REAL `~/.local/share/cortexkit/run/logs/`. Reported by
    // iceteaSA on #106 from a directory listing; reproduced here by deleting the
    // file, running this suite, and watching it reappear.
    //
    // The residue is inert (the modules write nothing to stderr, so the files stay
    // zero bytes) and the hazard is not: a fixture module id that COLLIDES with a
    // real one would append test output into the file an operator reads for the
    // live module, and the reading stays well-formed.
    let data_dir = runtime_dir
        .parent()
        .expect("runtime dir is inside the test temp tree")
        .join("data");
    // The daemon runs as `ckdev-subc`, never under the production `ck-subc`
    // name it was built with.
    let daemon_bin = &cortexkit_test_support::ckdev_binary(daemon_bin);
    cortexkit_test_support::dev_command(daemon_bin)
        .env_remove(subc_protocol::SUBC_MODULE_ID_ENV)
        .env_remove(subc_protocol::SUBC_LAUNCH_NONCE_ENV)
        .env_remove(subc_client_rs::launch_nonce::LAUNCH_NONCE_FD_ENV)
        .env("XDG_RUNTIME_DIR", runtime_dir)
        .env("XDG_CONFIG_HOME", config_dir)
        .env("XDG_DATA_HOME", &data_dir)
        .env("SUBC_PORT", "0")
        .env("SUBC_CGROUP_PLACEMENT", "disabled")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("failed to spawn {}: {error}", daemon_bin.display()))
}

fn spawn_provider(
    module_bin: &Path,
    connection_file: &Path,
    module_id: &str,
    events_path: &Path,
) -> ProviderProcess {
    let runtime_dir = connection_file
        .parent()
        .expect("connection file is inside the test runtime directory");
    let temp_dir = runtime_dir
        .parent()
        .expect("runtime directory is inside the test temp tree");
    let child = Command::new(module_bin)
        .arg("--subc")
        .arg(connection_file)
        .env("XDG_RUNTIME_DIR", runtime_dir)
        .env("XDG_CONFIG_HOME", temp_dir.join("config"))
        .env("XDG_DATA_HOME", temp_dir.join("data"))
        .env(subc_protocol::SUBC_MODULE_ID_ENV, module_id)
        .env("SUBC_MODULE_ECHO_EVENTS", events_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("failed to spawn {}: {error}", module_bin.display()));
    ProviderProcess { child }
}

fn config_doc(module_bin: &Path, events_path: &Path) -> String {
    let env = BTreeMap::from([(
        "SUBC_MODULE_ECHO_EVENTS".to_string(),
        events_path.to_string_lossy().into_owned(),
    )]);
    serde_json::to_string_pretty(&json!({
        "version": 1,
        "modules": {
            MODULE_ID: {
                "program": module_bin.to_string_lossy(),
                "args": [],
                "env": env,
                "enabled": true,
            }
        }
    }))
    .unwrap()
}

fn fake_aft_stub_config_doc(module_bin: &Path, module_id: &str) -> String {
    let env = BTreeMap::from([
        ("FAKE_AFT_MODULE_ID".to_string(), module_id.to_string()),
        ("FAKE_AFT_TOOLS".to_string(), "catalog.inspect".to_string()),
    ]);
    let modules = serde_json::Map::from_iter([(
        module_id.to_string(),
        json!({
            "program": module_bin.to_string_lossy(),
            "args": [],
            "env": env,
            "enabled": true,
        }),
    )]);
    serde_json::to_string_pretty(&json!({
        "version": 1,
        "modules": modules,
    }))
    .unwrap()
}

fn write_empty_config(config_dir: &Path) {
    fs::create_dir_all(config_dir.join("cortexkit")).unwrap();
    fs::write(
        config_dir.join("cortexkit").join("subc.jsonc"),
        serde_json::to_string_pretty(&json!({ "version": 1, "modules": {} })).unwrap(),
    )
    .unwrap();
}

fn fast_consumer_options() -> ConsumerOptions {
    ConsumerOptions {
        handshake_timeout: AUTH_DEADLINE,
        call_timeout: Duration::from_secs(8),
        reconnect_backoff: RetryBackoff {
            base: Duration::from_millis(50),
            cap: Duration::from_millis(250),
            max_attempts: 20,
        },
        restored_debounce: Duration::from_millis(10),
        liveness_probe_window: Duration::from_secs(2),
    }
}

fn fast_call_options() -> CallOptions {
    CallOptions {
        timeout: Duration::from_secs(8),
        route_retry: RetryBackoff {
            base: Duration::from_millis(50),
            cap: Duration::from_millis(250),
            max_attempts: 20,
        },
        route_retry_deadline: Duration::from_secs(5),
        ..CallOptions::default()
    }
}

/// A call against a target that will never register must fail on the first
/// route.open refusal. The two-second first backoff is the tell: any in-place
/// retry of a terminal code sleeps it before the second attempt, which the
/// caller-side elapsed bound catches.
fn terminal_absence_options() -> CallOptions {
    CallOptions {
        timeout: Duration::from_secs(8),
        route_retry: RetryBackoff {
            base: Duration::from_secs(2),
            cap: Duration::from_secs(2),
            max_attempts: 4,
        },
        route_retry_deadline: Duration::from_secs(6),
        ..CallOptions::default()
    }
}

fn fast_subscribe_options() -> SubscribeOptions {
    SubscribeOptions {
        route_retry: RetryBackoff {
            base: Duration::from_millis(50),
            cap: Duration::from_millis(250),
            max_attempts: 20,
        },
        route_retry_deadline: Duration::from_secs(5),
        route_open_timeout: Duration::from_secs(8),
        ..SubscribeOptions::default()
    }
}

fn consumer_identity(session: &str) -> BindIdentity {
    let project_root = unique_temp_dir("subc-client-rs-consumer-project");
    BindIdentity::new(project_root.keep(), "subc-client-rs-consumer-test", session)
}

fn tool_target(module_id: &str) -> RouteTarget {
    RouteTarget::ToolProvider {
        module_id: module_id.to_string(),
    }
}

fn json_body(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).unwrap()
}

async fn wait_for_module_route(handler: &PushModuleHandler, channel: u16) -> RouteHandle {
    let deadline = Instant::now() + EVENT_TIMEOUT;
    loop {
        if let Some(handle) = handler.route(channel) {
            return handle;
        }
        if Instant::now() >= deadline {
            panic!("push module did not bind route channel {channel} within {EVENT_TIMEOUT:?}");
        }
        sleep(Duration::from_millis(20)).await;
    }
}

struct PolicyHarness {
    daemon: LiveDaemon,
    _temp_dir: ScratchDir,
    module: ModuleHandle,
    serve_task: tokio::task::JoinHandle<Result<(), SubcModuleError>>,
    handler: PolicyModuleHandler,
}

impl PolicyHarness {
    async fn stop(mut self) {
        self.handler.close_bumps();
        self.daemon.kill_and_wait();
        drop(self.module);
        assert!(self.serve_task.await.unwrap().is_ok());
    }
}

async fn start_policy_harness(scripts: impl IntoIterator<Item = PolicyScript>) -> PolicyHarness {
    let workspace = workspace_root();
    let daemon_bin = ensure_binary(
        &workspace,
        &[
            "build",
            "--locked",
            "-p",
            "subc-core",
            "--bin",
            "ck-subc",
            "--message-format=json",
        ],
    );
    let temp_dir = unique_temp_dir("subc-client-rs-policy-resolver");
    let runtime_dir = temp_dir.join("runtime");
    let config_dir = temp_dir.join("config");
    fs::create_dir_all(&runtime_dir).unwrap();
    write_empty_config(&config_dir);

    let daemon = spawn_daemon(&daemon_bin, &runtime_dir, &config_dir);
    wait_for_connection_file(&daemon.connection_file, START_TIMEOUT).await;
    let handler = PolicyModuleHandler::new(scripts);
    // The resolver serves policy.resolve on its MANAGEMENT SURFACE (as the
    // live module does); the helper binds that plane, so the fake must too.
    let (module, serve_task) = spawn_inline_module_with_handler(
        &daemon.connection_file,
        management_surface_manifest(POLICY_MODULE_ID, &["policy.resolve"]),
        handler.clone(),
    )
    .await;
    wait_for_catalog_module(&daemon.connection_file, POLICY_MODULE_ID, START_TIMEOUT).await;

    PolicyHarness {
        daemon,
        _temp_dir: temp_dir,
        module,
        serve_task,
        handler,
    }
}

fn policy_resolver(consumer: SubcConsumer, hard_timeout: Duration) -> PolicyResolver {
    PolicyResolver::with_resolver_target(
        consumer,
        POLICY_MODULE_ID,
        PolicyResolverConfig {
            hard_timeout,
            ttl_floor_ms: 1,
        },
    )
}

fn policy_project_root() -> (ScratchDir, String) {
    let root = unique_temp_dir("subc-client-rs-policy-project");
    let path = root.to_string_lossy().into_owned();
    (root, path)
}

async fn wait_for_module_route_gone(handler: &PushModuleHandler, channel: u16) {
    let deadline = Instant::now() + EVENT_TIMEOUT;
    loop {
        if handler.route(channel).is_none() {
            return;
        }
        if Instant::now() >= deadline {
            panic!("push module did not release route channel {channel} within {EVENT_TIMEOUT:?}");
        }
        sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_for_push_drop_count(consumer: &SubcConsumer, minimum: u64) {
    let deadline = Instant::now() + EVENT_TIMEOUT;
    loop {
        if consumer.pushes_dropped_no_receiver() >= minimum {
            return;
        }
        if Instant::now() >= deadline {
            panic!(
                "pushes_dropped_no_receiver did not reach {minimum}; got {}",
                consumer.pushes_dropped_no_receiver()
            );
        }
        sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_for_connection_file(path: &Path, wait: Duration) {
    let deadline = Instant::now() + wait;
    loop {
        if path.exists() {
            return;
        }
        if Instant::now() >= deadline {
            panic!(
                "daemon did not write connection file {} within {wait:?}",
                path.display()
            );
        }
        sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_for_catalog_module(path: &Path, module_id: &str, wait: Duration) {
    let deadline = Instant::now() + wait;
    let mut corr = 1_000;
    let mut last_error = None;
    loop {
        // A daemon that has just restarted can publish its connection file before
        // it answers the handshake promptly (a 2 s ServerProof timeout on a loaded
        // Windows runner), so a failed connect means "not yet", like an empty
        // catalog, until the deadline.
        match try_catalog_modules(path, Some(module_id), corr).await {
            Ok(modules) if modules.len() == 1 => return,
            Ok(_) => {}
            Err(error) => last_error = Some(error),
        }
        if Instant::now() >= deadline {
            panic!(
                "module {module_id} did not register in catalog within {wait:?}; last connect error: {last_error:?}"
            );
        }
        corr += 1;
        sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_for_event(path: &Path, wait: Duration, predicate: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + wait;
    loop {
        for event in read_events(path) {
            if predicate(&event) {
                return event;
            }
        }
        if Instant::now() >= deadline {
            panic!(
                "event did not appear within {wait:?}; events: {:?}",
                read_events(path)
            );
        }
        sleep(Duration::from_millis(20)).await;
    }
}

fn read_events(path: &Path) -> Vec<Value> {
    let Ok(raw) = fs::read_to_string(path) else {
        return Vec::new();
    };
    raw.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect()
}

async fn catalog_modules(path: &Path, module_id: Option<&str>, corr: u64) -> Vec<Value> {
    try_catalog_modules(path, module_id, corr).await.unwrap()
}

async fn try_catalog_modules(
    path: &Path,
    module_id: Option<&str>,
    corr: u64,
) -> Result<Vec<Value>, String> {
    let mut client = connect_authed_client(path)
        .await
        .map_err(|error| error.to_string())?;
    let response = control_rpc_on_stream(
        &mut client,
        corr,
        json!({
            "op": "catalog.list",
            "module_id": module_id,
        }),
    )
    .await;
    assert_eq!(response["op"], "catalog.list");
    Ok(response["modules"].as_array().cloned().unwrap_or_default())
}

async fn open_route<S>(stream: &mut S, module_id: &str, corr: u64) -> (u16, u32)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let project_root = unique_temp_dir("subc-client-rs-project");
    let response = control_rpc_on_stream(
        stream,
        corr,
        json!({
            "op": "route.open",
            "target": RouteTarget::ToolProvider {
                module_id: module_id.to_string(),
            },
            "identity": BindIdentity::new(project_root.keep(), "subc-client-rs-test", "clean-api"),
        }),
    )
    .await;
    assert_eq!(response["op"], "route.open");
    (
        response["route_channel"]
            .as_u64()
            .expect("route.open must return route_channel") as u16,
        response["route_epoch"]
            .as_u64()
            .expect("route.open must return route_epoch") as u32,
    )
}

async fn control_rpc_on_stream<S>(stream: &mut S, corr: u64, request: Value) -> Value
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let body = serde_json::to_vec(&request).unwrap();
    write_frame(stream, &control_request_frame(corr, body))
        .await
        .unwrap();
    stream.flush().await.unwrap();
    let response = read_frame_timeout(stream).await;
    // Show the daemon's error body on a mismatch: a bare "left: Error, right:
    // Response" says a control call failed but not which check refused it.
    assert_eq!(
        response.header.ty,
        FrameType::Response,
        "control request {request} answered {:?}: {}",
        response.header.ty,
        String::from_utf8_lossy(&response.body)
    );
    assert_eq!(response.header.channel, 0);
    assert_eq!(response.header.corr, corr);
    serde_json::from_slice(&response.body).unwrap()
}

async fn connect_authed_client(path: &Path) -> io::Result<TcpStream> {
    let conn = subc_transport::read_for_client(path).map_err(io::Error::other)?;
    let endpoint = conn.endpoints.first().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "connection file has no endpoint",
        )
    })?;
    let ip: IpAddr = endpoint
        .host
        .parse()
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    let mut stream = TcpStream::connect(SocketAddr::new(ip, endpoint.port)).await?;
    authenticate_client(&mut stream, &conn, AUTH_DEADLINE)
        .await
        .map_err(io::Error::other)?;
    Ok(stream)
}

async fn read_frame_timeout<S>(stream: &mut S) -> Frame
where
    S: AsyncRead + Unpin,
{
    timeout(READ_TIMEOUT, read_frame(stream))
        .await
        .expect("timed out reading frame")
        .expect("frame read failed")
        .expect("connection closed")
}

fn control_request_frame(corr: u64, body: Vec<u8>) -> Frame {
    Frame::build(
        FrameType::Request,
        Flags::new(false, Priority::Interactive, false),
        0,
        0,
        corr,
        body,
    )
    .unwrap()
}

fn data_request(channel: u16, epoch: u32, corr: u64, body: &[u8]) -> Frame {
    Frame::build(
        FrameType::Request,
        Flags::new(false, Priority::Interactive, false),
        channel,
        epoch,
        corr,
        body.to_vec(),
    )
    .unwrap()
}

fn cancel_frame(channel: u16, epoch: u32, corr: u64) -> Frame {
    Frame::build(
        FrameType::Cancel,
        Flags::new(false, Priority::Interactive, false),
        channel,
        epoch,
        corr,
        Vec::new(),
    )
    .unwrap()
}

fn goodbye_frame(channel: u16, epoch: u32, corr: u64) -> Frame {
    Frame::build(
        FrameType::Goodbye,
        Flags::new(false, Priority::Interactive, false),
        channel,
        epoch,
        corr,
        Vec::new(),
    )
    .unwrap()
}

fn ensure_binary(workspace: &Path, cargo_args: &[&str]) -> PathBuf {
    type BuildResults = HashMap<Vec<String>, Result<PathBuf, String>>;
    static BUILD_RESULTS: OnceLock<Mutex<BuildResults>> = OnceLock::new();
    let mut results = BUILD_RESULTS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let key = cargo_args.iter().map(|arg| (*arg).to_owned()).collect();
    let result = results
        .entry(key)
        .or_insert_with(|| {
            let output = Command::new("cargo")
            .args(cargo_args)
            .current_dir(workspace)
            .output()
            .map_err(|error| format!("failed to run cargo {cargo_args:?}: {error}"))?;
            if !output.status.success() {
                return Err(format!(
                    "cargo {cargo_args:?} failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
                    output.status.code(),
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                ));
            }
            // Take the executable of the target the call named (`--bin X` or
            // `--example X`), not merely the first executable cargo reports,
            // so a build that also produces another binary can't hand back the
            // wrong one.
            let wanted = cargo_args
                .windows(2)
                .find(|pair| pair[0] == "--bin" || pair[0] == "--example")
                .map(|pair| pair[1]);
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .find_map(|message| {
                    (message["reason"] == "compiler-artifact"
                        && wanted.is_none_or(|name| message["target"]["name"] == name))
                        .then(|| message["executable"].as_str().map(PathBuf::from))
                        .flatten()
                })
                .ok_or_else(|| {
                    format!(
                        "Cargo did not report an executable for {cargo_args:?}\nstdout:\n{}\nstderr:\n{}",
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    )
                })
        })
        .clone();
    drop(results);
    let artifact = result.unwrap_or_else(|error| panic!("{error}"));
    assert!(
        artifact.exists(),
        "Cargo-reported executable does not exist: {}",
        artifact.display()
    );
    artifact
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .unwrap()
        .to_path_buf()
}

fn unique_temp_dir(name: &str) -> ScratchDir {
    ScratchDir::new(name)
}

/// Issue #35: the #31 push family must be OBSERVABLE by a Rust consumer. The
/// daemon emits `route.closed { reason: crash }` when a module connection dies
/// with live routes -- before this surface existed, dispatch_frame returned on
/// the route-epoch guard for channel 0 and the push vanished without even a
/// counter.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subc_consumer_control_pushes_deliver_route_closed_on_module_death() {
    let workspace = workspace_root();
    let daemon_bin = ensure_binary(
        &workspace,
        &[
            "build",
            "--locked",
            "-p",
            "subc-core",
            "--bin",
            "ck-subc",
            "--message-format=json",
        ],
    );
    let temp_dir = unique_temp_dir("subc-client-rs-control-push");
    let runtime_dir = temp_dir.join("runtime");
    let config_dir = temp_dir.join("config");
    fs::create_dir_all(&runtime_dir).unwrap();
    write_empty_config(&config_dir);

    let mut daemon = spawn_daemon(&daemon_bin, &runtime_dir, &config_dir);
    wait_for_connection_file(&daemon.connection_file, START_TIMEOUT).await;
    let handler = PushModuleHandler::default();
    let (module, serve_task) = spawn_inline_module_with_handler(
        &daemon.connection_file,
        inline_push_module_manifest(PUSH_MODULE_ID, &["push"]),
        handler.clone(),
    )
    .await;
    wait_for_catalog_module(&daemon.connection_file, PUSH_MODULE_ID, START_TIMEOUT).await;

    let consumer = SubcConsumer::connect(&daemon.connection_file, fast_consumer_options())
        .await
        .unwrap();
    let route = consumer
        .open_route(
            tool_target(PUSH_MODULE_ID),
            consumer_identity("control-push"),
            fast_call_options(),
        )
        .await
        .unwrap();
    wait_for_module_route(&handler, route.channel).await;
    let mut control = consumer.control_pushes(8);

    // Kill the module's connection: the daemon cleans up its live routes and
    // emits the crash-close push to every client holding one.
    serve_task.abort();
    drop(module);

    let push = timeout(EVENT_TIMEOUT, control.recv())
        .await
        .expect("route.closed should arrive after module death")
        .expect("control push receiver should remain open");
    assert_eq!(push.op, "route.closed");
    assert_eq!(push.body["module_id"], PUSH_MODULE_ID);
    assert_eq!(push.body["reason"], "crash");
    // The crash arm is normative from #31: no prior route.closing, drained
    // false -- a cut, not a drain.
    assert_eq!(push.body["drained"], false);
    assert_eq!(push.body["terminal"], false);

    daemon.kill_and_wait();
}

/// The no-receiver arm: the same event with no registered receiver must COUNT,
/// not vanish -- the silent-drop shape is the defect #35 named.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subc_consumer_counts_control_pushes_dropped_without_receiver() {
    let workspace = workspace_root();
    let daemon_bin = ensure_binary(
        &workspace,
        &[
            "build",
            "--locked",
            "-p",
            "subc-core",
            "--bin",
            "ck-subc",
            "--message-format=json",
        ],
    );
    let temp_dir = unique_temp_dir("subc-client-rs-control-push-drop");
    let runtime_dir = temp_dir.join("runtime");
    let config_dir = temp_dir.join("config");
    fs::create_dir_all(&runtime_dir).unwrap();
    write_empty_config(&config_dir);

    let mut daemon = spawn_daemon(&daemon_bin, &runtime_dir, &config_dir);
    wait_for_connection_file(&daemon.connection_file, START_TIMEOUT).await;
    let handler = PushModuleHandler::default();
    let (module, serve_task) = spawn_inline_module_with_handler(
        &daemon.connection_file,
        inline_push_module_manifest(PUSH_MODULE_ID, &["push"]),
        handler.clone(),
    )
    .await;
    wait_for_catalog_module(&daemon.connection_file, PUSH_MODULE_ID, START_TIMEOUT).await;

    let consumer = SubcConsumer::connect(&daemon.connection_file, fast_consumer_options())
        .await
        .unwrap();
    let _route = consumer
        .open_route(
            tool_target(PUSH_MODULE_ID),
            consumer_identity("control-push-drop"),
            fast_call_options(),
        )
        .await
        .unwrap();
    let before = consumer.control_pushes_dropped();

    serve_task.abort();
    drop(module);

    let deadline = tokio::time::Instant::now() + EVENT_TIMEOUT;
    loop {
        if consumer.control_pushes_dropped() > before {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "an unobserved control push must increment control_pushes_dropped"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    daemon.kill_and_wait();
}

/// Issue #40: a burst that overflows the push receiver's bounded buffer must
/// DROP-AND-COUNT, not destroy the subscription. Before the fix, the Full arm
/// removed the receiver (uncounted), so the burst's worst moment left no trace
/// and every later push was lost to a consumer that believed itself subscribed.
/// The discriminating pair: the overflow lands on pushes_dropped_receiver_full,
/// and a push sent AFTER the burst still arrives on the SAME receiver.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subc_consumer_push_burst_counts_overflow_and_keeps_the_subscription() {
    let workspace = workspace_root();
    let daemon_bin = ensure_binary(
        &workspace,
        &[
            "build",
            "--locked",
            "-p",
            "subc-core",
            "--bin",
            "ck-subc",
            "--message-format=json",
        ],
    );
    let temp_dir = unique_temp_dir("subc-client-rs-push-burst");
    let runtime_dir = temp_dir.join("runtime");
    let config_dir = temp_dir.join("config");
    fs::create_dir_all(&runtime_dir).unwrap();
    write_empty_config(&config_dir);

    let mut daemon = spawn_daemon(&daemon_bin, &runtime_dir, &config_dir);
    wait_for_connection_file(&daemon.connection_file, START_TIMEOUT).await;
    let handler = PushModuleHandler::default();
    let (module, serve_task) = spawn_inline_module_with_handler(
        &daemon.connection_file,
        inline_push_module_manifest(PUSH_MODULE_ID, &["push"]),
        handler.clone(),
    )
    .await;
    wait_for_catalog_module(&daemon.connection_file, PUSH_MODULE_ID, START_TIMEOUT).await;

    let consumer = SubcConsumer::connect(&daemon.connection_file, fast_consumer_options())
        .await
        .unwrap();
    let route = consumer
        .open_route(
            tool_target(PUSH_MODULE_ID),
            consumer_identity("push-burst"),
            fast_call_options(),
        )
        .await
        .unwrap();
    let module_route = wait_for_module_route(&handler, route.channel).await;
    let mut events = consumer.push_events(&route).unwrap();

    // Overflow the bounded buffer WITHOUT draining: the receiver holds
    // DEFAULT_PUSH_EVENT_BUFFER events; everything beyond it must drop onto the
    // full-counter while the subscription survives.
    // Mirrors DEFAULT_PUSH_EVENT_BUFFER (128) in consumer.rs: 160 undrained
    // pushes overflow the mpsc by a margin. PACED (yield every 16) so the
    // client reader keeps consuming the socket -- an unpaced tight loop outruns
    // the reader task and trips the DAEMON's slow-client egress policy instead
    // (failed try_send closes the whole connection), which is a different
    // backpressure layer than the one under test and reads as StaleRouteHandle
    // at the module.
    let burst = 160;
    for i in 0..burst {
        module
            .push(&module_route, format!("burst-{i}").into_bytes(), None)
            .await
            .unwrap();
        if i % 16 == 15 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
    let deadline = tokio::time::Instant::now() + EVENT_TIMEOUT;
    while consumer.pushes_dropped_receiver_full() == 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "an overflowing burst must count on pushes_dropped_receiver_full"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // The no-receiver counter must NOT absorb the burst: the receiver exists.
    assert_eq!(
        consumer.pushes_dropped_no_receiver(),
        0,
        "a full buffer is not 'no receiver'; the counters must not conflate"
    );

    // Drain what the buffer held, then prove the subscription SURVIVED: a
    // fresh push after the burst arrives on the same receiver. recv()->None
    // here would mean the channel CLOSED, which is exactly the pre-fix defect,
    // so it panics rather than ending the drain quietly.
    loop {
        match tokio::time::timeout(Duration::from_millis(200), events.recv()).await {
            Ok(Some(_)) => continue,
            Ok(None) => {
                panic!("push receiver closed during drain: the burst destroyed the subscription")
            }
            Err(_) => break,
        }
    }
    module
        .push(&module_route, b"after-burst".to_vec(), None)
        .await
        .unwrap();
    let event = timeout(EVENT_TIMEOUT, events.recv())
        .await
        .expect("a push after the burst should arrive")
        .expect("the subscription must survive the burst");
    assert_eq!(event.body, b"after-burst");

    daemon.kill_and_wait();
    assert!(serve_task.await.unwrap().is_ok());
}

/// Emits stale_route_epoch on the FIRST request, then echoes. Stands in for
/// the daemon's #39 router refusal until that lands; the client-side retry
/// classification is identical either way (the code's documented contract is
/// not-forwarded, so evict-reopen-retry-once is safe by construction).
#[derive(Clone, Default)]
struct StaleEpochOnceHandler {
    fired: Arc<Mutex<bool>>,
}

#[async_trait]
impl ModuleHandler for StaleEpochOnceHandler {
    async fn handle(&self, _ctx: RequestCtx, body: Vec<u8>) -> HandlerOutcome {
        let mut fired = self
            .fired
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !*fired {
            *fired = true;
            return HandlerOutcome::Error {
                code: "stale_route_epoch".to_string(),
                message: "stale epoch".to_string(),
            };
        }
        HandlerOutcome::Response(body)
    }
}

/// Issue #39 client half: stale_route_epoch joins unknown_channel's
/// evict-reopen-retry-once class. The caller sees success, not the error, and
/// exactly one retry happens (the handler flips to echo after firing once,
/// so a client that did NOT retry fails the assertion on the error and a
/// client that retried more than once is visible in the module's state).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subc_consumer_retries_once_in_place_on_stale_route_epoch() {
    let workspace = workspace_root();
    let daemon_bin = ensure_binary(
        &workspace,
        &[
            "build",
            "--locked",
            "-p",
            "subc-core",
            "--bin",
            "ck-subc",
            "--message-format=json",
        ],
    );
    let temp_dir = unique_temp_dir("subc-client-rs-stale-epoch");
    let runtime_dir = temp_dir.join("runtime");
    let config_dir = temp_dir.join("config");
    fs::create_dir_all(&runtime_dir).unwrap();
    write_empty_config(&config_dir);

    let mut daemon = spawn_daemon(&daemon_bin, &runtime_dir, &config_dir);
    wait_for_connection_file(&daemon.connection_file, START_TIMEOUT).await;
    let handler = StaleEpochOnceHandler::default();
    let (_module, serve_task) = spawn_inline_module_with_handler(
        &daemon.connection_file,
        inline_push_module_manifest(PUSH_MODULE_ID, &["echo"]),
        handler.clone(),
    )
    .await;
    wait_for_catalog_module(&daemon.connection_file, PUSH_MODULE_ID, START_TIMEOUT).await;

    let consumer = SubcConsumer::connect(&daemon.connection_file, fast_consumer_options())
        .await
        .unwrap();
    let payload = br#"{"jsonrpc":"2.0","id":"stale-epoch-retry"}"#.to_vec();
    let reply = consumer
        .call(
            tool_target(PUSH_MODULE_ID),
            consumer_identity("stale-epoch"),
            payload.clone(),
            fast_call_options(),
        )
        .await
        .expect("stale_route_epoch must be retried once in place, not surfaced");
    assert_eq!(reply, payload);

    daemon.kill_and_wait();
    assert!(serve_task.await.unwrap().is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn policy_resolver_uses_a_live_ttl_cache_entry_without_a_second_wire_call() {
    let harness = start_policy_harness([PolicyScript::Reply {
        verdict: "allow",
        revision: 1,
        ttl_ms: 1_000,
    }])
    .await;
    let consumer = SubcConsumer::connect(&harness.daemon.connection_file, fast_consumer_options())
        .await
        .unwrap();
    let resolver = policy_resolver(consumer, Duration::from_secs(1));
    let (_project_guard, project_root) = policy_project_root();
    let subject = Subject::AgentId("agent-cache".to_string());

    let first = resolver
        .resolve(
            "approval",
            "plexus.github_write",
            subject.clone(),
            ProjectRef::Root(project_root.clone()),
        )
        .await;
    assert_eq!(first, Ok(PolicyVerdict::Allow));
    assert_eq!(
        resolver
            .resolve(
                "approval",
                "plexus.github_write",
                subject,
                ProjectRef::Root(project_root.clone())
            )
            .await,
        Ok(PolicyVerdict::Allow)
    );
    // Mutation fence: a cache miss would consume an unscripted provider call.
    assert_eq!(harness.handler.calls(), 1);

    drop(resolver);
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn policy_resolver_reply_revision_invalidates_every_older_cache_entry() {
    let harness = start_policy_harness([
        PolicyScript::Reply {
            verdict: "allow",
            revision: 1,
            ttl_ms: 1_000,
        },
        PolicyScript::Reply {
            verdict: "allow",
            revision: 2,
            ttl_ms: 1_000,
        },
        PolicyScript::Reply {
            verdict: "deny",
            revision: 2,
            ttl_ms: 1_000,
        },
    ])
    .await;
    let consumer = SubcConsumer::connect(&harness.daemon.connection_file, fast_consumer_options())
        .await
        .unwrap();
    let resolver = policy_resolver(consumer, Duration::from_secs(1));
    let (_project_guard, project_root) = policy_project_root();
    let subject = Subject::AgentId("agent-revision".to_string());

    assert_eq!(
        resolver
            .resolve(
                "approval",
                "plexus.github_write",
                subject.clone(),
                ProjectRef::Root(project_root.clone())
            )
            .await,
        Ok(PolicyVerdict::Allow)
    );
    assert_eq!(
        resolver
            .resolve(
                "approval",
                "plexus.merge",
                subject.clone(),
                ProjectRef::Root(project_root.clone())
            )
            .await,
        Ok(PolicyVerdict::Allow)
    );
    assert_eq!(
        resolver
            .resolve(
                "approval",
                "plexus.github_write",
                subject,
                ProjectRef::Root(project_root.clone())
            )
            .await,
        Ok(PolicyVerdict::Deny)
    );
    // Mutation fence: revision 2 must evict the older gate rather than leave it cached.
    assert_eq!(harness.handler.calls(), 3);

    drop(resolver);
    harness.stop().await;
}

/// A long-lived resolver that sees many subjects must not grow without bound:
/// once a subject's entries expire, its entry, its route (observed from the
/// resolver module's side) and its subscription task all go.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn policy_resolver_releases_expired_subjects_entries_routes_and_tasks() {
    const SUBJECTS: usize = 6;
    let replies = (0..SUBJECTS + 2).map(|_| PolicyScript::Reply {
        verdict: "allow",
        revision: 1,
        // Long enough that no entry expires while the subjects are still being
        // resolved one round trip at a time on a loaded runner.
        ttl_ms: 3_000,
    });
    let harness = start_policy_harness(replies).await;
    let consumer = SubcConsumer::connect(&harness.daemon.connection_file, fast_consumer_options())
        .await
        .unwrap();
    let resolver = policy_resolver(consumer, Duration::from_secs(1));
    let (_project_guard, project_root) = policy_project_root();
    let resolve = |subject: String| {
        resolver.resolve(
            "approval",
            "plexus.github_write",
            Subject::AgentId(subject),
            ProjectRef::Root(project_root.clone()),
        )
    };

    assert_eq!(resolve("agent-0".into()).await, Ok(PolicyVerdict::Allow));
    assert_eq!(policy_module_routes_reaching(&harness.handler, 1).await, 1);
    let baseline = resolver.footprint();
    assert_eq!((baseline.entries, baseline.routes), (1, 1));

    for index in 1..=SUBJECTS {
        assert_eq!(
            resolve(format!("agent-{index}")).await,
            Ok(PolicyVerdict::Allow)
        );
    }
    assert_eq!(
        policy_module_routes_reaching(&harness.handler, SUBJECTS + 1).await,
        SUBJECTS + 1,
        "each subject holds its own route while its entry is live"
    );

    // Every entry so far expires; the next resolve (a new subject) sweeps them.
    sleep(Duration::from_millis(3_500)).await;
    assert_eq!(
        resolve("agent-fresh".into()).await,
        Ok(PolicyVerdict::Allow)
    );
    assert_eq!(
        policy_module_routes_reaching(&harness.handler, 1).await,
        1,
        "expired subjects' routes must be closed at the resolver module"
    );
    // The fake serves one held policy.subscribe stream per harness: agent-0's.
    // Every later subscribe is refused and its task ends on its own, so the
    // only task that can outlive its subject is agent-0's, and once its route
    // is released no task should remain. (An aborted task is counted until
    // the runtime drops it, hence the wait.)
    assert_eq!(baseline.subscriptions, 1);
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut after = resolver.footprint();
    while after.subscriptions > 0 && Instant::now() < deadline {
        sleep(Duration::from_millis(10)).await;
        after = resolver.footprint();
    }
    assert_eq!(
        (after.entries, after.routes),
        (baseline.entries, baseline.routes),
        "entry and route counts return to the one-subject baseline"
    );
    assert_eq!(
        after.subscriptions, 0,
        "the expired subject's subscription task must end with its route: {after:?}"
    );
    assert_eq!(harness.handler.calls(), SUBJECTS as u64 + 2);

    drop(resolver);
    harness.stop().await;
}

/// The resolver module's live route count once it reaches `expected`, or
/// whatever it is at the deadline.
async fn policy_module_routes_reaching(handler: &PolicyModuleHandler, expected: usize) -> usize {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let routes = handler
            .routes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len();
        if routes == expected || Instant::now() >= deadline {
            return routes;
        }
        sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn policy_resolver_refetches_after_ttl_expiry() {
    let harness = start_policy_harness([
        PolicyScript::Reply {
            verdict: "allow",
            revision: 1,
            ttl_ms: 5,
        },
        PolicyScript::Reply {
            verdict: "deny",
            revision: 1,
            ttl_ms: 5,
        },
    ])
    .await;
    let consumer = SubcConsumer::connect(&harness.daemon.connection_file, fast_consumer_options())
        .await
        .unwrap();
    let resolver = policy_resolver(consumer, Duration::from_secs(1));
    let (_project_guard, project_root) = policy_project_root();
    let subject = Subject::AgentId("agent-ttl".to_string());

    assert_eq!(
        resolver
            .resolve(
                "approval",
                "plexus.github_write",
                subject.clone(),
                ProjectRef::Root(project_root.clone())
            )
            .await,
        Ok(PolicyVerdict::Allow)
    );
    sleep(Duration::from_millis(40)).await;
    assert_eq!(
        resolver
            .resolve(
                "approval",
                "plexus.github_write",
                subject,
                ProjectRef::Root(project_root.clone())
            )
            .await,
        Ok(PolicyVerdict::Deny)
    );
    // Mutation fence: the post-expiry reply differs, so a stale hit cannot pass.
    assert_eq!(harness.handler.calls(), 2);

    drop(resolver);
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn policy_resolver_hard_timeout_is_a_fault_before_the_provider_stall_finishes() {
    let stall = Duration::from_secs(1);
    let harness = start_policy_harness([PolicyScript::Stall { duration: stall }]).await;
    let consumer = SubcConsumer::connect(&harness.daemon.connection_file, fast_consumer_options())
        .await
        .unwrap();
    let resolver = policy_resolver(consumer, Duration::from_millis(200));
    let (_project_guard, project_root) = policy_project_root();

    let started = Instant::now();
    let result = resolver
        .resolve(
            "approval",
            "plexus.github_write",
            Subject::AgentId("agent-timeout".to_string()),
            ProjectRef::Root(project_root.clone()),
        )
        .await;
    let elapsed = started.elapsed();
    assert!(
        matches!(result, Err(PolicyResolveError::Fault { .. })),
        "expected a fault, got {result:?}"
    );
    // Mutation fence: the helper timeout, not the test, must beat the provider's stall.
    assert!(
        elapsed < stall,
        "resolve took {elapsed:?}, stall was {stall:?}"
    );
    assert_eq!(
        harness
            .handler
            .calls_reaching(1, Duration::from_secs(5))
            .await,
        1
    );

    drop(resolver);
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn policy_resolver_keeps_denied_decisions_distinct_from_faults() {
    let harness = start_policy_harness([
        PolicyScript::Reply {
            verdict: "deny",
            revision: 1,
            ttl_ms: 1_000,
        },
        PolicyScript::Stall {
            duration: Duration::from_secs(1),
        },
    ])
    .await;
    let consumer = SubcConsumer::connect(&harness.daemon.connection_file, fast_consumer_options())
        .await
        .unwrap();
    let resolver = policy_resolver(consumer, Duration::from_millis(200));
    let (_project_guard, project_root) = policy_project_root();

    let denied = resolver
        .resolve(
            "approval",
            "plexus.github_write",
            Subject::AgentId("agent-denied".to_string()),
            ProjectRef::Root(project_root.clone()),
        )
        .await;
    let fault = resolver
        .resolve(
            "approval",
            "plexus.merge",
            Subject::AgentId("agent-stalled".to_string()),
            ProjectRef::Root(project_root.clone()),
        )
        .await;
    assert_eq!(denied, Ok(PolicyVerdict::Deny));
    assert!(
        matches!(fault, Err(PolicyResolveError::Fault { .. })),
        "expected a fault, got {fault:?}"
    );
    // Mutation fence: both branches reached the fake; neither is a local default.
    assert_eq!(
        harness
            .handler
            .calls_reaching(2, Duration::from_secs(5))
            .await,
        2
    );

    drop(resolver);
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn policy_revision_push_invalidates_but_never_satisfies_a_resolve() {
    let harness = start_policy_harness([
        PolicyScript::Reply {
            verdict: "allow",
            revision: 1,
            ttl_ms: 1_000,
        },
        PolicyScript::Reply {
            verdict: "deny",
            revision: 2,
            ttl_ms: 1_000,
        },
    ])
    .await;
    let consumer = SubcConsumer::connect(&harness.daemon.connection_file, fast_consumer_options())
        .await
        .unwrap();
    let resolver = policy_resolver(consumer, Duration::from_secs(1));
    let (_project_guard, project_root) = policy_project_root();
    let subject = Subject::SessionToResolve("session-push".to_string());

    assert_eq!(
        resolver
            .resolve(
                "approval",
                "plexus.github_write",
                subject.clone(),
                ProjectRef::Root(project_root.clone())
            )
            .await,
        Ok(PolicyVerdict::Allow)
    );
    // Queue the bump onto the HELD subscription stream (the live lane); it is
    // asynchronous, so give the subscriber a beat to fold it. The reply below
    // proves the bump did not answer the resolve.
    harness.handler.send_bump(2);
    sleep(Duration::from_millis(200)).await;

    assert_eq!(
        resolver
            .resolve(
                "approval",
                "plexus.github_write",
                subject,
                ProjectRef::Root(project_root.clone())
            )
            .await,
        Ok(PolicyVerdict::Deny)
    );
    // After a policy revision bump, the next resolution must contact the provider.
    assert_eq!(harness.handler.calls(), 2);

    drop(resolver);
    harness.stop().await;
}

/// A management-surface manifest for the policy fake: the live resolver serves
/// policy.resolve on its management plane, and a role mismatch between fake
/// and reality is exactly how the helper shipped binding the wrong plane.
fn management_surface_manifest(module_id: &str, operations: &[&str]) -> ModuleManifest {
    let mut manifest = inline_module_manifest(module_id, &[]);
    manifest.provides = vec![ProviderRole::ManagementSurface {
        operations: operations
            .iter()
            .map(|name| ManagementOperation {
                name: (*name).to_string(),
                kind: ManagementOperationKind::Query,
                description: None,
            })
            .collect(),
        config_schema: serde_json::json!({}),
        observability: Vec::new(),
        identity_scope: Vec::new(),
        concurrency: Concurrency::ModuleManaged,
    }];
    manifest
}

/// `serve()` MUST RETURN WHEN THE DAEMON'S CONNECTION CLOSES. It must not
/// reconnect.
///
/// Four modules (broca, cerebellum, astrocyte, plexus) independently hang their
/// entire graceful shutdown off this return: WAL seals, browser-session
/// teardown, refresher joins, scheduler stops. None of them is told by the
/// daemon that it is going away — on a daemon cut nothing is sent, the socket
/// simply ends — so this return IS the fleet's shutdown signal.
///
/// Nothing asserted it until now, and the gap was worse than absent: many tests
/// here kill the daemon and then `serve_task.await`, so the property is
/// EXERCISED everywhere. But if `serve()` ever reconnected instead of returning,
/// those awaits would HANG rather than fail, and a hang reads as a slow suite or
/// a flake rather than as a named defect. Exercised-with-a-hang-as-its-failure
/// mode is not coverage.
///
/// So this test bounds the return. A reconnecting `serve()` fails here by name,
/// in five seconds, saying what it broke — and every module owner reading the
/// failure learns their shutdown never fires.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn serve_returns_when_the_daemon_connection_closes_rather_than_reconnecting() {
    let workspace = workspace_root();
    let daemon_bin = ensure_binary(
        &workspace,
        &[
            "build",
            "--locked",
            "-p",
            "subc-core",
            "--bin",
            "ck-subc",
            "--message-format=json",
        ],
    );

    let temp_dir = unique_temp_dir("subc-client-rs-serve-returns-on-eof");
    let runtime_dir = temp_dir.join("runtime");
    let config_dir = temp_dir.join("config");
    fs::create_dir_all(&runtime_dir).unwrap();
    write_empty_config(&config_dir);

    let mut daemon = spawn_daemon(&daemon_bin, &runtime_dir, &config_dir);
    wait_for_connection_file(&daemon.connection_file, START_TIMEOUT).await;
    let module_id = "subc-client-rs-serve-returns-on-eof";
    let (_handle, serve_task) = spawn_inline_module(
        &daemon.connection_file,
        inline_module_manifest(module_id, &["a"]),
    )
    .await;
    wait_for_catalog_module(&daemon.connection_file, module_id, START_TIMEOUT).await;

    // The daemon dies with no GOODBYE and no warning, which is exactly what a
    // `launchctl bootout` does: subc-core installs no signal handler, so the
    // process is terminated outright and every module's socket just ends.
    daemon.kill_and_wait();

    // The BOUND is the assertion. Without it a reconnecting serve() hangs here
    // forever and the suite reports a timeout, which names nothing.
    let returned = tokio::time::timeout(Duration::from_secs(5), serve_task).await;
    let joined = returned.expect(
        "serve() did not return within 5s of the daemon's connection closing. \
         Every module's graceful shutdown hangs off this return -- if serve() now \
         reconnects or waits, their WAL seals and teardown hooks never run on a \
         daemon cut, silently.",
    );
    assert!(
        joined.expect("serve task panicked").is_ok(),
        "serve() must return Ok on a clean connection close, not an error"
    );
}

// Scoped route.open against a real daemon.
//
// Four copies of fake-aft-stub (crates/subc-core/src/bin/fake-aft-stub.rs, a
// raw-protocol module whose behaviour is chosen by FAKE_AFT_* environment
// variables), all supervised by the daemon under test:
// - the scope OWNER, which syncs whatever scopes the test writes to its sync
//   file (writing the file is "the owner synced");
// - the CARRIER, listed as a carrier on every scope the tests sync;
// - a STRANGER, supervised like the carrier but never listed;
// - the PROVIDER the routes open to, which records each bind and its stamp.
// The carrier and stranger write their launch nonces out, so a consumer in this
// process can present them and be attested as that module. The owner, carrier
// and stranger are reserved because a scope's owner and carriers are named as
// reserved principals.

const SCOPE_OWNER: &str = "subc-client-rs-scope-owner";
const SCOPE_CARRIER: &str = "subc-client-rs-scope-carrier";
const SCOPE_STRANGER: &str = "subc-client-rs-scope-stranger";
const SCOPE_PROVIDER: &str = "subc-client-rs-scope-provider";
/// Must equal `SCOPE_SYNC_CORR_BASE` in crates/subc-core/src/bin/fake-aft-stub.rs:
/// the stub sends its `n`th scope.sync with corr `SCOPE_SYNC_CORR_BASE + n` and
/// records the daemon's reply under that corr, which is how `sync` below finds
/// the reply to the sync it asked for.
const SCOPE_SYNC_CORR_BASE: u64 = 1_000_000;

struct ScopeHarness {
    daemon: LiveDaemon,
    _temp_dir: ScratchDir,
    sync_path: PathBuf,
    owner_events: PathBuf,
    provider_events: PathBuf,
    carrier: ConsumerIdentity,
    stranger: ConsumerIdentity,
    syncs: u64,
}

impl ScopeHarness {
    fn options_as(&self, identity: &ConsumerIdentity) -> CallOptions {
        CallOptions {
            consumer_identity: Some(identity.clone()),
            ..fast_call_options()
        }
    }

    fn carrier_options(&self) -> CallOptions {
        self.options_as(&self.carrier)
    }

    /// Have the owner sync `scopes` (a JSON array of scope records) and wait
    /// for the daemon to accept every record.
    async fn sync(&mut self, scopes: Value) {
        self.syncs += 1;
        let corr = SCOPE_SYNC_CORR_BASE + self.syncs;
        let staging = self.sync_path.with_extension("staging");
        fs::write(&staging, serde_json::to_vec(&scopes).unwrap()).unwrap();
        fs::rename(&staging, &self.sync_path).unwrap();
        let reply = wait_for_event(&self.owner_events, EVENT_TIMEOUT, |event| {
            event["corr"] == corr
                && (event["kind"] == "scope_sync_response" || event["kind"] == "error")
        })
        .await;
        assert_eq!(
            reply["kind"], "scope_sync_response",
            "the daemon refused scope.sync: {reply}"
        );
        for result in reply["body_json"]["results"].as_array().unwrap() {
            assert_ne!(
                result["outcome"], "refused",
                "a scope record was refused: {result}"
            );
        }
    }

    fn provider_attaches(&self) -> Vec<Value> {
        read_events(&self.provider_events)
            .into_iter()
            .filter(|event| event["kind"] == "attach")
            .collect()
    }

    async fn connect(&self) -> SubcConsumer {
        SubcConsumer::connect(&self.daemon.connection_file, fast_consumer_options())
            .await
            .unwrap()
    }

    fn stop(mut self) {
        self.daemon.kill_and_wait();
    }
}

async fn start_scope_harness() -> ScopeHarness {
    let workspace = workspace_root();
    let daemon_bin = ensure_binary(
        &workspace,
        &[
            "build",
            "--locked",
            "-p",
            "subc-core",
            "--bin",
            "ck-subc",
            "--message-format=json",
        ],
    );
    let stub_bin = ensure_binary(
        &workspace,
        &[
            "build",
            "--locked",
            "-p",
            "subc-core",
            "--bin",
            "fake-aft-stub",
            "--message-format=json",
        ],
    );

    let temp_dir = unique_temp_dir("subc-client-rs-scopes");
    let runtime_dir = temp_dir.join("runtime");
    let config_dir = temp_dir.join("config");
    fs::create_dir_all(&runtime_dir).unwrap();
    fs::create_dir_all(config_dir.join("cortexkit")).unwrap();
    let sync_path = temp_dir.join("owner-scopes.json");
    let owner_events = temp_dir.join("owner-events.jsonl");
    let provider_events = temp_dir.join("provider-events.jsonl");
    let carrier_nonce = temp_dir.join("carrier.nonce");
    let stranger_nonce = temp_dir.join("stranger.nonce");

    let module = |module_id: &str, reserved: bool, extra: &[(&str, &Path)]| {
        let mut env = BTreeMap::from([("FAKE_AFT_MODULE_ID".to_string(), module_id.to_string())]);
        for (key, path) in extra {
            env.insert((*key).to_string(), path.to_string_lossy().into_owned());
        }
        json!({
            "program": stub_bin.to_string_lossy(),
            "args": [],
            "env": env,
            "enabled": true,
            "reserved": reserved,
        })
    };
    let config = json!({
        "version": 1,
        "modules": {
            SCOPE_OWNER: module(SCOPE_OWNER, true, &[
                ("FAKE_AFT_SCOPE_SYNC_PATH", &sync_path),
                ("FAKE_AFT_EVENTS_PATH", &owner_events),
            ]),
            SCOPE_CARRIER: module(SCOPE_CARRIER, true, &[
                ("FAKE_AFT_LAUNCH_NONCE_PATH", &carrier_nonce),
            ]),
            SCOPE_STRANGER: module(SCOPE_STRANGER, true, &[
                ("FAKE_AFT_LAUNCH_NONCE_PATH", &stranger_nonce),
            ]),
            SCOPE_PROVIDER: module(SCOPE_PROVIDER, false, &[
                ("FAKE_AFT_EVENTS_PATH", &provider_events),
            ]),
        }
    });
    fs::write(
        config_dir.join("cortexkit").join("subc.jsonc"),
        serde_json::to_string_pretty(&config).unwrap(),
    )
    .unwrap();

    let daemon = spawn_daemon(&daemon_bin, &runtime_dir, &config_dir);
    wait_for_connection_file(&daemon.connection_file, START_TIMEOUT).await;
    for module_id in [SCOPE_OWNER, SCOPE_CARRIER, SCOPE_STRANGER, SCOPE_PROVIDER] {
        wait_for_catalog_module(&daemon.connection_file, module_id, START_TIMEOUT).await;
    }
    let carrier = ConsumerIdentity {
        module_id: SCOPE_CARRIER.to_string(),
        launch_nonce: wait_for_file(&carrier_nonce).await,
    };
    let stranger = ConsumerIdentity {
        module_id: SCOPE_STRANGER.to_string(),
        launch_nonce: wait_for_file(&stranger_nonce).await,
    };
    ScopeHarness {
        daemon,
        _temp_dir: temp_dir,
        sync_path,
        owner_events,
        provider_events,
        carrier,
        stranger,
        syncs: 0,
    }
}

async fn wait_for_file(path: &Path) -> String {
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        if let Ok(raw) = fs::read_to_string(path) {
            return raw;
        }
        if Instant::now() >= deadline {
            panic!("{} did not appear within {START_TIMEOUT:?}", path.display());
        }
        sleep(Duration::from_millis(20)).await;
    }
}

/// A head scope with the carrier module listed as a carrier for any target.
fn scope_record(scope_ref: &str, scope_epoch: u64) -> Value {
    json!({
        "ref": scope_ref,
        "scope_epoch": scope_epoch,
        "kind": "head",
        "carriers": [
            { "principal": { "kind": "reserved", "module_id": SCOPE_CARRIER } }
        ],
    })
}

fn scope_selector(scope_ref: &str, scope_epoch: u64) -> ScopeSelector {
    ScopeSelector {
        owner: Principal::Reserved {
            module_id: SCOPE_OWNER.to_string(),
        },
        scope_ref: scope_ref.to_string(),
        scope_epoch: Some(scope_epoch),
    }
}

fn refusal_code(err: &CallError) -> Option<&str> {
    err.route_open_refusal().map(|body| body.code.as_str())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_carriers_scoped_open_is_admitted_and_the_provider_bind_carries_the_scope() {
    let mut harness = start_scope_harness().await;
    harness.sync(json!([scope_record("session-a", 1)])).await;
    let consumer = harness.connect().await;

    consumer
        .open_route_scoped(
            tool_target(SCOPE_PROVIDER),
            consumer_identity("scoped-admitted"),
            scope_selector("session-a", 1),
            harness.carrier_options(),
        )
        .await
        .unwrap_or_else(|err| panic!("the carrier's scoped open must be admitted: {err}"));

    let attach = wait_for_event(&harness.provider_events, EVENT_TIMEOUT, |event| {
        event["kind"] == "attach"
    })
    .await;
    assert_eq!(
        attach["scope"]["owner"],
        json!({ "kind": "reserved", "module_id": SCOPE_OWNER })
    );
    assert_eq!(attach["scope"]["ref"], "session-a");
    assert_eq!(attach["scope"]["scope_epoch"], 1);
    assert_eq!(
        attach["principal"],
        json!({ "kind": "reserved", "module_id": SCOPE_CARRIER }),
        "the bind names the carrier as the opener"
    );

    consumer.close().await;
    harness.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_scoped_open_before_the_owners_first_sync_retries_until_the_owner_syncs() {
    let mut harness = start_scope_harness().await;
    let consumer = Arc::new(harness.connect().await);
    let identity = consumer_identity("scoped-before-sync");

    // Control: with a short retry budget the open runs out on scope_not_synced,
    // so the daemon really is refusing with it while the owner has not synced.
    // Only the retry budget is short. The call itself gets a long deadline, so
    // a slow first round trip on a loaded runner still returns the daemon's
    // refusal instead of timing out before any answer arrives.
    let short = CallOptions {
        timeout: Duration::from_secs(10),
        route_retry_deadline: Duration::from_millis(300),
        ..harness.carrier_options()
    };
    let err = consumer
        .open_route_scoped(
            tool_target(SCOPE_PROVIDER),
            identity.clone(),
            scope_selector("session-a", 1),
            short,
        )
        .await
        .expect_err("no scope is live before the owner syncs");
    assert_eq!(refusal_code(&err), Some("scope_not_synced"), "{err}");

    let patient = CallOptions {
        timeout: Duration::from_secs(10),
        route_retry_deadline: Duration::from_secs(10),
        ..harness.carrier_options()
    };
    let opener = Arc::clone(&consumer);
    let open = tokio::spawn(async move {
        opener
            .open_route_scoped(
                tool_target(SCOPE_PROVIDER),
                identity,
                scope_selector("session-a", 1),
                patient,
            )
            .await
    });
    sleep(Duration::from_millis(500)).await;
    assert!(
        !open.is_finished(),
        "the open must still be retrying scope_not_synced, not settled"
    );

    harness.sync(json!([scope_record("session-a", 1)])).await;
    timeout(Duration::from_secs(10), open)
        .await
        .expect("the open must settle within its deadline")
        .unwrap()
        .unwrap_or_else(|err| panic!("the open must succeed once the owner syncs: {err}"));
    let attach = wait_for_event(&harness.provider_events, EVENT_TIMEOUT, |event| {
        event["kind"] == "attach"
    })
    .await;
    assert_eq!(attach["scope"]["ref"], "session-a");

    consumer.close().await;
    harness.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scope_not_carrier_and_scope_ended_are_terminal_refusals_with_their_codes() {
    let mut harness = start_scope_harness().await;
    harness.sync(json!([scope_record("session-a", 2)])).await;
    let consumer = harness.connect().await;
    let identity = consumer_identity("scoped-refusals");

    let mut no_epoch = scope_selector("session-a", 2);
    no_epoch.scope_epoch = None;
    let cases = [
        (
            "a supervised module that is not a listed carrier",
            harness.stranger.clone(),
            scope_selector("session-a", 2),
            "scope_not_carrier",
        ),
        (
            "an epoch the scope has moved past",
            harness.carrier.clone(),
            scope_selector("session-a", 1),
            "scope_ended",
        ),
        (
            "a ref the owner holds no live scope for",
            harness.carrier.clone(),
            scope_selector("session-unknown", 1),
            "scope_not_live",
        ),
        (
            "a selector without an epoch",
            harness.carrier.clone(),
            no_epoch,
            "scope_epoch_required",
        ),
    ];
    for (label, opener, selector, code) in cases {
        // terminal_absence_options sets a two-second route-retry backoff, and
        // jitter keeps at least half of it, so if the SDK wrongly retried a
        // terminal code the call would take a second or more.
        let options = CallOptions {
            consumer_identity: Some(opener),
            ..terminal_absence_options()
        };
        let started = Instant::now();
        let err = consumer
            .open_route_scoped(
                tool_target(SCOPE_PROVIDER),
                identity.clone(),
                selector,
                options,
            )
            .await
            .expect_err(label);
        let elapsed = started.elapsed();
        assert!(matches!(err, CallError::NotSent(_)), "{label}: {err}");
        assert_eq!(refusal_code(&err), Some(code), "{label}: {err}");
        assert!(
            elapsed < Duration::from_millis(900),
            "{label}: {code} is terminal and must not be retried (took {elapsed:?})"
        );
    }
    assert!(
        harness.provider_attaches().is_empty(),
        "no refused open may reach the provider"
    );

    consumer.close().await;
    harness.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scoped_opens_under_other_refs_or_epochs_and_unscoped_opens_get_their_own_routes() {
    let mut harness = start_scope_harness().await;
    harness
        .sync(json!([
            scope_record("session-a", 1),
            scope_record("session-b", 1)
        ]))
        .await;
    let consumer = harness.connect().await;
    let identity = consumer_identity("scoped-cache");
    let carrier_options = harness.carrier_options();
    let open = |selector: ScopeSelector| {
        consumer.open_route_scoped(
            tool_target(SCOPE_PROVIDER),
            identity.clone(),
            selector,
            carrier_options.clone(),
        )
    };

    let session_a = open(scope_selector("session-a", 1)).await.unwrap();
    let session_a_again = open(scope_selector("session-a", 1)).await.unwrap();
    assert_eq!(
        session_a_again, session_a,
        "the same selector reuses the cached route"
    );
    let session_b = open(scope_selector("session-b", 1)).await.unwrap();
    assert_ne!(session_b, session_a, "another scope ref gets its own route");
    let unscoped = consumer
        .open_route(
            tool_target(SCOPE_PROVIDER),
            identity.clone(),
            carrier_options.clone(),
        )
        .await
        .unwrap();
    assert_ne!(unscoped, session_a, "an unscoped open gets its own route");
    assert_ne!(unscoped, session_b, "an unscoped open gets its own route");

    // An epoch the owner has not synced is refused by the daemon, so it must
    // reach the daemon: the session-a route cached under epoch 1 must not be
    // handed to a caller asking for epoch 2.
    let err = open(scope_selector("session-a", 2))
        .await
        .expect_err("epoch 2 of session-a is not live");
    assert_eq!(refusal_code(&err), Some("scope_ended"), "{err}");

    let mut scopes = harness
        .provider_attaches()
        .into_iter()
        .map(|attach| attach["scope"]["ref"].as_str().map(str::to_string))
        .collect::<Vec<_>>();
    scopes.sort();
    assert_eq!(
        scopes,
        vec![None, Some("session-a".into()), Some("session-b".into())],
        "three binds: one per scope and one unscoped"
    );

    // Once the owner moves session-a to epoch 2, an open under epoch 2 is a
    // second route for session-a, distinct from the one epoch 1 had.
    harness
        .sync(json!([
            scope_record("session-a", 2),
            scope_record("session-b", 1)
        ]))
        .await;
    let session_a_epoch_2 = open(scope_selector("session-a", 2)).await.unwrap();
    assert_ne!(session_a_epoch_2, session_a);
    wait_for_event(&harness.provider_events, EVENT_TIMEOUT, |event| {
        event["kind"] == "attach" && event["scope"]["scope_epoch"] == 2
    })
    .await;

    consumer.close().await;
    harness.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scoped_route_close_names_only_its_channel_and_restart_names_all_remaining_routes() {
    let mut harness = start_scope_harness().await;
    harness.sync(json!([scope_record("session-a", 1)])).await;
    let consumer = Arc::new(harness.connect().await);
    let mut pushes = consumer.control_pushes(16);
    let identity = consumer_identity("two-route-reasons");
    let scoped = consumer
        .open_route_scoped(
            tool_target(SCOPE_PROVIDER),
            identity.clone(),
            scope_selector("session-a", 1),
            harness.carrier_options(),
        )
        .await
        .unwrap();
    let unscoped = consumer
        .open_route(
            tool_target(SCOPE_PROVIDER),
            identity.clone(),
            harness.carrier_options(),
        )
        .await
        .unwrap();
    let pending = {
        let consumer = Arc::clone(&consumer);
        tokio::spawn(async move {
            consumer
                .request(
                    &scoped,
                    serde_json::to_vec(&json!({"name":"hold", "arguments":{"delay_ms":30_000}}))
                        .unwrap(),
                    CallOptions {
                        timeout: Duration::from_secs(60),
                        ..fast_call_options()
                    },
                )
                .await
        })
    };
    wait_for_event(&harness.provider_events, EVENT_TIMEOUT, |event| {
        event["kind"] == "request_received" && event["body_json"]["name"] == "hold"
    })
    .await;
    harness.sync(json!([])).await;
    let closed = loop {
        let push = timeout(EVENT_TIMEOUT, pushes.recv())
            .await
            .unwrap()
            .unwrap();
        if push.op == "route.closed" {
            break push;
        }
    };
    assert_eq!(closed.body["channels"], json!([scoped.channel]));
    assert_eq!(closed.body["reason"], "scope_ended");
    let err = timeout(EVENT_TIMEOUT, pending)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(matches!(err, CallError::OutcomeUnknown(_)), "{err}");
    assert_eq!(err.outcome_cause(), Some(OutcomeUnknownCause::RouteEnded));
    assert_eq!(
        err.close_reason(),
        Some(&RouteEndReason::Daemon(RouteCloseReason::ScopeEnded))
    );
    consumer
        .request(&unscoped, b"{}".to_vec(), fast_call_options())
        .await
        .expect("the unscoped route remains usable");

    let other = consumer
        .open_route(
            tool_target(SCOPE_PROVIDER),
            consumer_identity("restart-second-route"),
            harness.carrier_options(),
        )
        .await
        .unwrap();
    let mut client = connect_authed_client(&harness.daemon.connection_file)
        .await
        .unwrap();
    let body = serde_json::to_vec(&ClientControlRequest::SupervisorRestart {
        module_id: SCOPE_PROVIDER.to_string(),
        drain_timeout_ms: Some(100),
    })
    .unwrap();
    write_frame(&mut client, &control_request_frame(77, body))
        .await
        .unwrap();
    client.flush().await.unwrap();
    let response = timeout(EVENT_TIMEOUT, read_frame(&mut client))
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(response.header.ty, FrameType::Response);
    let closed = loop {
        let push = timeout(EVENT_TIMEOUT, pushes.recv())
            .await
            .unwrap()
            .unwrap();
        if push.op == "route.closed" {
            break push;
        }
    };
    let mut expected = vec![unscoped.channel, other.channel];
    expected.sort_unstable();
    assert_eq!(closed.body["channels"], json!(expected));
    assert_eq!(closed.body["reason"], "restart");
    consumer.close().await;
    harness.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ending_the_scope_closes_the_route_with_scope_ended_and_it_is_not_reopened() {
    let mut harness = start_scope_harness().await;
    harness.sync(json!([scope_record("session-a", 1)])).await;
    let consumer = harness.connect().await;
    let mut pushes = consumer.control_pushes(16);
    let identity = consumer_identity("scoped-ended");

    let handle = consumer
        .open_route_scoped(
            tool_target(SCOPE_PROVIDER),
            identity.clone(),
            scope_selector("session-a", 1),
            harness.carrier_options(),
        )
        .await
        .unwrap();
    wait_for_event(&harness.provider_events, EVENT_TIMEOUT, |event| {
        event["kind"] == "attach"
    })
    .await;

    // The owner syncs an empty set: session-a has ended.
    harness.sync(json!([])).await;

    let closed = loop {
        let push = timeout(EVENT_TIMEOUT, pushes.recv())
            .await
            .expect("route.closed must arrive after the scope ends")
            .expect("control push receiver open");
        if push.op == "route.closed" {
            break push;
        }
    };
    assert_eq!(closed.body["reason"], "scope_ended", "{}", closed.body);
    let reason = closed
        .route_close_reason()
        .expect("route.closed carries a reason");
    assert_eq!(
        reason.disposition(),
        RouteCloseDisposition::MustNotReopen,
        "a route whose scope ended must not be reopened"
    );

    // The daemon also sends a GOODBYE on the route's channel, and the SDK drops
    // a route from its managed cache when that arrives, so the handle goes
    // stale. The GOODBYE and the push travel separately, hence the poll.
    let deadline = Instant::now() + EVENT_TIMEOUT;
    loop {
        match consumer
            .request(&handle, b"{}".to_vec(), fast_call_options())
            .await
        {
            Err(CallError::StaleRouteHandle(_)) => break,
            other if Instant::now() >= deadline => {
                panic!("the ended route's handle is still live: {other:?}")
            }
            _ => sleep(Duration::from_millis(20)).await,
        }
    }

    // Asking again under the ended selector reaches the daemon and is refused;
    // nothing reopens the route behind the caller's back.
    let err = consumer
        .open_route_scoped(
            tool_target(SCOPE_PROVIDER),
            identity,
            scope_selector("session-a", 1),
            terminal_absence_options_as(&harness.carrier),
        )
        .await
        .expect_err("an ended scope admits no new route");
    assert_eq!(refusal_code(&err), Some("scope_not_live"), "{err}");
    sleep(Duration::from_millis(200)).await;
    assert_eq!(
        harness.provider_attaches().len(),
        1,
        "the provider saw exactly the one bind made before the scope ended"
    );

    consumer.close().await;
    harness.stop();
}

fn terminal_absence_options_as(identity: &ConsumerIdentity) -> CallOptions {
    CallOptions {
        consumer_identity: Some(identity.clone()),
        ..terminal_absence_options()
    }
}

// `module.draining` reaching a module built on `serve`, against a real daemon.
//
// Both tests supervise the `echo-module` example, which records each
// `on_draining` call, each `health` answer (when it declares a busy gauge) and
// its `on_connection_end` in an events file, every line stamped with its pid.
// `supervisor.restart` replaces the process, and the replacement writes to the
// same file, so the assertions read only the drained process's lines.

const DRAIN_MODULE_ID: &str = "subc-client-rs-drain";

/// A daemon supervising `echo-module` as [`DRAIN_MODULE_ID`] with the given
/// extra environment and module settings.
async fn start_drain_harness(
    name: &str,
    extra_env: &[(&str, &str)],
    module_settings: Value,
) -> (ScratchDir, LiveDaemon, PathBuf) {
    let workspace = workspace_root();
    let daemon_bin = ensure_binary(
        &workspace,
        &[
            "build",
            "--locked",
            "-p",
            "subc-core",
            "--bin",
            "ck-subc",
            "--message-format=json",
        ],
    );
    let module_bin = ensure_binary(
        &workspace,
        &[
            "build",
            "--locked",
            "-p",
            "subc-client-rs",
            "--example",
            "echo-module",
            "--message-format=json",
        ],
    );
    let temp_dir = unique_temp_dir(name);
    let runtime_dir = temp_dir.join("runtime");
    let config_dir = temp_dir.join("config");
    let events_path = temp_dir.join("events.jsonl");
    fs::create_dir_all(&runtime_dir).unwrap();
    fs::create_dir_all(config_dir.join("cortexkit")).unwrap();
    let mut env = BTreeMap::from([(
        "SUBC_MODULE_ECHO_EVENTS".to_string(),
        events_path.to_string_lossy().into_owned(),
    )]);
    for (key, value) in extra_env {
        env.insert((*key).to_string(), (*value).to_string());
    }
    let mut module = json!({
        "program": module_bin.to_string_lossy(),
        "args": [],
        "env": env,
        "enabled": true,
    });
    if let (Some(module), Some(settings)) = (module.as_object_mut(), module_settings.as_object()) {
        module.extend(settings.clone());
    }
    fs::write(
        config_dir.join("cortexkit").join("subc.jsonc"),
        serde_json::to_string_pretty(&json!({
            "version": 1,
            "modules": { DRAIN_MODULE_ID: module },
        }))
        .unwrap(),
    )
    .unwrap();

    let daemon = spawn_daemon(&daemon_bin, &runtime_dir, &config_dir);
    wait_for_connection_file(&daemon.connection_file, START_TIMEOUT).await;
    wait_for_catalog_module(&daemon.connection_file, DRAIN_MODULE_ID, START_TIMEOUT).await;
    (temp_dir, daemon, events_path)
}

/// Run `supervisor.restart` on [`DRAIN_MODULE_ID`] and require its ACK. The
/// ACK can come before the drain has finished, so callers wait for the
/// drained process's `connection_end` event to know it is over.
async fn restart_drain_module(connection_file: &Path) {
    let mut client = connect_authed_client(connection_file).await.unwrap();
    let body = serde_json::to_vec(&ClientControlRequest::SupervisorRestart {
        module_id: DRAIN_MODULE_ID.to_string(),
        drain_timeout_ms: None,
    })
    .unwrap();
    write_frame(&mut client, &control_request_frame(77, body))
        .await
        .unwrap();
    client.flush().await.unwrap();
    let response = timeout(Duration::from_secs(30), read_frame(&mut client))
        .await
        .expect("supervisor.restart must be answered")
        .unwrap()
        .expect("daemon closed the control connection");
    assert_eq!(
        response.header.ty,
        FrameType::Response,
        "supervisor.restart failed: {}",
        String::from_utf8_lossy(&response.body)
    );
}

/// The pid of the process that registered first: its `hello_ack` line.
async fn first_module_pid(events_path: &Path) -> u64 {
    wait_for_event(events_path, EVENT_TIMEOUT, |event| {
        event["kind"] == "hello_ack"
    })
    .await["pid"]
        .as_u64()
        .expect("the module must stamp its events with its pid")
}

/// Every event `pid` recorded, in order.
fn events_of(events_path: &Path, pid: u64) -> Vec<Value> {
    read_events(events_path)
        .into_iter()
        .filter(|event| event["pid"].as_u64() == Some(pid))
        .collect()
}

fn position_of(events: &[Value], what: &str, predicate: impl Fn(&Value) -> bool) -> usize {
    events
        .iter()
        .position(predicate)
        .unwrap_or_else(|| panic!("no {what} event; events: {events:#?}"))
}

fn unix_ms_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// `supervisor.restart` drains the module before stopping it. The daemon
/// tells the module so with `module.draining`, and a module built on `serve`
/// must hear it through `on_draining`: reason `restart`, a deadline still in
/// the future and inside the configured drain budget, and before the GOODBYE
/// that ends the drain (seen as `on_connection_end(goodbye)`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn supervisor_restart_calls_on_draining_with_restart_and_a_future_deadline_before_goodbye() {
    const DRAIN_BUDGET_MS: u64 = 10_000;
    let (_temp_dir, mut daemon, events_path) = start_drain_harness(
        "subc-client-rs-drain-notice",
        &[],
        json!({ "drain_timeout_ms": DRAIN_BUDGET_MS }),
    )
    .await;
    let drained_pid = first_module_pid(&events_path).await;

    let restart_sent_ms = unix_ms_now();
    restart_drain_module(&daemon.connection_file).await;
    wait_for_event(&events_path, EVENT_TIMEOUT, |event| {
        event["pid"].as_u64() == Some(drained_pid) && event["kind"] == "connection_end"
    })
    .await;

    let events = events_of(&events_path, drained_pid);
    let draining_at = position_of(&events, "draining", |event| event["kind"] == "draining");
    let end_at = position_of(&events, "connection_end", |event| {
        event["kind"] == "connection_end"
    });
    let draining = &events[draining_at];
    assert_eq!(draining["reason"], "Restart", "{draining}");
    let deadline_ms = draining["deadline_ms"].as_u64().unwrap();
    let now_ms = draining["at_ms"].as_u64().unwrap();
    assert!(
        deadline_ms > now_ms,
        "the drain deadline must still be ahead when the hook runs: {draining}"
    );
    assert!(
        deadline_ms <= restart_sent_ms + DRAIN_BUDGET_MS + 1_000,
        "the deadline must come from the configured drain budget: {draining}"
    );
    assert_eq!(events[end_at]["end"], "goodbye", "{:#?}", events);
    assert!(
        draining_at < end_at,
        "on_draining must be called before the GOODBYE that ends the drain: {events:#?}"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event["kind"] == "draining")
            .count(),
        1,
        "one drain notice, one call: {events:#?}"
    );
    daemon.kill_and_wait();
}

/// A module that declares a `Busy` health gauge holds its drain open until the
/// gauge reads 0. Here the gauge reads 1 until `on_draining` has run plus a
/// delay, so the drain can only finish early if the hook ran and the daemon's
/// drain probe read the gauge falling:
///
/// - the module is not torn down (GOODBYE, `on_connection_end`) until after
///   it answered a health probe with the gauge at 0;
/// - the drain (from the hook's call to the module's GOODBYE) lasts at least
///   the delay and ends well before the drain budget, so it was the gauge, not
///   the deadline, that ended the wait;
/// - the route the test holds is closed with `drained: true`, which the daemon
///   reports only when its drain wait saw every declared gauge at zero.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_busy_gauge_released_after_on_draining_holds_the_restart_drain_until_it_reads_zero() {
    const BUSY_AFTER_DRAIN_MS: u64 = 1_500;
    const DRAIN_BUDGET_MS: u64 = 8_000;
    let busy_after_drain = BUSY_AFTER_DRAIN_MS.to_string();
    let (_temp_dir, mut daemon, events_path) = start_drain_harness(
        "subc-client-rs-drain-busy",
        &[("SUBC_MODULE_ECHO_BUSY_AFTER_DRAIN_MS", &busy_after_drain)],
        json!({
            "drain_timeout_ms": DRAIN_BUDGET_MS,
            // The drain re-probes on the health cadence; keep it short so the
            // wait ends soon after the gauge falls.
            "health": { "cadence_ms": 200, "deadline_ms": 150 },
        }),
    )
    .await;
    let drained_pid = first_module_pid(&events_path).await;

    let consumer = SubcConsumer::connect(&daemon.connection_file, fast_consumer_options())
        .await
        .unwrap();
    consumer
        .open_route(
            tool_target(DRAIN_MODULE_ID),
            consumer_identity("drain-busy"),
            fast_call_options(),
        )
        .await
        .unwrap();
    let mut control = consumer.control_pushes(8);

    restart_drain_module(&daemon.connection_file).await;
    // Wait out the whole budget: a drain that is never released ends at its
    // deadline, and the assertions below should say so rather than this wait.
    wait_for_event(
        &events_path,
        Duration::from_millis(DRAIN_BUDGET_MS) + EVENT_TIMEOUT,
        |event| event["pid"].as_u64() == Some(drained_pid) && event["kind"] == "connection_end",
    )
    .await;

    let events = events_of(&events_path, drained_pid);
    let draining_at = position_of(&events, "draining", |event| event["kind"] == "draining");
    let busy_done_at = position_of(&events, "busy_done", |event| event["kind"] == "busy_done");
    let end_at = position_of(&events, "connection_end", |event| {
        event["kind"] == "connection_end"
    });
    assert_eq!(events[end_at]["end"], "goodbye", "{events:#?}");
    assert!(
        draining_at < busy_done_at && busy_done_at < end_at,
        "{events:#?}"
    );
    assert!(
        events[draining_at..busy_done_at]
            .iter()
            .any(|event| event["kind"] == "health" && event["drain_work"] == 1),
        "the drain must have probed the gauge while it was still 1: {events:#?}"
    );
    assert!(
        events[busy_done_at..end_at]
            .iter()
            .any(|event| event["kind"] == "health" && event["drain_work"] == 0),
        "the module must be probed and read 0 before it is torn down: {events:#?}"
    );
    let drain_ms =
        events[end_at]["at_ms"].as_u64().unwrap() - events[draining_at]["at_ms"].as_u64().unwrap();
    assert!(
        drain_ms >= BUSY_AFTER_DRAIN_MS,
        "the drain ended before the gauge could fall: {drain_ms}ms"
    );
    assert!(
        drain_ms < DRAIN_BUDGET_MS - 2_000,
        "the drain ran to its deadline instead of ending on the gauge: {drain_ms}ms"
    );

    let closed = timeout(EVENT_TIMEOUT, async {
        loop {
            let push = control
                .recv()
                .await
                .expect("control push receiver should remain open");
            if push.op == "route.closed" {
                return push;
            }
        }
    })
    .await
    .expect("route.closed should arrive after the drain");
    assert_eq!(closed.body["reason"], "restart", "{:?}", closed.body);
    assert_eq!(
        closed.body["drained"], true,
        "the daemon's drain wait must have seen the gauge at zero: {:?}",
        closed.body
    );
    daemon.kill_and_wait();
}

// scope.sync and scope.describe through the SDK's ModuleHandle, against a
// real daemon.
//
// The daemon gives sync authority only to a module it launched itself, so the
// owner is the `scope-owner` example (crates/subc-client-rs/examples), run
// under the daemon's supervision: it calls `ModuleHandle::scope_sync` and
// `scope_describe` for each step of a script and records each reply, or the
// daemon's refusal code read from `ScopeCallError::Refused`, as a JSON line.
// The second module, which reads the owner's scopes, is served from this
// process, so its replies are checked here as typed values.

const SDK_SCOPE_OWNER: &str = "subc-client-rs-sdk-scope-owner";
const SDK_SCOPE_READER: &str = "subc-client-rs-sdk-scope-reader";

struct ScopeOwnerRun {
    daemon: LiveDaemon,
    _temp_dir: ScratchDir,
    /// One entry per script step, in order: `{"ok": reply}`,
    /// `{"refused": {"code", "message"}}`, or `{"other": error}`.
    results: Vec<Value>,
    /// Requests as executed by the example, including resolved absolute deadlines.
    requests: Vec<Value>,
}

impl ScopeOwnerRun {
    /// A module served from this test process, which never holds sync
    /// authority.
    async fn reader(
        &self,
    ) -> (
        ModuleHandle,
        tokio::task::JoinHandle<Result<(), SubcModuleError>>,
    ) {
        let reader = spawn_inline_module(
            &self.daemon.connection_file,
            inline_module_manifest(SDK_SCOPE_READER, &["a"]),
        )
        .await;
        wait_for_catalog_module(
            &self.daemon.connection_file,
            SDK_SCOPE_READER,
            START_TIMEOUT,
        )
        .await;
        reader
    }
}

/// Start a daemon that supervises the `scope-owner` example with `steps` as
/// its script, and wait until it has run every step.
async fn run_scope_owner(steps: Value) -> ScopeOwnerRun {
    run_scope_owner_with_config(steps, json!({})).await
}

/// [`run_scope_owner`], with the top-level members of `extra_config` added to
/// the daemon's `subc.jsonc`.
async fn run_scope_owner_with_config(steps: Value, extra_config: Value) -> ScopeOwnerRun {
    let workspace = workspace_root();
    let daemon_bin = ensure_binary(
        &workspace,
        &[
            "build",
            "--locked",
            "-p",
            "subc-core",
            "--bin",
            "ck-subc",
            "--message-format=json",
        ],
    );
    let owner_bin = ensure_binary(
        &workspace,
        &[
            "build",
            "--locked",
            "-p",
            "subc-client-rs",
            "--example",
            "scope-owner",
            "--message-format=json",
        ],
    );

    let temp_dir = unique_temp_dir("subc-client-rs-sdk-scopes");
    let runtime_dir = temp_dir.join("runtime");
    let config_dir = temp_dir.join("config");
    let script_path = temp_dir.join("owner-script.json");
    let results_path = temp_dir.join("owner-results.jsonl");
    fs::create_dir_all(&runtime_dir).unwrap();
    fs::create_dir_all(config_dir.join("cortexkit")).unwrap();
    fs::write(&script_path, serde_json::to_vec(&steps).unwrap()).unwrap();
    let env = BTreeMap::from([
        (
            "SUBC_SCOPE_OWNER_SCRIPT".to_string(),
            script_path.to_string_lossy().into_owned(),
        ),
        (
            "SUBC_SCOPE_OWNER_RESULTS".to_string(),
            results_path.to_string_lossy().into_owned(),
        ),
    ]);
    let mut config = json!({
        "version": 1,
        "modules": {
            SDK_SCOPE_OWNER: {
                "program": owner_bin.to_string_lossy(),
                "args": [],
                "env": env,
                "enabled": true,
                "reserved": true,
            }
        }
    });
    for (key, value) in extra_config.as_object().unwrap() {
        config[key] = value.clone();
    }
    fs::write(
        config_dir.join("cortexkit").join("subc.jsonc"),
        serde_json::to_string_pretty(&config).unwrap(),
    )
    .unwrap();

    let daemon = spawn_daemon(&daemon_bin, &runtime_dir, &config_dir);
    wait_for_connection_file(&daemon.connection_file, START_TIMEOUT).await;
    wait_for_event(&results_path, START_TIMEOUT, |event| event["done"] == true).await;
    let mut results = vec![Value::Null; steps.as_array().unwrap().len()];
    let mut requests = vec![Value::Null; results.len()];
    for event in read_events(&results_path) {
        if let Some(step) = event["step"].as_u64() {
            let slot = &mut results[usize::try_from(step).unwrap()];
            // The first run of the script is the one that counts; a restarted
            // owner would run it again under a new connection.
            if slot.is_null() {
                *slot = event["result"].clone();
                requests[usize::try_from(step).unwrap()] = event["request"].clone();
            }
        }
    }
    ScopeOwnerRun {
        daemon,
        _temp_dir: temp_dir,
        results,
        requests,
    }
}

fn sdk_scope_owner() -> Principal {
    Principal::Reserved {
        module_id: SDK_SCOPE_OWNER.to_string(),
    }
}

fn sync_step(generation: u64, scopes: Value) -> Value {
    json!({ "op": "sync", "generation": generation, "scopes": scopes })
}

fn describe_step(owner: &Principal, scope_ref: &str) -> Value {
    json!({ "op": "describe", "owner": owner, "ref": scope_ref })
}

fn head_scope(scope_ref: &str, scope_epoch: u64) -> Value {
    json!({ "ref": scope_ref, "scope_epoch": scope_epoch, "kind": "head" })
}

/// The JSON the `scope-owner` example records for a describe reply, so a
/// reply read here can be compared with one the owner read.
fn describe_reply_json(reply: &ScopeDescribeReply) -> Value {
    json!({
        "status": reply.status,
        "scope_epoch": reply.scope_epoch,
        "daemon_incarnation": reply.daemon_incarnation,
        "owner_synced": reply.owner_synced,
        "owner_configured": reply.owner_configured,
        "scope": reply.scope,
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_owner_syncs_a_scope_and_both_it_and_another_module_describe_it_live() {
    let mut run = run_scope_owner(json!([
        sync_step(1, json!([head_scope("session-a", 3)])),
        describe_step(&sdk_scope_owner(), "session-a"),
    ]))
    .await;

    let sync = &run.results[0]["ok"];
    assert_eq!(sync["generation"], 1, "{:?}", run.results);
    assert_eq!(sync["results"].as_array().unwrap().len(), 1, "{sync}");
    assert_eq!(sync["results"][0]["ref"], "session-a");
    assert_eq!(sync["results"][0]["scope_epoch"], 3);
    assert_eq!(sync["results"][0]["outcome"], "created");
    assert_eq!(sync["ended"], json!([]));

    let owner_view = &run.results[1]["ok"];
    assert_eq!(owner_view["status"], "live", "{:?}", run.results);
    assert_eq!(owner_view["scope_epoch"], 3);
    assert_eq!(owner_view["owner_synced"], true);
    assert_eq!(owner_view["owner_configured"], true);
    assert_eq!(owner_view["scope"]["owner"], json!(sdk_scope_owner()));
    assert_eq!(owner_view["scope"]["ref"], "session-a");
    assert_eq!(owner_view["scope"]["scope_epoch"], 3);
    assert_eq!(owner_view["scope"]["kind"], "head");

    let (reader, serve_task) = run.reader().await;
    let reader_view = reader
        .scope_describe(sdk_scope_owner(), "session-a".to_string())
        .await
        .unwrap();
    assert_eq!(reader_view.status, ScopeStatus::Live);
    assert_eq!(reader_view.scope_epoch, Some(3));
    assert_eq!(
        reader_view.scope.as_ref().map(|stamp| stamp.scope_epoch),
        Some(3)
    );
    assert_eq!(
        &describe_reply_json(&reader_view),
        owner_view,
        "a module that is not the owner reads the same answer"
    );

    run.daemon.kill_and_wait();
    assert!(serve_task.await.unwrap().is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_scope_sync_carries_the_daemons_typed_code() {
    let mut run = run_scope_owner(json!([
        sync_step(5, json!([head_scope("session-a", 1)])),
        // Generation 5 again, not larger than the last accepted one: refused.
        sync_step(5, json!([head_scope("session-a", 2)])),
        // Generation 4 is smaller still. Had it applied, its empty set would
        // have ended session-a.
        sync_step(4, json!([])),
        describe_step(&sdk_scope_owner(), "session-a"),
    ]))
    .await;

    assert_eq!(run.results[0]["ok"]["generation"], 5, "{:?}", run.results);
    for step in [1, 2] {
        assert_eq!(
            run.results[step]["refused"]["code"],
            subc_protocol::error_codes::SCOPE_SYNC_STALE,
            "step {step}: {:?}",
            run.results
        );
    }
    let after = &run.results[3]["ok"];
    assert_eq!(after["status"], "live", "a refused sync changes nothing");
    assert_eq!(after["scope_epoch"], 1);

    // A module the daemon did not launch has no sync authority; the refusal
    // reaches this caller with the code as the typed value.
    let (reader, serve_task) = run.reader().await;
    let error = reader
        .scope_sync(1, vec![serde_json::from_value(head_scope("x", 1)).unwrap()])
        .await
        .unwrap_err();
    assert!(
        matches!(
            &error,
            ScopeCallError::Refused { code, .. }
                if code == subc_protocol::error_codes::SCOPE_SYNC_NOT_AUTHORITY
        ),
        "{error:?}"
    );
    assert_eq!(
        error.code(),
        Some(subc_protocol::error_codes::SCOPE_SYNC_NOT_AUTHORITY)
    );

    run.daemon.kill_and_wait();
    assert!(serve_task.await.unwrap().is_ok());
}

const SDK_SCOPE_PROVIDER: &str = "subc-client-rs-sdk-scope-provider";

/// A module's own tests build the `RouteBindRequest` its `on_bind` receives
/// through the public constructor and setters, from outside this crate.
#[test]
fn a_route_bind_request_is_built_through_its_constructor_and_setters() {
    let handle = RouteHandle::detached(7, 2);
    let target = RouteTarget::ToolProvider {
        module_id: "aft".to_string(),
    };
    let identity = BindIdentity::new("/tmp/project", "opencode", "session-1");

    let bare = RouteBindRequest::new(handle, target.clone(), identity.clone());
    assert_eq!(bare.handle, handle);
    assert_eq!((bare.handle.channel, bare.handle.epoch), (7, 2));
    assert_eq!(bare.target, target);
    assert_eq!(bare.identity, identity);
    assert_eq!(bare.principal, None);
    assert_eq!(bare.consumer_capabilities, None);
    assert_eq!(bare.role_versions, None);
    assert_eq!(bare.admission_facts, None);
    assert_eq!(bare.scope, None);

    let stamp: ScopeStamp = serde_json::from_value(json!({
        "owner": { "kind": "reserved", "module_id": "prefrontal-core" },
        "ref": "session-a",
        "scope_epoch": 3,
        "kind": "head",
        "attributes": { "agent_id": "agent-7" },
        "owner_authorized": true,
    }))
    .unwrap();
    let principal = Principal::Reserved {
        module_id: "broca".to_string(),
    };
    let role_versions = BTreeMap::from([("tool-provider".to_string(), "v1".to_string())]);
    let full = RouteBindRequest::new(handle, target, identity)
        .with_principal(principal.clone())
        .with_consumer_capabilities(vec!["elicitation".to_string()])
        .with_role_versions(role_versions.clone())
        .with_admission_facts(json!({ "schema": 1 }))
        .with_scope(stamp.clone());
    assert_eq!(full.principal, Some(principal));
    assert_eq!(
        full.consumer_capabilities,
        Some(vec!["elicitation".to_string()])
    );
    assert_eq!(full.role_versions, Some(role_versions));
    assert_eq!(full.admission_facts, Some(json!({ "schema": 1 })));
    assert_eq!(full.scope, Some(stamp));
}

/// A detached handle belongs to no connection: every operation that would
/// reach one fails with the stale-route error and sends nothing, even against
/// a live daemon with a live module and consumer on the other end.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_operation_on_a_detached_route_handle_fails_as_a_stale_route() {
    let workspace = workspace_root();
    let daemon_bin = ensure_binary(
        &workspace,
        &[
            "build",
            "--locked",
            "-p",
            "subc-core",
            "--bin",
            "ck-subc",
            "--message-format=json",
        ],
    );
    let temp_dir = unique_temp_dir("subc-client-rs-detached-handle");
    let runtime_dir = temp_dir.join("runtime");
    let config_dir = temp_dir.join("config");
    fs::create_dir_all(&runtime_dir).unwrap();
    write_empty_config(&config_dir);
    let mut daemon = spawn_daemon(&daemon_bin, &runtime_dir, &config_dir);
    wait_for_connection_file(&daemon.connection_file, START_TIMEOUT).await;

    let module_id = "subc-client-rs-detached-handle";
    let (module, serve_task) = spawn_inline_module(
        &daemon.connection_file,
        inline_module_manifest(module_id, &["a"]),
    )
    .await;
    wait_for_catalog_module(&daemon.connection_file, module_id, START_TIMEOUT).await;
    let consumer = SubcConsumer::connect(&daemon.connection_file, fast_consumer_options())
        .await
        .unwrap();

    // Channel 1, epoch 1 is the first route either side would be given, so a
    // check that compared only the wire pair could mistake this for a live one.
    let detached = RouteHandle::detached(1, 1);
    // A real route on the same consumer, so the connection under test is live.
    let live = consumer
        .open_route(
            RouteTarget::ToolProvider {
                module_id: module_id.to_string(),
            },
            BindIdentity::new(temp_dir.join("project"), "test", "detached"),
            fast_call_options(),
        )
        .await
        .unwrap();
    assert_ne!(live, detached);

    let stale = |label: &str, error: CallError| {
        assert!(
            matches!(error, CallError::StaleRouteHandle(handle) if handle == detached),
            "{label}: {error:?}"
        );
    };
    stale(
        "request",
        consumer
            .request(&detached, b"{}".to_vec(), fast_call_options())
            .await
            .unwrap_err(),
    );
    stale(
        "subscribe_route",
        consumer
            .subscribe_route(&detached, b"{}".to_vec(), SubscribeOptions::default())
            .await
            .err()
            .expect("subscribe_route on a detached handle fails"),
    );
    stale(
        "poll_route",
        consumer
            .poll_route(&detached, PollKind::Status, Duration::from_secs(2))
            .await
            .unwrap_err(),
    );
    stale(
        "push_events",
        consumer
            .push_events(&detached)
            .expect_err("push_events on a detached handle fails"),
    );
    stale(
        "close_handle",
        consumer
            .close_handle(&detached, CloseRouteOptions::default())
            .await
            .unwrap_err(),
    );

    let push = module
        .push(&detached, b"{}".to_vec(), None)
        .await
        .unwrap_err();
    assert!(
        matches!(push, SubcModuleError::StaleRouteHandle(handle) if handle == detached),
        "push: {push:?}"
    );

    assert!(matches!(
        detached.on_request("elicitation", |_body, _ctx| async { Vec::new() }),
        Err(ReverseRequestRegistrationError::NotConsumerRoute)
    ));
    assert!(matches!(
        detached.on_request_fallible("elicitation", |_body, _ctx| async { Ok(Vec::new()) }),
        Err(ReverseRequestRegistrationError::NotConsumerRoute)
    ));

    // The live route still works, so the refusals above were about the
    // handle, not a dead connection.
    let echoed = consumer
        .request(&live, b"still-live".to_vec(), fast_call_options())
        .await
        .unwrap();
    assert_eq!(echoed, b"still-live");

    drop(consumer);
    daemon.kill_and_wait();
    assert!(serve_task.await.unwrap().is_ok());
}

/// One bind's session and the scope stamp it carried.
type RecordedBind = (String, Option<ScopeStamp>);

/// Records the scope stamp each bind carried, keyed by the bind's session.
#[derive(Clone, Default)]
struct ScopeRecordingHandler {
    binds: Arc<Mutex<Vec<RecordedBind>>>,
}

impl ScopeRecordingHandler {
    fn stamp_for(&self, session: &str) -> Option<Option<ScopeStamp>> {
        self.binds
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .find(|(bound_session, _)| bound_session == session)
            .map(|(_, stamp)| stamp.clone())
    }
}

#[async_trait]
impl ModuleHandler for ScopeRecordingHandler {
    async fn handle(&self, _ctx: RequestCtx, body: Vec<u8>) -> HandlerOutcome {
        HandlerOutcome::Response(body)
    }

    async fn on_bind(&self, req: &RouteBindRequest) -> BindDecision {
        self.binds
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push((req.identity.session.clone(), req.scope.clone()));
        BindDecision::accept()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_applied_run_scope_expires_and_closes_its_route_without_an_owner_resync() {
    let mut run = run_scope_owner_with_config(
        json!([
            sync_step(1, json!([])),
            {
                "op": "apply",
                "generation": 2,
                "upsert": [{
                    "ref": "run-a",
                    "scope_epoch": 1,
                    "kind": "ephemeral",
                    "expires_in_ms": 15000,
                    "carriers": [{ "principal": { "kind": "direct" } }],
                    "attributes": { "agent_id": "agent-7", "run_id": "run-7" }
                }],
                "end": []
            }
        ]),
        json!({ "scope_authority_owners": [SDK_SCOPE_OWNER] }),
    )
    .await;
    assert_eq!(run.results[0]["ok"]["generation"], 1, "{:?}", run.results);
    assert_eq!(run.results[1]["ok"]["generation"], 2, "{:?}", run.results);
    assert_eq!(
        run.results[1]["ok"]["results"][0]["outcome"], "created",
        "{:?}",
        run.results
    );
    let sent_record = &run.requests[1]["upsert"][0];
    assert!(sent_record.get("expires_in_ms").is_none());
    let expires_at_ms = sent_record["expires_at_ms"]
        .as_u64()
        .expect("the example records the absolute deadline it sent");
    let closes_by_ms = expires_at_ms + 2000;

    // Registration happens after the owner's last call. The run gate reads
    // the provider's advertised capability when the consumer opens the route.
    let (_provider, serve_task) = spawn_inline_module(
        &run.daemon.connection_file,
        inline_capability_module_manifest("run-provider", Some("agent-run-scopes/v1")),
    )
    .await;
    wait_for_catalog_module(&run.daemon.connection_file, "run-provider", START_TIMEOUT).await;
    let consumer = SubcConsumer::connect(&run.daemon.connection_file, fast_consumer_options())
        .await
        .unwrap();
    let mut pushes = consumer.control_pushes(16);
    let identity = consumer_identity("run-expiry");
    assert!(
        expires_at_ms.saturating_sub(unix_ms_now()) >= 2000,
        "harness fault: fewer than 2000 ms remain before the recorded expiry at route.open"
    );
    let handle = consumer
        .open_route_scoped(
            tool_target("run-provider"),
            identity,
            ScopeSelector {
                owner: sdk_scope_owner(),
                scope_ref: "run-a".to_string(),
                scope_epoch: Some(1),
            },
            fast_call_options(),
        )
        .await
        .expect("a live run scope admits an opted-in provider");

    // Use the recorded wall deadline, not EVENT_TIMEOUT: an early close or
    // an expiry loop taking more than two seconds after the deadline is wrong.
    let closed = loop {
        assert!(
            unix_ms_now() <= closes_by_ms,
            "route.closed missed the expiry bound"
        );
        let push = timeout(
            Duration::from_millis(closes_by_ms.saturating_sub(unix_ms_now())),
            pushes.recv(),
        )
        .await
        .expect("route.closed must arrive by expires_at_ms + 2000")
        .expect("control push receiver stays open");
        if push.op == "route.closed" {
            let received_at_ms = unix_ms_now();
            assert!(
                received_at_ms >= expires_at_ms,
                "route.closed arrived before expiry"
            );
            assert!(
                received_at_ms <= closes_by_ms,
                "route.closed missed the expiry bound"
            );
            break push;
        }
    };
    assert_eq!(
        closed.body["channels"],
        json!([handle.channel]),
        "{}",
        closed.body
    );
    assert_eq!(closed.body["reason"], "scope_ended", "{}", closed.body);
    assert_eq!(
        closed.route_close_reason().unwrap().disposition(),
        RouteCloseDisposition::MustNotReopen
    );

    // The channel GOODBYE and control push are separate frames. Require both
    // effects inside the same wall-time bound, even if the push arrives first.
    loop {
        assert!(
            unix_ms_now() <= closes_by_ms,
            "the expired route handle is still live"
        );
        let result = timeout(
            Duration::from_millis(closes_by_ms.saturating_sub(unix_ms_now())),
            consumer.request(
                &handle,
                br#"{"tool":"capability.resolve","arguments":{}}"#.to_vec(),
                fast_call_options(),
            ),
        )
        .await
        .expect("the route handle must become stale by expires_at_ms + 2000");
        if matches!(result, Err(CallError::StaleRouteHandle(_))) {
            assert!(
                unix_ms_now() <= closes_by_ms,
                "GOODBYE missed the expiry bound"
            );
            break;
        }
        sleep(Duration::from_millis(
            20.min(closes_by_ms.saturating_sub(unix_ms_now())),
        ))
        .await;
    }

    consumer.close().await;
    run.daemon.kill_and_wait();
    assert!(serve_task.await.unwrap().is_ok());
}

/// A module served through the SDK sees the daemon's scope stamp in `on_bind`,
/// exactly as the daemon stamped it, and sees none on an unscoped route.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_served_modules_on_bind_receives_the_daemons_scope_stamp() {
    // The owner sets `agent_id`, which the daemon accepts only from an owner
    // listed in its `scope_authority_owners` config, hence the extra config.
    // It lists the `direct` principal as a carrier: this test process connects
    // with the daemon's key and no launch nonce, so the daemon stamps it
    // `direct`, and only the owner or a listed carrier may open a scoped route.
    let mut run = run_scope_owner_with_config(
        json!([
            sync_step(
                1,
                json!([{
                    "ref": "session-a",
                    "scope_epoch": 3,
                    "kind": "head",
                    "carriers": [{ "principal": { "kind": "direct" } }],
                    "attributes": { "agent_id": "agent-7" },
                }])
            ),
            describe_step(&sdk_scope_owner(), "session-a"),
        ]),
        json!({ "scope_authority_owners": [SDK_SCOPE_OWNER] }),
    )
    .await;
    assert_eq!(
        run.results[0]["ok"]["results"][0]["outcome"], "created",
        "{:?}",
        run.results
    );
    let described: ScopeStamp =
        serde_json::from_value(run.results[1]["ok"]["scope"].clone()).unwrap();

    let handler = ScopeRecordingHandler::default();
    let (_provider, serve_task) = spawn_inline_module_with_handler(
        &run.daemon.connection_file,
        inline_module_manifest(SDK_SCOPE_PROVIDER, &["a"]),
        handler.clone(),
    )
    .await;
    wait_for_catalog_module(
        &run.daemon.connection_file,
        SDK_SCOPE_PROVIDER,
        START_TIMEOUT,
    )
    .await;

    let consumer = SubcConsumer::connect(&run.daemon.connection_file, fast_consumer_options())
        .await
        .unwrap();
    let target = RouteTarget::ToolProvider {
        module_id: SDK_SCOPE_PROVIDER.to_string(),
    };
    let identity =
        |session: &str| BindIdentity::new(run._temp_dir.join("project"), "test", session);
    fs::create_dir_all(run._temp_dir.join("project")).unwrap();
    consumer
        .open_route_scoped(
            target.clone(),
            identity("scoped"),
            ScopeSelector {
                owner: sdk_scope_owner(),
                scope_ref: "session-a".to_string(),
                scope_epoch: Some(3),
            },
            fast_call_options(),
        )
        .await
        .unwrap();
    consumer
        .open_route(target, identity("unscoped"), fast_call_options())
        .await
        .unwrap();

    let stamp = handler
        .stamp_for("scoped")
        .expect("the scoped route was bound")
        .expect("on_bind must receive the daemon's scope stamp");
    assert_eq!(stamp.owner, sdk_scope_owner());
    assert_eq!(stamp.scope_ref, "session-a");
    assert_eq!(stamp.scope_epoch, 3);
    assert_eq!(stamp.kind, ScopeKind::Head);
    assert_eq!(stamp.attributes.agent_id.as_deref(), Some("agent-7"));
    assert!(stamp.owner_authorized);
    assert_eq!(
        stamp, described,
        "the bind carries the daemon's stamp unchanged"
    );
    assert_eq!(
        handler.stamp_for("unscoped"),
        Some(None),
        "an unscoped route is bound with no stamp"
    );

    drop(consumer);
    run.daemon.kill_and_wait();
    assert!(serve_task.await.unwrap().is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn describing_an_unknown_ref_answers_not_live_with_the_owners_state() {
    let mut run = run_scope_owner(json!([
        sync_step(1, json!([head_scope("session-a", 1)])),
        describe_step(&sdk_scope_owner(), "never-synced"),
    ]))
    .await;

    // The owner has synced in this daemon incarnation and is configured, so a
    // ref it never synced is `not_live` with both flags set: a reader treats it
    // as gone, not as waiting for the owner to re-sync after a restart.
    let owner_view = &run.results[1]["ok"];
    assert_eq!(owner_view["status"], "not_live", "{:?}", run.results);
    assert_eq!(owner_view["scope_epoch"], Value::Null);
    assert_eq!(owner_view["owner_synced"], true);
    assert_eq!(owner_view["owner_configured"], true);
    assert_eq!(owner_view["scope"], Value::Null);

    // An owner the daemon does not supervise has neither synced nor ever will.
    let (reader, serve_task) = run.reader().await;
    let unknown_owner = reader
        .scope_describe(
            Principal::Reserved {
                module_id: "subc-client-rs-no-such-owner".to_string(),
            },
            "session-a".to_string(),
        )
        .await
        .unwrap();
    assert_eq!(unknown_owner.status, ScopeStatus::NotLive);
    assert_eq!(unknown_owner.scope_epoch, None);
    assert!(!unknown_owner.owner_synced);
    assert!(!unknown_owner.owner_configured);
    assert_eq!(unknown_owner.scope, None);

    run.daemon.kill_and_wait();
    assert!(serve_task.await.unwrap().is_ok());
}

// These tests share the wire helpers but have a separate, feature-enabled daemon.
#[path = "real_daemon/operator_confirm.rs"]
mod operator_confirm;
