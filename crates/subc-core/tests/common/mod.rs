#![allow(dead_code)]

pub mod scripted_daemon;

use std::{
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    process::{self, Command},
    sync::{Arc, Once},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use subc_test_support::TestTempDir;

use subc_daemon::{
    serve_listener, ConnectedClients, ControlHandler, ForwardingTable, ModuleProcessLiveness,
    Registry, Router, ServerAuth, SupervisorHandle,
};
use subc_protocol::PROTOCOL_VERSION;
use subc_transport::{
    authenticate_client, generate_daemon_id, generate_key, write_atomic, ConnectionInfo, Endpoint,
    SCHEMA_VERSION,
};
use tokio::{
    net::{TcpListener, TcpStream},
    task::JoinHandle,
};

/// Copy an executable via a child: the test process must not hold its writable
/// descriptor while another test thread forks to run it.
pub fn copy_executable(src: &Path, dst: &Path) {
    #[cfg(unix)]
    assert!(Command::new("cp")
        .arg(src)
        .arg(dst)
        .status()
        .expect("copy executable fixture")
        .success());
    #[cfg(not(unix))]
    std::fs::copy(src, dst).expect("copy executable fixture");
}

const TEST_DAEMON_VER: &str = "test-subc";
const TEST_AUTH_DEADLINE: Duration = Duration::from_secs(2);

/// Returns the dedicated CLI target after proving it contains the fixture-key
/// support required by integration tests. A build-shape check makes a Cargo
/// feature-resolution change fail before any test fixture can mask it.
pub fn ck_under_test_command() -> Command {
    assert_ck_under_test_build_shape();
    Command::new(env!("CARGO_BIN_EXE_ck-under-test"))
}

fn assert_ck_under_test_build_shape() {
    static CHECKED: Once = Once::new();

    CHECKED.call_once(|| {
        let path = Path::new(env!("CARGO_BIN_EXE_ck-under-test"));
        let output = Command::new(path)
            .arg("--ck-build-shape")
            .output()
            .unwrap_or_else(|error| panic!("could not inspect ck-under-test {}: {error}", path.display()));
        let build_shape = String::from_utf8_lossy(&output.stdout);
        assert_eq!(
            build_shape.trim(),
            "test-support: on",
            "ck-under-test was built without test-support; the test spawn target's features must not vary by invocation: {}\nstderr: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr),
        );
    });
}

pub struct TestDaemon {
    pub registry: Arc<Registry>,
    pub forwarding: Arc<ForwardingTable>,
    pub connection_file_path: PathBuf,
    pub temp_dir: TestTempDir,
    pub task: JoinHandle<Result<(), subc_daemon::ServerError>>,
}

impl TestDaemon {
    pub async fn start(name: &str) -> Self {
        start_test_daemon(name).await
    }
}

impl Drop for TestDaemon {
    fn drop(&mut self) {
        self.task.abort();
        // The temp dir is owned by the `subc_test_support::TestTempDir` guard, whose `Drop` removes
        // the tree (or preserves it on panic).
    }
}

/// Bind-relay breaker policy for a test daemon: consecutive relay timeouts that
/// open a module's breaker, and how long it stays open before one probe.
pub type BreakerPolicy = (u32, Duration);

pub async fn start_test_daemon(name: &str) -> TestDaemon {
    start_test_daemon_inner(name, None, None, None, Vec::new(), None).await
}

pub async fn start_test_daemon_with_process_liveness(
    name: &str,
    process_liveness: Arc<dyn ModuleProcessLiveness>,
) -> TestDaemon {
    start_test_daemon_inner(name, Some(process_liveness), None, None, Vec::new(), None).await
}

pub async fn start_test_daemon_with_process_liveness_and_supervisor(
    name: &str,
    process_liveness: Arc<dyn ModuleProcessLiveness>,
    supervisor_handle: SupervisorHandle,
) -> TestDaemon {
    start_test_daemon_inner(
        name,
        Some(process_liveness),
        Some(supervisor_handle),
        None,
        Vec::new(),
        None,
    )
    .await
}

/// Start a test daemon with an explicit route.bind relay timeout, so the
/// timeout-path tests fire quickly instead of waiting on the production default.
pub async fn start_test_daemon_with_bind_timeout(
    name: &str,
    process_liveness: Arc<dyn ModuleProcessLiveness>,
    supervisor_handle: SupervisorHandle,
    bind_timeout: Duration,
) -> TestDaemon {
    start_test_daemon_inner(
        name,
        Some(process_liveness),
        Some(supervisor_handle),
        Some(bind_timeout),
        Vec::new(),
        None,
    )
    .await
}

/// Start a test daemon with an explicit route.bind relay timeout AND an
/// explicit bind-relay breaker policy.
///
/// The production breaker needs three full 12s budgets to open and stays open
/// for 20s, which no test can afford to wait out; the constants carry the
/// reasoning for those numbers and the tests pin the mechanism.
pub async fn start_test_daemon_with_bind_timeout_and_breaker(
    name: &str,
    process_liveness: Arc<dyn ModuleProcessLiveness>,
    supervisor_handle: SupervisorHandle,
    bind_timeout: Duration,
    breaker: BreakerPolicy,
) -> TestDaemon {
    start_test_daemon_inner(
        name,
        Some(process_liveness),
        Some(supervisor_handle),
        Some(bind_timeout),
        Vec::new(),
        Some(breaker),
    )
    .await
}

/// Start a test daemon with both a daemon-wide route.bind relay timeout AND
/// per-module overrides — mirrors how a live daemon reads `subc.jsonc`. Use
/// when a test needs to prove that a per-module override is what the bind
/// path actually uses (not the daemon-wide default).
pub async fn start_test_daemon_with_route_bind_relay_overrides(
    name: &str,
    process_liveness: Arc<dyn ModuleProcessLiveness>,
    supervisor_handle: SupervisorHandle,
    daemon_wide: Duration,
    per_module: Vec<(String, Duration)>,
) -> TestDaemon {
    start_test_daemon_inner(
        name,
        Some(process_liveness),
        Some(supervisor_handle),
        Some(daemon_wide),
        per_module,
        None,
    )
    .await
}

async fn start_test_daemon_inner(
    name: &str,
    process_liveness: Option<Arc<dyn ModuleProcessLiveness>>,
    supervisor_handle: Option<SupervisorHandle>,
    bind_timeout: Option<Duration>,
    per_module_bind_timeouts: Vec<(String, Duration)>,
    breaker: Option<BreakerPolicy>,
) -> TestDaemon {
    let temp_dir = unique_temp_dir(name);
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let connection_file_path = temp_dir.join("subc-conn.json");
    let conn = ConnectionInfo {
        schema: SCHEMA_VERSION,
        wire_version: Some(PROTOCOL_VERSION),
        endpoints: vec![Endpoint {
            host: Ipv4Addr::LOCALHOST.to_string(),
            port,
        }],
        key: generate_key().unwrap(),
        daemon_id: generate_daemon_id().unwrap(),
        pid: process::id(),
        daemon_ver: TEST_DAEMON_VER.to_owned(),
    };
    write_atomic(&connection_file_path, &conn).unwrap();

    let registry = Arc::new(Registry::default());
    let connected_clients = ConnectedClients::new();
    let mut handler = ControlHandler::new(Arc::clone(&registry))
        .with_connected_clients(connected_clients.clone())
        .with_route_bind_relay_timeouts(per_module_bind_timeouts)
        .with_daemon_provenance(
            conn.pid,
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX),
            std::env::current_exe().ok(),
            Some("test-daemon-build".to_string()),
            Some("test-daemon-lock".to_string()),
        );
    if let Some(process_liveness) = process_liveness {
        handler = handler.with_process_liveness(process_liveness);
    }
    if let Some(supervisor_handle) = supervisor_handle {
        handler = handler.with_supervisor(supervisor_handle);
    }
    if let Some(bind_timeout) = bind_timeout {
        handler = handler.with_route_bind_relay_timeout(bind_timeout);
    }
    if let Some((threshold, cooldown)) = breaker {
        handler = handler.with_route_bind_breaker(threshold, cooldown);
    }
    let control = Arc::new(handler);
    let forwarding = control.forwarding();
    let router = Arc::new(Router::with_control_handler(control));
    let auth = ServerAuth::new(conn.key, conn.daemon_id, conn.daemon_ver)
        .with_connected_clients(connected_clients);
    let task = tokio::spawn(serve_listener(listener, router, auth));

    TestDaemon {
        registry,
        forwarding,
        connection_file_path,
        temp_dir,
        task,
    }
}

pub async fn connect_authed_client(path: impl AsRef<Path>) -> io::Result<TcpStream> {
    let conn = subc_transport::read_for_client(path.as_ref()).map_err(io::Error::other)?;
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
    authenticate_client(&mut stream, &conn, TEST_AUTH_DEADLINE)
        .await
        .map_err(io::Error::other)?;
    Ok(stream)
}

fn unique_temp_dir(name: &str) -> TestTempDir {
    TestTempDir::new(name)
}
