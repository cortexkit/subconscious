use std::{error::Error, fmt, io, path::Path, sync::Arc};

use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufWriter},
    net::UnixListener,
    sync::mpsc,
};
use tracing::{debug, warn};

use crate::{
    read_frame,
    router::{FrameSink, RouteCtx, Router},
    write_frame, FrameIoError, RouterError,
};

const CONNECTION_EGRESS_BUFFER: usize = 64;

/// Bind a Unix-domain socket at `path` and serve connections forever.
///
/// Path discovery, per-user singleton locking, stale-socket recovery, and
/// daemon lifecycle are later stories. This function only binds the provided
/// path and starts the accept loop.
pub async fn serve_uds(path: impl AsRef<Path>, router: Arc<Router>) -> Result<(), ServerError> {
    let listener = UnixListener::bind(path).map_err(ServerError::Bind)?;
    serve_listener(listener, router).await
}

/// Serve an already-bound Unix listener. Each accepted connection gets its own
/// async task so concurrent clients do not block the accept loop.
pub async fn serve_listener(
    listener: UnixListener,
    router: Arc<Router>,
) -> Result<(), ServerError> {
    loop {
        let (stream, _) = listener.accept().await.map_err(ServerError::Accept)?;
        debug!("accepted subc Unix socket connection");
        let router = Arc::clone(&router);
        tokio::spawn(async move {
            if let Err(err) = handle_connection(stream, router).await {
                warn!(error = %err, "subc connection ended with error");
            }
        });
    }
}

/// Run the serial frame read -> route loop for one connection.
///
/// Outbound frames flow through a bounded [`FrameSink`] drained by one writer
/// task. This locks in the streaming-capable sink shape while intentionally
/// keeping inbound dispatch serial: each routed frame is awaited before reading
/// the next one.
pub async fn handle_connection<S>(stream: S, router: Arc<Router>) -> Result<(), ConnectionError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let connection = router.begin_connection();
    let connection_id = connection.id();
    debug!(
        connection_id = connection_id.get(),
        "subc connection opened"
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
    Bind(io::Error),
    Accept(io::Error),
}

impl fmt::Display for ServerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bind(err) => write!(f, "failed to bind Unix socket: {err}"),
            Self::Accept(err) => write!(f, "failed to accept Unix socket connection: {err}"),
        }
    }
}

impl Error for ServerError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Bind(err) | Self::Accept(err) => Some(err),
        }
    }
}

#[derive(Debug)]
pub enum ConnectionError {
    FrameIo(FrameIoError),
    Router(RouterError),
    WriterTask(tokio::task::JoinError),
}

impl fmt::Display for ConnectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FrameIo(err) => write!(f, "frame connection error: {err}"),
            Self::Router(err) => write!(f, "router connection error: {err}"),
            Self::WriterTask(err) => write!(f, "connection writer task failed: {err}"),
        }
    }
}

impl Error for ConnectionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::FrameIo(err) => Some(err),
            Self::Router(err) => Some(err),
            Self::WriterTask(err) => Some(err),
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

    use crate::{frame_io::ReadStage, EchoBackend, Frame};

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

    #[tokio::test]
    async fn interleaved_channels_on_one_stream_demux_byte_identically() {
        let (mut client, server_stream) = duplex(4096);
        let server = tokio::spawn(handle_connection(server_stream, echo_router()));
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
    async fn channel_zero_goes_to_subc_self_handler() {
        let (mut client, server_stream) = duplex(512);
        let server = tokio::spawn(handle_connection(
            server_stream,
            Arc::new(Router::with_default_self_handler()),
        ));
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
    async fn malformed_header_returns_typed_error_no_panic() {
        let (mut client, server_stream) = duplex(128);
        let server = tokio::spawn(handle_connection(server_stream, echo_router()));
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
        let server = tokio::spawn(handle_connection(server_stream, echo_router()));
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
        let server = tokio::spawn(handle_connection(server_stream, echo_router()));
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
