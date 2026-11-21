include!("agent_commands_tests.rs");
include!("supervisor_health_vectors_tests.rs");
include!("resource_revision_tests.rs");
include!("route_open_recovery_tests.rs");
mod router_audit {
    use super::*;
    include!("router_audit_tests.rs");
}
#[tokio::test]
async fn route_busy_wait_attribution_resolves_targets_instead_of_trusting_selectors() {
    let registry = Arc::new(Registry::default());
    let handler = ControlHandler::new(registry);
    assert_eq!(
        handler.resolve_reader_wait_target(ops::SUPERVISOR_HEALTH_PROBE, Some("daemon")),
        None
    );
    let (ctx, mut rx) = route_ctx(ConnectionId::new(901));
    hello_via_sink(
        &handler,
        &ctx,
        &mut rx,
        hello_frame("daemon", PROTOCOL_VERSION, 1),
    )
    .await;
    assert_eq!(
        handler.resolve_reader_wait_target(ops::SUPERVISOR_HEALTH_PROBE, Some("daemon")),
        Some("daemon".into())
    );
    assert_eq!(
        handler.resolve_reader_wait_target(ops::SUPERVISOR_HEALTH_PROBE, Some("missing")),
        None
    );
    assert_eq!(
        handler.resolve_reader_wait_target(ops::SUPERVISOR_TERMINALS, Some("daemon")),
        None,
        "a registered module is not necessarily supervised"
    );
    assert_eq!(
        handler.resolve_reader_wait_target(ops::SUPERVISOR_PROVENANCE, None),
        None,
        "aggregate provenance has no single target"
    );
    assert_eq!(
        handler.resolve_reader_wait_target(ops::APP_OPEN, Some("daemon")),
        None
    );
}

mod peer_manifest {
    include!("../peer_manifest_tests.rs");
}
mod catalog_declarations {
    include!("catalog_tests.rs");
}
mod activity_read {
    include!("../activity_read_tests.rs");
}

mod lifecycle_activity {
    include!("../control_lifecycle_tests.rs");
}
#[cfg(unix)]
#[tokio::test]
async fn rescan_health_only_is_live_but_launch_edits_need_reload() {
    let dir = cortexkit_test_support::ScratchDir::new("rescan-live-health");
    let path = dir.join("subc.jsonc");
    std::fs::write(&path, serde_json::json!({"version":1,"modules":{"stock":{
            "program":"/bin/sleep","args":["60"],"protocol":"none",
            "env":{"XDG_DATA_HOME":dir.path(),"XDG_RUNTIME_DIR":dir.path(),"XDG_CONFIG_HOME":dir.path()}
        }}}).to_string()).unwrap();
    let mut configured = crate::daemon_config::load(&path)
        .unwrap()
        .unwrap()
        .modules
        .pop()
        .unwrap();
    let registry = std::sync::Arc::new(crate::Registry::default());
    let handle = crate::SupervisorHandle::new();
    let supervisor = crate::Supervisor::new(registry.clone(), crate::RestartPolicy::default())
        .with_handle(handle.clone());
    let module = supervisor
        .supervise_configured_with_health(
            configured.module_spec(),
            true,
            configured.health.clone(),
            None,
            configured.restart,
        )
        .unwrap();
    let handler = super::ControlHandler::new(registry).with_supervisor(handle);
    let before = module.status().unwrap().pid;
    configured.health.http = Some("http://127.0.0.1:1/healthz".into());
    configured.health.cadence = std::time::Duration::from_secs(3600);
    let health_only = handler
        .reconcile_supervised_modules(
            &supervisor,
            vec![configured.clone()],
            false,
            Principal::Direct,
        )
        .await
        .unwrap();
    assert!(
        health_only.0.changed_pending_reload.is_empty(),
        "health policy is already applied live"
    );
    assert_eq!(module.status().unwrap().pid, before);
    assert_eq!(
        module.configuration().unwrap().1.http,
        configured.health.http
    );
    configured.args = vec!["61".into()];
    let launch = handler
        .reconcile_supervised_modules(&supervisor, vec![configured], false, Principal::Direct)
        .await
        .unwrap();
    assert_eq!(launch.0.changed_pending_reload, ["stock"]);
    assert_eq!(
        module.status().unwrap().pid,
        before,
        "a launch edit is stored until reload"
    );
    module.drain().await.unwrap();
}
use cortexkit_test_support::ScratchDir;
use std::{
    collections::BTreeMap,
    fmt,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use serde_json::{json, Value};
use subc_protocol::{
    manifest::{
        Concurrency, ExecutionMode, IdentityScope, ManagementOperation, ManagementOperationKind,
        ObservabilityKind, ObservabilitySurface, ProviderRole, Tool,
    },
    session::HealthStatus,
    FrameType,
};

use super::*;
use crate::{
    forwarding::{DataRoute, DataRouteState},
    registry::ChannelState,
    router::FrameSink,
    stderr_tail::DEFAULT_MAX_LINE_BYTES,
    supervise::{ModuleSpec, ModuleState, RestartPolicy, Supervisor, SupervisorHandle},
    RouteCtx, Router,
};
use tokio::{
    sync::mpsc,
    time::{sleep, Instant},
};
use tracing::{
    field::{Field, Visit},
    Event, Subscriber,
};
use tracing_subscriber::{layer::Context, prelude::*, Layer};

/// Locates the `fake-aft-stub` binary from a `src/lib.rs` unit test.
///
/// `CARGO_BIN_EXE_*` (compile-time `env!` and runtime `std::env::var` alike)
/// is only populated for `tests/*.rs` integration test binaries -- this file
/// compiles as part of the library target, which gets neither. The binary
/// lives in the profile folder this test was built into,
/// `<target-dir>/<profile>/fake-aft-stub`, which `cargo_profile_bin` finds
/// from the test's own path.
///
/// THE BINARY IS NOT ALWAYS THERE, and the existence check below is why.
/// `cargo test -p subc-core` builds every target including `[[bin]]`, so the
/// stub is on disk; `cargo test -p subc-core --lib` builds ONLY the library
/// test and leaves the stub unbuilt. A bare spawn then fails with a raw
/// `NotFound`, which reads as a broken test rather than an unbuilt
/// dependency -- so state the cause and the remedy instead. Deliberately a
/// panic and not a silent skip: a test that quietly passes when it could not
/// run is worse than one that fails, because it reports health it never
/// verified.
fn fake_aft_stub_path() -> PathBuf {
    let path = cortexkit_test_support::cargo_profile_bin("fake-aft-stub");
    assert!(
        path.exists(),
        "fake-aft-stub not built at {}: run `cargo test -p subc-core` (which builds \
             [[bin]] targets) rather than `cargo test -p subc-core --lib` (which does not)",
        path.display()
    );
    path
}

/// Whether clients retry `code` in place: the predicate itself, never a copy
/// of its set. A copied list breaks silently when a code is added to or
/// removed from the real one, and a stale copy here would let exactly the
/// failure this test exists to catch pass.
fn client_retries(code: &str) -> bool {
    subc_protocol::error_codes::is_retryable_route_open(code)
}

/// A code is not a label — clients branch on it, so publishing the wrong KIND
/// of failure is worse than publishing none. A permanent fault dressed as
/// retryable makes every client in the fleet retry forever against something
/// that cannot recover; a transient fault dressed as permanent abandons work
/// that would have succeeded.
///
/// Asserting "a code exists" cannot catch either, because the string is free
/// to say anything. This enumerates every variant and pins which side of the
/// retry boundary it lands on, so a new variant must be classified here
/// deliberately rather than inheriting whichever arm it was appended to.
#[test]
fn retryability_of_forwarding_codes_matches_the_failure() {
    // Transient by nature: the target is booting, reloading, or its endpoint
    // was swapped mid-flight. Retrying is how these resolve.
    let transient = [
        ForwardingError::NoModuleConnection,
        ForwardingError::ModuleReloading {
            module_id: "m".into(),
        },
        ForwardingError::StaleModuleEndpoint,
        ForwardingError::UnknownReservation {
            client_channel: 1,
            module_channel: 1,
        },
        ForwardingError::ConnectionClosing {
            connection_id: ConnectionId::new(1),
        },
        ForwardingError::ClientEgressClosed {
            connection_id: ConnectionId::new(1),
        },
        ForwardingError::ModuleEgressUnavailable {
            connection_id: ConnectionId::new(1),
        },
    ];
    for err in transient {
        let code = forwarding_error_code(&err);
        assert!(
            client_retries(code),
            "{err:?} is transient but publishes {code:?}, which clients treat as permanent"
        );
    }

    // Not fixed by retrying. Channel and correlation exhaustion need the
    // caller to close routes, and a poisoned lock is a daemon that cannot
    // recover at all — the worst thing to advertise as retryable, since every
    // client would storm a daemon that will never answer.
    let permanent = [
        ForwardingError::ConnectionRoleConflict {
            connection_id: ConnectionId::new(1),
        },
        ForwardingError::ClientRouteChannelExhausted {
            connection_id: ConnectionId::new(1),
        },
        ForwardingError::ModuleRouteChannelExhausted {
            endpoint: ModuleEndpointId {
                connection_id: ConnectionId::new(1),
                generation: 1,
            },
        },
        ForwardingError::RelayCorrelationExhausted,
        ForwardingError::RouteOpenBuild("x".into()),
        ForwardingError::Poisoned,
    ];
    for err in permanent {
        let code = forwarding_error_code(&err);
        assert!(
            !client_retries(code),
            "{err:?} cannot be fixed by retrying but publishes {code:?}, which clients retry"
        );
    }
}

/// The principal is the daemon's answer to "who is calling", and modules
/// branch on it: aft gates bash on it, cerebellum gates browser control,
/// plexus gates connector invocation. So a stamp is an authorization input in
/// another process, not a label — and both possible answers SUCCEED, which is
/// what makes a wrong one quiet. An unattested caller stamped `Reserved` hands
/// first-party capability to something that never proved it; a supervised one
/// stamped `Direct` silently strips a module of capability it is entitled to.
///
/// Neither shows up in a test that only checks the bind succeeded. Before this
/// test the only coverage was accidental —
/// `route_open_round_trip_via_tagged_shape_forwards_through_stub` asserts the
/// stamped principal on its way past, so narrowing that wire-shape test to its
/// stated subject would have deleted the last assertion on this value. It
/// still asserts the stamp, which is now redundancy rather than the only
/// guard: both fail under the same mutation, and this one names the reason.
/// SCOPE: this handler's supervisor has spawned nothing, so
/// `spawned_consumer_authorized` can only ever return false and the GRANT arm
/// is unreachable here. Both assertions below are refusals, and a mutant that
/// refuses everything would satisfy them.
///
/// The grant side is covered where a real nonce exists: `tests/forwarding.rs`
/// spawns a supervised consumer, reads its live nonce, and asserts the module
/// observed `principal.kind == "reserved"` carrying that module_id — verified
/// at source rather than assumed, since a citation is a claim about another
/// file and ages like one. Recorded because a harness that structurally
/// cannot reach an arm reports "none" for that arm identically to one that
/// covers it and found nothing.
#[tokio::test]
async fn an_unattested_caller_is_never_stamped_as_a_supervised_module() {
    let handler = ControlHandler::default();
    let frame = Frame::build(FrameType::Request, control_flags(), 0, 0, 900, Vec::new()).unwrap();

    // Absent consumer_identity is the ordinary case: a human at a terminal, or
    // any process holding the connection file. Nothing was proved, so nothing
    // may be granted beyond the unattested floor.
    let stamped = handler
        .route_open_principal(ConnectionId::new(1), &frame, None)
        .unwrap()
        .unwrap();
    assert_eq!(
        stamped,
        Principal::Direct,
        "a caller that proved nothing must not be stamped as a supervised module"
    );

    // A claimed module_id with a nonce no supervised child was given is a
    // forgery attempt, not a weaker caller: it must be REFUSED rather than
    // quietly demoted to Direct, or an impersonation attempt looks identical
    // to an ordinary unattested connection.
    let forged = handler
        .route_open_principal(
            ConnectionId::new(1),
            &frame,
            Some(ConsumerIdentity {
                module_id: "aft".to_string(),
                launch_nonce: "not-a-real-nonce".to_string(),
            }),
        )
        .unwrap();
    let refusal = forged.expect_err("an unmatched launch nonce must not yield a principal");
    assert_eq!(parse_error(&refusal)["code"], "bad_consumer_identity");
}

/// The test above hands `route_open_principal` an identity it built itself,
/// which proves the stamping rule and nothing about where the identity comes
/// from. The real producer is a wire body, and the two are joined by a serde
/// field name that nothing else asserts.
///
/// That join fails quietly in one specific way: an unrecognised key is simply
/// absent after parsing, so a renamed or misspelled `consumer_identity`
/// yields `None` and every supervised module silently drops to `Direct`.
/// Capability-wise that is the safe direction, but it surfaces far from its
/// cause — as a module mysteriously refused bash — and it would pass every
/// test that builds its own input.
///
/// Deliberately NOT closed with `deny_unknown_fields`: refusing unknown keys
/// would break every client the moment the daemon gains a field, trading a
/// quiet demotion for a hard refusal on additive change. Asserting the join
/// instead means a rename breaks a test here rather than the fleet.
#[test]
fn a_wire_body_actually_yields_the_consumer_identity_the_daemon_stamps_from() {
    let body = br#"{"op":"route.open","target":{"kind":"tool_provider","module_id":"m"},"identity":{"session":"s","project_root":"/p","harness":"h"},"consumer_identity":{"module_id":"aft","launch_nonce":"n"}}"#;
    let parsed: ClientControlRequest = serde_json::from_slice(body).unwrap();
    let ClientControlRequest::RouteOpen {
        consumer_identity, ..
    } = parsed
    else {
        panic!("route.open body must parse as RouteOpen");
    };
    assert_eq!(
        consumer_identity,
        Some(ConsumerIdentity {
            module_id: "aft".to_string(),
            launch_nonce: "n".to_string(),
        }),
        "the wire field name must reach the value route_open_principal reads"
    );
}

fn manifest(module_id: &str, protocol_ver: u8) -> ModuleManifest {
    ModuleManifest::builder(module_id, "0.1.0")
        .protocol_ver(protocol_ver)
        .provides(vec![ProviderRole::ToolProvider {
            tools: vec![Tool {
                name: "read".to_string(),
                description: None,
                execution_mode: ExecutionMode::Pure,
                schema: json!({"type": "object"}),
            }],
            identity_scope: vec![IdentityScope::Project, IdentityScope::Session],
            concurrency: Concurrency::ModuleManaged,
            emits_push: true,
            sub_supervises: true,
        }])
        .build()
}

fn hello_frame(module_id: &str, protocol_ver: u8, corr: u64) -> Frame {
    hello_frame_with_control_ops(module_id, protocol_ver, corr, None)
}

fn hello_frame_with_control_ops(
    module_id: &str,
    protocol_ver: u8,
    corr: u64,
    control_ops: Option<Vec<String>>,
) -> Frame {
    hello_frame_full(module_id, protocol_ver, corr, control_ops, None)
}

fn hello_frame_with_nonce(
    module_id: &str,
    protocol_ver: u8,
    corr: u64,
    launch_nonce: Option<&str>,
) -> Frame {
    hello_frame_full(
        module_id,
        protocol_ver,
        corr,
        None,
        launch_nonce.map(ToOwned::to_owned),
    )
}

fn hello_frame_full(
    module_id: &str,
    protocol_ver: u8,
    corr: u64,
    control_ops: Option<Vec<String>>,
    launch_nonce: Option<String>,
) -> Frame {
    let body = serde_json::to_vec(&ModuleHelloBody {
        manifest: manifest(module_id, protocol_ver),
        protocol_ver,
        control_ops,
        launch_nonce,
    })
    .unwrap();
    Frame::build(FrameType::Hello, control_flags(), 0, 0, corr, body).unwrap()
}

fn hello_frame_with_rich_ui(
    module_id: &str,
    protocol_ver: u8,
    corr: u64,
    launch_nonce: Option<&str>,
    rich_ui: Value,
) -> Frame {
    let mut body = serde_json::to_value(ModuleHelloBody {
        manifest: manifest(module_id, protocol_ver),
        protocol_ver,
        control_ops: None,
        launch_nonce: launch_nonce.map(str::to_string),
    })
    .unwrap();
    body["manifest"]["rich_ui"] = rich_ui;
    Frame::build(
        FrameType::Hello,
        control_flags(),
        0,
        0,
        corr,
        serde_json::to_vec(&body).unwrap(),
    )
    .unwrap()
}

fn non_routable_hello_frame_with_control_ops(
    module_id: &str,
    corr: u64,
    control_ops: Option<Vec<String>>,
) -> Frame {
    let mut manifest = manifest(module_id, PROTOCOL_VERSION);
    manifest.provides.clear();
    let body = serde_json::to_vec(&ModuleHelloBody {
        manifest,
        protocol_ver: PROTOCOL_VERSION,
        control_ops,
        launch_nonce: None,
    })
    .unwrap();
    Frame::build(FrameType::Hello, control_flags(), 0, 0, corr, body).unwrap()
}

fn capability_grammar_hello_frame(
    capabilities: Value,
    runtime_computed: Option<Value>,
    corr: u64,
) -> Frame {
    let mut body = serde_json::to_value(ModuleHelloBody {
        manifest: manifest("capability-grammar-test", PROTOCOL_VERSION),
        protocol_ver: PROTOCOL_VERSION,
        control_ops: None,
        launch_nonce: None,
    })
    .expect("HELLO body serializes");
    body["manifest"]["capabilities"] = capabilities;
    if let Some(runtime_computed) = runtime_computed {
        body["runtime_computed"] = runtime_computed;
    }
    Frame::build(
        FrameType::Hello,
        control_flags(),
        0,
        0,
        corr,
        serde_json::to_vec(&body).expect("HELLO body reserializes"),
    )
    .expect("HELLO frame builds")
}

fn channel_request(channel: u16, corr: u64) -> Frame {
    Frame::build(
        FrameType::Request,
        Flags::new(true, Priority::Interactive, false),
        channel,
        0,
        corr,
        b"opaque".to_vec(),
    )
    .unwrap()
}

fn route_ctx(
    connection_id: ConnectionId,
) -> (RouteCtx, mpsc::Receiver<crate::router::OutboundFrame>) {
    let (tx, rx) = mpsc::channel(8);
    (
        RouteCtx {
            connection_id,
            egress: FrameSink::new(tx),
        },
        rx,
    )
}

fn parse_ack(frame: &Frame) -> ModuleHelloAckBody {
    serde_json::from_slice(&frame.body).unwrap()
}

/// Register a module over a connection that has a sink and return the
/// HELLO_ACK the module reads. A successful HELLO queues its ack on the
/// module's own sink rather than returning it as a reply, so the ack is
/// taken off `rx` here and whatever the test reads next is what followed it.
async fn hello_via_sink(
    handler: &ControlHandler,
    ctx: &RouteCtx,
    rx: &mut mpsc::Receiver<crate::router::OutboundFrame>,
    hello: Frame,
) -> Frame {
    let replies = handler.handle_control_frame(ctx, hello).await.unwrap();
    assert!(
        replies.is_empty(),
        "a registered HELLO replies with nothing; its ack is already queued: {replies:?}"
    );
    let ack = rx
        .try_recv()
        .expect("HELLO_ACK is queued on the module sink")
        .frame;
    assert_eq!(ack.header.ty, FrameType::HelloAck);
    ack
}

fn parse_error(frame: &Frame) -> Value {
    serde_json::from_slice(&frame.body).unwrap()
}

fn parse_route_poll(frame: &Frame) -> ClientControlResponse {
    serde_json::from_slice(&frame.body).unwrap()
}

fn route_poll_frame(corr: u64, kind: PollKind, route_channel: u16) -> Frame {
    let body = serde_json::to_vec(&ClientControlRequest::RoutePoll {
        route_channel,
        route_epoch: 0,
        kind,
    })
    .unwrap();
    Frame::build(FrameType::Request, control_flags(), 0, 0, corr, body).unwrap()
}

fn supervisor_health_probe_frame(corr: u64, module_id: &str) -> Frame {
    let body = serde_json::to_vec(&ClientControlRequest::SupervisorHealthProbe {
        module_id: module_id.to_string(),
    })
    .unwrap();
    Frame::build(FrameType::Request, control_flags(), 0, 0, corr, body).unwrap()
}

fn route_open_frame(corr: u64, module_id: &str, project_root: ScratchDir) -> Frame {
    route_open_frame_with_consumer_capabilities(corr, module_id, project_root, None)
}

// Exercise retired opener fields without putting them back in BindIdentity.
fn legacy_route_open_frame(corr: u64, module_id: &str, root: ScratchDir) -> Frame {
    let path = ProjectRootId::from_path_allowing_missing(root.path()).unwrap();
    let mut frame = route_open_frame(corr, module_id, root);
    let mut body: Value = serde_json::from_slice(&frame.body).unwrap();
    body["identity"]["project_root"] = serde_json::json!(path.as_path());
    frame.body = serde_json::to_vec(&body).unwrap();
    frame
}

fn route_open_frame_with_consumer_capabilities(
    corr: u64,
    module_id: &str,
    _project_root: ScratchDir,
    consumer_capabilities: Option<Vec<String>>,
) -> Frame {
    let body = serde_json::to_vec(&ClientControlRequest::RouteOpen {
        busy_refusals: None,
        target: RouteTarget::ToolProvider {
            module_id: module_id.to_string(),
        },
        identity: BindIdentity::new("unit".to_string(), "session".to_string()),
        consumer_identity: None,
        consumer_capabilities,
        role_versions: None,
        admission_facts: None,
        scope: None,
    })
    .unwrap();
    Frame::build(FrameType::Request, control_flags(), 0, 0, corr, body).unwrap()
}

fn route_open_frame_with_role_versions(
    corr: u64,
    module_id: &str,
    _project_root: ScratchDir,
    role_versions: Option<BTreeMap<String, String>>,
) -> Frame {
    let body = serde_json::to_vec(&ClientControlRequest::RouteOpen {
        busy_refusals: None,
        target: RouteTarget::ToolProvider {
            module_id: module_id.to_string(),
        },
        identity: BindIdentity::new("unit".to_string(), format!("session-{corr}")),
        consumer_identity: None,
        consumer_capabilities: None,
        role_versions,
        admission_facts: None,
        scope: None,
    })
    .unwrap();
    Frame::build(FrameType::Request, control_flags(), 0, 0, corr, body).unwrap()
}

fn role_versions(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
    entries
        .iter()
        .map(|(role, version)| (role.to_string(), version.to_string()))
        .collect()
}

fn route_open_frame_with_admission_facts(
    corr: u64,
    module_id: &str,
    _project_root: ScratchDir,
    consumer_identity: Option<subc_control::ConsumerIdentity>,
    facts: Option<Value>,
) -> Frame {
    let body = serde_json::to_vec(&ClientControlRequest::RouteOpen {
        busy_refusals: None,
        target: RouteTarget::ToolProvider {
            module_id: module_id.to_string(),
        },
        identity: BindIdentity::new("unit".to_string(), format!("session-{corr}")),
        consumer_identity,
        consumer_capabilities: None,
        role_versions: None,
        admission_facts: facts,
        scope: None,
    })
    .unwrap();
    Frame::build(FrameType::Request, control_flags(), 0, 0, corr, body).unwrap()
}

#[derive(Clone, Default)]
struct EventCapture {
    events: Arc<Mutex<Vec<CapturedEvent>>>,
}

#[derive(Clone, Debug)]
struct CapturedEvent {
    target: String,
    level: tracing::Level,
    fields: BTreeMap<String, String>,
}

impl EventCapture {
    fn events(&self) -> Vec<CapturedEvent> {
        self.events.lock().unwrap().clone()
    }
}

impl<S> Layer<S> for EventCapture
where
    S: Subscriber,
{
    fn on_event(&self, event: &Event<'_>, _context: Context<'_, S>) {
        let mut visitor = EventFieldVisitor::default();
        event.record(&mut visitor);
        self.events.lock().unwrap().push(CapturedEvent {
            target: event.metadata().target().to_string(),
            level: *event.metadata().level(),
            fields: visitor.fields,
        });
    }
}

#[derive(Default)]
struct EventFieldVisitor {
    fields: BTreeMap<String, String>,
}

impl Visit for EventFieldVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.fields
            .insert(field.name().to_string(), format!("{value:?}"));
    }
}

fn health_response(corr: u64, status: HealthStatus) -> Frame {
    let body = serde_json::to_vec(&ModuleControlResponse::HealthCheck {
        status,
        detail: Some("warming".to_string()),
        metrics: Some(json!({"queue_depth": 3})),
    })
    .unwrap();
    Frame::build(FrameType::Response, control_flags(), 0, 0, corr, body).unwrap()
}

fn route_bind_ack(corr: u64) -> Frame {
    let body = serde_json::to_vec(&ModuleControlResponse::RouteBindAck {}).unwrap();
    Frame::build(FrameType::Response, control_flags(), 0, 0, corr, body).unwrap()
}

fn unique_project_root(label: &str) -> ScratchDir {
    ScratchDir::new(label)
}

fn assert_route_poll_liveness(frame: &Frame, expected_live: bool) {
    match parse_route_poll(frame) {
        ClientControlResponse::RoutePoll {
            status: None,
            live: Some(live),
            ..
        } => assert_eq!(live, expected_live),
        other => panic!("unexpected route.poll response: {other:?}"),
    }
}

fn bind_liveness_route(
    registry: &Registry,
    forwarding: &ForwardingTable,
    module_id: &str,
) -> (RouteCtx, u16, u32) {
    let module_connection = ConnectionId::new(101);
    let client_connection = ConnectionId::new(202);
    let registration = registry
        .register_with_control_ops(
            manifest(module_id, PROTOCOL_VERSION),
            PROTOCOL_VERSION,
            module_connection,
            module_baseline_control_ops(),
        )
        .unwrap();
    let (module_tx, _module_rx) = mpsc::channel(8);
    let endpoint = forwarding
        .register_module_connection(
            module_connection,
            module_id.to_string(),
            PROTOCOL_VERSION,
            manifest_concurrency(&registration.manifest),
            FrameSink::new(module_tx),
        )
        .unwrap();
    let (client_ctx, _client_rx) = route_ctx(client_connection);
    let pending = forwarding
        .begin_route_bind_relay_for_test(client_connection, client_ctx.egress.clone(), 1, module_id)
        .unwrap();
    assert_eq!(pending.endpoint, endpoint);
    let route_channel = pending.client_channel;
    let route_epoch = pending.client_epoch;
    forwarding
        .complete_pending_relay(
            module_connection,
            pending.corr,
            RouteBindRelayOutcome::Accepted,
        )
        .unwrap();
    (client_ctx, route_channel, route_epoch)
}

struct FakeProcessLiveness {
    live: Option<bool>,
}

impl ModuleProcessLiveness for FakeProcessLiveness {
    fn process_live(&self, _module_id: &str) -> Option<bool> {
        self.live
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supervisor_stderr_tail_converts_a_real_truncated_ring_entry_to_prefix_only_wire_data() {
    let registry = Arc::new(Registry::default());
    let supervisor_handle = SupervisorHandle::new();
    let supervisor = Supervisor::new_for_test(
        Arc::clone(&registry),
        RestartPolicy::new(1, Duration::from_millis(10)),
    )
    .with_handle(supervisor_handle.clone());
    let source_line = format!("config error: {}", "x".repeat(DEFAULT_MAX_LINE_BYTES));
    let module = supervisor
        .spawn(ModuleSpec {
            script: None,
            carry_containment: false,
            module_id: "stderr-tail-wire".to_string(),
            program: fake_aft_stub_path(),
            args: Vec::new(),
            env: vec![
                ("FAKE_AFT_STDERR_LINE".to_string(), source_line.clone()),
                ("FAKE_AFT_EXIT_CODE".to_string(), "1".to_string()),
            ],
            reserved: false,
            reserved_prefixes: Vec::new(),
            protocol: ModuleProtocol::Subc,
            overlap: Default::default(),
        })
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let tail = module.stderr_tail(None, None);
        if tail
            .entries
            .iter()
            .any(|entry| matches!(entry, TailEntry::ProcessStart))
            && tail.entries.iter().any(|entry| {
                matches!(
                    entry,
                    TailEntry::Line {
                        truncated: true,
                        ..
                    }
                )
            })
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "module did not produce a truncated line and restart boundary: {tail:?}"
        );
        sleep(Duration::from_millis(10)).await;
    }

    let handler = ControlHandler::new(Arc::clone(&registry)).with_supervisor(supervisor_handle);
    let request = ClientControlRequest::SupervisorStderrTail {
        module_id: "stderr-tail-wire".to_string(),
        max_lines: None,
        max_bytes: None,
    };
    let frame = Frame::build(
        FrameType::Request,
        control_flags(),
        0,
        0,
        1,
        serde_json::to_vec(&request).unwrap(),
    )
    .unwrap();
    let (ctx, _egress) = route_ctx(ConnectionId::new(1));
    let responses = handler.handle_control_frame(&ctx, frame).await.unwrap();
    let ClientControlResponse::SupervisorStderrTail { tail, .. } =
        serde_json::from_slice(&responses[0].body).unwrap()
    else {
        panic!("expected supervisor.stderr_tail response");
    };

    assert!(
        tail.entries
            .iter()
            .any(|entry| matches!(entry, StderrTailEntry::ProcessStart)),
        "the control response lost the restart boundary"
    );
    let Some(StderrTailEntry::Line {
        text,
        truncated,
        at_ms,
    }) = tail.entries.iter().find(|entry| {
        matches!(
            entry,
            StderrTailEntry::Line {
                truncated: true,
                ..
            }
        )
    })
    else {
        panic!("the control response lost the truncated line");
    };
    assert_eq!(text, &source_line[..DEFAULT_MAX_LINE_BYTES]);
    assert!(*truncated);
    assert!(
        at_ms.is_some(),
        "the control response lost the line's capture time"
    );
}

/// `supervisor.terminals` reads journal files. On a single-worker runtime a
/// read done on the worker thread would stall every other task until it
/// finished; the read must run off the worker so this test's own task keeps
/// running while the read is paused.
#[tokio::test(flavor = "current_thread")]
async fn supervisor_terminals_reads_the_journal_off_the_runtime_worker() {
    let dir = ScratchDir::new("terminals-off-worker");
    let journal_path = dir.join("terminals.jsonl");
    let registry = Arc::new(Registry::default());
    let supervisor_handle = SupervisorHandle::new();
    let supervisor =
        Supervisor::new_for_test(Arc::clone(&registry), RestartPolicy::new(1, Duration::ZERO))
            .with_handle(supervisor_handle.clone())
            .with_terminal_journal(journal_path.clone(), "off-worker-daemon".to_string());
    let module = supervisor
        .spawn(ModuleSpec {
            script: None,
            carry_containment: false,
            module_id: "terminal-off-worker".to_string(),
            program: fake_aft_stub_path(),
            args: Vec::new(),
            env: vec![("FAKE_AFT_EXIT_CODE".to_string(), "23".to_string())],
            reserved: false,
            reserved_prefixes: Vec::new(),
            protocol: ModuleProtocol::Subc,
            overlap: Default::default(),
        })
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while module.terminal_history().entries.len() != 2 {
        assert!(Instant::now() < deadline, "module did not record two exits");
        sleep(Duration::from_millis(10)).await;
    }

    let (started, release) = crate::terminal_journal::read_pause::install(&journal_path);
    let handler =
        Arc::new(ControlHandler::new(Arc::clone(&registry)).with_supervisor(supervisor_handle));
    let frame = Frame::build(
        FrameType::Request,
        control_flags(),
        0,
        0,
        1,
        serde_json::to_vec(&ClientControlRequest::SupervisorTerminals {
            module_id: "terminal-off-worker".to_string(),
        })
        .unwrap(),
    )
    .unwrap();
    let (ctx, _egress) = route_ctx(ConnectionId::new(1));
    let spawned_at = std::time::Instant::now();
    let read = tokio::spawn({
        let handler = Arc::clone(&handler);
        async move { handler.handle_control_frame(&ctx, frame).await }
    });
    // Waiting for the pause from a blocking thread keeps this task pending,
    // so the runtime's single worker is free to run the read task.
    tokio::task::spawn_blocking(move || started.recv_timeout(Duration::from_secs(5)))
        .await
        .unwrap()
        .expect("the history read reached its pause");
    let elapsed = spawned_at.elapsed();
    assert!(
        elapsed < Duration::from_secs(2) && !read.is_finished(),
        "this task could not run while the history read was paused \
             (resumed after {elapsed:?}, read finished: {})",
        read.is_finished()
    );

    drop(release);
    let responses = read.await.unwrap().unwrap();
    let response: ClientControlResponse = serde_json::from_slice(&responses[0].body).unwrap();
    let ClientControlResponse::SupervisorTerminals { terminals, .. } = response else {
        panic!("expected supervisor.terminals response");
    };
    assert_eq!(terminals.entries.len(), 2);
    assert_eq!(terminals.journal_skipped_lines, 0);
    assert_eq!(terminals.journal_read_errors, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supervisor_terminals_golden_is_generated_through_the_real_handler() {
    let registry = Arc::new(Registry::default());
    let supervisor_handle = SupervisorHandle::new();
    let supervisor =
        Supervisor::new_for_test(Arc::clone(&registry), RestartPolicy::new(1, Duration::ZERO))
            .with_handle(supervisor_handle.clone());
    let module = supervisor
        .spawn(ModuleSpec {
            script: None,
            carry_containment: false,
            module_id: "terminal-golden".to_string(),
            program: fake_aft_stub_path(),
            args: Vec::new(),
            env: vec![("FAKE_AFT_EXIT_CODE".to_string(), "23".to_string())],
            reserved: false,
            reserved_prefixes: Vec::new(),
            protocol: ModuleProtocol::Subc,
            overlap: Default::default(),
        })
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    while module.terminal_history().entries.len() != 2 {
        assert!(
            Instant::now() < deadline,
            "module did not retain two terminal exits: {:?}",
            module.terminal_history()
        );
        sleep(Duration::from_millis(10)).await;
    }

    let handler = ControlHandler::new(Arc::clone(&registry)).with_supervisor(supervisor_handle);
    let request = ClientControlRequest::SupervisorTerminals {
        module_id: "terminal-golden".to_string(),
    };
    let frame = Frame::build(
        FrameType::Request,
        control_flags(),
        0,
        0,
        1,
        serde_json::to_vec(&request).unwrap(),
    )
    .unwrap();
    let (ctx, _egress) = route_ctx(ConnectionId::new(1));
    let responses = handler.handle_control_frame(&ctx, frame).await.unwrap();
    let response: ClientControlResponse = serde_json::from_slice(&responses[0].body).unwrap();
    let ClientControlResponse::SupervisorTerminals { terminals, .. } = &response else {
        panic!("expected supervisor.terminals response");
    };
    assert_eq!(terminals.entries.len(), 2);
    assert_eq!(terminals.dropped, 0);

    let mut rendered = serde_json::to_value(response).unwrap();
    // Wall-clock fields are the observation contract, but not stable fixture
    // bytes; normalize only them after the real handler has shaped the response.
    rendered["daemon_started_at_ms"] = json!(1_700_000_000_000u64);
    for (index, entry) in rendered["entries"]
        .as_array_mut()
        .expect("terminal response entries array")
        .iter_mut()
        .enumerate()
    {
        entry["at_ms"] = json!(1_700_000_000_001u64 + index as u64);
    }

    let golden_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../subc-control/tests/golden/client_control_response_supervisor_terminals.json");
    let serialized = serde_json::to_string_pretty(&rendered).unwrap() + "\n";
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::write(&golden_path, &serialized).unwrap();
    }
    let expected: Value =
        serde_json::from_str(&std::fs::read_to_string(&golden_path).unwrap()).unwrap();
    assert_eq!(rendered, expected);
}

#[test]
fn hello_registers_manifest_and_returns_ack() {
    let registry = Arc::new(Registry::default());
    let handler = ControlHandler::new(Arc::clone(&registry));
    let conn = ConnectionId::new(1);

    let responses = handler
        .handle_control(conn, hello_frame("aft", PROTOCOL_VERSION, 7))
        .unwrap();

    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0].header.ty, FrameType::HelloAck);
    assert_eq!(responses[0].header.channel, 0);
    assert_eq!(responses[0].header.corr, 7);
    let ack = parse_ack(&responses[0]);
    assert_eq!(ack.negotiated_ver, PROTOCOL_VERSION);
    assert!(ack
        .subc_capabilities
        .contains(&CAP_MANIFEST_REGISTRATION.to_string()));
    assert!(ack.subc_ops.contains(&ops::SUPERVISOR_LIST.to_string()));
    assert!(ack.subc_ops.contains(&ops::SUPERVISOR_RESTART.to_string()));
    assert!(ack
        .subc_ops
        .contains(&ops::SUPERVISOR_SET_ENABLED.to_string()));
    assert!(ack
        .subc_ops
        .contains(&MODULE_TO_SUBC_OP_CATALOG_UPDATE.to_string()));
    assert!(ack
        .subc_ops
        .contains(&MODULE_TO_SUBC_OP_CATALOG_UPDATE_ROLE_DECLARATIONS.to_string()));

    let registration = registry.get_module("aft").unwrap().unwrap();
    assert_eq!(registration.negotiated_ver, PROTOCOL_VERSION);
    assert_eq!(registration.state, ChannelState::Active);
    assert_eq!(registration.connection_id, conn);
    assert_eq!(registration.control_ops, module_baseline_control_ops());
    assert!(registration.manifest.display_name.is_none());
}

#[test]
fn hello_display_name_refuses_invalid_manifest_by_field() {
    let registry = Arc::new(Registry::default());
    let handler = ControlHandler::new(Arc::clone(&registry));
    for (index, invalid) in [
        json!(""),
        json!("a".repeat(41)),
        json!(" Routing"),
        json!("a\nb"),
        json!("a\u{202e}b"),
        json!(7),
    ]
    .into_iter()
    .enumerate()
    {
        let module_id = format!("bad-display-name-{index}");
        let mut body = serde_json::to_value(ModuleHelloBody {
            manifest: manifest(&module_id, PROTOCOL_VERSION),
            protocol_ver: PROTOCOL_VERSION,
            control_ops: None,
            launch_nonce: None,
        })
        .unwrap();
        body["manifest"]["display_name"] = invalid;
        let hello = Frame::build(
            FrameType::Hello,
            control_flags(),
            0,
            0,
            7,
            serde_json::to_vec(&body).unwrap(),
        )
        .unwrap();
        let response = handler
            .handle_control(ConnectionId::new(100 + index as u64), hello)
            .unwrap();
        assert_eq!(response[0].header.ty, FrameType::Error);
        let error = parse_error(&response[0]);
        assert_eq!(error["code"], "invalid_manifest");
        assert!(error["message"].as_str().unwrap().contains("display_name"));
        assert!(registry.get_module(&module_id).unwrap().is_none());
    }
}

#[test]
fn capability_grammar_refusals_name_the_field_and_leave_no_catalog_entry() {
    let invalid_identifiers = [
        ("case_change", "credentials-Provider/v1"),
        ("leading_zero", "credentials-provider/v01"),
        ("trailing_hyphen", "credentials-provider-/v1"),
        ("consecutive_hyphens", "credentials--provider/v1"),
        ("uppercase", "Credentials-provider/v1"),
        ("missing_v", "credentials-provider/1"),
        ("whitespace", "credentials provider/v1"),
        ("zero_version", "credentials-provider/v0"),
        ("out_of_range_version", "credentials-provider/v4294967296"),
        (
            "overlength_name",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/v1",
        ),
    ];
    let mut cases = invalid_identifiers
        .into_iter()
        .map(|(name, identifier)| {
            (
                format!("identifier_{name}"),
                "capabilities.provides[0]".to_string(),
                identifier.to_string(),
                json!({ "provides": [identifier] }),
                None,
            )
        })
        .collect::<Vec<_>>();
    cases.extend([
            (
                "unknown_need".to_string(),
                "capabilities.requires[0].need".to_string(),
                "deferred".to_string(),
                json!({ "requires": [{ "capability": "credentials-provider/v1", "need": "deferred" }] }),
                None,
            ),
            (
                "duplicate_provides".to_string(),
                "capabilities.provides[1]".to_string(),
                "credentials-provider/v1".to_string(),
                json!({ "provides": ["credentials-provider/v1", "credentials-provider/v1"] }),
                None,
            ),
            (
                "duplicate_must_never_reach".to_string(),
                "capabilities.must_never_reach[1]".to_string(),
                "credentials-provider/v1".to_string(),
                json!({ "must_never_reach": ["credentials-provider/v1", "credentials-provider/v1"] }),
                None,
            ),
            (
                "duplicate_requires_same_need".to_string(),
                "capabilities.requires[1]".to_string(),
                "credentials-provider/v1".to_string(),
                json!({ "requires": [
                    { "capability": "credentials-provider/v1", "need": "required" },
                    { "capability": "credentials-provider/v1", "need": "required" }
                ] }),
                None,
            ),
            (
                "duplicate_requires_conflicting_need".to_string(),
                "capabilities.requires[1]".to_string(),
                "credentials-provider/v1".to_string(),
                json!({ "requires": [
                    { "capability": "credentials-provider/v1", "need": "required" },
                    { "capability": "credentials-provider/v1", "need": "optional" }
                ] }),
                None,
            ),
            (
                "capabilities_root_pointer".to_string(),
                "runtime_computed[0]".to_string(),
                "/capabilities".to_string(),
                json!({}),
                Some(json!(["/capabilities"])),
            ),
            (
                "capabilities_descendant_pointer".to_string(),
                "runtime_computed[0]".to_string(),
                "/capabilities/provides".to_string(),
                json!({}),
                Some(json!(["/capabilities/provides"])),
            ),
            (
                "malformed_pointer_without_leading_slash".to_string(),
                "runtime_computed[0]".to_string(),
                "capabilities".to_string(),
                json!({}),
                Some(json!(["capabilities"])),
            ),
            (
                "malformed_pointer_escape".to_string(),
                "runtime_computed[0]".to_string(),
                "/roles/~2/tools".to_string(),
                json!({}),
                Some(json!(["/roles/~2/tools"])),
            ),
            (
                "unknown_capabilities_field".to_string(),
                "capabilities.future".to_string(),
                "<array>".to_string(),
                json!({ "future": [] }),
                None,
            ),
        ]);

    for (index, (name, field, value, capabilities, runtime_computed)) in
        cases.into_iter().enumerate()
    {
        let registry = Arc::new(Registry::default());
        let handler = ControlHandler::new(Arc::clone(&registry));
        let response = handler
            .handle_control(
                ConnectionId::new((index + 1) as u64),
                capability_grammar_hello_frame(capabilities, runtime_computed, index as u64 + 1),
            )
            .expect("invalid HELLO returns a refusal");

        assert_eq!(response.len(), 1, "{name} must emit one refusal");
        let error = parse_error(&response[0]);
        assert_eq!(error["code"], "invalid_capability_grammar", "{name}");
        let message = error["message"]
            .as_str()
            .expect("error message is a string");
        assert!(
            message.contains(&field),
            "{name}: field missing from {message}"
        );
        assert!(
            message.contains(&value),
            "{name}: value missing from {message}"
        );
        assert_eq!(
            registry
                .active_registration_count()
                .expect("registry reads"),
            0,
            "{name}: refused HELLO must not create a catalog entry"
        );
    }
}

#[test]
fn legal_runtime_pointer_and_capabilities_are_mirrored_in_catalog_list() {
    let registry = Arc::new(Registry::default());
    let handler = ControlHandler::new(Arc::clone(&registry));
    let capabilities = json!({
        "provides": ["credentials-provider/v1"],
        "requires": [{ "capability": "context-transform/v1", "need": "optional" }],
        "must_never_reach": ["federation-transport/v1"]
    });
    let response = handler
        .handle_control(
            ConnectionId::new(99),
            capability_grammar_hello_frame(
                capabilities.clone(),
                Some(json!(["/roles/0/tools"])),
                99,
            ),
        )
        .expect("valid HELLO registers");
    assert_eq!(response[0].header.ty, FrameType::HelloAck);

    let request = Frame::build(
        FrameType::Request,
        control_flags(),
        0,
        0,
        100,
        serde_json::to_vec(&ClientControlRequest::CatalogList { module_id: None })
            .expect("catalog request serializes"),
    )
    .expect("catalog request frame builds");
    let response = handler
        .handle_catalog_list(request, None)
        .expect("catalog list succeeds");
    let ClientControlResponse::CatalogList { modules, .. } =
        serde_json::from_slice(&response[0].body).expect("catalog response decodes")
    else {
        panic!("catalog request must return catalog.list");
    };
    assert_eq!(modules.len(), 1);
    assert_eq!(
        serde_json::to_value(&modules[0].capabilities).expect("catalog capabilities serialize"),
        capabilities
    );
}

#[test]
fn catalog_list_mirrors_management_operation_description() {
    let registry = Arc::new(Registry::default());
    let handler = ControlHandler::new(Arc::clone(&registry));
    let description = "List managed records and return their identifiers and metadata.";
    let mut manifest = manifest("described-management", PROTOCOL_VERSION);
    manifest.provides = vec![ProviderRole::ManagementSurface {
        operations: vec![ManagementOperation {
            name: "records.list".to_string(),
            kind: ManagementOperationKind::Query,
            description: Some(description.to_string()),
        }],
        config_schema: json!({"type": "object"}),
        observability: vec![ObservabilitySurface {
            name: "records.stats".to_string(),
            kind: ObservabilityKind::Snapshot,
        }],
        identity_scope: vec![IdentityScope::Project],
        concurrency: Concurrency::ModuleManaged,
    }];
    registry
        .register_with_control_ops(
            manifest,
            PROTOCOL_VERSION,
            ConnectionId::new(99),
            Vec::new(),
        )
        .expect("described management manifest registers");

    let request = Frame::build(
        FrameType::Request,
        control_flags(),
        0,
        0,
        100,
        serde_json::to_vec(&ClientControlRequest::CatalogList { module_id: None })
            .expect("catalog request serializes"),
    )
    .expect("catalog request frame builds");
    let response = handler
        .handle_catalog_list(request, None)
        .expect("catalog list succeeds");
    let body: Value = serde_json::from_slice(&response[0].body).expect("catalog response JSON");
    assert_eq!(
        body["modules"][0]["roles"][0]["operations"][0]["description"], description,
        "catalog.list must preserve the declared operation description verbatim"
    );
}

#[test]
fn catalog_list_mirrors_registered_operation_delivery() {
    let registry = Arc::new(Registry::default());
    let handler = ControlHandler::new(Arc::clone(&registry));
    let delivery =
        json!({"status": "watch", "future": "stream-v2", "reset": "watch", "missing": "watch"});
    let mut body = serde_json::to_value(ModuleHelloBody {
        manifest: manifest("watch-management", PROTOCOL_VERSION),
        protocol_ver: PROTOCOL_VERSION,
        control_ops: None,
        launch_nonce: None,
    })
    .unwrap();
    body["manifest"]["provides"] = json!([{
        "role": "management_surface",
        "operations": [
            {"name": "status", "kind": "query"},
            {"name": "future", "kind": "query"},
            {"name": "reset", "kind": "mutate"}
        ],
        "config_schema": {}, "observability": [], "identity_scope": []
    }]);
    body["manifest"]["operation_delivery"] = delivery.clone();
    let hello = Frame::build(
        FrameType::Hello,
        control_flags(),
        0,
        0,
        99,
        serde_json::to_vec(&body).unwrap(),
    )
    .unwrap();
    let response = handler
        .handle_control(ConnectionId::new(99), hello)
        .unwrap();
    assert_eq!(response[0].header.ty, FrameType::HelloAck);

    let (_, registrations) = registry.list_modules().unwrap();
    let registered = &registrations[0].manifest;
    assert!(registered.is_watch_operation("status"));
    assert!(!registered.is_watch_operation("future"));
    assert!(!registered.is_watch_operation("reset"));
    assert!(!registered.is_watch_operation("missing"));

    let request = Frame::build(
        FrameType::Request,
        control_flags(),
        0,
        0,
        100,
        serde_json::to_vec(&ClientControlRequest::CatalogList { module_id: None }).unwrap(),
    )
    .unwrap();
    let response = handler.handle_catalog_list(request, None).unwrap();
    let body: Value = serde_json::from_slice(&response[0].body).unwrap();
    assert_eq!(body["modules"][0]["operation_delivery"], delivery);
    let ClientControlResponse::CatalogList { modules, .. } =
        serde_json::from_slice(&response[0].body).unwrap()
    else {
        panic!("expected catalog.list");
    };
    assert_eq!(modules[0].operation_delivery, registered.operation_delivery);
}

#[test]
fn catalog_list_mirrors_rich_ui_through_hello_and_catalog_update() {
    let registry = Arc::new(Registry::default());
    let supervisor = SupervisorHandle::new();
    supervisor.set_reserved_nonce("vault", "the-real-nonce".to_string());
    let handler = ControlHandler::new(Arc::clone(&registry)).with_supervisor(supervisor);
    let descriptor = json!({
        "schema": "ckios.richui.manifest/1",
        "manifest_digest": format!("sha256:{}", "a".repeat(64)),
        "bytes": 4096,
        "chunks": 4
    });
    let connection = ConnectionId::new(201);
    let hello = handler
        .handle_control(
            connection,
            hello_frame_with_rich_ui(
                "vault",
                PROTOCOL_VERSION,
                201,
                Some("the-real-nonce"),
                descriptor.clone(),
            ),
        )
        .unwrap();
    assert_eq!(hello[0].header.ty, FrameType::HelloAck);

    let request = Frame::build(FrameType::Request, control_flags(), 0, 0, 202, Vec::new()).unwrap();
    let provides = registry
        .get_module_by_connection(connection)
        .unwrap()
        .unwrap()
        .manifest
        .provides;
    handler
        .handle_catalog_update(connection, request, provides, None, Some(false))
        .unwrap();

    let request = Frame::build(
        FrameType::Request,
        control_flags(),
        0,
        0,
        203,
        serde_json::to_vec(&ClientControlRequest::CatalogList { module_id: None }).unwrap(),
    )
    .unwrap();
    let response = handler.handle_catalog_list(request, None).unwrap();
    let ClientControlResponse::CatalogList { modules, .. } =
        serde_json::from_slice(&response[0].body).unwrap()
    else {
        panic!("expected catalog.list");
    };
    assert_eq!(
        serde_json::to_value(&modules[0]).unwrap()["rich_ui"],
        descriptor
    );
    assert!(modules[0].nonce_verified);
}

fn hello_with_role_declarations(declarations: Value) -> Frame {
    let mut body = serde_json::to_value(ModuleHelloBody {
        manifest: manifest("role-test", PROTOCOL_VERSION),
        protocol_ver: PROTOCOL_VERSION,
        control_ops: None,
        launch_nonce: None,
    })
    .unwrap();
    body["manifest"]["role_declarations"] = declarations;
    Frame::build(
        FrameType::Hello,
        control_flags(),
        0,
        0,
        301,
        serde_json::to_vec(&body).unwrap(),
    )
    .unwrap()
}

fn update_role_declarations_frame(declarations: Value) -> Frame {
    Frame::build(
        FrameType::Request,
        control_flags(),
        0,
        0,
        303,
        serde_json::to_vec(&json!({
            "op": "catalog.update_role_declarations",
            "role_declarations": declarations,
        }))
        .unwrap(),
    )
    .unwrap()
}

#[tokio::test]
async fn role_declarations_update_replaces_then_clears_catalog() {
    let registry = Arc::new(Registry::default());
    let handler = ControlHandler::new(Arc::clone(&registry));
    let (ctx, mut rx) = route_ctx(ConnectionId::new(301));
    let initial = json!({"media-provider/v1": {"engines": ["old"]}, "old/v1": {}});
    hello_via_sink(
        &handler,
        &ctx,
        &mut rx,
        hello_with_role_declarations(initial.clone()),
    )
    .await;
    assert_eq!(
        role_declarations_catalog(&handler).0["role_declarations"],
        initial
    );
    let before = registry.get_module("role-test").unwrap().unwrap();
    for declarations in [
        json!({"media-provider/v1": {"engines": ["new"]}}),
        json!({}),
    ] {
        let response = handler
            .handle_control_frame(&ctx, update_role_declarations_frame(declarations.clone()))
            .await
            .unwrap();
        assert_eq!(response[0].header.ty, FrameType::Response);
        assert_eq!(
            serde_json::from_slice::<Value>(&response[0].body).unwrap(),
            json!({"op": "catalog.update_role_declarations"})
        );
        let (raw, entry) = role_declarations_catalog(&handler);
        assert_eq!(
            serde_json::to_value(entry.role_declarations).unwrap(),
            declarations
        );
        if declarations.as_object().unwrap().is_empty() {
            assert!(raw.get("role_declarations").is_none());
        } else {
            assert_eq!(raw["role_declarations"], declarations);
        }
        let after = registry.get_module("role-test").unwrap().unwrap();
        let mut expected = before.manifest.clone();
        expected.role_declarations = serde_json::from_value(declarations).unwrap();
        assert_eq!(after.manifest, expected);
        assert_eq!(after.ready, before.ready);
    }
}

#[tokio::test]
async fn role_declarations_update_refuses_invalid_maps_without_changing_catalog() {
    let registry = Arc::new(Registry::default());
    let handler = ControlHandler::new(registry);
    let (ctx, mut rx) = route_ctx(ConnectionId::new(301));
    let initial = json!({"media-provider/v1": {"engines": ["old"]}});
    hello_via_sink(
        &handler,
        &ctx,
        &mut rx,
        hello_with_role_declarations(initial.clone()),
    )
    .await;
    for (invalid, reason) in [
        (json!(null), "role_declarations must be an object"),
        (json!([]), "role_declarations must be an object"),
        (
            json!({"bad key": {}}),
            "role_declarations key must use capability-name grammar",
        ),
        (
            json!({"media-provider/v1": []}),
            "role_declarations values must be objects",
        ),
        (
            json!({"media-provider/v1": {"padding": "x".repeat(16384)}}),
            "role_declarations exceeds 16384 compact JSON bytes",
        ),
    ] {
        let response = handler
            .handle_control_frame(&ctx, update_role_declarations_frame(invalid))
            .await
            .unwrap();
        let error = parse_error(&response[0]);
        assert_eq!(error["code"], "invalid_role_declarations");
        assert_eq!(error["message"], reason);
        assert_eq!(
            role_declarations_catalog(&handler).0["role_declarations"],
            initial
        );
    }
}

#[tokio::test]
async fn role_declarations_update_is_owned_by_the_registered_connection() {
    let registry = Arc::new(Registry::default());
    let handler = ControlHandler::new(Arc::clone(&registry));
    let (owner, mut owner_rx) = route_ctx(ConnectionId::new(301));
    let initial = json!({"media-provider/v1": {"engines": ["old"]}});
    hello_via_sink(
        &handler,
        &owner,
        &mut owner_rx,
        hello_with_role_declarations(initial.clone()),
    )
    .await;
    let (other, mut other_rx) = route_ctx(ConnectionId::new(302));
    let replacement = json!({"media-provider/v1": {"engines": ["other"]}});
    let response = handler
        .handle_control_frame(&other, update_role_declarations_frame(replacement.clone()))
        .await
        .unwrap();
    assert_eq!(parse_error(&response[0])["code"], "not_registered");
    hello_via_sink(
        &handler,
        &other,
        &mut other_rx,
        hello_frame("other", PROTOCOL_VERSION, 304),
    )
    .await;
    let response = handler
        .handle_control_frame(&other, update_role_declarations_frame(replacement.clone()))
        .await
        .unwrap();
    assert_eq!(response[0].header.ty, FrameType::Response);
    assert_eq!(
        serde_json::to_value(
            registry
                .get_module("role-test")
                .unwrap()
                .unwrap()
                .manifest
                .role_declarations
        )
        .unwrap(),
        initial
    );
    assert_eq!(
        serde_json::to_value(
            registry
                .get_module("other")
                .unwrap()
                .unwrap()
                .manifest
                .role_declarations
        )
        .unwrap(),
        replacement
    );
}

fn role_declarations_catalog(handler: &ControlHandler) -> (Value, CatalogEntry) {
    let request = Frame::build(
        FrameType::Request,
        control_flags(),
        0,
        0,
        302,
        serde_json::to_vec(&ClientControlRequest::CatalogList { module_id: None }).unwrap(),
    )
    .unwrap();
    let response = handler.handle_catalog_list(request, None).unwrap();
    let raw: Value = serde_json::from_slice(&response[0].body).unwrap();
    let ClientControlResponse::CatalogList { mut modules, .. } =
        serde_json::from_slice(&response[0].body).unwrap()
    else {
        panic!("expected catalog.list");
    };
    (raw["modules"][0].clone(), modules.remove(0))
}

#[test]
fn catalog_list_mirrors_role_declarations_through_hello_and_catalog_update() {
    let registry = Arc::new(Registry::default());
    let handler = ControlHandler::new(Arc::clone(&registry));
    let declarations = json!({
        "routing/v1": {"consumers": [{"name": "chat", "default_demand": 2}]},
        "future/v2": {"unknown": [null, true, {"nested": "λ"}]}
    });
    let connection = ConnectionId::new(301);
    let response = handler
        .handle_control(
            connection,
            hello_with_role_declarations(declarations.clone()),
        )
        .unwrap();
    assert_eq!(response[0].header.ty, FrameType::HelloAck);
    let registered = registry
        .get_module_by_connection(connection)
        .unwrap()
        .unwrap()
        .manifest;
    // Compared as JSON values, not bytes: object key order is not part of
    // the relay's promise, and workspace builds unify serde_json's
    // preserve_order feature, which changes the order a literal keeps.
    assert_eq!(
        serde_json::to_value(&registered.role_declarations).unwrap(),
        declarations
    );
    let (raw, entry) = role_declarations_catalog(&handler);
    assert_eq!(raw["role_declarations"], declarations);
    assert_eq!(entry.role_declarations, registered.role_declarations);

    let request = Frame::build(FrameType::Request, control_flags(), 0, 0, 303, Vec::new()).unwrap();
    handler
        .handle_catalog_update(connection, request, registered.provides, None, Some(false))
        .unwrap();
    let (raw, entry) = role_declarations_catalog(&handler);
    assert_eq!(raw["role_declarations"], declarations);
    assert_eq!(entry.role_declarations, registered.role_declarations);
}

fn assert_hello_drops_role_declarations_with_warning(declarations: Value, reason: &str) {
    let registry = Arc::new(Registry::default());
    let handler = ControlHandler::new(Arc::clone(&registry));
    let capture = EventCapture::default();
    let _subscriber =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(capture.clone()));
    let response = handler
        .handle_control(
            ConnectionId::new(301),
            hello_with_role_declarations(declarations),
        )
        .unwrap();
    assert_eq!(response[0].header.ty, FrameType::HelloAck);
    assert!(registry
        .get_module_by_connection(ConnectionId::new(301))
        .unwrap()
        .unwrap()
        .manifest
        .role_declarations
        .is_empty());
    let (raw, entry) = role_declarations_catalog(&handler);
    assert!(raw.get("role_declarations").is_none());
    assert!(entry.role_declarations.is_empty());
    let warnings: Vec<_> = capture
        .events()
        .into_iter()
        .filter(|event| {
            event.fields.get("message").map(String::as_str)
                == Some("dropping whole role_declarations map from HELLO")
        })
        .collect();
    assert_eq!(warnings.len(), 1);
    assert_eq!(warnings[0].level, tracing::Level::WARN);
    assert_eq!(warnings[0].fields["module_id"], "\"role-test\"");
    assert_eq!(warnings[0].fields["reason"], reason);
}

#[test]
fn hello_non_object_role_declarations_drop_whole_map_and_warn() {
    for value in [
        json!(null),
        json!(false),
        json!(0),
        json!("opaque"),
        json!([]),
    ] {
        assert_hello_drops_role_declarations_with_warning(
            json!({"routing/v1": {}, "broken/v1": value}),
            "role_declarations values must be objects",
        );
    }
    assert_hello_drops_role_declarations_with_warning(
        json!([]),
        "role_declarations must be an object",
    );
}

#[test]
fn hello_bad_role_declaration_key_drops_whole_map_and_warns() {
    assert_hello_drops_role_declarations_with_warning(
        json!({"routing/v1": {}, "bad key": {}}),
        "role_declarations key must use capability-name grammar",
    );
}

#[test]
fn hello_oversized_role_declarations_drop_whole_map_and_warn() {
    assert_hello_drops_role_declarations_with_warning(
        json!({"routing/v1": {}, "overflow/v1": {"padding": "x".repeat(16_384)}}),
        "role_declarations exceeds 16384 compact JSON bytes",
    );
}

fn hello_with_flow_operations(capabilities: Value, flow_operations: Option<Value>) -> Frame {
    let mut body = serde_json::to_value(ModuleHelloBody {
        manifest: manifest("flow-ops", PROTOCOL_VERSION),
        protocol_ver: PROTOCOL_VERSION,
        control_ops: None,
        launch_nonce: None,
    })
    .unwrap();
    body["manifest"]["provides"] = json!([{
        "role": "management_surface",
        "operations": [
            {"name": "diagnostics.findings", "kind": "query"},
            {"name": "reset", "kind": "mutate"}
        ],
        "config_schema": {}, "observability": [], "identity_scope": []
    }]);
    body["manifest"]["capabilities"] = json!({ "provides": capabilities });
    if let Some(flow_operations) = flow_operations {
        body["manifest"]["flow_operations"] = flow_operations;
    }
    Frame::build(
        FrameType::Hello,
        control_flags(),
        0,
        0,
        401,
        serde_json::to_vec(&body).unwrap(),
    )
    .unwrap()
}

#[test]
fn catalog_list_mirrors_flow_operations_unchanged() {
    // Order is kept as declared (not sorted) and an empty list stays an
    // affirmative "none", distinct from an absent declaration.
    for (capability, flow_operations) in [
        ("flow-scopes/v1", json!(["reset", "diagnostics.findings"])),
        ("call-modes/v1", json!(["diagnostics.findings"])),
        ("flow-scopes/v1", json!([])),
    ] {
        let registry = Arc::new(Registry::default());
        let handler = ControlHandler::new(Arc::clone(&registry));
        let connection = ConnectionId::new(401);
        let response = handler
            .handle_control(
                connection,
                hello_with_flow_operations(json!([capability]), Some(flow_operations.clone())),
            )
            .unwrap();
        assert_eq!(response[0].header.ty, FrameType::HelloAck);
        let registered = registry
            .get_module_by_connection(connection)
            .unwrap()
            .unwrap()
            .manifest;
        assert_eq!(
            serde_json::to_value(&registered.flow_operations).unwrap(),
            flow_operations
        );
        let (raw, entry) = role_declarations_catalog(&handler);
        assert_eq!(raw["flow_operations"], flow_operations);
        assert_eq!(entry.flow_operations, registered.flow_operations);
    }

    let registry = Arc::new(Registry::default());
    let handler = ControlHandler::new(Arc::clone(&registry));
    let response = handler
        .handle_control(
            ConnectionId::new(401),
            hello_with_flow_operations(json!(["flow-scopes/v1"]), None),
        )
        .unwrap();
    assert_eq!(response[0].header.ty, FrameType::HelloAck);
    let (raw, entry) = role_declarations_catalog(&handler);
    assert!(raw.get("flow_operations").is_none());
    assert_eq!(entry.flow_operations, None);
}

#[test]
fn operation_deferral_hello_accepts_valid_and_invalid_maps_and_catalog_relays_only_valid() {
    let oversized: serde_json::Map<String, Value> = (0..257)
        .map(|n| (format!("tool:t{n}"), json!(true)))
        .collect();
    for (capabilities, map, valid) in [
        (
            json!(["scope-federated/v1"]),
            json!({"tool:read":true,"management:reset":true}),
            true,
        ),
        (json!(["scope-federated/v1"]), json!({}), true),
        (json!([]), json!({"tool:read":true}), false),
        (
            json!(["scope-federated/v1"]),
            json!({"tool:valid":true,"tool:bad":false}),
            false,
        ),
        (
            json!(["scope-federated/v1"]),
            json!({"tool:valid":true,"management:missing":true}),
            false,
        ),
        (
            json!(["scope-federated/v1"]),
            Value::Object(oversized),
            false,
        ),
    ] {
        let capture = EventCapture::default();
        let _subscriber =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(capture.clone()));
        let registry = Arc::new(Registry::default());
        let handler = ControlHandler::new(Arc::clone(&registry));
        let hello = hello_with_flow_operations(capabilities, None);
        let mut body: Value = serde_json::from_slice(&hello.body).unwrap();
        body["manifest"]["operation_deferral"] = map.clone();
        let hello = Frame::build(
            FrameType::Hello,
            control_flags(),
            0,
            0,
            401,
            serde_json::to_vec(&body).unwrap(),
        )
        .unwrap();
        let response = handler
            .handle_control(ConnectionId::new(401), hello)
            .unwrap();
        assert_eq!(response[0].header.ty, FrameType::HelloAck);
        let (raw, entry) = role_declarations_catalog(&handler);
        if valid {
            assert_eq!(raw["operation_deferral"], map);
            assert_eq!(
                serde_json::to_value(entry.operation_deferral.unwrap()).unwrap(),
                map
            );
        } else {
            assert!(entry.operation_deferral.is_none());
            assert!(raw.get("operation_deferral").is_none());
            assert_eq!(
                capture
                    .events()
                    .iter()
                    .filter(|event| event.fields.get("message").map(String::as_str)
                        == Some("dropping operation_deferral from HELLO"))
                    .count(),
                1
            );
        }
    }
}

fn assert_hello_drops_flow_operations_with_warning(
    capabilities: Value,
    flow_operations: Value,
    reason: &str,
) {
    let registry = Arc::new(Registry::default());
    let handler = ControlHandler::new(Arc::clone(&registry));
    let capture = EventCapture::default();
    let _subscriber =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(capture.clone()));
    let response = handler
        .handle_control(
            ConnectionId::new(401),
            hello_with_flow_operations(capabilities, Some(flow_operations)),
        )
        .unwrap();
    assert_eq!(response[0].header.ty, FrameType::HelloAck);
    let registered = registry
        .get_module_by_connection(ConnectionId::new(401))
        .unwrap()
        .expect("the module still registers")
        .manifest;
    assert_eq!(registered.module_id, "flow-ops");
    assert_eq!(registered.flow_operations, None);
    let (raw, entry) = role_declarations_catalog(&handler);
    assert_eq!(raw["module_id"], "flow-ops");
    assert!(raw.get("flow_operations").is_none());
    assert_eq!(entry.flow_operations, None);
    let warnings: Vec<_> = capture
        .events()
        .into_iter()
        .filter(|event| {
            event.fields.get("message").map(String::as_str)
                == Some("dropping flow_operations from HELLO")
        })
        .collect();
    assert_eq!(warnings.len(), 1);
    assert_eq!(warnings[0].level, tracing::Level::WARN);
    assert_eq!(warnings[0].fields["module_id"], "\"flow-ops\"");
    assert_eq!(warnings[0].fields["reason"], reason);
}

#[test]
fn hello_flow_operations_naming_unknown_operation_is_dropped_with_warning() {
    assert_hello_drops_flow_operations_with_warning(
        json!(["flow-scopes/v1"]),
        json!(["diagnostics.findings", "missing"]),
        "flow_operations names 'missing', which is not a declared management operation",
    );
}

#[test]
fn hello_flow_operations_without_flow_capability_is_dropped_with_warning() {
    assert_hello_drops_flow_operations_with_warning(
        json!([]),
        json!(["diagnostics.findings"]),
        "flow_operations requires 'flow-scopes/v1' or 'call-modes/v1' in capabilities.provides",
    );
    // A run-only capability does not admit flows either.
    assert_hello_drops_flow_operations_with_warning(
        json!(["agent-run-scopes/v1"]),
        json!([]),
        "flow_operations requires 'flow-scopes/v1' or 'call-modes/v1' in capabilities.provides",
    );
}

#[test]
fn hello_malformed_flow_operations_is_dropped_with_warning() {
    assert_hello_drops_flow_operations_with_warning(
        json!(["flow-scopes/v1"]),
        json!(["reset", "reset"]),
        "flow_operations names 'reset' more than once",
    );
    assert_hello_drops_flow_operations_with_warning(
        json!(["flow-scopes/v1"]),
        json!(vec!["reset"; 257]),
        "flow_operations lists 257 names, more than 256",
    );
    assert_hello_drops_flow_operations_with_warning(
        json!(["flow-scopes/v1"]),
        json!("reset"),
        "flow_operations must be an array of operation names",
    );
}

#[test]
fn rich_ui_descriptor_malformed_fields_are_refused_by_name() {
    let registry = Arc::new(Registry::default());
    let handler = ControlHandler::new(registry.clone());
    let descriptor = json!({
        "schema": "ckios.richui.manifest/1",
        "manifest_digest": format!("sha256:{}", "a".repeat(64)),
        "bytes": 128,
        "chunks": 2
    });
    let malformed = [
        ("schema", json!("")),
        ("schema", json!("a".repeat(65))),
        ("schema", json!("Bad schema")),
        (
            "manifest_digest",
            json!(format!("sha256:{}", "A".repeat(64))),
        ),
        ("manifest_digest", json!("sha256:abc")),
        ("bytes", json!(0)),
        ("bytes", json!(16_777_217)),
        ("bytes", json!("128")),
        ("chunks", json!(0)),
        ("chunks", json!(1025)),
        ("chunks", json!("2")),
    ];
    for (index, (field, invalid_value)) in malformed.into_iter().enumerate() {
        let mut invalid = descriptor.clone();
        invalid[field] = invalid_value;
        let response = handler
            .handle_control(
                ConnectionId::new(210 + index as u64),
                hello_frame_with_rich_ui(
                    &format!("bad-rich-ui-{index}"),
                    PROTOCOL_VERSION,
                    210 + index as u64,
                    None,
                    invalid,
                ),
            )
            .unwrap();
        assert_eq!(response[0].header.ty, FrameType::Error);
        let error = parse_error(&response[0]);
        assert_eq!(error["code"], "invalid_manifest");
        assert!(error["message"]
            .as_str()
            .unwrap()
            .contains(&format!("rich_ui.{field}")));
        assert!(registry
            .get_module(&format!("bad-rich-ui-{index}"))
            .unwrap()
            .is_none());
    }
}

#[test]
fn catalog_update_refuses_malformed_rich_ui_by_field_name() {
    let registry = Arc::new(Registry::default());
    let handler = ControlHandler::new(Arc::clone(&registry));
    let connection = ConnectionId::new(219);
    registry
        .register_with_control_ops(
            ModuleManifest::builder("invalid-rich-ui", "0.1.0")
                .rich_ui(Some(subc_protocol::manifest::RichUiDescriptor::new(
                    "bad schema",
                    format!("sha256:{}", "a".repeat(64)),
                    128,
                    2,
                )))
                .build(),
            PROTOCOL_VERSION,
            connection,
            Vec::new(),
        )
        .unwrap();
    let request = Frame::build(FrameType::Request, control_flags(), 0, 0, 219, Vec::new()).unwrap();
    let response = handler
        .handle_catalog_update(connection, request, Vec::new(), None, None)
        .unwrap();
    assert_eq!(response[0].header.ty, FrameType::Error);
    let error = parse_error(&response[0]);
    assert_eq!(error["code"], "invalid_manifest");
    assert!(error["message"]
        .as_str()
        .unwrap()
        .contains("rich_ui.schema"));
}

#[test]
fn swap_candidate_hello_is_nonce_verified_after_promotion() {
    let registry = Arc::new(Registry::default());
    let supervisor = SupervisorHandle::new();
    supervisor.open_swap("provider", "candidate-nonce".to_string());
    let handler = ControlHandler::new(Arc::clone(&registry)).with_supervisor(supervisor);
    let connection = ConnectionId::new(218);
    let hello = handler
        .handle_control(
            connection,
            hello_frame_with_nonce("provider", PROTOCOL_VERSION, 218, Some("candidate-nonce")),
        )
        .unwrap();
    assert_eq!(hello[0].header.ty, FrameType::HelloAck);
    assert!(registry.list_modules().unwrap().1.is_empty());

    registry.promote_candidate("provider").unwrap().unwrap();
    let request = Frame::build(
        FrameType::Request,
        control_flags(),
        0,
        0,
        219,
        serde_json::to_vec(&ClientControlRequest::CatalogList { module_id: None }).unwrap(),
    )
    .unwrap();
    let response = handler.handle_catalog_list(request, None).unwrap();
    let ClientControlResponse::CatalogList { modules, .. } =
        serde_json::from_slice(&response[0].body).unwrap()
    else {
        panic!("expected catalog.list");
    };
    assert_eq!(modules.len(), 1);
    assert!(modules[0].nonce_verified);
}

#[test]
fn catalog_list_with_provenance_mirrors_declared_build_bytes() {
    for provenance_json in [
        r#"{"build_git_sha":"0123456789abcdef0123456789abcdef01234567","build_lock_digest":"abababababababababababababababababababababababababababababababab","wire_crate_version":"0.34.0","store_schema_version":"7","launch_nonce_source":"fd"}"#,
        r#"{"build_git_sha_absence_reason":"no_git_dir","build_lock_digest":"abababababababababababababababababababababababababababababababab"}"#,
    ] {
        let registry = Arc::new(Registry::default());
        let handler = ControlHandler::new(registry);
        let hello = Frame::build(
            FrameType::Hello,
            control_flags(),
            0,
            0,
            220,
            serde_json::to_vec(&ModuleHelloBody {
                manifest: ModuleManifest::builder("built", "0.1.0")
                    .provenance(Some(serde_json::from_str(provenance_json).unwrap()))
                    .build(),
                protocol_ver: PROTOCOL_VERSION,
                control_ops: None,
                launch_nonce: None,
            })
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            handler
                .handle_control(ConnectionId::new(220), hello)
                .unwrap()[0]
                .header
                .ty,
            FrameType::HelloAck
        );
        let request = Frame::build(
            FrameType::Request,
            control_flags(),
            0,
            0,
            221,
            serde_json::to_vec(&ClientControlRequest::CatalogList { module_id: None }).unwrap(),
        )
        .unwrap();
        let response = handler.handle_catalog_list(request, None).unwrap();
        let ClientControlResponse::CatalogList { modules, .. } =
            serde_json::from_slice(&response[0].body).unwrap()
        else {
            panic!("expected catalog.list");
        };
        assert_eq!(modules.len(), 1);
        assert_eq!(
                serde_json::to_vec(&catalog_legacy_projection(modules[0].clone())).unwrap(),
                format!(
                    r#"{{"module_id":"built","ready":true,"module_version":"0.1.0","provenance":{provenance_json},"roles":[],"control_ops":["route.bind","route.status"]}}"#
                )
                .as_bytes()
            );
    }
}

#[test]
fn catalog_list_without_rich_ui_or_nonce_keeps_legacy_entry_bytes() {
    let registry = Arc::new(Registry::default());
    let handler = ControlHandler::new(Arc::clone(&registry));
    let hello = Frame::build(
        FrameType::Hello,
        control_flags(),
        0,
        0,
        220,
        serde_json::to_vec(&ModuleHelloBody {
            manifest: ModuleManifest::builder("plain", "0.1.0").build(),
            protocol_ver: PROTOCOL_VERSION,
            control_ops: None,
            launch_nonce: Some("unverified".to_string()),
        })
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        handler
            .handle_control(ConnectionId::new(220), hello)
            .unwrap()[0]
            .header
            .ty,
        FrameType::HelloAck
    );
    let request = Frame::build(
        FrameType::Request,
        control_flags(),
        0,
        0,
        221,
        serde_json::to_vec(&ClientControlRequest::CatalogList { module_id: None }).unwrap(),
    )
    .unwrap();
    let response = handler.handle_catalog_list(request, None).unwrap();
    let wire: serde_json::Value = serde_json::from_slice(&response[0].body).unwrap();
    assert!(wire["modules"][0].get("provenance").is_none());
    let ClientControlResponse::CatalogList { modules, .. } =
        serde_json::from_slice(&response[0].body).unwrap()
    else {
        panic!("expected catalog.list");
    };
    assert_eq!(
            serde_json::to_vec(&catalog_legacy_projection(modules[0].clone())).unwrap(),
            br#"{"module_id":"plain","ready":true,"module_version":"0.1.0","roles":[],"control_ops":["route.bind","route.status"]}"#
        );
    assert!(!modules[0].nonce_verified);
    assert!(modules[0].provenance.is_none());
}

// Preserve the legacy-field byte pins while discovery adds optional facts.
fn catalog_legacy_projection(mut entry: CatalogEntry) -> CatalogEntry {
    assert_eq!(entry.first_party, Some(false));
    entry.incarnation = None;
    entry.spawn = None;
    entry.first_party = None;
    entry.origin = None;
    entry.requested_declarations_digest = None;
    entry.approved_declarations_digest = None;
    entry
}

#[test]
fn reserved_capability_refusal_mutation_proof_leaves_no_catalog_entry() {
    let registry = Arc::new(Registry::default());
    let handler = ControlHandler::new(Arc::clone(&registry)).with_capability_config(
        [("vault".to_string(), true), ("squatter".to_string(), true)],
        BTreeMap::from([("credentials-provider/v1".to_string(), "vault".to_string())]),
    );
    let mut squatter = manifest("squatter", PROTOCOL_VERSION);
    squatter.capabilities = Some(subc_protocol::manifest::CapabilityDeclarations {
        provides: vec!["credentials-provider/v1".to_string()],
        requires: Vec::new(),
        must_never_reach: Vec::new(),
    });
    let frame = Frame::build(
        FrameType::Hello,
        control_flags(),
        0,
        0,
        77,
        serde_json::to_vec(&ModuleHelloBody {
            manifest: squatter,
            protocol_ver: PROTOCOL_VERSION,
            control_ops: None,
            launch_nonce: None,
        })
        .expect("HELLO serializes"),
    )
    .expect("HELLO frame builds");
    let response = handler
        .handle_control(ConnectionId::new(77), frame)
        .expect("reserved claim receives a typed refusal");
    assert_eq!(parse_error(&response[0])["code"], "reserved_capability");
    assert_eq!(
        registry
            .active_registration_count()
            .expect("registry reads"),
        0,
        "a reserved capability refusal must not leave a catalog entry"
    );
}

#[test]
fn stale_relay_settlement_cannot_release_the_half_open_probe() {
    for settlement in ["timeout", "inconclusive", "drop"] {
        let breakers = RouteBindBreakers::default();
        let RouteBindAdmission::Admitted {
            guard: mut old,
            probe: false,
        } = breakers.admit("prov")
        else {
            panic!("ordinary relay admitted")
        };
        let RouteBindAdmission::Admitted {
            guard: mut opener, ..
        } = breakers.admit("prov")
        else {
            panic!("second relay admitted")
        };
        assert!(
            !opener
                .record_timeout(1, Duration::ZERO)
                .unwrap()
                .reopened_after_probe
        );
        let RouteBindAdmission::Admitted {
            guard: mut probe,
            probe: true,
        } = breakers.admit("prov")
        else {
            panic!("one cooldown probe admitted")
        };
        match settlement {
            "timeout" => assert!(
                !old.record_timeout(1, Duration::ZERO)
                    .unwrap()
                    .reopened_after_probe
            ),
            "inconclusive" => old.record_inconclusive(),
            "drop" => drop(old),
            _ => unreachable!(),
        }
        assert!(
            matches!(
                breakers.admit("prov"),
                RouteBindAdmission::Refused {
                    probe_in_flight: true,
                    ..
                }
            ),
            "{settlement} of a pre-open relay cannot release the real probe"
        );
        assert!(
            probe
                .record_timeout(1, Duration::ZERO)
                .unwrap()
                .reopened_after_probe
        );
        assert!(matches!(
            breakers.admit("prov"),
            RouteBindAdmission::Admitted { probe: true, .. }
        ));
    }
    let breakers = RouteBindBreakers::default();
    let admit = || match breakers.admit("prov") {
        RouteBindAdmission::Admitted { guard, .. } => guard,
        _ => panic!("relay admitted"),
    };
    admit().record_timeout(1, Duration::ZERO);
    let mut old_probe = admit();
    breakers.reset_for_new_module_connection("prov");
    admit().record_timeout(1, Duration::ZERO);
    let _new_probe = admit();
    old_probe.record_inconclusive();
    assert!(matches!(
        breakers.admit("prov"),
        RouteBindAdmission::Refused {
            probe_in_flight: true,
            ..
        }
    ));
}

#[tokio::test]
async fn catalog_update_refuses_reserved_capabilities_for_active_and_candidate() {
    for candidate in [false, true] {
        let registry = Arc::new(Registry::default());
        let handler = ControlHandler::new(Arc::clone(&registry)).with_capability_config(
            [("vault".to_string(), true), ("squatter".to_string(), true)],
            BTreeMap::from([("credentials-provider/v1".to_string(), "vault".to_string())]),
        );
        let conn = ConnectionId::new(77);
        let (ctx, mut rx) = route_ctx(conn);
        let initial = capability_manifest("squatter", &[], &[]);
        if candidate {
            registry
                .register_candidate_with_control_ops(
                    initial.clone(),
                    PROTOCOL_VERSION,
                    conn,
                    module_baseline_control_ops(),
                )
                .unwrap();
            handler
                .forwarding
                .register_candidate_module_connection(
                    conn,
                    "squatter".to_string(),
                    PROTOCOL_VERSION,
                    manifest_concurrency(&initial),
                    ctx.egress.clone(),
                )
                .unwrap();
        } else {
            hello_via_sink(
                &handler,
                &ctx,
                &mut rx,
                hello_frame_with_manifest(initial.clone(), 1),
            )
            .await;
        }
        let response = handler
            .handle_control_frame(
                &ctx,
                catalog_update_with_capabilities_frame(
                    2,
                    capability_manifest("squatter", &["credentials-provider/v1"], &[])
                        .capabilities
                        .unwrap(),
                ),
            )
            .await
            .unwrap();
        assert_eq!(parse_error(&response[0])["code"], "reserved_capability");
        assert_eq!(
            registry
                .get_module_by_connection(conn)
                .unwrap()
                .unwrap()
                .manifest,
            initial
        );
    }
}

#[test]
#[cfg(not(feature = "dev-desktop-approval"))]
fn server_describe_default_build_advertises_no_config_capability() {
    let assert_default = |handler: &ControlHandler| {
        let request = Frame::build(
            FrameType::Request,
            control_flags(),
            0,
            0,
            79,
            serde_json::to_vec(&ClientControlRequest::ServerDescribe {}).unwrap(),
        )
        .unwrap();
        let responses = handler.handle_server_describe(request).unwrap();
        assert_eq!(responses.len(), 1);
        let response: Value = serde_json::from_slice(&responses[0].body).unwrap();
        assert_eq!(response["op"], "server.describe");
        let capabilities = response["capabilities"]
            .as_array()
            .expect("capability array");
        assert!(
            !capabilities.is_empty(),
            "exercise an actual describe response, not an empty fixture"
        );
        assert!(
            capabilities
                .iter()
                .all(|capability| !capability.as_str().unwrap().starts_with("config.")),
            "a default build must not announce configuration authority: {capabilities:?}"
        );
    };
    assert_default(&ControlHandler::new(Arc::new(Registry::default())));
    #[cfg(unix)]
    {
        // A local store does not prove that project-membership validation,
        // active-scope usage tracking and approved-program launch selection
        // are connected. Defaults must not advertise that authority.
        let (_rig, _owner, _supervisor, handler, _spec) =
            crate::config_gate::runtime::RuntimeOwner::unactivated_supervisor_fixture_for_test();
        assert_default(&handler);
    }
}

#[test]
fn server_describe_surfaces_required_capability_verdict_fields() {
    let registry = Arc::new(Registry::default());
    let handler = ControlHandler::new(Arc::clone(&registry)).with_capability_config(
        [
            ("consumer".to_string(), true),
            ("provider".to_string(), false),
        ],
        BTreeMap::new(),
    );
    let mut consumer = manifest("consumer", PROTOCOL_VERSION);
    consumer.capabilities = Some(subc_protocol::manifest::CapabilityDeclarations {
        provides: Vec::new(),
        requires: vec![subc_protocol::manifest::CapabilityRequirement {
            capability: "credentials-provider/v1".to_string(),
            need: subc_protocol::manifest::CapabilityNeed::Required,
        }],
        must_never_reach: Vec::new(),
    });
    let hello = Frame::build(
        FrameType::Hello,
        control_flags(),
        0,
        0,
        78,
        serde_json::to_vec(&ModuleHelloBody {
            manifest: consumer,
            protocol_ver: PROTOCOL_VERSION,
            control_ops: None,
            launch_nonce: None,
        })
        .expect("HELLO serializes"),
    )
    .expect("HELLO frame builds");
    handler
        .handle_control(ConnectionId::new(78), hello)
        .expect("consumer registers");
    let describe = Frame::build(
        FrameType::Request,
        control_flags(),
        0,
        0,
        79,
        serde_json::to_vec(&ClientControlRequest::ServerDescribe {}).expect("request serializes"),
    )
    .expect("describe frame builds");
    let response = handler
        .handle_server_describe(describe)
        .expect("server.describe succeeds");
    let rendered: Value = serde_json::from_slice(&response[0].body).expect("response JSON");
    let requirement = &rendered["capability_requirements"][0];
    assert_eq!(requirement["consumer"], "consumer");
    assert_eq!(requirement["verdict"], "never_provided");
    assert_eq!(requirement["episode_seq"], 1);
    assert_eq!(requirement["config_satisfiable"], false);
    assert_eq!(requirement["runtime_available"], false);
    assert!(requirement["detail"]
        .as_str()
        .expect("detail string")
        .contains("credentials-provider/v1"));
}

/// Register a consumer that requires `project-identity/v1` and the provider
/// of it, optionally mark daemon shutdown as begun, then deregister the
/// provider as its connection closes. Returns the `capability.requirement`
/// events that deregistration logged and the verdict the evaluator then
/// holds for the requirement (the one `route.open` refusals read).
fn deregister_required_provider(
    daemon_shutdown_begun: bool,
) -> (Vec<CapturedEvent>, Option<CapabilityVerdict>) {
    const CAPABILITY: &str = "project-identity/v1";
    let registry = Arc::new(Registry::default());
    let daemon_shutdown = DaemonShutdownFlag::default();
    let handler = ControlHandler::new(Arc::clone(&registry))
        .with_capability_config(
            [
                ("consumer".to_string(), true),
                ("provider".to_string(), true),
            ],
            BTreeMap::new(),
        )
        .with_daemon_shutdown_flag(daemon_shutdown.clone());
    let mut consumer = manifest("consumer", PROTOCOL_VERSION);
    consumer.capabilities = Some(CapabilityDeclarations {
        provides: Vec::new(),
        requires: vec![subc_protocol::manifest::CapabilityRequirement {
            capability: CAPABILITY.to_string(),
            need: subc_protocol::manifest::CapabilityNeed::Required,
        }],
        must_never_reach: Vec::new(),
    });
    let provider = capability_manifest("provider", &[CAPABILITY], &[]);
    let provider_conn = ConnectionId::new(182);
    for (conn, manifest, corr) in [
        (ConnectionId::new(181), consumer, 181),
        (provider_conn, provider, 182),
    ] {
        let hello = handler
            .handle_control(conn, hello_frame_with_manifest(manifest, corr))
            .expect("module registers");
        assert_eq!(hello[0].header.ty, FrameType::HelloAck);
    }
    assert_eq!(
        handler.capability_evaluator.verdict("consumer", CAPABILITY),
        Some(CapabilityVerdict::Provided),
        "the registered provider must satisfy the requirement first"
    );
    if daemon_shutdown_begun {
        daemon_shutdown.set_for_test();
    }

    let capture = EventCapture::default();
    let _subscriber =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(capture.clone()));
    handler
        .cleanup_connection(provider_conn)
        .expect("provider deregisters");
    let events = capture
        .events()
        .into_iter()
        .filter(|event| {
            event.fields.get("message").map(String::as_str) == Some("capability.requirement")
        })
        .collect();
    (
        events,
        handler.capability_evaluator.verdict("consumer", CAPABILITY),
    )
}

#[test]
fn required_provider_leaving_during_daemon_shutdown_logs_info_with_shutdown_cause() {
    let (events, verdict) = deregister_required_provider(true);
    assert_eq!(
        verdict,
        Some(CapabilityVerdict::NeverProvided),
        "shutdown must not change the verdict route.open refusals read"
    );
    assert_eq!(events.len(), 1, "one requirement transition: {events:?}");
    let event = &events[0];
    assert_eq!(event.level, tracing::Level::INFO, "{event:?}");
    assert_eq!(
        event.fields.get("cause").map(String::as_str),
        Some("\"daemon_shutdown\""),
        "{event:?}"
    );
    assert_eq!(
        event.fields.get("verdict").map(String::as_str),
        Some("\"never_provided\""),
        "{event:?}"
    );
}

#[test]
fn required_provider_leaving_while_daemon_runs_still_logs_error() {
    let (events, verdict) = deregister_required_provider(false);
    assert_eq!(verdict, Some(CapabilityVerdict::NeverProvided));
    assert_eq!(events.len(), 1, "one requirement transition: {events:?}");
    let event = &events[0];
    assert_eq!(event.level, tracing::Level::ERROR, "{event:?}");
    assert_eq!(event.fields.get("cause"), None, "{event:?}");
    assert_eq!(
        event.fields.get("verdict").map(String::as_str),
        Some("\"never_provided\""),
        "{event:?}"
    );
}

#[test]
fn catalog_list_omits_capabilities_for_legacy_manifest() {
    let registry = Arc::new(Registry::default());
    let handler = ControlHandler::new(Arc::clone(&registry));
    let hello = handler
        .handle_control(
            ConnectionId::new(101),
            hello_frame("legacy-capability-manifest", PROTOCOL_VERSION, 101),
        )
        .expect("legacy HELLO registers");
    assert_eq!(hello[0].header.ty, FrameType::HelloAck);

    let request = Frame::build(
        FrameType::Request,
        control_flags(),
        0,
        0,
        102,
        serde_json::to_vec(&ClientControlRequest::CatalogList { module_id: None })
            .expect("catalog request serializes"),
    )
    .expect("catalog request frame builds");
    let response = handler
        .handle_catalog_list(request, None)
        .expect("catalog list succeeds");
    let body: Value = serde_json::from_slice(&response[0].body).expect("catalog response JSON");
    assert!(
        body["modules"][0].get("capabilities").is_none(),
        "legacy manifest must retain an absent capabilities field on catalog.list"
    );
}

#[test]
fn hello_ack_omits_storage_when_no_storage_config() {
    let registry = Arc::new(Registry::default());
    let handler = ControlHandler::new(Arc::clone(&registry));
    let responses = handler
        .handle_control(
            ConnectionId::new(1),
            hello_frame("aft", PROTOCOL_VERSION, 7),
        )
        .unwrap();
    let ack = parse_ack(&responses[0]);
    assert_eq!(ack.storage, None, "no storage config -> no descriptor");
    assert_eq!(ack.machine_id, None, "no machine id configured -> no field");
}

#[tokio::test]
async fn hello_ack_and_server_describe_carry_the_configured_machine_id() {
    let id = crate::machine_id::MachineId::parse("0123456789abcdef0123456789abcdef").unwrap();
    let registry = Arc::new(Registry::default());
    let handler = ControlHandler::new(Arc::clone(&registry)).with_machine_id(Some(id.clone()));
    let responses = handler
        .handle_control(
            ConnectionId::new(1),
            hello_frame("aft", PROTOCOL_VERSION, 7),
        )
        .unwrap();
    let ack = parse_ack(&responses[0]);
    assert_eq!(ack.machine_id.as_deref(), Some(id.as_str()));

    let described = handler
        .handle_control_frame(
            &route_ctx(ConnectionId::new(2)).0,
            Frame::build(
                FrameType::Request,
                control_flags(),
                0,
                0,
                9,
                serde_json::to_vec(&ClientControlRequest::ServerDescribe {}).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let ClientControlResponse::ServerDescribe { machine_id, .. } =
        serde_json::from_slice(&described[0].body).unwrap()
    else {
        panic!("server.describe answered with another shape");
    };
    assert_eq!(machine_id.as_deref(), Some(id.as_str()));
}

#[test]
fn hello_ack_delivers_resolved_storage_descriptor_per_module() {
    // With a central sqlite storage policy, each registering module gets its
    // own resolved descriptor in HELLO_ACK, keyed by its module id.
    let registry = Arc::new(Registry::default());
    let handler = ControlHandler::new(Arc::clone(&registry)).with_storage_config(Some(
        crate::daemon_config::StorageConfig::Sqlite {
            data_home: std::path::PathBuf::from("/data"),
        },
    ));

    let responses = handler
        .handle_control(
            ConnectionId::new(1),
            hello_frame("alfonso-routing", PROTOCOL_VERSION, 7),
        )
        .unwrap();
    let ack = parse_ack(&responses[0]);
    assert_eq!(
        ack.storage,
        Some(serde_json::json!({
            "module_id": "alfonso-routing",
            "storage_namespace": "default",
            "isolation": { "kind": "module" },
            "backend": {
                "backend": "sqlite",
                "path": "/data/cortexkit/alfonso-routing/store.db"
            }
        })),
        "the delivered descriptor is the module's own sqlite store path"
    );
}

#[test]
fn hello_control_ops_none_is_baseline_and_guard_rejects_synthetic_gated_op() {
    let registry = Arc::new(Registry::default());
    let handler = ControlHandler::new(Arc::clone(&registry));
    let conn = ConnectionId::new(1);
    let responses = handler
        .handle_control(
            conn,
            hello_frame_with_control_ops("aft", PROTOCOL_VERSION, 7, None),
        )
        .unwrap();
    assert_eq!(responses[0].header.ty, FrameType::HelloAck);
    let registration = registry.get_module("aft").unwrap().unwrap();
    assert_eq!(registration.control_ops, module_baseline_control_ops());

    let frame = Frame::build(FrameType::Request, control_flags(), 0, 0, 77, Vec::new()).unwrap();
    assert!(handler
        .guard_module_control_op(&frame, "aft", "route.bind")
        .unwrap()
        .is_none());
    let error = handler
        .guard_module_control_op(&frame, "aft", "test.synthetic")
        .unwrap()
        .expect("synthetic ungranted op should be rejected");
    assert_eq!(error.header.ty, FrameType::Error);
    assert_eq!(parse_error(&error)["code"], "op_not_allowed");
}

#[test]
fn hello_control_ops_some_adds_optional_grants() {
    let registry = Arc::new(Registry::default());
    let handler = ControlHandler::new(Arc::clone(&registry));
    handler
        .handle_control(
            ConnectionId::new(1),
            hello_frame_with_control_ops(
                "aft",
                PROTOCOL_VERSION,
                7,
                Some(vec![
                    "future.synthetic".to_string(),
                    "route.bind".to_string(),
                ]),
            ),
        )
        .unwrap();
    let registration = registry.get_module("aft").unwrap().unwrap();
    assert_eq!(
        registration.control_ops,
        vec![
            "route.bind".to_string(),
            "route.status".to_string(),
            "future.synthetic".to_string(),
        ]
    );
    let frame = Frame::build(FrameType::Request, control_flags(), 0, 0, 78, Vec::new()).unwrap();
    assert!(handler
        .guard_module_control_op(&frame, "aft", "future.synthetic")
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn health_probe_refuses_unadvertised_module_without_sending_frame() {
    let registry = Arc::new(Registry::default());
    let forwarding = Arc::new(ForwardingTable::default());
    let handler = ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding));
    let (module_ctx, mut module_rx) = route_ctx(ConnectionId::new(10));
    hello_via_sink(
        &handler,
        &module_ctx,
        &mut module_rx,
        hello_frame_with_control_ops("aft", PROTOCOL_VERSION, 7, None),
    )
    .await;

    let (client_ctx, _client_rx) = route_ctx(ConnectionId::new(20));
    let responses = handler
        .handle_control_frame(&client_ctx, supervisor_health_probe_frame(77, "aft"))
        .await
        .unwrap();
    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0].header.ty, FrameType::Error);
    assert_eq!(parse_error(&responses[0])["code"], "health_not_advertised");
    assert!(module_rx.try_recv().is_err());
}

#[tokio::test]
async fn health_probe_demuxes_while_route_bind_relay_is_in_flight() {
    let registry = Arc::new(Registry::default());
    let forwarding = Arc::new(ForwardingTable::default());
    let handler = ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding));
    let (module_ctx, mut module_rx) = route_ctx(ConnectionId::new(30));
    hello_via_sink(
        &handler,
        &module_ctx,
        &mut module_rx,
        hello_frame_with_control_ops(
            "aft",
            PROTOCOL_VERSION,
            7,
            Some(vec![MODULE_CONTROL_OP_HEALTH_CHECK.to_string()]),
        ),
    )
    .await;

    let project_root = unique_project_root("demux");
    let (route_client_ctx, mut route_client_rx) = route_ctx(ConnectionId::new(31));
    let route_handler = handler.clone();
    let route_task = tokio::spawn(async move {
        route_handler
            .handle_control_frame(
                &route_client_ctx,
                route_open_frame(100, "aft", project_root),
            )
            .await
            .unwrap()
    });
    let bind_frame = tokio::time::timeout(Duration::from_secs(1), module_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        serde_json::from_slice::<ModuleControlRequest>(&bind_frame.body).unwrap(),
        ModuleControlRequest::RouteBind { .. }
    ));

    let (health_client_ctx, _health_client_rx) = route_ctx(ConnectionId::new(32));
    let health_handler = handler.clone();
    let health_task = tokio::spawn(async move {
        health_handler
            .handle_control_frame(
                &health_client_ctx,
                supervisor_health_probe_frame(101, "aft"),
            )
            .await
            .unwrap()
    });
    let health_frame = tokio::time::timeout(Duration::from_secs(1), module_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<ModuleControlRequest>(&health_frame.body).unwrap(),
        ModuleControlRequest::HealthCheck {}
    );

    handler
        .handle_control_frame(
            &module_ctx,
            health_response(health_frame.header.corr, HealthStatus::Degraded),
        )
        .await
        .unwrap();
    let health_response = health_task.await.unwrap();
    assert_eq!(health_response.len(), 1);
    match serde_json::from_slice::<ClientControlResponse>(&health_response[0].body).unwrap() {
        ClientControlResponse::SupervisorHealthProbe {
            module_id,
            status,
            detail,
            metrics,
        } => {
            assert_eq!(module_id, "aft");
            assert_eq!(status, HealthStatus::Degraded);
            assert_eq!(detail.as_deref(), Some("warming"));
            assert_eq!(metrics, Some(json!({"queue_depth": 3})));
        }
        other => panic!("unexpected health response: {other:?}"),
    }

    handler
        .handle_control_frame(&module_ctx, route_bind_ack(bind_frame.header.corr))
        .await
        .unwrap();
    let route_response = route_task.await.unwrap();
    assert!(route_response.is_empty());
    let published = route_client_rx.recv().await.unwrap();
    assert!(matches!(
        serde_json::from_slice::<ClientControlResponse>(&published.body).unwrap(),
        ClientControlResponse::RouteOpen { .. }
    ));
}

/// Start one `route.open` on `client_connection` and return its still-running
/// handler task together with the `route.bind` the module received for it.
/// The handler blocks until the module answers, so it has to run as a task
/// while the test drives the module side.
async fn relay_route_open(
    handler: &ControlHandler,
    client_connection: ConnectionId,
    client_egress: &FrameSink,
    module_rx: &mut mpsc::Receiver<crate::router::OutboundFrame>,
    corr: u64,
    module_id: &str,
    project_root_label: &str,
) -> (tokio::task::JoinHandle<Vec<Frame>>, Frame) {
    let ctx = RouteCtx {
        connection_id: client_connection,
        egress: client_egress.clone(),
    };
    let handler = handler.clone();
    let project_root = unique_project_root(project_root_label);
    let module_id = module_id.to_string();
    let dispatch = tracing::dispatcher::get_default(|dispatch| dispatch.clone());
    let task = tokio::spawn(async move {
        let _guard = tracing::dispatcher::set_default(&dispatch);
        handler
            .handle_control_frame(&ctx, route_open_frame(corr, &module_id, project_root))
            .await
            .unwrap()
    });
    let bind = tokio::time::timeout(Duration::from_secs(2), module_rx.recv())
        .await
        .expect("module receives the relayed route.bind")
        .expect("module egress is open");
    (task, bind.frame)
}

fn route_bind_channel(frame: &Frame) -> (u16, u32) {
    match serde_json::from_slice::<ModuleControlRequest>(&frame.body).unwrap() {
        ModuleControlRequest::RouteBind {
            route_channel,
            epoch,
            ..
        } => (route_channel, epoch),
        other => panic!("expected a route.bind request, got {other:?}"),
    }
}

fn published_route(frame: &Frame) -> (u16, u32) {
    match serde_json::from_slice::<ClientControlResponse>(&frame.body).unwrap() {
        ClientControlResponse::RouteOpen {
            route_channel,
            route_epoch,
            ..
        } => (route_channel, route_epoch),
        other => panic!("expected a route.open response, got {other:?}"),
    }
}

/// Reproduction of a production outage. A client had `route.open`s in
/// flight to a module and was already marked closing -- its egress had refused a
/// module frame, so the daemon asked its connection to end -- while its sink
/// was still open. When the module acked those binds, the daemon refused to
/// commit a route for a closing client, and that refusal was returned from
/// the MODULE connection's frame handler, where a router error that has no
/// ERROR-frame translation ends the connection. The module saw EOF, exited 0,
/// the supervisor correctly did not respawn a clean exit, and every seat lost
/// its tools for hours -- one client's teardown took down a connection
/// carrying ~170 other routes.
///
/// The window is opened here by calling the production path that opens it
/// (`escalate_client_delivery_failure`) rather than by closing a socket. The
/// state that matters is "in `closing_connections`, sink still open, relay
/// still pending", and it lasts only from the close request until the
/// connection loop reacts to it; a socket-level test can flood a client into
/// that escalation but cannot pin the module's ack inside the window. Closing
/// the socket instead takes the other path entirely -- connection teardown
/// removes the pending relay under the same lock, so the ack finds nothing.
#[tokio::test]
async fn late_bind_ack_for_a_closing_client_keeps_the_module_connection_serving() {
    let registry = Arc::new(Registry::default());
    let forwarding = Arc::new(ForwardingTable::default());
    let handler = ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding));

    let module_connection = ConnectionId::new(30);
    let (module_ctx, mut module_rx) = route_ctx(module_connection);
    hello_via_sink(
        &handler,
        &module_ctx,
        &mut module_rx,
        hello_frame("aft", PROTOCOL_VERSION, 7),
    )
    .await;

    let dying_client = ConnectionId::new(31);
    let (dying_ctx, mut dying_rx) = route_ctx(dying_client);

    // A published route on the dying client. The escalation below only marks
    // a connection closing for a route it has already published.
    let (first_task, first_bind) = relay_route_open(
        &handler,
        dying_client,
        &dying_ctx.egress,
        &mut module_rx,
        100,
        "aft",
        "closing-first",
    )
    .await;
    handler
        .handle_control_frame(&module_ctx, route_bind_ack(first_bind.header.corr))
        .await
        .unwrap();
    assert!(first_task.await.unwrap().is_empty());
    let (first_channel, first_epoch) = published_route(&dying_rx.recv().await.unwrap());

    // A second route.open from the same client, relayed and awaiting its ack.
    let (second_task, second_bind) = relay_route_open(
        &handler,
        dying_client,
        &dying_ctx.egress,
        &mut module_rx,
        101,
        "aft",
        "closing-second",
    )
    .await;
    let (abandoned_channel, abandoned_epoch) = route_bind_channel(&second_bind);

    // The window: the client is closing, its sink is still open, and its
    // second bind is still pending.
    assert!(forwarding
        .escalate_client_delivery_failure(
            dying_client,
            first_channel,
            first_epoch,
            CloseReason::new(
                "module_to_client_delivery_failed",
                "client egress refused a module frame",
            ),
            crate::forwarding::UndeliveredFrame {
                module_id: None,
                sink: &dying_ctx.egress,
            },
        )
        .unwrap());
    assert!(!dying_ctx.egress.is_closed());

    // The frame that used to end the module connection.
    let ack = handler
        .handle_control_frame(&module_ctx, route_bind_ack(second_bind.header.corr))
        .await;
    let module_loop_error = ack.as_ref().err().map(ToString::to_string);
    if module_loop_error.is_some() {
        // What the server's connection loop does with a router error that has
        // no ERROR-frame translation: end the connection, which releases the
        // module's registration and every route on it.
        handler.cleanup_connection(module_connection).unwrap();
    }
    // Read the module's next frame before opening the co-tenant's route, so
    // the GOODBYE assertion below is about THIS ack and not about later
    // traffic. `None` means the module was told nothing.
    let post_ack_module_frame = tokio::time::timeout(Duration::from_secs(1), module_rx.recv())
        .await
        .ok()
        .flatten();

    // 1. The module connection is still registered.
    assert!(
        registry
            .get_module_by_connection(module_connection)
            .unwrap()
            .is_some(),
        "one client's closing connection ended the shared module connection: \
             {module_loop_error:?}"
    );
    // ...and still serving: another client can open and use a route on it.
    let cotenant = ConnectionId::new(32);
    let (cotenant_ctx, mut cotenant_rx) = route_ctx(cotenant);
    let (cotenant_task, cotenant_bind) = relay_route_open(
        &handler,
        cotenant,
        &cotenant_ctx.egress,
        &mut module_rx,
        102,
        "aft",
        "closing-cotenant",
    )
    .await;
    handler
        .handle_control_frame(&module_ctx, route_bind_ack(cotenant_bind.header.corr))
        .await
        .unwrap();
    assert!(cotenant_task.await.unwrap().is_empty());
    let (cotenant_channel, cotenant_epoch) = published_route(&cotenant_rx.recv().await.unwrap());
    assert!(matches!(
        forwarding
            .lookup_data_route(cotenant, cotenant_channel, cotenant_epoch)
            .unwrap(),
        DataRoute::Client(DataRouteState::Bound(_))
    ));

    // 2. The module was told to drop the binding it created for the route
    //    that will never be published.
    let goodbye =
        post_ack_module_frame.expect("module receives a GOODBYE for the abandoned route channel");
    assert_eq!(goodbye.header.ty, FrameType::Goodbye);
    assert_eq!(goodbye.header.channel, abandoned_channel);
    assert_eq!(goodbye.header.epoch, abandoned_epoch);

    // 3. The dying client received nothing: no route was ever published to
    //    it. Its route.open is answered as unavailable, which the connection
    //    loop would write to a socket that is already going away.
    assert!(dying_rx.try_recv().is_err());
    let second_response = second_task.await.unwrap();
    assert_eq!(second_response.len(), 1);
    assert_eq!(
        parse_error(&second_response[0])["code"],
        "target_unavailable"
    );
}

/// One `catalog.list` as a client sees it: the generation and the listed
/// module ids with their readiness.
async fn catalog_snapshot(
    handler: &ControlHandler,
    client_ctx: &RouteCtx,
    corr: u64,
) -> (u64, Vec<(String, bool)>) {
    let body = serde_json::to_vec(&ClientControlRequest::CatalogList { module_id: None }).unwrap();
    let request = Frame::build(FrameType::Request, control_flags(), 0, 0, corr, body).unwrap();
    let reply = handler
        .handle_control_frame(client_ctx, request)
        .await
        .unwrap()
        .pop()
        .unwrap();
    let ClientControlResponse::CatalogList {
        generation,
        modules,
        ..
    } = serde_json::from_slice(&reply.body).unwrap()
    else {
        panic!("not a catalog.list reply");
    };
    (
        generation,
        modules
            .into_iter()
            .map(|entry| (entry.module_id, entry.ready))
            .collect(),
    )
}

/// A handler whose HELLO for module "aft" on connection 40 is parked
/// between reserving its registry entry and installing its forwarding
/// endpoint. Returns the handler, the two-party barrier (wait once to know
/// the HELLO is parked, once more to release it), the HELLO's thread, and
/// the module's context and receiver.
fn hello_held_before_forwarding() -> (
    ControlHandler,
    Arc<std::sync::Barrier>,
    std::thread::JoinHandle<Vec<Frame>>,
    RouteCtx,
    mpsc::Receiver<crate::router::OutboundFrame>,
) {
    let hold = Arc::new(std::sync::Barrier::new(2));
    let handler = ControlHandler::with_forwarding(
        Arc::new(Registry::default()),
        Arc::new(ForwardingTable::default()),
    )
    .with_hello_registry_to_forwarding_hold(Arc::clone(&hold));
    let (module_ctx, module_rx) = route_ctx(ConnectionId::new(40));
    let hello = {
        let handler = handler.clone();
        let module_ctx = module_ctx.clone();
        std::thread::spawn(move || {
            handler
                .handle_hello(
                    module_ctx.connection_id,
                    Some(module_ctx.egress.clone()),
                    hello_frame("aft", PROTOCOL_VERSION, 7),
                )
                .unwrap()
        })
    };
    (handler, hold, hello, module_ctx, module_rx)
}

/// If a lookup can acquire the registry while publication is parked, let
/// the lookup finish before releasing HELLO. Otherwise release HELLO only
/// after the lookup has tried the lock. Both schedules avoid timing guesses
/// and return the actual lookup result, including an unpublished snapshot
/// when the publication lock is missing.
fn lookup_during_hello_publication<T: Send + 'static>(
    registry: &Registry,
    hold: &std::sync::Barrier,
    lookup: impl FnOnce() -> T + Send + 'static,
) -> T {
    let attempted = registry.observe_next_lock_attempt();
    let reader = std::thread::spawn(lookup);
    if attempted.recv().unwrap() {
        let result = reader.join().unwrap();
        hold.wait();
        result
    } else {
        hold.wait();
        reader.join().unwrap()
    }
}

#[test]
fn catalog_read_after_hello_ack_waits_for_publication() {
    let registry = Arc::new(Registry::default());
    let hold = Arc::new(std::sync::Barrier::new(2));
    let mut handler = ControlHandler::with_forwarding(
        Arc::clone(&registry),
        Arc::new(ForwardingTable::default()),
    );
    handler.hello_ack_to_publication_hold = Some(Arc::clone(&hold));
    let (module_ctx, mut module_rx) = route_ctx(ConnectionId::new(40));
    let hello = {
        let handler = handler.clone();
        std::thread::spawn(move || {
            handler
                .handle_hello(
                    module_ctx.connection_id,
                    Some(module_ctx.egress),
                    hello_frame("aft", PROTOCOL_VERSION, 7),
                )
                .unwrap()
        })
    };
    hold.wait();
    assert_eq!(
        module_rx.try_recv().unwrap().frame.header.ty,
        FrameType::HelloAck
    );
    let replies = lookup_during_hello_publication(&registry, &hold, move || {
        // A different connection can issue catalog.list as soon as the
        // module has read its ACK, even while HELLO has not returned.
        let request = Frame::build(
            FrameType::Request,
            control_flags(),
            0,
            0,
            300,
            serde_json::to_vec(&ClientControlRequest::CatalogList { module_id: None }).unwrap(),
        )
        .unwrap();
        handler.handle_catalog_list(request, None).unwrap()
    });
    assert!(hello.join().unwrap().is_empty());
    let ClientControlResponse::CatalogList {
        generation,
        modules,
        ..
    } = serde_json::from_slice(&replies[0].body).unwrap()
    else {
        panic!("not a catalog.list reply");
    };
    assert_eq!(
        modules
            .iter()
            .map(|entry| entry.module_id.as_str())
            .collect::<Vec<_>>(),
        vec!["aft"]
    );
    assert_eq!(generation, 1);
}

/// While a HELLO has reserved its registry entry but its forwarding
/// endpoint is not installed yet, `route.open` to the module can only be
/// refused. `catalog.list` must not list the module in that window: a
/// client that opens a route as soon as it sees a module listed and ready
/// would be refused for a module the daemon had just advertised. The
/// refusal in the window stays the retryable `target_unavailable`, never
/// `unknown_module`, and the id stays claimed against a duplicate HELLO.
/// Once the HELLO completes, the module is listed and a route to it opens.
#[tokio::test]
async fn module_is_listed_only_once_route_open_to_it_can_succeed() {
    let (handler, hold, hello, module_ctx, mut module_rx) = hello_held_before_forwarding();
    let client = ConnectionId::new(41);
    let (client_ctx, mut client_rx) = route_ctx(client);

    hold.wait();
    // Parked: the registry holds "aft", forwarding has no endpoint for it.
    assert!(handler.registry.get_module("aft").unwrap().is_some());
    assert!(!handler
        .forwarding
        .has_live_module_connection("aft")
        .unwrap());
    let (_, listed) = catalog_snapshot(&handler, &client_ctx, 300).await;
    assert!(
        listed.is_empty(),
        "catalog.list listed a module route.open cannot reach yet: {listed:?}"
    );
    let refused = handler
        .handle_control_frame(
            &client_ctx,
            route_open_frame(301, "aft", unique_project_root("listed-window")),
        )
        .await
        .unwrap();
    assert_eq!(parse_error(&refused[0])["code"], "target_unavailable");
    let (duplicate_ctx, _duplicate_rx) = route_ctx(ConnectionId::new(42));
    let duplicate = handler
        .handle_control_frame(&duplicate_ctx, hello_frame("aft", PROTOCOL_VERSION, 8))
        .await
        .unwrap();
    assert_eq!(parse_error(&duplicate[0])["code"], "duplicate_module_id");

    hold.wait();
    assert!(hello.join().unwrap().is_empty(), "HELLO succeeds");
    let ack = module_rx.try_recv().unwrap().frame;
    assert_eq!(ack.header.ty, FrameType::HelloAck);
    let (_, listed) = catalog_snapshot(&handler, &client_ctx, 302).await;
    assert_eq!(listed, vec![("aft".to_string(), true)]);

    let (open_task, bind) = relay_route_open(
        &handler,
        client,
        &client_ctx.egress,
        &mut module_rx,
        303,
        "aft",
        "listed-after",
    )
    .await;
    assert!(handler
        .handle_control_frame(&module_ctx, route_bind_ack(bind.header.corr))
        .await
        .unwrap()
        .is_empty());
    assert!(open_task.await.unwrap().is_empty(), "route.open accepted");
    published_route(&client_rx.try_recv().unwrap().frame);
}

/// A catalog watcher keys on the generation: when it changes, it re-reads
/// the listing. So the generation must advance exactly when the module
/// appears. If it advanced while the module was still hidden, a watcher
/// would read the new generation without the module, and the module would
/// later appear under that same generation with nothing telling the
/// watcher to look again.
#[tokio::test]
async fn catalog_generation_advances_when_the_module_appears_and_not_before() {
    let (handler, hold, hello, _module_ctx, _module_rx) = hello_held_before_forwarding();
    let (client_ctx, _client_rx) = route_ctx(ConnectionId::new(41));
    let (before, _) = catalog_snapshot(&handler, &client_ctx, 300).await;

    hold.wait();
    let (during, listed) = catalog_snapshot(&handler, &client_ctx, 301).await;
    assert!(listed.is_empty());
    assert_eq!(
        during, before,
        "the generation moved while the module was not listed"
    );

    hold.wait();
    assert!(hello.join().unwrap().is_empty(), "HELLO succeeds");
    let (after, listed) = catalog_snapshot(&handler, &client_ctx, 302).await;
    assert_eq!(listed, vec![("aft".to_string(), true)]);
    assert_eq!(
        after,
        before + 1,
        "the module appeared without exactly one generation bump"
    );
}

/// A HELLO whose forwarding endpoint cannot be installed (here the
/// module's sink is already closed, so its HELLO_ACK cannot be queued) is
/// refused and leaves nothing behind: no registry entry, no catalog entry,
/// and the catalog generation where it was.
#[tokio::test]
async fn failed_forwarding_registration_leaves_no_registry_or_catalog_trace() {
    let registry = Arc::new(Registry::default());
    let handler = ControlHandler::with_forwarding(
        Arc::clone(&registry),
        Arc::new(ForwardingTable::default()),
    );
    let (client_ctx, _client_rx) = route_ctx(ConnectionId::new(41));
    let (before, _) = catalog_snapshot(&handler, &client_ctx, 300).await;
    let (module_ctx, module_rx) = route_ctx(ConnectionId::new(40));
    drop(module_rx);

    let replies = handler
        .handle_control_frame(&module_ctx, hello_frame("aft", PROTOCOL_VERSION, 7))
        .await
        .unwrap();
    assert_eq!(replies[0].header.ty, FrameType::Error, "{replies:?}");
    assert!(registry.get_module("aft").unwrap().is_none());
    assert!(registry
        .get_module_by_connection(module_ctx.connection_id)
        .unwrap()
        .is_none());
    assert_eq!(
        catalog_snapshot(&handler, &client_ctx, 301).await,
        (before, Vec::new())
    );
}

/// A handler whose `route.open` parks after queueing the route.bind and
/// before marking it enqueued, with module "aft" registered on connection
/// 40. Returns the two-party barrier that parks and releases it.
async fn route_open_held_after_bind_send() -> (
    ControlHandler,
    Arc<tokio::sync::Barrier>,
    RouteCtx,
    mpsc::Receiver<crate::router::OutboundFrame>,
) {
    let hold = Arc::new(tokio::sync::Barrier::new(2));
    let handler = ControlHandler::with_forwarding(
        Arc::new(Registry::default()),
        Arc::new(ForwardingTable::default()),
    )
    .with_route_bind_relay_sent_hold(Arc::clone(&hold));
    let (module_ctx, mut module_rx) = route_ctx(ConnectionId::new(40));
    hello_via_sink(
        &handler,
        &module_ctx,
        &mut module_rx,
        hello_frame("aft", PROTOCOL_VERSION, 7),
    )
    .await;
    (handler, hold, module_ctx, module_rx)
}

/// A fast module acks the route.bind, and the reader commits the route,
/// before `route.open` has marked the relay enqueued. The daemon used to
/// read the missing relay as an abandoned bind and send the module a
/// GOODBYE for the route it had just committed: the client was told its
/// route was open, the module unbound the channel, and the client's first
/// request was dropped on the module side as unbound.
#[tokio::test]
async fn bind_acked_before_the_enqueue_mark_commits_without_a_module_goodbye() {
    let (handler, hold, module_ctx, mut module_rx) = route_open_held_after_bind_send().await;
    let client = ConnectionId::new(41);
    let (client_ctx, mut client_rx) = route_ctx(client);

    let (open_task, bind) = relay_route_open(
        &handler,
        client,
        &client_ctx.egress,
        &mut module_rx,
        200,
        "aft",
        "acked-before-mark",
    )
    .await;
    let (module_channel, module_epoch) = route_bind_channel(&bind);
    // route.open is now parked between queueing the bind and marking it.
    hold.wait().await;

    // The module answers inside that window, and the route commits.
    assert!(handler
        .handle_control_frame(&module_ctx, route_bind_ack(bind.header.corr))
        .await
        .unwrap()
        .is_empty());
    let (client_channel, client_epoch) = published_route(&client_rx.try_recv().unwrap().frame);

    hold.wait().await;
    assert!(
        open_task.await.unwrap().is_empty(),
        "route.open answers accepted"
    );

    // Every frame route.open could have sent is already queued, because it
    // has returned; the module must have been sent nothing.
    match module_rx.try_recv() {
        Err(mpsc::error::TryRecvError::Empty) => {}
        Ok(outbound) => panic!(
            "module was sent {:?} on channel {} epoch {} for a route the daemon committed",
            outbound.frame.header.ty, outbound.frame.header.channel, outbound.frame.header.epoch
        ),
        Err(err) => panic!("module egress closed: {err}"),
    }

    // The client's first request on the route is the next thing the module
    // reads, on the binding it just created.
    let router = Router::with_control_handler(Arc::new(handler.clone()));
    let request = Frame::build(
        FrameType::Request,
        Flags::new(true, Priority::Interactive, false),
        client_channel,
        client_epoch,
        201,
        b"first request".to_vec(),
    )
    .unwrap();
    router
        .route_for_connection(&client_ctx, request)
        .await
        .unwrap();
    let delivered = module_rx.try_recv().unwrap().frame;
    assert_eq!(delivered.header.ty, FrameType::Request);
    assert_eq!(
        (delivered.header.channel, delivered.header.epoch),
        (module_channel, module_epoch)
    );
    assert_eq!(delivered.body, b"first request");
}

/// The client goes away while `route.open` is still queueing the bind, so
/// the relay is dropped with no module reply. The teardown cannot send the
/// GOODBYE itself (it could reach the module ahead of the bind), so
/// `route.open` must send it once the bind is queued; otherwise the module
/// keeps a binding the daemon will never commit.
#[tokio::test]
async fn bind_released_before_the_enqueue_mark_still_sends_the_module_a_goodbye() {
    let (handler, hold, _module_ctx, mut module_rx) = route_open_held_after_bind_send().await;
    let client = ConnectionId::new(42);
    let (client_ctx, _client_rx) = route_ctx(client);

    let (open_task, bind) = relay_route_open(
        &handler,
        client,
        &client_ctx.egress,
        &mut module_rx,
        300,
        "aft",
        "released-before-mark",
    )
    .await;
    let (module_channel, module_epoch) = route_bind_channel(&bind);
    hold.wait().await;

    handler.cleanup_connection(client).unwrap();
    assert!(
        module_rx.try_recv().is_err(),
        "client teardown must not GOODBYE a bind route.open is still queueing"
    );

    hold.wait().await;
    let response = open_task.await.unwrap();
    assert_eq!(response.len(), 1);
    assert_eq!(parse_error(&response[0])["code"], "target_unavailable");

    let goodbye = module_rx
        .try_recv()
        .expect("module is told to drop the binding the daemon abandoned")
        .frame;
    assert_eq!(goodbye.header.ty, FrameType::Goodbye);
    assert_eq!(
        (goodbye.header.channel, goodbye.header.epoch),
        (module_channel, module_epoch)
    );
    assert!(module_rx.try_recv().is_err(), "exactly one GOODBYE");
}

/// The module acks inside the same window, but the client's egress closed
/// first, so the reader refuses to commit. The ack proves the module holds
/// the binding, so the reader sends the GOODBYE, and `route.open` must not
/// send a second one.
#[tokio::test]
async fn bind_acked_for_a_gone_client_before_the_enqueue_mark_sends_exactly_one_goodbye() {
    let (handler, hold, module_ctx, mut module_rx) = route_open_held_after_bind_send().await;
    let client = ConnectionId::new(43);
    let (client_ctx, client_rx) = route_ctx(client);

    let (open_task, bind) = relay_route_open(
        &handler,
        client,
        &client_ctx.egress,
        &mut module_rx,
        400,
        "aft",
        "gone-client-before-mark",
    )
    .await;
    let (module_channel, module_epoch) = route_bind_channel(&bind);
    hold.wait().await;

    drop(client_rx);
    assert!(client_ctx.egress.is_closed());
    handler
        .handle_control_frame(&module_ctx, route_bind_ack(bind.header.corr))
        .await
        .unwrap();

    hold.wait().await;
    let response = open_task.await.unwrap();
    assert_eq!(response.len(), 1);
    assert_eq!(parse_error(&response[0])["code"], "target_unavailable");

    let goodbye = module_rx
        .try_recv()
        .expect("module is told to drop the binding it acked")
        .frame;
    assert_eq!(goodbye.header.ty, FrameType::Goodbye);
    assert_eq!(
        (goodbye.header.channel, goodbye.header.epoch),
        (module_channel, module_epoch)
    );
    assert!(module_rx.try_recv().is_err(), "exactly one GOODBYE");
    assert_eq!(handler.forwarding.active_binding_count().unwrap(), 0);
}

/// The fence at the module-loop boundary, stated as its own contract: which
/// forwarding failures are allowed to end the module connection that is being
/// served. A `ConnectionClosing` naming some client is about that client, and
/// a module connection is shared; the same error naming the module's own
/// connection is about this connection and must stay fatal, as must failures
/// that are about the forwarding table itself.
#[test]
fn only_the_modules_own_closing_connection_ends_the_module_loop() {
    let handler = ControlHandler::default();
    let module_connection = ConnectionId::new(30);
    let client_connection = ConnectionId::new(31);

    handler
        .refuse_to_end_module_connection_for_a_client(
            module_connection,
            77,
            ForwardingError::ConnectionClosing {
                connection_id: client_connection,
            },
        )
        .expect("a closing client must never end the module connection");

    assert!(matches!(
        handler.refuse_to_end_module_connection_for_a_client(
            module_connection,
            78,
            ForwardingError::ConnectionClosing {
                connection_id: module_connection,
            },
        ),
        Err(RouterError::Forwarding(ForwardingError::ConnectionClosing {
            connection_id
        })) if connection_id == module_connection
    ));
    assert!(matches!(
        handler.refuse_to_end_module_connection_for_a_client(
            module_connection,
            79,
            ForwardingError::Poisoned,
        ),
        Err(RouterError::Forwarding(ForwardingError::Poisoned))
    ));
    assert!(matches!(
        handler.refuse_to_end_module_connection_for_a_client(
            module_connection,
            80,
            ForwardingError::StaleModuleEndpoint,
        ),
        Err(RouterError::Forwarding(
            ForwardingError::StaleModuleEndpoint
        ))
    ));
}

/// The spawn-attestation guard is what stops a connected module from claiming
/// another module's identity and being stamped `Reserved` for it. Every other
/// test that supplies a consumer_identity supplies a CORRECT one, because a
/// correct one is what the rest of the flow needs -- so the guard's rejection
/// branch was never the subject of an assertion, only its acceptance branch.
///
/// Deleting the guard's EFFECT (granting Reserved unconditionally) leaves the
/// whole subc-core library suite green; only the forwarding integration tests
/// notice, and they notice for unrelated reasons. This test exists so the
/// refusal itself is asserted where the guard lives: it fails if the identity
/// check stops refusing, which is the direction that matters, since a guard
/// that wrongly ACCEPTS is silent while one that wrongly REJECTS is loud.
#[tokio::test]
async fn route_open_refuses_consumer_identity_that_fails_spawn_attestation() {
    let registry = Arc::new(Registry::default());
    let forwarding = Arc::new(ForwardingTable::default());
    let supervisor = SupervisorHandle::new();
    supervisor.set_spawn_nonce("fed", "fed-nonce".to_string());
    let handler = ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding))
        .with_supervisor(supervisor);

    let (target_ctx, mut target_rx) = route_ctx(ConnectionId::new(90));
    hello_via_sink(
        &handler,
        &target_ctx,
        &mut target_rx,
        hello_frame("target", PROTOCOL_VERSION, 1),
    )
    .await;

    // A real supervised module id presenting the wrong nonce. This is the
    // impersonation case: the attacker knows a privileged module_id, which is
    // public, and guesses at the nonce, which is not.
    let wrong_nonce = handler
        .handle_control_frame(
            &route_ctx(ConnectionId::new(91)).0,
            route_open_frame_with_admission_facts(
                20,
                "target",
                unique_project_root("admission-facts"),
                Some(subc_control::ConsumerIdentity {
                    module_id: "fed".to_string(),
                    launch_nonce: "not-the-real-nonce".to_string(),
                }),
                None,
            ),
        )
        .await
        .unwrap();
    assert_eq!(
        parse_error(&wrong_nonce[0])["code"],
        "bad_consumer_identity",
        "a mismatched launch nonce must be refused, not stamped Reserved"
    );

    // A module id the supervisor never spawned at all, so no nonce exists to
    // compare against. An implementation that treats "no record" as "nothing
    // to check" fails open here while passing the case above.
    let never_spawned = handler
        .handle_control_frame(
            &route_ctx(ConnectionId::new(92)).0,
            route_open_frame_with_admission_facts(
                21,
                "target",
                unique_project_root("admission-facts"),
                Some(subc_control::ConsumerIdentity {
                    module_id: "never-spawned".to_string(),
                    launch_nonce: "any-nonce".to_string(),
                }),
                None,
            ),
        )
        .await
        .unwrap();
    assert_eq!(
        parse_error(&never_spawned[0])["code"],
        "bad_consumer_identity",
        "an unspawned module_id must be refused rather than accepted for lack of a record"
    );
}

/// The refusal test above proves the guard says NO. Nothing proved it can say
/// YES, and the difference is not academic: replacing the whole authorization
/// with `false` -- admitting no consumer identity at all, revoking Reserved
/// standing for every supervised module in the fleet -- leaves 110 of the 111
/// library tests GREEN. The one that notices does so by HANGING, because it
/// waits for a bind that can no longer happen.
///
/// A hang is the weakest signal a suite can produce. In CI it reads as a slow
/// or flaky test, invites a RETRY rather than an investigation, and the retry
/// hangs too and gets blamed on the runner. So a total revocation of the
/// daemon's trust grant would have shipped behind a symptom nobody attributes
/// to code.
///
/// The bias is structural rather than accidental. A REFUSAL looks like a
/// failure someone writes a test for; a GRANT looks like the happy path. Every
/// binary-outcome guard whose STRICTNESS is the point acquires a refusal-heavy
/// suite for that reason, and this one is the purest case in the daemon.
///
/// This test asserts the EFFECT rather than the absence of an error: the module
/// receives a RouteBind and it carries `Reserved` naming the attested module.
/// A guard that admitted nobody would produce no bind at all; one that admitted
/// everybody would stamp the wrong principal, which the refusal test catches.
#[tokio::test]
async fn route_open_stamps_reserved_for_a_correctly_attested_consumer() {
    let registry = Arc::new(Registry::default());
    let forwarding = Arc::new(ForwardingTable::default());
    let supervisor = SupervisorHandle::new();
    supervisor.set_spawn_nonce("fed", "fed-nonce".to_string());
    let handler = ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding))
        .with_supervisor(supervisor);

    let (target_ctx, mut target_rx) = route_ctx(ConnectionId::new(95));
    hello_via_sink(
        &handler,
        &target_ctx,
        &mut target_rx,
        hello_frame("target", PROTOCOL_VERSION, 1),
    )
    .await;

    let (client_ctx, mut client_rx) = route_ctx(ConnectionId::new(96));
    let route_handler = handler.clone();
    let route_task = tokio::spawn(async move {
        route_handler
            .handle_control_frame(
                &client_ctx,
                route_open_frame_with_admission_facts(
                    30,
                    "target",
                    unique_project_root("admission-facts"),
                    Some(subc_control::ConsumerIdentity {
                        module_id: "fed".to_string(),
                        launch_nonce: "fed-nonce".to_string(),
                    }),
                    None,
                ),
            )
            .await
            .unwrap()
    });

    // BOUND THE WAIT. The first version of this test recv'd unbounded, and under
    // the very mutation it exists to catch -- a guard that admits nobody -- no
    // bind is ever sent, so it HUNG rather than failing. That reproduces the
    // exact defect being fixed: a total revocation detected only as a stalled
    // suite, which reads as flakiness and invites a retry. An acceptance test
    // that waits for an effect must bound the wait, or a red becomes a hang.
    let bind_frame = tokio::time::timeout(Duration::from_secs(5), target_rx.recv())
            .await
            .expect("no route.bind within 5s: the consumer-identity guard refused a correctly attested consumer")
            .expect("module control channel closed before route.bind");
    let bind: ModuleControlRequest = serde_json::from_slice(&bind_frame.body).unwrap();
    let ModuleControlRequest::RouteBind { principal, .. } = bind else {
        panic!("expected route.bind")
    };
    assert_eq!(
        principal,
        Some(Principal::Reserved {
            module_id: "fed".to_string()
        }),
        "a correctly attested consumer must be stamped Reserved for its own id"
    );

    handler
        .handle_control_frame(&target_ctx, route_bind_ack(bind_frame.header.corr))
        .await
        .unwrap();
    assert!(route_task.await.unwrap().is_empty());
    assert!(
        matches!(
            serde_json::from_slice::<ClientControlResponse>(&client_rx.recv().await.unwrap().body)
                .unwrap(),
            ClientControlResponse::RouteOpen { .. }
        ),
        "the route must actually open, not merely avoid an error"
    );
}

#[tokio::test(start_paused = true)]
async fn supervisor_routes_serializes_live_draining_bindings_from_the_real_handler() {
    let registry = Arc::new(Registry::default());
    let forwarding = Arc::new(ForwardingTable::default());
    let supervisor = SupervisorHandle::new();
    supervisor.set_spawn_nonce("fed", "fed-nonce".to_string());
    let handler = ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding))
        .with_supervisor(supervisor);

    let (target_ctx, mut target_rx) = route_ctx(ConnectionId::new(101));
    hello_via_sink(
        &handler,
        &target_ctx,
        &mut target_rx,
        hello_frame("target", PROTOCOL_VERSION, 1),
    )
    .await;

    let (direct_ctx, mut direct_rx) = route_ctx(ConnectionId::new(102));
    let direct_handler = handler.clone();
    let direct_open = tokio::spawn(async move {
        direct_handler
            .handle_control_frame(
                &direct_ctx,
                route_open_frame(2, "target", unique_project_root("route-census-direct")),
            )
            .await
            .unwrap()
    });
    let direct_bind = tokio::time::timeout(Duration::from_secs(5), target_rx.recv())
        .await
        .expect("no direct route.bind within 5s")
        .expect("target control channel closed before direct route.bind");
    handler
        .handle_control_frame(&target_ctx, route_bind_ack(direct_bind.header.corr))
        .await
        .unwrap();
    assert!(direct_open.await.unwrap().is_empty());
    let _ = direct_rx.recv().await.unwrap();

    let (reserved_ctx, mut reserved_rx) = route_ctx(ConnectionId::new(103));
    let reserved_handler = handler.clone();
    let reserved_open = tokio::spawn(async move {
        reserved_handler
            .handle_control_frame(
                &reserved_ctx,
                route_open_frame_with_admission_facts(
                    3,
                    "target",
                    unique_project_root("admission-facts"),
                    Some(ConsumerIdentity {
                        module_id: "fed".to_string(),
                        launch_nonce: "fed-nonce".to_string(),
                    }),
                    None,
                ),
            )
            .await
            .unwrap()
    });
    let reserved_bind = tokio::time::timeout(Duration::from_secs(5), target_rx.recv())
        .await
        .expect("no reserved route.bind within 5s")
        .expect("target control channel closed before reserved route.bind");
    handler
        .handle_control_frame(&target_ctx, route_bind_ack(reserved_bind.header.corr))
        .await
        .unwrap();
    assert!(reserved_open.await.unwrap().is_empty());
    let _ = reserved_rx.recv().await.unwrap();

    forwarding
        .begin_module_drain("target", subc_control::RouteCloseReason::Reload)
        .unwrap();
    let (census_ctx, _census_rx) = route_ctx(ConnectionId::new(104));
    let census_body = serde_json::to_vec(&ClientControlRequest::SupervisorRoutes {
        module_id: Some("target".to_string()),
    })
    .unwrap();
    let census_frame =
        Frame::build(FrameType::Request, control_flags(), 0, 0, 4, census_body).unwrap();
    let response = handler
        .handle_control_frame(&census_ctx, census_frame)
        .await
        .unwrap()
        .pop()
        .unwrap();
    let actual: Value = serde_json::from_slice(&response.body).unwrap();
    let decoded: ClientControlResponse = serde_json::from_value(actual.clone()).unwrap();
    assert!(matches!(
        decoded,
        ClientControlResponse::SupervisorRoutes { .. }
    ));
    let routes = actual["modules"][0]["routes"].as_array().unwrap();
    assert_eq!(routes.len(), 2);
    assert!(routes.iter().all(|route| route["draining"] == true));
    // The census carries WHY: the reason the drain was begun with, in the
    // route.closing vocabulary, on every draining route this drain marked.
    assert!(
        routes.iter().all(|route| route["drain_reason"] == "reload"),
        "draining routes must name the drain's reason: {routes:?}"
    );
    assert!(routes.iter().any(|route| {
        route["consumer"] == serde_json::json!({"kind": "direct", "connection_id": 102})
    }));
    assert!(routes.iter().any(|route| {
        route["consumer"] == serde_json::json!({"kind": "reserved", "module_id": "fed"})
    }));

    let golden_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../subc-control/tests/golden/client_control_response_supervisor_routes.json");
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::write(
            &golden_path,
            format!("{}\n", serde_json::to_string_pretty(&actual).unwrap()),
        )
        .unwrap();
    }
    let expected: Value =
        serde_json::from_str(&std::fs::read_to_string(golden_path).unwrap()).unwrap();
    assert_eq!(actual, expected);
}

async fn query_live_roots(
    handler: &ControlHandler,
    module_ctx: &RouteCtx,
) -> ModuleControlResponseToModule {
    let body = serde_json::to_vec(&ModuleControlRequestFromModule::LiveRoots {}).unwrap();
    let frame = Frame::build(FrameType::Request, control_flags(), 0, 0, 900, body).unwrap();
    let response = handler
        .handle_control_frame(module_ctx, frame)
        .await
        .unwrap()
        .pop()
        .unwrap();
    serde_json::from_slice(&response.body).unwrap()
}

#[tokio::test(start_paused = true)]
async fn supervisor_live_roots_root_known_arm_counts_bound_and_pending_from_real_handler() {
    let registry = Arc::new(Registry::default());
    let forwarding = Arc::new(ForwardingTable::default());
    let handler = ControlHandler::with_forwarding(registry, forwarding);
    let (target_ctx, mut target_rx) = route_ctx(ConnectionId::new(301));
    hello_via_sink(
        &handler,
        &target_ctx,
        &mut target_rx,
        hello_frame("target", PROTOCOL_VERSION, 1),
    )
    .await;
    let root = unique_project_root("live-roots-known");
    let path = ProjectRootId::from_path_allowing_missing(root.path())
        .unwrap()
        .as_path()
        .to_path_buf();
    let (client_ctx, mut client_rx) = route_ctx(ConnectionId::new(302));
    let open_handler = handler.clone();
    let opened = tokio::spawn(async move {
        open_handler
            .handle_control_frame(&client_ctx, legacy_route_open_frame(2, "target", root))
            .await
            .unwrap()
    });
    let bind = tokio::time::timeout(Duration::from_secs(5), target_rx.recv())
        .await
        .unwrap()
        .unwrap();
    handler
        .handle_control_frame(&target_ctx, route_bind_ack(bind.header.corr))
        .await
        .unwrap();
    assert!(opened.await.unwrap().is_empty());
    let _ = client_rx.recv().await.unwrap();

    let root = unique_project_root("live-roots-pending");
    let pending_path = ProjectRootId::from_path_allowing_missing(root.path())
        .unwrap()
        .as_path()
        .to_path_buf();
    let (client_ctx, _client_rx) = route_ctx(ConnectionId::new(303));
    let open_handler = handler.clone();
    let pending = tokio::spawn(async move {
        open_handler
            .handle_control_frame(&client_ctx, legacy_route_open_frame(3, "target", root))
            .await
            .unwrap()
    });
    let pending_bind = tokio::time::timeout(Duration::from_secs(5), target_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let actual = query_live_roots(&handler, &target_ctx).await;
    let ModuleControlResponseToModule::LiveRoots {
        roots,
        unknown_root_bindings,
        total_bindings,
    } = actual
    else {
        panic!("expected live roots")
    };
    assert_eq!(total_bindings, 2, "root-known arm must count live routes");
    assert_eq!(unknown_root_bindings, 0);
    assert_eq!(
        roots.len(),
        2,
        "root-known arm must retain each canonical root"
    );
    assert_eq!(
        total_bindings,
        roots.iter().map(|r| r.bound + r.pending).sum::<u64>() + unknown_root_bindings
    );
    let counts = roots
        .iter()
        .map(|root| (root.project_root.clone(), root.bound, root.pending))
        .collect::<Vec<_>>();
    let mut expected = vec![(path, 1, 0), (pending_path, 0, 1)];
    expected.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        counts, expected,
        "roots must sort by path and count pending separately"
    );
    handler
        .handle_control_frame(&target_ctx, route_bind_ack(pending_bind.header.corr))
        .await
        .unwrap();
    assert!(pending.await.unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn supervisor_live_roots_unknown_root_arm_is_not_no_bindings() {
    let forwarding = Arc::new(ForwardingTable::default());
    let handler =
        ControlHandler::with_forwarding(Arc::new(Registry::default()), Arc::clone(&forwarding));
    let (target_ctx, mut target_rx) = route_ctx(ConnectionId::new(311));
    hello_via_sink(
        &handler,
        &target_ctx,
        &mut target_rx,
        hello_frame("target", PROTOCOL_VERSION, 1),
    )
    .await;
    let (client_ctx, _client_rx) = route_ctx(ConnectionId::new(312));
    let pending = forwarding
        .begin_route_bind_relay_for_test(
            client_ctx.connection_id,
            client_ctx.egress.clone(),
            2,
            "target",
        )
        .unwrap();
    forwarding
        .complete_pending_relay(
            target_ctx.connection_id,
            pending.corr,
            RouteBindRelayOutcome::Accepted,
        )
        .unwrap();
    let actual = query_live_roots(&handler, &target_ctx).await;
    let ModuleControlResponseToModule::LiveRoots {
        roots,
        unknown_root_bindings,
        total_bindings,
    } = actual
    else {
        panic!("expected live roots")
    };
    assert!(roots.is_empty(), "unknown-root arm must not invent a root");
    assert_eq!(
        unknown_root_bindings, 1,
        "unknown-root arm must not read as no bindings"
    );
    assert_eq!(total_bindings, 1, "unknown-root arm has a live binding");
    assert_eq!(
        total_bindings,
        roots.iter().map(|r| r.bound + r.pending).sum::<u64>() + unknown_root_bindings
    );
}

/// A module reads HELLO_ACK as its first frame and exits on anything else,
/// so the ack has to be on its outbound queue before the module is
/// routable. The connection loop writes a handler's replies only after the
/// handler returns; this test stops in exactly that gap, runs a real
/// route.open from another connection, and only then writes whatever the
/// HELLO handler returned, the way the loop would. If the ack were still a
/// reply, the route.bind request would reach the module first.
#[tokio::test(start_paused = true)]
async fn hello_ack_reaches_the_module_before_a_route_bind_raced_into_the_reply_gap() {
    let forwarding = Arc::new(ForwardingTable::default());
    let handler =
        ControlHandler::with_forwarding(Arc::new(Registry::default()), Arc::clone(&forwarding));
    let (module_ctx, mut module_rx) = route_ctx(ConnectionId::new(341));
    let replies = handler
        .handle_control_frame(&module_ctx, hello_frame("raced", PROTOCOL_VERSION, 7))
        .await
        .unwrap();
    let queued_by_hello = module_rx.len();

    let (client_ctx, mut client_rx) = route_ctx(ConnectionId::new(342));
    let open_handler = handler.clone();
    let open = tokio::spawn(async move {
        open_handler
            .handle_control_frame(
                &client_ctx,
                route_open_frame(2, "raced", unique_project_root("hello-ack-race")),
            )
            .await
            .unwrap()
    });
    // Let the route.open run until its route.bind is on the module's queue.
    let mut spins = 0;
    while module_rx.len() == queued_by_hello {
        spins += 1;
        assert!(spins < 10_000, "route.open never queued a route.bind");
        tokio::task::yield_now().await;
    }

    // Now the connection loop's half: write the HELLO handler's replies.
    for reply in replies {
        module_ctx.egress.send(reply).await.unwrap();
    }

    let first = module_rx.recv().await.unwrap().frame;
    assert_eq!(
        first.header.ty,
        FrameType::HelloAck,
        "the first frame a registering module reads must be its HELLO_ACK"
    );
    assert_eq!(first.header.corr, 7);
    let second = module_rx.recv().await.unwrap().frame;
    assert_eq!(second.header.ty, FrameType::Request);
    assert!(
        matches!(
            serde_json::from_slice::<ModuleControlRequest>(&second.body).unwrap(),
            ModuleControlRequest::RouteBind { .. }
        ),
        "the route.bind follows the ack"
    );
    assert!(module_rx.try_recv().is_err(), "nothing else was queued");

    handler
        .handle_control_frame(&module_ctx, route_bind_ack(second.header.corr))
        .await
        .unwrap();
    assert!(open.await.unwrap().is_empty());
    let _ = client_rx.recv().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn supervisor_live_roots_cross_module_scope_uses_requesting_connection() {
    let handler = ControlHandler::with_forwarding(
        Arc::new(Registry::default()),
        Arc::new(ForwardingTable::default()),
    );
    let (first_ctx, mut first_rx) = route_ctx(ConnectionId::new(315));
    let (second_ctx, mut second_rx) = route_ctx(ConnectionId::new(316));
    hello_via_sink(
        &handler,
        &first_ctx,
        &mut first_rx,
        hello_frame("first", PROTOCOL_VERSION, 1),
    )
    .await;
    hello_via_sink(
        &handler,
        &second_ctx,
        &mut second_rx,
        hello_frame("second", PROTOCOL_VERSION, 2),
    )
    .await;
    let root = unique_project_root("second-only");
    let (client_ctx, _client_rx) = route_ctx(ConnectionId::new(317));
    let cloned = handler.clone();
    let open = tokio::spawn(async move {
        cloned
            .handle_control_frame(&client_ctx, route_open_frame(3, "second", root))
            .await
            .unwrap()
    });
    let bind = tokio::time::timeout(Duration::from_secs(5), second_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let first = query_live_roots(&handler, &first_ctx).await;
    let second = query_live_roots(&handler, &second_ctx).await;
    assert!(
        matches!(
            first,
            ModuleControlResponseToModule::LiveRoots {
                total_bindings: 0,
                ..
            }
        ),
        "cross-module scope must not expose another module's roots"
    );
    assert!(
        matches!(
            second,
            ModuleControlResponseToModule::LiveRoots {
                total_bindings: 1,
                ..
            }
        ),
        "second module must see its pending route"
    );
    handler
        .handle_control_frame(&second_ctx, route_bind_ack(bind.header.corr))
        .await
        .unwrap();
    assert!(open.await.unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn supervisor_live_roots_no_bindings_arm_is_empty() {
    let handler = ControlHandler::with_forwarding(
        Arc::new(Registry::default()),
        Arc::new(ForwardingTable::default()),
    );
    let (target_ctx, mut target_rx) = route_ctx(ConnectionId::new(321));
    hello_via_sink(
        &handler,
        &target_ctx,
        &mut target_rx,
        hello_frame("target", PROTOCOL_VERSION, 1),
    )
    .await;
    let actual = query_live_roots(&handler, &target_ctx).await;
    let ModuleControlResponseToModule::LiveRoots {
        roots,
        unknown_root_bindings,
        total_bindings,
    } = actual
    else {
        panic!("expected live roots")
    };
    assert!(roots.is_empty());
    assert_eq!(unknown_root_bindings, 0);
    assert_eq!(total_bindings, 0);
    assert_eq!(
        total_bindings,
        roots.iter().map(|r| r.bound + r.pending).sum::<u64>() + unknown_root_bindings
    );
}

/// Read the vendored fed corpus rather than hand-building a package.
///
/// A hand-built object encodes what the test author believed the carrier
/// emits. These vectors are what it actually emits, and one of them exists
/// specifically to pin OUR side of the seam: its note reads "SUBC relay
/// ignores additive unknown fields at the traversal emit terminus."
fn fed_admission_facts_vectors() -> Vec<(String, Value)> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/fed/admission-facts-emit.jsonl");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("vendored fed corpus unreadable at {path:?}: {err}"));
    let vectors: Vec<(String, Value)> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let entry: Value = serde_json::from_str(line).expect("corpus line must be JSON");
            let id = entry["corpus_id"]
                .as_str()
                .expect("every vector carries a corpus_id")
                .to_string();
            (id, entry["package"].clone())
        })
        .collect();
    // Pin the count: a corpus that silently shrinks would take its coverage
    // with it, and a suite reading N-1 vectors reports the same clean pass
    // as one reading N.
    assert_eq!(
        vectors.len(),
        3,
        "vendored fed corpus changed size; re-sync from subc-federation"
    );

    // Pin what makes the corpus DISCRIMINATING, not just present.
    //
    // The relay test below takes its expected value from the corpus, so the
    // corpus supplies the test's power to detect a lossy relay rather than
    // its correctness. A relay that dropped unrecognised fields would still
    // be caught -- but only by a package carrying fields it does not know.
    // Shrink every package to the handful of keys any implementation would
    // recognise and the test keeps passing over an input that can no longer
    // fail, which is the same clean green as a corpus that shrank away.
    //
    // So assert the precondition rather than duplicating the packages here:
    // at least one vector must carry a field beyond the small common set.
    // That is one claim to maintain instead of nine, and it fails loudly if
    // a re-sync ever flattens the corpus.
    const COMMONLY_MODELLED: [&str; 3] = ["schema", "verified_class", "org"];
    let richest = vectors
        .iter()
        .filter_map(|(_, package)| package.as_object())
        .map(|object| {
            object
                .keys()
                .filter(|key| !COMMONLY_MODELLED.contains(&key.as_str()))
                .count()
        })
        .max()
        .unwrap_or(0);
    assert!(
        richest >= 2,
        "vendored corpus no longer carries a package with unmodelled fields, \
             so the relay test can no longer distinguish a verbatim relay from a lossy one"
    );

    vectors
}

/// The relay must carry the carrier's package through BYTE-FOR-BYTE.
///
/// The gate test below proves the ACCESS RULE (who may send facts, to whom).
/// This proves the PAYLOAD RULE, which the gate cannot: it hand-builds a
/// three-key object, so a relay that quietly dropped fields it did not
/// recognise would satisfy it. These vectors carry nine keys including ones
/// this crate has no type for, so a typed relay fails here and only here.
#[tokio::test]
async fn admission_facts_relay_carries_vendored_packages_verbatim() {
    for (corpus_id, package) in fed_admission_facts_vectors() {
        let registry = Arc::new(Registry::default());
        let forwarding = Arc::new(ForwardingTable::default());
        let supervisor = SupervisorHandle::new();
        supervisor.set_spawn_nonce("fed", "fed-nonce".to_string());
        let handler =
            ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding))
                .with_supervisor(supervisor)
                .with_admission_facts_config(
                    Some("fed".to_string()),
                    Some(vec!["target".to_string()]),
                );

        let (target_ctx, mut target_rx) = route_ctx(ConnectionId::new(90));
        hello_via_sink(
            &handler,
            &target_ctx,
            &mut target_rx,
            hello_frame("target", PROTOCOL_VERSION, 1),
        )
        .await;

        let (client_ctx, _client_rx) = route_ctx(ConnectionId::new(91));
        let route_handler = handler.clone();
        let expected = package.clone();
        let route_task = tokio::spawn(async move {
            route_handler
                .handle_control_frame(
                    &client_ctx,
                    route_open_frame_with_admission_facts(
                        20,
                        "target",
                        unique_project_root("admission-facts"),
                        Some(subc_control::ConsumerIdentity {
                            module_id: "fed".to_string(),
                            launch_nonce: "fed-nonce".to_string(),
                        }),
                        Some(package),
                    ),
                )
                .await
                .unwrap()
        });

        let bind_frame = target_rx.recv().await.unwrap();
        let bind: ModuleControlRequest = serde_json::from_slice(&bind_frame.body).unwrap();
        let ModuleControlRequest::RouteBind {
            admission_facts, ..
        } = bind
        else {
            panic!("{corpus_id}: expected route.bind")
        };
        assert_eq!(
            admission_facts,
            Some(expected),
            "{corpus_id}: relay must not add, drop or reshape any field"
        );

        handler
            .handle_control_frame(&target_ctx, route_bind_ack(bind_frame.header.corr))
            .await
            .unwrap();
        route_task.await.unwrap();
    }
}

#[tokio::test]
async fn route_bind_never_relays_the_openers_build_class() {
    let registry = Arc::new(Registry::default());
    let forwarding = Arc::new(ForwardingTable::default());
    let handler = ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding));
    let (target_ctx, mut target_rx) = route_ctx(ConnectionId::new(80));
    hello_via_sink(
        &handler,
        &target_ctx,
        &mut target_rx,
        hello_frame("target", PROTOCOL_VERSION, 1),
    )
    .await;

    let mut identity = BindIdentity::new("unit".to_string(), "s".to_string());
    identity.build_class = Some(subc_protocol::BUILD_CLASS_PRODUCTION.to_string());
    let body = serde_json::to_vec(&ClientControlRequest::RouteOpen {
        busy_refusals: None,
        target: RouteTarget::ToolProvider {
            module_id: "target".to_string(),
        },
        identity,
        consumer_identity: None,
        consumer_capabilities: None,
        role_versions: None,
        admission_facts: None,
        scope: None,
    })
    .unwrap();
    let open = Frame::build(FrameType::Request, control_flags(), 0, 0, 30, body).unwrap();

    let (client_ctx, _client_rx) = route_ctx(ConnectionId::new(81));
    let route_handler = handler.clone();
    let route_task = tokio::spawn(async move {
        route_handler
            .handle_control_frame(&client_ctx, open)
            .await
            .unwrap()
    });
    let bind_frame = target_rx.recv().await.unwrap();
    let bind: ModuleControlRequest = serde_json::from_slice(&bind_frame.body).unwrap();
    let ModuleControlRequest::RouteBind { identity, .. } = bind else {
        panic!("expected route.bind")
    };
    assert_eq!(
        identity.build_class, None,
        "route.bind carried the build class"
    );
    assert!(
        !String::from_utf8_lossy(&bind_frame.body).contains("build_class"),
        "the route.bind bytes still name build_class"
    );
    handler
        .handle_control_frame(&target_ctx, route_bind_ack(bind_frame.header.corr))
        .await
        .unwrap();
    route_task.await.unwrap();
}

#[tokio::test]
async fn admission_facts_gate_checks_carrier_target_and_precedence() {
    let registry = Arc::new(Registry::default());
    let forwarding = Arc::new(ForwardingTable::default());
    let supervisor = SupervisorHandle::new();
    supervisor.set_spawn_nonce("fed", "fed-nonce".to_string());
    supervisor.set_spawn_nonce("other", "other-nonce".to_string());
    let handler = ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding))
        .with_supervisor(supervisor)
        .with_admission_facts_config(Some("fed".to_string()), Some(vec!["target".to_string()]));

    let (target_ctx, mut target_rx) = route_ctx(ConnectionId::new(70));
    hello_via_sink(
        &handler,
        &target_ctx,
        &mut target_rx,
        hello_frame("target", PROTOCOL_VERSION, 1),
    )
    .await;
    let (other_ctx, mut other_rx) = route_ctx(ConnectionId::new(71));
    hello_via_sink(
        &handler,
        &other_ctx,
        &mut other_rx,
        hello_frame("other", PROTOCOL_VERSION, 2),
    )
    .await;

    let facts = json!({"schema": 1, "verified_class": "member", "org": "01H"});
    let expected_facts = facts.clone();
    let (client_ctx, mut client_rx) = route_ctx(ConnectionId::new(72));
    let route_handler = handler.clone();
    let route_task = tokio::spawn(async move {
        route_handler
            .handle_control_frame(
                &client_ctx,
                route_open_frame_with_admission_facts(
                    10,
                    "target",
                    unique_project_root("admission-facts"),
                    Some(subc_control::ConsumerIdentity {
                        module_id: "fed".to_string(),
                        launch_nonce: "fed-nonce".to_string(),
                    }),
                    Some(facts.clone()),
                ),
            )
            .await
            .unwrap()
    });
    let bind_frame = target_rx.recv().await.unwrap();
    let bind: ModuleControlRequest = serde_json::from_slice(&bind_frame.body).unwrap();
    let ModuleControlRequest::RouteBind {
        admission_facts, ..
    } = bind
    else {
        panic!("expected route.bind")
    };
    assert_eq!(admission_facts, Some(expected_facts));
    handler
        .handle_control_frame(&target_ctx, route_bind_ack(bind_frame.header.corr))
        .await
        .unwrap();
    assert!(route_task.await.unwrap().is_empty());
    assert!(matches!(
        serde_json::from_slice::<ClientControlResponse>(&client_rx.recv().await.unwrap().body)
            .unwrap(),
        ClientControlResponse::RouteOpen { .. }
    ));

    let direct = handler
        .handle_control_frame(
            &route_ctx(ConnectionId::new(73)).0,
            route_open_frame_with_admission_facts(
                11,
                "target",
                unique_project_root("admission-facts"),
                None,
                Some(json!({"x": 1})),
            ),
        )
        .await
        .unwrap();
    assert_eq!(
        parse_error(&direct[0])["code"],
        "admission_facts_not_permitted"
    );

    let different_reserved = handler
        .handle_control_frame(
            &route_ctx(ConnectionId::new(77)).0,
            route_open_frame_with_admission_facts(
                15,
                "target",
                unique_project_root("admission-facts"),
                Some(subc_control::ConsumerIdentity {
                    module_id: "other".to_string(),
                    launch_nonce: "other-nonce".to_string(),
                }),
                Some(json!({"x": 1})),
            ),
        )
        .await
        .unwrap();
    assert_eq!(
        parse_error(&different_reserved[0])["code"],
        "admission_facts_not_permitted"
    );

    let other_target = handler
        .handle_control_frame(
            &route_ctx(ConnectionId::new(74)).0,
            route_open_frame_with_admission_facts(
                12,
                "other",
                unique_project_root("admission-facts"),
                Some(subc_control::ConsumerIdentity {
                    module_id: "fed".to_string(),
                    launch_nonce: "fed-nonce".to_string(),
                }),
                Some(json!({"x": 1})),
            ),
        )
        .await
        .unwrap();
    assert_eq!(
        parse_error(&other_target[0])["code"],
        "admission_facts_target_not_allowed"
    );

    let nonexistent = handler
        .handle_control_frame(
            &route_ctx(ConnectionId::new(75)).0,
            route_open_frame_with_admission_facts(
                13,
                "missing",
                unique_project_root("admission-facts"),
                None,
                Some(json!({"x": 1})),
            ),
        )
        .await
        .unwrap();
    assert_eq!(parse_error(&nonexistent[0])["code"], "unknown_module");

    let described = handler
        .handle_control_frame(
            &route_ctx(ConnectionId::new(76)).0,
            Frame::build(
                FrameType::Request,
                control_flags(),
                0,
                0,
                14,
                serde_json::to_vec(&ClientControlRequest::ServerDescribe {}).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let ClientControlResponse::ServerDescribe { capabilities, .. } =
        serde_json::from_slice(&described[0].body).unwrap()
    else {
        panic!("expected server.describe response")
    };
    assert!(capabilities
        .iter()
        .any(|cap| cap == "admission_facts_relay_v1"));
}

#[tokio::test]
async fn admission_facts_without_configured_carrier_are_rejected() {
    let registry = Arc::new(Registry::default());
    let forwarding = Arc::new(ForwardingTable::default());
    let handler = ControlHandler::with_forwarding(registry, forwarding);
    let (target_ctx, mut target_rx) = route_ctx(ConnectionId::new(78));
    hello_via_sink(
        &handler,
        &target_ctx,
        &mut target_rx,
        hello_frame("target", PROTOCOL_VERSION, 1),
    )
    .await;

    let responses = handler
        .handle_control_frame(
            &route_ctx(ConnectionId::new(79)).0,
            route_open_frame_with_admission_facts(
                16,
                "target",
                unique_project_root("admission-facts"),
                None,
                Some(json!({"x": 1})),
            ),
        )
        .await
        .unwrap();
    assert_eq!(
        parse_error(&responses[0])["code"],
        "admission_facts_not_permitted"
    );
}

#[tokio::test]
async fn route_open_relays_consumer_capabilities_verbatim() {
    let registry = Arc::new(Registry::default());
    let forwarding = Arc::new(ForwardingTable::default());
    let handler = ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding));
    let (module_ctx, mut module_rx) = route_ctx(ConnectionId::new(37));
    hello_via_sink(
        &handler,
        &module_ctx,
        &mut module_rx,
        hello_frame("aft", PROTOCOL_VERSION, 7),
    )
    .await;

    let expected = vec!["elicitation".to_string(), "roots".to_string()];
    let expected_for_request = expected.clone();
    let project_root = unique_project_root("consumer-capabilities-present");
    let (client_ctx, mut client_rx) = route_ctx(ConnectionId::new(38));
    let route_handler = handler.clone();
    let route_task = tokio::spawn(async move {
        route_handler
            .handle_control_frame(
                &client_ctx,
                route_open_frame_with_consumer_capabilities(
                    401,
                    "aft",
                    project_root,
                    Some(expected_for_request),
                ),
            )
            .await
            .unwrap()
    });
    let bind_frame = tokio::time::timeout(Duration::from_secs(1), module_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let bind: ModuleControlRequest = serde_json::from_slice(&bind_frame.body).unwrap();
    let ModuleControlRequest::RouteBind {
        consumer_capabilities,
        ..
    } = bind
    else {
        panic!("expected route.bind request, got {bind:?}");
    };
    assert_eq!(consumer_capabilities, Some(expected.clone()));

    handler
        .handle_control_frame(&module_ctx, route_bind_ack(bind_frame.header.corr))
        .await
        .unwrap();
    let route_response = route_task.await.unwrap();
    assert!(route_response.is_empty());
    let published = client_rx.recv().await.unwrap();
    assert!(matches!(
        serde_json::from_slice::<ClientControlResponse>(&published.body).unwrap(),
        ClientControlResponse::RouteOpen { .. }
    ));
}

#[tokio::test]
async fn route_open_without_consumer_capabilities_relays_none() {
    let registry = Arc::new(Registry::default());
    let forwarding = Arc::new(ForwardingTable::default());
    let handler = ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding));
    let (module_ctx, mut module_rx) = route_ctx(ConnectionId::new(39));
    hello_via_sink(
        &handler,
        &module_ctx,
        &mut module_rx,
        hello_frame("aft", PROTOCOL_VERSION, 7),
    )
    .await;

    let project_root = unique_project_root("consumer-capabilities-absent");
    let (client_ctx, mut client_rx) = route_ctx(ConnectionId::new(40));
    let route_handler = handler.clone();
    let route_task = tokio::spawn(async move {
        route_handler
            .handle_control_frame(&client_ctx, route_open_frame(402, "aft", project_root))
            .await
            .unwrap()
    });
    let bind_frame = tokio::time::timeout(Duration::from_secs(1), module_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let bind: ModuleControlRequest = serde_json::from_slice(&bind_frame.body).unwrap();
    let ModuleControlRequest::RouteBind {
        consumer_capabilities,
        ..
    } = bind
    else {
        panic!("expected route.bind request, got {bind:?}");
    };
    assert_eq!(consumer_capabilities, None);

    handler
        .handle_control_frame(&module_ctx, route_bind_ack(bind_frame.header.corr))
        .await
        .unwrap();
    let route_response = route_task.await.unwrap();
    assert!(route_response.is_empty());
    let published = client_rx.recv().await.unwrap();
    assert!(matches!(
        serde_json::from_slice::<ClientControlResponse>(&published.body).unwrap(),
        ClientControlResponse::RouteOpen { .. }
    ));
}

/// Opens a route to a freshly registered `aft` with `sent` as the
/// route.open's role_versions, acks the bind, and returns the role_versions
/// the module's bind carried.
async fn bind_role_versions_for(
    sent: Option<BTreeMap<String, String>>,
    connection: u64,
) -> Option<BTreeMap<String, String>> {
    let registry = Arc::new(Registry::default());
    let forwarding = Arc::new(ForwardingTable::default());
    let handler = ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding));
    let (module_ctx, mut module_rx) = route_ctx(ConnectionId::new(connection));
    hello_via_sink(
        &handler,
        &module_ctx,
        &mut module_rx,
        hello_frame("aft", PROTOCOL_VERSION, 7),
    )
    .await;
    let project_root = unique_project_root("role-versions");
    let (client_ctx, mut client_rx) = route_ctx(ConnectionId::new(connection + 1));
    let route_handler = handler.clone();
    let route_task = tokio::spawn(async move {
        route_handler
            .handle_control_frame(
                &client_ctx,
                route_open_frame_with_role_versions(403, "aft", project_root, sent),
            )
            .await
            .unwrap()
    });
    let bind_frame = tokio::time::timeout(Duration::from_secs(1), module_rx.recv())
        .await
        .expect("a well-formed route.open reaches the module as a bind")
        .unwrap();
    let bind: ModuleControlRequest = serde_json::from_slice(&bind_frame.body).unwrap();
    let ModuleControlRequest::RouteBind { role_versions, .. } = bind else {
        panic!("expected route.bind request, got {bind:?}");
    };
    handler
        .handle_control_frame(&module_ctx, route_bind_ack(bind_frame.header.corr))
        .await
        .unwrap();
    assert!(route_task.await.unwrap().is_empty());
    let published = client_rx.recv().await.unwrap();
    assert!(matches!(
        serde_json::from_slice::<ClientControlResponse>(&published.body).unwrap(),
        ClientControlResponse::RouteOpen { .. }
    ));
    role_versions
}

#[tokio::test]
async fn route_open_relays_role_versions_verbatim() {
    let sent = role_versions(&[("tool-provider", "v1"), ("management-surface", "v12")]);
    assert_eq!(
        bind_role_versions_for(Some(sent.clone()), 141).await,
        Some(sent)
    );
}

/// An empty map declares nothing, so the provider sees no field rather
/// than an empty object it would have to treat as a second "none".
#[tokio::test]
async fn route_open_with_empty_or_absent_role_versions_relays_none() {
    assert_eq!(bind_role_versions_for(None, 143).await, None);
    assert_eq!(
        bind_role_versions_for(Some(BTreeMap::new()), 145).await,
        None
    );
}

#[tokio::test]
async fn route_open_refuses_malformed_role_versions_before_any_bind() {
    let registry = Arc::new(Registry::default());
    let forwarding = Arc::new(ForwardingTable::default());
    let handler = ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding));
    let (module_ctx, mut module_rx) = route_ctx(ConnectionId::new(147));
    hello_via_sink(
        &handler,
        &module_ctx,
        &mut module_rx,
        hello_frame("aft", PROTOCOL_VERSION, 7),
    )
    .await;

    let nine: BTreeMap<String, String> = (0..9)
        .map(|index| (format!("role-{index}"), "v1".to_string()))
        .collect();
    for (label, malformed) in [
        (
            "invalid role name",
            role_versions(&[("Tool_Provider", "v1")]),
        ),
        ("invalid version", role_versions(&[("tool-provider", "v0")])),
        ("nine entries", nine),
    ] {
        // A refused open answers at once; one that reached the module
        // would wait for its bind ack and trip this timeout.
        let responses = tokio::time::timeout(
            Duration::from_secs(1),
            handler.handle_control_frame(
                &route_ctx(ConnectionId::new(148)).0,
                route_open_frame_with_role_versions(
                    404,
                    "aft",
                    unique_project_root("role-versions-malformed"),
                    Some(malformed),
                ),
            ),
        )
        .await
        .unwrap_or_else(|_| panic!("{label}: the open was relayed instead of refused"))
        .unwrap();
        assert_eq!(responses.len(), 1, "{label}");
        assert_eq!(responses[0].header.ty, FrameType::Error, "{label}");
        let error = parse_error(&responses[0]);
        assert_eq!(error["code"], "invalid_request", "{label}: {error}");
        assert_eq!(
            error["detail"]["field"], "role_versions",
            "{label}: {error}"
        );
        assert!(
            !error_codes::is_retryable_route_open(error["code"].as_str().unwrap()),
            "{label}: a malformed declaration is terminal"
        );
        assert!(
            module_rx.try_recv().is_err(),
            "{label}: the module must never see a bind"
        );
    }
}

/// `route-role-versions/v1` is in HELLO_ACK and `server.describe`, so a
/// consumer can tell this daemon forwards the field from one that would
/// drop it.
#[tokio::test]
async fn route_role_versions_capability_is_advertised() {
    let handler = ControlHandler::new(Arc::new(Registry::default()));
    let (ctx, mut rx) = route_ctx(ConnectionId::new(149));
    let ack = hello_via_sink(
        &handler,
        &ctx,
        &mut rx,
        hello_frame("m", PROTOCOL_VERSION, 1),
    )
    .await;
    let ack = parse_ack(&ack);
    assert!(
        ack.subc_capabilities
            .iter()
            .any(|c| c == "route-role-versions/v1"),
        "{:?}",
        ack.subc_capabilities
    );

    let body = serde_json::to_vec(&ClientControlRequest::ServerDescribe {}).unwrap();
    let frame = Frame::build(FrameType::Request, control_flags(), 0, 0, 5, body).unwrap();
    let reply = handler
        .handle_control_frame(&route_ctx(ConnectionId::new(150)).0, frame)
        .await
        .unwrap()
        .pop()
        .unwrap();
    let ClientControlResponse::ServerDescribe { capabilities, .. } =
        serde_json::from_slice(&reply.body).unwrap()
    else {
        panic!("not a server.describe reply");
    };
    assert!(
        capabilities.iter().any(|c| c == CAP_ROUTE_ROLE_VERSIONS_V1),
        "{capabilities:?}"
    );
}

/// `catalog-events/v1` is in `server.describe`, so a consumer can tell a
/// catalog entry without `events` (the module declares none) from a daemon
/// that never reports declared events.
#[tokio::test]
async fn catalog_events_capability_is_advertised() {
    let handler = ControlHandler::new(Arc::new(Registry::default()));
    let body = serde_json::to_vec(&ClientControlRequest::ServerDescribe {}).unwrap();
    let frame = Frame::build(FrameType::Request, control_flags(), 0, 0, 5, body).unwrap();
    let reply = handler
        .handle_control_frame(&route_ctx(ConnectionId::new(151)).0, frame)
        .await
        .unwrap()
        .pop()
        .unwrap();
    let ClientControlResponse::ServerDescribe { capabilities, .. } =
        serde_json::from_slice(&reply.body).unwrap()
    else {
        panic!("not a server.describe reply");
    };
    assert!(
        capabilities
            .iter()
            .any(|c| c == subc_protocol::manifest::CAP_CATALOG_EVENTS_V1),
        "{capabilities:?}"
    );
}

#[tokio::test]
async fn scope_bind_proof_capability_is_advertised_in_hello_ack_and_server_describe() {
    let handler = ControlHandler::new(Arc::new(Registry::default()));
    let (module, mut module_rx) = route_ctx(ConnectionId::new(152));
    let ack = hello_via_sink(
        &handler,
        &module,
        &mut module_rx,
        hello_frame("scope-proof-owner", PROTOCOL_VERSION, 1),
    )
    .await;
    let ack = parse_ack(&ack);
    assert!(
        ack.subc_capabilities
            .iter()
            .any(|c| c == subc_protocol::manifest::CAP_SCOPE_BIND_PROOF_V1),
        "{:?}",
        ack.subc_capabilities
    );
    assert!(ack
        .subc_capabilities
        .iter()
        .any(|c| c == subc_protocol::manifest::CAP_SCOPE_BIND_PROOF_V2));

    let (client, _client_rx) = route_ctx(ConnectionId::new(153));
    let body = serde_json::to_vec(&ClientControlRequest::ServerDescribe {}).unwrap();
    let frame = Frame::build(FrameType::Request, control_flags(), 0, 0, 5, body).unwrap();
    let reply = handler
        .handle_control_frame(&client, frame)
        .await
        .unwrap()
        .pop()
        .unwrap();
    let ClientControlResponse::ServerDescribe { capabilities, .. } =
        serde_json::from_slice(&reply.body).unwrap()
    else {
        panic!("not a server.describe reply");
    };
    assert!(
        capabilities
            .iter()
            .any(|c| c == subc_protocol::manifest::CAP_SCOPE_BIND_PROOF_V1),
        "{capabilities:?}"
    );
    assert!(capabilities
        .iter()
        .any(|c| c == subc_protocol::manifest::CAP_SCOPE_BIND_PROOF_V2));
}

#[tokio::test]
async fn scope_on_behalf_of_capability_is_advertised_in_hello_ack_and_server_describe() {
    let handler = ControlHandler::new(Arc::new(Registry::default()));
    let (module, mut rx) = route_ctx(ConnectionId::new(156));
    let ack = hello_via_sink(
        &handler,
        &module,
        &mut rx,
        hello_frame("attribution-owner", PROTOCOL_VERSION, 1),
    )
    .await;
    assert!(parse_ack(&ack)
        .subc_capabilities
        .iter()
        .any(|cap| cap == subc_protocol::scope::SCOPE_ON_BEHALF_OF_CAPABILITY));
    assert!(parse_ack(&ack)
        .subc_capabilities
        .iter()
        .any(|cap| cap == subc_protocol::scope::SCOPE_SERVED_BY_CAPABILITY));
    assert!(parse_ack(&ack)
        .subc_capabilities
        .iter()
        .any(|cap| cap == subc_protocol::scope::SCOPE_FEDERATED_V2_CAPABILITY));
    let (client, _rx) = route_ctx(ConnectionId::new(157));
    let body = serde_json::to_vec(&ClientControlRequest::ServerDescribe {}).unwrap();
    let frame = Frame::build(FrameType::Request, control_flags(), 0, 0, 5, body).unwrap();
    let reply = handler
        .handle_control_frame(&client, frame)
        .await
        .unwrap()
        .pop()
        .unwrap();
    let ClientControlResponse::ServerDescribe { capabilities, .. } =
        serde_json::from_slice(&reply.body).unwrap()
    else {
        panic!("server.describe reply")
    };
    assert!(capabilities
        .iter()
        .any(|cap| cap == subc_protocol::scope::SCOPE_ON_BEHALF_OF_CAPABILITY));
    assert!(capabilities
        .iter()
        .any(|cap| cap == subc_protocol::scope::SCOPE_SERVED_BY_CAPABILITY));
    assert!(capabilities
        .iter()
        .any(|cap| cap == subc_protocol::scope::SCOPE_FEDERATED_V2_CAPABILITY));
}

#[tokio::test]
async fn scope_workdir_capability_is_advertised_in_hello_ack_and_server_describe() {
    let handler = ControlHandler::new(Arc::new(Registry::default()));
    let (module, mut rx) = route_ctx(ConnectionId::new(158));
    let ack = hello_via_sink(
        &handler,
        &module,
        &mut rx,
        hello_frame("project-scope-owner", PROTOCOL_VERSION, 1),
    )
    .await;
    assert!(parse_ack(&ack)
        .subc_capabilities
        .iter()
        .any(|cap| cap == "scope-workdir/v1"));
    let (client, _rx) = route_ctx(ConnectionId::new(159));
    let frame = Frame::build(
        FrameType::Request,
        control_flags(),
        0,
        0,
        5,
        serde_json::to_vec(&ClientControlRequest::ServerDescribe {}).unwrap(),
    )
    .unwrap();
    let reply = handler
        .handle_control_frame(&client, frame)
        .await
        .unwrap()
        .remove(0);
    let ClientControlResponse::ServerDescribe { capabilities, .. } =
        serde_json::from_slice(&reply.body).unwrap()
    else {
        panic!("server.describe reply")
    };
    assert!(capabilities.iter().any(|cap| cap == "scope-workdir/v1"));
}

#[tokio::test]
async fn worker_runs_capability_is_advertised_in_hello_ack_and_server_describe() {
    let handler = ControlHandler::new(Arc::new(Registry::default()));
    let (module, mut rx) = route_ctx(ConnectionId::new(160));
    let ack = hello_via_sink(
        &handler,
        &module,
        &mut rx,
        hello_frame("worker-run-owner", PROTOCOL_VERSION, 1),
    )
    .await;
    assert!(parse_ack(&ack)
        .subc_capabilities
        .iter()
        .any(|cap| cap == "worker-runs/v1"));
    let (client, _rx) = route_ctx(ConnectionId::new(161));
    let frame = Frame::build(
        FrameType::Request,
        control_flags(),
        0,
        0,
        5,
        serde_json::to_vec(&ClientControlRequest::ServerDescribe {}).unwrap(),
    )
    .unwrap();
    let reply = handler
        .handle_control_frame(&client, frame)
        .await
        .unwrap()
        .remove(0);
    let ClientControlResponse::ServerDescribe { capabilities, .. } =
        serde_json::from_slice(&reply.body).unwrap()
    else {
        panic!("server.describe reply")
    };
    assert!(capabilities.iter().any(|cap| cap == "worker-runs/v1"));
}

#[tokio::test]
async fn scope_replace_if_idle_capability_is_advertised_in_hello_ack_and_server_describe() {
    let handler = ControlHandler::new(Arc::new(Registry::default()));
    let (module, mut rx) = route_ctx(ConnectionId::new(154));
    let ack = hello_via_sink(
        &handler,
        &module,
        &mut rx,
        hello_frame("idle-scope-owner", PROTOCOL_VERSION, 1),
    )
    .await;
    assert!(parse_ack(&ack)
        .subc_capabilities
        .iter()
        .any(|cap| cap == subc_protocol::manifest::CAP_SCOPE_REPLACE_IF_IDLE_V1));
    let (client, _rx) = route_ctx(ConnectionId::new(155));
    let body = serde_json::to_vec(&ClientControlRequest::ServerDescribe {}).unwrap();
    let frame = Frame::build(FrameType::Request, control_flags(), 0, 0, 5, body).unwrap();
    let reply = handler
        .handle_control_frame(&client, frame)
        .await
        .unwrap()
        .pop()
        .unwrap();
    let ClientControlResponse::ServerDescribe { capabilities, .. } =
        serde_json::from_slice(&reply.body).unwrap()
    else {
        panic!("not a server.describe reply")
    };
    assert!(capabilities
        .iter()
        .any(|cap| cap == subc_protocol::manifest::CAP_SCOPE_REPLACE_IF_IDLE_V1));
}

#[tokio::test]
async fn supervision_only_module_health_probe_does_not_enable_route_open_and_cleans_up() {
    let registry = Arc::new(Registry::default());
    let forwarding = Arc::new(ForwardingTable::default());
    let handler = ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding))
        .with_health_probe_timeout(Duration::from_secs(5));
    let (module_ctx, mut module_rx) = route_ctx(ConnectionId::new(35));
    hello_via_sink(
        &handler,
        &module_ctx,
        &mut module_rx,
        non_routable_hello_frame_with_control_ops(
            "mcp",
            300,
            Some(vec![MODULE_CONTROL_OP_HEALTH_CHECK.to_string()]),
        ),
    )
    .await;
    assert!(registry
        .get_module("mcp")
        .unwrap()
        .unwrap()
        .manifest
        .provides
        .is_empty());

    let (route_client_ctx, _route_client_rx) = route_ctx(ConnectionId::new(36));
    let route_response = handler
        .handle_control_frame(
            &route_client_ctx,
            route_open_frame(301, "mcp", unique_project_root("non-routable-mcp")),
        )
        .await
        .unwrap();
    assert_eq!(route_response[0].header.ty, FrameType::Error);
    assert_eq!(
        parse_error(&route_response[0])["code"],
        "target_unavailable"
    );
    assert!(parse_error(&route_response[0])["message"]
        .as_str()
        .unwrap()
        .contains("does not provide the requested target"));
    assert!(module_rx.try_recv().is_err());

    let (health_client_ctx, _health_client_rx) = route_ctx(ConnectionId::new(37));
    let health_handler = handler.clone();
    let health_task = tokio::spawn(async move {
        health_handler
            .handle_control_frame(
                &health_client_ctx,
                supervisor_health_probe_frame(302, "mcp"),
            )
            .await
            .unwrap()
    });
    let health_frame = tokio::time::timeout(Duration::from_secs(1), module_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<ModuleControlRequest>(&health_frame.body).unwrap(),
        ModuleControlRequest::HealthCheck {}
    );
    handler
        .handle_control_frame(
            &module_ctx,
            health_response(health_frame.header.corr, HealthStatus::Ok),
        )
        .await
        .unwrap();
    let health_response = health_task.await.unwrap();
    assert_eq!(health_response[0].header.ty, FrameType::Response);
    match serde_json::from_slice::<ClientControlResponse>(&health_response[0].body).unwrap() {
        ClientControlResponse::SupervisorHealthProbe {
            module_id, status, ..
        } => {
            assert_eq!(module_id, "mcp");
            assert_eq!(status, HealthStatus::Ok);
        }
        other => panic!("unexpected health response: {other:?}"),
    }

    // Exercise the forwarding cleanup path directly while leaving the registry
    // advertisement in place. If cleanup leaves a stale control sink behind,
    // the next probe will enqueue onto it and wait for the long probe timeout
    // instead of returning an immediate no-connection error.
    forwarding
        .cleanup_connection(module_ctx.connection_id)
        .unwrap();
    let (cleanup_probe_ctx, _cleanup_probe_rx) = route_ctx(ConnectionId::new(38));
    let cleanup_response = tokio::time::timeout(
        Duration::from_millis(200),
        handler.handle_control_frame(
            &cleanup_probe_ctx,
            supervisor_health_probe_frame(303, "mcp"),
        ),
    )
    .await
    .expect("probe should fail immediately when the control lane is gone")
    .unwrap();
    assert_eq!(cleanup_response[0].header.ty, FrameType::Error);
    assert_eq!(
        parse_error(&cleanup_response[0])["code"],
        "target_unavailable"
    );
    assert!(parse_error(&cleanup_response[0])["message"]
        .as_str()
        .unwrap()
        .contains("no module connection"));

    handler
        .cleanup_connection(module_ctx.connection_id)
        .unwrap();
}

#[tokio::test]
async fn route_open_classifies_unregistered_running_supervised_module_as_warming() {
    let registry = Arc::new(Registry::default());
    let supervisor_handle = SupervisorHandle::new();
    let supervisor =
        Supervisor::new_for_test(Arc::clone(&registry), RestartPolicy::new(0, Duration::ZERO))
            .with_handle(supervisor_handle.clone())
            .with_connection_file_path(
                std::env::temp_dir()
                    .join(format!("subc-route-open-warming-{}", std::process::id())),
            );
    let module = supervisor
        .supervise_configured(
            ModuleSpec {
                script: None,
                carry_containment: false,
                module_id: "warming".to_string(),
                program: fake_aft_stub_path(),
                args: Vec::new(),
                env: Vec::new(),
                reserved: false,
                reserved_prefixes: Vec::new(),
                protocol: ModuleProtocol::Subc,
                overlap: Default::default(),
            },
            true,
        )
        .unwrap();
    assert_eq!(module.state().unwrap(), ModuleState::Running);

    let handler = ControlHandler::new(Arc::clone(&registry)).with_supervisor(supervisor_handle);
    let (ctx, _rx) = route_ctx(ConnectionId::new(39));
    let response = handler
        .handle_control_frame(
            &ctx,
            route_open_frame(304, "warming", unique_project_root("warming")),
        )
        .await
        .unwrap();
    module.stop().await.unwrap();

    assert_eq!(response[0].header.ty, FrameType::Error);
    let error = parse_error(&response[0]);
    assert_eq!(error["code"], "module_warming");
    assert!(error["message"]
        .as_str()
        .unwrap()
        .contains("state=running, enabled=true, live=false"));
}

#[test]
fn route_open_connection_cap_logs_admission_reason_and_capacity() {
    let handler = ControlHandler::new(Arc::new(Registry::default()));
    let capture = EventCapture::default();
    let _subscriber =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(capture.clone()));
    let (ctx, _rx) = route_ctx(ConnectionId::new(96));
    let limit = crate::server::MAX_PENDING_ROUTE_OPENS_PER_CONNECTION;
    let pending = (0..limit).collect::<Vec<_>>();
    let response = handler
        .route_open_capacity_refusal(
            &ctx,
            &route_open_frame(396, "busy", unique_project_root("connection-cap")),
            "busy",
            pending.len(),
            limit,
        )
        .unwrap();
    assert_eq!(parse_error(&response)["code"], "target_unavailable");
    let event = capture
        .events()
        .into_iter()
        .find(|event| {
            event.target == "control"
                && event.fields.get("reason") == Some(&"\"open_admission_full\"".to_string())
        })
        .expect("connection admission refusal event");
    assert_eq!(event.fields.get("in_flight"), Some(&limit.to_string()));
    assert_eq!(event.fields.get("limit"), Some(&limit.to_string()));
    // Refused before `handle_route_open` parses the request, so the caller
    // fields are read from the frame itself.
    assert_eq!(event.fields.get("principal"), Some(&"direct".to_string()));
    assert_eq!(event.fields.get("scope"), Some(&"none".to_string()));
}

#[test]
fn route_open_target_cap_logs_admission_reason_and_capacity() {
    let handler = ControlHandler::new(Arc::new(Registry::default()));
    let capture = EventCapture::default();
    let _subscriber =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(capture.clone()));
    let (ctx, _rx) = route_ctx(ConnectionId::new(97));
    let limit = MAX_PENDING_ROUTE_BINDS_PER_TARGET;
    let guards = (0..limit)
        .map(|_| {
            handler
                .route_bind_concurrency
                .try_admit("busy", limit)
                .unwrap()
        })
        .collect::<Vec<_>>();
    let in_flight = match handler.route_bind_concurrency.try_admit("busy", limit) {
        Err(in_flight) => in_flight,
        Ok(_) => panic!("target cap must refuse after {limit} admissions"),
    };
    let response = handler
        .route_open_target_capacity_refusal(
            &ctx,
            &RouteOpenCaller::unknown(),
            &route_open_frame(397, "busy", unique_project_root("target-cap")),
            "busy",
            in_flight,
        )
        .unwrap();
    assert_eq!(parse_error(&response)["code"], "target_unavailable");
    let event = capture
        .events()
        .into_iter()
        .find(|event| {
            event.target == "control"
                && event.fields.get("reason") == Some(&"\"target_binds_full\"".to_string())
        })
        .expect("target admission refusal event");
    assert_eq!(event.fields.get("in_flight"), Some(&limit.to_string()));
    assert_eq!(event.fields.get("limit"), Some(&limit.to_string()));
    drop(guards);
}

#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn supervisor_list_shows_a_failed_modules_next_retry() {
    let registry = Arc::new(Registry::default());
    let handle = SupervisorHandle::new();
    let supervisor =
        Supervisor::new_for_test(registry.clone(), RestartPolicy::new(0, Duration::ZERO))
            .with_handle(handle.clone());
    let module = supervisor
        .spawn(crate::ModuleSpec {
            script: None,
            carry_containment: false,
            module_id: "retry-list".into(),
            program: PathBuf::from("/bin/sh"),
            args: vec!["-c".into(), "exit 17".into()],
            env: vec![],
            reserved: false,
            reserved_prefixes: vec![],
            protocol: ModuleProtocol::None,
            overlap: Default::default(),
        })
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while module.state().unwrap() != crate::ModuleState::Failed {
        assert!(
            std::time::Instant::now() < deadline,
            "waiting for budget failure"
        );
        tokio::task::yield_now().await;
    }
    let handler = ControlHandler::new(registry).with_supervisor(handle);
    let frame = Frame::build(FrameType::Request, control_flags(), 0, 0, 2, Vec::new()).unwrap();
    let replies = handler.handle_supervisor_list(frame).await.unwrap();
    let body: Value = serde_json::from_slice(&replies[0].body).unwrap();
    assert_eq!(body["modules"][0]["state"], "failed");
    let retry = body["modules"][0]["next_retry_at_ms"]
        .as_u64()
        .expect("scheduled retry on the wire");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    assert!((599_000..=600_000).contains(&retry.saturating_sub(now)));
    module.set_enabled(false).await.unwrap();
    let frame = Frame::build(FrameType::Request, control_flags(), 0, 0, 3, Vec::new()).unwrap();
    let replies = handler.handle_supervisor_list(frame).await.unwrap();
    let body: Value = serde_json::from_slice(&replies[0].body).unwrap();
    assert!(body["modules"][0].get("next_retry_at_ms").is_none());
}

#[tokio::test]
async fn slow_step_before_pid_read_does_not_spend_its_budget() {
    let dir = cortexkit_test_support::ScratchDir::new("pid-read-budget");
    let supervisor =
        Supervisor::new_for_test(Arc::new(Registry::default()), RestartPolicy::default());
    let module = supervisor
        .spawn(crate::ModuleSpec {
            script: None,
            carry_containment: false,
            module_id: "pid-read-budget".into(),
            program: std::env::current_exe().unwrap(),
            args: vec![
                "--exact".into(),
                "control::tests::pid_resource_read_fixture".into(),
            ],
            env: ["XDG_DATA_HOME", "XDG_RUNTIME_DIR", "XDG_CONFIG_HOME"]
                .into_iter()
                .map(|key| (key.into(), dir.display().to_string()))
                .chain(std::iter::once((
                    "SUBC_SUBTREE_PID_FIXTURE".into(),
                    "1".into(),
                )))
                .collect(),
            reserved: false,
            reserved_prefixes: Vec::new(),
            protocol: ModuleProtocol::None,
            overlap: Default::default(),
        })
        .unwrap();
    let ready_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !matches!(
        module.child_resource_usage(),
        subc_control::ChildResourceUsage::Measured(_)
    ) {
        assert!(
            tokio::time::Instant::now() < ready_deadline,
            "fixture process was not measurable"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(crate::subtree_resources::READ_TIMEOUT * 2).await;
    let reading = {
        let _guard = crate::subtree_resources::fault_read_guard().await;
        crate::subtree_resources::read_process(module.clone()).await
    };
    stop_resource_fixture(&module, &dir).await;
    assert!(
        matches!(reading, subc_control::ChildResourceUsage::Measured(_)),
        "{reading:?}"
    );
}

#[test]
fn pid_resource_read_fixture() {
    if std::env::var_os("SUBC_SUBTREE_PID_FIXTURE").is_some() {
        let dir = std::path::PathBuf::from(std::env::var_os("XDG_DATA_HOME").unwrap());
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        while !dir.join("stop-module").exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        std::fs::write(dir.join("module-stopped"), "").unwrap();
    }
}

async fn stop_resource_fixture(module: &crate::supervise::SupervisedModule, dir: &std::path::Path) {
    let stopping = module.stop();
    tokio::pin!(stopping);
    tokio::select! {
        result = &mut stopping => result.unwrap(),
        () = async {
            while module.state().unwrap() != crate::supervise::ModuleState::Draining {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            std::fs::write(dir.join("stop-module"), "").unwrap();
        } => stopping.await.unwrap(),
    }
    #[cfg(windows)]
    assert!(
        dir.join("module-stopped").exists(),
        "the resource fixture must exit cooperatively, not wait for the drain kill"
    );
    assert_eq!(
        module.state().unwrap(),
        crate::supervise::ModuleState::Stopped
    );
}

#[tokio::test]
async fn disabled_module_tree_is_not_running_instead_of_permanently_pending() {
    let registry = Arc::new(Registry::default());
    let handle = SupervisorHandle::new();
    let supervisor = Supervisor::new_for_test(registry.clone(), RestartPolicy::default())
        .with_handle(handle.clone());
    supervisor
        .supervise_configured(
            crate::ModuleSpec {
                script: None,
                module_id: "disabled-resource-reading".into(),
                program: std::env::current_exe().unwrap(),
                args: Vec::new(),
                env: Vec::new(),
                reserved: false,
                reserved_prefixes: Vec::new(),
                protocol: ModuleProtocol::None,
                overlap: Default::default(),
                carry_containment: false,
            },
            false,
        )
        .unwrap();
    let handler = ControlHandler::new(registry).with_supervisor(handle);
    let frame = Frame::build(FrameType::Request, control_flags(), 0, 0, 1, Vec::new()).unwrap();
    let replies = handler.handle_supervisor_list(frame).await.unwrap();
    let body: Value = serde_json::from_slice(&replies[0].body).unwrap();
    assert!(body["modules"][0].get("tree").is_none());
    assert_eq!(body["modules"][0]["tree_unavailable"], "not_running");
}

#[tokio::test]
async fn supervisor_list_reads_process_resources_before_a_stalled_tree() {
    let dir = cortexkit_test_support::ScratchDir::new("list-process-priority");
    let registry = Arc::new(Registry::default());
    let handle = SupervisorHandle::new();
    let supervisor = Supervisor::new_for_test(registry.clone(), RestartPolicy::default())
        .with_handle(handle.clone());
    let module = supervisor
        .spawn(crate::ModuleSpec {
            script: None,
            carry_containment: false,
            module_id: "list-process-priority".into(),
            program: std::env::current_exe().unwrap(),
            args: vec![
                "--exact".into(),
                "control::tests::pid_resource_read_fixture".into(),
            ],
            env: ["XDG_DATA_HOME", "XDG_RUNTIME_DIR", "XDG_CONFIG_HOME"]
                .into_iter()
                .map(|key| (key.into(), dir.display().to_string()))
                .chain(std::iter::once((
                    "SUBC_SUBTREE_PID_FIXTURE".into(),
                    "1".into(),
                )))
                .collect(),
            reserved: false,
            reserved_prefixes: Vec::new(),
            protocol: ModuleProtocol::None,
            overlap: Default::default(),
        })
        .unwrap();
    let ready_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let image = module.running_image_agreement().await;
        if matches!(image, subc_control::RunningImageAgreement::Match { .. }) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < ready_deadline,
            "the fixture process never became observable: {image:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let guard = crate::subtree_resources::fault_read_guard().await;
    let (target, release, _) = crate::subtree_resources::held();
    module.set_subtree_target_for_test(target);
    let handler = ControlHandler::new(registry).with_supervisor(handle);
    let frame = Frame::build(FrameType::Request, control_flags(), 0, 0, 1, Vec::new()).unwrap();
    let replies = handler.handle_supervisor_list(frame).await.unwrap();
    drop(release);
    drop(guard);
    stop_resource_fixture(&module, &dir).await;
    let body: Value = serde_json::from_slice(&replies[0].body).unwrap();
    assert_eq!(
        body["modules"][0]["resources"]["status"], "measured",
        "the optional tree timeout must not discard the process reading: {body}"
    );
    assert!(body["modules"][0].get("tree").is_none());
    assert_eq!(body["modules"][0]["tree_unavailable"], "not_yet_measured");
    assert_eq!(
        body["modules"][0]["pending_reload"]["image"]["status"], "match",
        "the tree timeout must not hide a completed image observation"
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn cold_explicit_provenance_completes_while_list_stays_bounded() {
    let mut handler = ControlHandler::new(Arc::new(Registry::default())).with_daemon_provenance(
        std::process::id(),
        0,
        Some(std::env::current_exe().unwrap()),
        None,
        None,
    );
    let (probe, release, began) = ExecutableIdentityProbe::held();
    handler.daemon_provenance.probe = probe;
    let observe = |handler: ControlHandler, corr| {
        tokio::spawn(async move {
            let (ctx, _) = route_ctx(ConnectionId::new(corr));
            let frame =
                Frame::build(FrameType::Request, control_flags(), 0, 0, corr, Vec::new()).unwrap();
            handler
                .handle_supervisor_provenance(&ctx, frame, None)
                .await
                .unwrap()
        })
    };
    let explicit = observe(handler.clone(), 114);
    began.await.unwrap();
    let concurrent = observe(handler.clone(), 115);
    // Keep explicit provenance waiting on its first image observation beyond
    // the list deadline; release it only after the concurrent list replies.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let frame = Frame::build(FrameType::Request, control_flags(), 0, 0, 2, Vec::new()).unwrap();
    let list = tokio::time::timeout(
        Duration::from_secs(1),
        handler.handle_supervisor_list(frame),
    )
    .await;
    let explicit_was_pending = !explicit.is_finished() && !concurrent.is_finished();
    drop(release);
    let replies = explicit.await.unwrap();
    let concurrent_replies = concurrent.await.unwrap();
    assert!(
        explicit_was_pending,
        "explicit provenance must await the full observation"
    );
    let list = list
        .expect("a concurrent explicit probe must not stall list")
        .unwrap();
    let list_body: Value = serde_json::from_slice(&list[0].body).unwrap();
    assert_eq!(list_body["op"], "supervisor.list");
    for replies in [replies, concurrent_replies] {
        let body: Value = serde_json::from_slice(&replies[0].body).unwrap();
        assert_eq!(
            body["daemon"]["daemon_observed"]["running_image"]["status"],
            "match"
        );
    }
}

#[tokio::test]
async fn supervisor_list_slow_subtree_does_not_hold_connection_reader() {
    let registry = Arc::new(Registry::default());
    let handle = SupervisorHandle::new();
    let supervisor = Supervisor::new_for_test(registry.clone(), RestartPolicy::default())
        .with_handle(handle.clone());
    let module = supervisor
        .supervise_configured(
            crate::ModuleSpec {
                script: None,
                carry_containment: false,
                module_id: "memory-read".into(),
                program: PathBuf::from("disabled-module"),
                args: Vec::new(),
                env: Vec::new(),
                reserved: false,
                reserved_prefixes: Vec::new(),
                protocol: ModuleProtocol::Subc,
                overlap: Default::default(),
            },
            true,
        )
        .unwrap();
    let _guard = crate::subtree_resources::fault_read_guard().await;
    let (target, release, began) = crate::subtree_resources::held();
    // Optional list work may be skipped when its deadline is already spent.
    // Establish the stalled fixture before asking the list to observe it.
    let stalled_target = target.clone();
    let stalled =
        tokio::task::spawn_blocking(move || stalled_target.tree(&subc_os::ProcessTable::default()));
    began.await.unwrap();
    module.set_subtree_target_for_test(target);
    let handler = ControlHandler::new(registry).with_supervisor(handle);
    let (ctx, mut rx) = route_ctx(ConnectionId::new(112));
    let frame = Frame::build(FrameType::Request, control_flags(), 0, 0, 1, Vec::new()).unwrap();
    let replies = handler
        .handle_client_control_request(
            &ctx,
            frame,
            ClientControlRequest::SupervisorList {},
            &mut None,
        )
        .await
        .unwrap();
    assert!(
        replies.is_empty(),
        "list reply must be detached from the connection reader"
    );
    let frame = Frame::build(FrameType::Request, control_flags(), 0, 0, 2, Vec::new()).unwrap();
    let replies = handler
        .handle_client_control_request(
            &ctx,
            frame,
            ClientControlRequest::SupervisorSpawnSnapshot {},
            &mut None,
        )
        .await
        .unwrap();
    assert_eq!(
        replies.len(),
        1,
        "another request must complete while the subtree read stalls"
    );
    let reply = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap();
    release.send(()).unwrap();
    drop(release);
    stalled.await.unwrap();
    let body: Value = serde_json::from_slice(&reply.frame.body).unwrap();
    assert!(
        body["modules"][0].get("tree").is_none(),
        "an unknown tree reading must be omitted after the bounded read expires"
    );
    // Lists export completed work rather than measuring synchronously.
    // Complete a refresh after the fixture is released before asserting
    // that a later list contains its measured tree.
    handler
        .resource_tracker
        .read(handler.supervisor.list(), false, "released_tree_fixture")
        .await;
    let frame = Frame::build(FrameType::Request, control_flags(), 0, 0, 3, Vec::new()).unwrap();
    let replies = handler.handle_supervisor_list(frame).await.unwrap();
    let body: Value = serde_json::from_slice(&replies[0].body).unwrap();
    assert_eq!(
        body["modules"][0]["tree"]["source"], "process_tree",
        "the next request must export the completed refresh"
    );
    assert_eq!(body["modules"][0]["tree"]["process_count"], 1);
}

#[tokio::test]
async fn supervisor_list_expired_budget_keeps_committed_accounting_rows() {
    use subc_control::{
        AgentCommandResources, ChildMemoryKind, ChildResourceReading,
        ChildResourceUnavailableReason, ChildResourceUsage, CpuLastHour, DaemonResources,
        TreeReading, TreeSource,
    };
    let registry = Arc::new(Registry::default());
    let handle = SupervisorHandle::new();
    let supervisor = Supervisor::new_for_test(registry.clone(), RestartPolicy::default())
        .with_handle(handle.clone());
    supervisor
        .supervise_configured(
            crate::ModuleSpec {
                script: None,
                carry_containment: false,
                module_id: "budget-worker".into(),
                program: PathBuf::from("disabled-module"),
                args: Vec::new(),
                env: Vec::new(),
                reserved: false,
                reserved_prefixes: Vec::new(),
                protocol: ModuleProtocol::Subc,
                overlap: Default::default(),
            },
            true,
        )
        .unwrap();
    let handler = ControlHandler::new(registry).with_supervisor(handle);
    let now = u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap();
    let measured = now - 1_000;
    let hour = CpuLastHour::new(100, 60_000);
    let daemon = DaemonResources::new(7, 1, "test")
        .with_measured_at_ms(Some(measured))
        .with_resources(Some(ChildResourceUsage::Measured(
            ChildResourceReading::new(20, ChildMemoryKind::ResidentSet, 10, 0),
        )))
        .with_cpu_last_hour(Some(hour.clone()));
    let agents = AgentCommandResources::new("budget-worker", 50, 30, 1)
        .with_measured_at_ms(Some(measured))
        .with_cpu_last_hour(Some(hour.clone()));
    let tree = TreeReading::new(
        150,
        50,
        ChildMemoryKind::ResidentSet,
        1,
        TreeSource::ProcessTree,
    )
    .with_measured_at_ms(Some(measured));
    handler.resource_tracker.seed_committed_for_test(
        &handler.supervisor.list(),
        crate::resource_totals::Readings {
            modules: BTreeMap::from([("budget-worker".into(), (Some(tree), Some(hour)))]),
            daemon: Some(daemon),
            agent_commands: Some(vec![agents]),
            ..Default::default()
        },
        now,
    );
    list_diagnostics::scope(async {
        tokio::time::sleep(Duration::from_millis(110)).await;
        assert!(list_diagnostics::deadline().unwrap() <= tokio::time::Instant::now());
        let request =
            Frame::build(FrameType::Request, control_flags(), 0, 0, 1, Vec::new()).unwrap();
        let replies = handler.handle_supervisor_list_inner(request).await.unwrap();
        let ClientControlResponse::SupervisorList {
            modules,
            daemon,
            agent_commands,
            ..
        } = serde_json::from_slice(&replies[0].body).unwrap()
        else {
            panic!("expected a list response")
        };
        assert!(matches!(
            modules[0].resources,
            Some(ChildResourceUsage::Unavailable {
                reason: ChildResourceUnavailableReason::Unreadable
            })
        ));
        assert_eq!(
            (
                modules[0].tree.as_ref().unwrap().cpu_ms,
                modules[0].tree.as_ref().unwrap().measured_at_ms
            ),
            (150, Some(measured))
        );
        let daemon = daemon.unwrap();
        let agents = agent_commands.unwrap();
        assert_eq!(daemon.measured_at_ms, Some(measured));
        assert_eq!(
            (agents.len(), agents[0].cpu_ms, agents[0].measured_at_ms),
            (1, 50, Some(measured))
        );
        for hour in [
            modules[0].cpu_last_hour.as_ref(),
            daemon.cpu_last_hour.as_ref(),
            agents[0].cpu_last_hour.as_ref(),
        ] {
            let hour = hour.unwrap();
            assert_eq!((hour.cpu_ms, hour.covered_ms), (100, 60_000));
        }
    })
    .await;
}

#[tokio::test]
async fn supervisor_list_reports_loaded_config_and_omits_unconfigured() {
    let registry = Arc::new(Registry::default());
    let handle = SupervisorHandle::new();
    let supervisor = Supervisor::new_for_test(registry.clone(), RestartPolicy::default())
        .with_handle(handle.clone());
    supervisor
        .supervise_configured(
            crate::ModuleSpec {
                script: None,
                carry_containment: false,
                module_id: "drift".into(),
                program: PathBuf::from("disabled-module"),
                args: Vec::new(),
                env: Vec::new(),
                reserved: false,
                reserved_prefixes: Vec::new(),
                protocol: ModuleProtocol::Subc,
                overlap: Default::default(),
            },
            false,
        )
        .unwrap();
    let handler = ControlHandler::new(registry).with_supervisor(handle);
    for configured in [None, Some(false), Some(true)] {
        handler.capability_evaluator.configure(
            configured.map(|enabled| ("drift".to_string(), enabled)),
            BTreeMap::new(),
        );
        let frame = Frame::build(FrameType::Request, control_flags(), 0, 0, 1, Vec::new()).unwrap();
        let replies = handler
            .handle_supervisor_set_enabled(frame, "drift".into(), false, Principal::Direct)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&replies[0].body).unwrap();
        assert!(body.get("configured_enabled").is_none());
        assert_eq!(body["op"], "supervisor.ack");
        let frame = Frame::build(FrameType::Request, control_flags(), 0, 0, 2, Vec::new()).unwrap();
        let replies = handler.handle_supervisor_list(frame).await.unwrap();
        let body: Value = serde_json::from_slice(&replies[0].body).unwrap();
        assert_eq!(
            body["modules"][0].get("configured_enabled"),
            configured.map(Value::Bool).as_ref()
        );
        assert!(body["modules"][0].get("tree").is_none());
        assert!(body["modules"][0].get("subtree").is_none());
    }
}

#[tokio::test(flavor = "current_thread")]
async fn supervisor_set_enabled_logs_request_received_with_direct_caller() {
    let handler = ControlHandler::new(Arc::new(Registry::default()));
    let capture = EventCapture::default();
    let _subscriber =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(capture.clone()));
    let (ctx, _rx) = route_ctx(ConnectionId::new(101));
    let frame = Frame::build(FrameType::Request, control_flags(), 0, 0, 1, Vec::new()).unwrap();

    let _ = handler
        .handle_client_control_request(
            &ctx,
            frame,
            ClientControlRequest::SupervisorSetEnabled {
                module_id: "broca".to_string(),
                enabled: false,
            },
            &mut None,
        )
        .await
        .unwrap();

    let events = capture
        .events()
        .into_iter()
        .filter(|event| {
            event.target == "control"
                && event.fields.get("message").map(String::as_str)
                    == Some("supervisor request received")
        })
        .collect::<Vec<_>>();
    assert_eq!(events.len(), 1, "one supervisor request log line");
    let event = &events[0];
    assert_eq!(event.level, tracing::Level::INFO);
    assert_eq!(
        event.fields.get("op"),
        Some(&format!("{:?}", ops::SUPERVISOR_SET_ENABLED))
    );
    assert_eq!(
        event.fields.get("module_id"),
        Some(&"\"broca\"".to_string())
    );
    assert_eq!(event.fields.get("enabled"), Some(&"false".to_string()));
    assert_eq!(event.fields.get("connection_id"), Some(&"101".to_string()));
    assert_eq!(event.fields.get("caller"), Some(&"direct".to_string()));
}

#[tokio::test(flavor = "current_thread")]
async fn supervisor_request_from_a_registered_module_logs_its_reserved_principal() {
    let registry = Arc::new(Registry::default());
    let module_connection = ConnectionId::new(303);
    registry
        .register_with_control_ops(
            manifest("aft", PROTOCOL_VERSION),
            PROTOCOL_VERSION,
            module_connection,
            module_baseline_control_ops(),
        )
        .unwrap();
    let handler = ControlHandler::new(registry);
    let capture = EventCapture::default();
    let _subscriber =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(capture.clone()));
    let (ctx, _rx) = route_ctx(module_connection);
    let frame = Frame::build(FrameType::Request, control_flags(), 0, 0, 1, Vec::new()).unwrap();

    let _ = handler
        .handle_client_control_request(
            &ctx,
            frame,
            ClientControlRequest::SupervisorSetEnabled {
                module_id: "broca".to_string(),
                enabled: false,
            },
            &mut None,
        )
        .await
        .unwrap();

    let callers = capture
        .events()
        .into_iter()
        .filter(|event| {
            event.fields.get("message").map(String::as_str) == Some("supervisor request received")
        })
        .map(|event| event.fields.get("caller").cloned())
        .collect::<Vec<_>>();
    assert_eq!(callers, vec![Some("reserved:aft".to_string())]);
}

#[tokio::test(flavor = "current_thread")]
async fn supervisor_list_does_not_log_request_received() {
    let handler = ControlHandler::new(Arc::new(Registry::default()));
    let capture = EventCapture::default();
    let _subscriber =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(capture.clone()));
    let (ctx, _rx) = route_ctx(ConnectionId::new(102));
    let frame = Frame::build(FrameType::Request, control_flags(), 0, 0, 2, Vec::new()).unwrap();

    handler
        .handle_client_control_request(
            &ctx,
            frame,
            ClientControlRequest::SupervisorList {},
            &mut None,
        )
        .await
        .unwrap();

    assert!(capture.events().into_iter().all(|event| {
        event.fields.get("message").map(String::as_str) != Some("supervisor request received")
    }));
}

#[tokio::test(flavor = "current_thread")]
async fn supervisor_rescan_preview_does_not_log_request_received() {
    let handler = ControlHandler::new(Arc::new(Registry::default()));
    let capture = EventCapture::default();
    let _subscriber =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(capture.clone()));
    let (ctx, _rx) = route_ctx(ConnectionId::new(103));
    let frame = Frame::build(FrameType::Request, control_flags(), 0, 0, 3, Vec::new()).unwrap();

    handler
        .handle_client_control_request(
            &ctx,
            frame,
            ClientControlRequest::SupervisorRescan { preview: true },
            &mut None,
        )
        .await
        .unwrap();

    assert!(capture.events().into_iter().all(|event| {
        event.fields.get("message").map(String::as_str) != Some("supervisor request received")
    }));
}

struct ApproveOperator;
impl crate::operator_confirm::OperatorProvider for ApproveOperator {
    fn prompt(
        &self,
        _: &str,
        _: Box<dyn FnOnce(Arc<dyn crate::operator_confirm::OperatorWithdraw>) + Send>,
    ) -> crate::operator_confirm::ProviderResult {
        crate::operator_confirm::ProviderResult::Approved
    }
}

#[tokio::test]
async fn privileged_operator_prompt_rescan_reports_changes_without_changing_running_mode() {
    assert_eq!(
        ControlHandler::default().privileged_operator_prompt,
        PrivilegedOperatorPrompt::Log
    );
    for (initial, changed, running) in [
        ("log", "enforce", PrivilegedOperatorPrompt::Log),
        ("enforce", "log", PrivilegedOperatorPrompt::Enforce),
    ] {
        let dir = ScratchDir::new("rescan-privileged-operator-prompt");
        let path = dir.join("subc.jsonc");
        std::fs::write(
            &path,
            json!({"version":1,"privileged_operator_prompt":initial}).to_string(),
        )
        .unwrap();
        let config = crate::daemon_config::load(&path).unwrap().unwrap();
        let registry = Arc::new(Registry::default());
        let handle = SupervisorHandle::new();
        let supervisor =
            Supervisor::new(registry.clone(), RestartPolicy::default()).with_handle(handle.clone());
        let handler = ControlHandler::new(registry)
            .with_supervisor(handle)
            .with_privileged_operator_prompt(config.privileged_operator_prompt)
            .with_supervisor_rescan(supervisor, &path, config.port);
        handler.forwarding.operator_confirms().configure(
            Arc::new(ApproveOperator),
            crate::operator_confirm::OperatorLimits::default(),
        );
        let (ctx, _rx) = route_ctx(ConnectionId::new(104));

        for (disk_mode, needs_restart) in [
            (Some(initial), false),
            (Some(changed), true),
            (Some(changed), true),
            (None, running == PrivilegedOperatorPrompt::Enforce),
            (Some(initial), false),
        ] {
            let mut doc = json!({"version":1});
            if let Some(mode) = disk_mode {
                doc["privileged_operator_prompt"] = json!(mode);
            }
            std::fs::write(&path, doc.to_string()).unwrap();
            for preview in [true, false] {
                let request = ClientControlRequest::SupervisorRescan { preview };
                let frame = Frame::build(
                    FrameType::Request,
                    control_flags(),
                    0,
                    0,
                    1,
                    serde_json::to_vec(&request).unwrap(),
                )
                .unwrap();
                let frames = handler.handle_control_frame(&ctx, frame).await.unwrap();
                assert_eq!(frames.len(), 1);
                let ClientControlResponse::SupervisorRescan { result } =
                    serde_json::from_slice(&frames[0].body).unwrap()
                else {
                    panic!("expected a rescan response");
                };
                let expected = if needs_restart {
                    vec!["privileged_operator_prompt"]
                } else {
                    Vec::new()
                };
                assert_eq!(result.restart_required, expected);
                assert_eq!(handler.privileged_operator_prompt, running);
                assert_eq!(
                    handler.rescan.as_ref().unwrap().privileged_operator_prompt,
                    running
                );
            }
        }
    }
}

#[tokio::test]
async fn privileged_operator_prompt_rescan_refuses_invalid_values() {
    let dir = ScratchDir::new("rescan-invalid-privileged-operator-prompt");
    let path = dir.join("subc.jsonc");
    let registry = Arc::new(Registry::default());
    let handle = SupervisorHandle::new();
    let supervisor =
        Supervisor::new(registry.clone(), RestartPolicy::default()).with_handle(handle.clone());
    let handler = ControlHandler::new(registry)
        .with_supervisor(handle)
        .with_privileged_operator_prompt(PrivilegedOperatorPrompt::Enforce)
        .with_supervisor_rescan(supervisor, &path, None);
    handler.forwarding.operator_confirms().configure(
        Arc::new(ApproveOperator),
        crate::operator_confirm::OperatorLimits::default(),
    );
    let (ctx, _rx) = route_ctx(ConnectionId::new(105));
    for preview in [true, false] {
        std::fs::write(
            &path,
            r#"{"version":1,"privileged_operator_prompt":"unknown"}"#,
        )
        .unwrap();
        let request = ClientControlRequest::SupervisorRescan { preview };
        let frame = Frame::build(
            FrameType::Request,
            control_flags(),
            0,
            0,
            1,
            serde_json::to_vec(&request).unwrap(),
        )
        .unwrap();
        let frames = handler.handle_control_frame(&ctx, frame).await.unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(parse_error(&frames[0])["code"], "invalid_daemon_config");
        assert_eq!(
            handler.privileged_operator_prompt,
            PrivilegedOperatorPrompt::Enforce
        );
    }
}

/// One wire code has several senders, so the refusal line names the check
/// that refused. This drives the shared refusal path for ordinary refusals
/// with an unregistered
/// target and requires the branch label on the event.
#[tokio::test]
async fn route_open_refusal_names_the_check_that_refused() {
    let handler = ControlHandler::new(Arc::new(Registry::default()));
    let capture = EventCapture::default();
    let _subscriber =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(capture.clone()));
    let (ctx, _rx) = route_ctx(ConnectionId::new(95));
    let response = handler
        .handle_control_frame(
            &ctx,
            route_open_frame(395, "nobody", unique_project_root("refusal-reason")),
        )
        .await
        .unwrap();

    assert_eq!(parse_error(&response[0])["code"], "unknown_module");
    let event = capture
        .events()
        .into_iter()
        .find(|event| {
            event.target == "control"
                && event.fields.get("code") == Some(&"\"unknown_module\"".to_string())
        })
        .expect("route.open refusal event");
    assert_eq!(
        event.fields.get("reason"),
        Some(&"\"not_registered\"".to_string())
    );
}

#[tokio::test]
async fn route_open_supervised_absence_emits_refusal_fields_and_counts_code() {
    let registry = Arc::new(Registry::default());
    let supervisor_handle = SupervisorHandle::new();
    let supervisor =
        Supervisor::new_for_test(Arc::clone(&registry), RestartPolicy::new(0, Duration::ZERO))
            .with_handle(supervisor_handle.clone())
            .with_connection_file_path(std::env::temp_dir().join(format!(
                "subc-route-open-refusal-info-{}",
                std::process::id()
            )));
    let module = supervisor
        .supervise_configured(
            ModuleSpec {
                script: None,
                carry_containment: false,
                module_id: "warming".to_string(),
                program: fake_aft_stub_path(),
                args: Vec::new(),
                env: Vec::new(),
                reserved: false,
                reserved_prefixes: Vec::new(),
                protocol: ModuleProtocol::Subc,
                overlap: Default::default(),
            },
            true,
        )
        .unwrap();
    assert_eq!(module.state().unwrap(), ModuleState::Running);

    let handler = ControlHandler::new(Arc::clone(&registry)).with_supervisor(supervisor_handle);
    assert!(handler
        .counters()
        .snapshot()
        .get("route_open_refused_by_code")
        .is_none());
    let capture = EventCapture::default();
    let _subscriber =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(capture.clone()));
    let (ctx, _rx) = route_ctx(ConnectionId::new(94));
    let response = handler
        .handle_control_frame(
            &ctx,
            route_open_frame(394, "warming", unique_project_root("refusal-info")),
        )
        .await
        .unwrap();
    module.stop().await.unwrap();

    assert_eq!(parse_error(&response[0])["code"], "module_warming");
    let event = capture
        .events()
        .into_iter()
        .find(|event| {
            event.target == "control"
                && event.fields.get("code") == Some(&"\"module_warming\"".to_string())
        })
        .expect("route.open refusal event");
    assert_eq!(
        event.fields.get("module_id"),
        Some(&"\"warming\"".to_string())
    );
    assert_eq!(event.fields.get("connection_id"), Some(&"94".to_string()));
    assert_eq!(
        event.fields.get("reason"),
        Some(&"\"supervised_not_registered\"".to_string())
    );
    assert_eq!(event.fields.get("state"), Some(&"running".to_string()));
    assert_eq!(event.fields.get("enabled"), Some(&"true".to_string()));
    assert_eq!(event.fields.get("live"), Some(&"false".to_string()));
    assert_eq!(event.fields.get("principal"), Some(&"direct".to_string()));
    assert_eq!(event.fields.get("scope"), Some(&"none".to_string()));
    assert_eq!(event.fields.get("harness"), Some(&"\"unit\"".to_string()));
    assert_eq!(
        handler.counters().snapshot()["route_open_refused_by_code"],
        json!({ "module_warming": 1 })
    );
}

const OUTAGE_START: &str = "route.open refusing module: not serving";
const OUTAGE_RECOVERED: &str = "route.open accepted again after module outage";

fn outage_lines(capture: &EventCapture, message: &str) -> Vec<CapturedEvent> {
    capture
        .events()
        .into_iter()
        .filter(|event| event.fields.get("message").map(String::as_str) == Some(message))
        .collect()
}

fn supervise_stub(
    registry: &Arc<Registry>,
    module_id: &str,
    enabled: bool,
) -> (SupervisorHandle, crate::supervise::SupervisedModule) {
    let supervisor_handle = SupervisorHandle::new();
    let supervisor =
        Supervisor::new_for_test(Arc::clone(registry), RestartPolicy::new(0, Duration::ZERO))
            .with_handle(supervisor_handle.clone())
            .with_connection_file_path(std::env::temp_dir().join(format!(
                "subc-route-outage-{module_id}-{}",
                std::process::id()
            )));
    let module = supervisor
        .supervise_configured(
            ModuleSpec {
                script: None,
                carry_containment: false,
                module_id: module_id.to_string(),
                program: fake_aft_stub_path(),
                args: Vec::new(),
                env: Vec::new(),
                reserved: false,
                reserved_prefixes: Vec::new(),
                protocol: ModuleProtocol::Subc,
                overlap: Default::default(),
            },
            enabled,
        )
        .unwrap();
    (supervisor_handle, module)
}

fn supervisor_restart_frame(corr: u64, module_id: &str) -> Frame {
    let body = serde_json::to_vec(&ClientControlRequest::SupervisorRestart {
        module_id: module_id.to_string(),
        drain_timeout_ms: Some(50),
    })
    .unwrap();
    Frame::build(FrameType::Request, control_flags(), 0, 0, corr, body).unwrap()
}

/// Two handlers built over one forwarding table must share one outage
/// tracker; separate trackers would each log their own opening line for
/// the same outage.
#[test]
fn handlers_over_one_forwarding_table_share_the_outage_tracker() {
    let registry = Arc::new(Registry::default());
    let forwarding = Arc::new(ForwardingTable::default());
    let first = ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding));
    let second = ControlHandler::with_forwarding(registry, forwarding);
    assert!(Arc::ptr_eq(&first.route_outages, &second.route_outages));
}

/// A client can name any module id it likes. Refusing an unknown one,
/// however often, must not create outage state or outage lines, or the
/// tracker would be a memory sink any client could fill.
#[tokio::test(flavor = "current_thread")]
async fn route_open_unknown_module_refusals_add_no_outage_state() {
    let handler = ControlHandler::new(Arc::new(Registry::default()));
    let capture = EventCapture::default();
    let _subscriber =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(capture.clone()));
    let (ctx, _rx) = route_ctx(ConnectionId::new(90));
    for corr in 0..8 {
        let response = handler
            .handle_control_frame(
                &ctx,
                route_open_frame(
                    380 + corr,
                    &format!("nobody-{corr}"),
                    unique_project_root("outage-unknown"),
                ),
            )
            .await
            .unwrap();
        assert_eq!(parse_error(&response[0])["code"], "unknown_module");
    }

    assert_eq!(handler.route_outages.tracked_module_count(), 0);
    assert!(outage_lines(&capture, OUTAGE_START).is_empty());
    assert!(outage_lines(&capture, OUTAGE_RECOVERED).is_empty());
}

/// Drives the refusal path end to end: a supervised module that served
/// before and stopped being registered with no instruction to stop is a
/// WARN, and the same module refused after an operator `supervisor.restart`
/// is an INFO.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn route_open_outage_level_separates_operator_restart_from_unexplained() {
    let registry = Arc::new(Registry::default());
    let (supervisor_handle, module) = supervise_stub(&registry, "outage-restart", true);
    let handler = ControlHandler::new(Arc::clone(&registry)).with_supervisor(supervisor_handle);
    let capture = EventCapture::default();
    let _subscriber =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(capture.clone()));
    let (ctx, _rx) = route_ctx(ConnectionId::new(91));
    // The stub never registers, so pretend it served once: otherwise every
    // refusal would fall in its startup window.
    handler.route_outages.record_accepted("outage-restart");

    let response = handler
        .handle_control_frame(
            &ctx,
            route_open_frame(391, "outage-restart", unique_project_root("outage-a")),
        )
        .await
        .unwrap();
    assert_eq!(response[0].header.ty, FrameType::Error);
    let starts = outage_lines(&capture, OUTAGE_START);
    assert_eq!(starts.len(), 1, "{starts:?}");
    assert_eq!(starts[0].level, tracing::Level::WARN);
    assert_eq!(starts[0].fields["initiated_by"], "\"unexplained\"");
    assert_eq!(starts[0].fields["reason"], "\"supervised_not_registered\"");
    assert_eq!(starts[0].fields["module_id"], "\"outage-restart\"");
    handler.route_outages.record_accepted("outage-restart");
    assert_eq!(outage_lines(&capture, OUTAGE_RECOVERED).len(), 1);

    // The missing connection file makes the stub exit immediately. Restart
    // is deliberately deferred while that exit is being recorded; Failed
    // means the zero restart budget and terminal record have been published.
    tokio::time::timeout(Duration::from_secs(10), async {
        while module.state().unwrap() != crate::ModuleState::Failed {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("stub exit must be recorded before the operator restart");

    let restart = handler
        .handle_control_frame(&ctx, supervisor_restart_frame(392, "outage-restart"))
        .await
        .unwrap();
    assert_eq!(
        restart[0].header.ty,
        FrameType::Response,
        "{:?}",
        parse_error(&restart[0])
    );
    handler
        .handle_control_frame(
            &ctx,
            route_open_frame(393, "outage-restart", unique_project_root("outage-b")),
        )
        .await
        .unwrap();
    module.stop().await.unwrap();

    let starts = outage_lines(&capture, OUTAGE_START);
    assert_eq!(starts.len(), 2, "{starts:?}");
    assert_eq!(starts[1].level, tracing::Level::INFO);
    assert_eq!(starts[1].fields["initiated_by"], "\"operator\"");
}

/// A restart refused before it touched the module (here: the module is
/// disabled) must clear its operator mark, so the next real outage is
/// still reported as a warning.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_operator_restart_leaves_no_operator_mark() {
    let registry = Arc::new(Registry::default());
    let (supervisor_handle, _module) = supervise_stub(&registry, "outage-disabled", false);
    let handler = ControlHandler::new(Arc::clone(&registry)).with_supervisor(supervisor_handle);
    let capture = EventCapture::default();
    let _subscriber =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(capture.clone()));
    let (ctx, _rx) = route_ctx(ConnectionId::new(92));
    handler.route_outages.record_accepted("outage-disabled");

    let restart = handler
        .handle_control_frame(&ctx, supervisor_restart_frame(394, "outage-disabled"))
        .await
        .unwrap();
    assert_eq!(parse_error(&restart[0])["code"], "module_disabled");
    assert!(!handler.route_outages.has_operator_mark("outage-disabled"));

    handler
        .handle_control_frame(
            &ctx,
            route_open_frame(395, "outage-disabled", unique_project_root("outage-c")),
        )
        .await
        .unwrap();
    let starts = outage_lines(&capture, OUTAGE_START);
    assert_eq!(starts.len(), 1, "{starts:?}");
    assert_eq!(starts[0].level, tracing::Level::WARN);
}

#[tokio::test(flavor = "current_thread")]
async fn route_open_unknown_module_escapes_target_module_id() {
    let handler = ControlHandler::new(Arc::new(Registry::default()));
    let capture = EventCapture::default();
    let _subscriber =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(capture.clone()));
    let hostile_module_id = "\u{1b}]52;c;AAAA\u{07}";
    let (ctx, _rx) = route_ctx(ConnectionId::new(95));
    let response = handler
        .handle_control_frame(
            &ctx,
            route_open_frame(
                395,
                hostile_module_id,
                unique_project_root("hostile-target-module-id"),
            ),
        )
        .await
        .unwrap();

    assert_eq!(parse_error(&response[0])["code"], "unknown_module");
    let event = capture
        .events()
        .into_iter()
        .find(|event| {
            event.target == "control"
                && event.fields.get("code") == Some(&"\"unknown_module\"".to_string())
        })
        .expect("route.open unknown-module refusal event");
    let logged = event.fields.get("module_id").expect("module_id field");
    assert!(!logged.bytes().any(|byte| byte < 0x20));
    assert_eq!(logged, r#""\u{1b}]52;c;AAAA\u{7}""#);
}

/// A route.open refusal is the daemon's own answer, so its ERROR carries
/// the daemon-origin bit on the frame the client receives.
#[tokio::test(flavor = "current_thread")]
async fn route_open_refusal_carries_daemon_origin() {
    let handler = ControlHandler::new(Arc::new(Registry::default()));
    let (ctx, _rx) = route_ctx(ConnectionId::new(95));
    let response = handler
        .handle_control_frame(
            &ctx,
            route_open_frame(395, "nobody", unique_project_root("refusal-origin")),
        )
        .await
        .unwrap();

    assert_eq!(response.len(), 1);
    assert_eq!(response[0].header.ty, FrameType::Error);
    assert_eq!(parse_error(&response[0])["code"], "unknown_module");
    assert!(
        response[0].header.flags.is_daemon_origin(),
        "route.open refusal must carry daemon origin: {:?}",
        response[0].header.flags
    );
}

/// The drain `module_reloading` refusal is daemon-synthesized for an
/// endpoint that is reloading, so it carries the daemon-origin bit on the
/// frame the client receives.
#[tokio::test(flavor = "current_thread")]
async fn route_open_to_draining_module_refuses_module_reloading_with_daemon_origin() {
    let registry = Arc::new(Registry::default());
    let forwarding = Arc::new(ForwardingTable::default());
    let handler = ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding));
    let module_connection = ConnectionId::new(95);
    let (module_ctx, mut module_rx) = route_ctx(module_connection);
    hello_via_sink(
        &handler,
        &module_ctx,
        &mut module_rx,
        hello_frame("aft", PROTOCOL_VERSION, 395),
    )
    .await;
    forwarding
        .begin_module_drain("aft", RouteCloseReason::Restart)
        .unwrap()
        .expect("the module endpoint drains");

    let (client_ctx, _client_rx) = route_ctx(ConnectionId::new(96));
    let response = handler
        .handle_control_frame(
            &client_ctx,
            route_open_frame(396, "aft", unique_project_root("drain-origin")),
        )
        .await
        .unwrap();

    assert_eq!(response.len(), 1);
    assert_eq!(response[0].header.ty, FrameType::Error);
    assert_eq!(parse_error(&response[0])["code"], "module_reloading");
    assert!(
        response[0].header.flags.is_daemon_origin(),
        "drain module_reloading must carry daemon origin: {:?}",
        response[0].header.flags
    );
}

/// A module that refuses a bind with a daemon code still cannot mint a
/// daemon refusal: the relayed `module_reloading` body reaches the client
/// with the daemon-origin bit clear.
#[tokio::test(flavor = "current_thread")]
async fn route_open_module_rejection_with_daemon_code_carries_no_daemon_origin() {
    let registry = Arc::new(Registry::default());
    let forwarding = Arc::new(ForwardingTable::default());
    let handler = ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding));
    let module_connection = ConnectionId::new(95);
    let (module_ctx, mut module_rx) = route_ctx(module_connection);
    hello_via_sink(
        &handler,
        &module_ctx,
        &mut module_rx,
        hello_frame("aft", PROTOCOL_VERSION, 395),
    )
    .await;

    let client_connection = ConnectionId::new(96);
    let (client_ctx, _client_rx) = route_ctx(client_connection);
    let (route_task, bind) = relay_route_open(
        &handler,
        client_connection,
        &client_ctx.egress,
        &mut module_rx,
        396,
        "aft",
        "spoofed-reloading",
    )
    .await;
    // The module sets the origin bit on its own rejection too: neither
    // the bit nor the daemon code may survive the relay as daemon-owned.
    let rejection = Frame::build(
        FrameType::Error,
        crate::router::daemon_error_flags(),
        0,
        0,
        bind.header.corr,
        serde_json::to_vec(&ErrorBody::new(
            "module_reloading",
            "module refused route.bind",
        ))
        .unwrap(),
    )
    .unwrap();
    handler
        .handle_control_frame(&module_ctx, rejection)
        .await
        .unwrap();

    let response = route_task.await.unwrap();
    assert_eq!(response.len(), 1);
    assert_eq!(response[0].header.ty, FrameType::Error);
    assert_eq!(parse_error(&response[0])["code"], "module_reloading");
    assert!(
        !response[0].header.flags.is_daemon_origin(),
        "a module's own rejection must not carry daemon origin: {:?}",
        response[0].header.flags
    );
}

#[tokio::test(flavor = "current_thread")]
async fn route_open_module_rejection_uses_daemon_counter_key() {
    let registry = Arc::new(Registry::default());
    let forwarding = Arc::new(ForwardingTable::default());
    let handler = ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding));
    let module_connection = ConnectionId::new(95);
    let (module_ctx, mut module_rx) = route_ctx(module_connection);
    hello_via_sink(
        &handler,
        &module_ctx,
        &mut module_rx,
        hello_frame("aft", PROTOCOL_VERSION, 395),
    )
    .await;

    let client_connection = ConnectionId::new(96);
    let (client_ctx, _client_rx) = route_ctx(client_connection);
    let capture = EventCapture::default();
    // tracing-core decides once per callsite whether its events are enabled,
    // and caches the answer. If a parallel test with no subscriber reaches
    // this callsite first, the cached answer is "disabled" and this test's
    // capture sees nothing. Keeping a second dispatch alive makes tracing-core
    // ask every live subscriber instead, while only this test's default
    // receives the events.
    static INTEREST_ANCHOR: std::sync::OnceLock<tracing::Dispatch> = std::sync::OnceLock::new();
    INTEREST_ANCHOR.get_or_init(|| tracing::Dispatch::new(tracing_subscriber::registry()));
    let _subscriber =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(capture.clone()));
    let (route_task, bind) = relay_route_open(
        &handler,
        client_connection,
        &client_ctx.egress,
        &mut module_rx,
        396,
        "aft",
        "hostile-module-code",
    )
    .await;
    let hostile_code = "\u{1b}]52;c;AAAA\u{07}";
    let rejection = Frame::build(
        FrameType::Error,
        control_flags(),
        0,
        0,
        bind.header.corr,
        serde_json::to_vec(&ErrorBody::new(hostile_code, "module refused route.bind")).unwrap(),
    )
    .unwrap();
    handler
        .handle_control_frame(&module_ctx, rejection)
        .await
        .unwrap();

    let response = route_task.await.unwrap();
    assert_eq!(parse_error(&response[0])["code"], hostile_code);
    let counters = handler.counters().snapshot();
    assert_eq!(
        counters["route_open_refused_by_code"],
        json!({ "module_rejected": 1 })
    );
    assert!(counters["route_open_refused_by_code"]
        .get(hostile_code)
        .is_none());

    let event = capture
        .events()
        .into_iter()
        .find(|event| {
            event.target == "control"
                && event.fields.get("code") == Some(&"\"module_rejected\"".to_string())
        })
        .expect("route.open module-rejection refusal event");
    let logged = event.fields.get("module_code").expect("module_code field");
    assert!(!logged.bytes().any(|byte| byte < 0x20));
    assert_eq!(logged, r#""\u{1b}]52;c;AAAA\u{7}""#);
}

#[tokio::test]
async fn route_open_keeps_failed_unregistered_supervised_module_unavailable() {
    let registry = Arc::new(Registry::default());
    let supervisor_handle = SupervisorHandle::new();
    let missing_program = std::env::temp_dir().join(format!(
        "subc-route-open-missing-program-{}",
        std::process::id()
    ));
    let supervisor =
        Supervisor::new_for_test(Arc::clone(&registry), RestartPolicy::new(0, Duration::ZERO))
            .with_handle(supervisor_handle.clone());
    let module = supervisor
        .supervise_configured(
            ModuleSpec {
                script: None,
                carry_containment: false,
                module_id: "failed".to_string(),
                program: missing_program,
                args: Vec::new(),
                env: Vec::new(),
                reserved: false,
                reserved_prefixes: Vec::new(),
                protocol: ModuleProtocol::Subc,
                overlap: Default::default(),
            },
            true,
        )
        .unwrap();
    assert_eq!(module.state().unwrap(), ModuleState::Failed);

    let handler = ControlHandler::new(Arc::clone(&registry)).with_supervisor(supervisor_handle);
    let (ctx, _rx) = route_ctx(ConnectionId::new(40));
    let response = handler
        .handle_control_frame(
            &ctx,
            route_open_frame(305, "failed", unique_project_root("failed")),
        )
        .await
        .unwrap();

    assert_eq!(response[0].header.ty, FrameType::Error);
    let error = parse_error(&response[0]);
    assert_eq!(error["code"], "target_unavailable");
    assert!(error["message"]
        .as_str()
        .unwrap()
        .contains("state=failed, enabled=true, live=false"));
}

#[tokio::test]
async fn route_open_role_mismatch_remains_target_unavailable() {
    let registry = Arc::new(Registry::default());
    let handler = ControlHandler::new(Arc::clone(&registry));
    handler
        .handle_control(
            ConnectionId::new(41),
            non_routable_hello_frame_with_control_ops("health-only", 306, None),
        )
        .unwrap();

    let (ctx, _rx) = route_ctx(ConnectionId::new(42));
    let response = handler
        .handle_control_frame(
            &ctx,
            route_open_frame(307, "health-only", unique_project_root("role-mismatch")),
        )
        .await
        .unwrap();

    assert_eq!(parse_error(&response[0])["code"], "target_unavailable");
    assert!(parse_error(&response[0])["message"]
        .as_str()
        .unwrap()
        .contains("does not provide the requested target"));
}

#[tokio::test]
async fn route_open_inactive_registration_remains_target_unavailable() {
    let registry = Arc::new(Registry::default());
    let handler = ControlHandler::new(Arc::clone(&registry));
    handler
        .handle_control(
            ConnectionId::new(43),
            hello_frame("inactive", PROTOCOL_VERSION, 308),
        )
        .unwrap();
    assert!(registry
        .set_module_state_for_test("inactive", ChannelState::Closed)
        .unwrap());

    let (ctx, _rx) = route_ctx(ConnectionId::new(44));
    let response = handler
        .handle_control_frame(
            &ctx,
            route_open_frame(309, "inactive", unique_project_root("inactive")),
        )
        .await
        .unwrap();

    assert_eq!(parse_error(&response[0])["code"], "target_unavailable");
    assert!(parse_error(&response[0])["message"]
        .as_str()
        .unwrap()
        .contains("is not active"));
}

#[tokio::test]
async fn late_health_reply_is_recorded_through_the_module_response_path() {
    let registry = Arc::new(Registry::default());
    let forwarding = Arc::new(ForwardingTable::default());
    let supervisor_handle = SupervisorHandle::new();
    let supervisor =
        Supervisor::new_for_test(Arc::clone(&registry), crate::RestartPolicy::default())
            .with_forwarding(Arc::clone(&forwarding))
            .with_handle(supervisor_handle.clone());
    let module = supervisor
        .supervise_configured(
            crate::ModuleSpec {
                script: None,
                carry_containment: false,
                module_id: "late-health-response".to_string(),
                program: PathBuf::from("disabled-module"),
                args: Vec::new(),
                env: Vec::new(),
                reserved: false,
                reserved_prefixes: Vec::new(),
                protocol: ModuleProtocol::Subc,
                overlap: Default::default(),
            },
            false,
        )
        .unwrap();
    let handler = ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding))
        .with_supervisor(supervisor_handle);
    let (module_ctx, _module_rx) = route_ctx(ConnectionId::new(39));
    handler
        .handle_control_frame(
            &module_ctx,
            hello_frame_with_control_ops(
                "late-health-response",
                PROTOCOL_VERSION,
                7,
                Some(vec![MODULE_CONTROL_OP_HEALTH_CHECK.to_string()]),
            ),
        )
        .await
        .unwrap();
    let probe_started_at = Instant::now() - Duration::from_millis(80);
    let pending = forwarding
        .begin_health_probe_rpc_for(
            "late-health-response",
            MODULE_CONTROL_OP_HEALTH_CHECK,
            probe_started_at,
            Instant::now() - Duration::from_millis(1),
        )
        .unwrap();
    assert!(forwarding
        .tombstone_health_probe_rpc(pending.endpoint, pending.corr)
        .unwrap());

    let responses = handler
        .handle_control_frame(&module_ctx, health_response(pending.corr, HealthStatus::Ok))
        .await
        .unwrap();

    assert!(responses.is_empty());
    let health = module.status().unwrap().health;
    assert_eq!(health.late_answer_count, 1);
    assert!(health.last_late_answer_latency_ms.unwrap() >= 80);
}

#[tokio::test]
async fn health_probe_timeout_and_module_death_are_typed() {
    let registry = Arc::new(Registry::default());
    let forwarding = Arc::new(ForwardingTable::default());
    let handler = ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding))
        .with_health_probe_timeout(Duration::from_millis(50));
    let (module_ctx, mut module_rx) = route_ctx(ConnectionId::new(40));
    hello_via_sink(
        &handler,
        &module_ctx,
        &mut module_rx,
        hello_frame_with_control_ops(
            "aft",
            PROTOCOL_VERSION,
            7,
            Some(vec![MODULE_CONTROL_OP_HEALTH_CHECK.to_string()]),
        ),
    )
    .await;

    let (client_ctx, _client_rx) = route_ctx(ConnectionId::new(41));
    let responses = handler
        .handle_control_frame(&client_ctx, supervisor_health_probe_frame(201, "aft"))
        .await
        .unwrap();
    assert_eq!(responses[0].header.ty, FrameType::Error);
    assert_eq!(parse_error(&responses[0])["code"], "module_timeout");
    let _ = module_rx.try_recv();

    let (client_ctx, _client_rx) = route_ctx(ConnectionId::new(42));
    let health_handler = handler.clone();
    let death_task = tokio::spawn(async move {
        health_handler
            .handle_control_frame(&client_ctx, supervisor_health_probe_frame(202, "aft"))
            .await
            .unwrap()
    });
    tokio::time::timeout(Duration::from_secs(1), module_rx.recv())
        .await
        .unwrap()
        .unwrap();
    handler
        .cleanup_connection(module_ctx.connection_id)
        .unwrap();
    let responses = death_task.await.unwrap();
    assert_eq!(responses[0].header.ty, FrameType::Error);
    assert_eq!(parse_error(&responses[0])["code"], "target_unavailable");
}

#[test]
fn hello_requires_exact_protocol_version() {
    for (connection, offered) in [(1, PROTOCOL_VERSION - 1), (2, PROTOCOL_VERSION + 1)] {
        let registry = Arc::new(Registry::default());
        let handler = ControlHandler::new(Arc::clone(&registry));
        let responses = handler
            .handle_control(
                ConnectionId::new(connection),
                hello_frame("aft", offered, 9),
            )
            .unwrap();

        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0].header.ty, FrameType::Error);
        let error = parse_error(&responses[0]);
        assert_eq!(error["code"], "version_unsupported");
        assert!(registry.get_module("aft").unwrap().is_none());
        assert_eq!(registry.active_registration_count().unwrap(), 0);
    }
}

#[test]
fn unknown_module_push_op_is_ignored_but_malformed_known_op_errors() {
    let registry = Arc::new(Registry::default());
    let forwarding = Arc::new(ForwardingTable::default());
    let handler = ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding));
    let module_connection = ConnectionId::new(301);
    let registration = registry
        .register_with_control_ops(
            manifest("aft-push", PROTOCOL_VERSION),
            PROTOCOL_VERSION,
            module_connection,
            module_baseline_control_ops(),
        )
        .unwrap();
    let (module_tx, _module_rx) = mpsc::channel(8);
    let endpoint = forwarding
        .register_module_connection(
            module_connection,
            "aft-push".to_string(),
            PROTOCOL_VERSION,
            manifest_concurrency(&registration.manifest),
            FrameSink::new(module_tx),
        )
        .unwrap();

    // A push op this version does not know is ignored (forward-compat), not errored.
    let unknown = Frame::build(
        FrameType::Push,
        control_flags(),
        0,
        0,
        5,
        serde_json::to_vec(&json!({"op": "route.future.v2", "extra": 1})).unwrap(),
    )
    .unwrap();
    let out = handler.handle_status_update(endpoint, unknown).unwrap();
    assert!(
        out.is_empty(),
        "unknown push op must be ignored, got {out:?}"
    );

    // A malformed body for a KNOWN op is a real error worth surfacing.
    let malformed = Frame::build(
        FrameType::Push,
        control_flags(),
        0,
        0,
        6,
        serde_json::to_vec(&json!({"op": "route.status"})).unwrap(),
    )
    .unwrap();
    let out = handler.handle_status_update(endpoint, malformed).unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].header.ty, FrameType::Error);
    assert_eq!(parse_error(&out[0])["code"], "invalid_control_body");
}

#[test]
fn hello_rejected_when_connection_already_owns_client_routes() {
    let registry = Arc::new(Registry::default());
    let forwarding = Arc::new(ForwardingTable::default());
    let handler = ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding));
    // Commits a client route on connection 202 (bound to a module on conn 101).
    let _ = bind_liveness_route(&registry, &forwarding, "aft-module");
    let client_connection = ConnectionId::new(202);

    // That same connection now tries to register as a module: rejected, so one
    // connection never holds both client-route and module-endpoint state.
    let responses = handler
        .handle_control(
            client_connection,
            hello_frame("aft-second", PROTOCOL_VERSION, 9),
        )
        .unwrap();
    assert_eq!(responses[0].header.ty, FrameType::Error);
    assert_eq!(parse_error(&responses[0])["code"], "invalid_hello");
    assert!(registry.get_module("aft-second").unwrap().is_none());
}

#[tokio::test]
async fn second_hello_preserves_registration_routes_and_launch_nonce() {
    let registry = Arc::new(Registry::default());
    let forwarding = Arc::new(ForwardingTable::default());
    let handler = ControlHandler::with_forwarding(registry.clone(), forwarding.clone());
    let (module_ctx, mut module_rx) = route_ctx(ConnectionId::new(101));
    hello_via_sink(
        &handler,
        &module_ctx,
        &mut module_rx,
        hello_frame_with_nonce("alpha", PROTOCOL_VERSION, 1, Some("alpha-nonce")),
    )
    .await;
    let (client_ctx, mut client_rx) = route_ctx(ConnectionId::new(202));
    let pending = forwarding
        .begin_route_bind_relay_for_test(
            client_ctx.connection_id,
            client_ctx.egress.clone(),
            2,
            "alpha",
        )
        .unwrap();
    forwarding
        .complete_pending_relay(
            module_ctx.connection_id,
            pending.corr,
            RouteBindRelayOutcome::Accepted,
        )
        .unwrap();
    client_rx.try_recv().unwrap();
    for module_id in ["beta", "alpha"] {
        let replies = handler
            .handle_control_frame(
                &module_ctx,
                hello_frame_with_nonce(module_id, PROTOCOL_VERSION, 3, Some("replacement")),
            )
            .await
            .unwrap();
        assert_eq!(replies.len(), 1, "second HELLO must be refused");
        assert_eq!(parse_error(&replies[0])["code"], "invalid_hello");
    }
    assert_eq!(registry.list_modules().unwrap().1.len(), 1);
    assert!(registry.get_module("beta").unwrap().is_none());
    assert!(matches!(
        forwarding
            .lookup_data_route(
                client_ctx.connection_id,
                pending.client_channel,
                pending.client_epoch,
            )
            .unwrap(),
        DataRoute::Client(DataRouteState::Bound(_))
    ));
    assert!(handler
        .hello_launch_nonces
        .lock()
        .unwrap()
        .presented(module_ctx.connection_id, Some("alpha-nonce")));
    assert!(module_rx.try_recv().is_err());
}

#[test]
fn reserved_module_hello_requires_matching_launch_nonce() {
    let registry = Arc::new(Registry::default());
    let supervisor = SupervisorHandle::new();
    // The supervisor recorded the nonce it injected when it spawned the reserved
    // module; the HELLO verifier checks against the same shared handle.
    supervisor.set_reserved_nonce("vault", "the-real-nonce".to_string());
    let handler = ControlHandler::new(Arc::clone(&registry)).with_supervisor(supervisor);

    // A HELLO with NO nonce is rejected.
    let no_nonce = handler
        .handle_control(
            ConnectionId::new(1),
            hello_frame("vault", PROTOCOL_VERSION, 1),
        )
        .unwrap();
    assert_eq!(no_nonce[0].header.ty, FrameType::Error);
    assert_eq!(parse_error(&no_nonce[0])["code"], "reserved_module");
    assert!(registry.get_module("vault").unwrap().is_none());

    // A HELLO with the WRONG nonce is rejected.
    let wrong = handler
        .handle_control(
            ConnectionId::new(2),
            hello_frame_with_nonce("vault", PROTOCOL_VERSION, 2, Some("forged")),
        )
        .unwrap();
    assert_eq!(wrong[0].header.ty, FrameType::Error);
    assert_eq!(parse_error(&wrong[0])["code"], "reserved_module");
    assert!(registry.get_module("vault").unwrap().is_none());

    // A HELLO with the CORRECT nonce registers.
    let ok = handler
        .handle_control(
            ConnectionId::new(3),
            hello_frame_with_nonce("vault", PROTOCOL_VERSION, 3, Some("the-real-nonce")),
        )
        .unwrap();
    assert_eq!(ok[0].header.ty, FrameType::HelloAck);
    assert!(registry.get_module("vault").unwrap().is_some());
}

#[test]
fn reserved_prefix_hello_uses_delimiter_sensitive_owner_nonce() {
    let registry = Arc::new(Registry::default());
    let supervisor = SupervisorHandle::new();
    supervisor.set_spawn_nonce("federation", "owner-nonce".to_string());
    supervisor.set_reserved_prefixes("federation", &["fed:".to_string()]);
    let handler = ControlHandler::new(Arc::clone(&registry)).with_supervisor(supervisor);

    let squat = handler
        .handle_control(
            ConnectionId::new(1),
            hello_frame("fed:peerA:tool", PROTOCOL_VERSION, 1),
        )
        .unwrap();
    assert_eq!(squat[0].header.ty, FrameType::Error);
    assert_eq!(parse_error(&squat[0])["code"], "reserved_module");
    assert!(parse_error(&squat[0])["message"]
        .as_str()
        .unwrap()
        .contains("fed:"));

    let accepted_peer = handler
        .handle_control(
            ConnectionId::new(2),
            hello_frame_with_nonce("fed:peerA:tool", PROTOCOL_VERSION, 2, Some("owner-nonce")),
        )
        .unwrap();
    assert_eq!(accepted_peer[0].header.ty, FrameType::HelloAck);

    let accepted_short = handler
        .handle_control(
            ConnectionId::new(3),
            hello_frame_with_nonce("fed:x", PROTOCOL_VERSION, 3, Some("owner-nonce")),
        )
        .unwrap();
    assert_eq!(accepted_short[0].header.ty, FrameType::HelloAck);

    for (conn, module_id) in [(4, "fedx:tool"), (5, "fed"), (6, "FED:x")] {
        let response = handler
            .handle_control(
                ConnectionId::new(conn),
                hello_frame(module_id, PROTOCOL_VERSION, conn),
            )
            .unwrap();
        assert_eq!(response[0].header.ty, FrameType::HelloAck, "{module_id}");
    }
}

#[test]
fn exact_reserved_module_takes_precedence_over_reserved_prefix() {
    let registry = Arc::new(Registry::default());
    let supervisor = SupervisorHandle::new();
    supervisor.set_spawn_nonce("federation", "owner-nonce".to_string());
    supervisor.set_reserved_prefixes("federation", &["fed:".to_string()]);
    supervisor.set_reserved_nonce("fed:special", "exact-nonce".to_string());
    let handler = ControlHandler::new(Arc::clone(&registry)).with_supervisor(supervisor);

    let owner_nonce = handler
        .handle_control(
            ConnectionId::new(1),
            hello_frame_with_nonce("fed:special", PROTOCOL_VERSION, 1, Some("owner-nonce")),
        )
        .unwrap();
    assert_eq!(owner_nonce[0].header.ty, FrameType::Error);
    assert_eq!(parse_error(&owner_nonce[0])["code"], "reserved_module");
    assert!(registry.get_module("fed:special").unwrap().is_none());

    let exact_nonce = handler
        .handle_control(
            ConnectionId::new(2),
            hello_frame_with_nonce("fed:special", PROTOCOL_VERSION, 2, Some("exact-nonce")),
        )
        .unwrap();
    assert_eq!(exact_nonce[0].header.ty, FrameType::HelloAck);
    assert!(registry.get_module("fed:special").unwrap().is_some());
}

#[test]
fn non_reserved_module_ignores_launch_nonce() {
    let registry = Arc::new(Registry::default());
    // No reserved nonce recorded for these ids: they are not reserved, so HELLO
    // registration succeeds whether a spawned process echoes a nonce or not.
    let handler = ControlHandler::new(Arc::clone(&registry));
    let no_nonce = handler
        .handle_control(
            ConnectionId::new(1),
            hello_frame("aft-no-nonce", PROTOCOL_VERSION, 1),
        )
        .unwrap();
    assert_eq!(no_nonce[0].header.ty, FrameType::HelloAck);
    assert!(registry.get_module("aft-no-nonce").unwrap().is_some());

    let echoed_nonce = handler
        .handle_control(
            ConnectionId::new(2),
            hello_frame_with_nonce("aft-with-nonce", PROTOCOL_VERSION, 2, Some("spawn-nonce")),
        )
        .unwrap();
    assert_eq!(echoed_nonce[0].header.ty, FrameType::HelloAck);
    assert!(registry.get_module("aft-with-nonce").unwrap().is_some());
}

#[test]
fn malformed_hello_returns_error_and_handler_still_answers_ping() {
    let handler = ControlHandler::default();
    let conn = ConnectionId::new(1);
    let malformed = Frame::build(
        FrameType::Hello,
        control_flags(),
        0,
        0,
        3,
        b"{not json".to_vec(),
    )
    .unwrap();

    let error = handler.handle_control(conn, malformed).unwrap();
    assert_eq!(error[0].header.ty, FrameType::Error);
    assert_eq!(parse_error(&error[0])["code"], "invalid_hello");

    let ping = Frame::build(FrameType::Ping, control_flags(), 0, 0, 4, Vec::new()).unwrap();
    let pong = handler.handle_control(conn, ping).unwrap();
    assert_eq!(pong[0].header.ty, FrameType::Pong);
    assert_eq!(pong[0].header.corr, 4);
}

#[test]
fn duplicate_module_id_is_rejected_without_replacing_active_registration() {
    let registry = Arc::new(Registry::default());
    let handler = ControlHandler::new(Arc::clone(&registry));

    handler
        .handle_control(
            ConnectionId::new(1),
            hello_frame("aft", PROTOCOL_VERSION, 1),
        )
        .unwrap();
    let duplicate = handler
        .handle_control(
            ConnectionId::new(2),
            hello_frame("aft", PROTOCOL_VERSION, 2),
        )
        .unwrap();

    assert_eq!(duplicate[0].header.ty, FrameType::Error);
    assert_eq!(parse_error(&duplicate[0])["code"], "duplicate_module_id");
    let registration = registry.get_module("aft").unwrap().unwrap();
    assert_eq!(registration.connection_id, ConnectionId::new(1));
}

#[test]
fn liveness_poll_reports_false_when_process_liveness_reports_dead() {
    let registry = Arc::new(Registry::default());
    let forwarding = Arc::new(ForwardingTable::default());
    let process_liveness = Arc::new(FakeProcessLiveness { live: Some(false) });
    let handler = ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding))
        .with_process_liveness(process_liveness);
    let (ctx, route_channel, route_epoch) = bind_liveness_route(&registry, &forwarding, "aft-dead");
    let responses = handler
        .handle_route_poll(
            &ctx,
            route_poll_frame(41, PollKind::Liveness, route_channel),
            route_channel,
            route_epoch,
            PollKind::Liveness,
        )
        .unwrap();

    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0].header.ty, FrameType::Response);
    assert_route_poll_liveness(&responses[0], false);
}

#[test]
fn liveness_poll_without_process_source_uses_bound_route() {
    let registry = Arc::new(Registry::default());
    let forwarding = Arc::new(ForwardingTable::default());
    let handler = ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding));
    let (ctx, route_channel, route_epoch) =
        bind_liveness_route(&registry, &forwarding, "aft-bound-only");
    let responses = handler
        .handle_route_poll(
            &ctx,
            route_poll_frame(42, PollKind::Liveness, route_channel),
            route_channel,
            route_epoch,
            PollKind::Liveness,
        )
        .unwrap();

    assert_route_poll_liveness(&responses[0], true);
}

#[test]
fn liveness_poll_untracked_process_source_uses_bound_route() {
    let registry = Arc::new(Registry::default());
    let forwarding = Arc::new(ForwardingTable::default());
    let process_liveness = Arc::new(FakeProcessLiveness { live: None });
    let handler = ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding))
        .with_process_liveness(process_liveness);
    let (ctx, route_channel, route_epoch) =
        bind_liveness_route(&registry, &forwarding, "aft-untracked");
    let responses = handler
        .handle_route_poll(
            &ctx,
            route_poll_frame(43, PollKind::Liveness, route_channel),
            route_channel,
            route_epoch,
            PollKind::Liveness,
        )
        .unwrap();

    assert_route_poll_liveness(&responses[0], true);
}

#[tokio::test]
async fn unknown_op_returns_unknown_control_op() {
    let handler = ControlHandler::default();
    let (ctx, _rx) = route_ctx(ConnectionId::new(77));
    let request = Frame::build(
        FrameType::Request,
        control_flags(),
        0,
        0,
        55,
        br#"{"op":"route.nope","route_channel":1}"#.to_vec(),
    )
    .unwrap();

    let response = handler.handle_control_frame(&ctx, request).await.unwrap();

    assert_eq!(response.len(), 1);
    assert_eq!(response[0].header.ty, FrameType::Error);
    assert_eq!(response[0].header.corr, 55);
    assert_eq!(parse_error(&response[0])["code"], "unknown_control_op");
}

#[tokio::test]
async fn supervisor_provenance_rejects_unknown_exact_module() {
    let handler = ControlHandler::default();
    let (ctx, _rx) = route_ctx(ConnectionId::new(79));
    let request = Frame::build(
        FrameType::Request,
        control_flags(),
        0,
        0,
        57,
        br#"{"op":"supervisor.provenance","module_id":"missing"}"#.to_vec(),
    )
    .unwrap();

    let response = handler.handle_control_frame(&ctx, request).await.unwrap();

    assert_eq!(response.len(), 1);
    assert_eq!(response[0].header.ty, FrameType::Error);
    assert_eq!(response[0].header.corr, 57);
    let error = parse_error(&response[0]);
    assert_eq!(error["code"], "unknown_module");
    assert_eq!(error["message"], "module_id 'missing' is not supervised");
}

#[test]
fn provenance_probe_override_keeps_handler_tests_deterministic() {
    let expected = subc_control::RunningImageAgreement::Unavailable {
        reason: subc_control::RunningImageUnavailableReason::HashFailed,
    };
    let handler = ControlHandler::default().with_provenance_probe_result(expected.clone());
    assert_eq!(handler.provenance_probe_override, Some(expected));
}

#[test]
fn reload_verdict_detects_configured_program_different_from_spawned_path() {
    let verdict = reload_verdict(
        std::path::Path::new("/bin/new"),
        Some(std::path::Path::new("/bin/old")),
        subc_control::RunningImageAgreement::Unavailable {
            reason: subc_control::RunningImageUnavailableReason::HashFailed,
        },
    );
    assert!(matches!(
        verdict.path,
        subc_control::ReloadPathAgreement::Mismatch { configured, spawned_from }
            if configured == std::path::Path::new("/bin/new")
                && spawned_from == std::path::Path::new("/bin/old")
    ));
}

#[test]
fn launch_spec_without_running_digest_is_unavailable_not_match() {
    use subc_control::{LaunchSpecAgreement as Agreement, LaunchSpecUnavailableReason as Reason};
    let configured = crate::supervise::ModuleSpec {
        script: None,
        carry_containment: false,
        module_id: "legacy".into(),
        program: "/module".into(),
        args: Vec::new(),
        env: Vec::new(),
        reserved: false,
        reserved_prefixes: Vec::new(),
        protocol: subc_control::ModuleProtocol::Subc,
        overlap: Default::default(),
    };
    let digest = crate::supervise::LaunchSpecDigest::from_spec(&configured);
    assert_eq!(
        launch_spec_agreement(&configured, false, Some(&digest)),
        Agreement::Unavailable {
            reason: Reason::NotRunning
        }
    );
    assert_eq!(
        launch_spec_agreement(&configured, true, None),
        Agreement::Unavailable {
            reason: Reason::SpawnedSpecUnavailable
        }
    );
    assert_eq!(
        launch_spec_agreement(&configured, true, Some(&digest)),
        Agreement::Match
    );
}

#[cfg(windows)]
#[test]
fn windows_reload_path_matches_configured_and_verbatim_spawn_spellings() {
    let dir = cortexkit_test_support::ScratchDir::new("windows-reload-path-spelling");
    let configured = dir.join("module.exe");
    std::fs::write(&configured, b"executable path fixture").unwrap();
    let spawned = std::fs::canonicalize(&configured).unwrap();
    assert_ne!(
        configured, spawned,
        "fixture must exercise different path spellings"
    );
    let forward_slashes = configured.to_string_lossy().replace('\\', "/");
    let lower_case = configured.to_string_lossy().to_lowercase();
    for spelling in [
        &configured,
        Path::new(&forward_slashes),
        Path::new(&lower_case),
    ] {
        let verdict = reload_verdict(
            spelling,
            Some(&spawned),
            subc_control::RunningImageAgreement::Unavailable {
                reason: subc_control::RunningImageUnavailableReason::ProcessIdentityUnconfirmed,
            },
        );
        assert_eq!(
            verdict.path,
            ReloadPathAgreement::Match,
            "configured {spelling:?}, spawned {spawned:?}"
        );
    }
}

#[cfg(windows)]
#[test]
fn windows_reload_path_keeps_different_existing_program_pending() {
    let dir = cortexkit_test_support::ScratchDir::new("windows-reload-path-change");
    let old = dir.join("old.exe");
    let new = dir.join("new.exe");
    std::fs::write(&old, b"same executable content").unwrap();
    std::fs::copy(&old, &new).unwrap();
    let spawned = std::fs::canonicalize(&old).unwrap();
    let image = subc_control::RunningImageAgreement::Unavailable {
        reason: subc_control::RunningImageUnavailableReason::ProcessIdentityUnconfirmed,
    };
    let verdict = reload_verdict(&new, Some(&spawned), image.clone());
    assert!(
        matches!(verdict.path, ReloadPathAgreement::Mismatch { configured, spawned_from }
            if configured == new && spawned_from == spawned)
    );
    let missing = dir.join("missing.exe");
    assert!(matches!(
        reload_verdict(&missing, Some(&spawned), image).path,
        ReloadPathAgreement::Mismatch { .. }
    ));
}

#[test]
fn reload_verdict_detects_replaced_image_at_same_path() {
    let image = subc_control::RunningImageAgreement::Mismatch {
        running: subc_control::RunningImageEvidence::LinuxProcSha256 {
            digest: "old".into(),
        },
        disk: subc_control::RunningImageEvidence::LinuxProcSha256 {
            digest: "new".into(),
        },
    };
    let verdict = reload_verdict(
        std::path::Path::new("/bin/same"),
        Some(std::path::Path::new("/bin/same")),
        image.clone(),
    );
    assert_eq!(verdict.path, subc_control::ReloadPathAgreement::Match);
    assert_eq!(verdict.image, image);
}

#[test]
fn reload_verdict_preserves_stopped_and_unavailable_reasons() {
    let image = subc_control::RunningImageAgreement::Unavailable {
        reason: subc_control::RunningImageUnavailableReason::NotRunning,
    };
    let verdict = reload_verdict(std::path::Path::new("/bin/same"), None, image.clone());
    assert_eq!(
        verdict.path,
        subc_control::ReloadPathAgreement::Unavailable {
            reason: subc_control::ReloadPathUnavailableReason::NotRunning,
        }
    );
    assert_eq!(verdict.image, image);

    let unconfirmed = subc_control::RunningImageAgreement::Unavailable {
        reason: subc_control::RunningImageUnavailableReason::ProcessIdentityUnconfirmed,
    };
    let verdict = reload_verdict(
        std::path::Path::new("/bin/same"),
        Some(std::path::Path::new("/bin/same")),
        unconfirmed.clone(),
    );
    assert_eq!(verdict.path, subc_control::ReloadPathAgreement::Match);
    assert_eq!(verdict.image, unconfirmed);
}

#[test]
fn reload_verdict_preserves_each_image_unavailability_reason() {
    use subc_control::RunningImageUnavailableReason as Reason;

    for reason in [
        Reason::NotRunning,
        Reason::UnsupportedPlatform,
        Reason::RunningExecutableUnreadable,
        Reason::SpawnedPathUnreadable,
        Reason::HashFailed,
        Reason::ProcessIdentityUnconfirmed,
        Reason::Unknown("future_probe_reason".to_string()),
    ] {
        let image = subc_control::RunningImageAgreement::Unavailable {
            reason: reason.clone(),
        };
        let verdict = reload_verdict(
            std::path::Path::new("/bin/same"),
            Some(std::path::Path::new("/bin/same")),
            image.clone(),
        );
        assert_eq!(verdict.path, subc_control::ReloadPathAgreement::Match);
        assert_eq!(verdict.image, image, "{reason:?}");
    }
}

#[tokio::test]
async fn malformed_control_bodies_return_invalid_control_body() {
    let handler = ControlHandler::default();
    let (ctx, _rx) = route_ctx(ConnectionId::new(78));

    for (corr, body) in [
        (56, br#"{"route_channel":1}"#.as_slice()),
        (57, br#"{"op":17,"route_channel":1}"#.as_slice()),
        (
            58,
            br#"{"op":"route.poll","route_channel":"bad","kind":"status"}"#.as_slice(),
        ),
    ] {
        let request = Frame::build(
            FrameType::Request,
            control_flags(),
            0,
            0,
            corr,
            body.to_vec(),
        )
        .unwrap();
        let response = handler.handle_control_frame(&ctx, request).await.unwrap();

        assert_eq!(response.len(), 1);
        assert_eq!(response[0].header.ty, FrameType::Error);
        assert_eq!(response[0].header.corr, corr);
        assert_eq!(parse_error(&response[0])["code"], "invalid_control_body");
    }
}

#[tokio::test]
async fn goodbye_tears_down_registration_and_later_channel_is_unknown() {
    let (logs, _guard) = crate::router::test_log::log_capture(tracing::Level::INFO);
    let registry = Arc::new(Registry::default());
    let control = Arc::new(ControlHandler::new(Arc::clone(&registry)));
    let router = Router::with_control_handler(Arc::clone(&control));
    let connection = router.begin_connection();
    let (ctx, mut rx) = route_ctx(connection.id());

    router
        .route_for_connection(&ctx, hello_frame("aft", PROTOCOL_VERSION, 11))
        .await
        .unwrap();
    let response = rx.recv().await.unwrap();
    let ack = parse_ack(&response);
    assert_eq!(ack.negotiated_ver, PROTOCOL_VERSION);
    let channel = 1;

    let goodbye = Frame::build(FrameType::Goodbye, control_flags(), 0, 0, 12, Vec::new()).unwrap();
    router.route_for_connection(&ctx, goodbye).await.unwrap();
    assert!(rx.try_recv().is_err());
    assert!(registry.get_module("aft").unwrap().is_none());

    router
        .route_for_connection(&ctx, channel_request(channel, 13))
        .await
        .unwrap();
    let error_frame = rx.recv().await.unwrap();
    assert_eq!(error_frame.header.ty, FrameType::Error);
    assert_eq!(error_frame.header.channel, channel);
    let captured = crate::router::test_log::captured_logs(&logs);
    assert_eq!(
        captured
            .lines()
            .filter(|line| {
                line.contains("module_id=aft")
                    && line.contains("reason=explicit_goodbye")
                    && line.contains("module registration ended")
            })
            .count(),
        1,
        "unexpected GOODBYE registry log: {captured}"
    );
}

#[tokio::test]
async fn module_goodbye_refreshes_requirements_and_pushes_route_closed() {
    let registry = Arc::new(Registry::default());
    let handler = ControlHandler::new(registry).with_capability_config(
        [("prov".to_string(), true), ("cons".to_string(), true)],
        BTreeMap::new(),
    );
    let (provider_ctx, mut provider_rx) = route_ctx(ConnectionId::new(701));
    register_capability_manifest(
        &handler,
        &provider_ctx,
        &mut provider_rx,
        capability_manifest("prov", &["thing/v1"], &[]),
        1,
    )
    .await;
    let mut consumer = capability_manifest("cons", &[], &[]);
    consumer.capabilities.as_mut().unwrap().requires.push(
        subc_protocol::manifest::CapabilityRequirement {
            capability: "thing/v1".to_string(),
            need: subc_protocol::manifest::CapabilityNeed::Required,
        },
    );
    let (consumer_ctx, mut consumer_rx) = route_ctx(ConnectionId::new(702));
    register_capability_manifest(&handler, &consumer_ctx, &mut consumer_rx, consumer, 2).await;
    assert_eq!(
        handler.capability_evaluator.verdict("cons", "thing/v1"),
        Some(CapabilityVerdict::Provided)
    );
    let (mut client_rx, _) = open_route_for_capability_test(
        &handler,
        &provider_ctx,
        &mut provider_rx,
        703,
        3,
        "prov",
        None,
    )
    .await;
    let goodbye = Frame::build(FrameType::Goodbye, control_flags(), 0, 0, 4, Vec::new()).unwrap();
    handler
        .handle_control_frame(&provider_ctx, goodbye)
        .await
        .unwrap();
    assert_eq!(
        handler.capability_evaluator.verdict("cons", "thing/v1"),
        Some(CapabilityVerdict::NeverProvided)
    );
    let closed = client_rx
        .try_recv()
        .expect("GOODBYE pushes route.closed before route GOODBYE");
    assert!(
        matches!(serde_json::from_slice::<ClientControlPush>(&closed.body).unwrap(),
            ClientControlPush::RouteClosed { module_id, channels, .. } if module_id == "prov" && channels.len() == 1)
    );
    assert_eq!(client_rx.try_recv().unwrap().header.ty, FrameType::Goodbye);
    assert_eq!(handler.forwarding.active_binding_count().unwrap(), 0);
}

struct RouteClosedFixture {
    handler: ControlHandler,
    #[cfg(unix)]
    supervisor: Supervisor,
    module_ctx: RouteCtx,
    #[cfg(unix)]
    module_rx: mpsc::Receiver<crate::router::OutboundFrame>,
    client_rx: mpsc::Receiver<crate::router::OutboundFrame>,
    route: Arc<crate::forwarding::RouteBinding>,
}

async fn route_closed_fixture() -> RouteClosedFixture {
    let registry = Arc::new(Registry::default());
    let handler = ControlHandler::new(Arc::clone(&registry));
    #[cfg(unix)]
    let supervisor = Supervisor::new(registry, RestartPolicy::default())
        .with_forwarding(Arc::clone(&handler.forwarding));
    let (module_ctx, mut module_rx) = route_ctx(ConnectionId::new(701));
    register_capability_manifest(
        &handler,
        &module_ctx,
        &mut module_rx,
        capability_manifest("prov", &[], &[]),
        1,
    )
    .await;
    let (client_rx, bind) =
        open_route_for_capability_test(&handler, &module_ctx, &mut module_rx, 702, 2, "prov", None)
            .await;
    let ModuleControlRequest::RouteBind {
        route_channel: channel,
        epoch: route_epoch,
        ..
    } = bind
    else {
        panic!("expected route.bind");
    };
    let crate::forwarding::DataRoute::Module(crate::forwarding::DataRouteState::Bound(route)) =
        handler
            .forwarding
            .lookup_data_route(module_ctx.connection_id, channel, route_epoch)
            .unwrap()
    else {
        panic!("the acknowledged route must be bound");
    };
    RouteClosedFixture {
        handler,
        #[cfg(unix)]
        supervisor,
        module_ctx,
        #[cfg(unix)]
        module_rx,
        client_rx,
        route,
    }
}

impl RouteClosedFixture {
    #[cfg(unix)]
    async fn cut(&mut self) {
        let notices = async {
            let mut draining = self.module_rx.recv().await.unwrap();
            assert!(matches!(
                serde_json::from_slice::<subc_protocol::session::ModuleControlCommand>(
                    &draining.body
                )
                .unwrap(),
                subc_protocol::session::ModuleControlCommand::Draining {
                    reason: RouteCloseReason::Disable,
                    ..
                }
            ));
            draining.flushed.take().unwrap().send(()).unwrap();
            let mut closing = self.client_rx.recv().await.unwrap();
            assert!(matches!(
                serde_json::from_slice::<ClientControlPush>(&closing.body).unwrap(),
                ClientControlPush::RouteClosing {
                    reason: RouteCloseReason::Restart,
                    ..
                }
            ));
            closing.flushed.take().unwrap().send(()).unwrap();
        };
        let (result, ()) = tokio::join!(self.supervisor.drain_for_daemon_shutdown(), notices);
        result.unwrap();
    }

    fn disconnect(&mut self) -> ClientControlPush {
        self.handler
            .cleanup_connection(self.module_ctx.connection_id)
            .unwrap();
        let closed = self.client_rx.try_recv().unwrap();
        assert_eq!(closed.header.ty, FrameType::Push);
        let push = serde_json::from_slice::<ClientControlPush>(&closed.body).unwrap();
        assert!(matches!(
            &push,
            ClientControlPush::RouteClosed { channels, abandoned: 0, .. }
                if channels == &[self.route.client_channel]
        ));
        assert_eq!(
            self.client_rx.try_recv().unwrap().header.ty,
            FrameType::Goodbye
        );
        assert_eq!(self.handler.forwarding.active_binding_count().unwrap(), 0);
        push
    }
}

#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn daemon_cut_quiet_routes_report_restart_and_drained() {
    let mut fixture = route_closed_fixture().await;
    fixture.cut().await;
    assert!(matches!(
        fixture.disconnect(),
        ClientControlPush::RouteClosed {
            reason: RouteCloseReason::Restart,
            drained: true,
            ..
        }
    ));
}

#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn daemon_cut_in_flight_past_budget_reports_restart_and_not_drained() {
    let mut fixture = route_closed_fixture().await;
    fixture.route.flow.acquire_tagged(99, false).await.unwrap();
    let started = tokio::time::Instant::now();
    fixture.cut().await;
    assert!(started.elapsed() >= Duration::from_secs(2));
    assert_eq!(fixture.route.flow.in_flight(), 1);
    assert!(matches!(
        fixture.disconnect(),
        ClientControlPush::RouteClosed {
            reason: RouteCloseReason::Restart,
            drained: false,
            ..
        }
    ));
}

#[tokio::test]
async fn module_connection_crash_reports_crash_and_not_drained() {
    let mut fixture = route_closed_fixture().await;
    // The server calls cleanup_connection on module EOF. Without a planned
    // daemon shutdown, that disconnect must remain a crash with no prior
    // route.closing notice, even when the route has no in-flight calls.
    assert!(matches!(
        fixture.disconnect(),
        ClientControlPush::RouteClosed {
            reason: RouteCloseReason::Crash,
            drained: false,
            ..
        }
    ));
}

#[tokio::test]
async fn dropping_router_connection_releases_registration() {
    let (logs, _guard) = crate::router::test_log::log_capture(tracing::Level::INFO);
    let registry = Arc::new(Registry::default());
    let control = Arc::new(ControlHandler::new(Arc::clone(&registry)));
    let router = Router::with_control_handler(Arc::clone(&control));
    let connection = router.begin_connection();
    let connection_id = connection.id();
    let (ctx, mut rx) = route_ctx(connection_id);

    router
        .route_for_connection(&ctx, hello_frame("aft", PROTOCOL_VERSION, 31))
        .await
        .unwrap();
    let response = rx.recv().await.unwrap();
    let ack = parse_ack(&response);
    assert_eq!(ack.negotiated_ver, PROTOCOL_VERSION);
    assert!(registry.get_module("aft").unwrap().is_some());

    drop(connection);

    assert!(registry.get_module("aft").unwrap().is_none());
    assert_eq!(registry.active_registration_count().unwrap(), 0);

    control
        .cleanup_connection(ConnectionId::new(u64::MAX))
        .unwrap();
    let captured = crate::router::test_log::captured_logs(&logs);
    println!("captured registry lifecycle logs:\n{captured}");
    let events: Vec<_> = captured
        .lines()
        .filter(|line| {
            line.contains("module registered module_id=aft ")
                || line.contains("module registration ended")
        })
        .collect();
    assert_eq!(
        events.len(),
        2,
        "unexpected registry lifecycle logs: {captured}"
    );
    assert!(events[0].contains("module registered module_id=aft "));
    assert!(events[0].contains(&format!("connection_id={}", connection_id.get())));
    assert!(events[1].contains(&format!(
        "module_id=aft connection_id={}",
        connection_id.get()
    )));
    assert!(events[1].contains("reason=connection_closed"));
    assert!(events[1].contains("module registration ended"));
    assert_eq!(
        captured
            .lines()
            .filter(|line| line.contains("module registered module_id=aft "))
            .count(),
        1,
        "legacy registration admission line must appear once: {captured}"
    );
}

#[tokio::test]
async fn hello_registration_keeps_legacy_module_registered_line_once() {
    let (logs, _guard) = crate::router::test_log::log_capture(tracing::Level::INFO);
    let registry = Arc::new(Registry::default());
    let control = ControlHandler::new(Arc::clone(&registry));
    let (ctx, mut rx) = route_ctx(ConnectionId::new(777));
    hello_via_sink(
        &control,
        &ctx,
        &mut rx,
        hello_frame("prefrontal-host:test", PROTOCOL_VERSION, 1),
    )
    .await;

    let captured = crate::router::test_log::captured_logs(&logs);
    let admissions: Vec<_> = captured
        .lines()
        .filter(|line| line.contains("module registered module_id=prefrontal-host:test "))
        .collect();
    assert_eq!(
        admissions.len(),
        1,
        "legacy admission line must remain exactly once: {captured}"
    );
    assert!(admissions[0].contains("routable_provider=true"));
    assert!(admissions[0].contains("connection_id=777"));
}

fn capability_manifest(
    module_id: &str,
    provides: &[&str],
    must_never_reach: &[&str],
) -> ModuleManifest {
    let mut manifest = manifest(module_id, PROTOCOL_VERSION);
    manifest.capabilities = Some(CapabilityDeclarations {
        provides: provides
            .iter()
            .map(|capability| (*capability).to_string())
            .collect(),
        requires: Vec::new(),
        must_never_reach: must_never_reach
            .iter()
            .map(|capability| (*capability).to_string())
            .collect(),
    });
    manifest
}

fn hello_frame_with_manifest(manifest: ModuleManifest, corr: u64) -> Frame {
    Frame::build(
        FrameType::Hello,
        control_flags(),
        0,
        0,
        corr,
        serde_json::to_vec(&ModuleHelloBody {
            protocol_ver: manifest.protocol_ver,
            manifest,
            control_ops: None,
            launch_nonce: None,
        })
        .expect("capability test HELLO serializes"),
    )
    .expect("capability test HELLO frame builds")
}

fn catalog_update_with_capabilities_frame(
    corr: u64,
    capabilities: CapabilityDeclarations,
) -> Frame {
    Frame::build(
        FrameType::Request,
        control_flags(),
        0,
        0,
        corr,
        serde_json::to_vec(&ModuleControlRequestFromModule::CatalogUpdate {
            provides: manifest("catalog-update-placeholder", PROTOCOL_VERSION).provides,
            capabilities: Some(capabilities),
            ready: None,
        })
        .expect("capability catalog.update serializes"),
    )
    .expect("capability catalog.update frame builds")
}

async fn register_capability_manifest(
    handler: &ControlHandler,
    ctx: &RouteCtx,
    rx: &mut mpsc::Receiver<crate::router::OutboundFrame>,
    manifest: ModuleManifest,
    corr: u64,
) {
    hello_via_sink(handler, ctx, rx, hello_frame_with_manifest(manifest, corr)).await;
}

async fn open_route_for_capability_test(
    handler: &ControlHandler,
    target_ctx: &RouteCtx,
    target_rx: &mut mpsc::Receiver<crate::router::OutboundFrame>,
    client_connection_id: u64,
    corr: u64,
    target_module_id: &str,
    consumer_identity: Option<ConsumerIdentity>,
) -> (
    mpsc::Receiver<crate::router::OutboundFrame>,
    ModuleControlRequest,
) {
    let (client_ctx, mut client_rx) = route_ctx(ConnectionId::new(client_connection_id));
    let route_handler = handler.clone();
    let target_module_id = target_module_id.to_string();
    let route_task = tokio::spawn(async move {
        route_handler
            .handle_control_frame(
                &client_ctx,
                route_open_frame_with_admission_facts(
                    corr,
                    &target_module_id,
                    unique_project_root("admission-facts"),
                    consumer_identity,
                    None,
                ),
            )
            .await
            .expect("capability test route.open succeeds")
    });
    let bind = tokio::time::timeout(Duration::from_secs(1), target_rx.recv())
        .await
        .expect("capability test route.open must reach route.bind")
        .expect("target control receiver stays open");
    let bind_request: ModuleControlRequest =
        serde_json::from_slice(&bind.body).expect("route.bind decodes");
    handler
        .handle_control_frame(target_ctx, route_bind_ack(bind.header.corr))
        .await
        .expect("capability test route.bind ACK succeeds");
    assert!(route_task.await.expect("route.open task joins").is_empty());
    let opened = client_rx
        .recv()
        .await
        .expect("successful route.open publishes a response");
    assert!(matches!(
        serde_json::from_slice::<ClientControlResponse>(&opened.body),
        Ok(ClientControlResponse::RouteOpen { .. })
    ));
    (client_rx, bind_request)
}

fn assert_capability_denied_push(frame: Frame, target_module_id: &str) -> u16 {
    assert_eq!(frame.header.ty, FrameType::Push);
    assert_eq!(frame.header.channel, 0);
    let push = serde_json::from_slice::<ClientControlPush>(&frame.body)
        .expect("route.closed control push decodes");
    let ClientControlPush::RouteClosed { channels, .. } = &push else {
        panic!("expected route.closed");
    };
    assert_eq!(channels.len(), 1, "exactly one violating route closed");
    let closed_channel = channels[0];
    let channels = channels.clone();
    assert_eq!(
        push,
        ClientControlPush::RouteClosed {
            module_id: target_module_id.to_string(),
            channels,
            reason: RouteCloseReason::CapabilityDenied,
            drained: false,
            abandoned: 0,
            excluded_subscriptions: 0,
            terminal: Some(false),
        }
    );
    closed_channel
}

#[tokio::test]
async fn route_open_capability_forbidden_mutation_proof_creates_no_route() {
    let registry = Arc::new(Registry::default());
    let forwarding = Arc::new(ForwardingTable::default());
    let supervisor = SupervisorHandle::new();
    supervisor.set_spawn_nonce("opener", "opener-nonce".to_string());
    let handler = ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding))
        .with_supervisor(supervisor);
    let (target_ctx, mut target_rx) = route_ctx(ConnectionId::new(700));
    let (opener_ctx, mut opener_rx) = route_ctx(ConnectionId::new(701));
    register_capability_manifest(
        &handler,
        &target_ctx,
        &mut target_rx,
        capability_manifest("target", &["credentials-provider/v1"], &[]),
        1,
    )
    .await;
    register_capability_manifest(
        &handler,
        &opener_ctx,
        &mut opener_rx,
        capability_manifest("opener", &[], &["credentials-provider/v1"]),
        2,
    )
    .await;

    let (client_ctx, _client_rx) = route_ctx(ConnectionId::new(702));
    let replies = handler
        .handle_control_frame(
            &client_ctx,
            route_open_frame_with_admission_facts(
                3,
                "target",
                unique_project_root("admission-facts"),
                Some(ConsumerIdentity {
                    module_id: "opener".to_string(),
                    launch_nonce: "opener-nonce".to_string(),
                }),
                None,
            ),
        )
        .await
        .expect("denied route.open returns a typed frame");
    assert_eq!(parse_error(&replies[0])["code"], "capability_forbidden");
    assert_eq!(forwarding.active_binding_count().unwrap(), 0);
    assert!(
        target_rx.try_recv().is_err(),
        "forbidden route.open must not relay route.bind"
    );
}

#[tokio::test]
async fn capability_deny_edge_hello_mutation_proof_force_closes_existing_route() {
    let registry = Arc::new(Registry::default());
    let forwarding = Arc::new(ForwardingTable::default());
    let supervisor = SupervisorHandle::new();
    supervisor.set_spawn_nonce("opener", "opener-nonce".to_string());
    let handler = ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding))
        .with_supervisor(supervisor);
    let (target_ctx, mut target_rx) = route_ctx(ConnectionId::new(710));
    let (old_opener_ctx, mut old_opener_rx) = route_ctx(ConnectionId::new(711));
    register_capability_manifest(
        &handler,
        &target_ctx,
        &mut target_rx,
        capability_manifest("target", &["credentials-provider/v1"], &[]),
        1,
    )
    .await;
    register_capability_manifest(
        &handler,
        &old_opener_ctx,
        &mut old_opener_rx,
        capability_manifest("opener", &[], &[]),
        2,
    )
    .await;
    let (mut client_rx, _) = open_route_for_capability_test(
        &handler,
        &target_ctx,
        &mut target_rx,
        712,
        3,
        "target",
        Some(ConsumerIdentity {
            module_id: "opener".to_string(),
            launch_nonce: "opener-nonce".to_string(),
        }),
    )
    .await;
    assert_eq!(forwarding.active_binding_count().unwrap(), 1);

    handler
        .cleanup_connection(old_opener_ctx.connection_id)
        .expect("old opener registration cleans up");
    let (new_opener_ctx, mut new_opener_rx) = route_ctx(ConnectionId::new(713));
    register_capability_manifest(
        &handler,
        &new_opener_ctx,
        &mut new_opener_rx,
        capability_manifest("opener", &[], &["credentials-provider/v1"]),
        4,
    )
    .await;

    let closed_channel = assert_capability_denied_push(
        client_rx
            .try_recv()
            .expect("HELLO deny addition must emit route.closed")
            .frame,
        "target",
    );
    let goodbye = client_rx
        .try_recv()
        .expect("HELLO deny addition must send the client a route GOODBYE")
        .frame;
    assert_eq!(goodbye.header.ty, FrameType::Goodbye);
    assert_eq!(goodbye.header.channel, closed_channel);
    assert_ne!(goodbye.header.channel, 0);
    assert_ne!(goodbye.header.epoch, 0);
    assert!(client_rx.try_recv().is_err());
    assert_eq!(forwarding.active_binding_count().unwrap(), 0);
    assert!(matches!(
        target_rx.try_recv(),
        Ok(outbound) if outbound.header.ty == FrameType::Goodbye
    ));
}

#[tokio::test]
async fn capability_claim_catalog_update_mutation_proof_force_closes_existing_route() {
    let registry = Arc::new(Registry::default());
    let forwarding = Arc::new(ForwardingTable::default());
    let supervisor = SupervisorHandle::new();
    supervisor.set_spawn_nonce("opener", "opener-nonce".to_string());
    let handler = ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding))
        .with_supervisor(supervisor);
    let (target_ctx, mut target_rx) = route_ctx(ConnectionId::new(720));
    let (opener_ctx, mut opener_rx) = route_ctx(ConnectionId::new(721));
    register_capability_manifest(
        &handler,
        &target_ctx,
        &mut target_rx,
        capability_manifest("target", &[], &[]),
        1,
    )
    .await;
    register_capability_manifest(
        &handler,
        &opener_ctx,
        &mut opener_rx,
        capability_manifest("opener", &[], &["credentials-provider/v1"]),
        2,
    )
    .await;
    let (mut client_rx, _) = open_route_for_capability_test(
        &handler,
        &target_ctx,
        &mut target_rx,
        722,
        3,
        "target",
        Some(ConsumerIdentity {
            module_id: "opener".to_string(),
            launch_nonce: "opener-nonce".to_string(),
        }),
    )
    .await;
    assert_eq!(forwarding.active_binding_count().unwrap(), 1);

    let replies = handler
        .handle_control_frame(
            &target_ctx,
            catalog_update_with_capabilities_frame(
                4,
                CapabilityDeclarations {
                    provides: vec!["credentials-provider/v1".to_string()],
                    requires: Vec::new(),
                    must_never_reach: Vec::new(),
                },
            ),
        )
        .await
        .expect("claim catalog.update succeeds");
    assert!(matches!(
        serde_json::from_slice::<ModuleControlResponseToModule>(&replies[0].body),
        Ok(ModuleControlResponseToModule::CatalogUpdate {})
    ));
    let closed_channel = assert_capability_denied_push(
        client_rx
            .try_recv()
            .expect("claim addition must emit route.closed")
            .frame,
        "target",
    );
    let goodbye = client_rx
        .try_recv()
        .expect("claim addition must send the client a route GOODBYE")
        .frame;
    assert_eq!(goodbye.header.ty, FrameType::Goodbye);
    assert_eq!(goodbye.header.channel, closed_channel);
    assert_ne!(goodbye.header.channel, 0);
    assert_ne!(goodbye.header.epoch, 0);
    assert!(client_rx.try_recv().is_err());
    assert_eq!(forwarding.active_binding_count().unwrap(), 0);
    assert!(matches!(
        target_rx.try_recv(),
        Ok(outbound) if outbound.header.ty == FrameType::Goodbye
    ));
}

#[tokio::test]
async fn capability_claim_removal_mutation_proof_keeps_route_open_without_close_frame() {
    let registry = Arc::new(Registry::default());
    let forwarding = Arc::new(ForwardingTable::default());
    let supervisor = SupervisorHandle::new();
    supervisor.set_spawn_nonce("opener", "opener-nonce".to_string());
    let handler = ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding))
        .with_supervisor(supervisor);
    let (target_ctx, mut target_rx) = route_ctx(ConnectionId::new(730));
    let (opener_ctx, mut opener_rx) = route_ctx(ConnectionId::new(731));
    register_capability_manifest(
        &handler,
        &target_ctx,
        &mut target_rx,
        capability_manifest("target", &["credentials-provider/v1"], &[]),
        1,
    )
    .await;
    register_capability_manifest(
        &handler,
        &opener_ctx,
        &mut opener_rx,
        capability_manifest("opener", &[], &[]),
        2,
    )
    .await;
    let (mut client_rx, _) = open_route_for_capability_test(
        &handler,
        &target_ctx,
        &mut target_rx,
        732,
        3,
        "target",
        Some(ConsumerIdentity {
            module_id: "opener".to_string(),
            launch_nonce: "opener-nonce".to_string(),
        }),
    )
    .await;
    assert_eq!(forwarding.active_binding_count().unwrap(), 1);

    handler
        .handle_control_frame(
            &target_ctx,
            catalog_update_with_capabilities_frame(
                4,
                CapabilityDeclarations {
                    provides: Vec::new(),
                    requires: Vec::new(),
                    must_never_reach: Vec::new(),
                },
            ),
        )
        .await
        .expect("claim removal catalog.update succeeds");
    assert_eq!(
        forwarding.active_binding_count().unwrap(),
        1,
        "removing an attested target claim must leave the route census unchanged"
    );
    assert!(
        client_rx.try_recv().is_err(),
        "claim removal must not emit route.closed or a client route GOODBYE"
    );
    assert!(
        target_rx.try_recv().is_err(),
        "claim removal must not send the target a route GOODBYE"
    );
}

/// A direct client may open a route to a denied capability provider; this
/// policy applies only to attested supervised module origins, not to direct clients.
#[tokio::test]
async fn direct_client_scope_honesty_mutation_proof_opens_denied_capability_provider() {
    let registry = Arc::new(Registry::default());
    let forwarding = Arc::new(ForwardingTable::default());
    let supervisor = SupervisorHandle::new();
    supervisor.set_spawn_nonce("opener", "opener-nonce".to_string());
    let handler = ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding))
        .with_supervisor(supervisor);
    let (target_ctx, mut target_rx) = route_ctx(ConnectionId::new(740));
    let (opener_ctx, mut opener_rx) = route_ctx(ConnectionId::new(741));
    register_capability_manifest(
        &handler,
        &target_ctx,
        &mut target_rx,
        capability_manifest("target", &["credentials-provider/v1"], &[]),
        1,
    )
    .await;
    register_capability_manifest(
        &handler,
        &opener_ctx,
        &mut opener_rx,
        capability_manifest("opener", &[], &["credentials-provider/v1"]),
        2,
    )
    .await;

    let (_client_rx, bind) = open_route_for_capability_test(
        &handler,
        &target_ctx,
        &mut target_rx,
        742,
        3,
        "target",
        None,
    )
    .await;
    let ModuleControlRequest::RouteBind { principal, .. } = bind else {
        panic!("direct scope-honesty route must bind");
    };
    assert_eq!(principal, Some(Principal::Direct));
    assert_eq!(forwarding.active_binding_count().unwrap(), 1);
}

/// A module that denies a capability receives no self-route exemption when it
/// also attestedly provides that capability.
#[tokio::test]
async fn must_never_reach_self_route_is_capability_forbidden() {
    let registry = Arc::new(Registry::default());
    let forwarding = Arc::new(ForwardingTable::default());
    let supervisor = SupervisorHandle::new();
    supervisor.set_spawn_nonce("self-provider", "self-nonce".to_string());
    let handler = ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding))
        .with_supervisor(supervisor);
    let (self_ctx, mut self_rx) = route_ctx(ConnectionId::new(750));
    register_capability_manifest(
        &handler,
        &self_ctx,
        &mut self_rx,
        capability_manifest(
            "self-provider",
            &["credentials-provider/v1"],
            &["credentials-provider/v1"],
        ),
        1,
    )
    .await;

    let (client_ctx, _client_rx) = route_ctx(ConnectionId::new(751));
    let replies = handler
        .handle_control_frame(
            &client_ctx,
            route_open_frame_with_admission_facts(
                2,
                "self-provider",
                unique_project_root("admission-facts"),
                Some(ConsumerIdentity {
                    module_id: "self-provider".to_string(),
                    launch_nonce: "self-nonce".to_string(),
                }),
                None,
            ),
        )
        .await
        .expect("self-route refusal returns a typed frame");
    assert_eq!(parse_error(&replies[0])["code"], "capability_forbidden");
    assert_eq!(forwarding.active_binding_count().unwrap(), 0);
    assert!(
        self_rx.try_recv().is_err(),
        "self denial must not relay route.bind"
    );
}

#[test]
fn unsupported_channel_zero_frame_returns_error() {
    let handler = ControlHandler::default();
    let request = Frame::build(
        FrameType::Request,
        control_flags(),
        0,
        0,
        21,
        b"opaque".to_vec(),
    )
    .unwrap();

    let response = handler
        .handle_control(ConnectionId::new(1), request)
        .unwrap();

    assert_eq!(response[0].header.ty, FrameType::Error);
    assert_eq!(
        parse_error(&response[0])["code"],
        "unsupported_control_frame"
    );
}

/// Blue/green swap at the control-plane boundary. The supervisor that opens
/// a swap is not wired yet, so the candidate is registered here directly
/// into the registry and forwarding candidate slots, the way the swap's
/// HELLO admission will.
mod swap {
    use super::*;

    const INCUMBENT: ConnectionId = ConnectionId::new(30);
    const CANDIDATE: ConnectionId = ConnectionId::new(40);

    struct Swap {
        registry: Arc<Registry>,
        forwarding: Arc<ForwardingTable>,
        handler: ControlHandler,
        incumbent_ctx: RouteCtx,
        incumbent_rx: mpsc::Receiver<crate::router::OutboundFrame>,
        candidate_ctx: RouteCtx,
        candidate_rx: mpsc::Receiver<crate::router::OutboundFrame>,
    }

    async fn swap_with_incumbent() -> Swap {
        let registry = Arc::new(Registry::default());
        let forwarding = Arc::new(ForwardingTable::default());
        let handler =
            ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding));
        let (incumbent_ctx, mut incumbent_rx) = route_ctx(INCUMBENT);
        hello_via_sink(
            &handler,
            &incumbent_ctx,
            &mut incumbent_rx,
            hello_frame("aft", PROTOCOL_VERSION, 7),
        )
        .await;
        let (candidate_ctx, candidate_rx) = route_ctx(CANDIDATE);
        Swap {
            registry,
            forwarding,
            handler,
            incumbent_ctx,
            incumbent_rx,
            candidate_ctx,
            candidate_rx,
        }
    }

    fn register_candidate(swap: &Swap, ready: Option<bool>) {
        let mut candidate_manifest = manifest("aft", PROTOCOL_VERSION);
        candidate_manifest.ready = ready;
        let registration = swap
            .registry
            .register_candidate_with_control_ops(
                candidate_manifest,
                PROTOCOL_VERSION,
                CANDIDATE,
                module_baseline_control_ops(),
            )
            .unwrap();
        swap.forwarding
            .register_candidate_module_connection(
                CANDIDATE,
                "aft".to_string(),
                PROTOCOL_VERSION,
                manifest_concurrency(&registration.manifest),
                swap.candidate_ctx.egress.clone(),
            )
            .unwrap();
    }

    fn cutover(swap: &Swap) -> crate::forwarding::ModuleEndpointId {
        let cutover = swap.forwarding.cutover_candidate("aft").unwrap().unwrap();
        swap.registry.promote_candidate("aft").unwrap().unwrap();
        cutover.incumbent.unwrap()
    }

    /// A swap candidate's HELLO reserves its candidate registry entry
    /// before installing its candidate forwarding endpoint. The swap
    /// supervisor promotes a ready candidate as soon as it reads it as
    /// registered, and promotion cuts forwarding over to the candidate
    /// endpoint, so the lookup the supervisor reads
    /// (`Registry::published_registration`) must not return the candidate
    /// until that endpoint exists. Otherwise the cutover finds no candidate
    /// endpoint and the swap fails as if the candidate had disconnected.
    #[tokio::test]
    async fn swap_candidate_is_published_only_after_its_forwarding_endpoint_exists() {
        let mut swap = swap_with_incumbent().await;
        let hold = Arc::new(std::sync::Barrier::new(2));
        let handler = swap
            .handler
            .clone()
            .with_hello_registry_to_forwarding_hold(Arc::clone(&hold));
        handler
            .supervisor
            .open_swap("aft", "candidate-nonce".to_string());
        let hello = {
            let handler = handler.clone();
            let candidate_ctx = swap.candidate_ctx.clone();
            std::thread::spawn(move || {
                handler
                    .handle_hello(
                        candidate_ctx.connection_id,
                        Some(candidate_ctx.egress.clone()),
                        hello_frame_with_nonce("aft", PROTOCOL_VERSION, 8, Some("candidate-nonce")),
                    )
                    .unwrap()
            })
        };

        hold.wait();
        // Parked: the candidate slot is claimed but not published, and
        // forwarding has no candidate endpoint.
        assert!(swap
            .registry
            .registration(crate::registry::RegistrationSlot::Candidate("aft"))
            .unwrap()
            .is_some());
        assert!(swap
            .registry
            .published_registration(crate::registry::RegistrationSlot::Candidate("aft"))
            .unwrap()
            .is_none());
        assert!(swap
            .forwarding
            .module_endpoint_for_connection(CANDIDATE)
            .unwrap()
            .is_none());

        hold.wait();
        assert!(hello.join().unwrap().is_empty(), "candidate HELLO succeeds");
        assert_eq!(
            swap.candidate_rx.try_recv().unwrap().frame.header.ty,
            FrameType::HelloAck
        );
        let published = swap
            .registry
            .published_registration(crate::registry::RegistrationSlot::Candidate("aft"))
            .unwrap()
            .expect("candidate is published once its HELLO completes");
        assert_eq!(published.connection_id, CANDIDATE);
        assert!(swap
            .forwarding
            .module_endpoint_for_connection(CANDIDATE)
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn candidate_lookup_after_hello_ack_waits_for_publication() {
        let mut swap = swap_with_incumbent().await;
        let hold = Arc::new(std::sync::Barrier::new(2));
        swap.handler.hello_ack_to_publication_hold = Some(Arc::clone(&hold));
        swap.handler
            .supervisor
            .open_swap("aft", "candidate-nonce".to_string());
        let hello = {
            let handler = swap.handler.clone();
            let candidate_ctx = swap.candidate_ctx.clone();
            std::thread::spawn(move || {
                handler
                    .handle_hello(
                        CANDIDATE,
                        Some(candidate_ctx.egress),
                        hello_frame_with_nonce("aft", PROTOCOL_VERSION, 8, Some("candidate-nonce")),
                    )
                    .unwrap()
            })
        };
        hold.wait();
        assert_eq!(
            swap.candidate_rx.try_recv().unwrap().frame.header.ty,
            FrameType::HelloAck
        );
        let reader_registry = Arc::clone(&swap.registry);
        let published = lookup_during_hello_publication(&swap.registry, &hold, move || {
            reader_registry
                .published_registration(crate::registry::RegistrationSlot::Candidate("aft"))
                .unwrap()
        });
        assert!(hello.join().unwrap().is_empty());
        assert_eq!(
            published
                .expect("ACK makes candidate publication visible")
                .connection_id,
            CANDIDATE
        );
        assert_eq!(
            swap.registry.generation().unwrap(),
            1,
            "candidate is not a second catalog entry"
        );
    }

    #[tokio::test]
    async fn audit4_ungated_removed_candidate_publication_rolls_back_admission() {
        let mut swap = swap_with_incumbent().await;
        let hold = Arc::new(std::sync::Barrier::new(2));
        let handler = swap
            .handler
            .clone()
            .with_hello_registry_to_forwarding_hold(hold.clone());
        handler
            .supervisor
            .open_swap("aft", "candidate-nonce".into());
        let hello = {
            let handler = handler.clone();
            let ctx = swap.candidate_ctx.clone();
            std::thread::spawn(move || {
                handler
                    .handle_hello(
                        CANDIDATE,
                        Some(ctx.egress),
                        hello_frame_with_nonce("aft", PROTOCOL_VERSION, 8, Some("candidate-nonce")),
                    )
                    .unwrap()
            })
        };
        hold.wait();
        assert_eq!(
            swap.registry
                .deregister_connection(CANDIDATE)
                .unwrap()
                .len(),
            1
        );
        hold.wait();
        assert!(hello.join().unwrap().is_empty());
        assert_eq!(
            swap.candidate_rx.try_recv().unwrap().frame.header.ty,
            FrameType::HelloAck
        );
        assert!(swap.registry.get_candidate("aft").unwrap().is_none());
        assert_eq!(
            handler
                .supervisor
                .swap_hello_admission("aft", Some("candidate-nonce")),
            SwapHelloAdmission::Candidate,
            "failed publication must roll back candidate_admitted for an ungated nonce swap"
        );
        handler.supervisor.close_swap("aft");
    }

    fn keyed_total(counters: &Value, key: &str) -> u64 {
        counters[key]
            .as_object()
            .map(|counts| counts.values().filter_map(Value::as_u64).sum())
            .unwrap_or(0)
    }

    #[tokio::test]
    async fn replacement_logs_old_end_and_new_admission_once() {
        let (logs, _guard) = crate::router::test_log::log_capture(tracing::Level::INFO);
        let mut swap = swap_with_incumbent().await;
        swap.handler
            .supervisor
            .open_swap("aft", "candidate-nonce".to_string());
        hello_via_sink(
            &swap.handler,
            &swap.candidate_ctx,
            &mut swap.candidate_rx,
            hello_frame_with_nonce("aft", PROTOCOL_VERSION, 8, Some("candidate-nonce")),
        )
        .await;
        cutover(&swap);
        swap.handler.cleanup_connection(INCUMBENT).unwrap();

        let captured = crate::router::test_log::captured_logs(&logs);
        println!("captured replacement registry logs:\n{captured}");
        let new_admissions: Vec<_> = captured
            .lines()
            .filter(|line| {
                line.contains("connection_id=40")
                    && line.contains("swap candidate registered; not routable until cutover")
            })
            .collect();
        assert_eq!(
            new_admissions.len(),
            1,
            "unexpected admission logs: {captured}"
        );
        assert!(new_admissions[0].contains("module_id=aft"));
        assert!(new_admissions[0].contains("connection_id=40"));
        assert!(new_admissions[0].contains("swap candidate registered; not routable until cutover"));
        assert!(new_admissions[0].contains("ready=true"));

        let old_admissions: Vec<_> = captured
            .lines()
            .filter(|line| {
                line.contains("module registered module_id=aft ")
                    && line.contains("connection_id=30")
            })
            .collect();
        assert_eq!(
            old_admissions.len(),
            1,
            "unexpected incumbent admission: {captured}"
        );

        let old_ends: Vec<_> = captured
            .lines()
            .filter(|line| {
                line.contains("connection_id=30") && line.contains("module registration ended")
            })
            .collect();
        assert_eq!(old_ends.len(), 1, "unexpected end logs: {captured}");
        assert!(old_ends[0].contains("module_id=aft"));
        assert!(old_ends[0].contains("connection_id=30"));
        assert!(old_ends[0].contains("reason=replaced"));
        assert!(old_ends[0].contains("replaced_by_connection_id=40"));
        assert_eq!(
            captured
                .lines()
                .filter(|line| line.contains("module registration promoted"))
                .count(),
            1,
            "unexpected promotion logs: {captured}"
        );
    }

    /// An ack from the incumbent for a bind it was sent before cutover,
    /// arriving before the incumbent is drained. The incumbent is the live
    /// connection carrying every other client's routes, so the ack must
    /// not end it: the waiting client is told to retry, the reservation is
    /// given back, and the incumbent is told to drop just that binding.
    #[tokio::test]
    async fn incumbent_ack_between_promotion_and_drain_keeps_the_incumbent_serving() {
        let mut swap = swap_with_incumbent().await;
        let handler = swap.handler.clone();

        // A co-tenant route, bound on the incumbent before the swap.
        let cotenant = ConnectionId::new(31);
        let (cotenant_ctx, mut cotenant_rx) = route_ctx(cotenant);
        let (cotenant_task, cotenant_bind) = relay_route_open(
            &handler,
            cotenant,
            &cotenant_ctx.egress,
            &mut swap.incumbent_rx,
            100,
            "aft",
            "swap-cotenant",
        )
        .await;
        handler
            .handle_control_frame(
                &swap.incumbent_ctx,
                route_bind_ack(cotenant_bind.header.corr),
            )
            .await
            .unwrap();
        assert!(cotenant_task.await.unwrap().is_empty());
        let (cotenant_channel, cotenant_epoch) =
            published_route(&cotenant_rx.recv().await.unwrap());

        // A second route.open, relayed to the incumbent and not yet acked.
        let caller = ConnectionId::new(32);
        let (caller_ctx, mut caller_rx) = route_ctx(caller);
        let (caller_task, caller_bind) = relay_route_open(
            &handler,
            caller,
            &caller_ctx.egress,
            &mut swap.incumbent_rx,
            101,
            "aft",
            "swap-caller",
        )
        .await;
        let (abandoned_channel, abandoned_epoch) = route_bind_channel(&caller_bind);

        register_candidate(&swap, None);
        cutover(&swap);

        // The incumbent acks after promotion and before any drain.
        let ack = handler
            .handle_control_frame(&swap.incumbent_ctx, route_bind_ack(caller_bind.header.corr))
            .await;
        let module_loop_error = ack.as_ref().err().map(ToString::to_string);
        if module_loop_error.is_some() {
            // What the connection loop does with an untranslated router
            // error: end the connection, releasing every route on it.
            handler.cleanup_connection(INCUMBENT).unwrap();
        }

        // 1. The incumbent's other routes survive.
        assert!(
            cotenant_rx.try_recv().is_err(),
            "the co-tenant route on the incumbent was torn down by one late ack: \
                 {module_loop_error:?}"
        );
        assert!(matches!(
            swap.forwarding
                .lookup_data_route(cotenant, cotenant_channel, cotenant_epoch)
                .unwrap(),
            DataRoute::Client(DataRouteState::Bound(_))
        ));
        assert_eq!(module_loop_error, None);
        assert!(swap
            .registry
            .get_module_by_connection(INCUMBENT)
            .unwrap()
            .is_some());

        // 2. Exactly one channel-scoped GOODBYE to the incumbent.
        let goodbye = tokio::time::timeout(Duration::from_secs(1), swap.incumbent_rx.recv())
            .await
            .expect("the incumbent is told to drop the abandoned binding")
            .unwrap()
            .frame;
        assert_eq!(goodbye.header.ty, FrameType::Goodbye);
        assert_eq!(goodbye.header.channel, abandoned_channel);
        assert_eq!(goodbye.header.epoch, abandoned_epoch);
        assert!(swap.incumbent_rx.try_recv().is_err());

        // 3. The waiting client gets a retryable refusal and no route.
        let response = caller_task.await.unwrap();
        assert_eq!(response.len(), 1);
        assert_eq!(parse_error(&response[0])["code"], "module_reloading");
        assert!(caller_rx.try_recv().is_err());

        // 4. The reservation pair is given back, and the pending bind
        //    settled exactly once: one accepted open (the co-tenant) and one
        //    refused open (the caller), nothing counted twice.
        assert_eq!(swap.forwarding.reserved_route_count().unwrap(), (0, 0));
        let counters = handler.counters().snapshot();
        assert_eq!(
            keyed_total(&counters, "route_open_accepted_by_principal"),
            1
        );
        assert_eq!(keyed_total(&counters, "route_open_refused_by_code"), 1);
        assert_eq!(counters["route_open_refused_by_code"]["module_rejected"], 1);
    }

    /// After cutover the incumbent is drained BY ENDPOINT. Draining by module
    /// id would resolve to the promoted candidate and every new route.open
    /// would be refused as reloading, leaving neither process routable.
    #[tokio::test]
    async fn route_open_after_cutover_and_incumbent_drain_is_relayed_to_the_candidate() {
        let mut swap = swap_with_incumbent().await;
        register_candidate(&swap, None);
        let incumbent = cutover(&swap);
        swap.forwarding
            .begin_endpoint_drain(incumbent, RouteCloseReason::Restart)
            .unwrap()
            .expect("the incumbent is still registered");

        let client = ConnectionId::new(33);
        let (client_ctx, mut client_rx) = route_ctx(client);
        let route_handler = swap.handler.clone();
        let open_ctx = RouteCtx {
            connection_id: client,
            egress: client_ctx.egress.clone(),
        };
        let mut route_task = tokio::spawn(async move {
            route_handler
                .handle_control_frame(
                    &open_ctx,
                    route_open_frame(90, "aft", unique_project_root("swap-after-drain")),
                )
                .await
                .unwrap()
        });
        let bind = tokio::select! {
            bind = swap.candidate_rx.recv() => bind.expect("candidate egress is open").frame,
            response = &mut route_task => {
                let response = response.unwrap();
                panic!(
                    "post-cutover route.open was refused instead of relayed to the candidate: {}",
                    parse_error(&response[0])["code"]
                );
            }
        };
        swap.handler
            .handle_control_frame(&swap.candidate_ctx, route_bind_ack(bind.header.corr))
            .await
            .unwrap();
        assert!(route_task.await.unwrap().is_empty());
        let (channel, epoch) = published_route(&client_rx.recv().await.unwrap());
        match swap
            .forwarding
            .lookup_data_route(client, channel, epoch)
            .unwrap()
        {
            DataRoute::Client(DataRouteState::Bound(route)) => {
                assert_eq!(route.module_endpoint.connection_id, CANDIDATE)
            }
            other => panic!("expected a bound route on the candidate, got {other:?}"),
        }
        assert!(swap.incumbent_rx.try_recv().is_err());
    }

    /// A candidate declares itself ready with `catalog.update` on its own
    /// connection. If the connection-keyed registry lookups searched only the
    /// active slot, this would answer `not_registered` and the candidate
    /// would never become ready.
    #[tokio::test]
    async fn candidate_catalog_update_ready_reaches_the_candidate_registration() {
        let swap = swap_with_incumbent().await;
        register_candidate(&swap, Some(false));
        let update = Frame::build(
            FrameType::Request,
            control_flags(),
            0,
            0,
            55,
            serde_json::to_vec(&ModuleControlRequestFromModule::CatalogUpdate {
                provides: manifest("aft", PROTOCOL_VERSION).provides,
                capabilities: None,
                ready: Some(true),
            })
            .unwrap(),
        )
        .unwrap();

        let replies = swap
            .handler
            .handle_control_frame(&swap.candidate_ctx, update)
            .await
            .unwrap();

        assert_eq!(replies.len(), 1);
        assert_eq!(
            replies[0].header.ty,
            FrameType::Response,
            "candidate catalog.update was refused: {:?}",
            serde_json::from_slice::<Value>(&replies[0].body).ok()
        );
        assert!(swap.registry.get_candidate("aft").unwrap().unwrap().ready);
        assert_eq!(
            swap.registry
                .get_module("aft")
                .unwrap()
                .unwrap()
                .connection_id,
            INCUMBENT
        );
    }
}

/// The HELLO gate while the supervisor has a swap open: only the nonce it
/// minted for the candidate admits a second process, into the candidate
/// slot, and that check runs ahead of the reserved-module gate.
mod swap_admission {
    use super::*;

    const INCUMBENT_NONCE: &str = "incumbent-nonce";
    const CANDIDATE_NONCE: &str = "candidate-nonce";

    fn handler_with_incumbent(
        module_id: &str,
        reserved: bool,
    ) -> (Arc<Registry>, SupervisorHandle, ControlHandler) {
        let registry = Arc::new(Registry::default());
        let supervisor = SupervisorHandle::new();
        supervisor.set_spawn_nonce(module_id, INCUMBENT_NONCE.to_string());
        if reserved {
            supervisor.set_reserved_nonce(module_id, INCUMBENT_NONCE.to_string());
        }
        let handler =
            ControlHandler::new(Arc::clone(&registry)).with_supervisor(supervisor.clone());
        let incumbent = handler
            .handle_control(
                ConnectionId::new(1),
                hello_frame_with_nonce(module_id, PROTOCOL_VERSION, 1, Some(INCUMBENT_NONCE)),
            )
            .unwrap();
        assert_eq!(incumbent[0].header.ty, FrameType::HelloAck);
        supervisor.open_swap(module_id, CANDIDATE_NONCE.to_string());
        (registry, supervisor, handler)
    }

    /// Design mutation arm (ii). On an UNRESERVED id the reserved gate
    /// admits every nonce, so while a swap is open the swap gate is the only
    /// thing between a key-holder and the candidate slot. A nonce the
    /// supervisor did not mint, or none at all, is refused, and neither the
    /// incumbent's registration nor the candidate slot moves.
    #[test]
    fn unminted_nonce_on_an_unreserved_id_with_an_open_swap_is_refused() {
        let (registry, _supervisor, handler) = handler_with_incumbent("aft", false);

        for (connection, nonce) in [(2, Some("forged")), (3, None)] {
            let replies = handler
                .handle_control(
                    ConnectionId::new(connection),
                    hello_frame_with_nonce("aft", PROTOCOL_VERSION, connection, nonce),
                )
                .unwrap();
            assert_eq!(replies[0].header.ty, FrameType::Error);
            assert_eq!(
                parse_error(&replies[0])["code"],
                "swap_token_invalid",
                "nonce {nonce:?}"
            );
        }
        assert!(registry.get_candidate("aft").unwrap().is_none());
        assert_eq!(
            registry.get_module("aft").unwrap().unwrap().connection_id,
            ConnectionId::new(1)
        );

        // Control: the minted token is admitted, into the candidate slot,
        // and only once.
        let admitted = handler
            .handle_control(
                ConnectionId::new(4),
                hello_frame_with_nonce("aft", PROTOCOL_VERSION, 4, Some(CANDIDATE_NONCE)),
            )
            .unwrap();
        assert_eq!(admitted[0].header.ty, FrameType::HelloAck);
        assert_eq!(
            registry
                .get_candidate("aft")
                .unwrap()
                .unwrap()
                .connection_id,
            ConnectionId::new(4)
        );
        assert_eq!(
            registry.get_module("aft").unwrap().unwrap().connection_id,
            ConnectionId::new(1),
            "the candidate must not take the active slot"
        );
        let replayed = handler
            .handle_control(
                ConnectionId::new(5),
                hello_frame_with_nonce("aft", PROTOCOL_VERSION, 5, Some(CANDIDATE_NONCE)),
            )
            .unwrap();
        assert_eq!(parse_error(&replayed[0])["code"], "swap_token_invalid");

        // The case only this gate covers: the incumbent has died mid-swap,
        // so its duplicate refusal is gone too, and without the gate a
        // key-holder would take the id's ACTIVE slot.
        handler.cleanup_connection(ConnectionId::new(1)).unwrap();
        let squatter = handler
            .handle_control(
                ConnectionId::new(6),
                hello_frame_with_nonce("aft", PROTOCOL_VERSION, 6, Some("forged")),
            )
            .unwrap();
        assert_eq!(parse_error(&squatter[0])["code"], "swap_token_invalid");
        assert!(
            registry.get_module("aft").unwrap().is_none(),
            "a squatter took the active slot of an id being swapped"
        );
    }

    /// Design mutation arm (iii). A reserved module's candidate presents a
    /// nonce the reserved gate has never seen (that gate holds the
    /// incumbent's), so the swap gate must run first or the candidate is
    /// refused `reserved_module` and a reserved module can never be swapped.
    #[test]
    fn reserved_module_candidate_is_admitted_ahead_of_the_reserved_gate() {
        let (registry, _supervisor, handler) = handler_with_incumbent("vault", true);

        let replies = handler
            .handle_control(
                ConnectionId::new(2),
                hello_frame_with_nonce("vault", PROTOCOL_VERSION, 2, Some(CANDIDATE_NONCE)),
            )
            .unwrap();

        assert_eq!(
            replies[0].header.ty,
            FrameType::HelloAck,
            "reserved candidate refused: {:?}",
            serde_json::from_slice::<Value>(&replies[0].body).ok()
        );
        assert_eq!(
            registry
                .get_candidate("vault")
                .unwrap()
                .unwrap()
                .connection_id,
            ConnectionId::new(2)
        );
    }

    /// With no swap open the gate is inert: the incumbent's reserved gate
    /// and duplicate refusal behave exactly as before.
    #[test]
    fn without_an_open_swap_the_ordinary_gates_decide() {
        let (registry, supervisor, handler) = handler_with_incumbent("vault", true);
        supervisor.close_swap("vault");

        let candidate = handler
            .handle_control(
                ConnectionId::new(2),
                hello_frame_with_nonce("vault", PROTOCOL_VERSION, 2, Some(CANDIDATE_NONCE)),
            )
            .unwrap();
        assert_eq!(parse_error(&candidate[0])["code"], "reserved_module");
        let duplicate = handler
            .handle_control(
                ConnectionId::new(3),
                hello_frame_with_nonce("vault", PROTOCOL_VERSION, 3, Some(INCUMBENT_NONCE)),
            )
            .unwrap();
        assert_eq!(parse_error(&duplicate[0])["code"], "duplicate_module_id");
        assert!(registry.get_candidate("vault").unwrap().is_none());
    }
}

/// `scope.sync` and `scope.describe` through the real control handler: who
/// may sync is decided by the registration and launch nonce of the module
/// connection, never by the request body.
mod scopes {
    mod close_reasons {
        include!("../route_close_reason_tests.rs");
    }
    mod idle_tests {
        use super::*;
        include!("../scope_idle_tests.rs");
    }
    mod route_open_timings {
        use super::*;
        include!("../route_open_timing_tests.rs");
    }
    mod route_open_refusal_log {
        use super::*;
        include!("../route_open_refusal_log_tests.rs");
    }
    mod activity {
        include!("../activity_scope_tests.rs");
    }
    mod delta {
        include!("../scope_handler_delta_tests.rs");
    }
    mod watch {
        include!("../scope_watch_tests.rs");
    }
    mod call_modes {
        include!("../scope_call_mode_tests.rs");
    }
    mod router_audit {
        use super::*;
        include!("router_audit_scope_tests.rs");
    }
    mod federated {
        include!("../scope_federated_tests.rs");
    }
    mod authorize {
        include!("../route_authorize_tests.rs");
    }
    #[tokio::test]
    async fn served_by_dispatch_bypasses_exhaustive_decoder_and_requires_registration() {
        let rig = rig().await;
        let body = serde_json::to_vec(&subc_control::ScopeServedByRequest::new(
            "s".into(),
            1,
            "tool".into(),
        ))
        .unwrap();
        assert!(serde_json::from_slice::<ModuleControlRequestFromModule>(&body).is_err());
        let (direct, _rx) = wide_ctx(999);
        for (ctx, expected) in [
            (&rig.owner, FrameType::Response),
            (&direct, FrameType::Error),
        ] {
            let frame =
                Frame::build(FrameType::Request, control_flags(), 0, 0, 90, body.clone()).unwrap();
            let replies = rig.handler.handle_control_frame(ctx, frame).await.unwrap();
            assert_eq!(replies[0].header.ty, expected);
            if expected == FrameType::Response {
                let reply: subc_control::ScopeServedByReply =
                    serde_json::from_slice(&replies[0].body).unwrap();
                assert_eq!(reply.answer, subc_control::ScopeServedByAnswer::Unknown);
            } else {
                assert_eq!(parse_error(&replies[0])["code"], "not_registered");
            }
        }
        let reply = call(
            &rig.handler,
            &rig.owner,
            &ModuleControlRequestFromModule::ScopeDescribe {
                owner: Principal::Reserved {
                    module_id: OWNER.into(),
                },
                scope_ref: "s".into(),
            },
        )
        .await;
        assert!(matches!(
            serde_json::from_slice::<ModuleControlResponseToModule>(&reply.body).unwrap(),
            ModuleControlResponseToModule::ScopeDescribe { .. }
        ));
    }

    use subc_protocol::scope::{
        ParentState, ScopeCarrier, ScopeKind, ScopeParent, ScopeRecordOutcome, ScopeStamp,
        ScopeStatus,
    };

    use super::*;

    const OWNER: &str = "prefrontal-core";

    fn head(scope_ref: &str, scope_epoch: u64) -> ScopeRecord {
        ScopeRecord::new(scope_ref, scope_epoch, ScopeKind::Head)
    }

    async fn call(
        handler: &ControlHandler,
        ctx: &RouteCtx,
        request: &ModuleControlRequestFromModule,
    ) -> Frame {
        let body = serde_json::to_vec(request).unwrap();
        let frame = Frame::build(FrameType::Request, control_flags(), 0, 0, 77, body).unwrap();
        let mut replies = handler.handle_control_frame(ctx, frame).await.unwrap();
        assert_eq!(replies.len(), 1, "{replies:?}");
        replies.pop().unwrap()
    }

    async fn sync(
        handler: &ControlHandler,
        ctx: &RouteCtx,
        generation: u64,
        scopes: Vec<ScopeRecord>,
    ) -> Result<ModuleControlResponseToModule, String> {
        let reply = call(
            handler,
            ctx,
            &ModuleControlRequestFromModule::ScopeSync { generation, scopes },
        )
        .await;
        match reply.header.ty {
            FrameType::Response => Ok(serde_json::from_slice(&reply.body).unwrap()),
            _ => Err(parse_error(&reply)["code"].as_str().unwrap().to_string()),
        }
    }

    async fn describe(
        handler: &ControlHandler,
        ctx: &RouteCtx,
        owner: &str,
        scope_ref: &str,
    ) -> ModuleControlResponseToModule {
        let reply = call(
            handler,
            ctx,
            &ModuleControlRequestFromModule::ScopeDescribe {
                owner: Principal::Reserved {
                    module_id: owner.to_string(),
                },
                scope_ref: scope_ref.to_string(),
            },
        )
        .await;
        assert_eq!(
            reply.header.ty,
            FrameType::Response,
            "{:?}",
            parse_error(&reply)
        );
        serde_json::from_slice(&reply.body).unwrap()
    }

    #[tokio::test]
    async fn scope_describe_never_echoes_bind_proof_or_hash() {
        const PROOF: &str = "BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc";
        const HASH: &str = "4bb06f8e4e3a7715d201d573d0aa423762e55dabd61a2c02278fa56cc6d294e0";
        let mut rig = rig().await;
        rig.sync(vec![session(1).with_bind_proof_sha256(Some(HASH.into()))])
            .await;
        let _bound = rig
            .bound(
                Some(AFT),
                PLEXUS,
                Some(
                    rig.selector("s", Some(1))
                        .with_bind_proof(Some(PROOF.into())),
                ),
            )
            .await;
        let reply = describe(&rig.handler, &rig.owner, OWNER, "s").await;
        let output = serde_json::to_string(&reply).unwrap();
        assert!(output.contains("\"status\":\"live\""));
        assert!(!output.contains(PROOF));
        assert!(!output.contains(HASH));
    }

    /// Register `module_id` on `connection` with `nonce`, returning its ctx.
    async fn module(
        handler: &ControlHandler,
        connection: u64,
        module_id: &str,
        nonce: Option<&str>,
    ) -> (RouteCtx, mpsc::Receiver<crate::router::OutboundFrame>) {
        let (ctx, mut rx) = route_ctx(ConnectionId::new(connection));
        hello_via_sink(
            handler,
            &ctx,
            &mut rx,
            hello_frame_with_nonce(module_id, PROTOCOL_VERSION, connection, nonce),
        )
        .await;
        (ctx, rx)
    }

    /// `direct` and every other client connection has no registration, so
    /// it can neither sync nor own a scope.
    #[tokio::test]
    async fn a_client_connection_cannot_sync_or_describe() {
        let handler = ControlHandler::new(Arc::new(Registry::default()));
        let (ctx, _rx) = route_ctx(ConnectionId::new(9));
        for request in [
            ModuleControlRequestFromModule::ScopeSync {
                generation: 1,
                scopes: vec![head("s", 1)],
            },
            ModuleControlRequestFromModule::ScopeDescribe {
                owner: Principal::Direct,
                scope_ref: "s".to_string(),
            },
        ] {
            let reply = call(&handler, &ctx, &request).await;
            assert_eq!(parse_error(&reply)["code"], "not_registered", "{request:?}");
        }
        assert!(
            !handler
                .scopes
                .read()
                .unwrap()
                .describe(
                    &Principal::Reserved {
                        module_id: OWNER.to_string()
                    },
                    "s"
                )
                .owner_synced
        );
    }

    /// A module the supervisor did not spawn registers without a launch
    /// nonce, so it is never an owner's current launch.
    #[tokio::test]
    async fn a_module_without_a_supervised_launch_cannot_sync() {
        let handler = ControlHandler::new(Arc::new(Registry::default()));
        let (ctx, _rx) = module(&handler, 1, OWNER, None).await;
        assert_eq!(
            sync(&handler, &ctx, 1, vec![head("s", 1)]).await,
            Err(error_codes::SCOPE_SYNC_NOT_AUTHORITY.to_string())
        );
    }

    #[tokio::test]
    async fn sync_authority_follows_the_supervisors_recorded_spawn_nonce_across_a_swap() {
        let supervisor = SupervisorHandle::new();
        supervisor.set_spawn_nonce(OWNER, "n1".to_string());
        let handler =
            ControlHandler::new(Arc::new(Registry::default())).with_supervisor(supervisor.clone());
        let (incumbent, _incumbent_rx) = module(&handler, 1, OWNER, Some("n1")).await;
        sync(&handler, &incumbent, 1, vec![head("s", 1)])
            .await
            .expect("the current launch syncs");

        // A swap candidate registers with the swap token and is refused
        // while the incumbent keeps syncing.
        supervisor.open_swap(OWNER, "n2".to_string());
        let (candidate, _candidate_rx) = module(&handler, 2, OWNER, Some("n2")).await;
        assert_eq!(
            sync(&handler, &candidate, 1, vec![head("x", 1)]).await,
            Err(error_codes::SCOPE_SYNC_NOT_AUTHORITY.to_string())
        );
        sync(&handler, &incumbent, 2, vec![head("s", 1)])
            .await
            .expect("the serving owner syncs during the swap");

        // The swap fails and is rolled back. The candidate never held sync
        // authority, and still cannot sync.
        supervisor.close_swap(OWNER);
        assert_eq!(
            sync(&handler, &candidate, 1, vec![head("x", 1)]).await,
            Err(error_codes::SCOPE_SYNC_NOT_AUTHORITY.to_string())
        );
        sync(&handler, &incumbent, 3, vec![head("s", 1)])
            .await
            .expect("the serving owner syncs after the rollback");
        handler.cleanup_connection(candidate.connection_id).unwrap();

        // A swap that cuts over. Promotion records the candidate's nonce as
        // the module's spawn nonce, which is what `set_spawn_nonce` does
        // here; the promoted connection then takes authority at any
        // generation and the superseded incumbent is refused.
        supervisor.open_swap(OWNER, "n3".to_string());
        let (promoted, _promoted_rx) = module(&handler, 3, OWNER, Some("n3")).await;
        supervisor.set_spawn_nonce(OWNER, "n3".to_string());
        let reply = sync(&handler, &promoted, 1, vec![head("s", 1)])
            .await
            .expect("the promoted launch takes authority");
        let ModuleControlResponseToModule::ScopeSync { results, .. } = reply else {
            panic!("unexpected reply {reply:?}");
        };
        assert_eq!(results[0].outcome, ScopeRecordOutcome::Unchanged);
        assert_eq!(
            sync(&handler, &incumbent, 4, Vec::new()).await,
            Err(error_codes::SCOPE_SYNC_NOT_AUTHORITY.to_string())
        );
    }

    /// Authority dies with its connection: the cleanup path releases it,
    /// so the owner's next connection takes it at any generation.
    #[tokio::test]
    async fn closing_the_authority_connection_frees_sync_authority() {
        let supervisor = SupervisorHandle::new();
        supervisor.set_spawn_nonce(OWNER, "n1".to_string());
        let handler =
            ControlHandler::new(Arc::new(Registry::default())).with_supervisor(supervisor.clone());
        let (first, _first_rx) = module(&handler, 1, OWNER, Some("n1")).await;
        sync(&handler, &first, 10, vec![head("s", 1)])
            .await
            .unwrap();
        handler.cleanup_connection(first.connection_id).unwrap();

        let (second, _second_rx) = module(&handler, 2, OWNER, Some("n1")).await;
        sync(&handler, &second, 1, vec![head("s", 1)])
            .await
            .expect("the next connection takes the released authority");
    }

    #[tokio::test]
    async fn module_goodbye_releases_scope_sync_authority_without_socket_close() {
        let supervisor = SupervisorHandle::new();
        supervisor.set_spawn_nonce(OWNER, "n1".to_string());
        let handler =
            ControlHandler::new(Arc::new(Registry::default())).with_supervisor(supervisor);
        let (first, _rx) = module(&handler, 1, OWNER, Some("n1")).await;
        sync(&handler, &first, 10, vec![head("s", 1)])
            .await
            .unwrap();
        handler
            .handle_control_frame(
                &first,
                Frame::build(FrameType::Goodbye, control_flags(), 0, 0, 4, Vec::new()).unwrap(),
            )
            .await
            .unwrap();
        let (second, _rx) = module(&handler, 2, OWNER, Some("n1")).await;
        sync(&handler, &second, 1, vec![head("s", 1)])
            .await
            .expect("GOODBYE releases authority even if the old socket remains open");
    }

    #[tokio::test]
    async fn describe_reports_the_incarnation_and_whether_the_owner_is_configured() {
        let registry = Arc::new(Registry::default());
        let supervisor_handle = SupervisorHandle::new();
        let supervisor = Supervisor::new_for_test(Arc::clone(&registry), RestartPolicy::default())
            .with_handle(supervisor_handle.clone())
            .with_daemon_incarnation("incarnation-7".to_string());
        // Configured with enabled: false, so the supervisor lists the
        // module without spawning a process for it.
        supervisor
            .supervise_configured(
                ModuleSpec {
                    script: None,
                    carry_containment: false,
                    module_id: OWNER.to_string(),
                    program: PathBuf::from("/nonexistent/prefrontal-core"),
                    args: Vec::new(),
                    env: Vec::new(),
                    reserved: false,
                    reserved_prefixes: Vec::new(),
                    protocol: ModuleProtocol::Subc,
                    overlap: Default::default(),
                },
                false,
            )
            .unwrap();
        supervisor_handle.set_spawn_nonce(OWNER, "n1".to_string());
        let handler = ControlHandler::new(Arc::clone(&registry)).with_supervisor(supervisor_handle);
        let (reader, _reader_rx) = module(&handler, 5, "reader", None).await;

        // Configured but not yet synced: a reader waits for the owner.
        let ModuleControlResponseToModule::ScopeDescribe {
            status,
            daemon_incarnation,
            owner_synced,
            owner_configured,
            scope,
            ..
        } = describe(&handler, &reader, OWNER, "s").await
        else {
            panic!("not a describe reply");
        };
        assert_eq!(status, ScopeStatus::NotLive);
        assert_eq!(daemon_incarnation, "incarnation-7");
        assert!(!owner_synced);
        assert!(owner_configured);
        assert!(scope.is_none());

        // Not a supervised module: the owner will never sync, and a reader
        // refuses rather than waits.
        let ModuleControlResponseToModule::ScopeDescribe {
            status,
            owner_configured,
            ..
        } = describe(&handler, &reader, "ghost", "s").await
        else {
            panic!("not a describe reply");
        };
        assert_eq!(status, ScopeStatus::NotLive);
        assert!(!owner_configured);

        // Live, with the stamp fields and the computed owner_authorized.
        let (owner, _owner_rx) = module(&handler, 6, OWNER, Some("n1")).await;
        sync(&handler, &owner, 1, vec![head("s", 4)]).await.unwrap();
        let ModuleControlResponseToModule::ScopeDescribe {
            status,
            scope_epoch,
            owner_synced,
            scope,
            ..
        } = describe(&handler, &reader, OWNER, "s").await
        else {
            panic!("not a describe reply");
        };
        assert_eq!(status, ScopeStatus::Live);
        assert_eq!(scope_epoch, Some(4));
        assert!(owner_synced);
        let stamp = scope.expect("a live scope carries its stamp");
        assert!(
            stamp.owner_authorized,
            "prefrontal-core is the default authority"
        );
        assert_eq!(stamp.kind, ScopeKind::Head);
    }

    #[tokio::test]
    async fn scope_authority_owners_decides_owner_authorized() {
        let supervisor = SupervisorHandle::new();
        supervisor.set_spawn_nonce("broca", "b1".to_string());
        let handler = ControlHandler::new(Arc::new(Registry::default()))
            .with_supervisor(supervisor)
            .with_scope_authority_owners(vec!["broca".to_string()]);
        let (broca, _rx) = module(&handler, 1, "broca", Some("b1")).await;
        let mut gated = head("s", 1);
        gated.attributes.agent_id = Some("agent".to_string());
        sync(&handler, &broca, 1, vec![gated]).await.unwrap();
        let ModuleControlResponseToModule::ScopeDescribe { scope, .. } =
            describe(&handler, &broca, "broca", "s").await
        else {
            panic!("not a describe reply");
        };
        assert!(scope.unwrap().owner_authorized);
    }

    /// With route admission, the stamp, the commit re-check and drains in
    /// place, the feature is advertised: the module ops in HELLO_ACK, and
    /// `scopes/v1` in HELLO_ACK and `server.describe`.
    #[tokio::test]
    async fn scope_ops_and_the_scopes_capability_are_advertised() {
        let handler = ControlHandler::new(Arc::new(Registry::default()));
        let (ctx, mut rx) = route_ctx(ConnectionId::new(1));
        let ack = hello_via_sink(
            &handler,
            &ctx,
            &mut rx,
            hello_frame("m", PROTOCOL_VERSION, 1),
        )
        .await;
        let ack = parse_ack(&ack);
        for op in [SCOPE_SYNC_OP, SCOPE_DESCRIBE_OP] {
            assert!(ack.subc_ops.iter().any(|o| o == op), "{:?}", ack.subc_ops);
        }
        assert!(ack.subc_capabilities.iter().any(|c| c == CAP_SCOPES_V1));

        let (client, _client_rx) = route_ctx(ConnectionId::new(2));
        let body = serde_json::to_vec(&ClientControlRequest::ServerDescribe {}).unwrap();
        let frame = Frame::build(FrameType::Request, control_flags(), 0, 0, 5, body).unwrap();
        let reply = handler
            .handle_control_frame(&client, frame)
            .await
            .unwrap()
            .pop()
            .unwrap();
        let ClientControlResponse::ServerDescribe { capabilities, .. } =
            serde_json::from_slice(&reply.body).unwrap()
        else {
            panic!("not a server.describe reply");
        };
        assert!(
            capabilities.iter().any(|c| c == CAP_SCOPES_V1),
            "{capabilities:?}"
        );
    }

    // ---- route admission, stamps, commit re-check and drains ----------

    const PLEXUS: &str = "plexus";
    const OTHER: &str = "other";
    const AFT: &str = "aft";
    const BROCA: &str = "broca";
    const MAGIC: &str = "magic-context";

    fn nonce(module_id: &str) -> String {
        format!("nonce-{module_id}")
    }

    fn wide_ctx(connection: u64) -> (RouteCtx, mpsc::Receiver<crate::router::OutboundFrame>) {
        let (tx, rx) = mpsc::channel(64);
        (
            RouteCtx {
                connection_id: ConnectionId::new(connection),
                egress: FrameSink::new(tx),
            },
            rx,
        )
    }

    /// A daemon with a configured owner (prefrontal-core) registered on its
    /// own module connection, two routable targets (plexus, other), and
    /// launch nonces minted for the modules that open routes as carriers.
    struct Rig {
        handler: ControlHandler,
        forwarding: Arc<ForwardingTable>,
        owner: RouteCtx,
        _owner_rx: mpsc::Receiver<crate::router::OutboundFrame>,
        modules: BTreeMap<String, (RouteCtx, mpsc::Receiver<crate::router::OutboundFrame>)>,
        generation: u64,
        next_connection: u64,
        _supervisor: Supervisor,
    }

    async fn rig() -> Rig {
        rig_with_flow_support(true).await
    }

    async fn rig_with_flow_support(flow_support: bool) -> Rig {
        rig_with_scope_capabilities(flow_support, false).await
    }

    async fn rig_with_scope_capabilities(flow_support: bool, run_support: bool) -> Rig {
        let mut provides = Vec::new();
        if flow_support {
            provides.push("flow-scopes/v1");
        }
        if run_support {
            provides.push("agent-run-scopes/v1");
        }
        rig_with_target_capabilities(&provides).await
    }

    fn hello_with_capabilities(module_id: &str, connection: u64, provides: &[&str]) -> Frame {
        let hello = hello_frame(module_id, PROTOCOL_VERSION, connection);
        let mut body: Value = serde_json::from_slice(&hello.body).unwrap();
        // Decoder versions alone cannot admit scoped calls: only a capability
        // declaration promises the required run or flow behaviour.
        body["manifest"]["provenance"] = serde_json::json!({"wire_crate_version": "0.29.0"});
        body["manifest"]["capabilities"] = serde_json::json!({"provides": provides});
        Frame::build(
            FrameType::Hello,
            control_flags(),
            0,
            0,
            connection,
            serde_json::to_vec(&body).unwrap(),
        )
        .unwrap()
    }

    async fn rig_with_target_capabilities(provides: &[&str]) -> Rig {
        let registry = Arc::new(Registry::default());
        let forwarding = Arc::new(ForwardingTable::default());
        let supervisor_handle = SupervisorHandle::new();
        let supervisor = Supervisor::new_for_test(Arc::clone(&registry), RestartPolicy::default())
            .with_handle(supervisor_handle.clone());
        supervisor
            .supervise_configured(
                ModuleSpec {
                    script: None,
                    carry_containment: false,
                    module_id: OWNER.to_string(),
                    program: PathBuf::from("/nonexistent/prefrontal-core"),
                    args: Vec::new(),
                    env: Vec::new(),
                    reserved: false,
                    reserved_prefixes: Vec::new(),
                    protocol: ModuleProtocol::Subc,
                    overlap: Default::default(),
                },
                false,
            )
            .unwrap();
        for module_id in [OWNER, AFT, BROCA, MAGIC] {
            supervisor_handle.set_spawn_nonce(module_id, nonce(module_id));
        }
        let handler =
            ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding))
                .with_supervisor(supervisor_handle);
        let (owner, mut owner_rx) = wide_ctx(1);
        hello_via_sink(
            &handler,
            &owner,
            &mut owner_rx,
            hello_frame_with_nonce(OWNER, PROTOCOL_VERSION, 1, Some(&nonce(OWNER))),
        )
        .await;
        let mut modules = BTreeMap::new();
        for (connection, module_id) in [(2, PLEXUS), (3, OTHER)] {
            let (ctx, mut rx) = wide_ctx(connection);
            let hello = hello_with_capabilities(module_id, connection, provides);
            hello_via_sink(&handler, &ctx, &mut rx, hello).await;
            modules.insert(module_id.to_string(), (ctx, rx));
        }
        Rig {
            handler,
            forwarding,
            owner,
            _owner_rx: owner_rx,
            modules,
            generation: 0,
            next_connection: 100,
            _supervisor: supervisor,
        }
    }

    fn carrier(module_id: &str, targets: Option<&[&str]>) -> ScopeCarrier {
        ScopeCarrier::new(Principal::Reserved {
            module_id: module_id.to_string(),
        })
        .with_targets(targets.map(|targets| targets.iter().map(|t| t.to_string()).collect()))
    }

    /// The scope most tests open under: aft carries to any module, broca
    /// only to plexus and other, and the owner delegates as agent-1.
    fn session(scope_epoch: u64) -> ScopeRecord {
        let mut record = head("s", scope_epoch);
        record.carriers = vec![carrier(AFT, None), carrier(BROCA, Some(&[PLEXUS, OTHER]))];
        record.attributes.agent_id = Some("agent-1".to_string());
        record.attributes.delegates = true;
        record
    }

    impl Rig {
        async fn sync(&mut self, scopes: Vec<ScopeRecord>) {
            self.generation += 1;
            sync(&self.handler, &self.owner, self.generation, scopes)
                .await
                .expect("the owner's sync is accepted");
        }

        fn selector(&self, scope_ref: &str, scope_epoch: Option<u64>) -> ScopeSelector {
            ScopeSelector::new(
                Principal::Reserved {
                    module_id: OWNER.to_string(),
                },
                scope_ref.to_string(),
                scope_epoch,
            )
        }

        fn open_frame(
            &mut self,
            opener: Option<&str>,
            target: &str,
            scope: Option<ScopeSelector>,
        ) -> (
            RouteCtx,
            mpsc::Receiver<crate::router::OutboundFrame>,
            Frame,
        ) {
            self.next_connection += 1;
            let (ctx, rx) = wide_ctx(self.next_connection);
            let body = serde_json::to_vec(&ClientControlRequest::RouteOpen {
                busy_refusals: None,
                target: RouteTarget::ToolProvider {
                    module_id: target.to_string(),
                },
                identity: BindIdentity::new("unit".to_string(), "session".to_string()),
                consumer_identity: opener.map(|module_id| ConsumerIdentity {
                    module_id: module_id.to_string(),
                    launch_nonce: nonce(module_id),
                }),
                consumer_capabilities: None,
                role_versions: None,
                admission_facts: None,
                scope,
            })
            .unwrap();
            let frame = Frame::build(
                FrameType::Request,
                control_flags(),
                0,
                0,
                self.next_connection,
                body,
            )
            .unwrap();
            (ctx, rx, frame)
        }

        /// Open and expect a refusal before anything is relayed.
        async fn refused(
            &mut self,
            opener: Option<&str>,
            target: &str,
            scope: Option<ScopeSelector>,
        ) -> String {
            self.refusal_body(opener, target, scope).await["code"]
                .as_str()
                .unwrap()
                .to_string()
        }

        async fn refusal_body(
            &mut self,
            opener: Option<&str>,
            target: &str,
            scope: Option<ScopeSelector>,
        ) -> Value {
            let (ctx, _rx, frame) = self.open_frame(opener, target, scope);
            let replies = tokio::time::timeout(
                Duration::from_secs(2),
                self.handler.handle_control_frame(&ctx, frame),
            )
            .await
            .expect("the open must be refused before waiting for a bind ack")
            .unwrap();
            assert_eq!(replies.len(), 1, "{replies:?}");
            assert_eq!(replies[0].header.ty, FrameType::Error);
            let (_, module_rx) = self.modules.get_mut(target).unwrap();
            assert!(
                module_rx.try_recv().is_err(),
                "a refused open relays nothing"
            );
            assert_eq!(self.forwarding.reserved_route_count().unwrap(), (0, 0));
            parse_error(&replies[0])
        }

        /// Start an open and return its task and the bind the target got.
        async fn relayed(
            &mut self,
            opener: Option<&str>,
            target: &str,
            scope: Option<ScopeSelector>,
        ) -> Relayed {
            self.relayed_with_legacy_root(opener, target, scope, None)
                .await
        }

        async fn relayed_with_legacy_root(
            &mut self,
            opener: Option<&str>,
            target: &str,
            scope: Option<ScopeSelector>,
            legacy_root: Option<&std::path::Path>,
        ) -> Relayed {
            let (ctx, rx, mut frame) = self.open_frame(opener, target, scope);
            if let Some(root) = legacy_root {
                let mut body: Value = serde_json::from_slice(&frame.body).unwrap();
                body["identity"]["project_root"] = serde_json::json!(root);
                frame.body = serde_json::to_vec(&body).unwrap();
            }
            let handler = self.handler.clone();
            let task_ctx = ctx.clone();
            let task = tokio::spawn(async move {
                handler
                    .handle_control_frame(&task_ctx, frame)
                    .await
                    .unwrap()
            });
            let (_, module_rx) = self.modules.get_mut(target).unwrap();
            let bind = tokio::time::timeout(Duration::from_secs(2), module_rx.recv())
                .await
                .expect("the target receives the relayed route.bind")
                .unwrap()
                .frame;
            Relayed {
                target: target.to_string(),
                client: ctx,
                client_rx: rx,
                task,
                bind,
            }
        }

        async fn ack(&self, relayed: &Relayed) {
            let (module, _) = &self.modules[&relayed.target];
            self.handler
                .handle_control_frame(module, route_bind_ack(relayed.bind.header.corr))
                .await
                .unwrap();
        }

        /// Open, ack and return the bound route.
        async fn bound(
            &mut self,
            opener: Option<&str>,
            target: &str,
            scope: Option<ScopeSelector>,
        ) -> Bound {
            let relayed = self.relayed(opener, target, scope).await;
            self.commit_relayed(relayed).await
        }

        async fn bound_with_legacy_root(
            &mut self,
            opener: Option<&str>,
            target: &str,
            scope: Option<ScopeSelector>,
            root: &std::path::Path,
        ) -> Bound {
            let relayed = self
                .relayed_with_legacy_root(opener, target, scope, Some(root))
                .await;
            self.commit_relayed(relayed).await
        }

        async fn commit_relayed(&self, relayed: Relayed) -> Bound {
            self.ack(&relayed).await;
            let Relayed {
                target,
                client,
                mut client_rx,
                task,
                bind,
            } = relayed;
            assert!(
                task.await.unwrap().is_empty(),
                "the open is answered by commit"
            );
            let (channel, epoch) = published_route(&client_rx.recv().await.unwrap().frame);
            Bound {
                target,
                client,
                client_rx,
                channel,
                epoch,
                bind,
            }
        }

        fn live(&self, route: &Bound) -> bool {
            matches!(
                self.forwarding
                    .lookup_data_route(route.client.connection_id, route.channel, route.epoch)
                    .unwrap(),
                DataRoute::Client(DataRouteState::Bound(_))
            )
        }
    }

    struct Relayed {
        target: String,
        client: RouteCtx,
        client_rx: mpsc::Receiver<crate::router::OutboundFrame>,
        task: tokio::task::JoinHandle<Vec<Frame>>,
        bind: Frame,
    }

    struct Bound {
        target: String,
        client: RouteCtx,
        client_rx: mpsc::Receiver<crate::router::OutboundFrame>,
        channel: u16,
        epoch: u32,
        bind: Frame,
    }

    impl Bound {
        /// The reason of the `route.closed` this client was sent, after
        /// checking it also got a GOODBYE on exactly this route.
        fn closed_reason(&mut self) -> RouteCloseReason {
            let mut reason = None;
            let mut goodbye = false;
            while let Ok(outbound) = self.client_rx.try_recv() {
                let frame = outbound.frame;
                match frame.header.ty {
                    FrameType::Goodbye => {
                        assert_eq!(
                            (frame.header.channel, frame.header.epoch),
                            (self.channel, self.epoch)
                        );
                        goodbye = true;
                    }
                    FrameType::Push => {
                        let ClientControlPush::RouteClosed {
                            reason: r,
                            module_id,
                            ..
                        } = serde_json::from_slice(&frame.body).unwrap()
                        else {
                            panic!("unexpected push");
                        };
                        assert_eq!(module_id, self.target);
                        reason = Some(r);
                    }
                    other => panic!("unexpected frame {other:?}"),
                }
            }
            assert!(goodbye, "the client is sent a GOODBYE for the closed route");
            reason.expect("the client is told why the route closed")
        }

        fn untouched(&mut self) -> bool {
            self.client_rx.try_recv().is_err()
        }

        fn stamp(&self) -> Option<ScopeStamp> {
            match serde_json::from_slice::<ModuleControlRequest>(&self.bind.body).unwrap() {
                ModuleControlRequest::RouteBind { scope, .. } => scope,
                other => panic!("expected a route.bind, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn only_the_owner_or_a_listed_carrier_is_admitted_and_a_targeted_carrier_only_to_its_modules(
    ) {
        let mut rig = rig().await;
        let mut record = session(1);
        record.carriers = vec![carrier(AFT, None), carrier(BROCA, Some(&[PLEXUS]))];
        record.child_owners = vec![Principal::Reserved {
            module_id: MAGIC.to_string(),
        }];
        rig.sync(vec![record]).await;
        let scope = || Some(rig_selector("s", Some(1)));

        // Admitted: the owner, a bare carrier to any module, a targeted
        // carrier to its listed module.
        rig.bound(Some(OWNER), PLEXUS, scope()).await;
        rig.bound(Some(AFT), OTHER, scope()).await;
        rig.bound(Some(BROCA), PLEXUS, scope()).await;

        // Refused scope_not_carrier: a targeted carrier to an unlisted
        // module, a module that is not listed at all (a child owner is not
        // a carrier), and a direct key-holder.
        for (opener, target) in [(Some(BROCA), OTHER), (Some(MAGIC), PLEXUS), (None, PLEXUS)] {
            assert_eq!(
                rig.refused(opener, target, scope()).await,
                error_codes::SCOPE_NOT_CARRIER,
                "{opener:?} -> {target}"
            );
        }
    }

    fn rig_selector(scope_ref: &str, scope_epoch: Option<u64>) -> ScopeSelector {
        ScopeSelector::new(
            Principal::Reserved {
                module_id: OWNER.to_string(),
            },
            scope_ref.to_string(),
            scope_epoch,
        )
    }

    #[tokio::test]
    async fn an_open_without_an_epoch_is_refused_the_owners_included() {
        let mut rig = rig().await;
        rig.sync(vec![session(1)]).await;
        for opener in [OWNER, AFT] {
            assert_eq!(
                rig.refused(Some(opener), PLEXUS, Some(rig.selector("s", None)))
                    .await,
                error_codes::SCOPE_EPOCH_REQUIRED,
                "{opener}"
            );
        }
    }

    #[tokio::test]
    async fn admission_separates_not_synced_not_live_and_ended() {
        let mut rig = rig().await;
        // Before the configured owner's first sync: retryable.
        let code = rig
            .refused(Some(AFT), PLEXUS, Some(rig_selector("s", Some(1))))
            .await;
        assert_eq!(code, error_codes::SCOPE_NOT_SYNCED);
        assert!(subc_protocol::error_codes::is_retryable_route_open(&code));

        // An owner that is not configured will never sync: terminal.
        let ghost = ScopeSelector::new(
            Principal::Reserved {
                module_id: "ghost".to_string(),
            },
            "s".to_string(),
            Some(1),
        );
        assert_eq!(
            rig.refused(Some(AFT), PLEXUS, Some(ghost)).await,
            error_codes::SCOPE_NOT_LIVE
        );

        rig.sync(vec![session(2)]).await;
        assert_eq!(
            rig.refused(Some(AFT), PLEXUS, Some(rig_selector("missing", Some(1))))
                .await,
            error_codes::SCOPE_NOT_LIVE
        );
        for epoch in [1, 3] {
            assert_eq!(
                rig.refused(Some(AFT), PLEXUS, Some(rig_selector("s", Some(epoch))))
                    .await,
                error_codes::SCOPE_ENDED,
                "epoch {epoch}"
            );
        }
        // Control: the live epoch is admitted.
        rig.bound(Some(AFT), PLEXUS, Some(rig_selector("s", Some(2))))
            .await;
    }

    #[tokio::test]
    async fn the_bind_is_stamped_and_owner_authorized_only_for_listed_owners() {
        let mut rig = rig().await;
        rig.sync(vec![session(1)]).await;
        let route = rig
            .bound(Some(AFT), PLEXUS, Some(rig_selector("s", Some(1))))
            .await;
        let stamp = route.stamp().expect("a scoped bind carries the stamp");
        assert_eq!(stamp.scope_ref, "s");
        assert_eq!(stamp.scope_epoch, 1);
        assert_eq!(stamp.kind, ScopeKind::Head);
        assert_eq!(stamp.attributes.agent_id.as_deref(), Some("agent-1"));
        assert!(stamp.attributes.delegates);
        assert!(stamp.owner_authorized);
        let unscoped = rig.bound(Some(AFT), PLEXUS, None).await;
        assert_eq!(unscoped.stamp(), None, "an unscoped open is not stamped");

        // broca owns a scope of its own on its own module connection; it is
        // not in scope_authority_owners, so its stamp is not authorized.
        let (broca, mut broca_rx) = wide_ctx(50);
        hello_via_sink(
            &rig.handler,
            &broca,
            &mut broca_rx,
            hello_frame_with_nonce(BROCA, PROTOCOL_VERSION, 50, Some(&nonce(BROCA))),
        )
        .await;
        sync(&rig.handler, &broca, 1, vec![head("b", 1)])
            .await
            .unwrap();
        let own = ScopeSelector::new(
            Principal::Reserved {
                module_id: BROCA.to_string(),
            },
            "b".to_string(),
            Some(1),
        );
        let route = rig.bound(Some(BROCA), PLEXUS, Some(own)).await;
        assert!(!route.stamp().unwrap().owner_authorized);
    }

    #[tokio::test]
    async fn an_authority_owners_flow_id_without_an_agent_is_stamped_verbatim_on_bind() {
        let mut rig = rig().await;
        let mut record = head("s", 1);
        record.carriers = vec![carrier(AFT, None)];
        let flow_id = "Flow:run-7/step_2!~";
        record.attributes.flow_id = Some(flow_id.to_string());
        let reply = sync(&rig.handler, &rig.owner, 1, vec![record])
            .await
            .unwrap();
        let ModuleControlResponseToModule::ScopeSync { results, .. } = reply else {
            panic!("not a sync reply");
        };
        assert_eq!(results[0].outcome, ScopeRecordOutcome::Created);
        let route = rig
            .bound(Some(AFT), PLEXUS, Some(rig_selector("s", Some(1))))
            .await;
        assert!(rig.live(&route), "the stamped bind committed");
        let stamp = route.stamp().expect("a flow scope carries a stamp");
        assert_eq!(stamp.attributes.flow_id.as_deref(), Some(flow_id));
        assert_eq!(stamp.attributes.agent_id, None);
        assert!(!stamp.attributes.delegates);
        assert!(stamp.owner_authorized);
        let unscoped = rig.bound(Some(AFT), PLEXUS, None).await;
        assert_eq!(unscoped.stamp(), None);
    }

    #[tokio::test]
    async fn flow_scope_refuses_a_0_29_target_without_flow_capability_and_relays_nothing() {
        let mut rig = rig_with_flow_support(false).await;
        let mut record = session(1);
        record.attributes.flow_id = Some("flow:7".to_string());
        rig.sync(vec![record]).await;
        for opener in [OWNER, AFT] {
            let body = rig
                .refusal_body(Some(opener), PLEXUS, Some(rig_selector("s", Some(1))))
                .await;
            assert_eq!(body["code"], "target_flow_unsupported");
            let message = body["message"].as_str().unwrap();
            for required in [PLEXUS, "flow-scopes/v1"] {
                assert!(message.contains(required), "{message}");
            }
        }
    }

    #[tokio::test]
    async fn flow_scope_admits_a_capable_target_and_preserves_flow_id_on_bind() {
        let mut rig = rig_with_flow_support(true).await;
        let mut record = session(1);
        record.attributes.flow_id = Some("flow:7".to_string());
        rig.sync(vec![record]).await;
        for opener in [OWNER, AFT] {
            let route = rig
                .bound(Some(opener), PLEXUS, Some(rig_selector("s", Some(1))))
                .await;
            assert!(rig.live(&route));
            assert_eq!(
                route.stamp().unwrap().attributes.flow_id.as_deref(),
                Some("flow:7")
            );
        }
    }

    #[tokio::test]
    async fn flow_scope_rechecks_the_relay_target_after_a_reconnect() {
        use std::future::Future;

        let mut rig = rig_with_flow_support(true).await;
        let mut record = session(1);
        record.attributes.flow_id = Some("flow:7".to_string());
        rig.sync(vec![record]).await;
        let (client, mut client_rx, frame) =
            rig.open_frame(Some(AFT), PLEXUS, Some(rig_selector("s", Some(1))));
        // Hold the route-open response permit so admission sees the first
        // target but relay reservation cannot capture an endpoint yet.
        for _ in 0..64 {
            client.egress.try_send(route_bind_ack(1)).unwrap();
        }
        let handler = rig.handler.clone();
        let mut open = Box::pin(handler.handle_control_frame(&client, frame));
        std::future::poll_fn(|cx| {
            assert!(open.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        assert_eq!(rig.forwarding.reserved_route_count().unwrap(), (0, 0));

        let old_connection = rig.modules[PLEXUS].0.connection_id;
        rig.handler.cleanup_connection(old_connection).unwrap();
        let (replacement, mut replacement_rx) = wide_ctx(200);
        let hello = hello_frame(PLEXUS, PROTOCOL_VERSION, 200);
        let mut body: Value = serde_json::from_slice(&hello.body).unwrap();
        body["manifest"]["provenance"] = serde_json::json!({"wire_crate_version": "0.29.0"});
        let hello = Frame::build(
            FrameType::Hello,
            control_flags(),
            0,
            0,
            200,
            serde_json::to_vec(&body).unwrap(),
        )
        .unwrap();
        hello_via_sink(&rig.handler, &replacement, &mut replacement_rx, hello).await;

        client_rx.try_recv().unwrap();
        let replies = tokio::time::timeout(Duration::from_secs(2), open)
            .await
            .expect("the replacement is refused without waiting for a bind ack")
            .unwrap();
        assert_eq!(replies.len(), 1);
        let body = parse_error(&replies[0]);
        assert_eq!(body["code"], "target_flow_unsupported");
        for required in [PLEXUS, "flow-scopes/v1"] {
            assert!(body["message"].as_str().unwrap().contains(required));
        }
        assert!(replacement_rx.try_recv().is_err(), "no bind is relayed");
        assert!(rig.modules.get_mut(PLEXUS).unwrap().1.try_recv().is_err());
        assert_eq!(rig.forwarding.reserved_route_count().unwrap(), (0, 0));
        assert!(rig
            .handler
            .registry
            .get_module_by_connection(replacement.connection_id)
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn scope_bind_proof_rechecks_current_hash_before_captured_relay() {
        use std::future::Future;
        const PROOF: &str = "BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc";
        const HASH: &str = "4bb06f8e4e3a7715d201d573d0aa423762e55dabd61a2c02278fa56cc6d294e0";
        const NEXT_HASH: &str = "2578ccf8645b2d1dc10c465eff843585970f3a7e22296a92cad55d489a272072";
        let mut rig = rig().await;
        let mut record = session(1).with_bind_proof_sha256(Some(HASH.into()));
        record.carriers.push(ScopeCarrier::new(Principal::Direct));
        rig.sync(vec![record.clone()]).await;
        let (client, mut rx, frame) = rig.open_frame(
            None,
            PLEXUS,
            Some(
                rig.selector("s", Some(1))
                    .with_bind_proof(Some(PROOF.into())),
            ),
        );
        // Hold egress reservation while the scope owner changes its bind-proof
        // hash, so a proof valid at admission must be checked again at relay.
        for _ in 0..64 {
            client.egress.try_send(route_bind_ack(1)).unwrap();
        }
        let handler = rig.handler.clone();
        let mut open = Box::pin(handler.handle_control_frame(&client, frame));
        std::future::poll_fn(|cx| {
            assert!(open.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        record.bind_proof_sha256 = Some(NEXT_HASH.into());
        rig.sync(vec![record]).await;
        rx.try_recv().unwrap();
        let replies = tokio::time::timeout(Duration::from_secs(2), open)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            parse_error(&replies[0])["code"],
            error_codes::SCOPE_BIND_PROOF_MISMATCH
        );
        assert!(!String::from_utf8_lossy(&replies[0].body).contains(PROOF));
        assert!(rig.modules.get_mut(PLEXUS).unwrap().1.try_recv().is_err());
        assert_eq!(rig.forwarding.reserved_route_count().unwrap(), (0, 0));
    }

    #[tokio::test]
    async fn scope_bind_proof_reserved_openers_ignore_proof() {
        const HASH: &str = "4bb06f8e4e3a7715d201d573d0aa423762e55dabd61a2c02278fa56cc6d294e0";
        let mut rig = rig().await;
        rig.sync(vec![session(1).with_bind_proof_sha256(Some(HASH.into()))])
            .await;
        for opener in [OWNER, AFT] {
            for proof in [None, Some("malformed+proof=")] {
                let route = rig
                    .bound(
                        Some(opener),
                        PLEXUS,
                        Some(
                            rig.selector("s", Some(1))
                                .with_bind_proof(proof.map(str::to_string)),
                        ),
                    )
                    .await;
                assert!(rig.live(&route));
            }
        }
    }

    #[tokio::test]
    async fn scope_bind_proof_reserved_opener_passes_hash_change_before_captured_relay() {
        use std::future::Future;
        const HASH: &str = "4bb06f8e4e3a7715d201d573d0aa423762e55dabd61a2c02278fa56cc6d294e0";
        const NEXT_HASH: &str = "2578ccf8645b2d1dc10c465eff843585970f3a7e22296a92cad55d489a272072";
        let mut rig = rig().await;
        rig.sync(vec![session(1).with_bind_proof_sha256(Some(HASH.into()))])
            .await;
        let (client, mut rx, frame) =
            rig.open_frame(Some(AFT), PLEXUS, Some(rig.selector("s", Some(1))));
        for _ in 0..64 {
            client.egress.try_send(route_bind_ack(1)).unwrap();
        }
        let handler = rig.handler.clone();
        let mut open = Box::pin(handler.handle_control_frame(&client, frame));
        std::future::poll_fn(|cx| {
            assert!(open.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        rig.sync(vec![
            session(1).with_bind_proof_sha256(Some(NEXT_HASH.into()))
        ])
        .await;
        rx.try_recv().unwrap();
        let (_, module_rx) = rig.modules.get_mut(PLEXUS).unwrap();
        let bind = std::future::poll_fn(|cx| {
            assert!(open.as_mut().poll(cx).is_pending());
            module_rx.poll_recv(cx)
        })
        .await
        .unwrap()
        .frame;
        handler
            .handle_control_frame(&rig.modules[PLEXUS].0, route_bind_ack(bind.header.corr))
            .await
            .unwrap();
        let replies = tokio::time::timeout(Duration::from_secs(2), open)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(replies.len(), 1);
        // Proof rechecking allowed the relay. Independent tag settlement
        // still asks the opener to retry after any same-epoch content change.
        assert_eq!(parse_error(&replies[0])["code"], error_codes::SCOPE_CHANGED);
        assert_eq!(rig.forwarding.reserved_route_count().unwrap(), (0, 0));
        while rig.modules.get_mut(PLEXUS).unwrap().1.try_recv().is_ok() {}
        let route = rig
            .bound(Some(AFT), PLEXUS, Some(rig.selector("s", Some(1))))
            .await;
        assert!(rig.live(&route));
    }

    #[tokio::test]
    async fn scope_without_flow_id_and_unscoped_routes_admit_a_target_without_flow_capability() {
        let mut rig = rig_with_flow_support(false).await;
        rig.sync(vec![session(1)]).await;
        let route = rig
            .bound(Some(AFT), PLEXUS, Some(rig_selector("s", Some(1))))
            .await;
        assert!(rig.live(&route));
        assert_eq!(route.stamp().unwrap().attributes.flow_id, None);
        let mut record = session(1);
        record.attributes.flow_id = Some("flow:7".to_string());
        rig.sync(vec![record]).await;
        let unscoped = rig.bound(Some(AFT), OTHER, None).await;
        assert!(rig.live(&unscoped));
        assert_eq!(unscoped.stamp(), None);
    }

    #[tokio::test]
    async fn a_same_epoch_flow_id_change_bumps_version_and_drains_all_scoped_routes() {
        let mut rig = rig().await;
        let mut record = session(1);
        record.attributes.flow_id = Some("flow:7".to_string());
        rig.sync(vec![record.clone()]).await;
        let before = rig.forwarding.published_scope_tag(OWNER, "s");
        let mut owner_route = rig
            .bound(Some(OWNER), PLEXUS, Some(rig_selector("s", Some(1))))
            .await;
        let mut carrier_route = rig
            .bound(Some(AFT), OTHER, Some(rig_selector("s", Some(1))))
            .await;
        let mut unscoped = rig.bound(Some(AFT), PLEXUS, None).await;
        rig.sync(vec![record.clone()]).await;
        assert_eq!(rig.forwarding.published_scope_tag(OWNER, "s"), before);
        assert!(rig.live(&owner_route) && owner_route.untouched());
        assert!(rig.live(&carrier_route) && carrier_route.untouched());

        record.attributes.flow_id = Some("flow:8".to_string());
        rig.sync(vec![record]).await;
        let after = rig.forwarding.published_scope_tag(OWNER, "s").unwrap();
        let before = before.unwrap();
        assert_eq!(after.scope_epoch, before.scope_epoch);
        assert!(after.version > before.version);
        for route in [&mut owner_route, &mut carrier_route] {
            assert!(!rig.live(route));
            assert_eq!(
                route.closed_reason(),
                RouteCloseReason::ScopeDelegationChanged
            );
        }
        assert!(rig.live(&unscoped) && unscoped.untouched());
        // Each provider also receives a GOODBYE for its drained route;
        // consume it before expecting the next route.bind on that sink.
        for target in [PLEXUS, OTHER] {
            let (_, module_rx) = rig.modules.get_mut(target).unwrap();
            let goodbye = module_rx
                .try_recv()
                .expect("the provider sees the drain")
                .frame;
            assert_eq!(goodbye.header.ty, FrameType::Goodbye);
            assert!(module_rx.try_recv().is_err());
        }
        let rebound = rig
            .bound(Some(AFT), PLEXUS, Some(rig_selector("s", Some(1))))
            .await;
        assert_eq!(
            rebound.stamp().unwrap().attributes.flow_id.as_deref(),
            Some("flow:8")
        );
    }

    /// The owner's sync lands between admission and the module's ack. The
    /// open is refused by name, the module's other routes stay up, and the
    /// reserved pair is released. Changed content is retryable; an ended
    /// scope is not.
    #[tokio::test]
    async fn a_scope_changed_or_ended_between_admission_and_commit_refuses_the_open() {
        let mut rig = rig().await;
        rig.sync(vec![session(1)]).await;
        let mut cotenant = rig
            .bound(Some(OWNER), PLEXUS, Some(rig_selector("s", Some(1))))
            .await;

        let mut changed = session(1);
        changed.child_owners.push(Principal::Reserved {
            module_id: MAGIC.to_string(),
        });
        let mut ended = None;
        for (code, next) in [
            (error_codes::SCOPE_CHANGED, vec![changed]),
            (error_codes::SCOPE_ENDED, Vec::new()),
        ] {
            let relayed = rig
                .relayed(Some(AFT), PLEXUS, Some(rig_selector("s", Some(1))))
                .await;
            let (bind_channel, bind_epoch) = route_bind_channel(&relayed.bind);
            ended = Some(next.is_empty());
            rig.sync(next).await;
            rig.ack(&relayed).await;
            let replies = relayed.task.await.unwrap();
            assert_eq!(replies.len(), 1, "{replies:?}");
            assert_eq!(parse_error(&replies[0])["code"], code);
            assert_eq!(
                subc_protocol::error_codes::is_retryable_route_open(code),
                code == error_codes::SCOPE_CHANGED
            );
            assert_eq!(rig.forwarding.reserved_route_count().unwrap(), (0, 0));
            // The module is told to drop just the binding it created.
            let (_, plexus_rx) = rig.modules.get_mut(PLEXUS).unwrap();
            // Collected, because ending the scope also closes the co-tenant
            // route, whose GOODBYE comes first.
            let mut goodbyes = Vec::new();
            while let Ok(outbound) = plexus_rx.try_recv() {
                assert_eq!(outbound.frame.header.ty, FrameType::Goodbye);
                goodbyes.push((outbound.frame.header.channel, outbound.frame.header.epoch));
            }
            assert!(
                goodbyes.contains(&(bind_channel, bind_epoch)),
                "{goodbyes:?}"
            );
            assert!(rig
                .handler
                .registry
                .get_module_by_connection(rig.modules[PLEXUS].0.connection_id)
                .unwrap()
                .is_some());
        }
        assert_eq!(ended, Some(true));
        // The co-tenant stayed up through the change, and closed only when
        // the scope ended, by the drain rule rather than by the commit.
        assert_eq!(cotenant.closed_reason(), RouteCloseReason::ScopeEnded);
    }

    /// Each row of the drain table on one set of routes: the owner's, a
    /// bare carrier's, and a targeted carrier's to each of its targets.
    #[tokio::test]
    async fn each_revocation_drains_exactly_the_affected_routes_with_its_own_reason() {
        struct Case {
            name: &'static str,
            change: fn(&mut ScopeRecord),
            /// Closed routes by index: owner->plexus, aft->plexus,
            /// broca->plexus, broca->other.
            closed: [Option<RouteCloseReason>; 4],
        }
        use RouteCloseReason::*;
        let cases = [
            Case {
                name: "a carrier entry removed",
                change: |r| {
                    r.carriers.retain(|c| {
                        c.principal
                            != Principal::Reserved {
                                module_id: AFT.to_string(),
                            }
                    })
                },
                closed: [None, Some(ScopeCarrierRemoved), None, None],
            },
            Case {
                name: "a target removed from a carrier",
                change: |r| r.carriers[1].targets = Some(vec![PLEXUS.to_string()]),
                closed: [None, None, None, Some(ScopeCarrierRemoved)],
            },
            Case {
                name: "a bare carrier narrowed to targets",
                change: |r| r.carriers[0].targets = Some(vec![OTHER.to_string()]),
                closed: [None, Some(ScopeCarrierRemoved), None, None],
            },
            Case {
                name: "delegates turned off",
                change: |r| r.attributes.delegates = false,
                closed: [Some(ScopeDelegationChanged); 4],
            },
            Case {
                name: "agent_id changed",
                change: |r| r.attributes.agent_id = Some("agent-2".to_string()),
                closed: [Some(ScopeDelegationChanged); 4],
            },
            Case {
                name: "a carrier added, child owners changed, the record re-sent",
                change: |r| {
                    r.carriers.push(carrier(MAGIC, None));
                    r.child_owners.push(Principal::Reserved {
                        module_id: MAGIC.to_string(),
                    });
                },
                closed: [None; 4],
            },
            Case {
                name: "a target added",
                change: |r| {
                    r.carriers[1]
                        .targets
                        .as_mut()
                        .unwrap()
                        .push("third".to_string())
                },
                closed: [None; 4],
            },
            Case {
                name: "delegates turned on",
                change: |r| r.attributes.delegates = true,
                closed: [None; 4],
            },
        ];
        for case in cases {
            let mut rig = rig().await;
            rig.sync(vec![session(1)]).await;
            let scope = || Some(rig_selector("s", Some(1)));
            let mut routes = [
                rig.bound(Some(OWNER), PLEXUS, scope()).await,
                rig.bound(Some(AFT), PLEXUS, scope()).await,
                rig.bound(Some(BROCA), PLEXUS, scope()).await,
                rig.bound(Some(BROCA), OTHER, scope()).await,
            ];
            let mut record = session(1);
            (case.change)(&mut record);
            rig.sync(vec![record]).await;
            for (index, expected) in case.closed.iter().enumerate() {
                let route = &mut routes[index];
                match expected {
                    Some(reason) => {
                        assert!(!rig.live(route), "{}: route {index} still live", case.name);
                        assert_eq!(
                            route.closed_reason(),
                            *reason,
                            "{}: route {index}",
                            case.name
                        );
                    }
                    None => {
                        assert!(rig.live(route), "{}: route {index} closed", case.name);
                        assert!(
                            route.untouched(),
                            "{}: route {index} was told something",
                            case.name
                        );
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn ending_or_replacing_a_scope_and_a_parent_ending_drain_every_route_under_it() {
        // Removed, and replaced by a higher epoch.
        for next in [Vec::new(), vec![session(2)]] {
            let mut rig = rig().await;
            rig.sync(vec![session(1)]).await;
            let mut route = rig
                .bound(Some(AFT), PLEXUS, Some(rig_selector("s", Some(1))))
                .await;
            rig.sync(next).await;
            assert!(!rig.live(&route));
            assert_eq!(route.closed_reason(), RouteCloseReason::ScopeEnded);
        }

        // A child whose parent ends: its routes close as parent-ended, the
        // child stays live, and routes under the parent close as ended.
        let mut rig = rig().await;
        let mut child = session(1);
        child.scope_ref = "child".to_string();
        child.kind = ScopeKind::Worker;
        child.parent = Some(ScopeParent::new(
            Principal::Reserved {
                module_id: OWNER.to_string(),
            },
            "s".to_string(),
            1,
        ));
        rig.sync(vec![session(1), child.clone()]).await;
        let mut child_route = rig
            .bound(Some(AFT), PLEXUS, Some(rig_selector("child", Some(1))))
            .await;
        assert_eq!(
            child_route.stamp().unwrap().parent_state,
            Some(ParentState::Linked)
        );
        rig.sync(vec![child]).await;
        assert!(!rig.live(&child_route));
        assert_eq!(
            child_route.closed_reason(),
            RouteCloseReason::ScopeParentEnded
        );
    }

    #[tokio::test]
    async fn re_sending_an_unchanged_record_drains_nothing_and_a_new_carrier_leaves_in_flight_calls(
    ) {
        let mut rig = rig().await;
        rig.sync(vec![session(1)]).await;
        let mut route = rig
            .bound(Some(AFT), PLEXUS, Some(rig_selector("s", Some(1))))
            .await;
        let before = rig.forwarding.published_scope_tag(OWNER, "s");
        rig.sync(vec![session(1)]).await;
        assert_eq!(rig.forwarding.published_scope_tag(OWNER, "s"), before);
        assert!(rig.live(&route) && route.untouched());

        // A call in flight on the route when another carrier is added. A
        // forwarded REQUEST holds one credit on the route's flow until the
        // module answers; the router takes it exactly like this.
        let DataRoute::Client(DataRouteState::Bound(binding)) = rig
            .forwarding
            .lookup_data_route(route.client.connection_id, route.channel, route.epoch)
            .unwrap()
        else {
            panic!("the route is bound");
        };
        binding.flow.acquire_tagged(9, false).await.unwrap();
        let mut widened = session(1);
        widened.carriers.push(carrier(MAGIC, None));
        rig.sync(vec![widened]).await;
        assert!(rig.live(&route) && route.untouched());
        let (_, plexus_rx) = rig.modules.get_mut(PLEXUS).unwrap();
        assert!(plexus_rx.try_recv().is_err(), "the module is told nothing");
        // The call's credit is still held on an open flow, so its answer
        // will be delivered: closing the route would have closed the flow.
        assert_eq!(binding.flow.in_flight(), 1);
        binding
            .flow
            .acquire_tagged(10, false)
            .await
            .expect("the flow is still open");
    }

    /// A swap's superseded endpoint keeps its routes until drained; ending
    /// the scope closes them there too.
    #[tokio::test]
    async fn ending_a_scope_drains_its_routes_on_a_superseded_endpoint() {
        let mut rig = rig().await;
        rig.sync(vec![session(1)]).await;
        let mut on_incumbent = rig
            .bound(Some(AFT), PLEXUS, Some(rig_selector("s", Some(1))))
            .await;

        // Swap plexus: register a candidate and cut over, leaving the
        // incumbent superseded with the route still on it.
        let (candidate, _candidate_rx) = wide_ctx(9);
        let registration = rig
            .handler
            .registry
            .register_candidate_with_control_ops(
                manifest(PLEXUS, PROTOCOL_VERSION),
                PROTOCOL_VERSION,
                candidate.connection_id,
                module_baseline_control_ops(),
            )
            .unwrap();
        rig.forwarding
            .register_candidate_module_connection(
                candidate.connection_id,
                PLEXUS.to_string(),
                PROTOCOL_VERSION,
                manifest_concurrency(&registration.manifest),
                candidate.egress.clone(),
            )
            .unwrap();
        rig.forwarding.cutover_candidate(PLEXUS).unwrap().unwrap();
        rig.handler
            .registry
            .promote_candidate(PLEXUS)
            .unwrap()
            .unwrap();
        assert!(rig.live(&on_incumbent), "cutover alone does not drain");

        rig.sync(Vec::new()).await;
        assert!(!rig.live(&on_incumbent));
        assert_eq!(on_incumbent.closed_reason(), RouteCloseReason::ScopeEnded);
        let (_, incumbent_rx) = rig.modules.get_mut(PLEXUS).unwrap();
        let goodbye = incumbent_rx
            .try_recv()
            .expect("the superseded endpoint is told")
            .frame;
        assert_eq!(goodbye.header.ty, FrameType::Goodbye);
    }
}
