use std::{
    collections::{BTreeMap, VecDeque},
    fmt, fs,
    ops::Deref,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};
use subc_test_support::TestTempDir;

use subc_control::{CatalogEntry, ClientControlRequest, ClientControlResponse};
use subc_daemon::{
    read_frame, write_frame, Frame, ModuleSpec, RestartPolicy, SupervisedModule, Supervisor,
    SupervisorHandle, SupervisorProcessLiveness,
};
use subc_protocol::{
    manifest::{
        CapabilityDeclarations, CapabilityNeed, CapabilityRequirement, Concurrency, ExecutionMode,
        IdentityScope, ManifestProvenance, ModuleManifest, ProviderRole, SelfSignalDeclaration,
        SelfSignalEffect, SelfSignalKind, SignalAnchor, SignalCadence, Tool,
    },
    session::{
        ModuleControlRequest, ModuleControlRequestFromModule, ModuleControlResponse,
        ModuleControlResponseToModule,
    },
    BindIdentity, ErrorBody, Flags, FrameType, ModuleHelloAckBody, ModuleHelloBody, Priority,
    RouteTarget, PROTOCOL_VERSION,
};
use tokio::{
    io::AsyncWriteExt,
    net::{
        tcp::{OwnedReadHalf, OwnedWriteHalf},
        TcpStream,
    },
    sync::mpsc,
    task::JoinHandle,
    time::{sleep, timeout, Instant},
};
use tracing::{
    field::{Field, Visit},
    Event, Subscriber,
};
use tracing_subscriber::{layer::Context, prelude::*, Layer};

mod common;
use common::{
    connect_authed_client, start_test_daemon_with_process_liveness_and_supervisor, TestDaemon,
};

const SETUP_TIMEOUT: Duration = Duration::from_secs(10);

struct TestServer {
    daemon: TestDaemon,
    process_liveness: Arc<SupervisorProcessLiveness>,
    supervisor_handle: SupervisorHandle,
}

impl TestServer {
    async fn start() -> Self {
        let _ = event_capture();
        let process_liveness = Arc::new(SupervisorProcessLiveness::new());
        let supervisor_handle = SupervisorHandle::new();
        let daemon = start_test_daemon_with_process_liveness_and_supervisor(
            "catalog-update-server",
            process_liveness.clone(),
            supervisor_handle.clone(),
        )
        .await;
        Self {
            daemon,
            process_liveness,
            supervisor_handle,
        }
    }

    fn supervisor(&self) -> Supervisor {
        Supervisor::new(
            Arc::clone(&self.registry),
            RestartPolicy::new(0, Duration::ZERO),
        )
        .with_process_liveness(Arc::clone(&self.process_liveness))
        .with_forwarding(Arc::clone(&self.forwarding))
        .with_handle(self.supervisor_handle.clone())
        .with_drain_timeout(Duration::from_millis(25))
        .with_connection_file_path(self.connection_file_path.clone())
    }

    fn stub_events_path(&self, label: &str) -> PathBuf {
        self.temp_dir.join(format!("{label}-events.jsonl"))
    }
}

impl Deref for TestServer {
    type Target = TestDaemon;

    fn deref(&self) -> &Self::Target {
        &self.daemon
    }
}

#[derive(Debug, Clone, Copy)]
struct RoutePair {
    client_channel: u16,
    client_epoch: u32,
    module_channel: u16,
    module_epoch: u32,
}

#[derive(Clone, Default)]
struct EventCapture {
    events: Arc<Mutex<Vec<CapturedEvent>>>,
}

#[derive(Clone, Debug)]
struct CapturedEvent {
    target: String,
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

fn event_capture() -> &'static EventCapture {
    static CAPTURE: OnceLock<EventCapture> = OnceLock::new();
    CAPTURE.get_or_init(|| {
        let capture = EventCapture::default();
        tracing::subscriber::set_global_default(
            tracing_subscriber::registry().with(capture.clone()),
        )
        .expect("catalog_update test process installs one tracing subscriber");
        capture
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn catalog_update_refreshes_catalog_without_disrupting_bound_routes() {
    let server = TestServer::start().await;
    let module_id = "catalog-update-provider";
    let mut module = connect_endpoint(&server, "module").await;
    let provenance = ManifestProvenance {
        build_git_sha: Some("0123456789abcdef0123456789abcdef01234567".to_string()),
        build_git_sha_absence_reason: None,
        build_lock_digest: Some("lock-digest".to_string()),
        wire_crate_version: Some("0.13.0".to_string()),
        store_schema_version: Some("3".to_string()),
    };
    let mut initial_manifest =
        tool_provider_manifest(module_id, &["a", "b"], Concurrency::ModuleManaged);
    initial_manifest.provenance = Some(provenance.clone());
    let hello_ack = register_module(&server, &mut module, initial_manifest, 101).await;
    assert!(hello_ack.subc_ops.contains(&"catalog.update".to_string()));

    let (initial_generation, initial_modules) = catalog_list(&server, Some(module_id), 201).await;
    assert_tool_names(&initial_modules[0], &["a", "b"]);

    let project = TestProject::new("catalog-update-route");
    let mut client = connect_endpoint(&server, "client").await;
    let route = open_route(&mut client, &mut module, &project, module_id, 301).await;

    let in_flight_body = br#"{"jsonrpc":"2.0","id":"in-flight","method":"a"}"#;
    client
        .send(&data_frame(
            FrameType::Request,
            route.client_channel,
            route.client_epoch,
            401,
            in_flight_body,
        ))
        .await;
    let forwarded = module
        .inbox
        .wait_for(SETUP_TIMEOUT, "in-flight module Request", |frame| {
            frame.header.ty == FrameType::Request
                && frame.header.channel == route.module_channel
                && frame.header.corr == 401
        })
        .await;
    assert_eq!(forwarded.body, in_flight_body);

    module
        .send(&catalog_update_frame(
            501,
            vec![tool_provider_role(&["a", "c"], Concurrency::ModuleManaged)],
        ))
        .await;
    let update_ack = module
        .inbox
        .wait_for(SETUP_TIMEOUT, "catalog.update ack", |frame| {
            frame.header.ty == FrameType::Response
                && frame.header.channel == 0
                && frame.header.corr == 501
        })
        .await;
    assert_eq!(
        serde_json::from_slice::<ModuleControlResponseToModule>(&update_ack.body).unwrap(),
        ModuleControlResponseToModule::CatalogUpdate {}
    );

    let (updated_generation, updated_modules) = catalog_list(&server, Some(module_id), 202).await;
    assert!(updated_generation > initial_generation);
    assert_tool_names(&updated_modules[0], &["a", "c"]);
    assert_eq!(
        server
            .registry
            .get_module(module_id)
            .expect("registry query succeeds")
            .expect("catalog.update keeps the registration")
            .manifest
            .provenance,
        Some(provenance),
        "catalog.update must preserve HELLO provenance inherited by its struct update"
    );
    assert_eq!(server.forwarding.active_binding_count().unwrap(), 1);
    assert!(server
        .forwarding
        .has_route_channel(route.client_channel)
        .unwrap());

    let in_flight_response = br#"{"jsonrpc":"2.0","id":"in-flight","result":"ok"}"#;
    module
        .send(&data_frame(
            FrameType::Response,
            route.module_channel,
            route.module_epoch,
            401,
            in_flight_response,
        ))
        .await;
    let delivered = client
        .inbox
        .wait_for(SETUP_TIMEOUT, "in-flight client Response", |frame| {
            frame.header.ty == FrameType::Response
                && frame.header.channel == route.client_channel
                && frame.header.corr == 401
        })
        .await;
    assert_eq!(delivered.body, in_flight_response);
    client
        .inbox
        .assert_no_buffered_match("route GOODBYE", |frame| {
            frame.header.ty == FrameType::Goodbye
        });

    module
        .send(&catalog_update_frame(
            502,
            vec![tool_provider_role(&["a", "c"], Concurrency::Serial)],
        ))
        .await;
    let concurrency_error = read_control_error(&mut module, 502).await;
    assert_eq!(concurrency_error.code, "catalog_update_frozen_field");

    module.send(&catalog_update_frame(503, Vec::new())).await;
    let empty_error = read_control_error(&mut module, 503).await;
    assert_eq!(empty_error.code, "catalog_update_frozen_field");

    let mut unregistered = connect_endpoint(&server, "unregistered").await;
    unregistered
        .send(&catalog_update_frame(
            504,
            vec![tool_provider_role(&["squat"], Concurrency::ModuleManaged)],
        ))
        .await;
    let not_registered = read_control_error(&mut unregistered, 504).await;
    assert_eq!(not_registered.code, "not_registered");

    let mut supervision_only = connect_endpoint(&server, "supervision-only").await;
    register_module(
        &server,
        &mut supervision_only,
        supervision_only_manifest("catalog-update-supervision-only"),
        601,
    )
    .await;
    supervision_only
        .send(&catalog_update_frame(
            602,
            vec![tool_provider_role(
                &["became-routable"],
                Concurrency::ModuleManaged,
            )],
        ))
        .await;
    let routability_error = read_control_error(&mut supervision_only, 602).await;
    assert_eq!(routability_error.code, "catalog_update_frozen_field");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn declared_not_ready_refuses_without_relay_until_catalog_update_marks_ready() {
    let server = TestServer::start().await;
    let supervisor = server.supervisor();
    let module_id = "declared-not-ready-provider";
    let update_path = server.temp_dir.join("publish-ready");
    let (module, events_path) = spawn_ready_stub(
        &server,
        &supervisor,
        module_id,
        "declared-not-ready",
        Some(&update_path),
    )
    .await;

    let registration = server
        .registry
        .get_module(module_id)
        .unwrap()
        .expect("stub registered");
    assert!(!registration.ready);
    let (_, initial_catalog) = catalog_list(&server, Some(module_id), 710).await;
    assert!(!initial_catalog[0].ready);

    let project = TestProject::new("declared-not-ready-first-open");
    let mut client = connect_endpoint(&server, "readiness-client").await;
    let first = route_open_terminal(&mut client, &project, module_id, 711).await;
    assert!(
        !stub_events(&events_path)
            .iter()
            .any(|event| event["kind"] == "attach"),
        "registered ready:false module must receive no route.bind; stub journal: {:?}",
        stub_events(&events_path)
    );
    assert_eq!(first.header.ty, FrameType::Error);
    let first_error: ErrorBody = serde_json::from_slice(&first.body).unwrap();
    assert_eq!(first_error.code, "module_warming");
    assert_eq!(
        first_error.detail,
        Some(serde_json::json!({"reason": "declared_not_ready"}))
    );

    fs::write(&update_path, b"ready").unwrap();
    wait_for_stub_event(&events_path, |event| {
        event["kind"] == "catalog_ready_update_sent"
    })
    .await;
    wait_for_stub_event(&events_path, |event| {
        event["kind"] == "catalog_ready_update_ack"
    })
    .await;

    let second = route_open_terminal(&mut client, &project, module_id, 713).await;
    assert_eq!(
        second.header.ty,
        FrameType::Response,
        "catalog.update ready:true must make the next route.open bind: {}",
        String::from_utf8_lossy(&second.body)
    );
    let response: ClientControlResponse = serde_json::from_slice(&second.body).unwrap();
    assert!(matches!(response, ClientControlResponse::RouteOpen { .. }));
    wait_for_stub_event(&events_path, |event| event["kind"] == "attach").await;
    wait_for_registration_ready(&server, module_id, true).await;
    let (_, updated_catalog) = catalog_list(&server, Some(module_id), 712).await;
    assert!(updated_catalog[0].ready);

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn declared_not_ready_and_supervised_absence_are_observably_distinct() {
    let server = TestServer::start().await;
    let supervisor = server.supervisor();
    let declared_id = "declared-not-ready-discrimination";
    let (declared, _events_path) = spawn_ready_stub(
        &server,
        &supervisor,
        declared_id,
        "declared-not-ready-discrimination",
        None,
    )
    .await;

    let absent_id = "supervised-not-registered-discrimination";
    let absent_ready = server.temp_dir.join("supervised-absent-ready");
    let absent = supervisor
        .spawn(ModuleSpec {
            module_id: absent_id.to_string(),
            program: PathBuf::from(env!("CARGO_BIN_EXE_fake-aft-stub")),
            args: Vec::new(),
            env: vec![
                ("FAKE_AFT_MODULE_ID".to_string(), absent_id.to_string()),
                ("FAKE_AFT_NEVER_CONNECT".to_string(), "1".to_string()),
                (
                    "FAKE_AFT_NEVER_CONNECT_READY_PATH".to_string(),
                    absent_ready.to_string_lossy().into_owned(),
                ),
            ],
            reserved: false,
            reserved_prefixes: Vec::new(),
            protocol: subc_control::ModuleProtocol::Subc,
            overlap: Default::default(),
        })
        .unwrap();
    wait_for_path(&absent_ready).await;

    let mut client = connect_endpoint(&server, "discrimination-client").await;
    let declared_project = TestProject::new("declared-discrimination");
    let declared_frame =
        route_open_terminal(&mut client, &declared_project, declared_id, 720).await;
    let declared_error: ErrorBody = serde_json::from_slice(&declared_frame.body).unwrap();
    let absent_project = TestProject::new("absent-discrimination");
    let absent_frame = route_open_terminal(&mut client, &absent_project, absent_id, 721).await;
    let absent_error: ErrorBody = serde_json::from_slice(&absent_frame.body).unwrap();

    assert_eq!(declared_error.code, "module_warming");
    assert_eq!(absent_error.code, "module_warming");

    let counters = server_describe_counters(&server, 722).await;
    assert_eq!(
        counters["route_open_refused_by_code"]["module_warming_declared_not_ready"],
        1
    );
    assert_eq!(counters["route_open_refused_by_code"]["module_warming"], 1);

    let events = event_capture().events();
    let declared_log = refusal_event(&events, declared_id);
    assert_eq!(
        declared_log.fields.get("reason"),
        Some(&"\"declared_not_ready\"".to_string())
    );
    assert!(!declared_log.fields.contains_key("state"));
    let absent_log = refusal_event(&events, absent_id);
    // Both refusals name their check; what tells them apart is the value.
    assert_eq!(
        absent_log.fields.get("reason"),
        Some(&"\"supervised_not_registered\"".to_string())
    );
    assert_eq!(absent_log.fields.get("state"), Some(&"running".to_string()));
    assert_eq!(absent_log.fields.get("enabled"), Some(&"true".to_string()));
    assert_eq!(absent_log.fields.get("live"), Some(&"false".to_string()));

    assert_eq!(
        declared_error.detail,
        Some(serde_json::json!({"reason": "declared_not_ready"}))
    );
    assert_eq!(absent_error.detail, None);

    declared.stop().await.unwrap();
    absent.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hello_self_signals_are_mirrored_and_missing_axes_are_refused() {
    let server = TestServer::start().await;
    let mut module = connect_endpoint(&server, "self-signal-module").await;
    let mut manifest = supervision_only_manifest("self-signal-module");
    let declarations = vec![
        SelfSignalDeclaration {
            name: "provider_usage_poller".to_string(),
            kind: SelfSignalKind::Poller,
            effect: SelfSignalEffect::Observe,
            anchored_to: SignalAnchor::FixedInterval,
            cadence: Some(SignalCadence::Literal {
                interval_ms: 300_000,
            }),
            domain: Some("provider-usage".to_string()),
            note: None,
        },
        SelfSignalDeclaration {
            name: "claude_keepalive".to_string(),
            kind: SelfSignalKind::Keepalive,
            effect: SelfSignalEffect::Mutate,
            anchored_to: SignalAnchor::Event {
                event: "window_expiry".to_string(),
            },
            cadence: Some(SignalCadence::Derived {
                source: "capacity_runtime.effective_cadence_ms".to_string(),
            }),
            domain: Some("provider-usage".to_string()),
            note: Some("Keeps the provider session alive at the window boundary.".to_string()),
        },
    ];
    manifest.self_signals = Some(declarations.clone());
    register_module(&server, &mut module, manifest, 701).await;

    let (_, modules) = catalog_list(&server, Some("self-signal-module"), 702).await;
    assert_eq!(modules.len(), 1);
    assert_eq!(modules[0].self_signals, Some(declarations));
    assert_eq!(
        modules[0]
            .self_signals
            .as_ref()
            .expect("catalog entry keeps self-signal declarations")[1]
            .effect,
        SelfSignalEffect::Mutate,
        "catalog.list must preserve a mutating signal's declared effect"
    );

    let mut invalid_module = connect_endpoint(&server, "invalid-self-signal-module").await;
    let invalid_manifest = supervision_only_manifest("invalid-self-signal-module");
    let mut body = serde_json::to_value(ModuleHelloBody {
        manifest: invalid_manifest,
        protocol_ver: PROTOCOL_VERSION,
        control_ops: None,
        launch_nonce: None,
    })
    .expect("invalid HELLO base serializes");
    body["manifest"]["self_signals"] = serde_json::json!([{
        "name": "missing_effect",
        "kind": "poller",
        "anchored_to": "fixed_interval"
    }]);
    invalid_module
        .send(
            &Frame::build(
                FrameType::Hello,
                control_flags(),
                0,
                0,
                703,
                serde_json::to_vec(&body).expect("invalid HELLO serializes"),
            )
            .expect("invalid HELLO frame builds"),
        )
        .await;
    let error = read_control_error(&mut invalid_module, 703).await;
    assert_eq!(error.code, "invalid_manifest");
    assert!(error.message.contains("invalid-self-signal-module"));
    assert!(error.message.contains("self_signals[0]"));
    assert!(error.message.contains("effect"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hello_with_fields_absent_manifest_registers_and_serves_catalog_list() {
    let server = TestServer::start().await;
    let module_id = "fields-absent-provider";
    let mut module = connect_endpoint(&server, "module").await;

    let manifest = ModuleManifest::builder(module_id, "1.0.0")
        .provides(vec![tool_provider_role(
            &["test_tool"],
            Concurrency::ModuleManaged,
        )])
        .build();
    assert!(manifest.trust_tier.is_none());
    assert!(manifest.consumes.is_empty());
    assert!(manifest.bindings.is_none());
    assert!(manifest.ready.is_none());

    let hello_body = serde_json::to_value(&ModuleHelloBody {
        manifest: manifest.clone(),
        protocol_ver: PROTOCOL_VERSION,
        control_ops: None,
        launch_nonce: None,
    })
    .unwrap();
    let manifest_obj = hello_body.get("manifest").unwrap();
    assert!(manifest_obj.get("trust_tier").is_none());
    assert!(manifest_obj.get("consumes").is_none());
    assert!(manifest_obj.get("bindings").is_none());
    assert!(manifest_obj.get("ready").is_none());

    let hello_ack = register_module(&server, &mut module, manifest, 101).await;
    assert_eq!(hello_ack.negotiated_ver, PROTOCOL_VERSION);
    assert!(
        server
            .registry
            .get_module(module_id)
            .unwrap()
            .expect("module registered")
            .ready,
        "a HELLO without ready must preserve the pre-field ready behavior"
    );

    let (_generation, modules) = catalog_list(&server, Some(module_id), 201).await;
    assert_eq!(modules.len(), 1);
    assert_eq!(modules[0].module_id, module_id);
    assert!(modules[0].ready);
    assert_tool_names(&modules[0], &["test_tool"]);

    let project = TestProject::new("ready-field-absent");
    let mut client = connect_endpoint(&server, "ready-field-absent-client").await;
    let _route = open_route(&mut client, &mut module, &project, module_id, 202).await;
}

// The premise `ck upgrade` rests on when it restarts the daemon before the
// modules: a new daemon must register a module built before the manifest
// diet, whose HELLO still carries `trust_tier`, `consumes` and `bindings`
// with values. The bytes here are what subc-protocol 0.18 serialised, kept
// as a literal so no builder in this tree can quietly modernise them. If
// this ever reddens, daemon-first ordering strands every not-yet-upgraded
// module at registration, and the failure would otherwise be read as the
// ordering being wrong rather than the premise having moved.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_daemon_registers_a_pre_diet_manifest_with_its_fields_present() {
    let server = TestServer::start().await;
    let module_id = "pre-diet-provider";
    let mut module = connect_endpoint(&server, "module").await;

    // Taken from the module_hello_body golden that pinned the 0.18 wire,
    // with the module id and tool name changed and `consumes` present as
    // the empty list every pre-diet builder emitted.
    let body = format!(
        r#"{{"manifest":{{"bindings":{{"identity":{{"optional":["session"],"requires":["project"]}},"storage":{{"kind":"sqlite","owns_schema":true,"scope":"project"}},"vault_grants":[]}},"consumes":[],"module_id":"{module_id}","module_version":"0.9.0","protocol_ver":{PROTOCOL_VERSION},"provides":[{{"concurrency":"module_managed","emits_push":true,"identity_scope":["project","session"],"role":"tool_provider","sub_supervises":true,"tools":[{{"execution_mode":"pure","name":"legacy_tool","schema":{{"required":["id"],"type":"object"}}}}]}}],"trust_tier":"first_party"}},"protocol_ver":{PROTOCOL_VERSION}}}"#
    );
    // The literal must actually carry the fields, or the test proves nothing.
    let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
    for field in ["trust_tier", "consumes", "bindings"] {
        assert!(
            parsed["manifest"].get(field).is_some(),
            "fixture lost the pre-diet field {field}"
        );
    }

    let corr = 111;
    module
        .send(
            &Frame::build(
                FrameType::Hello,
                control_flags(),
                0,
                0,
                corr,
                body.into_bytes(),
            )
            .unwrap(),
        )
        .await;
    let ack_frame = module
        .inbox
        .wait_for(SETUP_TIMEOUT, "HELLO_ACK", |frame| {
            frame.header.channel == 0
                && frame.header.corr == corr
                && matches!(frame.header.ty, FrameType::HelloAck | FrameType::Error)
        })
        .await;
    assert_eq!(
        ack_frame.header.ty,
        FrameType::HelloAck,
        "a pre-diet HELLO must register on a new daemon; got {}",
        String::from_utf8_lossy(&ack_frame.body)
    );
    let ack: ModuleHelloAckBody = serde_json::from_slice(&ack_frame.body).unwrap();
    assert_eq!(ack.negotiated_ver, PROTOCOL_VERSION);

    let (_generation, modules) = catalog_list(&server, Some(module_id), 211).await;
    assert_eq!(modules.len(), 1);
    assert_tool_names(&modules[0], &["legacy_tool"]);
}

// A module that declares a capability `need: required` is held not-ready, and
// refused `module_warming` at route.open, while no registered module provides
// that capability. The hold lifts as soon as a provider registers, with no
// restart of either module.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn required_capability_unprovided_refuses_until_a_provider_registers() {
    let server = TestServer::start().await;
    let consumer_id = "reqcap-consumer";
    let mut consumer = connect_endpoint(&server, "reqcap-consumer").await;
    register_module(
        &server,
        &mut consumer,
        capability_manifest(
            consumer_id,
            &[],
            &[
                ("reqcap-zeta/v1", CapabilityNeed::Required),
                ("reqcap-identity/v1", CapabilityNeed::Required),
            ],
        ),
        101,
    )
    .await;

    // Lexicographically first unprovided capability, not declaration order.
    let expected_detail = serde_json::json!({
        "reason": "required_capability_unprovided",
        "capability": "reqcap-identity/v1",
    });
    let (_, catalog) = catalog_list(&server, Some(consumer_id), 102).await;
    assert!(
        !catalog[0].ready,
        "catalog must report the effective readiness"
    );
    assert_eq!(
        serde_json::to_value(&catalog[0].not_ready).unwrap(),
        expected_detail
    );

    let project = TestProject::new("reqcap-unprovided");
    let mut client = connect_endpoint(&server, "reqcap-client").await;
    let refused =
        route_open_expect_refused(&mut client, &mut consumer, &project, consumer_id, 103).await;
    assert_eq!(refused.code, "module_warming");
    assert_eq!(refused.detail, Some(expected_detail));

    let counters = server_describe_counters(&server, 104).await;
    assert_eq!(
        counters["route_open_refused_by_code"]["module_warming_required_capability_unprovided"],
        1
    );
    let events = event_capture().events();
    let log = refusal_event(&events, consumer_id);
    assert_eq!(
        log.fields.get("reason"),
        Some(&"\"required_capability_unprovided\"".to_string())
    );
    assert_eq!(
        log.fields.get("capability"),
        Some(&"reqcap-identity/v1".to_string())
    );

    // One provider registering both capabilities lifts the hold; the consumer
    // is not restarted or re-registered.
    let mut provider = connect_endpoint(&server, "reqcap-provider").await;
    register_module(
        &server,
        &mut provider,
        capability_manifest(
            "reqcap-provider",
            &["reqcap-identity/v1", "reqcap-zeta/v1"],
            &[],
        ),
        105,
    )
    .await;
    let (_, catalog) = catalog_list(&server, Some(consumer_id), 106).await;
    assert!(catalog[0].ready);
    assert_eq!(catalog[0].not_ready, None);
    route_open_expect_bound(&mut client, &mut consumer, &project, consumer_id, 107).await;
}

// A provider going away holds NEW opens to its consumer, and leaves routes
// already bound to the consumer alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn required_capability_provider_leaving_refuses_new_opens_but_keeps_bound_routes() {
    let server = TestServer::start().await;
    let consumer_id = "reqcap-leave-consumer";
    let provider_id = "reqcap-leave-provider";
    let mut provider = connect_endpoint(&server, "reqcap-leave-provider").await;
    register_module(
        &server,
        &mut provider,
        capability_manifest(provider_id, &["reqcap-leave/v1"], &[]),
        201,
    )
    .await;
    let mut consumer = connect_endpoint(&server, "reqcap-leave-consumer").await;
    register_module(
        &server,
        &mut consumer,
        capability_manifest(
            consumer_id,
            &[],
            &[("reqcap-leave/v1", CapabilityNeed::Required)],
        ),
        202,
    )
    .await;

    let project = TestProject::new("reqcap-leave");
    let mut client = connect_endpoint(&server, "reqcap-leave-client").await;
    let route =
        route_open_expect_bound(&mut client, &mut consumer, &project, consumer_id, 203).await;

    drop(provider);
    // The catalog reads the same verdicts route.open does, so once it reports
    // the consumer not ready the next open must be refused.
    let deadline = Instant::now() + SETUP_TIMEOUT;
    loop {
        let (_, catalog) = catalog_list(&server, Some(consumer_id), 204).await;
        if !catalog[0].ready {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "consumer stayed ready after its required provider disconnected"
        );
        sleep(Duration::from_millis(10)).await;
    }
    let refused =
        route_open_expect_refused(&mut client, &mut consumer, &project, consumer_id, 205).await;
    assert_eq!(refused.code, "module_warming");
    assert_eq!(
        refused.detail,
        Some(serde_json::json!({
            "reason": "required_capability_unprovided",
            "capability": "reqcap-leave/v1",
        }))
    );

    let request = br#"{"jsonrpc":"2.0","id":"after-leave","method":"a"}"#;
    client
        .send(&data_frame(
            FrameType::Request,
            route.client_channel,
            route.client_epoch,
            206,
            request,
        ))
        .await;
    let forwarded = consumer
        .inbox
        .wait_for(
            SETUP_TIMEOUT,
            "request on the already bound route",
            |frame| {
                frame.header.ty == FrameType::Request
                    && frame.header.channel == route.module_channel
                    && frame.header.corr == 206
            },
        )
        .await;
    assert_eq!(forwarded.body, request);
    let response = br#"{"jsonrpc":"2.0","id":"after-leave","result":"ok"}"#;
    consumer
        .send(&data_frame(
            FrameType::Response,
            route.module_channel,
            route.module_epoch,
            206,
            response,
        ))
        .await;
    let delivered = client
        .inbox
        .wait_for(
            SETUP_TIMEOUT,
            "response on the already bound route",
            |frame| {
                frame.header.ty == FrameType::Response
                    && frame.header.channel == route.client_channel
                    && frame.header.corr == 206
            },
        )
        .await;
    assert_eq!(delivered.body, response);
}

// Two modules that require each other's capabilities are both routable once
// both have registered. "Provided" means the claimant has registered; if it
// meant the claimant is ready, each would wait on the other forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn required_capability_mutual_requirement_is_routable_once_both_register() {
    let server = TestServer::start().await;
    let alpha_id = "reqcap-mutual-alpha";
    let beta_id = "reqcap-mutual-beta";
    let mut alpha = connect_endpoint(&server, "reqcap-mutual-alpha").await;
    register_module(
        &server,
        &mut alpha,
        capability_manifest(
            alpha_id,
            &["reqcap-alpha/v1"],
            &[("reqcap-beta/v1", CapabilityNeed::Required)],
        ),
        301,
    )
    .await;
    let (_, catalog) = catalog_list(&server, Some(alpha_id), 302).await;
    assert!(!catalog[0].ready, "alpha alone must wait for beta");

    let mut beta = connect_endpoint(&server, "reqcap-mutual-beta").await;
    register_module(
        &server,
        &mut beta,
        capability_manifest(
            beta_id,
            &["reqcap-beta/v1"],
            &[("reqcap-alpha/v1", CapabilityNeed::Required)],
        ),
        303,
    )
    .await;

    let project = TestProject::new("reqcap-mutual");
    let mut client = connect_endpoint(&server, "reqcap-mutual-client").await;
    route_open_expect_bound(&mut client, &mut alpha, &project, alpha_id, 304).await;
    route_open_expect_bound(&mut client, &mut beta, &project, beta_id, 305).await;
    let (_, catalog) = catalog_list(&server, None, 306).await;
    for id in [alpha_id, beta_id] {
        let entry = catalog.iter().find(|entry| entry.module_id == id).unwrap();
        assert!(entry.ready, "{id} must be routable: {:?}", entry.not_ready);
    }
}

// An optional need with no provider holds nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn optional_capability_without_a_provider_does_not_hold_the_module() {
    let server = TestServer::start().await;
    let module_id = "reqcap-optional-consumer";
    let mut module = connect_endpoint(&server, "reqcap-optional").await;
    register_module(
        &server,
        &mut module,
        capability_manifest(
            module_id,
            &[],
            &[("reqcap-nobody/v1", CapabilityNeed::Optional)],
        ),
        401,
    )
    .await;
    let (_, catalog) = catalog_list(&server, Some(module_id), 402).await;
    assert!(catalog[0].ready);
    assert_eq!(catalog[0].not_ready, None);
    let project = TestProject::new("reqcap-optional");
    let mut client = connect_endpoint(&server, "reqcap-optional-client").await;
    route_open_expect_bound(&mut client, &mut module, &project, module_id, 403).await;
}

/// A routable tool provider carrying the given capability declarations.
fn capability_manifest(
    module_id: &str,
    provides: &[&str],
    requires: &[(&str, CapabilityNeed)],
) -> ModuleManifest {
    let mut manifest = tool_provider_manifest(module_id, &["a"], Concurrency::ModuleManaged);
    manifest.capabilities = Some(CapabilityDeclarations {
        provides: provides.iter().map(|id| (*id).to_string()).collect(),
        requires: requires
            .iter()
            .map(|(capability, need)| CapabilityRequirement {
                capability: (*capability).to_string(),
                need: *need,
            })
            .collect(),
        must_never_reach: Vec::new(),
    });
    manifest
}

fn route_open_frame(project: &TestProject, module_id: &str, corr: u64) -> Frame {
    control_request_frame(
        corr,
        ClientControlRequest::RouteOpen {
            target: RouteTarget::ToolProvider {
                module_id: module_id.to_string(),
            },
            identity: BindIdentity::new(
                project.path().to_path_buf(),
                "opencode".to_string(),
                format!("reqcap-{corr}"),
            ),
            consumer_identity: None,
            consumer_capabilities: None,
            admission_facts: None,
        },
    )
}

/// Send a route.open that must be refused. Watches the target module as well
/// as the client, so an open that wrongly reaches the module fails here by
/// name instead of as a timeout on a bind nobody acknowledges.
async fn route_open_expect_refused(
    client: &mut Endpoint,
    module: &mut Endpoint,
    project: &TestProject,
    module_id: &str,
    corr: u64,
) -> ErrorBody {
    client
        .send(&route_open_frame(project, module_id, corr))
        .await;
    tokio::select! {
        frame = client.inbox.wait_for(SETUP_TIMEOUT, "route.open terminal", |frame| {
            frame.header.channel == 0
                && frame.header.corr == corr
                && matches!(frame.header.ty, FrameType::Response | FrameType::Error)
        }) => {
            assert_eq!(
                frame.header.ty,
                FrameType::Error,
                "route.open to {module_id} must be refused: {}",
                String::from_utf8_lossy(&frame.body)
            );
            serde_json::from_slice(&frame.body).unwrap()
        }
        bind = module.inbox.wait_for(SETUP_TIMEOUT, "route.bind", |frame| {
            frame.header.ty == FrameType::Request && frame.header.channel == 0
        }) => panic!(
            "route.open reached {module_id} although a capability it requires has no provider: {bind:?}"
        ),
    }
}

/// Send a route.open that must bind, acknowledging the bind as the module.
async fn route_open_expect_bound(
    client: &mut Endpoint,
    module: &mut Endpoint,
    project: &TestProject,
    module_id: &str,
    corr: u64,
) -> RoutePair {
    client
        .send(&route_open_frame(project, module_id, corr))
        .await;
    let bind_frame = tokio::select! {
        bind = module.inbox.wait_for(SETUP_TIMEOUT, "route.bind request", |frame| {
            frame.header.ty == FrameType::Request && frame.header.channel == 0
        }) => bind,
        refused = client.inbox.wait_for(SETUP_TIMEOUT, "route.open error", |frame| {
            frame.header.channel == 0
                && frame.header.corr == corr
                && frame.header.ty == FrameType::Error
        }) => panic!(
            "route.open to {module_id} was refused: {}",
            String::from_utf8_lossy(&refused.body)
        ),
    };
    let ModuleControlRequest::RouteBind {
        route_channel,
        epoch: module_epoch,
        ..
    } = serde_json::from_slice(&bind_frame.body).unwrap()
    else {
        panic!("unexpected module control request: {bind_frame:?}");
    };
    module.send(&route_bind_ack(&bind_frame)).await;
    let ack_frame = client
        .inbox
        .wait_for(SETUP_TIMEOUT, "route.open ack", |frame| {
            frame.header.ty == FrameType::Response
                && frame.header.channel == 0
                && frame.header.corr == corr
        })
        .await;
    match serde_json::from_slice(&ack_frame.body).unwrap() {
        ClientControlResponse::RouteOpen {
            route_channel: client_channel,
            route_epoch: client_epoch,
        } => RoutePair {
            client_channel,
            client_epoch,
            module_channel: route_channel,
            module_epoch,
        },
        other => panic!("unexpected route.open response: {other:?}"),
    }
}

async fn register_module(
    server: &TestServer,
    module: &mut Endpoint,
    manifest: ModuleManifest,
    corr: u64,
) -> ModuleHelloAckBody {
    let module_id = manifest.module_id.clone();
    module.send(&hello_frame(manifest, corr)).await;
    let ack_frame = module
        .inbox
        .wait_for(SETUP_TIMEOUT, "HELLO_ACK", |frame| {
            frame.header.ty == FrameType::HelloAck
                && frame.header.channel == 0
                && frame.header.corr == corr
        })
        .await;
    let ack = serde_json::from_slice(&ack_frame.body).unwrap();
    assert!(server.registry.get_module(&module_id).unwrap().is_some());
    ack
}

async fn catalog_list(
    server: &TestServer,
    module_id: Option<&str>,
    corr: u64,
) -> (u64, Vec<CatalogEntry>) {
    let mut client = connect_endpoint(server, "catalog-client").await;
    client
        .send(&control_request_frame(
            corr,
            ClientControlRequest::CatalogList {
                module_id: module_id.map(ToOwned::to_owned),
            },
        ))
        .await;
    let frame = client
        .inbox
        .wait_for(SETUP_TIMEOUT, "catalog.list response", |frame| {
            frame.header.ty == FrameType::Response
                && frame.header.channel == 0
                && frame.header.corr == corr
        })
        .await;
    match serde_json::from_slice(&frame.body).unwrap() {
        ClientControlResponse::CatalogList {
            generation,
            modules,
            ..
        } => (generation, modules),
        other => panic!("unexpected catalog.list response: {other:?}"),
    }
}

async fn open_route(
    client: &mut Endpoint,
    module: &mut Endpoint,
    project: &TestProject,
    module_id: &str,
    corr: u64,
) -> RoutePair {
    client
        .send(&control_request_frame(
            corr,
            ClientControlRequest::RouteOpen {
                target: RouteTarget::ToolProvider {
                    module_id: module_id.to_string(),
                },
                identity: BindIdentity::new(
                    project.path().to_path_buf(),
                    "opencode".to_string(),
                    "catalog-update-session".to_string(),
                ),
                consumer_identity: None,
                consumer_capabilities: None,

                admission_facts: None,
            },
        ))
        .await;

    let bind_frame = module
        .inbox
        .wait_for(SETUP_TIMEOUT, "route.bind request", |frame| {
            frame.header.ty == FrameType::Request && frame.header.channel == 0
        })
        .await;
    let bind: ModuleControlRequest = serde_json::from_slice(&bind_frame.body).unwrap();
    let ModuleControlRequest::RouteBind {
        route_channel,
        epoch: module_epoch,
        ..
    } = bind
    else {
        panic!("unexpected module control request: {bind:?}");
    };
    module.send(&route_bind_ack(&bind_frame)).await;

    let ack_frame = client
        .inbox
        .wait_for(SETUP_TIMEOUT, "route.open ack", |frame| {
            frame.header.ty == FrameType::Response
                && frame.header.channel == 0
                && frame.header.corr == corr
        })
        .await;
    match serde_json::from_slice(&ack_frame.body).unwrap() {
        ClientControlResponse::RouteOpen {
            route_channel: client_channel,
            route_epoch: client_epoch,
        } => RoutePair {
            client_channel,
            client_epoch,
            module_channel: route_channel,
            module_epoch,
        },
        other => panic!("unexpected route.open response: {other:?}"),
    }
}

async fn read_control_error(endpoint: &mut Endpoint, corr: u64) -> ErrorBody {
    let frame = endpoint
        .inbox
        .wait_for(SETUP_TIMEOUT, "control error", |frame| {
            frame.header.ty == FrameType::Error
                && frame.header.channel == 0
                && frame.header.corr == corr
        })
        .await;
    serde_json::from_slice(&frame.body).unwrap()
}

fn assert_tool_names(entry: &CatalogEntry, expected: &[&str]) {
    let ProviderRole::ToolProvider { tools, .. } = &entry.roles[0] else {
        panic!("expected tool provider role: {:?}", entry.roles);
    };
    let names = tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(names, expected);
}

fn hello_frame(manifest: ModuleManifest, corr: u64) -> Frame {
    let body = serde_json::to_vec(&ModuleHelloBody {
        manifest,
        protocol_ver: PROTOCOL_VERSION,
        control_ops: None,
        launch_nonce: None,
    })
    .unwrap();
    Frame::build(FrameType::Hello, control_flags(), 0, 0, corr, body).unwrap()
}

fn catalog_update_frame(corr: u64, provides: Vec<ProviderRole>) -> Frame {
    let body = serde_json::to_vec(&ModuleControlRequestFromModule::CatalogUpdate {
        provides,
        capabilities: None,
        ready: None,
    })
    .unwrap();
    Frame::build(FrameType::Request, control_flags(), 0, 0, corr, body).unwrap()
}

fn route_bind_ack(request: &Frame) -> Frame {
    let body = serde_json::to_vec(&ModuleControlResponse::RouteBindAck {}).unwrap();
    Frame::build_with_version(
        request.header.ver,
        FrameType::Response,
        control_flags(),
        0,
        0,
        request.header.corr,
        body,
    )
    .unwrap()
}

fn control_request_frame(corr: u64, request: ClientControlRequest) -> Frame {
    let body = serde_json::to_vec(&request).unwrap();
    Frame::build(FrameType::Request, control_flags(), 0, 0, corr, body).unwrap()
}

fn data_frame(ty: FrameType, channel: u16, epoch: u32, corr: u64, body: &[u8]) -> Frame {
    Frame::build(
        ty,
        Flags::new(false, Priority::Interactive, false),
        channel,
        epoch,
        corr,
        body.to_vec(),
    )
    .unwrap()
}

fn control_flags() -> Flags {
    Flags::new(false, Priority::Passive, false)
}

fn tool_provider_manifest(
    module_id: &str,
    tools: &[&str],
    concurrency: Concurrency,
) -> ModuleManifest {
    let mut manifest = supervision_only_manifest(module_id);
    manifest.provides = vec![tool_provider_role(tools, concurrency)];
    manifest
}

fn supervision_only_manifest(module_id: &str) -> ModuleManifest {
    ModuleManifest::builder(module_id, "0.0.0-catalog-update-test").build()
}

fn tool_provider_role(tools: &[&str], concurrency: Concurrency) -> ProviderRole {
    ProviderRole::ToolProvider {
        tools: tools
            .iter()
            .map(|name| Tool {
                name: (*name).to_string(),
                description: None,
                execution_mode: ExecutionMode::Pure,
                schema: serde_json::json!({"type": "object"}),
            })
            .collect(),
        identity_scope: vec![IdentityScope::Project, IdentityScope::Session],
        concurrency,
        emits_push: true,
        sub_supervises: true,
    }
}

async fn spawn_ready_stub(
    server: &TestServer,
    supervisor: &Supervisor,
    module_id: &str,
    label: &str,
    ready_update_path: Option<&Path>,
) -> (SupervisedModule, PathBuf) {
    let events_path = server.stub_events_path(label);
    let mut env = vec![
        ("FAKE_AFT_MODULE_ID".to_string(), module_id.to_string()),
        ("FAKE_AFT_READY_FALSE".to_string(), "1".to_string()),
        (
            "FAKE_AFT_EVENTS_PATH".to_string(),
            events_path.to_string_lossy().into_owned(),
        ),
    ];
    if let Some(path) = ready_update_path {
        env.push((
            "FAKE_AFT_READY_UPDATE_PATH".to_string(),
            path.to_string_lossy().into_owned(),
        ));
    }
    let module = supervisor
        .spawn(ModuleSpec {
            module_id: module_id.to_string(),
            program: PathBuf::from(env!("CARGO_BIN_EXE_fake-aft-stub")),
            args: Vec::new(),
            env,
            reserved: false,
            reserved_prefixes: Vec::new(),
            protocol: subc_control::ModuleProtocol::Subc,
            overlap: Default::default(),
        })
        .unwrap();
    wait_for_registration_ready(server, module_id, false).await;
    (module, events_path)
}

async fn route_open_terminal(
    client: &mut Endpoint,
    project: &TestProject,
    module_id: &str,
    corr: u64,
) -> Frame {
    client
        .send(&control_request_frame(
            corr,
            ClientControlRequest::RouteOpen {
                target: RouteTarget::ToolProvider {
                    module_id: module_id.to_string(),
                },
                identity: BindIdentity::new(
                    project.path().to_path_buf(),
                    "opencode".to_string(),
                    format!("readiness-{corr}"),
                ),
                consumer_identity: None,
                consumer_capabilities: None,
                admission_facts: None,
            },
        ))
        .await;
    client
        .inbox
        .wait_for(SETUP_TIMEOUT, "route.open terminal", |frame| {
            frame.header.channel == 0
                && frame.header.corr == corr
                && matches!(frame.header.ty, FrameType::Response | FrameType::Error)
        })
        .await
}

async fn server_describe_counters(server: &TestServer, corr: u64) -> serde_json::Value {
    let mut client = connect_endpoint(server, "server-describe-client").await;
    client
        .send(&control_request_frame(
            corr,
            ClientControlRequest::ServerDescribe {},
        ))
        .await;
    let frame = client
        .inbox
        .wait_for(SETUP_TIMEOUT, "server.describe response", |frame| {
            frame.header.ty == FrameType::Response
                && frame.header.channel == 0
                && frame.header.corr == corr
        })
        .await;
    match serde_json::from_slice::<ClientControlResponse>(&frame.body).unwrap() {
        ClientControlResponse::ServerDescribe { counters, .. } => {
            counters.expect("server.describe counters")
        }
        other => panic!("unexpected server.describe response: {other:?}"),
    }
}

fn refusal_event<'a>(events: &'a [CapturedEvent], module_id: &str) -> &'a CapturedEvent {
    let rendered_module_id = format!("{module_id:?}");
    events
        .iter()
        .find(|event| {
            event.target == "control"
                && event.fields.get("message") == Some(&"route.open refused".to_string())
                && event.fields.get("module_id") == Some(&rendered_module_id)
        })
        .unwrap_or_else(|| panic!("no route.open refusal log for {module_id:?}: {events:?}"))
}

async fn wait_for_registration_ready(server: &TestServer, module_id: &str, ready: bool) {
    let deadline = Instant::now() + SETUP_TIMEOUT;
    loop {
        if server
            .registry
            .get_module(module_id)
            .unwrap()
            .is_some_and(|registration| registration.ready == ready)
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "module {module_id} did not reach ready={ready} within {SETUP_TIMEOUT:?}"
        );
        sleep(Duration::from_millis(10)).await;
    }
}

async fn wait_for_path(path: &Path) {
    let deadline = Instant::now() + SETUP_TIMEOUT;
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "{} did not appear within {SETUP_TIMEOUT:?}",
            path.display()
        );
        sleep(Duration::from_millis(10)).await;
    }
}

async fn wait_for_stub_event(path: &Path, matches: impl Fn(&serde_json::Value) -> bool) {
    let deadline = Instant::now() + SETUP_TIMEOUT;
    loop {
        if stub_events(path).iter().any(&matches) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "stub event did not appear within {SETUP_TIMEOUT:?}; events: {:?}",
            stub_events(path)
        );
        sleep(Duration::from_millis(10)).await;
    }
}

fn stub_events(path: &Path) -> Vec<serde_json::Value> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

struct Endpoint {
    writer: OwnedWriteHalf,
    inbox: FrameInbox,
}

impl Endpoint {
    async fn send(&mut self, frame: &Frame) {
        write_frame(&mut self.writer, frame).await.unwrap();
        self.writer.flush().await.unwrap();
    }
}

async fn connect_endpoint(server: &TestServer, name: &'static str) -> Endpoint {
    let stream = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    endpoint_from_stream(stream, name)
}

fn endpoint_from_stream(stream: TcpStream, name: &'static str) -> Endpoint {
    let (reader, writer) = stream.into_split();
    Endpoint {
        writer,
        inbox: FrameInbox::new(name, reader),
    }
}

enum ReaderEvent {
    Frame(Frame),
    Closed,
    Error(String),
}

struct FrameInbox {
    name: &'static str,
    rx: mpsc::UnboundedReceiver<ReaderEvent>,
    buffered: VecDeque<Frame>,
    reader: JoinHandle<()>,
}

impl FrameInbox {
    fn new(name: &'static str, mut reader: OwnedReadHalf) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let reader_task = tokio::spawn(async move {
            loop {
                match read_frame(&mut reader).await {
                    Ok(Some(frame)) => {
                        if tx.send(ReaderEvent::Frame(frame)).is_err() {
                            break;
                        }
                    }
                    Ok(None) => {
                        let _ = tx.send(ReaderEvent::Closed);
                        break;
                    }
                    Err(err) => {
                        let _ = tx.send(ReaderEvent::Error(err.to_string()));
                        break;
                    }
                }
            }
        });
        Self {
            name,
            rx,
            buffered: VecDeque::new(),
            reader: reader_task,
        }
    }

    async fn wait_for<F>(&mut self, wait: Duration, description: &str, mut matches: F) -> Frame
    where
        F: FnMut(&Frame) -> bool,
    {
        let deadline = Instant::now() + wait;
        loop {
            if let Some(pos) = self.buffered.iter().position(&mut matches) {
                return self.buffered.remove(pos).unwrap();
            }

            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "timed out waiting for {description} on {}; buffered frames: {:?}",
                self.name,
                self.buffered
            );

            match timeout(remaining, self.rx.recv()).await {
                Ok(Some(ReaderEvent::Frame(frame))) => {
                    if matches(&frame) {
                        return frame;
                    }
                    self.buffered.push_back(frame);
                }
                Ok(Some(ReaderEvent::Closed)) => {
                    panic!(
                        "{} connection closed while waiting for {description}; buffered frames: {:?}",
                        self.name, self.buffered
                    );
                }
                Ok(Some(ReaderEvent::Error(err))) => {
                    panic!(
                        "{} reader failed while waiting for {description}: {err}; buffered frames: {:?}",
                        self.name, self.buffered
                    );
                }
                Ok(None) => {
                    panic!(
                        "{} reader task ended while waiting for {description}; buffered frames: {:?}",
                        self.name, self.buffered
                    );
                }
                Err(_) => {
                    panic!(
                        "timed out waiting for {description} on {}; buffered frames: {:?}",
                        self.name, self.buffered
                    );
                }
            }
        }
    }

    fn assert_no_buffered_match<F>(&self, description: &str, mut matches: F)
    where
        F: FnMut(&Frame) -> bool,
    {
        if let Some(frame) = self.buffered.iter().find(|frame| matches(frame)) {
            panic!(
                "unexpected buffered {description} on {}: {frame:?}; buffered frames: {:?}",
                self.name, self.buffered
            );
        }
    }
}

impl Drop for FrameInbox {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

struct TestProject {
    temp: TestTempDir,
}

impl TestProject {
    fn new(name: &str) -> Self {
        Self {
            temp: TestTempDir::new(name),
        }
    }

    fn path(&self) -> &Path {
        self.temp.path()
    }
}
