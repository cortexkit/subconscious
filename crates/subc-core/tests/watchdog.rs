use std::{
    io,
    net::Ipv4Addr,
    path::{Path, PathBuf},
    process,
    sync::{Arc, Mutex},
    time::Duration,
};

use subc_control::ClientControlRequest;
use subc_core::{
    bootstrap::{run_with_config, BootstrapConfig, BootstrapError},
    read_frame, serve_listener,
    test_support::TestTempDir,
    write_frame, ControlHandler, DaemonSelfWatchdog, DaemonSelfWatchdogConfig, Frame, Registry,
    Router, ServerAuth,
};
use subc_protocol::{Flags, FrameType, Priority, PROTOCOL_VERSION};
use subc_transport::{
    generate_daemon_id, generate_key, read_for_client as read_connection_file, write_atomic,
    ConnectionInfo, Endpoint, SCHEMA_VERSION,
};
use tokio::{
    io::AsyncWriteExt,
    net::{TcpListener, TcpStream},
    task::JoinHandle,
    time::{sleep, Instant},
};
use tracing_subscriber::fmt::MakeWriter;

mod common;
use common::connect_authed_client;

const LOG_TIMEOUT: Duration = Duration::from_secs(2);
const START_TIMEOUT: Duration = Duration::from_secs(2);

#[tokio::test(flavor = "current_thread")]
async fn watchdog_tick_against_live_daemon_succeeds_silently() {
    let capture = LogCapture::default();
    let _guard = tracing::subscriber::set_default(capture.subscriber());
    let server = common::start_test_daemon("watchdog-live").await;
    let live_info = read_connection_file(&server.connection_file_path).unwrap();
    let watchdog = DaemonSelfWatchdog::new(live_info, &server.connection_file_path).with_config(
        DaemonSelfWatchdogConfig::default()
            .with_interval(Duration::from_millis(25))
            .with_deadline(Duration::from_millis(250)),
    );

    watchdog.run_once().await.unwrap();

    let watchdog_logs = capture
        .entries()
        .into_iter()
        .filter(|line| line.contains("subc_core::watchdog"))
        .collect::<Vec<_>>();
    assert!(
        watchdog_logs.is_empty(),
        "unexpected watchdog logs: {watchdog_logs:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn watchdog_detects_wire_version_divergence() {
    let temp_dir = unique_temp_dir("watchdog-wire-version");
    let connection_file_path = temp_dir.join("subc-conn.json");
    let port = reserve_free_port().await;
    let live_info = test_connection_info(port);
    let mut file_info = live_info.clone();
    file_info.wire_version = None;
    write_atomic(&connection_file_path, &file_info).unwrap();
    let server_task = start_fixed_port_server(port, &live_info).await;
    let watchdog = DaemonSelfWatchdog::new(live_info, &connection_file_path);

    let err = watchdog
        .run_once()
        .await
        .expect_err("missing wire_version must diverge from a daemon-published file");
    assert_eq!(err.stage(), subc_core::WatchdogStage::ConnectionFile);
    assert!(err.to_string().contains("wire_version"), "error: {err}");

    server_task.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn watchdog_detects_daemon_id_divergence() {
    let temp_dir = unique_temp_dir("watchdog-daemon-id");
    let connection_file_path = temp_dir.join("subc-conn.json");
    let port = reserve_free_port().await;
    let live_info = test_connection_info(port);
    let mut file_info = live_info.clone();
    file_info.daemon_id = generate_daemon_id().unwrap();
    assert_ne!(file_info.daemon_id, live_info.daemon_id);
    write_atomic(&connection_file_path, &file_info).unwrap();
    let server_task = start_fixed_port_server(port, &live_info).await;
    let watchdog = DaemonSelfWatchdog::new(live_info, &connection_file_path);

    let err = watchdog
        .run_once()
        .await
        .expect_err("daemon_id must match the live daemon identity");
    assert_eq!(err.stage(), subc_core::WatchdogStage::ConnectionFile);
    assert!(err.to_string().contains("daemon_id"), "error: {err}");

    server_task.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn bootstrap_watchdog_logs_connection_file_divergence() {
    let capture = LogCapture::default();
    let _guard = tracing::subscriber::set_default(capture.subscriber());
    let daemon = RunningDaemon::start(
        "watchdog-bootstrap-divergence",
        DaemonSelfWatchdogConfig::default()
            .with_interval(Duration::from_millis(25))
            .with_deadline(Duration::from_millis(250)),
    )
    .await;

    let mut diverged = read_connection_file(&daemon.connection_file_path).unwrap();
    diverged.endpoints = vec![Endpoint {
        host: Ipv4Addr::LOCALHOST.to_string(),
        port: diverged.endpoints[0].port.saturating_add(7),
    }];
    diverged.key = generate_key().unwrap();
    write_atomic(&daemon.connection_file_path, &diverged).unwrap();

    wait_for_log(&capture, LOG_TIMEOUT, |line| {
        line.contains("daemon self-watchdog tick failed")
            && line.contains("stage=\"connection_file\"")
            && line.contains("connection file divergence: port")
            && line.contains("key")
            && line.contains(&daemon.connection_file_path.display().to_string())
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn watchdog_rejects_a_daemon_that_answers_the_wrong_control_response() {
    let port = reserve_free_port().await;
    let live_info = test_connection_info(port);
    let temp_dir = unique_temp_dir("watchdog-wrong-reply");
    let connection_file_path = temp_dir.join("subc-conn.json");
    write_atomic(&connection_file_path, &live_info).unwrap();

    let server_task = start_wrong_reply_server(port, &live_info).await;
    let watchdog = DaemonSelfWatchdog::new(live_info, &connection_file_path);

    let err = watchdog
        .run_once()
        .await
        .expect_err("a reply that is not server.describe must not count as a healthy tick");

    // Assert the STAGE, not just that something failed: connect and authenticate
    // both succeeded here, and attributing this to either of them would send an
    // operator to the wrong layer.
    assert_eq!(
        err.stage(),
        subc_core::WatchdogStage::Describe,
        "a wrong control response is a describe-stage fault, not a transport one"
    );
    assert!(
        err.to_string().contains("unexpected server.describe reply"),
        "the message must name what arrived so the fault is diagnosable: {err}"
    );

    server_task.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn watchdog_failed_tick_logs_stage_and_recovery_streak() {
    let capture = LogCapture::default();
    let _guard = tracing::subscriber::set_default(capture.subscriber());
    let temp_dir = unique_temp_dir("watchdog-recovery");
    let connection_file_path = temp_dir.join("subc-conn.json");
    let port = reserve_free_port().await;
    let live_info = ConnectionInfo {
        schema: SCHEMA_VERSION,
        wire_version: None,
        endpoints: vec![Endpoint {
            host: Ipv4Addr::LOCALHOST.to_string(),
            port,
        }],
        key: generate_key().unwrap(),
        daemon_id: generate_daemon_id().unwrap(),
        pid: process::id(),
        daemon_ver: "test-subc".to_owned(),
    };
    write_atomic(&connection_file_path, &live_info).unwrap();

    let watchdog = DaemonSelfWatchdog::new(live_info.clone(), &connection_file_path).with_config(
        DaemonSelfWatchdogConfig::default()
            .with_interval(Duration::from_millis(25))
            .with_deadline(Duration::from_millis(150)),
    );
    let watchdog_task = watchdog.spawn();

    // A dead endpoint manifests OS-dependently: unix refuses fast (stage
    // "connect"), Windows keeps retrying SYN past the tick deadline (stage
    // "timeout"). The mechanism under test is the failure streak + recovery,
    // not which stage a dead endpoint surfaces as.
    let dead_endpoint_failure = |line: &str, streak: &str| {
        line.contains("daemon self-watchdog tick failed")
            && (line.contains("stage=\"connect\"") || line.contains("stage=\"timeout\""))
            && line.contains(streak)
    };
    wait_for_log(&capture, LOG_TIMEOUT, |line| {
        dead_endpoint_failure(line, "consecutive_failures=1")
    })
    .await;
    wait_for_log(&capture, LOG_TIMEOUT, |line| {
        dead_endpoint_failure(line, "consecutive_failures=2")
    })
    .await;

    let server_task = start_fixed_port_server(port, &live_info).await;
    wait_for_responsive_daemon(&connection_file_path, START_TIMEOUT).await;

    wait_for_log(&capture, LOG_TIMEOUT, |line| {
        line.contains("daemon self-watchdog recovered") && line.contains("failure_streak=")
    })
    .await;

    watchdog_task.abort();
    server_task.abort();
}

struct RunningDaemon {
    connection_file_path: PathBuf,
    // Held for RAII lifetime only: the guard's `Drop` removes the tree (or
    // preserves it on panic). Never read directly.
    #[allow(dead_code)]
    temp_dir: TestTempDir,
    task: JoinHandle<Result<(), BootstrapError>>,
}

impl RunningDaemon {
    async fn start(name: &str, watchdog_config: DaemonSelfWatchdogConfig) -> Self {
        let temp_dir = unique_temp_dir(name);
        let connection_file_path = temp_dir.join("subc-conn.json");
        let task = tokio::spawn(run_with_config(
            BootstrapConfig::new(&connection_file_path, 0).with_watchdog_config(watchdog_config),
        ));
        wait_for_responsive_daemon(&connection_file_path, START_TIMEOUT).await;

        Self {
            connection_file_path,
            temp_dir,
            task,
        }
    }
}

impl Drop for RunningDaemon {
    fn drop(&mut self) {
        self.task.abort();
        // The temp dir is owned by the `TestTempDir` guard, whose `Drop` removes
        // the tree (or preserves it on panic).
    }
}

#[derive(Clone, Default)]
struct LogCapture {
    entries: Arc<Mutex<Vec<String>>>,
}

impl LogCapture {
    fn subscriber(&self) -> impl tracing::Subscriber + Send + Sync {
        tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_writer(CaptureMakeWriter {
                entries: Arc::clone(&self.entries),
            })
            .finish()
    }

    fn entries(&self) -> Vec<String> {
        self.entries.lock().unwrap().clone()
    }
}

#[derive(Clone)]
struct CaptureMakeWriter {
    entries: Arc<Mutex<Vec<String>>>,
}

impl<'a> MakeWriter<'a> for CaptureMakeWriter {
    type Writer = CaptureWriter;

    fn make_writer(&'a self) -> Self::Writer {
        CaptureWriter {
            entries: Arc::clone(&self.entries),
            buf: Vec::new(),
        }
    }
}

struct CaptureWriter {
    entries: Arc<Mutex<Vec<String>>>,
    buf: Vec<u8>,
}

impl io::Write for CaptureWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for CaptureWriter {
    fn drop(&mut self) {
        if self.buf.is_empty() {
            return;
        }
        let line = String::from_utf8_lossy(&self.buf).trim().to_owned();
        if !line.is_empty() {
            self.entries.lock().unwrap().push(line);
        }
    }
}

async fn wait_for_responsive_daemon(connection_file_path: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(mut stream) = connect_authed_client(connection_file_path).await {
            write_frame(
                &mut stream,
                &control_request_frame(1, ClientControlRequest::ServerDescribe {}),
            )
            .await
            .unwrap();
            stream.flush().await.unwrap();
            let frame = read_frame_timeout(&mut stream).await;
            if frame.header.ty == FrameType::Response {
                return;
            }
        }
        assert!(Instant::now() < deadline, "daemon never became responsive");
        sleep(Duration::from_millis(10)).await;
    }
}

async fn wait_for_log(capture: &LogCapture, timeout: Duration, predicate: impl Fn(&str) -> bool) {
    let deadline = Instant::now() + timeout;
    loop {
        let entries = capture.entries();
        if entries.iter().any(|entry| predicate(entry)) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "log line not observed; logs={entries:?}"
        );
        sleep(Duration::from_millis(10)).await;
    }
}

/// A listener that completes the auth handshake and then answers channel-0 with
/// a well-formed reply to the WRONG request.
///
/// The watchdog's `describe` stage exists for a daemon that is reachable and
/// authenticates but whose control plane is not answering correctly -- the state
/// where every cheaper signal (port open, key valid) says healthy. No fixture
/// could reach that stage, because a fixture built from the real router answers
/// `server.describe` correctly by construction, so the branches that classify a
/// wrong reply were unreachable from the suite that covers this file.
async fn start_wrong_reply_server(port: u16, live_info: &ConnectionInfo) -> JoinHandle<()> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))
        .await
        .unwrap();
    // Drive the handshake directly rather than through ServerAuth: its fields are
    // private, and this fixture needs the auth to succeed while what follows it
    // does not.
    let key = live_info.key.clone();
    let daemon_id = live_info.daemon_id;
    let daemon_ver = live_info.daemon_ver.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let key = key.clone();
            let daemon_ver = daemon_ver.clone();
            tokio::spawn(async move {
                if subc_transport::authenticate_server(
                    &mut stream,
                    key.as_ref(),
                    &daemon_id,
                    daemon_ver.as_ref(),
                    Duration::from_secs(2),
                )
                .await
                .is_err()
                {
                    return;
                }
                // Read whatever channel-0 request arrives, then reply with a
                // DIFFERENT well-formed control response. The frame decodes, the
                // correlation matches, and only the variant is wrong -- so this
                // isolates the variant check from the transport checks around it.
                let Ok(Some(request)) = read_frame(&mut stream).await else {
                    return;
                };
                let body =
                    serde_json::to_vec(&subc_control::ClientControlResponse::SupervisorAck {
                        module_id: "not-a-describe".to_string(),
                        applied: true,
                    })
                    .unwrap();
                let reply = Frame::build(
                    FrameType::Response,
                    Flags::new(false, Priority::Interactive, false),
                    0,
                    0,
                    request.header.corr,
                    body,
                )
                .unwrap();
                let _ = write_frame(&mut stream, &reply).await;
                let _ = stream.flush().await;
            });
        }
    })
}

async fn start_fixed_port_server(port: u16, live_info: &ConnectionInfo) -> JoinHandle<()> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))
        .await
        .unwrap();
    let router =
        Router::with_control_handler(Arc::new(ControlHandler::new(Arc::new(Registry::default()))));
    let auth = ServerAuth::new(
        live_info.key.clone(),
        live_info.daemon_id,
        live_info.daemon_ver.clone(),
    );
    tokio::spawn(async move {
        let _ = serve_listener(listener, Arc::new(router), auth).await;
    })
}

async fn reserve_free_port() -> u16 {
    TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn test_connection_info(port: u16) -> ConnectionInfo {
    ConnectionInfo {
        schema: SCHEMA_VERSION,
        wire_version: Some(PROTOCOL_VERSION),
        endpoints: vec![Endpoint {
            host: Ipv4Addr::LOCALHOST.to_string(),
            port,
        }],
        key: generate_key().unwrap(),
        daemon_id: generate_daemon_id().unwrap(),
        pid: process::id(),
        daemon_ver: "test-subc".to_owned(),
    }
}

fn unique_temp_dir(name: &str) -> TestTempDir {
    TestTempDir::new(name)
}

fn control_request_frame(corr: u64, request: ClientControlRequest) -> Frame {
    Frame::build(
        FrameType::Request,
        Flags::new(false, Priority::Interactive, false),
        0,
        0,
        corr,
        serde_json::to_vec(&request).unwrap(),
    )
    .unwrap()
}

async fn read_frame_timeout(stream: &mut TcpStream) -> Frame {
    tokio::time::timeout(Duration::from_secs(2), read_frame(stream))
        .await
        .unwrap()
        .unwrap()
        .unwrap()
}
