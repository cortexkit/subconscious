use std::{
    collections::HashMap,
    error::Error,
    fmt,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Instant,
};

use subc_protocol::{ErrorBody, Flags, FrameType, Priority};
use tokio::sync::mpsc;
use tracing::debug;

use crate::{
    control::ControlHandler,
    forwarding::{
        CloseReason, ConnectionCloseReceiver, DataRoute, DataRouteState, ForwardingError,
        ForwardingTable, RouteBinding, RouteRelease,
    },
    registry::ConnectionId,
    DaemonCounters, Frame, FrameBuildError,
};

/// One queued outbound frame plus the instant it entered the writer queue.
///
/// The stamp exists for the reply-path half of slow-control diagnosis: a
/// handler can finish in microseconds while the reply sits in this queue
/// waiting for the writer task to be scheduled, and without a per-item stamp
/// that wait is invisible to every other timing point (the client's round
/// trip is the only witness, and it cannot say which side ate the time).
/// Constructed exclusively inside [`FrameSink`] so no caller can forget it.
#[derive(Debug)]
pub struct OutboundFrame {
    pub frame: Frame,
    pub enqueued_at: std::time::Instant,
}

impl OutboundFrame {
    fn now(frame: Frame) -> Self {
        Self {
            frame,
            enqueued_at: std::time::Instant::now(),
        }
    }
}

/// An `OutboundFrame` is a stamped `Frame`; deref keeps every existing frame
/// read (headers, bodies, assertions) working on queued items unchanged.
impl std::ops::Deref for OutboundFrame {
    type Target = Frame;

    fn deref(&self) -> &Frame {
        &self.frame
    }
}

/// Shared tracing-capture helpers for timing-observability tests across
/// modules (router dispatch, server reply path). Test-only.
#[cfg(test)]
pub(crate) mod test_log {
    use std::{
        io::Write,
        sync::{Arc, Mutex},
    };

    #[derive(Clone)]
    struct TestLogWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for TestLogWriter {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .expect("test log capture is not poisoned")
                .extend(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    pub(crate) fn log_capture(
        level: tracing::Level,
    ) -> (Arc<Mutex<Vec<u8>>>, tracing::dispatcher::DefaultGuard) {
        let output = Arc::new(Mutex::new(Vec::new()));
        let writer = Arc::clone(&output);
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(level)
            .with_ansi(false)
            .without_time()
            .with_target(false)
            .with_writer(move || TestLogWriter(Arc::clone(&writer)))
            .finish();
        let guard = tracing::subscriber::set_default(subscriber);
        (output, guard)
    }

    pub(crate) fn captured_logs(output: &Arc<Mutex<Vec<u8>>>) -> String {
        String::from_utf8(
            output
                .lock()
                .expect("test log capture is not poisoned")
                .clone(),
        )
        .expect("tracing output is UTF-8")
    }
}

/// Cheaply cloneable handle to one connection's bounded outbound frame queue.
///
/// Backends emit responses, streaming frames, and future PUSH frames through this
/// single path. The bounded `mpsc` sender is the connection-level backpressure
/// substrate; the socket layer owns the sole receiver/writer.
#[derive(Debug, Clone)]
pub struct FrameSink {
    tx: mpsc::Sender<OutboundFrame>,
}

impl FrameSink {
    pub fn new(tx: mpsc::Sender<OutboundFrame>) -> Self {
        Self { tx }
    }

    pub async fn send(&self, frame: Frame) -> Result<(), RouterError> {
        let channel = frame.header.channel;
        let epoch = frame.header.epoch;
        let corr = frame.header.corr;
        self.tx.send(OutboundFrame::now(frame)).await.map_err(|_| {
            RouterError::backend_with_epoch(channel, epoch, corr, "connection writer closed")
        })
    }

    pub(crate) async fn reserve_owned(
        &self,
    ) -> Result<mpsc::OwnedPermit<OutboundFrame>, RouterError> {
        self.tx
            .clone()
            .reserve_owned()
            .await
            .map_err(|_| RouterError::backend(0, 0, "connection writer closed"))
    }

    #[cfg(test)]
    pub(crate) fn try_reserve_owned(
        &self,
    ) -> Result<mpsc::OwnedPermit<OutboundFrame>, RouterError> {
        self.tx
            .clone()
            .try_reserve_owned()
            .map_err(|err| RouterError::backend(0, 0, err.to_string()))
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }

    pub(crate) fn try_send(&self, frame: Frame) -> Result<(), RouterError> {
        let channel = frame.header.channel;
        let epoch = frame.header.epoch;
        let corr = frame.header.corr;
        self.tx.try_send(OutboundFrame::now(frame)).map_err(|err| {
            RouterError::backend_with_epoch(
                channel,
                epoch,
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
///
/// DATA-PLANE BODIES ARE NEVER DECODED HERE, and the consequence is worth stating
/// because it looks like a guarantee and is not. Additive fields in a request or
/// response body reach the far side untouched — not because anything permits them,
/// but because routing reads only the 21-byte header and treats the body as opaque
/// bytes. That is a performance property, so **nothing prevents it from changing**:
/// a future reason to inspect a body would convert a wire-transparent path into a
/// filtering one, and nobody would think of it as a contract change.
///
/// So when a peer asks whether subc sees an additive field, the answer is per-path
/// and this path's zero means WE NEVER LOOK rather than WE LOOK AT EVERYTHING. The
/// control plane (typed enums at the frame boundary) and the MCP gateway (envelope
/// unwrap plus a named struct) both narrow; only this one does not.
pub struct Router {
    backends: HashMap<u16, Backend>,
    control: Arc<ControlHandler>,
    forwarding: Arc<ForwardingTable>,
    forward_backend: ForwardBackend,
    counters: DaemonCounters,
    next_connection_id: AtomicU64,
}

impl Router {
    pub fn with_control_handler(control: Arc<ControlHandler>) -> Self {
        let forwarding = control.forwarding();
        let counters = control.counters();
        Self {
            backends: HashMap::new(),
            control,
            forwarding: Arc::clone(&forwarding),
            forward_backend: ForwardBackend::new(forwarding),
            counters,
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
    fn record_module_frame_drop(&self, connection_id: ConnectionId) -> Result<(), RouterError> {
        let module_id = self
            .forwarding
            .module_id_for_connection(connection_id)
            .map_err(RouterError::Forwarding)?;
        self.counters
            .increment_module_frames_dropped_no_route(module_id.as_deref());
        Ok(())
    }

    pub fn begin_connection(&self) -> RouterConnection {
        let raw = self.next_connection_id.fetch_add(1, Ordering::Relaxed);
        let id = ConnectionId::new(raw);
        let close_receiver = self.forwarding.register_connection_close(id);
        RouterConnection {
            id,
            control_handler: Arc::clone(&self.control),
            forwarding: Arc::clone(&self.forwarding),
            close_receiver: Some(close_receiver),
        }
    }

    pub async fn route_for_connection(
        &self,
        ctx: &RouteCtx,
        frame: Frame,
    ) -> Result<(), RouterError> {
        let channel = frame.header.channel;
        let epoch = frame.header.epoch;
        let corr = frame.header.corr;
        if channel == 0 {
            debug!(
                connection_id = ctx.connection_id.get(),
                corr,
                frame_type = ?frame.header.ty,
                "routing control frame"
            );
            // The connection loop calls this handler directly after reading the frame;
            // there is no channel send, semaphore, or spawn await between receipt and
            // dispatch, so this timing has no queue segment by construction.
            let dispatch_started_at = (frame.header.ty == FrameType::Request).then(Instant::now);
            let responses = self
                .control
                .handle_control_frame_timed(ctx, frame, dispatch_started_at)
                .await?;
            for response in responses {
                ctx.egress.send(response).await?;
            }
            return Ok(());
        }

        let data_route = self
            .forwarding
            .lookup_data_route(ctx.connection_id, channel, epoch)
            .map_err(RouterError::Forwarding)?;

        match data_route {
            DataRoute::Module(DataRouteState::EpochMismatch) => {
                if frame.header.ty == FrameType::Request {
                    self.counters
                        .increment_module_requests_dropped_stale_route();
                    let err = RouterError::StaleRouteEpoch {
                        channel,
                        epoch,
                        corr,
                    };
                    if let Some(error_frame) = err.to_error_frame() {
                        ctx.egress.send(error_frame).await?;
                    }
                } else {
                    self.record_module_frame_drop(ctx.connection_id)?;
                }
                debug!(
                    connection_id = ctx.connection_id.get(),
                    channel, epoch, corr, "dropping module frame for stale route epoch"
                );
                return Ok(());
            }
            DataRoute::Module(DataRouteState::Reserved) => {
                if frame.header.ty == FrameType::Request {
                    self.counters
                        .increment_module_requests_dropped_stale_route();
                    let err = RouterError::UnknownChannel {
                        channel,
                        epoch,
                        corr,
                    };
                    if let Some(error_frame) = err.to_error_frame() {
                        ctx.egress.send(error_frame).await?;
                    }
                } else {
                    self.record_module_frame_drop(ctx.connection_id)?;
                }
                debug!(
                    connection_id = ctx.connection_id.get(),
                    channel, epoch, corr, "dropping module frame for reserved route handle"
                );
                return Ok(());
            }
            DataRoute::Module(DataRouteState::Absent) => {
                if frame.header.ty == FrameType::Request {
                    self.counters
                        .increment_module_requests_dropped_stale_route();
                    let err = RouterError::UnknownChannel {
                        channel,
                        epoch,
                        corr,
                    };
                    if let Some(error_frame) = err.to_error_frame() {
                        ctx.egress.send(error_frame).await?;
                    }
                } else {
                    self.record_module_frame_drop(ctx.connection_id)?;
                }
                debug!(
                    connection_id = ctx.connection_id.get(),
                    channel, epoch, corr, "dropping module frame for absent route handle"
                );
                return Ok(());
            }
            DataRoute::Module(DataRouteState::Bound(route)) => {
                if frame.header.ty == FrameType::Goodbye {
                    if let RouteRelease::Removed(target) = self
                        .forwarding
                        .release_module_route(ctx.connection_id, channel, epoch)
                        .map_err(RouterError::Forwarding)?
                    {
                        let mut goodbye = frame;
                        goodbye.header.channel = target.channel;
                        goodbye.header.epoch = target.epoch;
                        if let Err(err) = target.sink.try_send(goodbye) {
                            if target.close_on_delivery_failure()
                                && self
                                    .forwarding
                                    .escalate_client_delivery_failure(
                                        target.connection_id,
                                        target.channel,
                                        target.epoch,
                                        CloseReason::new(
                                            "route_goodbye_delivery_failed",
                                            format!(
                                                "failed to enqueue route GOODBYE for client channel {}: {err}",
                                                target.channel
                                            ),
                                        ),
                                    )
                                    .map_err(RouterError::Forwarding)?
                            {
                                self.counters.increment_goodbye_relay_client_failed();
                            }
                        }
                    }
                    return Ok(());
                }

                let releases_credit = is_terminal_frame(frame.header.ty);
                let mut frame = frame;
                frame.header.channel = route.client_channel;
                frame.header.epoch = route.client_epoch;
                if let Err(err) = route.client_sink.try_send(frame) {
                    if self
                        .forwarding
                        .escalate_client_delivery_failure(
                            route.client_connection_id,
                            route.client_channel,
                            route.client_epoch,
                            CloseReason::new(
                                "module_to_client_delivery_failed",
                                format!(
                                    "failed to enqueue module frame for client channel {} corr {corr}: {err}",
                                    route.client_channel
                                ),
                            ),
                        )
                        .map_err(RouterError::Forwarding)?
                    {
                        self.counters
                            .increment_client_egress_close_delivery_failed();
                    }
                    return Ok(());
                }
                if releases_credit {
                    route.flow.release();
                }
                return Ok(());
            }
            DataRoute::Client(DataRouteState::EpochMismatch) => {
                if frame.header.ty == FrameType::Request {
                    self.counters.increment_client_frames_dropped_stale_route();
                    // Dropped before forwarding; a re-bind retry cannot double-execute this request.
                    let err = RouterError::StaleRouteEpoch {
                        channel,
                        epoch,
                        corr,
                    };
                    if let Some(error_frame) = err.to_error_frame() {
                        ctx.egress.send(error_frame).await?;
                    }
                }
                debug!(
                    connection_id = ctx.connection_id.get(),
                    channel, epoch, corr, "dropping client frame for stale route epoch"
                );
                return Ok(());
            }
            DataRoute::Client(DataRouteState::Reserved) => {
                if frame.header.ty == FrameType::Request {
                    let err = RouterError::UnknownChannel {
                        channel,
                        epoch,
                        corr,
                    };
                    if let Some(error_frame) = err.to_error_frame() {
                        ctx.egress.send(error_frame).await?;
                    }
                }
                return Ok(());
            }
            DataRoute::Client(DataRouteState::Bound(route)) => {
                if frame.header.ty == FrameType::Goodbye {
                    let _ = self
                        .control
                        .handle_route_goodbye(ctx.connection_id, channel, epoch)?;
                    return Ok(());
                }
                return self.forward_backend.handle_bound(frame, route).await;
            }
            DataRoute::Client(DataRouteState::Absent) => {}
        }

        if let Some(backend) = self.backends.get(&channel) {
            return backend.handle(ctx.clone(), frame).await;
        }
        if frame.header.ty == FrameType::Request {
            let err = RouterError::UnknownChannel {
                channel,
                epoch,
                corr,
            };
            if let Some(error_frame) = err.to_error_frame() {
                ctx.egress.send(error_frame).await?;
            }
        }
        Ok(())
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
    forwarding: Arc<ForwardingTable>,
    close_receiver: Option<ConnectionCloseReceiver>,
}

impl RouterConnection {
    pub fn id(&self) -> ConnectionId {
        self.id
    }

    pub(crate) fn take_close_receiver(&mut self) -> ConnectionCloseReceiver {
        self.close_receiver
            .take()
            .expect("connection close receiver can only be taken once")
    }
}

impl Drop for RouterConnection {
    fn drop(&mut self) {
        self.forwarding.unregister_connection_close(self.id);
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
            frame.header.epoch,
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
        let route = match self
            .forwarding
            .lookup_data_route(ctx.connection_id, channel, frame.header.epoch)
            .map_err(RouterError::Forwarding)?
        {
            DataRoute::Client(DataRouteState::Bound(route)) => route,
            DataRoute::Client(_) | DataRoute::Module(_) => {
                return Err(RouterError::UnknownChannel {
                    channel,
                    epoch: frame.header.epoch,
                    corr,
                });
            }
        };
        self.handle_bound(frame, route).await
    }

    pub(crate) async fn handle_bound(
        &self,
        frame: Frame,
        route: Arc<RouteBinding>,
    ) -> Result<(), RouterError> {
        let channel = frame.header.channel;
        let corr = frame.header.corr;
        let frame_type = frame.header.ty;

        // CANCEL and other non-REQUEST frames bypass the request-credit window;
        // the original request's credit is freed only by the module's terminal frame.
        let acquired_credit = frame_type == FrameType::Request;
        if acquired_credit {
            if let Err(err) = route.flow.acquire().await {
                if self
                    .forwarding
                    .endpoint_is_draining(route.module_endpoint)
                    .map_err(RouterError::Forwarding)?
                {
                    return Err(RouterError::route_error_with_epoch(
                        channel,
                        frame.header.epoch,
                        corr,
                        "module_reloading",
                        format!("module endpoint for route channel {channel} is reloading"),
                    ));
                }
                return Err(RouterError::backend_with_epoch(
                    channel,
                    frame.header.epoch,
                    corr,
                    format!("{err} for route channel {channel}"),
                ));
            }
        }

        let mut frame = frame;
        frame.header.channel = route.module_channel;
        frame.header.epoch = route.module_epoch;
        let result = route.module_sink.send(frame).await.map_err(|err| {
            RouterError::backend_with_epoch(channel, route.client_epoch, corr, err.to_string())
        });
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
        epoch: u32,
        corr: u64,
    },
    StaleRouteEpoch {
        channel: u16,
        epoch: u32,
        corr: u64,
    },
    Backend {
        channel: u16,
        epoch: u32,
        corr: u64,
        message: String,
    },
    RouteError {
        channel: u16,
        epoch: u32,
        corr: u64,
        code: String,
        message: String,
    },
    FrameBuild(FrameBuildError),
    Forwarding(ForwardingError),
}

impl RouterError {
    pub fn backend(channel: u16, corr: u64, message: impl Into<String>) -> Self {
        Self::backend_with_epoch(channel, 0, corr, message)
    }

    pub fn backend_with_epoch(
        channel: u16,
        epoch: u32,
        corr: u64,
        message: impl Into<String>,
    ) -> Self {
        Self::Backend {
            channel,
            epoch,
            corr,
            message: message.into(),
        }
    }

    pub fn route_error(
        channel: u16,
        corr: u64,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self::route_error_with_epoch(channel, 0, corr, code, message)
    }

    pub fn route_error_with_epoch(
        channel: u16,
        epoch: u32,
        corr: u64,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self::RouteError {
            channel,
            epoch,
            corr,
            code: code.into(),
            message: message.into(),
        }
    }

    /// Translate route failures that belong on the wire into an `ERROR` frame.
    pub fn to_error_frame(&self) -> Option<Frame> {
        match self {
            Self::UnknownChannel {
                channel,
                epoch,
                corr,
            } => error_frame(
                *channel,
                *epoch,
                *corr,
                "unknown_channel",
                format!("unknown channel {channel}"),
            ),
            Self::StaleRouteEpoch {
                channel,
                epoch,
                corr,
            } => error_frame(
                *channel,
                *epoch,
                *corr,
                "stale_route_epoch",
                format!("stale route epoch for channel {channel}"),
            ),
            Self::Backend {
                channel,
                epoch,
                corr,
                message,
            } => error_frame(*channel, *epoch, *corr, "backend_error", message.clone()),
            Self::RouteError {
                channel,
                epoch,
                corr,
                code,
                message,
            } => error_frame(*channel, *epoch, *corr, code, message.clone()),
            Self::ReservedChannelZero
            | Self::DuplicateChannel { .. }
            | Self::FrameBuild(_)
            | Self::Forwarding(_) => None,
        }
    }
}

fn error_frame(channel: u16, epoch: u32, corr: u64, code: &str, message: String) -> Option<Frame> {
    let body = serde_json::to_vec(&ErrorBody {
        code: code.to_string(),
        message,
        detail: None,
    })
    .ok()?;

    Frame::build(
        FrameType::Error,
        Flags::new(false, Priority::Passive, false),
        channel,
        epoch,
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
            Self::UnknownChannel { channel, corr, .. } => {
                write!(f, "unknown channel {channel} for corr {corr}")
            }
            Self::StaleRouteEpoch { channel, corr, .. } => {
                write!(f, "stale route epoch for channel {channel} corr {corr}")
            }
            Self::Backend {
                channel,
                corr,
                message,
                ..
            } => write!(
                f,
                "backend error on channel {channel} corr {corr}: {message}"
            ),
            Self::RouteError {
                channel,
                corr,
                code,
                message,
                ..
            } => write!(
                f,
                "route error {code} on channel {channel} corr {corr}: {message}"
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
            | Self::StaleRouteEpoch { .. }
            | Self::Backend { .. }
            | Self::RouteError { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        forwarding::RouteBindRelayOutcome,
        supervise::{ModuleSpec, RestartPolicy, Supervisor, SupervisorHandle},
        ControlHandler, Registry,
    };
    use std::{
        sync::{mpsc as std_mpsc, Arc},
        time::Duration,
    };
    use subc_protocol::{manifest::Concurrency, ErrorBody, Flags, FrameType, Priority};
    use tokio::sync::mpsc;

    pub(crate) use crate::router::test_log::{captured_logs, log_capture};

    fn logged_millis(logs: &str, field: &str) -> u64 {
        logs.split_whitespace()
            .find_map(|part| part.strip_prefix(field))
            .and_then(|value| value.parse().ok())
            .unwrap_or_else(|| panic!("missing numeric {field} in logs: {logs}"))
    }

    fn request(channel: u16, corr: u64, body: &[u8]) -> Frame {
        Frame::build(
            FrameType::Request,
            Flags::new(true, Priority::Interactive, false),
            channel,
            0,
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
            0,
            corr,
            Vec::new(),
        )
        .unwrap()
    }

    fn route_ctx() -> (RouteCtx, mpsc::Receiver<crate::router::OutboundFrame>) {
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

    #[tokio::test]
    async fn slow_control_dispatch_logs_decoded_op_and_elapsed_time() {
        let control = Arc::new(
            ControlHandler::new(Arc::new(Registry::default()))
                .with_control_dispatch_delay(Duration::from_millis(1050)),
        );
        let router = Router::with_control_handler(control);
        let (ctx, mut rx) = route_ctx();
        let (output, guard) = log_capture(tracing::Level::WARN);

        router
            .route_for_connection(&ctx, request(0, 41, br#"{"op":"server.describe"}"#))
            .await
            .expect("slow request routes");
        assert!(rx.recv().await.is_some(), "request receives a response");
        drop(guard);

        let logs = captured_logs(&output);
        assert!(logs.contains("slow control dispatch"));
        assert!(logs.contains("op=server.describe"));
        assert!(logs.contains("connection_id=0"));
        assert!(logs.contains("corr=41"));
        assert!(
            logged_millis(&logs, "elapsed_ms=") >= 1050,
            "elapsed must include the injected handler delay: {logs}"
        );
    }

    #[tokio::test]
    async fn fast_control_dispatch_emits_arrival_without_slow_warning() {
        let router = Router::with_default_self_handler();
        let (ctx, mut rx) = route_ctx();
        let (output, guard) = log_capture(tracing::Level::DEBUG);

        router
            .route_for_connection(&ctx, request(0, 42, br#"{"op":"server.describe"}"#))
            .await
            .expect("fast request routes");
        assert!(rx.recv().await.is_some(), "request receives a response");
        drop(guard);

        let logs = captured_logs(&output);
        assert!(logs.contains("control dispatch op=server.describe connection_id=0 corr=42"));
        assert!(!logs.contains("slow control dispatch"));
    }

    #[tokio::test]
    async fn control_dispatch_arrival_is_hidden_at_info() {
        let router = Router::with_default_self_handler();
        let (ctx, mut rx) = route_ctx();
        let (output, guard) = log_capture(tracing::Level::INFO);

        router
            .route_for_connection(&ctx, request(0, 43, br#"{"op":"server.describe"}"#))
            .await
            .expect("fast request routes");
        assert!(rx.recv().await.is_some(), "request receives a response");
        drop(guard);

        assert!(
            !captured_logs(&output).contains("control dispatch"),
            "arrival logging must stay hidden at INFO"
        );
    }

    #[tokio::test]
    async fn supervisor_list_logs_contended_snapshot_lock_only() {
        let registry = Arc::new(Registry::default());
        let handle = SupervisorHandle::new();
        let supervisor = Supervisor::new(Arc::clone(&registry), RestartPolicy::default())
            .with_handle(handle.clone());
        let module = supervisor
            .supervise_configured(
                ModuleSpec {
                    module_id: "held-module".to_string(),
                    program: "test-module".into(),
                    args: Vec::new(),
                    env: Vec::new(),
                    reserved: false,
                    reserved_prefixes: Vec::new(),
                },
                false,
            )
            .expect("disabled test module is supervised");
        let router = Router::with_control_handler(Arc::new(
            ControlHandler::new(Arc::clone(&registry)).with_supervisor(handle),
        ));
        let (ctx, mut rx) = route_ctx();
        let (acquired, ready) = std_mpsc::channel();
        let holder = module.hold_snapshot_for_test(acquired, Duration::from_millis(400));
        ready.recv().expect("holder acquired snapshot lock");
        let (output, guard) = log_capture(tracing::Level::WARN);

        router
            .route_for_connection(&ctx, request(0, 44, br#"{"op":"supervisor.list"}"#))
            .await
            .expect("list request routes after the lock releases");
        assert!(
            rx.recv().await.is_some(),
            "list request receives a response"
        );
        holder.join().expect("snapshot holder exits cleanly");
        drop(guard);

        let logs = captured_logs(&output);
        assert!(logs.contains("slow snapshot lock"));
        assert!(logs.contains("module_id=held-module"));
        assert!(logs.contains("caller=list"));
        assert!(
            logged_millis(&logs, "waited_ms=") >= 250,
            "wait must exceed the slow-lock threshold: {logs}"
        );

        let (output, guard) = log_capture(tracing::Level::WARN);
        router
            .route_for_connection(&ctx, request(0, 45, br#"{"op":"supervisor.list"}"#))
            .await
            .expect("uncontended list request routes");
        assert!(
            rx.recv().await.is_some(),
            "uncontended list receives a response"
        );
        drop(guard);
        assert!(
            !captured_logs(&output).contains("slow snapshot lock"),
            "uncontended list acquisition must not warn"
        );
    }

    #[tokio::test]
    async fn full_module_to_client_sink_requests_client_close_without_erroring_module() {
        let forwarding = Arc::new(ForwardingTable::default());
        let control = Arc::new(ControlHandler::with_forwarding(
            Arc::new(crate::Registry::default()),
            Arc::clone(&forwarding),
        ));
        let router = Router::with_control_handler(control);
        let module_connection = ConnectionId::new(10);
        let client_connection = ConnectionId::new(20);
        let mut close_receiver = forwarding.register_connection_close(client_connection);
        let (module_tx, _module_rx) = mpsc::channel(1);
        forwarding
            .register_module_connection(
                module_connection,
                "full-sink-provider".to_string(),
                1,
                Concurrency::ModuleManaged,
                FrameSink::new(module_tx),
            )
            .unwrap();
        let (client_tx, mut client_rx) = mpsc::channel(1);
        let pending = forwarding
            .begin_route_bind_relay_for_test(
                client_connection,
                FrameSink::new(client_tx),
                700,
                "full-sink-provider",
            )
            .unwrap();
        forwarding
            .complete_pending_relay(
                module_connection,
                pending.corr,
                RouteBindRelayOutcome::Accepted,
            )
            .unwrap();

        let (module_egress_tx, _module_egress_rx) = mpsc::channel(1);
        let module_ctx = RouteCtx {
            connection_id: module_connection,
            egress: FrameSink::new(module_egress_tx),
        };
        let terminal = Frame::build(
            FrameType::Response,
            Flags::new(false, Priority::Interactive, true),
            pending.module_channel,
            pending.module_epoch,
            701,
            b"terminal".to_vec(),
        )
        .unwrap();

        router
            .route_for_connection(&module_ctx, terminal)
            .await
            .unwrap();
        let reason = tokio::time::timeout(Duration::from_secs(1), &mut close_receiver)
            .await
            .expect("close request should be sent for the full client sink")
            .expect("close sender should include a reason");
        assert!(
            reason
                .to_string()
                .contains("module_to_client_delivery_failed"),
            "unexpected close reason: {reason}"
        );
        assert_eq!(client_rx.try_recv().unwrap().header.corr, 700);
        assert!(client_rx.try_recv().is_err());
        assert_eq!(
            router.counters.snapshot()["client_egress_close_delivery_failed"],
            1
        );
    }

    #[tokio::test]
    async fn full_route_goodbye_sink_requests_target_close_without_erroring_module() {
        let forwarding = Arc::new(ForwardingTable::default());
        let control = Arc::new(ControlHandler::with_forwarding(
            Arc::new(crate::Registry::default()),
            Arc::clone(&forwarding),
        ));
        let router = Router::with_control_handler(control);
        let module_connection = ConnectionId::new(30);
        let client_connection = ConnectionId::new(40);
        let mut close_receiver = forwarding.register_connection_close(client_connection);
        let (module_tx, _module_rx) = mpsc::channel(1);
        forwarding
            .register_module_connection(
                module_connection,
                "goodbye-full-provider".to_string(),
                1,
                Concurrency::ModuleManaged,
                FrameSink::new(module_tx),
            )
            .unwrap();
        let (client_tx, mut client_rx) = mpsc::channel(1);
        let pending = forwarding
            .begin_route_bind_relay_for_test(
                client_connection,
                FrameSink::new(client_tx),
                800,
                "goodbye-full-provider",
            )
            .unwrap();
        forwarding
            .complete_pending_relay(
                module_connection,
                pending.corr,
                RouteBindRelayOutcome::Accepted,
            )
            .unwrap();

        let (module_egress_tx, _module_egress_rx) = mpsc::channel(1);
        let module_ctx = RouteCtx {
            connection_id: module_connection,
            egress: FrameSink::new(module_egress_tx),
        };
        let goodbye = Frame::build(
            FrameType::Goodbye,
            Flags::new(false, Priority::Passive, true),
            pending.module_channel,
            pending.module_epoch,
            801,
            Vec::new(),
        )
        .unwrap();

        router
            .route_for_connection(&module_ctx, goodbye)
            .await
            .unwrap();
        let reason = tokio::time::timeout(Duration::from_secs(1), &mut close_receiver)
            .await
            .expect("close request should be sent for the full GOODBYE sink")
            .expect("close sender should include a reason");
        assert!(
            reason.to_string().contains("route_goodbye_delivery_failed"),
            "unexpected close reason: {reason}"
        );
        assert_eq!(client_rx.try_recv().unwrap().header.corr, 800);
        assert!(client_rx.try_recv().is_err());
        assert_eq!(router.counters.snapshot()["goodbye_relay_client_failed"], 1);
        assert_eq!(router.counters.snapshot()["route_released_epoch_fenced"], 1);
    }

    fn route_frame(ty: FrameType, channel: u16, epoch: u32, corr: u64) -> Frame {
        Frame::build(
            ty,
            Flags::new(false, Priority::Interactive, false),
            channel,
            epoch,
            corr,
            if ty == FrameType::Request || ty == FrameType::Response {
                b"route-body".to_vec()
            } else {
                Vec::new()
            },
        )
        .unwrap()
    }

    type DynamicRouteFixture = (
        Router,
        Arc<ForwardingTable>,
        RouteCtx,
        mpsc::Receiver<crate::router::OutboundFrame>,
        RouteCtx,
        mpsc::Receiver<crate::router::OutboundFrame>,
        mpsc::Receiver<crate::router::OutboundFrame>,
        crate::forwarding::PendingRouteBindRelay,
    );

    fn dynamic_route_fixture(commit: bool) -> DynamicRouteFixture {
        let forwarding = Arc::new(ForwardingTable::default());
        let control = Arc::new(crate::ControlHandler::with_forwarding(
            Arc::new(crate::Registry::default()),
            Arc::clone(&forwarding),
        ));
        let router = Router::with_control_handler(control);
        let module_connection = ConnectionId::new(500);
        let client_connection = ConnectionId::new(501);
        let (module_tx, module_rx) = mpsc::channel(8);
        forwarding
            .register_module_connection(
                module_connection,
                "epoch-router".into(),
                2,
                Concurrency::ModuleManaged,
                FrameSink::new(module_tx),
            )
            .unwrap();
        let (client_tx, client_rx) = mpsc::channel(8);
        let client_sink = FrameSink::new(client_tx);
        let pending = forwarding
            .begin_route_bind_relay_for_test(
                client_connection,
                client_sink.clone(),
                700,
                "epoch-router",
            )
            .unwrap();
        if commit {
            forwarding
                .complete_pending_relay(
                    module_connection,
                    pending.corr,
                    RouteBindRelayOutcome::Accepted,
                )
                .unwrap();
        }
        let (module_egress_tx, module_egress_rx) = mpsc::channel(8);
        (
            router,
            forwarding,
            RouteCtx {
                connection_id: client_connection,
                egress: client_sink,
            },
            client_rx,
            RouteCtx {
                connection_id: module_connection,
                egress: FrameSink::new(module_egress_tx),
            },
            module_egress_rx,
            module_rx,
            pending,
        )
    }

    #[tokio::test]
    async fn route_epochs_validate_both_directions_and_rewrite_to_peer_handle() {
        let (
            router,
            _forwarding,
            client_ctx,
            mut client_rx,
            module_ctx,
            _module_egress_rx,
            mut module_rx,
            pending,
        ) = dynamic_route_fixture(true);
        let route_open = client_rx.recv().await.unwrap();
        assert_eq!(route_open.header.corr, 700);

        router
            .route_for_connection(
                &client_ctx,
                route_frame(
                    FrameType::Request,
                    pending.client_channel,
                    pending.client_epoch,
                    701,
                ),
            )
            .await
            .unwrap();
        let forwarded = module_rx.recv().await.unwrap();
        assert_eq!(forwarded.header.channel, pending.module_channel);
        assert_eq!(forwarded.header.epoch, pending.module_epoch);

        router
            .route_for_connection(
                &module_ctx,
                route_frame(
                    FrameType::Response,
                    pending.module_channel,
                    pending.module_epoch,
                    701,
                ),
            )
            .await
            .unwrap();
        let delivered = client_rx.recv().await.unwrap();
        assert_eq!(delivered.header.channel, pending.client_channel);
        assert_eq!(delivered.header.epoch, pending.client_epoch);

        router
            .route_for_connection(
                &client_ctx,
                route_frame(
                    FrameType::Request,
                    pending.client_channel,
                    pending.client_epoch + 1,
                    702,
                ),
            )
            .await
            .unwrap();
        router
            .route_for_connection(
                &module_ctx,
                route_frame(
                    FrameType::Response,
                    pending.module_channel,
                    pending.module_epoch + 1,
                    703,
                ),
            )
            .await
            .unwrap();
        let stale_error = client_rx.recv().await.unwrap();
        assert_eq!(stale_error.header.ty, FrameType::Error);
        assert_eq!(stale_error.header.channel, pending.client_channel);
        assert_eq!(stale_error.header.epoch, pending.client_epoch + 1);
        assert_eq!(stale_error.header.corr, 702);
        let body: ErrorBody = serde_json::from_slice(&stale_error.body).unwrap();
        assert_eq!(body.code, "stale_route_epoch");
        assert!(module_rx.try_recv().is_err());
        assert!(client_rx.try_recv().is_err());
        let counters = router.counters.snapshot();
        assert_eq!(counters["client_frames_dropped_stale_route"], 1);
        assert_eq!(counters["module_frames_dropped_no_route"], 1);
    }

    #[tokio::test]
    async fn accepted_route_publishes_route_open_before_immediate_reverse_request() {
        let (
            router,
            _,
            _client_ctx,
            mut client_rx,
            module_ctx,
            _module_egress_rx,
            _module_rx,
            pending,
        ) = dynamic_route_fixture(true);
        router
            .route_for_connection(
                &module_ctx,
                route_frame(
                    FrameType::Request,
                    pending.module_channel,
                    pending.module_epoch,
                    800,
                ),
            )
            .await
            .unwrap();

        let first = client_rx.recv().await.unwrap();
        let second = client_rx.recv().await.unwrap();
        assert_eq!(first.header.channel, 0);
        assert_eq!(first.header.corr, 700);
        assert_eq!(second.header.channel, pending.client_channel);
        assert_eq!(second.header.epoch, pending.client_epoch);
        assert_eq!(second.header.corr, 800);
    }

    #[tokio::test]
    async fn reserved_slot_ingress_errors_only_matching_client_requests() {
        let (
            router,
            _forwarding,
            client_ctx,
            mut client_rx,
            _module_ctx,
            _module_egress_rx,
            mut module_rx,
            pending,
        ) = dynamic_route_fixture(false);
        router
            .route_for_connection(
                &client_ctx,
                route_frame(
                    FrameType::Request,
                    pending.client_channel,
                    pending.client_epoch,
                    900,
                ),
            )
            .await
            .unwrap();
        let error = client_rx.recv().await.unwrap();
        assert_eq!(error.header.ty, FrameType::Error);
        assert_eq!(error.header.channel, pending.client_channel);
        assert_eq!(error.header.epoch, pending.client_epoch);
        assert_eq!(error.header.corr, 900);

        router
            .route_for_connection(
                &client_ctx,
                route_frame(
                    FrameType::Response,
                    pending.client_channel,
                    pending.client_epoch,
                    901,
                ),
            )
            .await
            .unwrap();
        router
            .route_for_connection(
                &client_ctx,
                route_frame(
                    FrameType::Request,
                    pending.client_channel,
                    pending.client_epoch + 1,
                    902,
                ),
            )
            .await
            .unwrap();
        let stale_error = client_rx.recv().await.unwrap();
        assert_eq!(stale_error.header.ty, FrameType::Error);
        assert_eq!(stale_error.header.channel, pending.client_channel);
        assert_eq!(stale_error.header.epoch, pending.client_epoch + 1);
        assert_eq!(stale_error.header.corr, 902);
        let body: ErrorBody = serde_json::from_slice(&stale_error.body).unwrap();
        assert_eq!(body.code, "stale_route_epoch");
        assert!(module_rx.try_recv().is_err());
        let counters = router.counters.snapshot();
        assert_eq!(counters["client_frames_dropped_stale_route"], 1);
        assert_eq!(counters["module_frames_dropped_no_route"], 0);
    }

    #[tokio::test]
    async fn dropped_module_route_goodbye_increments_counter() {
        let (
            router,
            _forwarding,
            client_ctx,
            mut client_rx,
            _module_ctx,
            _module_egress_rx,
            mut module_rx,
            pending,
        ) = dynamic_route_fixture(true);
        let _ = client_rx.recv().await;
        module_rx.close();

        router
            .route_for_connection(
                &client_ctx,
                route_frame(
                    FrameType::Goodbye,
                    pending.client_channel,
                    pending.client_epoch,
                    999,
                ),
            )
            .await
            .unwrap();

        let counters = router.counters.snapshot();
        assert_eq!(counters["goodbye_relay_module_dropped"], 1);
        assert_eq!(
            counters["goodbye_relay_module_dropped_by_module"],
            serde_json::json!({ "epoch-router": 1 })
        );
        assert_eq!(counters["route_released_epoch_fenced"], 1);
    }

    #[tokio::test]
    async fn module_request_on_stale_epoch_receives_stale_route_epoch() {
        let (
            router,
            _forwarding,
            _client_ctx,
            _client_rx,
            module_ctx,
            mut module_egress_rx,
            mut module_rx,
            pending,
        ) = dynamic_route_fixture(true);

        router
            .route_for_connection(
                &module_ctx,
                route_frame(
                    FrameType::Request,
                    pending.module_channel,
                    pending.module_epoch + 1,
                    1_000,
                ),
            )
            .await
            .unwrap();

        let error = module_egress_rx.try_recv().unwrap();
        assert_eq!(error.header.ty, FrameType::Error);
        assert_eq!(error.header.channel, pending.module_channel);
        assert_eq!(error.header.epoch, pending.module_epoch + 1);
        assert_eq!(error.header.corr, 1_000);
        let body: ErrorBody = serde_json::from_slice(&error.body).unwrap();
        assert_eq!(body.code, "stale_route_epoch");
        assert!(module_rx.try_recv().is_err());
        let counters = router.counters.snapshot();
        assert_eq!(counters["module_requests_dropped_stale_route"], 1);
        assert_eq!(counters["module_frames_dropped_no_route"], 0);
    }

    #[tokio::test]
    async fn module_request_on_reserved_or_absent_route_receives_unknown_channel() {
        let (
            reserved_router,
            _forwarding,
            _client_ctx,
            _client_rx,
            reserved_module_ctx,
            mut reserved_module_egress_rx,
            _module_rx,
            reserved,
        ) = dynamic_route_fixture(false);
        reserved_router
            .route_for_connection(
                &reserved_module_ctx,
                route_frame(
                    FrameType::Request,
                    reserved.module_channel,
                    reserved.module_epoch,
                    1_001,
                ),
            )
            .await
            .unwrap();
        let reserved_error = reserved_module_egress_rx.try_recv().unwrap();
        let reserved_body: ErrorBody = serde_json::from_slice(&reserved_error.body).unwrap();
        assert_eq!(reserved_error.header.ty, FrameType::Error);
        assert_eq!(reserved_error.header.channel, reserved.module_channel);
        assert_eq!(reserved_error.header.epoch, reserved.module_epoch);
        assert_eq!(reserved_error.header.corr, 1_001);
        assert_eq!(reserved_body.code, "unknown_channel");
        assert_eq!(
            reserved_router.counters.snapshot()["module_requests_dropped_stale_route"],
            1
        );

        let (
            absent_router,
            _forwarding,
            _client_ctx,
            _client_rx,
            absent_module_ctx,
            mut absent_module_egress_rx,
            _module_rx,
            absent,
        ) = dynamic_route_fixture(false);
        absent_router
            .route_for_connection(
                &absent_module_ctx,
                route_frame(
                    FrameType::Request,
                    absent.module_channel + 1,
                    absent.module_epoch,
                    1_002,
                ),
            )
            .await
            .unwrap();
        let absent_error = absent_module_egress_rx.try_recv().unwrap();
        let absent_body: ErrorBody = serde_json::from_slice(&absent_error.body).unwrap();
        assert_eq!(absent_error.header.ty, FrameType::Error);
        assert_eq!(absent_error.header.channel, absent.module_channel + 1);
        assert_eq!(absent_error.header.epoch, absent.module_epoch);
        assert_eq!(absent_error.header.corr, 1_002);
        assert_eq!(absent_body.code, "unknown_channel");
        assert_eq!(
            absent_router.counters.snapshot()["module_requests_dropped_stale_route"],
            1
        );
    }

    #[tokio::test]
    async fn non_request_module_frame_on_dead_route_is_counted_without_error() {
        let (
            router,
            forwarding,
            client_ctx,
            mut client_rx,
            module_ctx,
            mut module_egress_rx,
            mut module_rx,
            pending,
        ) = dynamic_route_fixture(true);
        let (other_module_tx, _other_module_rx) = mpsc::channel(8);
        forwarding
            .register_module_connection(
                ConnectionId::new(502),
                "other-module".into(),
                2,
                Concurrency::ModuleManaged,
                FrameSink::new(other_module_tx),
            )
            .unwrap();
        let _ = client_rx.recv().await.unwrap();

        router
            .route_for_connection(
                &client_ctx,
                route_frame(
                    FrameType::Goodbye,
                    pending.client_channel,
                    pending.client_epoch,
                    1_003,
                ),
            )
            .await
            .unwrap();
        let _ = module_rx.recv().await.unwrap();

        router
            .route_for_connection(
                &module_ctx,
                route_frame(
                    FrameType::StreamData,
                    pending.module_channel,
                    pending.module_epoch,
                    1_004,
                ),
            )
            .await
            .unwrap();

        assert!(module_egress_rx.try_recv().is_err());
        let counters = router.counters.snapshot();
        assert_eq!(counters["module_frames_dropped_no_route"], 1);
        assert_eq!(
            counters["module_frames_dropped_no_route_by_module"],
            serde_json::json!({ "epoch-router": 1 })
        );
        assert_eq!(counters["module_requests_dropped_stale_route"], 0);
    }

    #[test]
    fn channel_zero_cannot_be_registered_as_backend() {
        let mut router = Router::with_default_self_handler();

        let err = router.register_backend(0, EchoBackend).unwrap_err();

        assert_eq!(err, RouterError::ReservedChannelZero);
    }
}
