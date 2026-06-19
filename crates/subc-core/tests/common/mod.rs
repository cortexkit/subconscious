#![allow(dead_code)]

use std::{
    fs, io,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    process,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use subc_core::{
    auth::authenticate_client,
    connection_file::{
        generate_daemon_id, generate_key, write_atomic, ConnectionInfo, Endpoint, SCHEMA_VERSION,
    },
    serve_listener, ControlHandler, ForwardingTable, Registry, Router, ServerAuth,
};
use tokio::{
    net::{TcpListener, TcpStream},
    task::JoinHandle,
};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
const TEST_DAEMON_VER: &str = "test-subc";
const TEST_AUTH_DEADLINE: Duration = Duration::from_secs(2);

pub struct TestDaemon {
    pub registry: Arc<Registry>,
    pub forwarding: Arc<ForwardingTable>,
    pub connection_file_path: PathBuf,
    pub temp_dir: PathBuf,
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
        let _ = fs::remove_dir_all(&self.temp_dir);
    }
}

pub async fn start_test_daemon(name: &str) -> TestDaemon {
    let temp_dir = unique_temp_dir(name);
    fs::create_dir_all(&temp_dir).unwrap();
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let connection_file_path = temp_dir.join("subc-conn.json");
    let conn = ConnectionInfo {
        schema: SCHEMA_VERSION,
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
    let control = Arc::new(ControlHandler::new(Arc::clone(&registry)));
    let forwarding = control.forwarding();
    let router = Arc::new(Router::with_control_handler(control));
    let auth = ServerAuth::new(conn.key, conn.daemon_id, conn.daemon_ver);
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
    let conn = subc_core::connection_file::read(path.as_ref()).map_err(io::Error::other)?;
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

fn unique_temp_dir(name: &str) -> PathBuf {
    let nonce = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("sc-{name}-{}-{nonce}", process::id()))
}
