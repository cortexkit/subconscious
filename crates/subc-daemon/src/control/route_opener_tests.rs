use super::*;
use crate::{
    local_listener::{self, LocalListenerConfig},
    server::{serve_listener, serve_local_listener, ServerAuth},
};
use subc_transport::{
    authenticate_client, generate_daemon_id, generate_key, ConnectionInfo, Endpoint, LocalEndpoint,
    SCHEMA_VERSION,
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::{TcpListener, TcpStream, UnixStream},
    task::JoinHandle,
};

struct Census {
    handler: Arc<ControlHandler>,
    target: RouteCtx,
    target_rx: tokio::sync::mpsc::Receiver<crate::router::OutboundFrame>,
    info: ConnectionInfo,
    socket: std::path::PathBuf,
    servers: [JoinHandle<Result<(), crate::server::ServerError>>; 2],
}

impl Census {
    async fn start() -> Self {
        let handler = Arc::new(ControlHandler::default());
        let (target, mut target_rx) = route_ctx(ConnectionId::new(50_000));
        hello_via_sink(
            &handler,
            &target,
            &mut target_rx,
            hello_frame("target", PROTOCOL_VERSION, 1),
        )
        .await;
        let router = Arc::new(crate::Router::with_control_handler(Arc::clone(&handler)));
        let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let info = ConnectionInfo {
            schema: SCHEMA_VERSION,
            wire_version: Some(PROTOCOL_VERSION),
            endpoints: vec![Endpoint {
                host: "127.0.0.1".into(),
                port: tcp.local_addr().unwrap().port(),
            }],
            key: generate_key().unwrap(),
            daemon_id: generate_daemon_id().unwrap(),
            pid: std::process::id(),
            daemon_ver: "test".into(),
        };
        let socket_dir = std::env::temp_dir().join(format!("subc-open-{}-{}", info.pid, info.endpoints[0].port));
        let config = LocalListenerConfig::in_socket_dir(&socket_dir);
        let bound = local_listener::bind(&config, &socket_dir.join("conn.json")).unwrap();
        let LocalEndpoint::Unix { path: socket } = bound.endpoint() else {
            panic!("expected Unix endpoint");
        };
        let auth = ServerAuth::new(info.key.clone(), info.daemon_id, info.daemon_ver.clone());
        let servers = [
            tokio::spawn(serve_listener(tcp, Arc::clone(&router), auth.clone())),
            tokio::spawn(serve_local_listener(bound, router, auth)),
        ];
        Self { handler, target, target_rx, info, socket, servers }
    }

    async fn acknowledge_bind(&mut self) {
        let bind = tokio::time::timeout(Duration::from_secs(5), self.target_rx.recv())
            .await
            .expect("no route.bind within 5s")
            .unwrap();
        self.handler
            .handle_control_frame(&self.target, route_bind_ack(bind.header.corr))
            .await
            .unwrap();
    }

    async fn route(&self) -> Value {
        let (ctx, _rx) = route_ctx(ConnectionId::new(50_001));
        let query = Frame::build(
            FrameType::Request,
            control_flags(),
            0,
            0,
            3,
            serde_json::to_vec(&ClientControlRequest::SupervisorRoutes {
                module_id: Some("target".into()),
            })
            .unwrap(),
        )
        .unwrap();
        let replies = self.handler.handle_control_frame(&ctx, query).await.unwrap();
        assert_eq!(replies[0].header.ty, FrameType::Response);
        let body: Value = serde_json::from_slice(&replies[0].body).unwrap();
        let routes = body["modules"][0]["routes"].as_array().unwrap();
        assert_eq!(routes.len(), 1);
        routes[0].clone()
    }

    async fn open(&mut self, stream: &mut (impl AsyncRead + AsyncWrite + Unpin)) {
        send_open(stream, &self.info).await;
        self.acknowledge_bind().await;
        read_open_reply(stream).await;
    }
}

impl Drop for Census {
    fn drop(&mut self) {
        for task in &self.servers {
            task.abort();
        }
        let _ = std::fs::remove_dir_all(self.socket.parent().unwrap());
    }
}

async fn send_open(stream: &mut (impl AsyncRead + AsyncWrite + Unpin), info: &ConnectionInfo) {
    authenticate_client(stream, info, Duration::from_secs(5)).await.unwrap();
    let open = route_open_frame(2, "target", unique_project_root("route-opener"));
    crate::write_frame(stream, &open).await.unwrap();
}

async fn read_open_reply(stream: &mut (impl AsyncRead + Unpin)) {
    let reply = tokio::time::timeout(Duration::from_secs(5), crate::read_frame(stream))
        .await.unwrap().unwrap().unwrap();
    assert!(matches!(
        serde_json::from_slice::<ClientControlResponse>(&reply.body).unwrap(),
        ClientControlResponse::RouteOpen { .. }
    ));
}

#[tokio::test]
async fn supervisor_routes_reports_the_local_opener_pid() {
    let mut census = Census::start().await;
    let mut client = UnixStream::connect(&census.socket).await.unwrap();
    census.open(&mut client).await;
    let route = census.route().await;
    assert_eq!(route["opener"]["transport"], "local");
    assert_eq!(route["opener"]["pid"], std::process::id());
    assert_eq!(route["opener"]["connection_id"], route["consumer"]["connection_id"]);
    assert!(!route["opener"]["command"].as_str().unwrap().is_empty());
}

#[tokio::test]
async fn supervisor_routes_reports_tcp_without_a_pid() {
    let mut census = Census::start().await;
    let mut client = TcpStream::connect(("127.0.0.1", census.info.endpoints[0].port)).await.unwrap();
    census.open(&mut client).await;
    let route = census.route().await;
    assert_eq!(route["opener"]["transport"], "tcp");
    assert_eq!(route["opener"]["connection_id"], route["consumer"]["connection_id"]);
    assert!(route["opener"].get("pid").is_none());
    assert!(route["opener"].get("command").is_none());
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn supervisor_routes_omits_command_after_the_opener_exits() {
    use rustix::net::{recvmsg, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags};
    use std::{io::IoSliceMut, mem::MaybeUninit, os::fd::OwnedFd, os::unix::net::UnixDatagram, process::Stdio};

    let mut census = Census::start().await;
    let (parent_end, child_end) = UnixDatagram::pair().unwrap();
    parent_end.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--ignored",
            "--exact",
            concat!(module_path!(), "::connect_and_pass_the_socket")
                .split_once("::")
                .unwrap()
                .1,
        ])
        .env("SUBC_TEST_ROUTE_OPENER_SOCKET", &census.socket)
        .env("SUBC_TEST_ROUTE_OPENER_INFO", serde_json::to_string(&census.info).unwrap())
        .stdin(Stdio::from(OwnedFd::from(child_end)))
        .kill_on_drop(true);
    let mut child = subc_os::child_spawn::tokio_spawn(&mut command).unwrap();
    let pid = child.id().unwrap();
    census.acknowledge_bind().await;
    let before = census.route().await;
    assert_eq!(before["opener"]["pid"], pid);
    assert!(before["opener"]["command"].is_string());

    // Retain a copy of the opener's socket so the route stays live after the
    // opener exits. Otherwise disconnect cleanup would remove the route.
    parent_end.send(b"x").unwrap();
    let _retained_socket = tokio::task::spawn_blocking(move || {
        let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
        let mut ancillary = RecvAncillaryBuffer::new(&mut space);
        recvmsg(
            &parent_end,
            &mut [IoSliceMut::new(&mut [0])],
            &mut ancillary,
            RecvFlags::CMSG_CLOEXEC,
        )
        .unwrap();
        let Some(RecvAncillaryMessage::ScmRights(mut rights)) = ancillary.drain().next() else {
            panic!("opener did not pass its socket");
        };
        rights.next().unwrap()
    })
    .await
    .unwrap();
    let exited = tokio::time::timeout(Duration::from_secs(5), child.wait()).await.unwrap().unwrap();
    assert!(exited.success());
    let after = census.route().await;
    assert_eq!(after["opener"]["connection_id"], before["opener"]["connection_id"]);
    assert_eq!(after["opener"]["pid"], pid);
    assert!(after["opener"].get("command").is_none());
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "child fixture for the route opener exit test"]
async fn connect_and_pass_the_socket() {
    use rustix::net::{sendmsg, SendAncillaryBuffer, SendAncillaryMessage, SendFlags};
    use std::{io::{IoSlice, Read}, mem::MaybeUninit, os::fd::AsFd};

    let path = std::env::var_os("SUBC_TEST_ROUTE_OPENER_SOCKET").unwrap();
    let info = serde_json::from_str(&std::env::var("SUBC_TEST_ROUTE_OPENER_INFO").unwrap()).unwrap();
    let mut client = UnixStream::connect(path).await.unwrap();
    send_open(&mut client, &info).await;
    read_open_reply(&mut client).await;
    let mut stdin = std::io::stdin();
    stdin.read_exact(&mut [0]).unwrap();
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut ancillary = SendAncillaryBuffer::new(&mut space);
    let socket = [client.as_fd()];
    assert!(ancillary.push(SendAncillaryMessage::ScmRights(&socket)));
    sendmsg(stdin, &[IoSlice::new(b"x")], &mut ancillary, SendFlags::empty()).unwrap();
}
