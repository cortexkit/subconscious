use std::{error::Error, fmt, io, path::Path, sync::Arc};

use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::UnixListener,
};

use crate::{read_frame, router::Router, write_frame, FrameIoError, RouterError};

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
        let router = Arc::clone(&router);
        tokio::spawn(async move {
            let _ = handle_connection(stream, router).await;
        });
    }
}

/// Run the frame read -> route -> write loop for one connection.
pub async fn handle_connection<S>(mut stream: S, router: Arc<Router>) -> Result<(), ConnectionError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    loop {
        let Some(frame) = read_frame(&mut stream)
            .await
            .map_err(ConnectionError::FrameIo)?
        else {
            return Ok(());
        };

        match router.route(frame) {
            Ok(responses) => {
                for response in responses {
                    write_frame(&mut stream, &response)
                        .await
                        .map_err(ConnectionError::FrameIo)?;
                }
            }
            Err(err) => {
                if let Some(error_frame) = err.to_error_frame() {
                    write_frame(&mut stream, &error_frame)
                        .await
                        .map_err(ConnectionError::FrameIo)?;
                } else {
                    return Err(ConnectionError::Router(err));
                }
            }
        }
    }
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
}

impl fmt::Display for ConnectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FrameIo(err) => write!(f, "frame connection error: {err}"),
            Self::Router(err) => write!(f, "router connection error: {err}"),
        }
    }
}

impl Error for ConnectionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::FrameIo(err) => Some(err),
            Self::Router(err) => Some(err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use subc_protocol::{DecodeError, Flags, FrameType, Priority, HEADER_LEN, PROTOCOL_VERSION};
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

        let response = crate::read_frame(&mut client).await.unwrap().unwrap();
        assert_eq!(response.header.ty, FrameType::Response);
        assert_eq!(response.header.channel, 7);
        assert_eq!(response.header.corr, 11);
        assert_eq!(response.body, b"still-routes");

        drop(client);
        server.await.unwrap().unwrap();
    }
}
