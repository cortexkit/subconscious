use std::{collections::VecDeque, ops::Deref, path::Path, time::Duration};

use subc_control::{ClientControlRequest, ClientControlResponse};
use subc_core::{
    read_frame, test_support::TestTempDir, write_frame, ForwardingTable, Frame, Registry,
};
use subc_protocol::{
    manifest::{
        Bindings, Concurrency, ExecutionMode, IdentityBinding, IdentityScope, ModuleManifest,
        ProviderRole, StorageBinding, StorageKind, StorageScope, Tool, TrustTier,
    },
    session::{ModuleControlRequest, ModuleControlResponse},
    BindIdentity, Flags, FrameType, ModuleHelloAckBody, ModuleHelloBody, Priority, RouteTarget,
    PROTOCOL_VERSION,
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

mod common;
use common::{connect_authed_client, TestDaemon};

/// Timeout for test setup steps that wait for subc system state to be ready
/// before declaring a hang, such as registration and route-bind completion.
const SETUP_TIMEOUT: Duration = Duration::from_secs(10);

struct TestServer {
    daemon: TestDaemon,
}

impl TestServer {
    async fn start() -> Self {
        Self {
            daemon: TestDaemon::start("reverse-request-server").await,
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
async fn reverse_request_forwards_and_route_back_response_rewrites_channel() {
    let server = TestServer::start().await;
    let module_id = "reverse-forward-provider";
    let mut module = connect_endpoint(&server, "module").await;
    register_module(
        &server,
        &mut module,
        module_id,
        Concurrency::ModuleManaged,
        101,
    )
    .await;

    let project = TestProject::new("reverse-forward");
    let mut client = connect_endpoint(&server, "client").await;
    let route = open_route(
        &mut client,
        &mut module,
        &project,
        module_id,
        201,
        "ses-reverse-forward",
    )
    .await;
    wait_for_binding_count(&server.forwarding, 1, SETUP_TIMEOUT).await;

    let reverse_corr = 301;
    let reverse_body = br#"{"jsonrpc":"2.0","id":"ask-1","method":"elicitation/create"}"#;
    module
        .send(&data_frame(
            FrameType::Request,
            route.module_channel,
            route.module_epoch,
            reverse_corr,
            reverse_body,
        ))
        .await;

    let forwarded = client
        .inbox
        .wait_for(SETUP_TIMEOUT, "client-local reverse Request", |frame| {
            frame.header.ty == FrameType::Request
                && frame.header.channel == route.client_channel
                && frame.header.corr == reverse_corr
        })
        .await;
    assert_eq!(forwarded.body, reverse_body);

    let answer_body = br#"{"jsonrpc":"2.0","id":"ask-1","result":{"accepted":true}}"#;
    client
        .send(&data_frame(
            FrameType::Response,
            route.client_channel,
            route.client_epoch,
            reverse_corr,
            answer_body,
        ))
        .await;

    let routed_back = module
        .inbox
        .wait_for(SETUP_TIMEOUT, "module-local reverse Response", |frame| {
            frame.header.ty == FrameType::Response
                && frame.header.channel == route.module_channel
                && frame.header.corr == reverse_corr
        })
        .await;
    assert_eq!(routed_back.body, answer_body);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reverse_request_does_not_consume_or_release_serial_forward_credit() {
    let server = TestServer::start().await;
    let module_id = "reverse-serial-provider";
    let mut module = connect_endpoint(&server, "module").await;
    register_module(&server, &mut module, module_id, Concurrency::Serial, 111).await;

    let project = TestProject::new("reverse-serial");
    let mut client = connect_endpoint(&server, "client").await;
    let route = open_route(
        &mut client,
        &mut module,
        &project,
        module_id,
        211,
        "ses-reverse-serial",
    )
    .await;
    wait_for_binding_count(&server.forwarding, 1, SETUP_TIMEOUT).await;

    let first_corr = 311;
    let second_corr = 312;
    let reverse_corr = 313;
    let first_body = br#"{"jsonrpc":"2.0","id":"forward-1"}"#;
    let second_body = br#"{"jsonrpc":"2.0","id":"forward-2"}"#;

    client
        .send(&data_frame(
            FrameType::Request,
            route.client_channel,
            route.client_epoch,
            first_corr,
            first_body,
        ))
        .await;
    let first_forward = module
        .inbox
        .wait_for(SETUP_TIMEOUT, "first forward Request", |frame| {
            frame.header.ty == FrameType::Request
                && frame.header.channel == route.module_channel
                && frame.header.corr == first_corr
        })
        .await;
    assert_eq!(first_forward.body, first_body);

    let reverse_body = br#"{"jsonrpc":"2.0","id":"serial-ask","method":"elicitation/create"}"#;
    module
        .send(&data_frame(
            FrameType::Request,
            route.module_channel,
            route.module_epoch,
            reverse_corr,
            reverse_body,
        ))
        .await;
    let reverse_request = client
        .inbox
        .wait_for(
            SETUP_TIMEOUT,
            "reverse Request while forward credit is held",
            |frame| {
                frame.header.ty == FrameType::Request
                    && frame.header.channel == route.client_channel
                    && frame.header.corr == reverse_corr
            },
        )
        .await;
    assert_eq!(reverse_request.body, reverse_body);

    let reverse_answer = br#"{"jsonrpc":"2.0","id":"serial-ask","result":"allowed"}"#;
    client
        .send(&data_frame(
            FrameType::Response,
            route.client_channel,
            route.client_epoch,
            reverse_corr,
            reverse_answer,
        ))
        .await;
    let reverse_response = module
        .inbox
        .wait_for(
            SETUP_TIMEOUT,
            "reverse Response while forward credit is held",
            |frame| {
                frame.header.ty == FrameType::Response
                    && frame.header.channel == route.module_channel
                    && frame.header.corr == reverse_corr
            },
        )
        .await;
    assert_eq!(reverse_response.body, reverse_answer);

    client
        .send(&data_frame(
            FrameType::Request,
            route.client_channel,
            route.client_epoch,
            second_corr,
            second_body,
        ))
        .await;
    module
        .inbox
        .assert_no_matching_within(
            Duration::from_millis(100),
            "second forward Request before first terminal",
            |frame| {
                frame.header.ty == FrameType::Request
                    && frame.header.channel == route.module_channel
                    && frame.header.corr == second_corr
            },
        )
        .await;

    let first_response_body = br#"{"jsonrpc":"2.0","id":"forward-1","result":"done"}"#;
    module
        .send(&data_frame(
            FrameType::Response,
            route.module_channel,
            route.module_epoch,
            first_corr,
            first_response_body,
        ))
        .await;
    let first_response = client
        .inbox
        .wait_for(SETUP_TIMEOUT, "first forward terminal Response", |frame| {
            frame.header.ty == FrameType::Response
                && frame.header.channel == route.client_channel
                && frame.header.corr == first_corr
        })
        .await;
    assert_eq!(first_response.body, first_response_body);

    let second_forward = module
        .inbox
        .wait_for(
            SETUP_TIMEOUT,
            "second forward Request after terminal",
            |frame| {
                frame.header.ty == FrameType::Request
                    && frame.header.channel == route.module_channel
                    && frame.header.corr == second_corr
            },
        )
        .await;
    assert_eq!(second_forward.body, second_body);

    let second_response_body = br#"{"jsonrpc":"2.0","id":"forward-2","result":"done"}"#;
    module
        .send(&data_frame(
            FrameType::Response,
            route.module_channel,
            route.module_epoch,
            second_corr,
            second_response_body,
        ))
        .await;
    let second_response = client
        .inbox
        .wait_for(SETUP_TIMEOUT, "second forward terminal Response", |frame| {
            frame.header.ty == FrameType::Response
                && frame.header.channel == route.client_channel
                && frame.header.corr == second_corr
        })
        .await;
    assert_eq!(second_response.body, second_response_body);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn late_reverse_response_after_route_goodbye_is_dropped_and_sibling_route_survives() {
    let server = TestServer::start().await;
    let module_id = "reverse-teardown-provider";
    let mut module = connect_endpoint(&server, "module").await;
    register_module(
        &server,
        &mut module,
        module_id,
        Concurrency::ModuleManaged,
        121,
    )
    .await;

    let project = TestProject::new("reverse-teardown");
    let mut client = connect_endpoint(&server, "client").await;
    let first = open_route(
        &mut client,
        &mut module,
        &project,
        module_id,
        221,
        "ses-reverse-teardown-a",
    )
    .await;
    let second = open_route(
        &mut client,
        &mut module,
        &project,
        module_id,
        222,
        "ses-reverse-teardown-b",
    )
    .await;
    wait_for_binding_count(&server.forwarding, 2, SETUP_TIMEOUT).await;

    let reverse_corr = 321;
    let reverse_body = br#"{"jsonrpc":"2.0","id":"teardown-ask","method":"elicitation/create"}"#;
    module
        .send(&data_frame(
            FrameType::Request,
            first.module_channel,
            first.module_epoch,
            reverse_corr,
            reverse_body,
        ))
        .await;
    let reverse_request = client
        .inbox
        .wait_for(SETUP_TIMEOUT, "reverse Request before teardown", |frame| {
            frame.header.ty == FrameType::Request
                && frame.header.channel == first.client_channel
                && frame.header.corr == reverse_corr
        })
        .await;
    assert_eq!(reverse_request.body, reverse_body);

    client
        .send(&pure_header_frame(
            FrameType::Goodbye,
            first.client_channel,
            first.client_epoch,
            322,
        ))
        .await;
    module
        .inbox
        .wait_for(SETUP_TIMEOUT, "module route GOODBYE", |frame| {
            frame.header.ty == FrameType::Goodbye && frame.header.channel == first.module_channel
        })
        .await;
    wait_for_binding_count(&server.forwarding, 1, SETUP_TIMEOUT).await;

    let late_answer = br#"{"jsonrpc":"2.0","id":"teardown-ask","result":"late"}"#;
    client
        .send(&data_frame(
            FrameType::Response,
            first.client_channel,
            first.client_epoch,
            reverse_corr,
            late_answer,
        ))
        .await;
    let late_module_corr = 323;
    let late_module_body = br#"{"jsonrpc":"2.0","id":"teardown-module-late"}"#;
    module
        .send(&data_frame(
            FrameType::Response,
            first.module_channel,
            first.module_epoch,
            late_module_corr,
            late_module_body,
        ))
        .await;
    module
        .send(&pure_header_frame(FrameType::Ping, 0, 0, 324))
        .await;
    module
        .inbox
        .wait_for(
            SETUP_TIMEOUT,
            "module ping barrier after late module frame",
            |frame| {
                frame.header.ty == FrameType::Pong
                    && frame.header.channel == 0
                    && frame.header.corr == 324
            },
        )
        .await;
    module
        .inbox
        .assert_no_buffered_match("module-visible Error for late module frame", |frame| {
            frame.header.corr == late_module_corr
        });
    client
        .send(&pure_header_frame(FrameType::Ping, 0, 0, 325))
        .await;
    client
        .inbox
        .wait_for(
            SETUP_TIMEOUT,
            "client ping barrier after late module frame",
            |frame| {
                frame.header.ty == FrameType::Pong
                    && frame.header.channel == 0
                    && frame.header.corr == 325
            },
        )
        .await;
    client
        .inbox
        .assert_no_buffered_match("client-visible misroute of late module frame", |frame| {
            frame.header.corr == late_module_corr
        });

    let live_corr = 326;
    let live_body = br#"{"jsonrpc":"2.0","id":"teardown-live"}"#;
    client
        .send(&data_frame(
            FrameType::Request,
            second.client_channel,
            second.client_epoch,
            live_corr,
            live_body,
        ))
        .await;
    let live_request = module
        .inbox
        .wait_for(SETUP_TIMEOUT, "sibling route forward Request", |frame| {
            frame.header.ty == FrameType::Request
                && frame.header.channel == second.module_channel
                && frame.header.corr == live_corr
        })
        .await;
    assert_eq!(live_request.body, live_body);
    module.inbox.assert_no_buffered_match(
        "module-visible late reverse Response on any channel",
        |frame| frame.header.corr == reverse_corr,
    );

    let live_response_body = br#"{"jsonrpc":"2.0","id":"teardown-live","result":"ok"}"#;
    module
        .send(&data_frame(
            FrameType::Response,
            second.module_channel,
            second.module_epoch,
            live_corr,
            live_response_body,
        ))
        .await;
    let live_response = client
        .inbox
        .wait_for(SETUP_TIMEOUT, "sibling route forward Response", |frame| {
            frame.header.ty == FrameType::Response
                && frame.header.channel == second.client_channel
                && frame.header.corr == live_corr
        })
        .await;
    assert_eq!(live_response.body, live_response_body);
    client.inbox.assert_no_buffered_match(
        "client-visible misroute of late module frame after sibling round trip",
        |frame| frame.header.corr == late_module_corr,
    );
    module.inbox.assert_no_buffered_match(
        "module-visible late client Response after sibling round trip",
        |frame| frame.header.corr == reverse_corr,
    );
    module.inbox.assert_no_buffered_match(
        "module-visible Error for late module frame after sibling round trip",
        |frame| frame.header.corr == late_module_corr,
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn equal_corr_forward_and_reverse_requests_do_not_cross_contaminate() {
    let server = TestServer::start().await;
    let module_id = "reverse-corr-provider";
    let mut module = connect_endpoint(&server, "module").await;
    register_module(
        &server,
        &mut module,
        module_id,
        Concurrency::ModuleManaged,
        131,
    )
    .await;

    let project = TestProject::new("reverse-corr");
    let mut client = connect_endpoint(&server, "client").await;
    let route = open_route(
        &mut client,
        &mut module,
        &project,
        module_id,
        231,
        "ses-reverse-corr",
    )
    .await;
    wait_for_binding_count(&server.forwarding, 1, SETUP_TIMEOUT).await;

    let shared_corr = 331;
    let forward_body = br#"{"jsonrpc":"2.0","id":"same-corr-forward"}"#;
    client
        .send(&data_frame(
            FrameType::Request,
            route.client_channel,
            route.client_epoch,
            shared_corr,
            forward_body,
        ))
        .await;
    let forward_request = module
        .inbox
        .wait_for(
            SETUP_TIMEOUT,
            "client-originated forward Request",
            |frame| {
                frame.header.ty == FrameType::Request
                    && frame.header.channel == route.module_channel
                    && frame.header.corr == shared_corr
            },
        )
        .await;
    assert_eq!(forward_request.body, forward_body);

    let reverse_body =
        br#"{"jsonrpc":"2.0","id":"same-corr-reverse","method":"elicitation/create"}"#;
    module
        .send(&data_frame(
            FrameType::Request,
            route.module_channel,
            route.module_epoch,
            shared_corr,
            reverse_body,
        ))
        .await;
    let reverse_request = client
        .inbox
        .wait_for(
            SETUP_TIMEOUT,
            "module-originated reverse Request with equal corr",
            |frame| {
                frame.header.ty == FrameType::Request
                    && frame.header.channel == route.client_channel
                    && frame.header.corr == shared_corr
            },
        )
        .await;
    assert_eq!(reverse_request.body, reverse_body);

    let reverse_answer = br#"{"jsonrpc":"2.0","id":"same-corr-reverse","result":"answer"}"#;
    client
        .send(&data_frame(
            FrameType::Response,
            route.client_channel,
            route.client_epoch,
            shared_corr,
            reverse_answer,
        ))
        .await;
    let delivered_reverse_answer = module
        .inbox
        .wait_for(
            SETUP_TIMEOUT,
            "client Response to module-originated reverse Request",
            |frame| {
                frame.header.ty == FrameType::Response
                    && frame.header.channel == route.module_channel
                    && frame.header.corr == shared_corr
            },
        )
        .await;
    assert_eq!(delivered_reverse_answer.body, reverse_answer);

    let forward_answer = br#"{"jsonrpc":"2.0","id":"same-corr-forward","result":"forward"}"#;
    module
        .send(&data_frame(
            FrameType::Response,
            route.module_channel,
            route.module_epoch,
            shared_corr,
            forward_answer,
        ))
        .await;
    let delivered_forward_answer = client
        .inbox
        .wait_for(
            SETUP_TIMEOUT,
            "module Response to client-originated forward Request",
            |frame| {
                frame.header.ty == FrameType::Response
                    && frame.header.channel == route.client_channel
                    && frame.header.corr == shared_corr
            },
        )
        .await;
    assert_eq!(delivered_forward_answer.body, forward_answer);
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

    async fn assert_no_matching_within<F>(
        &mut self,
        wait: Duration,
        description: &str,
        mut matches: F,
    ) where
        F: FnMut(&Frame) -> bool,
    {
        if let Some(frame) = self.buffered.iter().find(|frame| matches(frame)) {
            panic!(
                "unexpected buffered {description} on {}: {frame:?}; buffered frames: {:?}",
                self.name, self.buffered
            );
        }

        let deadline = Instant::now() + wait;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return;
            }

            match timeout(remaining, self.rx.recv()).await {
                Ok(Some(ReaderEvent::Frame(frame))) => {
                    if matches(&frame) {
                        panic!("unexpected {description} on {}: {frame:?}", self.name);
                    }
                    self.buffered.push_back(frame);
                }
                Ok(Some(ReaderEvent::Closed)) => {
                    panic!(
                        "{} connection closed while checking for no {description}",
                        self.name
                    );
                }
                Ok(Some(ReaderEvent::Error(err))) => {
                    panic!(
                        "{} reader failed while checking for no {description}: {err}",
                        self.name
                    );
                }
                Ok(None) => {
                    panic!(
                        "{} reader task ended while checking for no {description}",
                        self.name
                    );
                }
                Err(_) => return,
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

async fn register_module(
    server: &TestServer,
    module: &mut Endpoint,
    module_id: &str,
    concurrency: Concurrency,
    corr: u64,
) -> ModuleHelloAckBody {
    module
        .send(&hello_frame(
            tool_provider_manifest(module_id, concurrency),
            corr,
        ))
        .await;
    let ack_frame = module
        .inbox
        .wait_for(SETUP_TIMEOUT, "HELLO_ACK", |frame| {
            frame.header.ty == FrameType::HelloAck
                && frame.header.channel == 0
                && frame.header.corr == corr
        })
        .await;
    let ack: ModuleHelloAckBody = serde_json::from_slice(&ack_frame.body).unwrap();
    assert_eq!(ack.negotiated_ver, PROTOCOL_VERSION);
    wait_for_registration(&server.registry, module_id, SETUP_TIMEOUT).await;
    ack
}

async fn open_route(
    client: &mut Endpoint,
    module: &mut Endpoint,
    project: &TestProject,
    module_id: &str,
    corr: u64,
    session: &str,
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
                    session: session.to_string(),
                },
                consumer_identity: None,
                consumer_capabilities: None,

                admission_facts: None,
            },
        ))
        .await;

    let bind_frame = module
        .inbox
        .wait_for(SETUP_TIMEOUT, "module route.bind Request", |frame| {
            frame.header.ty == FrameType::Request
                && frame.header.channel == 0
                && route_bind_targets_module(frame, module_id)
        })
        .await;
    let bind: ModuleControlRequest = serde_json::from_slice(&bind_frame.body).unwrap();
    let ModuleControlRequest::RouteBind {
        route_channel: module_channel,
        epoch: module_epoch,
        target,
        identity,
        ..
    } = bind
    else {
        panic!("expected route.bind request, got {bind:?}");
    };
    assert!(route_target_is_module(&target, module_id));
    assert_eq!(identity.session, session);

    module.send(&route_bind_ack(&bind_frame)).await;

    let ack_frame = client
        .inbox
        .wait_for(SETUP_TIMEOUT, "client route.open Response", |frame| {
            frame.header.ty == FrameType::Response
                && frame.header.channel == 0
                && frame.header.corr == corr
        })
        .await;
    match serde_json::from_slice(&ack_frame.body).unwrap() {
        ClientControlResponse::RouteOpen {
            route_channel,
            route_epoch: client_epoch,
        } => RoutePair {
            client_channel: route_channel,
            client_epoch,
            module_channel,
            module_epoch,
        },
        other => panic!("unexpected route.open response: {other:?}"),
    }
}

fn route_bind_targets_module(frame: &Frame, module_id: &str) -> bool {
    match serde_json::from_slice::<ModuleControlRequest>(&frame.body) {
        Ok(ModuleControlRequest::RouteBind { target, .. }) => {
            route_target_is_module(&target, module_id)
        }
        Ok(ModuleControlRequest::HealthCheck {}) => false,
        Err(_) => false,
    }
}

fn route_target_is_module(target: &RouteTarget, module_id: &str) -> bool {
    match target {
        RouteTarget::ToolProvider { module_id: actual }
        | RouteTarget::ManagementSurface { module_id: actual }
        | RouteTarget::InternalService {
            module_id: actual, ..
        } => actual == module_id,
    }
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

fn control_request_frame(corr: u64, request: ClientControlRequest) -> Frame {
    let body = serde_json::to_vec(&request).unwrap();
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

fn pure_header_frame(ty: FrameType, channel: u16, epoch: u32, corr: u64) -> Frame {
    let frame = Frame::build(
        ty,
        Flags::new(false, Priority::Interactive, false),
        channel,
        epoch,
        corr,
        Vec::new(),
    )
    .unwrap();
    assert_eq!(frame.header.len, 0);
    frame
}

fn control_flags() -> Flags {
    Flags::new(false, Priority::Passive, false)
}

fn tool_provider_manifest(module_id: &str, concurrency: Concurrency) -> ModuleManifest {
    ModuleManifest {
        module_id: module_id.to_string(),
        module_version: "0.0.0-reverse-test".to_string(),
        protocol_ver: PROTOCOL_VERSION,
        trust_tier: TrustTier::FirstParty,
        provides: vec![ProviderRole::ToolProvider {
            tools: vec![Tool {
                name: "read".to_string(),
                description: None,
                execution_mode: ExecutionMode::Pure,
                schema: serde_json::json!({"type": "object"}),
            }],
            identity_scope: vec![IdentityScope::Project, IdentityScope::Session],
            concurrency,
            emits_push: true,
            sub_supervises: true,
        }],
        consumes: Vec::new(),
        bindings: Bindings {
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
        capabilities: None,
        self_signals: None,
        provenance: None,
    }
}

async fn wait_for_registration(registry: &Registry, module_id: &str, wait: Duration) {
    let deadline = Instant::now() + wait;
    loop {
        if registry.get_module(module_id).unwrap().is_some() {
            return;
        }
        if Instant::now() >= deadline {
            panic!("module {module_id} did not register within {wait:?}");
        }
        sleep(Duration::from_millis(10)).await;
    }
}

async fn wait_for_binding_count(forwarding: &ForwardingTable, expected: usize, wait: Duration) {
    let deadline = Instant::now() + wait;
    loop {
        let count = forwarding.active_binding_count().unwrap();
        if count == expected {
            return;
        }
        if Instant::now() >= deadline {
            panic!("forwarding binding count {count} did not become {expected} within {wait:?}");
        }
        sleep(Duration::from_millis(10)).await;
    }
}

struct TestProject {
    temp: TestTempDir,
}

impl TestProject {
    fn new(label: &str) -> Self {
        Self {
            temp: TestTempDir::new(label),
        }
    }

    fn path(&self) -> &Path {
        self.temp.path()
    }
}
