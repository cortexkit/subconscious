use std::{error::Error, fmt, io, net::SocketAddr, sync::Arc, time::Duration};

use subc_transport::{authenticate_server, AuthError, DAEMON_ID_LEN};
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufWriter},
    net::TcpListener,
    sync::{mpsc, Semaphore},
};
use tracing::{debug, warn};

use crate::{
    read_frame,
    router::{FrameSink, RouteCtx, Router},
    write_frame, FrameIoError, RouterError,
};

pub const CONNECTION_EGRESS_BUFFER: usize = 64;
pub const DEFAULT_AUTH_DEADLINE: Duration = Duration::from_secs(2);
pub const DEFAULT_MAX_UNAUTHENTICATED_CONNECTIONS: usize = 32;

/// Authentication material and DoS bounds applied before a TCP connection may
/// reach the frame router.
#[derive(Clone)]
pub struct ServerAuth {
    key: Arc<[u8]>,
    daemon_id: [u8; DAEMON_ID_LEN],
    daemon_ver: Arc<str>,
    deadline: Duration,
    unauthenticated: Arc<Semaphore>,
}

impl ServerAuth {
    pub fn new(
        key: Vec<u8>,
        daemon_id: [u8; DAEMON_ID_LEN],
        daemon_ver: impl Into<String>,
    ) -> Self {
        Self::with_limits(
            key,
            daemon_id,
            daemon_ver,
            DEFAULT_AUTH_DEADLINE,
            DEFAULT_MAX_UNAUTHENTICATED_CONNECTIONS,
        )
    }

    pub fn with_limits(
        key: Vec<u8>,
        daemon_id: [u8; DAEMON_ID_LEN],
        daemon_ver: impl Into<String>,
        deadline: Duration,
        max_unauthenticated: usize,
    ) -> Self {
        Self {
            key: Arc::from(key),
            daemon_id,
            daemon_ver: Arc::from(daemon_ver.into()),
            deadline,
            unauthenticated: Arc::new(Semaphore::new(max_unauthenticated.max(1))),
        }
    }
}

impl fmt::Debug for ServerAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServerAuth")
            .field("key", &"<redacted>")
            .field("daemon_id", &self.daemon_id)
            .field("daemon_ver", &self.daemon_ver)
            .field("deadline", &self.deadline)
            .finish_non_exhaustive()
    }
}

/// Serve an already-bound TCP listener. Each accepted connection gets its own
/// async task so concurrent clients do not block the accept loop.
pub async fn serve_listener(
    listener: TcpListener,
    router: Arc<Router>,
    auth: ServerAuth,
) -> Result<(), ServerError> {
    let local_addr = listener.local_addr().ok();
    loop {
        let (stream, peer_addr) = listener
            .accept()
            .await
            .map_err(|source| ServerError::Accept { local_addr, source })?;
        debug!(?peer_addr, ?local_addr, "accepted subc TCP connection");
        let router = Arc::clone(&router);
        let auth = auth.clone();
        tokio::spawn(async move {
            if let Err(err) = handle_connection(stream, router, auth).await {
                if err.is_quiet_reject() {
                    debug!(?peer_addr, error = %err, "subc TCP connection rejected before routing");
                } else {
                    warn!(?peer_addr, error = %err, "subc connection ended with error");
                }
            }
        });
    }
}

/// Serve all already-bound loopback TCP listeners until one accept loop fails.
pub async fn serve_listeners(
    listeners: Vec<TcpListener>,
    router: Arc<Router>,
    auth: ServerAuth,
) -> Result<(), ServerError> {
    if listeners.is_empty() {
        return Err(ServerError::NoListeners);
    }

    let (tx, mut rx) = mpsc::channel(listeners.len());
    for listener in listeners {
        let router = Arc::clone(&router);
        let auth = auth.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            let result = serve_listener(listener, router, auth).await;
            let _ = tx.send(result).await;
        });
    }
    drop(tx);

    rx.recv().await.unwrap_or(Ok(()))
}

/// Run the authenticated frame read -> route loop for one connection.
///
/// Every accepted TCP connection must complete the key-auth prelude before any
/// envelope bytes are read by the router. Outbound frames flow through a bounded
/// [`FrameSink`] drained by one writer task. This locks in the streaming-capable
/// sink shape while intentionally keeping inbound dispatch serial: each routed
/// frame is awaited before reading the next one.
pub async fn handle_connection<S>(
    mut stream: S,
    router: Arc<Router>,
    auth: ServerAuth,
) -> Result<(), ConnectionError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let permit = match auth.unauthenticated.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            let _ = stream.shutdown().await;
            return Err(ConnectionError::UnauthenticatedCapacity);
        }
    };

    authenticate_server(
        &mut stream,
        auth.key.as_ref(),
        &auth.daemon_id,
        auth.daemon_ver.as_ref(),
        auth.deadline,
    )
    .await
    .map_err(ConnectionError::Auth)?;
    drop(permit);

    let connection = router.begin_connection();
    let connection_id = connection.id();
    debug!(
        connection_id = connection_id.get(),
        "subc authenticated connection opened"
    );

    let (mut read_half, write_half) = tokio::io::split(stream);
    let (tx, rx) = mpsc::channel::<crate::Frame>(CONNECTION_EGRESS_BUFFER);
    let writer = tokio::spawn(drain_writer(write_half, rx));

    let egress = FrameSink::new(tx);
    let ctx = RouteCtx {
        connection_id,
        egress: egress.clone(),
    };

    let loop_result = connection_loop(&mut read_half, &router, &ctx).await;

    drop(ctx);
    drop(egress);
    drop(connection);

    let writer_result = writer.await.map_err(ConnectionError::WriterTask);
    let result = match (loop_result, writer_result) {
        (Err(loop_err), Ok(Ok(()))) => Err(loop_err),
        (Err(loop_err), Ok(Err(writer_err))) => {
            warn!(
                connection_id = connection_id.get(),
                writer_error = %writer_err,
                "writer failed while closing after connection error"
            );
            Err(loop_err)
        }
        (Err(loop_err), Err(join_err)) => {
            warn!(
                connection_id = connection_id.get(),
                join_error = %join_err,
                "writer task join failed while closing after connection error"
            );
            Err(loop_err)
        }
        (Ok(()), Ok(Ok(()))) => Ok(()),
        (Ok(()), Ok(Err(writer_err))) => Err(ConnectionError::FrameIo(writer_err)),
        (Ok(()), Err(join_err)) => Err(join_err),
    };

    match &result {
        Ok(()) => debug!(
            connection_id = connection_id.get(),
            "subc connection closed"
        ),
        Err(err) => debug!(
            connection_id = connection_id.get(),
            error = %err,
            "subc connection exited with error"
        ),
    }

    result
}

async fn connection_loop<R>(
    read_half: &mut R,
    router: &Router,
    ctx: &RouteCtx,
) -> Result<(), ConnectionError>
where
    R: AsyncRead + Unpin,
{
    loop {
        let Some(frame) = read_frame(read_half)
            .await
            .map_err(ConnectionError::FrameIo)?
        else {
            return Ok(());
        };

        if let Err(err) = router.route_for_connection(ctx, frame).await {
            if let Some(error_frame) = err.to_error_frame() {
                warn!(
                    connection_id = ctx.connection_id.get(),
                    error = %err,
                    "routing failure recovered with ERROR frame"
                );
                ctx.egress
                    .send(error_frame)
                    .await
                    .map_err(ConnectionError::Router)?;
            } else {
                debug!(
                    connection_id = ctx.connection_id.get(),
                    error = %err,
                    "fatal routing failure"
                );
                return Err(ConnectionError::Router(err));
            }
        }
    }
}

async fn drain_writer<W>(
    write_half: W,
    mut rx: mpsc::Receiver<crate::Frame>,
) -> Result<(), FrameIoError>
where
    W: AsyncWrite + Unpin,
{
    let mut writer = BufWriter::new(write_half);
    while let Some(frame) = rx.recv().await {
        write_frame(&mut writer, &frame).await?;
        while let Ok(frame) = rx.try_recv() {
            write_frame(&mut writer, &frame).await?;
        }
        writer.flush().await.map_err(FrameIoError::Io)?;
    }
    writer.flush().await.map_err(FrameIoError::Io)?;
    Ok(())
}

#[derive(Debug)]
pub enum ServerError {
    NoListeners,
    Accept {
        local_addr: Option<SocketAddr>,
        source: io::Error,
    },
}

impl fmt::Display for ServerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoListeners => write!(f, "no TCP listeners were provided"),
            Self::Accept { local_addr, source } => match local_addr {
                Some(addr) => write!(f, "failed to accept TCP connection on {addr}: {source}"),
                None => write!(f, "failed to accept TCP connection: {source}"),
            },
        }
    }
}

impl Error for ServerError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Accept { source, .. } => Some(source),
            Self::NoListeners => None,
        }
    }
}

#[derive(Debug)]
pub enum ConnectionError {
    Auth(AuthError),
    UnauthenticatedCapacity,
    FrameIo(FrameIoError),
    Router(RouterError),
    WriterTask(tokio::task::JoinError),
}

impl ConnectionError {
    fn is_quiet_reject(&self) -> bool {
        matches!(self, Self::Auth(_) | Self::UnauthenticatedCapacity)
    }
}

impl fmt::Display for ConnectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Auth(err) => write!(f, "connection auth failed: {err}"),
            Self::UnauthenticatedCapacity => write!(
                f,
                "too many concurrent unauthenticated subc TCP connections"
            ),
            Self::FrameIo(err) => write!(f, "frame connection error: {err}"),
            Self::Router(err) => write!(f, "router connection error: {err}"),
            Self::WriterTask(err) => write!(f, "connection writer task failed: {err}"),
        }
    }
}

impl Error for ConnectionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Auth(err) => Some(err),
            Self::FrameIo(err) => Some(err),
            Self::Router(err) => Some(err),
            Self::WriterTask(err) => Some(err),
            Self::UnauthenticatedCapacity => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use subc_protocol::{
        DecodeError, ErrorBody, Flags, FrameType, Priority, HEADER_LEN, PROTOCOL_VERSION,
    };
    use tokio::io::{duplex, AsyncWriteExt};

    use subc_transport::{authenticate_client, ConnectionInfo, Endpoint, SCHEMA_VERSION};

    use crate::{frame_io::ReadStage, ControlHandler, EchoBackend, Frame, Registry};

    const TEST_DEADLINE: Duration = Duration::from_secs(2);
    const TEST_DAEMON_VER: &str = "test-subc-server";

    fn request(channel: u16, corr: u64, body: &[u8]) -> Frame {
        Frame::build(
            FrameType::Request,
            Flags::new(true, Priority::Interactive, false),
            channel,
            corr,
            body.to_vec(),
        )
        .unwrap()
    }

    fn echo_router() -> Arc<Router> {
        let mut router = Router::with_default_self_handler();
        router.register_backend(7, EchoBackend).unwrap();
        router.register_backend(9, EchoBackend).unwrap();
        Arc::new(router)
    }

    fn test_auth() -> (ServerAuth, ConnectionInfo) {
        let key = vec![0x42; 32];
        let daemon_id = [0x24; 16];
        let conn = ConnectionInfo {
            schema: SCHEMA_VERSION,
            endpoints: vec![Endpoint {
                host: "127.0.0.1".to_owned(),
                port: 1,
            }],
            key: key.clone(),
            daemon_id,
            pid: std::process::id(),
            daemon_ver: TEST_DAEMON_VER.to_owned(),
        };
        (
            ServerAuth::with_limits(key, daemon_id, TEST_DAEMON_VER, TEST_DEADLINE, 4),
            conn,
        )
    }

    async fn authenticate<S>(stream: &mut S, conn: &ConnectionInfo)
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        authenticate_client(stream, conn, TEST_DEADLINE)
            .await
            .expect("test client should authenticate")
    }

    #[tokio::test]
    async fn interleaved_channels_on_one_stream_demux_byte_identically_after_auth() {
        let (mut client, server_stream) = duplex(4096);
        let (auth, conn) = test_auth();
        let server = tokio::spawn(handle_connection(server_stream, echo_router(), auth));
        authenticate(&mut client, &conn).await;
        let frames = [
            request(7, 1, b"chan7-first\0opaque"),
            request(9, 2, b"chan9-middle-{json?}"),
            request(7, 3, b"chan7-second\xffbytes"),
        ];

        for frame in &frames {
            crate::write_frame(&mut client, frame).await.unwrap();
        }

        for expected in &frames {
            let response = crate::read_frame(&mut client).await.unwrap().unwrap();
            assert_eq!(response.header.ty, FrameType::Response);
            assert_eq!(response.header.channel, expected.header.channel);
            assert_eq!(response.header.corr, expected.header.corr);
            assert_eq!(response.body, expected.body);
        }

        drop(client);
        server.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn channel_zero_goes_to_subc_self_handler_after_auth() {
        let (mut client, server_stream) = duplex(512);
        let (auth, conn) = test_auth();
        let server = tokio::spawn(handle_connection(
            server_stream,
            Arc::new(Router::with_default_self_handler()),
            auth,
        ));
        authenticate(&mut client, &conn).await;
        let ping = Frame::build(
            FrameType::Ping,
            Flags::new(false, Priority::Passive, false),
            0,
            55,
            Vec::new(),
        )
        .unwrap();

        crate::write_frame(&mut client, &ping).await.unwrap();
        let response = crate::read_frame(&mut client).await.unwrap().unwrap();

        assert_eq!(response.header.ty, FrameType::Pong);
        assert_eq!(response.header.channel, 0);
        assert_eq!(response.header.corr, 55);
        assert!(response.body.is_empty());

        drop(client);
        server.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn unauthenticated_connection_is_rejected_before_routing() {
        let (mut client, server_stream) = duplex(512);
        let (auth, _conn) = test_auth();
        let registry = Arc::new(Registry::default());
        let router = Arc::new(Router::with_control_handler(Arc::new(ControlHandler::new(
            Arc::clone(&registry),
        ))));
        let server = tokio::spawn(handle_connection(server_stream, router, auth));
        let ping = Frame::build(
            FrameType::Ping,
            Flags::new(false, Priority::Passive, false),
            0,
            66,
            Vec::new(),
        )
        .unwrap();

        crate::write_frame(&mut client, &ping).await.unwrap();
        if let Ok(Ok(Some(frame))) =
            tokio::time::timeout(Duration::from_millis(200), crate::read_frame(&mut client)).await
        {
            panic!("unauthenticated frame reached router: {frame:?}");
        }

        let err = server.await.unwrap().unwrap_err();
        assert!(matches!(err, ConnectionError::Auth(_)));
        assert_eq!(registry.active_registration_count().unwrap(), 0);
    }

    #[tokio::test]
    async fn malformed_header_returns_typed_error_no_panic() {
        let (mut client, server_stream) = duplex(128);
        let (auth, conn) = test_auth();
        let server = tokio::spawn(handle_connection(server_stream, echo_router(), auth));
        authenticate(&mut client, &conn).await;
        let mut header = [0u8; HEADER_LEN];
        header[4] = PROTOCOL_VERSION;
        header[5] = 250;

        client.write_all(&header).await.unwrap();
        drop(client);

        let err = server.await.unwrap().unwrap_err();
        assert!(matches!(
            err,
            ConnectionError::FrameIo(FrameIoError::DecodeHeader(DecodeError::UnknownFrameType {
                byte: 250
            }))
        ));
    }

    #[tokio::test]
    async fn truncated_body_returns_typed_error_no_panic() {
        let (mut client, server_stream) = duplex(128);
        let (auth, conn) = test_auth();
        let server = tokio::spawn(handle_connection(server_stream, echo_router(), auth));
        authenticate(&mut client, &conn).await;
        let frame = request(7, 8, b"abcd");

        client.write_all(&frame.header.encode()).await.unwrap();
        client.write_all(b"ab").await.unwrap();
        drop(client);

        let err = server.await.unwrap().unwrap_err();
        assert!(matches!(
            err,
            ConnectionError::FrameIo(FrameIoError::UnexpectedEof {
                stage: ReadStage::Body,
                expected: 4,
                actual: 2
            })
        ));
    }

    #[tokio::test]
    async fn unknown_channel_is_returned_as_error_frame_and_connection_continues() {
        let (mut client, server_stream) = duplex(1024);
        let (auth, conn) = test_auth();
        let server = tokio::spawn(handle_connection(server_stream, echo_router(), auth));
        authenticate(&mut client, &conn).await;
        let unknown = request(42, 10, b"lost");
        let known = request(7, 11, b"still-routes");

        crate::write_frame(&mut client, &unknown).await.unwrap();
        crate::write_frame(&mut client, &known).await.unwrap();

        let error = crate::read_frame(&mut client).await.unwrap().unwrap();
        assert_eq!(error.header.ty, FrameType::Error);
        assert_eq!(error.header.channel, 42);
        assert_eq!(error.header.corr, 10);
        let error_body: ErrorBody = serde_json::from_slice(&error.body).unwrap();
        assert_eq!(error_body.code, "unknown_channel");

        let response = crate::read_frame(&mut client).await.unwrap().unwrap();
        assert_eq!(response.header.ty, FrameType::Response);
        assert_eq!(response.header.channel, 7);
        assert_eq!(response.header.corr, 11);
        assert_eq!(response.body, b"still-routes");

        drop(client);
        server.await.unwrap().unwrap();
    }
}
