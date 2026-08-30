#![allow(dead_code)]

use std::{
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    process,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use subc_core::{
    serve_listener, test_support::TestTempDir, ConnectedClients, ControlHandler, ForwardingTable,
    ModuleProcessLiveness, Registry, Router, ServerAuth, SupervisorHandle,
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

const TEST_DAEMON_VER: &str = "test-subc";
const TEST_AUTH_DEADLINE: Duration = Duration::from_secs(2);

pub struct TestDaemon {
    pub registry: Arc<Registry>,
    pub forwarding: Arc<ForwardingTable>,
    pub connection_file_path: PathBuf,
    pub temp_dir: TestTempDir,
    pub task: JoinHandle<Result<(), subc_core::ServerError>>,
}

impl TestDaemon {
    pub async fn start(name: &str) -> Self {
        start_test_daemon(name).await
    }
}

impl Drop for TestDaemon {
    fn drop(&mut self) {
        self.task.abort();
        // The temp dir is owned by the `TestTempDir` guard, whose `Drop` removes
        // the tree (or preserves it on panic).
    }
}

pub async fn start_test_daemon(name: &str) -> TestDaemon {
    start_test_daemon_inner(name, None, None, None, Vec::new()).await
}

pub async fn start_test_daemon_with_process_liveness(
    name: &str,
    process_liveness: Arc<dyn ModuleProcessLiveness>,
) -> TestDaemon {
    start_test_daemon_inner(name, Some(process_liveness), None, None, Vec::new()).await
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
    )
    .await
}

async fn start_test_daemon_inner(
    name: &str,
    process_liveness: Option<Arc<dyn ModuleProcessLiveness>>,
    supervisor_handle: Option<SupervisorHandle>,
    bind_timeout: Option<Duration>,
    per_module_bind_timeouts: Vec<(String, Duration)>,
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
