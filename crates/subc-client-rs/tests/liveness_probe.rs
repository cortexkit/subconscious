#![forbid(unsafe_code)]

use std::{
    fs, io,
    path::PathBuf,
    process,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use subc_client_rs::{CallError, CallOptions, ConsumerOptions, RetryBackoff, SubcConsumer};
use subc_control::{ClientControlRequest, ClientControlResponse};
use subc_protocol::{BindIdentity, Flags, Frame, FrameType, Priority, RouteTarget};
use subc_transport::{
    authenticate_server, generate_daemon_id, generate_key, read_frame, write_atomic, write_frame,
    ConnectionInfo, Endpoint, SCHEMA_VERSION,
};
use tokio::{
    io::AsyncWriteExt,
    net::{TcpListener, TcpStream},
    task::JoinHandle,
    time::sleep,
};

const AUTH_DEADLINE: Duration = Duration::from_secs(2);
const PROBE_WINDOW: Duration = Duration::from_millis(75);
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy)]
enum DataMode {
    HalfOpen,
    SilentHold,
}

struct FakeDaemon {
    connection_file: PathBuf,
    connections: Arc<AtomicU64>,
    server_task: JoinHandle<()>,
    temp_dir: PathBuf,
}

impl FakeDaemon {
    fn connection_count(&self) -> u64 {
        self.connections.load(Ordering::Relaxed)
    }
}

impl Drop for FakeDaemon {
    fn drop(&mut self) {
        self.server_task.abort();
        let _ = fs::remove_dir_all(&self.temp_dir);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn liveness_probe_convicts_half_open_socket_and_next_call_recovers() {
    let daemon = start_fake_daemon(DataMode::HalfOpen).await;
    let consumer = SubcConsumer::connect(&daemon.connection_file, consumer_options())
        .await
        .expect("connect consumer");
    let target = target();
    let identity = identity();

    let first = consumer
        .call(
            target.clone(),
            identity.clone(),
            b"first".to_vec(),
            call_options(Duration::from_millis(100)),
        )
        .await
        .expect_err("the deaf connection must time out after accepting the data frame");
    assert!(matches!(first, CallError::OutcomeUnknown(_)));

    // The first connection remains open at the TCP layer but answers no Ping. Let the
    // probe convict it before the next call proves the regular reconnect path recovers.
    sleep(PROBE_WINDOW + PROBE_WINDOW + Duration::from_millis(100)).await;
    let reply = consumer
        .call(
            target,
            identity,
            b"second".to_vec(),
            call_options(Duration::from_secs(1)),
        )
        .await
        .expect("the next call must use the reconnect's serving connection");
    assert_eq!(reply, b"second");
    assert_eq!(daemon.connection_count(), 2);

    consumer.close().await;
}

/// An in-flight CHANNEL-0 request suspends conviction. The daemon's connection
/// loop is FIFO and some channel-0 handlers legally park it inline for seconds
/// (route.open awaits the module bind ack for up to route_bind_relay_timeout),
/// during which a probe Ping sits unread — silence explained by the client's
/// own control op, not by a dead socket. The fake parks catalog.list forever
/// (it answers only RouteOpen) and goes deaf on data, so even the Ping is
/// unanswered — the worst case — and the gate must still withhold conviction:
/// the second deadline error rides the ORIGINAL connection (count stays 1)
/// instead of a reconnect's fresh one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn liveness_probe_withholds_conviction_while_a_control_request_is_pending() {
    let daemon = start_fake_daemon(DataMode::HalfOpen).await;
    let consumer = SubcConsumer::connect(&daemon.connection_file, consumer_options())
        .await
        .expect("connect consumer");
    let target = target();
    let identity = identity();

    // Parked forever server-side (the fake never answers CatalogList); this is
    // the channel-0 pending the gate consults. Polled just long enough to send,
    // then held pinned-but-unpolled: the pending registration lives in the
    // consumer's shared state, not in this future's polling.
    let held = consumer.catalog_list();
    tokio::pin!(held);
    tokio::select! {
        _ = &mut held => panic!("catalog_list must stay parked at the fake daemon"),
        () = sleep(Duration::from_millis(50)) => {}
    }

    let first = consumer
        .call(
            target.clone(),
            identity.clone(),
            b"first".to_vec(),
            call_options(Duration::from_millis(100)),
        )
        .await
        .expect_err("the deaf connection must time out after accepting the data frame");
    assert!(matches!(first, CallError::OutcomeUnknown(_)));

    // Give a (wrong) conviction ample time to fire, then prove it did not: the
    // second call must ride the SAME connection into the same deadline class.
    sleep(PROBE_WINDOW + PROBE_WINDOW + Duration::from_millis(100)).await;
    let second = consumer
        .call(
            target,
            identity,
            b"second".to_vec(),
            call_options(Duration::from_millis(100)),
        )
        .await
        .expect_err("the corpse is deliberately kept while the control op explains the silence");
    assert!(matches!(second, CallError::OutcomeUnknown(_)));
    assert_eq!(
        daemon.connection_count(),
        1,
        "conviction must be withheld while a channel-0 request is in flight"
    );

    // `held` (pinned, never completed) is released by scope exit; close() then
    // settles its registration with the connection teardown.
    consumer.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn liveness_probe_keeps_silent_but_healthy_socket_for_next_call() {
    let daemon = start_fake_daemon(DataMode::SilentHold).await;
    let consumer = SubcConsumer::connect(&daemon.connection_file, consumer_options())
        .await
        .expect("connect consumer");
    let target = target();
    let identity = identity();

    let first = consumer
        .call(
            target.clone(),
            identity.clone(),
            b"first".to_vec(),
            call_options(Duration::from_millis(100)),
        )
        .await
        .expect_err("the held request must settle as outcome_unknown");
    assert!(matches!(first, CallError::OutcomeUnknown(_)));

    // This daemon answers the probe's Ping, proving the connection is healthy even
    // though the request has no reply. Waiting through the window gives a faulty
    // unconditional conviction time to open a replacement connection.
    sleep(PROBE_WINDOW + PROBE_WINDOW + Duration::from_millis(100)).await;
    assert_eq!(daemon.connection_count(), 1);

    // This second request is load-bearing: it proves the original socket still carries
    // calls, rather than merely observing that a teardown has not happened yet.
    let second = consumer
        .call(
            target,
            identity,
            b"second".to_vec(),
            call_options(Duration::from_millis(100)),
        )
        .await
        .expect_err("the second held request must remain on the same connection");
    assert!(matches!(second, CallError::OutcomeUnknown(_)));
    assert_eq!(daemon.connection_count(), 1);

    consumer.close().await;
}

fn consumer_options() -> ConsumerOptions {
    ConsumerOptions {
        handshake_timeout: AUTH_DEADLINE,
        call_timeout: Duration::from_secs(1),
        reconnect_backoff: RetryBackoff {
            base: Duration::from_millis(5),
            cap: Duration::from_millis(10),
            max_attempts: 4,
        },
        restored_debounce: Duration::ZERO,
        liveness_probe_window: PROBE_WINDOW,
    }
}

fn call_options(timeout: Duration) -> CallOptions {
    CallOptions {
        timeout,
        route_retry_deadline: timeout,
        ..CallOptions::default()
    }
}

fn target() -> RouteTarget {
    RouteTarget::ToolProvider {
        module_id: "liveness-fake".to_string(),
    }
}

fn identity() -> BindIdentity {
    BindIdentity {
        project_root: PathBuf::from("/tmp/subc-client-rs-liveness-probe"),
        harness: "subc-client-rs-test".to_string(),
        session: "liveness-probe".to_string(),
    }
}

async fn start_fake_daemon(mode: DataMode) -> FakeDaemon {
    let temp_dir = unique_temp_dir("subc-client-rs-liveness-probe");
    fs::create_dir_all(&temp_dir).expect("create fake daemon directory");
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fake daemon listener");
    let connection = ConnectionInfo {
        schema: SCHEMA_VERSION,
        wire_version: None,
        endpoints: vec![Endpoint {
            host: "127.0.0.1".to_string(),
            port: listener.local_addr().expect("read listener address").port(),
        }],
        key: generate_key().expect("generate fake daemon key"),
        daemon_id: generate_daemon_id().expect("generate fake daemon id"),
        pid: process::id(),
        daemon_ver: "subc-client-rs-liveness-fake".to_string(),
    };
    let connection_file = temp_dir.join("subc-conn.json");
    write_atomic(&connection_file, &connection).expect("write fake connection file");

    let connections = Arc::new(AtomicU64::new(0));
    let accepted_data = Arc::new(AtomicU64::new(0));
    let server_connections = Arc::clone(&connections);
    let server_task = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            server_connections.fetch_add(1, Ordering::Relaxed);
            let connection = connection.clone();
            let accepted_data = Arc::clone(&accepted_data);
            tokio::spawn(async move {
                let _ = serve_connection(stream, connection, mode, accepted_data).await;
            });
        }
    });

    FakeDaemon {
        connection_file,
        connections,
        server_task,
        temp_dir,
    }
}

async fn serve_connection(
    mut stream: TcpStream,
    connection: ConnectionInfo,
    mode: DataMode,
    accepted_data: Arc<AtomicU64>,
) -> io::Result<()> {
    authenticate_server(
        &mut stream,
        &connection.key,
        &connection.daemon_id,
        &connection.daemon_ver,
        AUTH_DEADLINE,
    )
    .await
    .map_err(io::Error::other)?;

    let mut deaf = false;
    loop {
        let Some(frame) = read_frame(&mut stream).await.map_err(io::Error::other)? else {
            return Ok(());
        };
        if deaf {
            continue;
        }
        if frame.header.ty == FrameType::Ping && frame.header.channel == 0 {
            send_frame(
                &mut stream,
                Frame::build_with_version(
                    frame.header.ver,
                    FrameType::Pong,
                    Flags::new(false, Priority::Interactive, false),
                    0,
                    0,
                    frame.header.corr,
                    Vec::new(),
                )
                .map_err(io::Error::other)?,
            )
            .await?;
            continue;
        }
        if frame.header.ty != FrameType::Request {
            continue;
        }
        if frame.header.channel == 0 {
            let request: ClientControlRequest =
                serde_json::from_slice(&frame.body).map_err(io::Error::other)?;
            if matches!(request, ClientControlRequest::RouteOpen { .. }) {
                let body = serde_json::to_vec(&ClientControlResponse::RouteOpen {
                    route_channel: 41,
                    route_epoch: 1,
                })
                .map_err(io::Error::other)?;
                send_frame(
                    &mut stream,
                    Frame::build_with_version(
                        frame.header.ver,
                        FrameType::Response,
                        Flags::new(false, Priority::Interactive, false),
                        0,
                        0,
                        frame.header.corr,
                        body,
                    )
                    .map_err(io::Error::other)?,
                )
                .await?;
            }
            continue;
        }

        match mode {
            DataMode::SilentHold => {}
            DataMode::HalfOpen if accepted_data.fetch_add(1, Ordering::Relaxed) == 0 => {
                deaf = true;
            }
            DataMode::HalfOpen => {
                send_frame(
                    &mut stream,
                    Frame::build_with_version(
                        frame.header.ver,
                        FrameType::Response,
                        Flags::new(false, Priority::Interactive, false),
                        frame.header.channel,
                        frame.header.epoch,
                        frame.header.corr,
                        frame.body,
                    )
                    .map_err(io::Error::other)?,
                )
                .await?;
            }
        }
    }
}

async fn send_frame(stream: &mut TcpStream, frame: Frame) -> io::Result<()> {
    write_frame(stream, &frame)
        .await
        .map_err(io::Error::other)?;
    stream.flush().await
}

fn unique_temp_dir(name: &str) -> PathBuf {
    let nonce = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("{name}-{}-{nonce}", process::id()))
}
