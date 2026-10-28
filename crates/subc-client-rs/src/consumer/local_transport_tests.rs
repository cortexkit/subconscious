use super::*;
use crate::control_flags;
use cortexkit_test_support::ScratchDir;
use subc_transport::connection_file::{ConnectionInfo, Endpoint, LocalEndpoint, SCHEMA_VERSION};
use subc_transport::{authenticate_server, write_atomic_with_local_endpoint};
use tokio::net::TcpListener;
use tokio::time::timeout;

const BUDGET: Duration = Duration::from_secs(3);

fn socket_dir() -> ScratchDir {
    // Short labels leave room for the runner's prefix within macOS's socket path limit.
    ScratchDir::new("lt")
}

fn info(tcp: &TcpListener) -> ConnectionInfo {
    ConnectionInfo {
        schema: SCHEMA_VERSION,
        wire_version: None,
        endpoints: vec![Endpoint {
            host: "127.0.0.1".into(),
            port: tcp.local_addr().unwrap().port(),
        }],
        key: vec![0x42; 32],
        daemon_id: [0x21; 16],
        pid: std::process::id(),
        daemon_ver: "0.38.0".into(),
    }
}

#[cfg(unix)]
type LocalListener = tokio::net::UnixListener;
#[cfg(windows)]
type LocalListener = tokio::net::windows::named_pipe::NamedPipeServer;

#[cfg(unix)]
fn listen(dir: &Path, name: &str) -> (LocalEndpoint, LocalListener) {
    let path = dir.join(name);
    let listener = LocalListener::bind(&path).unwrap();
    (LocalEndpoint::Unix { path }, listener)
}

#[cfg(windows)]
fn listen(_dir: &Path, name: &str) -> (LocalEndpoint, LocalListener) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let name = format!(
        r"\\.\pipe\subc-sdk-{}-{}-{name}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    let listener = tokio::net::windows::named_pipe::ServerOptions::new()
        .first_pipe_instance(true)
        .create(&name)
        .unwrap();
    (LocalEndpoint::Pipe { name }, listener)
}

async fn accept(listener: LocalListener) -> Box<dyn ConsumerStream> {
    #[cfg(unix)]
    {
        Box::new(listener.accept().await.unwrap().0)
    }
    #[cfg(windows)]
    {
        listener.connect().await.unwrap();
        Box::new(listener)
    }
}

async fn server(mut stream: Box<dyn ConsumerStream>, info: ConnectionInfo) {
    authenticate_server(
        &mut stream,
        &info.key,
        &info.daemon_id,
        &info.daemon_ver,
        BUDGET,
    )
    .await
    .unwrap();
    let request = read_frame(&mut stream).await.unwrap().unwrap();
    assert_eq!(request.header.channel, 0);
    assert_eq!(request.body, br#"{"op":"catalog.list"}"#);
    let reply = Frame::build(
        FrameType::Response,
        control_flags(),
        0,
        0,
        request.header.corr,
        br#"{"op":"catalog.list","generation":1,"modules":[],"subc_ops":[]}"#.to_vec(),
    )
    .unwrap();
    write_frame(&mut stream, &reply).await.unwrap();
    stream.flush().await.unwrap();
}

fn spawn_local(listener: LocalListener, info: ConnectionInfo) -> JoinHandle<()> {
    tokio::spawn(async move { server(accept(listener).await, info).await })
}

async fn assert_no_tcp(tcp: &TcpListener) {
    assert!(
        timeout(Duration::from_millis(20), tcp.accept())
            .await
            .is_err(),
        "local mode attempted TCP"
    );
}

fn local_opts() -> ConsumerOptions {
    ConsumerOptions {
        reconnect_backoff: RetryBackoff {
            max_attempts: 2,
            base: Duration::from_millis(1),
            ..RetryBackoff::default()
        },
        ..ConsumerOptions::default()
    }
}

#[tokio::test]
async fn default_transport_is_tcp_with_unchanged_frames() {
    let dir = socket_dir();
    let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let info = info(&tcp);
    let (endpoint, local) = listen(&dir, "local");
    let file = dir.join("connection.json");
    write_atomic_with_local_endpoint(&file, &info, Some(&endpoint)).unwrap();
    let task =
        tokio::spawn(async move { server(Box::new(tcp.accept().await.unwrap().0), info).await });
    let client = SubcConsumer::connect(&file, ConsumerOptions::default())
        .await
        .unwrap();
    assert!(client.catalog_list().await.unwrap().modules.is_empty());
    client.close().await;
    timeout(BUDGET, task).await.unwrap().unwrap();
    assert!(
        timeout(Duration::from_millis(20), accept(local))
            .await
            .is_err(),
        "default dialed local endpoint"
    );
}

#[tokio::test]
async fn local_transport_handshake_frames_and_reconnect_use_current_endpoint() {
    let dir = socket_dir();
    let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let info = info(&tcp);
    let file = dir.join("connection.json");
    let (first, listener) = listen(&dir, "first");
    write_atomic_with_local_endpoint(&file, &info, Some(&first)).unwrap();
    let task = spawn_local(listener, info.clone());
    let client = SubcConsumer::connect_local(&file, local_opts())
        .await
        .unwrap();
    assert!(client.catalog_list().await.unwrap().modules.is_empty());
    timeout(BUDGET, task).await.unwrap().unwrap();

    let (next, listener) = listen(&dir, "next");
    write_atomic_with_local_endpoint(&file, &info, Some(&next)).unwrap();
    let task = spawn_local(listener, info);
    assert!(client.catalog_list().await.unwrap().modules.is_empty());
    client.close().await;
    timeout(BUDGET, task).await.unwrap().unwrap();
    assert_no_tcp(&tcp).await;
}

#[tokio::test]
async fn local_transport_each_reconnect_attempt_rereads_file() {
    let dir = socket_dir();
    let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let info = info(&tcp);
    let file = dir.join("connection.json");
    let (stale, stale_listener) = listen(&dir, "stale");
    let (current, current_listener) = listen(&dir, "current");
    write_atomic_with_local_endpoint(&file, &info, Some(&stale)).unwrap();
    let task = spawn_local(current_listener, info.clone());
    let rotate_file = file.clone();
    let stale_task = tokio::spawn(async move {
        let mut stream = accept(stale_listener).await;
        // Rotate the endpoint after the first attempt reads the old file but
        // before authentication fails, as during a daemon restart race.
        write_atomic_with_local_endpoint(&rotate_file, &info, Some(&current)).unwrap();
        assert!(authenticate_server(
            &mut stream,
            &[0x99; 32],
            &info.daemon_id,
            &info.daemon_ver,
            BUDGET
        )
        .await
        .is_err());
    });
    let shared = Arc::new(Shared::with_transport(
        file,
        local_opts(),
        ConsumerTransport::Local,
    ));
    shared.lock_inner().reconnect = ReconnectState::Inline { generation: 1 };
    shared.reconnect_with_retry(1).await.unwrap();
    shared.lock_inner().reconnect = ReconnectState::Idle;
    let client = SubcConsumer { shared };
    assert!(client.catalog_list().await.unwrap().modules.is_empty());
    client.close().await;
    timeout(BUDGET, task).await.unwrap().unwrap();
    timeout(BUDGET, stale_task).await.unwrap().unwrap();
    assert_no_tcp(&tcp).await;
}

#[tokio::test]
async fn local_transport_absence_and_connect_error_survive_initial_and_reconnect() {
    for absent in [true, false] {
        let dir = socket_dir();
        let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let info = info(&tcp);
        let file = dir.join("connection.json");
        let (endpoint, listener) = listen(&dir, "unreachable");
        drop(listener);
        let endpoint = (!absent).then_some(endpoint);
        write_atomic_with_local_endpoint(&file, &info, endpoint.as_ref()).unwrap();
        let absent_message = format!(
            "subc connection file '{}' has no endpoint for the requested transport",
            file.display()
        );
        let check_error = |error: ConsumerError| {
            if absent {
                assert_eq!(error.to_string(), absent_message);
                assert!(matches!(error, ConsumerError::NoEndpoint { .. }));
            } else {
                match error {
                    ConsumerError::Connect {
                        endpoint: label,
                        source,
                        ..
                    } => {
                        assert!(label.contains("unreachable"));
                        assert!(matches!(
                            source.kind(),
                            io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                        ));
                    }
                    error => panic!("lost local connect error: {error}"),
                }
            }
        };
        check_error(
            SubcConsumer::connect_local(&file, local_opts())
                .await
                .err()
                .unwrap(),
        );

        let (working, listener) = listen(&dir, "working");
        write_atomic_with_local_endpoint(&file, &info, Some(&working)).unwrap();
        let task = spawn_local(listener, info.clone());
        let client = SubcConsumer::connect_local(&file, local_opts())
            .await
            .unwrap();
        client.catalog_list().await.unwrap();
        client.close().await;
        timeout(BUDGET, task).await.unwrap().unwrap();
        write_atomic_with_local_endpoint(&file, &info, endpoint.as_ref()).unwrap();
        let shared = Arc::new(Shared::with_transport(
            file,
            local_opts(),
            ConsumerTransport::Local,
        ));
        shared.lock_inner().reconnect = ReconnectState::Inline { generation: 1 };
        check_error(shared.reconnect_with_retry(1).await.err().unwrap());
        shared.close_sync("test finished");
        assert_no_tcp(&tcp).await;
    }
}
