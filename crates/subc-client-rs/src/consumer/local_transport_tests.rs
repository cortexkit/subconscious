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

fn identity_query() -> ActivityQueryRequest {
    serde_json::from_value(serde_json::json!({"op": "activity.query", "limit": 1})).unwrap()
}

async fn run_identity_round(
    client: &SubcConsumer,
    target: &RouteTarget,
    identity: &BindIdentity,
    explicit: &ConsumerIdentity,
) {
    for consumer_identity in [None, Some(explicit.clone())] {
        let handle = client
            .open_route(
                target.clone(),
                identity.clone(),
                CallOptions {
                    consumer_identity: consumer_identity.clone(),
                    timeout: BUDGET,
                    ..CallOptions::default()
                },
            )
            .await
            .unwrap();
        client
            .close_route(
                target.clone(),
                identity.clone(),
                CloseRouteOptions {
                    consumer_identity,
                    ..CloseRouteOptions::default()
                },
            )
            .await;
        assert!(
            client.shared.route_state(handle).is_err(),
            "close must find the same identity-keyed route"
        );
    }
    let error = client.query_activity(identity_query()).await.unwrap_err();
    assert_eq!(error.code(), Some("test_complete"));
    let mut subscription = client.subscribe_activity(None).await.unwrap();
    assert!(subscription.next().await.is_err());
}

fn spawn_identity_server(
    listener: LocalListener,
    info: ConnectionInfo,
    identities: Vec<Option<ConsumerIdentity>>,
    activity_identity: Option<ConsumerIdentity>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut stream = accept(listener).await;
        authenticate_server(
            &mut stream,
            &info.key,
            &info.daemon_id,
            &info.daemon_ver,
            BUDGET,
        )
        .await
        .unwrap();
        for (index, identity) in identities.into_iter().enumerate() {
            let request = read_frame(&mut stream).await.unwrap().unwrap();
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            assert_eq!(body["op"], "route.open");
            assert_eq!(
                body.get("consumer_identity"),
                identity
                    .as_ref()
                    .map(serde_json::to_value)
                    .transpose()
                    .unwrap()
                    .as_ref()
            );
            let channel = 7 + index as u16;
            let reply = Frame::build(
                FrameType::Response,
                control_flags(),
                0,
                0,
                request.header.corr,
                serde_json::to_vec(&ClientControlResponse::RouteOpen {
                    route_channel: channel,
                    route_epoch: 1,
                })
                .unwrap(),
            )
            .unwrap();
            write_frame(&mut stream, &reply).await.unwrap();
            stream.flush().await.unwrap();
            // Closing is a one-way GOODBYE, not a JSON identity-bearing request.
            // Seeing it proves close_route used the same identity-keyed route.
            let goodbye = read_frame(&mut stream).await.unwrap().unwrap();
            assert_eq!(goodbye.header.ty, FrameType::Goodbye);
            assert_eq!(goodbye.header.channel, channel);
            assert!(goodbye.body.is_empty());
        }
        for op in ["activity.query", "activity.subscribe"] {
            let request = read_frame(&mut stream).await.unwrap().unwrap();
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            assert_eq!(body["op"], op);
            assert_eq!(
                body.get("consumer_identity"),
                activity_identity
                    .as_ref()
                    .map(serde_json::to_value)
                    .transpose()
                    .unwrap()
                    .as_ref()
            );
            let reply = Frame::build(
                FrameType::Error,
                control_flags(),
                0,
                0,
                request.header.corr,
                serde_json::to_vec(&ErrorBody::new("test_complete", "identity captured")).unwrap(),
            )
            .unwrap();
            write_frame(&mut stream, &reply).await.unwrap();
            stream.flush().await.unwrap();
        }
    })
}

#[tokio::test]
async fn operator_app_omits_environment_identity_and_honours_explicit() {
    const CHILD: &str = "SUBC_OPERATOR_IDENTITY_CHILD";
    let inherited = ConsumerIdentity {
        module_id: "untrusted-launcher".into(),
        launch_nonce: "untrusted-environment-nonce".into(),
    };
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "consumer::local_transport_tests::operator_app_omits_environment_identity_and_honours_explicit",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("SUBC_MODULE_ID", &inherited.module_id)
            .env("SUBC_LAUNCH_NONCE", &inherited.launch_nonce)
            .env_remove("SUBC_LAUNCH_NONCE_FD")
            .env_remove("SUBC_LAUNCH_NONCE_HANDLE")
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
        return;
    }

    timeout(Duration::from_secs(20), async {
        let dir = socket_dir();
        let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let info = info(&tcp);
        let file = dir.join("connection.json");
        let explicit = ConsumerIdentity {
            module_id: "explicit-caller".into(),
            launch_nonce: "explicit-nonce".into(),
        };
        let target = RouteTarget::ToolProvider {
            module_id: "provider".into(),
        };
        let identity = BindIdentity::new(dir.to_path_buf(), "test", "operator");

        let (endpoint, listener) = listen(&dir, "operator");
        write_atomic_with_local_endpoint(&file, &info, Some(&endpoint)).unwrap();
        let task = spawn_identity_server(
            listener,
            info.clone(),
            vec![None, Some(explicit.clone())],
            None,
        );
        // Inject only the resolved path so the real constructor's policy and
        // local connection setup run without touching the installed daemon.
        let client =
            SubcConsumer::connect_operator_app_resolving(local_opts(), || Ok(file.clone()))
                .await
                .unwrap();

        run_identity_round(&client, &target, &identity, &explicit).await;
        timeout(BUDGET, task).await.unwrap().unwrap();
        let (endpoint, listener) = listen(&dir, "reconnected");
        write_atomic_with_local_endpoint(&file, &info, Some(&endpoint)).unwrap();
        let next = spawn_identity_server(
            listener,
            info.clone(),
            vec![None, Some(explicit.clone())],
            None,
        );
        // The next call must reconnect using the new file endpoint,
        // retaining the operator-app identity policy.
        run_identity_round(&client, &target, &identity, &explicit).await;
        timeout(BUDGET, next).await.unwrap().unwrap();
        client.close().await;

        // Legacy constructors still use the identity supplied through the launch environment.
        let (endpoint, listener) = listen(&dir, "legacy");
        write_atomic_with_local_endpoint(&file, &info, Some(&endpoint)).unwrap();
        let task = spawn_identity_server(
            listener,
            info,
            vec![Some(inherited.clone())],
            Some(inherited),
        );
        let legacy = SubcConsumer::connect_local(&file, local_opts())
            .await
            .unwrap();
        legacy
            .open_route(target.clone(), identity.clone(), CallOptions::default())
            .await
            .unwrap();
        legacy
            .close_route(target, identity, CloseRouteOptions::default())
            .await;
        assert_eq!(
            legacy
                .query_activity(identity_query())
                .await
                .unwrap_err()
                .code(),
            Some("test_complete")
        );
        let mut subscription = legacy.subscribe_activity(None).await.unwrap();
        assert!(subscription.next().await.is_err());
        timeout(BUDGET, task).await.unwrap().unwrap();
        legacy.close().await;
        assert_no_tcp(&tcp).await;
    })
    .await
    .expect("identity exercise must finish");
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
