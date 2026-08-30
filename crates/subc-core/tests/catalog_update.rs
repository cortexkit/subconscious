use std::{collections::VecDeque, ops::Deref, path::Path, time::Duration};

use subc_control::{CatalogEntry, ClientControlRequest, ClientControlResponse};
use subc_core::{read_frame, test_support::TestTempDir, write_frame, Frame};
use subc_protocol::{
    manifest::{
        Bindings, Concurrency, ExecutionMode, IdentityBinding, IdentityScope, ManifestProvenance,
        ModuleManifest, ProviderRole, SelfSignalDeclaration, SelfSignalEffect, SelfSignalKind,
        SignalAnchor, SignalCadence, StorageBinding, StorageKind, StorageScope, Tool, TrustTier,
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
    time::{timeout, Instant},
};

mod common;
use common::{connect_authed_client, TestDaemon};

const SETUP_TIMEOUT: Duration = Duration::from_secs(10);

struct TestServer {
    daemon: TestDaemon,
}

impl TestServer {
    async fn start() -> Self {
        Self {
            daemon: TestDaemon::start("catalog-update-server").await,
        }
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn catalog_update_refreshes_catalog_without_disrupting_bound_routes() {
    let server = TestServer::start().await;
    let module_id = "catalog-update-provider";
    let mut module = connect_endpoint(&server, "module").await;
    let provenance = ManifestProvenance {
        build_git_sha: Some("0123456789abcdef0123456789abcdef01234567".to_string()),
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
                identity: BindIdentity {
                    project_root: project.path().to_path_buf(),
                    harness: "opencode".to_string(),
                    session: "catalog-update-session".to_string(),
                },
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
    ModuleManifest::builder(
        module_id,
        "0.0.0-catalog-update-test",
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
    .build()
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
