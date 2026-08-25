use std::{
    collections::BTreeSet,
    fs,
    ops::Deref,
    path::PathBuf,
    process,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use subc_control::{ops, ClientControlRequest, ClientControlResponse, PollKind};
use subc_core::{
    read_frame, write_frame, Frame, ModuleSpec, RestartPolicy, SupervisedModule, Supervisor,
    SupervisorHandle, SupervisorProcessLiveness,
};
use subc_protocol::{
    BindIdentity, ErrorBody, Flags, FrameType, Priority, RouteTarget, PROTOCOL_VERSION,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt},
    time::{sleep, timeout, Instant},
};

mod common;
use common::{
    connect_authed_client, start_test_daemon_with_process_liveness_and_supervisor, TestDaemon,
};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
const READ_TIMEOUT: Duration = Duration::from_secs(2);

struct TestServer {
    daemon: TestDaemon,
    process_liveness: Arc<SupervisorProcessLiveness>,
    supervisor_handle: SupervisorHandle,
}

impl TestServer {
    async fn start() -> Self {
        let process_liveness = Arc::new(SupervisorProcessLiveness::new());
        let supervisor_handle = SupervisorHandle::new();
        let daemon = start_test_daemon_with_process_liveness_and_supervisor(
            "closure-server",
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
}

impl Deref for TestServer {
    type Target = TestDaemon;

    fn deref(&self) -> &Self::Target {
        &self.daemon
    }
}

struct OpaqueRoundTripCase {
    archetype: &'static str,
    module_id: &'static str,
    target: RouteTarget,
    env_pairs: Vec<(&'static str, String)>,
    payload: Vec<u8>,
    corr_base: u64,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreseeable_modules_close_over_existing_control_primitives() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let mut modules = Vec::new();
    let mut used_channel0_ops = BTreeSet::new();
    let mut observed_frame_types = Vec::new();

    assert_server_describe_uses_only_thin_core_ops(&server, &mut used_channel0_ops).await;
    assert_supervisor_routes_reads_the_empty_forwarding_table(&server, &mut used_channel0_ops)
        .await;
    assert_supervisor_provenance_reads_empty_supervisor(&server, &mut used_channel0_ops).await;
    let embedding_payload = embedding_payload();

    modules.push(
        assert_opaque_round_trip(
            &server,
            &supervisor,
            OpaqueRoundTripCase {
                archetype: "MC management surface",
                module_id: "closure-mc",
                target: RouteTarget::ManagementSurface {
                    module_id: "closure-mc".to_string(),
                },
                env_pairs: vec![("FAKE_AFT_ROLE", "management_surface".to_string())],
                payload: br#"{"op":"memory.list","args":{"scope":"project"}}"#.to_vec(),
                corr_base: 100,
            },
            &mut used_channel0_ops,
            &mut observed_frame_types,
        )
        .await,
    );

    modules.push(
        assert_opaque_round_trip(
            &server,
            &supervisor,
            OpaqueRoundTripCase {
                archetype: "embedding tool provider",
                module_id: "closure-embedding",
                target: RouteTarget::ToolProvider {
                    module_id: "closure-embedding".to_string(),
                },
                env_pairs: vec![("FAKE_AFT_ROLE", "tool_provider".to_string())],
                payload: embedding_payload,
                corr_base: 200,
            },
            &mut used_channel0_ops,
            &mut observed_frame_types,
        )
        .await,
    );

    modules.push(
        assert_opaque_round_trip(
            &server,
            &supervisor,
            OpaqueRoundTripCase {
                archetype: "LLM runner internal service",
                module_id: "closure-llm",
                target: RouteTarget::InternalService {
                    module_id: "closure-llm".to_string(),
                    service_id: "llm".to_string(),
                },
                env_pairs: vec![
                    ("FAKE_AFT_ROLE", "internal_service".to_string()),
                    ("FAKE_AFT_SERVICE_ID", "llm".to_string()),
                ],
                payload: br#"{"op":"llm.complete","selection":{"objective":"summarize_foreground_context","budget":"small"},"prompt":"opaque to subc"}"#.to_vec(),
                corr_base: 300,
            },
            &mut used_channel0_ops,
            &mut observed_frame_types,
        )
        .await,
    );

    modules.push(
        assert_bus_pubsub_rides_data_plane(
            &server,
            &supervisor,
            &mut used_channel0_ops,
            &mut observed_frame_types,
        )
        .await,
    );

    modules.push(
        assert_opaque_round_trip(
            &server,
            &supervisor,
            OpaqueRoundTripCase {
                archetype: "federation peer internal service",
                module_id: "closure-peer",
                target: RouteTarget::InternalService {
                    module_id: "closure-peer".to_string(),
                    service_id: "peer".to_string(),
                },
                env_pairs: vec![
                    ("FAKE_AFT_ROLE", "internal_service".to_string()),
                    ("FAKE_AFT_SERVICE_ID", "peer".to_string()),
                ],
                payload: br#"{"op":"peer.forward","peer":"edge-a","payload":{"trace":"opaque-federation-bytes"}}"#.to_vec(),
                corr_base: 500,
            },
            &mut used_channel0_ops,
            &mut observed_frame_types,
        )
        .await,
    );

    modules.push(
        spawn_stub(
            &server,
            &supervisor,
            "closure-pipeline-stage",
            vec![("FAKE_AFT_ROLE", "pipeline_stage".to_string())],
        )
        .await,
    );

    assert_catalog_lists_every_archetype_with_only_thin_core_ops(&server, &mut used_channel0_ops)
        .await;
    assert_unknown_domain_op_is_not_smuggled_into_channel0(&server, &mut observed_frame_types)
        .await;

    // Closure proven: 5 routed archetypes + 1 unrouted pipeline registration,
    // 0 new FrameType, 0 new subc-understood channel-0 op. Every domain body
    // above is opaque bytes and must be echoed byte-identically by the stub.
    let routed_archetypes = 5;
    let new_frame_types = observed_frame_types
        .iter()
        .filter(|ty| !is_existing_frame_type(**ty))
        .count();
    let allowed_ops = thin_core_ops();
    let new_subc_ops = used_channel0_ops.difference(&allowed_ops).count();
    assert_eq!(routed_archetypes, 5);
    assert_eq!(
        new_frame_types, 0,
        "closure proven: 5 archetypes, 0 new FrameType"
    );
    assert_eq!(
        new_subc_ops, 0,
        "closure proven: 5 archetypes, 0 new subc op; used ops: {used_channel0_ops:?}"
    );
    assert_eq!(
        FrameType::from_u8(12),
        None,
        "no thirteenth frame type is needed"
    );

    for module in modules {
        module.stop().await.unwrap();
    }
}

async fn assert_opaque_round_trip(
    server: &TestServer,
    supervisor: &Supervisor,
    case: OpaqueRoundTripCase,
    used_channel0_ops: &mut BTreeSet<&'static str>,
    observed_frame_types: &mut Vec<FrameType>,
) -> SupervisedModule {
    let module = spawn_stub(server, supervisor, case.module_id, case.env_pairs).await;
    let project = TestProject::new(case.archetype);
    let mut client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    let ack = open_route(&mut client, &project, case.corr_base, case.target).await;
    used_channel0_ops.insert(ops::ROUTE_OPEN);
    observed_frame_types.push(FrameType::Request);
    observed_frame_types.push(FrameType::Response);

    let poll = route_poll_frame(case.corr_base + 1, ack.route_channel, ack.route_epoch);
    write_frame(&mut client, &poll).await.unwrap();
    client.flush().await.unwrap();
    let poll_reply = read_frame_timeout(&mut client).await;
    assert_route_poll_status_none(&poll_reply, case.corr_base + 1);
    used_channel0_ops.insert(ops::ROUTE_POLL);
    observed_frame_types.push(FrameType::Request);
    observed_frame_types.push(poll_reply.header.ty);

    let request = data_request(
        ack.route_channel,
        ack.route_epoch,
        case.corr_base + 2,
        &case.payload,
    );
    observed_frame_types.push(request.header.ty);
    write_frame(&mut client, &request).await.unwrap();
    client.flush().await.unwrap();

    let response = read_frame_timeout(&mut client).await;
    observed_frame_types.push(response.header.ty);
    assert_eq!(
        response.header.ty,
        FrameType::Response,
        "{} should return RESPONSE",
        case.archetype
    );
    assert_eq!(response.header.channel, ack.route_channel);
    assert_eq!(response.header.corr, case.corr_base + 2);
    assert_eq!(
        response.body, case.payload,
        "{} payload was not byte-identical",
        case.archetype
    );
    module
}

async fn assert_bus_pubsub_rides_data_plane(
    server: &TestServer,
    supervisor: &Supervisor,
    used_channel0_ops: &mut BTreeSet<&'static str>,
    observed_frame_types: &mut Vec<FrameType>,
) -> SupervisedModule {
    let module_id = "closure-bus";
    let module = spawn_stub(
        server,
        supervisor,
        module_id,
        vec![
            ("FAKE_AFT_ROLE", "tool_provider".to_string()),
            ("FAKE_AFT_FANOUT_ON_REQUEST", "1".to_string()),
        ],
    )
    .await;

    let project = TestProject::new("bus");
    let target = RouteTarget::ToolProvider {
        module_id: module_id.to_string(),
    };
    let mut publisher = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    let mut subscriber = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    let publisher_ack = open_route(&mut publisher, &project, 400, target.clone()).await;
    let subscriber_ack = open_route(&mut subscriber, &project, 401, target).await;
    used_channel0_ops.insert(ops::ROUTE_OPEN);
    observed_frame_types.extend([FrameType::Request, FrameType::Response]);

    let payload = br#"{"op":"bus.publish","topic":"memories.changed","body":{"id":"m-1"}}"#;
    let request = data_request(
        publisher_ack.route_channel,
        publisher_ack.route_epoch,
        402,
        payload,
    );
    observed_frame_types.push(request.header.ty);
    write_frame(&mut publisher, &request).await.unwrap();
    publisher.flush().await.unwrap();

    let (publisher_push, response) =
        read_push_and_response(&mut publisher, publisher_ack.route_channel, 402, payload).await;
    observed_frame_types.push(publisher_push.header.ty);
    observed_frame_types.push(response.header.ty);

    let subscriber_push = read_push(&mut subscriber, subscriber_ack.route_channel).await;
    observed_frame_types.push(subscriber_push.header.ty);
    module
}

async fn assert_server_describe_uses_only_thin_core_ops(
    server: &TestServer,
    used_channel0_ops: &mut BTreeSet<&'static str>,
) {
    let mut client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    let response = control_round_trip(
        &mut client,
        10,
        ClientControlRequest::ServerDescribe {},
        ops::SERVER_DESCRIBE,
        used_channel0_ops,
    )
    .await;
    match response {
        ClientControlResponse::ServerDescribe {
            protocol_ver,
            subc_ops,
            ..
        } => {
            assert_eq!(protocol_ver, PROTOCOL_VERSION);
            assert_eq!(
                subc_ops.into_iter().collect::<BTreeSet<_>>(),
                thin_core_ops_as_strings()
            );
        }
        other => panic!("unexpected server.describe response: {other:?}"),
    }
}

async fn assert_supervisor_routes_reads_the_empty_forwarding_table(
    server: &TestServer,
    used_channel0_ops: &mut BTreeSet<&'static str>,
) {
    let mut client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    let response = control_round_trip(
        &mut client,
        11,
        ClientControlRequest::SupervisorRoutes { module_id: None },
        ops::SUPERVISOR_ROUTES,
        used_channel0_ops,
    )
    .await;
    match response {
        ClientControlResponse::SupervisorRoutes { modules } => assert!(modules.is_empty()),
        other => panic!("unexpected supervisor.routes response: {other:?}"),
    }
}

async fn assert_supervisor_provenance_reads_empty_supervisor(
    server: &TestServer,
    used_channel0_ops: &mut BTreeSet<&'static str>,
) {
    let mut client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    let response = control_round_trip(
        &mut client,
        12,
        ClientControlRequest::SupervisorProvenance { module_id: None },
        ops::SUPERVISOR_PROVENANCE,
        used_channel0_ops,
    )
    .await;
    match response {
        ClientControlResponse::SupervisorProvenance { daemon, modules } => {
            assert!(modules.is_empty());
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            assert!(matches!(
                daemon.daemon_observed.running_image,
                subc_control::RunningImageAgreement::Match { .. }
            ));
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            assert!(matches!(
                daemon.daemon_observed.running_image,
                subc_control::RunningImageAgreement::Unavailable {
                    reason: subc_control::RunningImageUnavailableReason::UnsupportedPlatform
                }
            ));
        }
        other => panic!("unexpected supervisor.provenance response: {other:?}"),
    }
}

async fn assert_catalog_lists_every_archetype_with_only_thin_core_ops(
    server: &TestServer,
    used_channel0_ops: &mut BTreeSet<&'static str>,
) {
    let mut client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    let response = control_round_trip(
        &mut client,
        20,
        ClientControlRequest::CatalogList { module_id: None },
        ops::CATALOG_LIST,
        used_channel0_ops,
    )
    .await;
    match response {
        ClientControlResponse::CatalogList {
            modules, subc_ops, ..
        } => {
            assert_eq!(
                subc_ops.into_iter().collect::<BTreeSet<_>>(),
                thin_core_ops_as_strings()
            );
            let module_ids = modules
                .into_iter()
                .map(|entry| entry.module_id)
                .collect::<BTreeSet<_>>();
            for module_id in [
                "closure-mc",
                "closure-embedding",
                "closure-llm",
                "closure-bus",
                "closure-peer",
                "closure-pipeline-stage",
            ] {
                assert!(
                    module_ids.contains(module_id),
                    "catalog missing {module_id}"
                );
            }
        }
        other => panic!("unexpected catalog.list response: {other:?}"),
    }
}

async fn assert_unknown_domain_op_is_not_smuggled_into_channel0(
    server: &TestServer,
    observed_frame_types: &mut Vec<FrameType>,
) {
    let mut client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    let frame = Frame::build(
        FrameType::Request,
        Flags::new(false, Priority::Passive, false),
        0,
        0,
        30,
        br#"{"op":"memory.list","args":{"this":"belongs on a route channel"}}"#.to_vec(),
    )
    .unwrap();
    observed_frame_types.push(frame.header.ty);
    write_frame(&mut client, &frame).await.unwrap();
    client.flush().await.unwrap();

    let error = read_frame_timeout(&mut client).await;
    observed_frame_types.push(error.header.ty);
    assert_eq!(error.header.ty, FrameType::Error);
    assert_eq!(error.header.channel, 0);
    assert_eq!(error.header.corr, 30);
    let body: ErrorBody = serde_json::from_slice(&error.body).unwrap();
    assert_eq!(body.code, "unknown_control_op");
}

async fn control_round_trip<S>(
    client: &mut S,
    corr: u64,
    request: ClientControlRequest,
    op: &'static str,
    used_channel0_ops: &mut BTreeSet<&'static str>,
) -> ClientControlResponse
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    used_channel0_ops.insert(op);
    write_frame(client, &control_request_frame(corr, request))
        .await
        .unwrap();
    client.flush().await.unwrap();
    let frame = read_frame_timeout(client).await;
    assert_eq!(frame.header.ty, FrameType::Response);
    assert_eq!(frame.header.channel, 0);
    assert_eq!(frame.header.corr, corr);
    serde_json::from_slice(&frame.body).unwrap()
}

#[derive(Debug, Clone, Copy)]
struct RouteOpenAck {
    route_channel: u16,
    route_epoch: u32,
}

async fn open_route<S>(
    client: &mut S,
    project: &TestProject,
    corr: u64,
    target: RouteTarget,
) -> RouteOpenAck
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let request = ClientControlRequest::RouteOpen {
        target,
        identity: BindIdentity {
            project_root: project.path.clone(),
            harness: "opencode".to_string(),
            session: format!("closure-{}", corr),
        },
        consumer_identity: None,
        consumer_capabilities: None,
        admission_facts: None,
    };
    write_frame(client, &control_request_frame(corr, request))
        .await
        .unwrap();
    client.flush().await.unwrap();

    let ack_frame = read_frame_timeout(client).await;
    assert_eq!(ack_frame.header.ty, FrameType::Response);
    assert_eq!(ack_frame.header.channel, 0);
    assert_eq!(ack_frame.header.corr, corr);
    match serde_json::from_slice(&ack_frame.body).unwrap() {
        ClientControlResponse::RouteOpen {
            route_channel,
            route_epoch,
        } => RouteOpenAck {
            route_channel,
            route_epoch,
        },
        other => panic!("unexpected route.open response: {other:?}"),
    }
}

fn control_request_frame(corr: u64, request: ClientControlRequest) -> Frame {
    let body = serde_json::to_vec(&request).unwrap();
    Frame::build(
        FrameType::Request,
        Flags::new(false, Priority::Passive, false),
        0,
        0,
        corr,
        body,
    )
    .unwrap()
}

fn route_poll_frame(corr: u64, route_channel: u16, route_epoch: u32) -> Frame {
    let body = serde_json::to_vec(&ClientControlRequest::RoutePoll {
        route_channel,
        route_epoch,
        kind: PollKind::Status,
    })
    .unwrap();
    Frame::build(
        FrameType::Request,
        Flags::new(false, Priority::Passive, false),
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

fn assert_route_poll_status_none(frame: &Frame, corr: u64) {
    assert_eq!(frame.header.ty, FrameType::Response);
    assert_eq!(frame.header.channel, 0);
    assert_eq!(frame.header.corr, corr);
    match serde_json::from_slice(&frame.body).unwrap() {
        ClientControlResponse::RoutePoll {
            route_channel: _route_channel,
            route_epoch: _route_epoch,
            status: None,
            live: None,
        } => {}
        other => panic!("unexpected route.poll response: {other:?}"),
    }
}

async fn read_push_and_response<S>(
    stream: &mut S,
    route_channel: u16,
    response_corr: u64,
    response_body: &[u8],
) -> (Frame, Frame)
where
    S: AsyncRead + Unpin,
{
    let mut push = None;
    let mut response = None;

    for _ in 0..2 {
        let frame = read_frame_timeout(stream).await;
        assert_eq!(frame.header.channel, route_channel);
        match frame.header.ty {
            FrameType::Push if push.is_none() => {
                assert_push(&frame, route_channel);
                push = Some(frame);
            }
            FrameType::Response if response.is_none() => {
                assert_eq!(frame.header.corr, response_corr);
                assert_eq!(frame.body, response_body);
                response = Some(frame);
            }
            ty => panic!("unexpected frame type while waiting for bus PUSH/RESPONSE: {ty:?}"),
        }
    }

    (
        push.expect("missing bus PUSH"),
        response.expect("missing bus response"),
    )
}

async fn read_push<S>(stream: &mut S, route_channel: u16) -> Frame
where
    S: AsyncRead + Unpin,
{
    let frame = read_frame_timeout(stream).await;
    assert_push(&frame, route_channel);
    frame
}

fn assert_push(frame: &Frame, route_channel: u16) {
    assert_eq!(frame.header.ty, FrameType::Push);
    assert_eq!(frame.header.channel, route_channel);
    assert_eq!(frame.header.corr, 0);
    assert_eq!(frame.body, b"push-event");
}

async fn read_frame_timeout<S>(stream: &mut S) -> Frame
where
    S: AsyncRead + Unpin,
{
    timeout(READ_TIMEOUT, async {
        read_frame(stream)
            .await
            .unwrap()
            .expect("connection should stay open")
    })
    .await
    .expect("timed out waiting for frame")
}

fn supervisor(server: &TestServer, max_restarts: u32, backoff: Duration) -> Supervisor {
    Supervisor::new(
        Arc::clone(&server.registry),
        RestartPolicy::new(max_restarts, backoff),
    )
    .with_process_liveness(Arc::clone(&server.process_liveness))
    .with_handle(server.supervisor_handle.clone())
    .with_drain_timeout(Duration::from_millis(25))
    .with_connection_file_path(server.connection_file_path.clone())
}

async fn spawn_stub(
    server: &TestServer,
    supervisor: &Supervisor,
    module_id: &str,
    extra_env: Vec<(&str, String)>,
) -> SupervisedModule {
    let module = supervisor
        .spawn(stub_spec_with_env(module_id, extra_env))
        .unwrap();
    wait_for_registration(server, module_id, Duration::from_secs(1)).await;
    module
}

fn stub_spec_with_env(module_id: &str, extra_env: Vec<(&str, String)>) -> ModuleSpec {
    let mut env = vec![("FAKE_AFT_MODULE_ID".to_string(), module_id.to_string())];
    env.extend(
        extra_env
            .into_iter()
            .map(|(key, value)| (key.to_string(), value)),
    );

    ModuleSpec {
        module_id: module_id.to_string(),
        program: PathBuf::from(env!("CARGO_BIN_EXE_fake-aft-stub")),
        args: Vec::new(),
        env,
        reserved: false,
        reserved_prefixes: Vec::new(),
    }
}

async fn wait_for_registration(server: &TestServer, module_id: &str, wait: Duration) {
    let deadline = Instant::now() + wait;
    loop {
        if server.registry.get_module(module_id).unwrap().is_some() {
            return;
        }
        if Instant::now() >= deadline {
            panic!("module {module_id} did not register within {wait:?}");
        }
        sleep(Duration::from_millis(10)).await;
    }
}

fn thin_core_ops() -> BTreeSet<&'static str> {
    BTreeSet::from([
        ops::SERVER_DESCRIBE,
        ops::CATALOG_LIST,
        ops::ROUTE_OPEN,
        ops::ROUTE_POLL,
        ops::ROUTE_CLOSING,
        ops::ROUTE_CLOSED,
        ops::SUPERVISOR_LIST,
        ops::SUPERVISOR_RESTART,
        ops::SUPERVISOR_RELOAD,
        ops::SUPERVISOR_RESCAN,
        ops::SUPERVISOR_RELEASE_RESERVED,
        ops::SUPERVISOR_SET_ENABLED,
        ops::SUPERVISOR_HEALTH_PROBE,
        ops::SUPERVISOR_HEALTH,
        ops::SUPERVISOR_STDERR_TAIL,
        ops::SUPERVISOR_TERMINALS,
        ops::SUPERVISOR_ROUTES,
        ops::SUPERVISOR_PROVENANCE,
    ])
}

fn thin_core_ops_as_strings() -> BTreeSet<String> {
    thin_core_ops()
        .into_iter()
        .map(str::to_string)
        .collect::<BTreeSet<_>>()
}

fn is_existing_frame_type(ty: FrameType) -> bool {
    matches!(
        ty,
        FrameType::Request
            | FrameType::Response
            | FrameType::Push
            | FrameType::StreamData
            | FrameType::StreamEnd
            | FrameType::Error
            | FrameType::Cancel
            | FrameType::Ping
            | FrameType::Pong
            | FrameType::Hello
            | FrameType::HelloAck
            | FrameType::Goodbye
    )
}

fn embedding_payload() -> Vec<u8> {
    let bulk = "semantic-vector-source:".repeat(256);
    format!(r#"{{"op":"embed","body":"{bulk}","ann_query":{{"top_k":8,"space":"memories"}}}}"#)
        .into_bytes()
}

struct TestProject {
    path: PathBuf,
}

impl TestProject {
    fn new(label: &str) -> Self {
        let path = unique_temp_dir(label);
        fs::create_dir_all(&path).unwrap();
        Self { path }
    }
}

impl Drop for TestProject {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn unique_temp_dir(label: &str) -> PathBuf {
    let nonce = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("sc-closure-{label}-{}-{nonce}", process::id()))
}
