#![forbid(unsafe_code)]

use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    fs, io,
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};

use serde_json::{json, Value};
use subc_client_rs::{
    async_trait, serve_with_handle, CallError, CallOptions, CatalogUpdateError, CloseRouteOptions,
    ConsumerOptions, HandlerOutcome, ModuleHandle, ModuleHandler, PolicyResolveError,
    PolicyResolver, PolicyResolverConfig, PolicyVerdict, ProjectRef, RequestCtx, RetryBackoff,
    RouteHandle, SubcConsumer, SubcModuleError, Subject, SubscribeOptions,
};
use subc_control::{ClientControlRequest, ClientControlResponse};
use subc_protocol::{
    manifest::{
        Bindings, CapabilityDeclarations, Concurrency, ExecutionMode, IdentityBinding,
        IdentityScope, ManagementOperation, ManagementOperationKind, ModuleManifest, ProviderRole,
        StorageBinding, StorageKind, StorageScope, Tool, TrustTier,
    },
    session::HealthStatus,
    BindIdentity, ErrorBody, Flags, Frame, FrameType, Priority, RouteTarget,
};
use subc_transport::{authenticate_client, read_frame, write_frame};
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
    time::{sleep, timeout, Instant},
};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

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
        let _ = fs::remove_dir_all(&self.runtime_dir);
        let _ = fs::remove_dir_all(&self.config_dir);
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
        binary_path(&workspace, "ck-subc"),
        &["build", "-p", "subc-core", "--bins"],
    );
    let fake_aft_bin = ensure_binary(
        &workspace,
        binary_path(&workspace, "fake-aft-stub"),
        &["build", "-p", "subc-core", "--bin", "fake-aft-stub"],
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn capability_resolver_does_not_fall_back_to_module_id_equality_and_orders_claimants() {
    let workspace = workspace_root();
    let daemon_bin = ensure_binary(
        &workspace,
        binary_path(&workspace, "ck-subc"),
        &["build", "-p", "subc-core", "--bins"],
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
        binary_path(&workspace, "ck-subc"),
        &["build", "-p", "subc-core", "--bins"],
    );
    let module_bin = ensure_binary(
        &workspace,
        example_path(&workspace, "echo-module"),
        &["build", "-p", "subc-client-rs", "--example", "echo-module"],
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn module_handle_catalog_update_refreshes_catalog_without_dropping_open_routes() {
    let workspace = workspace_root();
    let daemon_bin = ensure_binary(
        &workspace,
        binary_path(&workspace, "ck-subc"),
        &["build", "-p", "subc-core", "--bins"],
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
async fn module_handle_catalog_update_surfaces_frozen_field_rejections() {
    let workspace = workspace_root();
    let daemon_bin = ensure_binary(
        &workspace,
        binary_path(&workspace, "ck-subc"),
        &["build", "-p", "subc-core", "--bins"],
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
        binary_path(&workspace, "ck-subc"),
        &["build", "-p", "subc-core", "--bins"],
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
        binary_path(&workspace, "ck-subc"),
        &["build", "-p", "subc-core", "--bins"],
    );
    let module_bin = ensure_binary(
        &workspace,
        example_path(&workspace, "echo-module"),
        &["build", "-p", "subc-client-rs", "--example", "echo-module"],
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
async fn subc_consumer_retries_unknown_module_until_provider_registers_and_bounds_absence() {
    let workspace = workspace_root();
    let daemon_bin = ensure_binary(
        &workspace,
        binary_path(&workspace, "ck-subc"),
        &["build", "-p", "subc-core", "--bins"],
    );
    let module_bin = ensure_binary(
        &workspace,
        example_path(&workspace, "echo-module"),
        &["build", "-p", "subc-client-rs", "--example", "echo-module"],
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

    let racing_call = {
        let consumer = Arc::clone(&consumer);
        let identity = identity.clone();
        tokio::spawn(async move {
            consumer
                .call(
                    tool_target(CONSUMER_MODULE_A),
                    identity,
                    br#"{"kind":"unary","value":"after-register"}"#.to_vec(),
                    fast_call_options(),
                )
                .await
        })
    };
    sleep(Duration::from_millis(250)).await;
    let restarted_events = temp_dir.join("restarted-provider.jsonl");
    let _provider = spawn_provider(
        &module_bin,
        &daemon.connection_file,
        CONSUMER_MODULE_A,
        &restarted_events,
    );
    wait_for_catalog_module(&daemon.connection_file, CONSUMER_MODULE_A, START_TIMEOUT).await;
    let raced = racing_call.await.unwrap().unwrap();
    assert_eq!(json_body(&raced)["echo"]["value"], "after-register");

    let absent = consumer
        .call(
            tool_target("subc-client-rs-never-registers"),
            identity,
            br#"{"kind":"unary","value":"missing"}"#.to_vec(),
            bounded_absence_options(),
        )
        .await;
    assert!(
        matches!(absent, Err(CallError::NotSent(_))),
        "bounded target absence should terminate as NotSent, got {absent:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subc_consumer_multiplexes_targets_and_classifies_reconnect_in_flight() {
    let workspace = workspace_root();
    let daemon_bin = ensure_binary(
        &workspace,
        binary_path(&workspace, "ck-subc"),
        &["build", "-p", "subc-core", "--bins"],
    );
    let module_bin = ensure_binary(
        &workspace,
        example_path(&workspace, "echo-module"),
        &["build", "-p", "subc-client-rs", "--example", "echo-module"],
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
        binary_path(&workspace, "ck-subc"),
        &["build", "-p", "subc-core", "--bins"],
    );
    let module_bin = ensure_binary(
        &workspace,
        example_path(&workspace, "echo-module"),
        &["build", "-p", "subc-client-rs", "--example", "echo-module"],
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
        binary_path(&workspace, "ck-subc"),
        &["build", "-p", "subc-core", "--bins"],
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
        binary_path(&workspace, "ck-subc"),
        &["build", "-p", "subc-core", "--bins"],
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
        binary_path(&workspace, "ck-subc"),
        &["build", "-p", "subc-core", "--bins"],
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
        binary_path(&workspace, "ck-subc"),
        &["build", "-p", "subc-core", "--bins"],
    );
    let module_bin = ensure_binary(
        &workspace,
        example_path(&workspace, "echo-module"),
        &["build", "-p", "subc-client-rs", "--example", "echo-module"],
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
    ModuleManifest::builder(
        module_id,
        env!("CARGO_PKG_VERSION"),
        TrustTier::FirstParty,
        Bindings {
            storage: StorageBinding {
                kind: StorageKind::Sqlite,
                scope: StorageScope::Project,
                owns_schema: false,
            },
            vault_grants: Vec::new(),
            identity: IdentityBinding {
                requires: vec![IdentityScope::Project],
                optional: vec![IdentityScope::Session],
            },
        },
    )
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
    Command::new(daemon_bin)
        .env_remove(subc_protocol::SUBC_MODULE_ID_ENV)
        .env_remove(subc_protocol::SUBC_LAUNCH_NONCE_ENV)
        .env("XDG_RUNTIME_DIR", runtime_dir)
        .env("XDG_CONFIG_HOME", config_dir)
        .env("SUBC_PORT", "0")
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
    let child = Command::new(module_bin)
        .arg("--subc")
        .arg(connection_file)
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

fn bounded_absence_options() -> CallOptions {
    CallOptions {
        timeout: Duration::from_secs(2),
        route_retry: RetryBackoff {
            base: Duration::from_millis(25),
            cap: Duration::from_millis(50),
            max_attempts: 4,
        },
        route_retry_deadline: Duration::from_millis(300),
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
    fs::create_dir_all(&project_root).unwrap();
    BindIdentity {
        project_root,
        harness: "subc-client-rs-consumer-test".to_string(),
        session: session.to_string(),
    }
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
        binary_path(&workspace, "ck-subc"),
        &["build", "-p", "subc-core", "--bins"],
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

fn policy_project_root() -> String {
    let root = unique_temp_dir("subc-client-rs-policy-project");
    fs::create_dir_all(&root).unwrap();
    root.to_string_lossy().into_owned()
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
    loop {
        if catalog_modules(path, Some(module_id), corr).await.len() == 1 {
            return;
        }
        if Instant::now() >= deadline {
            panic!("module {module_id} did not register in catalog within {wait:?}");
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
    let mut client = connect_authed_client(path).await.unwrap();
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
    response["modules"].as_array().cloned().unwrap_or_default()
}

async fn open_route<S>(stream: &mut S, module_id: &str, corr: u64) -> (u16, u32)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let project_root = unique_temp_dir("subc-client-rs-project");
    fs::create_dir_all(&project_root).unwrap();
    let response = control_rpc_on_stream(
        stream,
        corr,
        json!({
            "op": "route.open",
            "target": RouteTarget::ToolProvider {
                module_id: module_id.to_string(),
            },
            "identity": BindIdentity {
                project_root,
                harness: "subc-client-rs-test".to_string(),
                session: "clean-api".to_string(),
            },
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
    assert_eq!(response.header.ty, FrameType::Response);
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

fn ensure_binary(workspace: &Path, path: PathBuf, cargo_args: &[&str]) -> PathBuf {
    static BUILD_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let _guard = BUILD_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let output = Command::new("cargo")
        .args(cargo_args)
        .current_dir(workspace)
        .output()
        .unwrap_or_else(|error| panic!("failed to run cargo {cargo_args:?}: {error}"));
    if !output.status.success() {
        panic!(
            "cargo {cargo_args:?} failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert!(path.exists(), "expected binary at {}", path.display());
    path
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .unwrap()
        .to_path_buf()
}

fn binary_path(workspace: &Path, name: &str) -> PathBuf {
    workspace
        .join("target")
        .join("debug")
        .join(format!("{name}{}", std::env::consts::EXE_SUFFIX))
}

fn example_path(workspace: &Path, name: &str) -> PathBuf {
    workspace
        .join("target")
        .join("debug")
        .join("examples")
        .join(format!("{name}{}", std::env::consts::EXE_SUFFIX))
}

fn unique_temp_dir(name: &str) -> PathBuf {
    let nonce = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("{name}-{}-{nonce}", std::process::id()))
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
        binary_path(&workspace, "ck-subc"),
        &["build", "-p", "subc-core", "--bins"],
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
        binary_path(&workspace, "ck-subc"),
        &["build", "-p", "subc-core", "--bins"],
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
        binary_path(&workspace, "ck-subc"),
        &["build", "-p", "subc-core", "--bins"],
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
        binary_path(&workspace, "ck-subc"),
        &["build", "-p", "subc-core", "--bins"],
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
    let project_root = policy_project_root();
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
    let project_root = policy_project_root();
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
    let project_root = policy_project_root();
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
    let project_root = policy_project_root();

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
    assert_eq!(harness.handler.calls(), 1);

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
    let project_root = policy_project_root();

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
    assert_eq!(harness.handler.calls(), 2);

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
    let project_root = policy_project_root();
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
