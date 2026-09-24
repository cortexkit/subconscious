use std::{
    collections::HashMap,
    error::Error,
    fmt,
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use subc_protocol::{error_codes, ErrorBody, Flags, FrameType, Priority};
use tokio::sync::{mpsc, Notify};
use tracing::debug;

use crate::{
    control::ControlHandler,
    forwarding::{
        CloseReason, ConnectionCloseReceiver, DataRoute, DataRouteState, ForwardingError,
        ForwardingTable, RouteBinding, RouteRelease, UndeliveredFrame,
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
    pub(crate) flushed: Option<tokio::sync::oneshot::Sender<()>>,
    /// This frame's share of the connection's queued-byte count. Dropping the
    /// frame (after the writer has written it, or when the queue itself is
    /// dropped with the frame still in it) gives the bytes back. `None` only for
    /// frames built outside a [`FrameSink`], which exist in tests alone.
    pub(crate) charge: Option<EgressCharge>,
}

impl OutboundFrame {
    fn charged(frame: Frame, charge: EgressCharge) -> Self {
        Self {
            frame,
            enqueued_at: charge.enqueued_at,
            flushed: None,
            charge: Some(charge),
        }
    }
}

/// Bytes a frame occupies in the connection's egress queue for budget
/// purposes: the fixed envelope header plus the body.
fn queued_frame_bytes(frame: &Frame) -> usize {
    subc_protocol::HEADER_LEN + frame.body.len()
}

/// A point-in-time view of one connection's egress queue, for diagnosing why a
/// frame did not fit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EgressBacklog {
    pub queued_bytes: usize,
    pub queued_frames: usize,
    /// Now minus the enqueue time of the frame the writer most recently took
    /// off the queue (the frame it is writing, or has just written), or, if
    /// the writer has taken nothing since the queue was last empty, of the
    /// frame that made the queue non-empty. While the writer is blocked on a
    /// frame this is exactly the oldest frame's age; between frames it can be
    /// one frame older than the true head. `None` when nothing is queued.
    pub oldest_age: Option<Duration>,
}

/// Queued-byte accounting shared by every clone of one connection's
/// [`FrameSink`] and by every frame that sink has admitted. Everything here is
/// an atomic: this is on the path of every frame a module sends to a client,
/// so it takes no lock per frame.
#[derive(Debug)]
struct EgressAccounting {
    byte_budget: usize,
    queued_bytes: AtomicUsize,
    queued_frames: AtomicUsize,
    /// Origin for the nanosecond timestamps in `oldest_enqueued_nanos`.
    time_base: Instant,
    /// Enqueue time, as nanoseconds after `time_base` plus one, of the frame
    /// described by [`EgressBacklog::oldest_age`]; 0 means none.
    oldest_enqueued_nanos: AtomicU64,
    /// Awaited senders currently parked on `freed`. A release only pays for a
    /// notification when this is non-zero.
    waiters: AtomicUsize,
    /// Woken when a charged frame releases its bytes while a sender waits, so
    /// the awaited send can re-check for room.
    freed: Notify,
}

impl EgressAccounting {
    fn new(byte_budget: usize) -> Self {
        Self {
            byte_budget,
            queued_bytes: AtomicUsize::new(0),
            queued_frames: AtomicUsize::new(0),
            time_base: Instant::now(),
            oldest_enqueued_nanos: AtomicU64::new(0),
            waiters: AtomicUsize::new(0),
            freed: Notify::new(),
        }
    }

    fn stamp(&self, at: Instant) -> u64 {
        (at.saturating_duration_since(self.time_base).as_nanos() as u64).saturating_add(1)
    }

    /// Charge `bytes` if they fit in the budget. A frame larger than the whole
    /// budget (bodies may be up to 64 MiB) is still admitted into an EMPTY
    /// queue, since it could otherwise never be sent at all; it simply has the
    /// queue to itself until it is written.
    fn try_charge(self: &Arc<Self>, bytes: usize) -> Option<EgressCharge> {
        // SeqCst pairs with `release`: either this load sees bytes a release
        // just freed, or that release sees this sender counted in `waiters`.
        let mut current = self.queued_bytes.load(Ordering::SeqCst);
        loop {
            if current != 0 && current.saturating_add(bytes) > self.byte_budget {
                return None;
            }
            match self.queued_bytes.compare_exchange_weak(
                current,
                current + bytes,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return Some(self.record(bytes)),
                Err(actual) => current = actual,
            }
        }
    }

    /// Charge `bytes` regardless of the budget. Used only for a frame whose
    /// queue slot was reserved in advance, which must be sendable.
    fn charge_unconditionally(self: &Arc<Self>, bytes: usize) -> EgressCharge {
        self.queued_bytes.fetch_add(bytes, Ordering::SeqCst);
        self.record(bytes)
    }

    fn record(self: &Arc<Self>, bytes: usize) -> EgressCharge {
        let enqueued_at = Instant::now();
        let stamp = self.stamp(enqueued_at);
        if self.queued_frames.fetch_add(1, Ordering::AcqRel) == 0 {
            // This frame made the queue non-empty, so it is the head until the
            // writer takes something.
            self.oldest_enqueued_nanos.store(stamp, Ordering::Release);
        }
        EgressCharge {
            accounting: Arc::clone(self),
            bytes,
            stamp,
            enqueued_at,
        }
    }

    /// The writer has taken the frame stamped `stamp` off the queue.
    fn taken_by_writer(&self, stamp: u64) {
        self.oldest_enqueued_nanos.store(stamp, Ordering::Release);
    }

    fn release(&self, bytes: usize, stamp: u64) {
        self.queued_bytes.fetch_sub(bytes, Ordering::SeqCst);
        if self.queued_frames.fetch_sub(1, Ordering::AcqRel) == 1 {
            // The queue drained. Clear the marker only if it still names this
            // frame, so a frame admitted in the meantime keeps its stamp.
            let _ = self.oldest_enqueued_nanos.compare_exchange(
                stamp,
                0,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
        }
        if self.waiters.load(Ordering::SeqCst) != 0 {
            self.freed.notify_waiters();
        }
    }

    fn backlog(&self) -> EgressBacklog {
        let queued_frames = self.queued_frames.load(Ordering::Acquire);
        let stamp = self.oldest_enqueued_nanos.load(Ordering::Acquire);
        let oldest_age = (queued_frames != 0 && stamp != 0).then(|| {
            let enqueued = self.time_base + Duration::from_nanos(stamp - 1);
            enqueued.elapsed()
        });
        EgressBacklog {
            queued_bytes: self.queued_bytes.load(Ordering::Acquire),
            queued_frames,
            oldest_age,
        }
    }
}

/// One admitted frame's claim on its connection's egress byte budget,
/// returned when the frame is dropped.
#[derive(Debug)]
pub(crate) struct EgressCharge {
    accounting: Arc<EgressAccounting>,
    bytes: usize,
    stamp: u64,
    enqueued_at: Instant,
}

impl EgressCharge {
    /// Called by the connection writer when it takes this frame off the queue,
    /// so the backlog's oldest-age figure follows the writer.
    pub(crate) fn taken_by_writer(&self) {
        self.accounting.taken_by_writer(self.stamp);
    }
}

impl Drop for EgressCharge {
    fn drop(&mut self) {
        self.accounting.release(self.bytes, self.stamp);
    }
}

/// A queue slot reserved ahead of time (a pending `route.open` holds one until
/// its module answers). Sending through it never waits and never fails for
/// lack of room: the slot is already held, and its frame is charged to the
/// byte count outside the budget check. At most
/// `MAX_PENDING_ROUTE_OPENS_PER_CONNECTION` such frames exist per connection.
#[derive(Debug)]
pub(crate) struct EgressPermit {
    permit: mpsc::OwnedPermit<OutboundFrame>,
    accounting: Arc<EgressAccounting>,
}

impl EgressPermit {
    /// Enqueue `frame` in the reserved slot. Returns true when the connection's
    /// writer had already gone away, meaning the frame will never be written.
    pub(crate) fn send(self, frame: Frame) -> bool {
        let charge = self
            .accounting
            .charge_unconditionally(queued_frame_bytes(&frame));
        let sender = self.permit.send(OutboundFrame::charged(frame, charge));
        sender.is_closed()
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
/// single path. The queue is bounded twice: by queued BYTES (the budget that
/// matters for memory and for how long a slow reader may pause), and by the
/// `mpsc` channel's frame count (a backstop). The socket layer owns the sole
/// receiver/writer.
#[derive(Debug, Clone)]
pub struct FrameSink {
    tx: mpsc::Sender<OutboundFrame>,
    accounting: Arc<EgressAccounting>,
}

impl FrameSink {
    /// A sink with the standard per-connection byte budget
    /// ([`crate::server::CONNECTION_EGRESS_BYTE_BUDGET`]); the frame-count bound
    /// is whatever capacity `tx`'s channel was created with.
    pub fn new(tx: mpsc::Sender<OutboundFrame>) -> Self {
        Self::with_byte_budget(tx, crate::server::CONNECTION_EGRESS_BYTE_BUDGET)
    }

    pub(crate) fn with_byte_budget(tx: mpsc::Sender<OutboundFrame>, byte_budget: usize) -> Self {
        Self {
            tx,
            accounting: Arc::new(EgressAccounting::new(byte_budget)),
        }
    }

    /// Wait until the frame's bytes fit the budget, then charge them. Awaited
    /// senders (control replies, error replies, shutdown notices) are never
    /// refused by the byte budget: they wait for the writer to free bytes, just
    /// as they wait for a free slot, which is the same backpressure they had
    /// when the queue was bounded by frame count alone. Returns `None` if the
    /// writer goes away while waiting.
    async fn charge_waiting(&self, bytes: usize) -> Option<EgressCharge> {
        if let Some(charge) = self.accounting.try_charge(bytes) {
            return Some(charge);
        }
        // Counted as a waiter for as long as this future is parked, including
        // when it is cancelled, so releases notify only while someone waits.
        struct Waiting<'a>(&'a AtomicUsize);
        impl Drop for Waiting<'_> {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }
        self.accounting.waiters.fetch_add(1, Ordering::SeqCst);
        let _waiting = Waiting(&self.accounting.waiters);
        loop {
            let freed = self.accounting.freed.notified();
            tokio::pin!(freed);
            // Register interest before checking, so a release that happens
            // between the check and the await still wakes this sender.
            freed.as_mut().enable();
            if let Some(charge) = self.accounting.try_charge(bytes) {
                return Some(charge);
            }
            tokio::select! {
                _ = &mut freed => {}
                _ = self.tx.closed() => return None,
            }
        }
    }

    pub async fn send(&self, frame: Frame) -> Result<(), RouterError> {
        let channel = frame.header.channel;
        let epoch = frame.header.epoch;
        let corr = frame.header.corr;
        let closed =
            || RouterError::backend_with_epoch(channel, epoch, corr, "connection writer closed");
        let charge = self
            .charge_waiting(queued_frame_bytes(&frame))
            .await
            .ok_or_else(closed)?;
        self.tx
            .send(OutboundFrame::charged(frame, charge))
            .await
            .map_err(|_| closed())
    }

    /// Shutdown notices must leave the socket writer before an idle daemon exits.
    /// Queue admission alone does not prove this; the writer acknowledges flush.
    #[cfg(unix)]
    pub(crate) async fn send_flushed(&self, frame: Frame) -> Result<(), RouterError> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let charge = self
            .charge_waiting(queued_frame_bytes(&frame))
            .await
            .ok_or_else(|| RouterError::backend(0, 0, "connection writer closed"))?;
        let mut outbound = OutboundFrame::charged(frame, charge);
        outbound.flushed = Some(tx);
        self.tx
            .send(outbound)
            .await
            .map_err(|_| RouterError::backend(0, 0, "connection writer closed"))?;
        rx.await
            .map_err(|_| RouterError::backend(0, 0, "connection flush failed"))
    }

    pub(crate) async fn reserve_owned(&self) -> Result<EgressPermit, RouterError> {
        let permit = self
            .tx
            .clone()
            .reserve_owned()
            .await
            .map_err(|_| RouterError::backend(0, 0, "connection writer closed"))?;
        Ok(EgressPermit {
            permit,
            accounting: Arc::clone(&self.accounting),
        })
    }

    #[cfg(test)]
    pub(crate) fn try_reserve_owned(&self) -> Result<EgressPermit, RouterError> {
        let permit = self
            .tx
            .clone()
            .try_reserve_owned()
            .map_err(|err| RouterError::backend(0, 0, err.to_string()))?;
        Ok(EgressPermit {
            permit,
            accounting: Arc::clone(&self.accounting),
        })
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }

    /// What the connection's egress queue holds right now.
    pub(crate) fn backlog(&self) -> EgressBacklog {
        self.accounting.backlog()
    }

    /// Enqueue without waiting. Fails when the frame's bytes would take the
    /// queue past its byte budget, when the channel's frame-count backstop is
    /// full, or when the writer is gone.
    pub(crate) fn try_send(&self, frame: Frame) -> Result<(), RouterError> {
        let channel = frame.header.channel;
        let epoch = frame.header.epoch;
        let corr = frame.header.corr;
        let unavailable = |why: String| {
            RouterError::backend_with_epoch(
                channel,
                epoch,
                corr,
                format!("connection writer unavailable: {why}"),
            )
        };
        let bytes = queued_frame_bytes(&frame);
        let Some(charge) = self.accounting.try_charge(bytes) else {
            if self.tx.is_closed() {
                return Err(unavailable("channel closed".to_string()));
            }
            return Err(unavailable(format!(
                "egress byte budget exhausted ({} queued bytes, frame of {bytes} bytes, budget {})",
                self.accounting.queued_bytes.load(Ordering::Acquire),
                self.accounting.byte_budget
            )));
        };
        // A refused frame is dropped inside the error, which returns its charge.
        self.tx
            .try_send(OutboundFrame::charged(frame, charge))
            .map_err(|err| unavailable(err.to_string()))
    }
}

/// Minimum time between two route GOODBYEs the daemon sends one module
/// connection for one channel in answer to the module's frames on a (channel,
/// epoch) the daemon holds no route for.
///
/// A module that missed a GOODBYE keeps sending on that route (a streaming
/// module, many frames a second), and every one of those frames lands here. One
/// answer is enough when it arrives, so answering each frame would only flood
/// the module's egress queue at the moment it is already behind. Five seconds
/// is long enough for an answer queued behind a deep backlog to reach the module
/// and take effect before a second is sent, and short enough that an answer
/// the queue refused is retried by the next orphan frame well within a minute.
const ORPHAN_ROUTE_GOODBYE_INTERVAL: Duration = Duration::from_secs(5);

/// Once one connection has this many remembered channels, entries older than
/// [`ORPHAN_ROUTE_GOODBYE_INTERVAL`] are pruned before another is added. An
/// expired entry has no effect on rate limiting, so pruning never changes a
/// decision; it keeps a module that orphaned many channels long ago from
/// pinning memory for all of them. The map is bounded in any case by the
/// 16-bit channel space and is dropped whole when the connection ends.
const ORPHAN_ROUTE_GOODBYE_PRUNE_AT: usize = 256;

/// When each module connection was last sent an orphan-route GOODBYE, per
/// channel. Shared by the [`Router`] (which consults it) and every
/// [`RouterConnection`] (which removes its own entry when the connection ends).
#[derive(Debug, Default)]
struct OrphanGoodbyeLimiter {
    last_sent: Mutex<HashMap<ConnectionId, HashMap<u16, tokio::time::Instant>>>,
}

impl OrphanGoodbyeLimiter {
    /// True, and the send recorded, when `channel` on `connection_id` has not
    /// been answered within the interval.
    fn claim(&self, connection_id: ConnectionId, channel: u16) -> bool {
        let now = tokio::time::Instant::now();
        let mut last_sent = self
            .last_sent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let channels = last_sent.entry(connection_id).or_default();
        if channels.get(&channel).is_some_and(|sent| {
            now.saturating_duration_since(*sent) < ORPHAN_ROUTE_GOODBYE_INTERVAL
        }) {
            return false;
        }
        if channels.len() >= ORPHAN_ROUTE_GOODBYE_PRUNE_AT {
            channels.retain(|_, sent| {
                now.saturating_duration_since(*sent) < ORPHAN_ROUTE_GOODBYE_INTERVAL
            });
        }
        channels.insert(channel, now);
        true
    }

    fn forget_connection(&self, connection_id: ConnectionId) {
        self.last_sent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&connection_id);
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
    orphan_goodbyes: Arc<OrphanGoodbyeLimiter>,
}

impl Router {
    pub fn with_control_handler(control: Arc<ControlHandler>) -> Self {
        // The handler the router serves is the one a swap must tell when it
        // promotes a candidate; this is where it first sits behind an `Arc`.
        control.install_swap_promotion_observer();
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
            orphan_goodbyes: Arc::default(),
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

    /// A module sent a non-request frame on a (channel, epoch) the daemon holds
    /// no route for: most often a route the daemon released whose GOODBYE the
    /// module never received, so the module still believes it is open and keeps
    /// sending. Count the drop, and tell the module to let go of exactly that
    /// (channel, epoch) with a route GOODBYE, at most once per
    /// [`ORPHAN_ROUTE_GOODBYE_INTERVAL`] per connection and channel.
    ///
    /// The answer uses `try_send`: it is a best-effort nudge, and if the queue
    /// refuses it, the module's next frame on that route after the interval
    /// asks again. A GOODBYE from the module is not answered, since the module
    /// is already letting go of the route.
    fn handle_orphan_module_frame(&self, ctx: &RouteCtx, frame: &Frame) -> Result<(), RouterError> {
        let channel = frame.header.channel;
        let epoch = frame.header.epoch;
        let module_id = self
            .forwarding
            .module_id_for_connection(ctx.connection_id)
            .map_err(RouterError::Forwarding)?;
        self.counters
            .increment_module_frames_dropped_no_route(module_id.as_deref());
        if self
            .forwarding
            .module_route_epoch_was_allocated(ctx.connection_id, channel, epoch)
            .map_err(RouterError::Forwarding)?
        {
            self.counters
                .increment_module_frames_dropped_released_route(module_id.as_deref());
        }
        if frame.header.ty == FrameType::Goodbye
            || !self.orphan_goodbyes.claim(ctx.connection_id, channel)
        {
            return Ok(());
        }
        let goodbye = Frame::build_with_version(
            frame.header.ver,
            FrameType::Goodbye,
            Flags::new(false, Priority::Passive, false),
            channel,
            epoch,
            0,
            Vec::new(),
        )
        .map_err(RouterError::FrameBuild)?;
        match ctx.egress.try_send(goodbye) {
            Ok(()) => {
                self.counters.increment_module_orphan_route_goodbyes_sent();
                debug!(
                    connection_id = ctx.connection_id.get(),
                    module_id = module_id.as_deref().unwrap_or("unknown"),
                    channel,
                    epoch,
                    "answered module frame on a route the daemon does not hold with a route GOODBYE"
                );
            }
            Err(err) => debug!(
                connection_id = ctx.connection_id.get(),
                module_id = module_id.as_deref().unwrap_or("unknown"),
                channel,
                epoch,
                error = %err,
                "could not enqueue route GOODBYE for module frame on a route the daemon does not hold; the next such frame retries"
            ),
        }
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
            orphan_goodbyes: Arc::clone(&self.orphan_goodbyes),
        }
    }

    pub(crate) fn route_open_target(&self, frame: &Frame) -> Option<String> {
        self.control.route_open_target(frame)
    }

    pub(crate) fn route_open_capacity_refusal(
        &self,
        ctx: &RouteCtx,
        frame: &Frame,
        target_module_id: &str,
        limit: usize,
    ) -> Result<Frame, RouterError> {
        self.control
            .route_open_capacity_refusal(ctx, frame, target_module_id, limit)
    }

    pub async fn route_for_connection(
        &self,
        ctx: &RouteCtx,
        frame: Frame,
    ) -> Result<(), RouterError> {
        self.route_for_connection_started(ctx, frame, None).await
    }

    pub(crate) async fn route_for_connection_started(
        &self,
        ctx: &RouteCtx,
        frame: Frame,
        dispatch_started_at: Option<Instant>,
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
            // The connection loop dispatches every control operation directly
            // except `route.open`. Its task passes a timestamp captured before
            // spawn, so slow-dispatch timing includes scheduler delay but no
            // wait in an application-owned queue.
            let dispatch_started_at = (frame.header.ty == FrameType::Request)
                .then(|| dispatch_started_at.unwrap_or_else(Instant::now));
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
                    self.handle_orphan_module_frame(ctx, &frame)?;
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
                    self.handle_orphan_module_frame(ctx, &frame)?;
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
                                        UndeliveredFrame {
                                            module_id: Some(&route.module_id),
                                            sink: &target.sink,
                                        },
                                    )
                                    .map_err(RouterError::Forwarding)?
                            {
                                self.counters.increment_goodbye_relay_client_failed();
                            }
                        }
                    }
                    return Ok(());
                }

                // A terminal frame ends the request at the module whether or not
                // the client can still take it, so its credit is released before
                // delivery is attempted. Releasing only after a successful
                // enqueue would leave a drain counting a finished request until
                // the client connection's cleanup removes the route.
                let releases_credit = is_terminal_frame(frame.header.ty);
                if releases_credit {
                    route.flow.release_corr(corr);
                }
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
                            UndeliveredFrame {
                                module_id: Some(&route.module_id),
                                sink: &route.client_sink,
                            },
                        )
                        .map_err(RouterError::Forwarding)?
                    {
                        self.counters
                            .increment_client_egress_close_delivery_failed();
                    }
                    return Ok(());
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
    orphan_goodbyes: Arc<OrphanGoodbyeLimiter>,
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
        self.orphan_goodbyes.forget_connection(self.id);
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
            if let Err(err) = route
                .flow
                .acquire_tagged(corr, frame.header.flags.is_subscription())
                .await
            {
                // `module_reloading` here is answered BEFORE the frame is
                // forwarded, so the module never sees the request and callers
                // may re-dispatch it after reopening the route. That is a wire
                // guarantee documented on `error_codes::MODULE_RELOADING`; do
                // not emit this code for a request that already reached
                // `module_sink.send` below.
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
            route.flow.release_corr(corr);
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
                error_codes::UNKNOWN_CHANNEL,
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
                error_codes::STALE_ROUTE_EPOCH,
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
    use subc_control::ModuleProtocol;
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
                    protocol: ModuleProtocol::Subc,
                    overlap: Default::default(),
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

    /// A terminal frame ends the request at the module even when the client
    /// cannot take it, so the drain must stop counting it at once rather than
    /// when the client connection's cleanup later removes the route.
    #[tokio::test]
    async fn terminal_frame_releases_its_credit_even_when_client_delivery_fails() {
        let forwarding = Arc::new(ForwardingTable::default());
        let control = Arc::new(ControlHandler::with_forwarding(
            Arc::new(crate::Registry::default()),
            Arc::clone(&forwarding),
        ));
        let router = Router::with_control_handler(control);
        let module_connection = ConnectionId::new(11);
        let client_connection = ConnectionId::new(21);
        let _close_receiver = forwarding.register_connection_close(client_connection);
        let (module_tx, _module_rx) = mpsc::channel(1);
        forwarding
            .register_module_connection(
                module_connection,
                "credit-provider".to_string(),
                1,
                Concurrency::ModuleManaged,
                FrameSink::new(module_tx),
            )
            .unwrap();
        // Capacity one, filled by the route.open response, so the terminal
        // frame below cannot be enqueued for the client.
        let (client_tx, _client_rx) = mpsc::channel(1);
        let pending = forwarding
            .begin_route_bind_relay_for_test(
                client_connection,
                FrameSink::new(client_tx),
                800,
                "credit-provider",
            )
            .unwrap();
        forwarding
            .complete_pending_relay(
                module_connection,
                pending.corr,
                RouteBindRelayOutcome::Accepted,
            )
            .unwrap();
        let DataRoute::Client(DataRouteState::Bound(route)) = forwarding
            .lookup_data_route(
                client_connection,
                pending.client_channel,
                pending.client_epoch,
            )
            .unwrap()
        else {
            panic!("expected a bound client route");
        };
        route.flow.acquire_tagged(801, false).await.unwrap();
        assert_eq!(route.flow.drain_in_flight(), 1);

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
            801,
            b"terminal".to_vec(),
        )
        .unwrap();
        router
            .route_for_connection(&module_ctx, terminal)
            .await
            .unwrap();

        assert_eq!(
            router.counters.snapshot()["client_egress_close_delivery_failed"],
            1,
            "the client delivery must have failed for this test to mean anything"
        );
        assert_eq!(
            route.flow.drain_in_flight(),
            0,
            "the module's terminal frame must release its credit even though the client could not take it"
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

    /// One module and one client connection with `routes` routes bound between
    /// them. The client uses a real connection egress queue (the same byte
    /// budget and frame-count backstop as a live connection), and its route.open
    /// responses are drained so the queue starts empty.
    async fn multi_route_client(
        module_id: &str,
        module_connection: ConnectionId,
        client_connection: ConnectionId,
        routes: usize,
    ) -> (
        Router,
        FrameSink,
        mpsc::Receiver<OutboundFrame>,
        RouteCtx,
        Vec<crate::forwarding::PendingRouteBindRelay>,
        ConnectionCloseReceiver,
    ) {
        let forwarding = Arc::new(ForwardingTable::default());
        let control = Arc::new(ControlHandler::with_forwarding(
            Arc::new(crate::Registry::default()),
            Arc::clone(&forwarding),
        ));
        let router = Router::with_control_handler(control);
        let close_receiver = forwarding.register_connection_close(client_connection);
        let (module_tx, _module_rx) = mpsc::channel(8);
        forwarding
            .register_module_connection(
                module_connection,
                module_id.to_string(),
                1,
                Concurrency::ModuleManaged,
                FrameSink::new(module_tx),
            )
            .unwrap();
        let (client_sink, mut client_rx) = crate::server::connection_egress();
        let mut bound = Vec::with_capacity(routes);
        for index in 0..routes {
            let pending = forwarding
                .begin_route_bind_relay_for_test(
                    client_connection,
                    client_sink.clone(),
                    900 + index as u64,
                    module_id,
                )
                .unwrap();
            forwarding
                .complete_pending_relay(
                    module_connection,
                    pending.corr,
                    RouteBindRelayOutcome::Accepted,
                )
                .unwrap();
            assert_eq!(
                client_rx.recv().await.unwrap().header.corr,
                900 + index as u64
            );
            bound.push(pending);
        }
        let (module_egress_tx, _module_egress_rx) = mpsc::channel(8);
        let module_ctx = RouteCtx {
            connection_id: module_connection,
            egress: FrameSink::new(module_egress_tx),
        };
        (
            router,
            client_sink,
            client_rx,
            module_ctx,
            bound,
            close_receiver,
        )
    }

    /// Per-frame cost of the egress sink's admission and release accounting:
    /// one million 200-byte frames enqueued with `try_send` and taken off the
    /// queue the way the connection writer does, in batches of 1,000 so the
    /// queue stays well inside its budget. Prints nanoseconds per frame; it is
    /// a measurement, not a gate. Run with
    /// `cargo test --release -p subc-daemon --lib egress_sink_per_frame_cost -- --ignored --nocapture`.
    #[test]
    #[ignore = "timing measurement, run on demand"]
    fn egress_sink_per_frame_cost() {
        const FRAMES: usize = 1_000_000;
        const BATCH: usize = 1_000;
        let (sink, mut rx) = crate::server::connection_egress();
        let template = stream_frame(9, 1, 0, vec![b't'; 200]);
        let started = Instant::now();
        for _ in 0..FRAMES / BATCH {
            for _ in 0..BATCH {
                sink.try_send(template.clone()).unwrap();
            }
            for _ in 0..BATCH {
                let outbound = rx.try_recv().unwrap();
                if let Some(charge) = &outbound.charge {
                    charge.taken_by_writer();
                }
                drop(std::hint::black_box(outbound));
            }
        }
        let elapsed = started.elapsed();
        assert_eq!(sink.backlog().queued_bytes, 0);
        println!(
            "egress sink: {FRAMES} frames in {elapsed:?}, {:.1} ns/frame",
            elapsed.as_nanos() as f64 / FRAMES as f64
        );
    }

    fn stream_frame(channel: u16, epoch: u32, corr: u64, body: Vec<u8>) -> Frame {
        Frame::build(
            FrameType::StreamData,
            Flags::new(false, Priority::Interactive, false),
            channel,
            epoch,
            corr,
            body,
        )
        .unwrap()
    }

    /// An awaited send (a control or error reply) is never refused by the byte
    /// budget: it parks until the writer frees bytes. If a release could miss a
    /// parked sender, that reply would hang for the connection's lifetime, so
    /// this pins the wakeup: the send stays parked while the queue is full and
    /// completes as soon as one frame leaves it.
    #[tokio::test]
    async fn awaited_send_parked_behind_a_full_byte_budget_wakes_when_bytes_free() {
        let (tx, mut rx) = mpsc::channel(1024);
        let sink = FrameSink::with_byte_budget(tx, 2_000);
        let mut queued = 0u64;
        while sink
            .try_send(stream_frame(7, 1, queued, vec![b'x'; 200]))
            .is_ok()
        {
            queued += 1;
        }
        assert!(queued > 0, "the budget admitted nothing");

        let parked = tokio::spawn({
            let sink = sink.clone();
            async move { sink.send(stream_frame(0, 0, 999, vec![b'r'; 200])).await }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            !parked.is_finished(),
            "the awaited send must wait while the byte budget is full"
        );

        // The writer takes one frame and drops it, releasing its bytes.
        drop(rx.recv().await.expect("a queued frame"));
        tokio::time::timeout(Duration::from_secs(2), parked)
            .await
            .expect("the parked send was never woken after bytes were freed")
            .unwrap()
            .unwrap();
    }

    /// A client multiplexing several token streams pauses its reader while the
    /// module keeps producing small frames. Far more frames than the old
    /// 64-frame queue allowed, but far fewer bytes than the budget, must all be
    /// held without closing the connection and delivered in order afterwards.
    #[tokio::test]
    async fn paused_client_reader_keeps_connection_through_small_frame_burst() {
        const ROUTES: usize = 4;
        const FRAMES_PER_ROUTE: usize = 250;
        let (router, client_sink, mut client_rx, module_ctx, routes, mut close_receiver) =
            multi_route_client(
                "burst-provider",
                ConnectionId::new(60),
                ConnectionId::new(61),
                ROUTES,
            )
            .await;

        // The reader is paused: nothing is received until every frame is sent.
        for seq in 0..FRAMES_PER_ROUTE as u64 {
            for (index, route) in routes.iter().enumerate() {
                let body = format!("route-{index}-token-{seq:05}-{}", "t".repeat(170));
                router
                    .route_for_connection(
                        &module_ctx,
                        stream_frame(
                            route.module_channel,
                            route.module_epoch,
                            seq,
                            body.into_bytes(),
                        ),
                    )
                    .await
                    .unwrap();
            }
        }

        let backlog = client_sink.backlog();
        assert_eq!(backlog.queued_frames, ROUTES * FRAMES_PER_ROUTE);
        assert!(backlog.queued_bytes < crate::server::CONNECTION_EGRESS_BYTE_BUDGET);
        assert!(
            close_receiver.try_recv().is_err(),
            "a paused reader under the byte budget must not be closed"
        );
        assert_eq!(
            router.counters.snapshot()["client_egress_close_delivery_failed"],
            0
        );

        // The reader resumes: every frame arrives, in order within each route.
        let mut next_seq = vec![0u64; ROUTES];
        for _ in 0..ROUTES * FRAMES_PER_ROUTE {
            let frame = client_rx.try_recv().expect("every queued frame arrives");
            let index = routes
                .iter()
                .position(|route| route.client_channel == frame.header.channel)
                .expect("frame arrives on one of the bound client channels");
            assert_eq!(frame.header.corr, next_seq[index], "route {index} order");
            let expected_prefix = format!("route-{index}-token-{:05}-", next_seq[index]);
            assert!(frame.body.starts_with(expected_prefix.as_bytes()));
            next_seq[index] += 1;
        }
        assert!(client_rx.try_recv().is_err());
        assert_eq!(next_seq, vec![FRAMES_PER_ROUTE as u64; ROUTES]);
        assert_eq!(client_sink.backlog().queued_bytes, 0);
    }

    /// A client that never reads is closed once the module's frames exceed
    /// the byte budget, and that close is reported once at WARN with what an
    /// operator needs to find the stuck reader.
    #[tokio::test]
    async fn never_reading_client_is_closed_at_byte_budget_with_warn_diagnosis() {
        let (logs, _guard) = test_log::log_capture(tracing::Level::WARN);
        const BODY: usize = 16 * 1024;
        let (router, client_sink, _client_rx, module_ctx, routes, mut close_receiver) =
            multi_route_client(
                "stuck-reader-provider",
                ConnectionId::new(70),
                ConnectionId::new(71),
                2,
            )
            .await;

        let mut admitted = 0usize;
        let mut sent = 0u64;
        while router.counters.snapshot()["client_egress_close_delivery_failed"] == 0 {
            assert!(sent < 1_000, "the byte budget never refused a frame");
            // Other tests running in parallel hit the same WARN call site with
            // no subscriber installed; if one of them registers that call site
            // while this test's capture subscriber is being installed, tracing
            // can cache the call site as disabled. Recomputing the cache just
            // before each frame that may trigger the WARN keeps the capture
            // from silently missing it.
            tracing::callsite::rebuild_interest_cache();
            let route = &routes[(sent % 2) as usize];
            router
                .route_for_connection(
                    &module_ctx,
                    stream_frame(
                        route.module_channel,
                        route.module_epoch,
                        sent,
                        vec![b'z'; BODY],
                    ),
                )
                .await
                .unwrap();
            sent += 1;
            admitted = client_sink.backlog().queued_frames;
        }
        // The budget, not the frame-count backstop, did the refusing.
        let frame_bytes = subc_protocol::HEADER_LEN + BODY;
        assert_eq!(
            admitted,
            crate::server::CONNECTION_EGRESS_BYTE_BUDGET / frame_bytes
        );
        let reason = close_receiver
            .try_recv()
            .expect("the client connection must be asked to close");
        assert!(reason
            .to_string()
            .contains("module_to_client_delivery_failed"));

        // A second refused frame for the same connection adds no second WARN.
        router
            .route_for_connection(
                &module_ctx,
                stream_frame(
                    routes[0].module_channel,
                    routes[0].module_epoch,
                    sent,
                    vec![b'z'; BODY],
                ),
            )
            .await
            .unwrap();

        let captured = test_log::captured_logs(&logs);
        let warn_lines = captured
            .lines()
            .filter(|line| {
                line.contains("closing client connection: its egress queue could not take a frame")
            })
            .collect::<Vec<_>>();
        assert_eq!(warn_lines.len(), 1, "exactly one WARN, got: {captured}");
        let line = warn_lines[0];
        assert!(line.contains("WARN"), "{line}");
        assert!(line.contains("connection_id=71"), "{line}");
        assert!(
            line.contains("module_id=\"stuck-reader-provider\""),
            "{line}"
        );
        assert!(line.contains("client_channel="), "{line}");
        assert!(line.contains("principals=direct"), "{line}");
        let queued_bytes: usize = line
            .split("queued_bytes=")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|value| value.parse().ok())
            .expect("queued_bytes is logged");
        assert_eq!(queued_bytes, admitted * frame_bytes);
        assert!(
            line.contains(&format!("queued_frames={admitted}")),
            "{line}"
        );
        assert!(line.contains("oldest_queued_ms="), "{line}");
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

        // No ERROR goes back for a non-request frame. The one reply is the
        // route GOODBYE telling the module to let go of the released route.
        let reply = module_egress_rx.try_recv().unwrap();
        assert_eq!(reply.header.ty, FrameType::Goodbye);
        assert!(module_egress_rx.try_recv().is_err());
        let counters = router.counters.snapshot();
        assert_eq!(counters["module_frames_dropped_no_route"], 1);
        assert_eq!(
            counters["module_frames_dropped_no_route_by_module"],
            serde_json::json!({ "epoch-router": 1 })
        );
        assert_eq!(counters["module_requests_dropped_stale_route"], 0);
    }

    /// Drain whatever the module connection's queue holds right now, the way a
    /// module's reader would before it stalls.
    fn drain_now(rx: &mut mpsc::Receiver<OutboundFrame>) {
        while rx.try_recv().is_ok() {}
    }

    /// A module that stops reading for a moment when a client closes one of
    /// its routes still learns the route is gone: the GOODBYE its full egress
    /// queue refused is delivered as soon as it reads again, rather than
    /// dropped, which would leave the module holding the route for the rest of
    /// its connection.
    #[tokio::test]
    async fn route_goodbye_refused_by_stalled_module_is_delivered_when_it_resumes_reading() {
        const BUDGET: usize = 4_096;
        let forwarding = Arc::new(ForwardingTable::default());
        let control = Arc::new(ControlHandler::with_forwarding(
            Arc::new(Registry::default()),
            Arc::clone(&forwarding),
        ));
        let router = Router::with_control_handler(control);
        let module_connection = ConnectionId::new(80);
        let client_connection = ConnectionId::new(81);
        let (module_tx, mut module_rx) = mpsc::channel(64);
        let module_sink = FrameSink::with_byte_budget(module_tx, BUDGET);
        forwarding
            .register_module_connection(
                module_connection,
                "stalled-provider".into(),
                2,
                Concurrency::ModuleManaged,
                module_sink.clone(),
            )
            .unwrap();
        let (client_tx, mut client_rx) = mpsc::channel(8);
        let client_sink = FrameSink::new(client_tx);
        let pending = forwarding
            .begin_route_bind_relay_for_test(
                client_connection,
                client_sink.clone(),
                1_100,
                "stalled-provider",
            )
            .unwrap();
        forwarding
            .complete_pending_relay(
                module_connection,
                pending.corr,
                RouteBindRelayOutcome::Accepted,
            )
            .unwrap();
        let _ = client_rx.recv().await.unwrap();
        drain_now(&mut module_rx);

        // The module stalls: its queue holds more than the byte budget, so the
        // queue refuses anything further.
        module_sink
            .try_send(stream_frame(9, 1, 0, vec![b'f'; BUDGET]))
            .unwrap();
        assert!(module_sink
            .try_send(stream_frame(9, 1, 1, Vec::new()))
            .is_err());

        let client_ctx = RouteCtx {
            connection_id: client_connection,
            egress: client_sink,
        };
        router
            .route_for_connection(
                &client_ctx,
                route_frame(
                    FrameType::Goodbye,
                    pending.client_channel,
                    pending.client_epoch,
                    1_101,
                ),
            )
            .await
            .unwrap();
        tokio::task::yield_now().await;
        assert_eq!(
            router.counters.snapshot()["goodbye_relay_module_dropped"],
            0,
            "a GOODBYE refused by a momentarily full module queue must not be dropped"
        );

        // The module reads again: the filler comes off, then the GOODBYE.
        let filler = module_rx.recv().await.unwrap();
        assert_eq!(filler.header.ty, FrameType::StreamData);
        drop(filler);
        let goodbye = tokio::time::timeout(Duration::from_secs(2), module_rx.recv())
            .await
            .expect("the refused GOODBYE must be delivered once the module frees room")
            .unwrap();
        assert_eq!(goodbye.header.ty, FrameType::Goodbye);
        assert_eq!(goodbye.header.channel, pending.module_channel);
        assert_eq!(goodbye.header.epoch, pending.module_epoch);
        assert_eq!(
            router.counters.snapshot()["goodbye_relay_module_dropped"],
            0
        );
    }

    /// A module still sending on a route the daemon released is told, with a
    /// route GOODBYE for exactly the (channel, epoch) it sent on. The same
    /// happens for a stale epoch on a channel whose route moved on and for a
    /// channel that never had a route; only the first is counted as traffic on
    /// a released route.
    #[tokio::test]
    async fn module_frame_on_route_the_daemon_does_not_hold_is_answered_with_goodbye() {
        let (
            router,
            _forwarding,
            client_ctx,
            mut client_rx,
            module_ctx,
            mut module_egress_rx,
            mut module_rx,
            pending,
        ) = dynamic_route_fixture(true);
        let _ = client_rx.recv().await.unwrap();
        router
            .route_for_connection(
                &client_ctx,
                route_frame(
                    FrameType::Goodbye,
                    pending.client_channel,
                    pending.client_epoch,
                    1_200,
                ),
            )
            .await
            .unwrap();
        drain_now(&mut module_rx);

        let cases = [
            // Released route: the daemon allocated this (channel, epoch).
            (pending.module_channel, pending.module_epoch),
            // A channel the daemon never allocated on this connection.
            (pending.module_channel + 1, 1),
        ];
        for (channel, epoch) in cases {
            router
                .route_for_connection(
                    &module_ctx,
                    route_frame(FrameType::StreamData, channel, epoch, 1_201),
                )
                .await
                .unwrap();
            let reply = module_egress_rx
                .try_recv()
                .expect("a frame on a route the daemon does not hold is answered");
            assert_eq!(reply.header.ty, FrameType::Goodbye);
            assert_eq!(reply.header.channel, channel);
            assert_eq!(reply.header.epoch, epoch);
            assert_eq!(reply.header.corr, 0);
            assert!(module_egress_rx.try_recv().is_err());
        }

        // A GOODBYE from the module for a route the daemon already released
        // is not answered: the module is letting go already.
        router
            .route_for_connection(
                &module_ctx,
                route_frame(FrameType::Goodbye, pending.module_channel + 2, 1, 0),
            )
            .await
            .unwrap();
        assert!(module_egress_rx.try_recv().is_err());

        let counters = router.counters.snapshot();
        assert_eq!(counters["module_frames_dropped_no_route"], 3);
        assert_eq!(counters["module_frames_dropped_released_route"], 1);
        assert_eq!(
            counters["module_frames_dropped_released_route_by_module"],
            serde_json::json!({ "epoch-router": 1 })
        );
        assert_eq!(counters["module_orphan_route_goodbyes_sent"], 2);
    }

    /// A chatty orphan (a streaming module still producing on a route it
    /// missed the GOODBYE for) is answered once per interval, not once per
    /// frame, and answered again once the interval has passed.
    #[tokio::test(start_paused = true)]
    async fn burst_of_orphan_module_frames_is_answered_once_per_interval() {
        let (
            router,
            _forwarding,
            client_ctx,
            mut client_rx,
            module_ctx,
            mut module_egress_rx,
            mut module_rx,
            pending,
        ) = dynamic_route_fixture(true);
        let _ = client_rx.recv().await.unwrap();
        router
            .route_for_connection(
                &client_ctx,
                route_frame(
                    FrameType::Goodbye,
                    pending.client_channel,
                    pending.client_epoch,
                    1_300,
                ),
            )
            .await
            .unwrap();
        drain_now(&mut module_rx);

        let send_burst = |corr: u64| {
            route_frame(
                FrameType::StreamData,
                pending.module_channel,
                pending.module_epoch,
                corr,
            )
        };
        for corr in 0..20 {
            router
                .route_for_connection(&module_ctx, send_burst(corr))
                .await
                .unwrap();
        }
        let mut replies = 0;
        while let Ok(reply) = module_egress_rx.try_recv() {
            assert_eq!(reply.header.ty, FrameType::Goodbye);
            replies += 1;
        }
        assert_eq!(replies, 1, "a burst within the interval gets one GOODBYE");

        tokio::time::advance(ORPHAN_ROUTE_GOODBYE_INTERVAL).await;
        router
            .route_for_connection(&module_ctx, send_burst(20))
            .await
            .unwrap();
        let retry = module_egress_rx
            .try_recv()
            .expect("the first orphan frame after the interval is answered again");
        assert_eq!(retry.header.channel, pending.module_channel);
        assert_eq!(retry.header.epoch, pending.module_epoch);

        let counters = router.counters.snapshot();
        assert_eq!(counters["module_frames_dropped_no_route"], 21);
        assert_eq!(counters["module_orphan_route_goodbyes_sent"], 2);
    }

    /// The rate-limit memory is per connection and goes away with it.
    #[test]
    fn orphan_goodbye_rate_limit_state_is_released_with_the_connection() {
        let router = Router::with_default_self_handler();
        let connection = router.begin_connection();
        let id = connection.id();
        assert!(router.orphan_goodbyes.claim(id, 7));
        assert!(!router.orphan_goodbyes.claim(id, 7));
        assert!(router.orphan_goodbyes.claim(id, 8));
        drop(connection);
        assert!(router
            .orphan_goodbyes
            .last_sent
            .lock()
            .unwrap()
            .get(&id)
            .is_none());
    }

    #[test]
    fn channel_zero_cannot_be_registered_as_backend() {
        let mut router = Router::with_default_self_handler();

        let err = router.register_backend(0, EchoBackend).unwrap_err();

        assert_eq!(err, RouterError::ReservedChannelZero);
    }
}
