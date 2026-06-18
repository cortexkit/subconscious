use std::{collections::HashMap, error::Error, fmt, sync::Arc};

use subc_protocol::{Flags, FrameType, Priority};

use crate::{Frame, FrameBuildError};

/// A destination that can receive an opaque envelope body for a routed channel.
///
/// Implementations receive the decoded header plus body bytes. The router does
/// not deserialize those bytes; backends own any payload semantics.
pub trait Backend: Send + Sync {
    fn handle(&self, frame: Frame) -> Result<Vec<Frame>, RouterError>;
}

/// I/O-agnostic splice router keyed by envelope `channel`.
///
/// Channel 0 is reserved for subc itself and is always dispatched to the
/// configured self handler. Other channels must be explicitly registered.
/// Unknown non-zero channels return a typed [`RouterError::UnknownChannel`];
/// the socket layer may translate that into an `ERROR` frame for the peer.
pub struct Router {
    backends: HashMap<u16, Arc<dyn Backend>>,
    self_handler: Arc<dyn Backend>,
}

impl Router {
    pub fn new(self_handler: Arc<dyn Backend>) -> Self {
        Self {
            backends: HashMap::new(),
            self_handler,
        }
    }

    pub fn with_default_self_handler() -> Self {
        Self::new(Arc::new(SubcSelfHandler))
    }

    pub fn register_backend<B>(&mut self, channel: u16, backend: B) -> Result<(), RouterError>
    where
        B: Backend + 'static,
    {
        self.register_backend_arc(channel, Arc::new(backend))
    }

    pub fn register_backend_arc(
        &mut self,
        channel: u16,
        backend: Arc<dyn Backend>,
    ) -> Result<(), RouterError> {
        if channel == 0 {
            return Err(RouterError::ReservedChannelZero);
        }
        if self.backends.contains_key(&channel) {
            return Err(RouterError::DuplicateChannel { channel });
        }
        self.backends.insert(channel, backend);
        Ok(())
    }

    /// Route a frame by header only, moving the body bytes unchanged into the
    /// selected backend.
    pub fn route(&self, frame: Frame) -> Result<Vec<Frame>, RouterError> {
        let channel = frame.header.channel;
        if channel == 0 {
            return self.self_handler.handle(frame);
        }

        let backend = self
            .backends
            .get(&channel)
            .ok_or(RouterError::UnknownChannel {
                channel,
                corr: frame.header.corr,
            })?;
        backend.handle(frame)
    }
}

impl Default for Router {
    fn default() -> Self {
        Self::with_default_self_handler()
    }
}

/// Minimal in-memory backend used by tests and early wiring: it returns a
/// `RESPONSE` on the same channel/correlation id with the exact same body bytes.
#[derive(Debug, Default, Clone, Copy)]
pub struct EchoBackend;

impl Backend for EchoBackend {
    fn handle(&self, frame: Frame) -> Result<Vec<Frame>, RouterError> {
        let response = Frame::build_with_version(
            frame.header.ver,
            FrameType::Response,
            frame.header.flags,
            frame.header.channel,
            frame.header.corr,
            frame.body,
        )
        .map_err(RouterError::FrameBuild)?;
        Ok(vec![response])
    }
}

/// The default channel-0 handler for the skeleton daemon.
///
/// Story 1.6 will replace this with HELLO/manifest registration. For now, PING
/// is answered as PONG; other channel-0 frames receive an ERROR frame so they do
/// not fall through to module backends.
#[derive(Debug, Default, Clone, Copy)]
pub struct SubcSelfHandler;

impl Backend for SubcSelfHandler {
    fn handle(&self, frame: Frame) -> Result<Vec<Frame>, RouterError> {
        match frame.header.ty {
            FrameType::Ping => Ok(vec![Frame::build_with_version(
                frame.header.ver,
                FrameType::Pong,
                frame.header.flags,
                0,
                frame.header.corr,
                Vec::new(),
            )
            .map_err(RouterError::FrameBuild)?]),
            FrameType::Goodbye => Ok(Vec::new()),
            _ => Ok(vec![Frame::build_with_version(
                frame.header.ver,
                FrameType::Error,
                Flags::new(false, Priority::Passive, false),
                0,
                frame.header.corr,
                b"subc channel-0 handler not implemented".to_vec(),
            )
            .map_err(RouterError::FrameBuild)?]),
        }
    }
}

/// Typed router errors. Unknown channels are intentionally represented as typed
/// errors at the pure-router boundary; [`RouterError::to_error_frame`] provides
/// the socket-layer translation to an `ERROR` frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouterError {
    ReservedChannelZero,
    DuplicateChannel {
        channel: u16,
    },
    UnknownChannel {
        channel: u16,
        corr: u64,
    },
    Backend {
        channel: u16,
        corr: u64,
        message: String,
    },
    FrameBuild(FrameBuildError),
}

impl RouterError {
    pub fn backend(channel: u16, corr: u64, message: impl Into<String>) -> Self {
        Self::Backend {
            channel,
            corr,
            message: message.into(),
        }
    }

    /// Translate route failures that belong on the wire into an `ERROR` frame.
    pub fn to_error_frame(&self) -> Option<Frame> {
        match self {
            Self::UnknownChannel { channel, corr } => error_frame(
                *channel,
                *corr,
                format!("unknown channel {channel}").into_bytes(),
            ),
            Self::Backend {
                channel,
                corr,
                message,
            } => error_frame(*channel, *corr, message.as_bytes().to_vec()),
            Self::ReservedChannelZero | Self::DuplicateChannel { .. } | Self::FrameBuild(_) => None,
        }
    }
}

fn error_frame(channel: u16, corr: u64, body: Vec<u8>) -> Option<Frame> {
    Frame::build(
        FrameType::Error,
        Flags::new(false, Priority::Passive, false),
        channel,
        corr,
        body,
    )
    .ok()
}

impl fmt::Display for RouterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReservedChannelZero => write!(f, "channel 0 is reserved for subc"),
            Self::DuplicateChannel { channel } => {
                write!(f, "backend already registered for channel {channel}")
            }
            Self::UnknownChannel { channel, corr } => {
                write!(f, "unknown channel {channel} for corr {corr}")
            }
            Self::Backend {
                channel,
                corr,
                message,
            } => write!(
                f,
                "backend error on channel {channel} corr {corr}: {message}"
            ),
            Self::FrameBuild(err) => write!(f, "failed to build routed frame: {err}"),
        }
    }
}

impl Error for RouterError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::FrameBuild(err) => Some(err),
            Self::ReservedChannelZero
            | Self::DuplicateChannel { .. }
            | Self::UnknownChannel { .. }
            | Self::Backend { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use subc_protocol::{Flags, FrameType, Priority};

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

    #[derive(Debug)]
    struct StaticBackend {
        body: Vec<u8>,
    }

    impl Backend for StaticBackend {
        fn handle(&self, frame: Frame) -> Result<Vec<Frame>, RouterError> {
            Ok(vec![Frame::build_with_version(
                frame.header.ver,
                FrameType::Response,
                frame.header.flags,
                frame.header.channel,
                frame.header.corr,
                self.body.clone(),
            )
            .unwrap()])
        }
    }

    #[test]
    fn echo_backend_returns_response_with_byte_identical_body() {
        let mut router = Router::with_default_self_handler();
        router.register_backend(7, EchoBackend).unwrap();
        let body = b"{not parsed}\0\xff";

        let responses = router.route(request(7, 123, body)).unwrap();

        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0].header.ty, FrameType::Response);
        assert_eq!(responses[0].header.channel, 7);
        assert_eq!(responses[0].header.corr, 123);
        assert_eq!(responses[0].body, body);
    }

    #[test]
    fn unknown_channel_returns_typed_error() {
        let router = Router::with_default_self_handler();

        let err = router.route(request(99, 5, b"payload")).unwrap_err();

        assert_eq!(
            err,
            RouterError::UnknownChannel {
                channel: 99,
                corr: 5
            }
        );
        let error_frame = err.to_error_frame().unwrap();
        assert_eq!(error_frame.header.ty, FrameType::Error);
        assert_eq!(error_frame.header.channel, 99);
        assert_eq!(error_frame.header.corr, 5);
    }

    #[test]
    fn channel_zero_uses_self_handler_not_backend_registry() {
        let self_handler = Arc::new(StaticBackend {
            body: b"self".to_vec(),
        });
        let mut router = Router::new(self_handler);
        router.register_backend(1, EchoBackend).unwrap();

        let responses = router.route(request(0, 77, b"control")).unwrap();

        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0].header.channel, 0);
        assert_eq!(responses[0].body, b"self");
    }

    #[test]
    fn channel_zero_cannot_be_registered_as_backend() {
        let mut router = Router::with_default_self_handler();

        let err = router.register_backend(0, EchoBackend).unwrap_err();

        assert_eq!(err, RouterError::ReservedChannelZero);
    }
}
