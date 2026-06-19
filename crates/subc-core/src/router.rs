use std::{
    collections::HashMap,
    error::Error,
    fmt,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use subc_protocol::{ErrorBody, Flags, FrameType, Priority};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::{
    control::ControlHandler,
    forwarding::{ForwardingError, ForwardingTable},
    registry::ConnectionId,
    Frame, FrameBuildError,
};

/// Cheaply cloneable handle to one connection's bounded outbound frame queue.
///
/// Backends emit responses, streaming frames, and future PUSH frames through this
/// single path. The bounded `mpsc` sender is the connection-level backpressure
/// substrate; the socket layer owns the sole receiver/writer.
#[derive(Debug, Clone)]
pub struct FrameSink {
    tx: mpsc::Sender<Frame>,
}

impl FrameSink {
    pub fn new(tx: mpsc::Sender<Frame>) -> Self {
        Self { tx }
    }

    pub async fn send(&self, frame: Frame) -> Result<(), RouterError> {
        let channel = frame.header.channel;
        let corr = frame.header.corr;
        self.tx
            .send(frame)
            .await
            .map_err(|_| RouterError::backend(channel, corr, "connection writer closed"))
    }

    pub(crate) fn try_send(&self, frame: Frame) -> Result<(), RouterError> {
        let channel = frame.header.channel;
        let corr = frame.header.corr;
        self.tx.try_send(frame).map_err(|err| {
            RouterError::backend(
                channel,
                corr,
                format!("connection writer unavailable: {err}"),
            )
        })
    }
}

/// Per-route context shared with backends besides the frame itself.
#[derive(Debug, Clone)]
pub struct RouteCtx {
    pub connection_id: ConnectionId,
    pub egress: FrameSink,
}

/// Closed set of data-plane backends for non-zero channels.
///
/// Channel 0 is structurally special and remains [`Router::control`], not an
/// enum variant. Static/test backends live in [`Router::backends`]; the forwarding
/// backend is selected dynamically only when a per-connection forwarding binding exists.
#[derive(Debug, Clone)]
pub enum Backend {
    Echo(EchoBackend),
    Forward(ForwardBackend),
}

impl From<EchoBackend> for Backend {
    fn from(backend: EchoBackend) -> Self {
        Self::Echo(backend)
    }
}

impl From<ForwardBackend> for Backend {
    fn from(backend: ForwardBackend) -> Self {
        Self::Forward(backend)
    }
}

impl Backend {
    pub async fn handle(&self, ctx: RouteCtx, frame: Frame) -> Result<(), RouterError> {
        match self {
            Self::Echo(backend) => backend.handle(ctx, frame).await,
            Self::Forward(backend) => backend.handle(ctx, frame).await,
        }
    }
}

/// I/O-agnostic splice router keyed by envelope `channel`.
///
/// Channel 0 is reserved for subc itself and is always dispatched to the
/// dedicated control handler. Other channels must be explicitly registered.
/// Unknown client-originated non-zero channels are translated to canonical JSON `ERROR` frames on
/// the connection sink so the peer can continue using the same socket. Module-originated frames for
/// released route channels are logged and dropped as the channel-gone race backstop.
pub struct Router {
    backends: HashMap<u16, Backend>,
    control: Arc<ControlHandler>,
    forwarding: Arc<ForwardingTable>,
    forward_backend: Backend,
    next_connection_id: AtomicU64,
}

impl Router {
    pub fn with_control_handler(control: Arc<ControlHandler>) -> Self {
        let forwarding = control.forwarding();
        Self {
            backends: HashMap::new(),
            control,
            forwarding: Arc::clone(&forwarding),
            forward_backend: Backend::Forward(ForwardBackend::new(forwarding)),
            // ConnectionId::LOCAL is 0; real socket ids start at 1 and never collide.
            next_connection_id: AtomicU64::new(1),
        }
    }

    pub fn with_default_self_handler() -> Self {
        Self::with_control_handler(Arc::new(ControlHandler::default()))
    }

    pub fn forwarding(&self) -> Arc<ForwardingTable> {
        Arc::clone(&self.forwarding)
    }

    pub fn register_backend(
        &mut self,
        channel: u16,
        backend: impl Into<Backend>,
    ) -> Result<(), RouterError> {
        self.register_backend_arc(channel, Arc::new(backend.into()))
    }

    pub(crate) fn register_backend_arc(
        &mut self,
        channel: u16,
        backend: Arc<Backend>,
    ) -> Result<(), RouterError> {
        if channel == 0 {
            return Err(RouterError::ReservedChannelZero);
        }
        if self.backends.contains_key(&channel) {
            return Err(RouterError::DuplicateChannel { channel });
        }
        self.backends.insert(channel, backend.as_ref().clone());
        Ok(())
    }

    /// Start a connection-scoped routing context. Dropping the guard releases
    /// any control-plane registrations owned by the connection.
    pub fn begin_connection(&self) -> RouterConnection {
        let raw = self.next_connection_id.fetch_add(1, Ordering::Relaxed);
        RouterConnection {
            id: ConnectionId::new(raw),
            control_handler: Arc::clone(&self.control),
        }
    }

    /// Route a frame associated with one socket connection.
    ///
    /// Dispatch remains serial at the caller: this async function is awaited for
    /// each inbound frame before the connection reads the next one.
    pub async fn route_for_connection(
        &self,
        ctx: &RouteCtx,
        frame: Frame,
    ) -> Result<(), RouterError> {
        let channel = frame.header.channel;
        let corr = frame.header.corr;
        if channel == 0 {
            debug!(
                connection_id = ctx.connection_id.get(),
                corr,
                frame_type = ?frame.header.ty,
                "routing control frame"
            );
            let responses = self.control.handle_control_frame(ctx, frame).await?;
            for response in responses {
                ctx.egress.send(response).await?;
            }
            return Ok(());
        }

        let is_module_connection = self
            .forwarding
            .module_endpoint_for_connection(ctx.connection_id)
            .map_err(RouterError::Forwarding)?
            .is_some();

        if is_module_connection {
            if frame.header.ty == FrameType::Goodbye {
                if let Some(target) = self
                    .forwarding
                    .release_module_route(ctx.connection_id, channel)
                    .map_err(RouterError::Forwarding)?
                {
                    debug!(
                        connection_id = ctx.connection_id.get(),
                        module_channel = channel,
                        client_channel = target.channel,
                        corr,
                        "forwarding module route GOODBYE to client"
                    );
                    let mut goodbye = frame;
                    goodbye.header.channel = target.channel;
                    target
                        .sink
                        .send(goodbye)
                        .await
                        .map_err(|err| RouterError::backend(channel, corr, err.to_string()))?;
                    return Ok(());
                }

                warn!(
                    connection_id = ctx.connection_id.get(),
                    channel, corr, "dropping module GOODBYE for released route channel"
                );
                return Ok(());
            }

            if let Some(route) = self
                .forwarding
                .module_route(ctx.connection_id, channel)
                .map_err(RouterError::Forwarding)?
            {
                debug!(
                    connection_id = ctx.connection_id.get(),
                    module_channel = channel,
                    client_channel = route.client_channel,
                    corr,
                    frame_type = ?frame.header.ty,
                    "forwarding module data-plane frame to client"
                );
                if is_terminal_frame(frame.header.ty) {
                    route.flow.release();
                }
                let mut frame = frame;
                frame.header.channel = route.client_channel;
                route
                    .sink
                    .send(frame)
                    .await
                    .map_err(|err| RouterError::backend(channel, corr, err.to_string()))?;
                return Ok(());
            }

            // follow-up: durable PUSH replay remains module-owned; subc only drops
            // stale route frames for released channels.
            warn!(
                connection_id = ctx.connection_id.get(),
                channel,
                corr,
                frame_type = ?frame.header.ty,
                "dropping module data-plane frame for released route channel"
            );
            return Ok(());
        }

        if frame.header.ty == FrameType::Goodbye {
            let _ = self
                .control
                .handle_route_goodbye(ctx.connection_id, channel)?;
            return Ok(());
        }

        if self
            .forwarding
            .client_route(ctx.connection_id, channel)
            .map_err(RouterError::Forwarding)?
            .is_some()
        {
            debug!(
                connection_id = ctx.connection_id.get(),
                channel,
                corr,
                frame_type = ?frame.header.ty,
                "forwarding client data-plane frame to module"
            );
            return self.forward_backend.handle(ctx.clone(), frame).await;
        }

        let Some(backend) = self.backends.get(&channel) else {
            let err = RouterError::UnknownChannel { channel, corr };
            if let Some(error_frame) = err.to_error_frame() {
                warn!(
                    connection_id = ctx.connection_id.get(),
                    channel,
                    corr,
                    error = %err,
                    "unknown channel; emitted ERROR frame"
                );
                ctx.egress.send(error_frame).await?;
                return Ok(());
            }
            return Err(err);
        };

        debug!(
            connection_id = ctx.connection_id.get(),
            channel,
            corr,
            frame_type = ?frame.header.ty,
            "routing data-plane frame"
        );
        backend.handle(ctx.clone(), frame).await
    }
}

impl Default for Router {
    fn default() -> Self {
        Self::with_default_self_handler()
    }
}

/// Connection-scoped cleanup guard returned by [`Router::begin_connection`].
#[must_use]
pub struct RouterConnection {
    id: ConnectionId,
    control_handler: Arc<ControlHandler>,
}

impl RouterConnection {
    pub fn id(&self) -> ConnectionId {
        self.id
    }
}

impl Drop for RouterConnection {
    fn drop(&mut self) {
        // GOODBYE (explicit) and connection-drop cleanup both call the same
        // idempotent deregistration path.
        let _ = self.control_handler.cleanup_connection(self.id);
    }
}

/// Minimal in-memory backend used by tests and early wiring: it emits a
/// `RESPONSE` on the same channel/correlation id with the exact same body bytes.
#[derive(Debug, Default, Clone, Copy)]
pub struct EchoBackend;

impl EchoBackend {
    pub async fn handle(&self, ctx: RouteCtx, frame: Frame) -> Result<(), RouterError> {
        let response = Frame::build_with_version(
            frame.header.ver,
            FrameType::Response,
            frame.header.flags,
            frame.header.channel,
            frame.header.corr,
            frame.body,
        )
        .map_err(RouterError::FrameBuild)?;
        ctx.egress.send(response).await
    }
}

/// Data-plane backend that splices client frames to the module connection bound at attach time.
#[derive(Debug, Clone)]
pub struct ForwardBackend {
    forwarding: Arc<ForwardingTable>,
}

impl ForwardBackend {
    pub fn new(forwarding: Arc<ForwardingTable>) -> Self {
        Self { forwarding }
    }

    pub async fn handle(&self, ctx: RouteCtx, frame: Frame) -> Result<(), RouterError> {
        let channel = frame.header.channel;
        let corr = frame.header.corr;
        let frame_type = frame.header.ty;
        let route = self
            .forwarding
            .client_route(ctx.connection_id, channel)
            .map_err(RouterError::Forwarding)?
            .ok_or(RouterError::UnknownChannel { channel, corr })?;

        // CANCEL and other non-REQUEST frames bypass the request-credit window;
        // the original request's credit is freed only by the module's terminal frame.
        let acquired_credit = frame_type == FrameType::Request;
        if acquired_credit {
            route.flow.acquire().await.map_err(|err| {
                RouterError::backend(channel, corr, format!("{err} for route channel {channel}"))
            })?;
        }

        let mut frame = frame;
        frame.header.channel = route.module_channel;
        let result = route
            .sink
            .send(frame)
            .await
            .map_err(|err| RouterError::backend(channel, corr, err.to_string()));
        if acquired_credit && result.is_err() {
            route.flow.release();
        }
        result
    }
}

fn is_terminal_frame(frame_type: FrameType) -> bool {
    matches!(
        frame_type,
        FrameType::Response | FrameType::Error | FrameType::StreamEnd
    )
}

/// Typed router errors. Routable failures can be translated to canonical JSON
/// `ERROR` frames with [`RouterError::to_error_frame`].
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
    Forwarding(ForwardingError),
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
                "unknown_channel",
                format!("unknown channel {channel}"),
            ),
            Self::Backend {
                channel,
                corr,
                message,
            } => error_frame(*channel, *corr, "backend_error", message.clone()),
            Self::ReservedChannelZero
            | Self::DuplicateChannel { .. }
            | Self::FrameBuild(_)
            | Self::Forwarding(_) => None,
        }
    }
}

fn error_frame(channel: u16, corr: u64, code: &str, message: String) -> Option<Frame> {
    let body = serde_json::to_vec(&ErrorBody {
        code: code.to_string(),
        message,
    })
    .ok()?;

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
            Self::Forwarding(err) => write!(f, "forwarding error: {err}"),
        }
    }
}

impl Error for RouterError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::FrameBuild(err) => Some(err),
            Self::Forwarding(err) => Some(err),
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
    use subc_protocol::{ErrorBody, Flags, FrameType, Priority};
    use tokio::sync::mpsc;

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

    fn ping(corr: u64) -> Frame {
        Frame::build(
            FrameType::Ping,
            Flags::new(false, Priority::Passive, false),
            0,
            corr,
            Vec::new(),
        )
        .unwrap()
    }

    fn route_ctx() -> (RouteCtx, mpsc::Receiver<Frame>) {
        let (tx, rx) = mpsc::channel(8);
        (
            RouteCtx {
                connection_id: ConnectionId::LOCAL,
                egress: FrameSink::new(tx),
            },
            rx,
        )
    }

    #[tokio::test]
    async fn echo_backend_returns_response_with_byte_identical_body() {
        let mut router = Router::with_default_self_handler();
        router.register_backend(7, EchoBackend).unwrap();
        let (ctx, mut rx) = route_ctx();
        let body = b"{not parsed}\0\xff";

        router
            .route_for_connection(&ctx, request(7, 123, body))
            .await
            .unwrap();
        let response = rx.recv().await.unwrap();

        assert_eq!(response.header.ty, FrameType::Response);
        assert_eq!(response.header.channel, 7);
        assert_eq!(response.header.corr, 123);
        assert_eq!(response.body, body);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn unknown_channel_emits_canonical_error_frame() {
        let router = Router::with_default_self_handler();
        let (ctx, mut rx) = route_ctx();

        router
            .route_for_connection(&ctx, request(99, 5, b"payload"))
            .await
            .unwrap();
        let error_frame = rx.recv().await.unwrap();

        assert_eq!(error_frame.header.ty, FrameType::Error);
        assert_eq!(error_frame.header.channel, 99);
        assert_eq!(error_frame.header.corr, 5);
        let body: ErrorBody = serde_json::from_slice(&error_frame.body).unwrap();
        assert_eq!(body.code, "unknown_channel");
        assert_eq!(body.message, "unknown channel 99");
    }

    #[tokio::test]
    async fn channel_zero_uses_control_handler_not_backend_registry() {
        let mut router = Router::with_default_self_handler();
        router.register_backend(1, EchoBackend).unwrap();
        let (ctx, mut rx) = route_ctx();

        router.route_for_connection(&ctx, ping(77)).await.unwrap();
        let response = rx.recv().await.unwrap();

        assert_eq!(response.header.ty, FrameType::Pong);
        assert_eq!(response.header.channel, 0);
        assert_eq!(response.header.corr, 77);
        assert!(response.body.is_empty());
    }

    #[test]
    fn channel_zero_cannot_be_registered_as_backend() {
        let mut router = Router::with_default_self_handler();

        let err = router.register_backend(0, EchoBackend).unwrap_err();

        assert_eq!(err, RouterError::ReservedChannelZero);
    }
}
