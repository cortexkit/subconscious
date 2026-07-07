use std::{
    collections::HashMap,
    error::Error,
    fmt,
    future::Future,
    io,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, MutexGuard,
    },
    task::{Context, Poll},
    time::Duration,
};

use subc_control::{ClientControlRequest, ClientControlResponse, ConsumerIdentity};
use subc_protocol::{
    BindIdentity, ErrorBody, Flags, Frame, FrameBuildError, FrameType, Priority, RouteTarget,
    SUBC_LAUNCH_NONCE_ENV, SUBC_MODULE_ID_ENV,
};
use subc_transport::{
    authenticate_client, connection_file, read_frame, write_frame, AuthError, ConnectionFileError,
    FrameIoError,
};
use tokio::{
    io::{AsyncWriteExt, BufWriter},
    net::{tcp::OwnedReadHalf, tcp::OwnedWriteHalf, TcpStream},
    sync::{mpsc, oneshot, Notify, OwnedSemaphorePermit, Semaphore},
    task::JoinHandle,
    time::{sleep, Instant},
};
use tokio_util::sync::CancellationToken;

const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);
const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(30);
// Sized against the daemon's route.bind relay timeout (12s default): one
// load-stalled bind relay burns ~12s before the daemon rejects with
// module_timeout, so a 10s budget could be exhausted by a SINGLE slow bind.
// 30s leaves room for ~2 full relay waits plus backoff (still clamped by the
// overall call timeout below).
const DEFAULT_ROUTE_RETRY_DEADLINE: Duration = Duration::from_secs(30);
const DEFAULT_RESTORED_DEBOUNCE: Duration = Duration::from_millis(250);
const EGRESS_BUFFER: usize = 128;
const DEFAULT_ROUTE_WINDOW: usize = 1024;
const DEFAULT_SUBSCRIPTION_EVENT_BUFFER: usize = 128;

/// Capped exponential backoff used for reconnects and transient route-open retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryBackoff {
    pub base: Duration,
    pub cap: Duration,
    /// Maximum attempts, including the first immediate attempt.
    pub max_attempts: usize,
}

impl Default for RetryBackoff {
    fn default() -> Self {
        Self {
            base: Duration::from_millis(100),
            cap: Duration::from_secs(2),
            max_attempts: 6,
        }
    }
}

impl RetryBackoff {
    fn delay_after_attempt(self, attempt: usize) -> Duration {
        let mut delay = self.base;
        for _ in 1..attempt {
            delay = (delay * 2).min(self.cap);
        }
        delay
    }
}

/// Options for [`SubcConsumer::connect`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsumerOptions {
    pub handshake_timeout: Duration,
    pub reconnect_backoff: RetryBackoff,
    pub restored_debounce: Duration,
}

impl Default for ConsumerOptions {
    fn default() -> Self {
        Self {
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
            reconnect_backoff: RetryBackoff::default(),
            restored_debounce: DEFAULT_RESTORED_DEBOUNCE,
        }
    }
}

/// Options for [`SubcConsumer::close_route`].
#[derive(Debug, Clone)]
pub struct CloseRouteOptions {
    /// Await in-flight unary requests on the route to settle naturally before tearing
    /// it down. Defaults to false: close immediately, settling anything in flight as
    /// at-most-once failures (outcome_unknown if already sent, not_sent otherwise).
    pub drain: bool,
    /// Upper bound on the drain wait (ignored when `drain` is false).
    pub drain_timeout: Duration,
    /// Override for the consumer identity used to locate the route being closed;
    /// when absent, SUBC_MODULE_ID and SUBC_LAUNCH_NONCE environment variables
    /// identify the route for a supervised consumer.
    pub consumer_identity: Option<ConsumerIdentity>,
    /// Consumer-declared reverse-request capabilities for the route being closed.
    /// This is a declaration, not a verified privilege; providers treat an absent
    /// field as no reverse-request capability. Known MCP method-family values
    /// today are "elicitation", "sampling", and "roots".
    pub consumer_capabilities: Option<Vec<String>>,
}

impl Default for CloseRouteOptions {
    fn default() -> Self {
        Self {
            drain: false,
            drain_timeout: DEFAULT_CALL_TIMEOUT,
            consumer_identity: None,
            consumer_capabilities: None,
        }
    }
}

/// Per-call options for [`SubcConsumer::call`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallOptions {
    /// Deadline for the whole managed call, including route-open retry and the response wait.
    pub timeout: Duration,
    pub priority: Priority,
    pub route_retry: RetryBackoff,
    /// Maximum real-time limit for retrying route.open attempts when the target is temporarily absent.
    pub route_retry_deadline: Duration,
    /// Explicit consumer identity for route.open; when absent, non-empty SUBC_MODULE_ID and SUBC_LAUNCH_NONCE environment variables are used.
    pub consumer_identity: Option<ConsumerIdentity>,
    /// Consumer-declared reverse-request capabilities for route.open. This is a
    /// declaration, not a verified privilege; providers treat an absent field as
    /// no reverse-request capability. Known MCP method-family values today are
    /// "elicitation", "sampling", and "roots".
    pub consumer_capabilities: Option<Vec<String>>,
}

impl Default for CallOptions {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_CALL_TIMEOUT,
            priority: Priority::Interactive,
            route_retry: RetryBackoff::default(),
            route_retry_deadline: DEFAULT_ROUTE_RETRY_DEADLINE,
            consumer_identity: None,
            consumer_capabilities: None,
        }
    }
}

/// Options for [`SubcConsumer::subscribe`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubscribeOptions {
    pub priority: Priority,
    /// Maximum number of events buffered for the caller before the subscription is dropped.
    /// The reader task never awaits a slow event consumer; if this bounded channel fills,
    /// `closed()` resolves with [`CallError::SubscriptionBackpressure`].
    pub event_buffer: usize,
    pub route_retry: RetryBackoff,
    /// Maximum real-time limit for retrying route.open attempts when the target is temporarily absent.
    pub route_retry_deadline: Duration,
    /// Deadline for opening the managed route and queuing the held-open request.
    /// The subscription itself has no response timeout once the request is sent.
    pub route_open_timeout: Duration,
    /// Explicit consumer identity for route.open; when absent, non-empty SUBC_MODULE_ID and SUBC_LAUNCH_NONCE environment variables are used.
    pub consumer_identity: Option<ConsumerIdentity>,
    /// Consumer-declared reverse-request capabilities for route.open. This is a
    /// declaration, not a verified privilege; providers treat an absent field as
    /// no reverse-request capability. Known MCP method-family values today are
    /// "elicitation", "sampling", and "roots".
    pub consumer_capabilities: Option<Vec<String>>,
}

impl Default for SubscribeOptions {
    fn default() -> Self {
        Self {
            priority: Priority::Interactive,
            event_buffer: DEFAULT_SUBSCRIPTION_EVENT_BUFFER,
            route_retry: RetryBackoff::default(),
            route_retry_deadline: DEFAULT_ROUTE_RETRY_DEADLINE,
            route_open_timeout: DEFAULT_CALL_TIMEOUT,
            consumer_identity: None,
            consumer_capabilities: None,
        }
    }
}

/// Minimal connection lifecycle signal. It is useful for logging and route-cache invalidation,
/// but callers must not use the consumer epoch as proof that a target provider is current.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionState {
    Dropped,
    Restored { epoch: u64 },
}

/// Managed Rust consumer for subc route calls.
pub struct SubcConsumer {
    shared: Arc<Shared>,
}

/// A live subscription to a provider event stream.
///
/// The event receiver yields each `StreamData` payload for the held-open request's
/// correlation id. Await [`Subscription::closed`] to learn whether the provider ended
/// the stream cleanly (`StreamEnd`) or the stream was rejected by an Error frame,
/// route GOODBYE, connection drop, or local backpressure. Dropping the subscription
/// sends a best-effort Cancel frame, the same as calling [`Subscription::unsubscribe`].
pub struct Subscription {
    events: mpsc::Receiver<Vec<u8>>,
    closed: SubscriptionClosed,
    cancel: SubscriptionCancel,
}

impl Subscription {
    /// Receive event payloads emitted as `StreamData` frames for this subscription.
    pub fn events(&mut self) -> &mut mpsc::Receiver<Vec<u8>> {
        &mut self.events
    }

    /// Future that resolves when the subscription reaches a terminal state.
    ///
    /// It resolves with `Ok(())` on `StreamEnd` or local unsubscribe, and returns a
    /// [`CallError`] for module Error frames, route teardown, connection loss, or
    /// event-channel backpressure. Await it after the event receiver returns `None`
    /// to distinguish a clean end from an error.
    pub fn closed(&mut self) -> &mut SubscriptionClosed {
        &mut self.closed
    }

    /// Cancel the held-open request.
    ///
    /// This sends a best-effort header-only Cancel frame for the subscription's
    /// `(channel, corr)` and settles [`Subscription::closed`] promptly with `Ok(())`.
    /// The provider may still send a terminal frame later; it is ignored because the
    /// local subscription is already closed.
    pub fn unsubscribe(&self) {
        self.cancel.unsubscribe();
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.cancel.unsubscribe();
    }
}

/// Future returned by [`Subscription::closed`].
pub struct SubscriptionClosed {
    rx: oneshot::Receiver<Result<(), CallError>>,
}

impl Unpin for SubscriptionClosed {}

impl Future for SubscriptionClosed {
    type Output = Result<(), CallError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        match Pin::new(&mut this.rx).poll(cx) {
            Poll::Ready(Ok(result)) => Poll::Ready(result),
            Poll::Ready(Err(_)) => Poll::Ready(Err(CallError::outcome_unknown(
                "subscription closed result channel dropped",
            ))),
            Poll::Pending => Poll::Pending,
        }
    }
}

struct SubscriptionCancel {
    shared: Arc<Shared>,
    key: PendingKey,
    priority: Priority,
    cancelled: AtomicBool,
}

impl SubscriptionCancel {
    fn new(shared: Arc<Shared>, key: PendingKey, priority: Priority) -> Self {
        Self {
            shared,
            key,
            priority,
            cancelled: AtomicBool::new(false),
        }
    }

    fn unsubscribe(&self) {
        if self.cancelled.swap(true, Ordering::AcqRel) {
            return;
        }
        self.shared
            .unsubscribe_subscription(self.key, self.priority);
    }
}

impl SubcConsumer {
    /// Connect, authenticate, and start the connection's I/O. The epoch starts at 1.
    pub async fn connect(
        connection_file: &Path,
        opts: ConsumerOptions,
    ) -> Result<Self, ConsumerError> {
        let opened = open_connection(connection_file, opts.handshake_timeout).await?;
        let shared = Arc::new(Shared::new(connection_file.to_path_buf(), opts));
        shared.install_initial(opened)?;
        Ok(Self { shared })
    }

    /// Managed unary call. Route-open failures happen before the body is sent and are
    /// classified as `NotSent`; module handler Error frames are the only `Module` errors.
    pub async fn call(
        &self,
        target: RouteTarget,
        identity: BindIdentity,
        body: Vec<u8>,
        opts: CallOptions,
    ) -> Result<Vec<u8>, CallError> {
        let call_deadline = Instant::now() + opts.timeout;
        let mut retried_unknown_channel = false;
        let consumer_identity = route_open_consumer_identity(&opts);
        let consumer_capabilities = route_open_consumer_capabilities(&opts);
        let route_key = RouteKey::new(
            &target,
            &identity,
            consumer_identity.as_ref(),
            consumer_capabilities.as_deref(),
        );

        let route_open = RouteOpenParams {
            target: &target,
            identity: &identity,
            consumer_identity: &consumer_identity,
            consumer_capabilities: &consumer_capabilities,
        };

        loop {
            let route = self
                .shared
                .ensure_route(&route_key, &route_open, &opts, call_deadline)
                .await?;
            let permit = match Arc::clone(&route.sem).acquire_owned().await {
                Ok(permit) => permit,
                Err(_) => {
                    return Err(CallError::not_sent("route flow-control semaphore closed"));
                }
            };

            if !self.shared.route_is_current(&route_key, &route) {
                drop(permit);
                self.shared
                    .sleep_until_retry(call_deadline, opts.route_retry.base)
                    .await?;
                continue;
            }

            let remaining = remaining_duration(call_deadline)?;
            let response = self
                .shared
                .send_request(
                    Some(route.generation),
                    route.channel,
                    body.clone(),
                    opts.priority,
                    remaining,
                )
                .await;
            drop(permit);

            match response {
                Ok(TerminalFrame::Response { body, .. }) => return Ok(body),
                Ok(TerminalFrame::StreamEnd) => return Ok(Vec::new()),
                // unknown_channel is the daemon ROUTER refusing an unrouted channel:
                // the request provably never reached a module, so one in-place retry
                // cannot double-execute. The cached bind is dead (module restarted;
                // its route-gone GOODBYE raced or was missed) — invalidate it so the
                // retry re-opens instead of resending into the same dead channel.
                // Parity with the TS client's retry-once in call().
                Ok(TerminalFrame::Error { body, .. })
                    if body.code == "unknown_channel"
                        && !retried_unknown_channel
                        && Instant::now() < call_deadline =>
                {
                    retried_unknown_channel = true;
                    self.shared
                        .invalidate_route(&route_key, Some(route.generation));
                    continue;
                }
                Ok(TerminalFrame::Error { body, .. }) => return Err(CallError::Module(body)),
                Err(err) if err.is_not_sent() && Instant::now() < call_deadline => {
                    self.shared
                        .invalidate_route(&route_key, Some(route.generation));
                    self.shared.ensure_connected_for_call().await?;
                    continue;
                }
                Err(err) => return Err(err),
            }
        }
    }

    /// Open a held-open subscription on a managed route.
    ///
    /// This opens or reuses the same `(target, identity, consumer_identity, consumer_capabilities)` route as
    /// [`SubcConsumer::call`], sends one Request that the provider keeps open, and
    /// returns a [`Subscription`] whose event receiver yields each matching
    /// `StreamData` payload. The request holds one route flow-control permit until
    /// `StreamEnd`, an Error frame, route teardown, connection loss, local
    /// backpressure, or [`Subscription::unsubscribe`]. Reconnects reject the
    /// subscription; callers that need durable replay should resubscribe with their
    /// own cursor after observing the failure.
    pub async fn subscribe(
        &self,
        target: RouteTarget,
        identity: BindIdentity,
        body: Vec<u8>,
        opts: SubscribeOptions,
    ) -> Result<Subscription, CallError> {
        let open_deadline = Instant::now() + opts.route_open_timeout;
        let route_opts = CallOptions {
            timeout: opts.route_open_timeout,
            priority: opts.priority,
            route_retry: opts.route_retry,
            route_retry_deadline: opts.route_retry_deadline,
            consumer_identity: opts.consumer_identity.clone(),
            consumer_capabilities: opts.consumer_capabilities.clone(),
        };
        let consumer_identity = route_open_consumer_identity(&route_opts);
        let consumer_capabilities = route_open_consumer_capabilities(&route_opts);
        let route_key = RouteKey::new(
            &target,
            &identity,
            consumer_identity.as_ref(),
            consumer_capabilities.as_deref(),
        );

        let route_open = RouteOpenParams {
            target: &target,
            identity: &identity,
            consumer_identity: &consumer_identity,
            consumer_capabilities: &consumer_capabilities,
        };

        loop {
            let route = self
                .shared
                .ensure_route(&route_key, &route_open, &route_opts, open_deadline)
                .await?;
            let permit = match Arc::clone(&route.sem).acquire_owned().await {
                Ok(permit) => permit,
                Err(_) => {
                    return Err(CallError::not_sent("route flow-control semaphore closed"));
                }
            };

            if !self.shared.route_is_current(&route_key, &route) {
                drop(permit);
                self.shared
                    .sleep_until_retry(open_deadline, opts.route_retry.base)
                    .await?;
                continue;
            }

            match self
                .shared
                .send_subscription(
                    Some(route.generation),
                    route.channel,
                    body.clone(),
                    opts.priority,
                    opts.event_buffer,
                    permit,
                )
                .await
            {
                Ok(subscription) => return Ok(subscription),
                Err(err) if err.is_not_sent() && Instant::now() < open_deadline => {
                    self.shared
                        .invalidate_route(&route_key, Some(route.generation));
                    self.shared.ensure_connected_for_call().await?;
                    continue;
                }
                Err(err) => return Err(err),
            }
        }
    }

    /// Tear down ONE route, keyed by its route-open identity tuple — the parity of the TS
    /// client's `closeRoute`. For a long-lived consumer that opens unbounded distinct
    /// routes (one per session), this releases a route on session-end without dropping
    /// the whole consumer: it drops the cached route, settles in-flight requests on it
    /// at-most-once (OutcomeUnknown if already sent, NotSent otherwise), and sends a
    /// best-effort route GOODBYE so the daemon releases it and notifies the module.
    ///
    /// Idempotent: a no-op if the route was never opened or is already closed (callers
    /// over-call on session-end). NOT a permanent tombstone — a later `call()` for the
    /// same key opens a fresh route. The close-beats-reopen guard ensures a close that
    /// races an in-flight route.open WINS (the opened channel is GOODBYE'd, not cached).
    pub async fn close_route(
        &self,
        target: RouteTarget,
        identity: BindIdentity,
        opts: CloseRouteOptions,
    ) {
        let consumer_identity = close_route_consumer_identity(&opts);
        let consumer_capabilities = close_route_consumer_capabilities(&opts);
        let key = RouteKey::new(
            &target,
            &identity,
            consumer_identity.as_ref(),
            consumer_capabilities.as_deref(),
        );
        self.shared.close_route(&key, &opts).await;
    }

    /// Current transport epoch: 1 on initial connect, then +1 per successful reconnect.
    pub fn current_epoch(&self) -> u64 {
        self.shared.lock_inner().epoch
    }

    /// Register a connection-state callback. Callbacks are best-effort observability hooks.
    pub fn on_connection_state(&self, cb: impl Fn(ConnectionState) + Send + 'static) {
        self.shared
            .lock_inner()
            .callbacks
            .push(Arc::new(Mutex::new(Box::new(cb))));
    }

    /// Close the consumer and settle every pending caller.
    pub async fn close(&self) {
        self.shared.close_sync("consumer closed");
        tokio::task::yield_now().await;
    }
}

impl Drop for SubcConsumer {
    fn drop(&mut self) {
        self.shared.close_sync("consumer dropped");
    }
}

/// Error returned by [`SubcConsumer::connect`].
#[derive(Debug)]
pub enum ConsumerError {
    ConnectionFile {
        path: PathBuf,
        source: ConnectionFileError,
    },
    NoEndpoint {
        path: PathBuf,
    },
    Connect {
        path: PathBuf,
        endpoint: String,
        source: io::Error,
    },
    Auth {
        path: PathBuf,
        endpoint: String,
        source: AuthError,
    },
    Closed,
}

impl fmt::Display for ConsumerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ConnectionFile { path, source } => write!(
                f,
                "failed to read subc connection file '{}': {source}",
                path.display()
            ),
            Self::NoEndpoint { path } => {
                write!(
                    f,
                    "subc connection file '{}' has no endpoints",
                    path.display()
                )
            }
            Self::Connect {
                path,
                endpoint,
                source,
            } => write!(
                f,
                "failed to connect to subc endpoint {endpoint} from '{}': {source}",
                path.display()
            ),
            Self::Auth {
                path,
                endpoint,
                source,
            } => write!(
                f,
                "failed to authenticate to subc endpoint {endpoint} from '{}': {source}",
                path.display()
            ),
            Self::Closed => write!(f, "consumer closed"),
        }
    }
}

impl Error for ConsumerError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ConnectionFile { source, .. } => Some(source),
            Self::Connect { source, .. } => Some(source),
            Self::Auth { source, .. } => Some(source),
            Self::NoEndpoint { .. } | Self::Closed => None,
        }
    }
}

/// Managed call or subscription failure.
#[derive(Debug)]
pub enum CallError {
    /// The request body was not accepted by the writer path, or route.open failed before data send.
    NotSent(Box<dyn Error + Send + Sync>),
    /// The request body was accepted by the writer path, but no terminal response was observed.
    OutcomeUnknown(Box<dyn Error + Send + Sync>),
    /// The target module handler returned an Error frame. Application-level rejections
    /// are returned as ordinary successful response bytes and do not produce this variant.
    Module(ErrorBody),
    /// A subscription event receiver stopped keeping up with its bounded channel.
    ///
    /// The reader task must never await a slow consumer while it is dispatching frames
    /// for the whole connection, so a full event channel terminates only that subscription.
    SubscriptionBackpressure(Box<dyn Error + Send + Sync>),
}

impl CallError {
    fn not_sent(reason: impl Into<String>) -> Self {
        Self::NotSent(Box::new(SimpleError(reason.into())))
    }

    fn outcome_unknown(reason: impl Into<String>) -> Self {
        Self::OutcomeUnknown(Box::new(SimpleError(reason.into())))
    }

    fn is_not_sent(&self) -> bool {
        matches!(self, Self::NotSent(_))
    }

    fn subscription_backpressure(reason: impl Into<String>) -> Self {
        Self::SubscriptionBackpressure(Box::new(SimpleError(reason.into())))
    }
}

impl fmt::Display for CallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotSent(err) => write!(f, "request not sent: {err}"),
            Self::OutcomeUnknown(err) => write!(f, "request outcome unknown: {err}"),
            Self::Module(body) => write!(f, "module error {}: {}", body.code, body.message),
            Self::SubscriptionBackpressure(err) => {
                write!(f, "subscription event channel backpressure: {err}")
            }
        }
    }
}

impl Error for CallError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::NotSent(err)
            | Self::OutcomeUnknown(err)
            | Self::SubscriptionBackpressure(err) => Some(err.as_ref()),
            Self::Module(_) => None,
        }
    }
}

#[derive(Debug)]
struct SimpleError(String);

impl fmt::Display for SimpleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Error for SimpleError {}

type Callback = Arc<Mutex<Box<dyn Fn(ConnectionState) + Send + 'static>>>;

type OpeningWaiter = oneshot::Sender<Result<RouteState, SharedCallFailure>>;

/// An in-flight route.open for one key: the waiters parked behind the lead opener,
/// plus a `closed` flag a concurrent `close_route` flips so the lead opener refuses
/// to install its channel (the close-beats-reopen guard). The flag lives here, with
/// the in-flight open, so it vanishes when the open finishes — no lingering per-key
/// state to leak for a long-lived consumer with unbounded distinct routes.
struct Opening {
    waiters: Vec<OpeningWaiter>,
    closed: bool,
}

struct Shared {
    connection_file: PathBuf,
    opts: ConsumerOptions,
    inner: Mutex<Inner>,
    notify: Notify,
    close_token: CancellationToken,
}

struct Inner {
    generation: u64,
    epoch: u64,
    next_corr: u64,
    writer: Option<mpsc::Sender<WriteCommand>>,
    pending: HashMap<PendingKey, PendingEntry>,
    routes: HashMap<RouteKey, RouteState>,
    openings: HashMap<RouteKey, Opening>,
    callbacks: Vec<Callback>,
    closed: bool,
    reconnecting: bool,
    restored_token: u64,
    reader_task: Option<JoinHandle<()>>,
    writer_task: Option<JoinHandle<()>>,
    reconnect_task: Option<JoinHandle<()>>,
}

impl Shared {
    fn new(connection_file: PathBuf, opts: ConsumerOptions) -> Self {
        Self {
            connection_file,
            opts,
            inner: Mutex::new(Inner {
                generation: 1,
                epoch: 1,
                next_corr: 1,
                writer: None,
                pending: HashMap::new(),
                routes: HashMap::new(),
                openings: HashMap::new(),
                callbacks: Vec::new(),
                closed: false,
                reconnecting: false,
                restored_token: 0,
                reader_task: None,
                writer_task: None,
                reconnect_task: None,
            }),
            notify: Notify::new(),
            close_token: CancellationToken::new(),
        }
    }

    fn lock_inner(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn install_initial(self: &Arc<Self>, opened: OpenedConnection) -> Result<(), ConsumerError> {
        self.install_connection(opened, InstallKind::Initial)
            .map(|_| ())
    }

    fn install_reconnected(
        self: &Arc<Self>,
        opened: OpenedConnection,
    ) -> Result<(u64, u64), ConsumerError> {
        let generation_epoch = self.install_connection(opened, InstallKind::Reconnect)?;
        self.notify.notify_waiters();
        Ok(generation_epoch)
    }

    fn install_connection(
        self: &Arc<Self>,
        opened: OpenedConnection,
        kind: InstallKind,
    ) -> Result<(u64, u64), ConsumerError> {
        if self.close_token.is_cancelled() {
            return Err(ConsumerError::Closed);
        }

        let (reader, writer) = opened.stream.into_split();
        let (tx, rx) = mpsc::channel(EGRESS_BUFFER);
        let (generation, epoch, old_reader, old_writer) = {
            let mut inner = self.lock_inner();
            if inner.closed {
                return Err(ConsumerError::Closed);
            }
            let (generation, epoch) = match kind {
                InstallKind::Initial => (inner.generation, inner.epoch),
                InstallKind::Reconnect => {
                    inner.generation = inner.generation.saturating_add(1);
                    inner.epoch = inner.epoch.saturating_add(1);
                    (inner.generation, inner.epoch)
                }
            };
            close_routes(&mut inner.routes);
            inner.writer = Some(tx);
            (
                generation,
                epoch,
                inner.reader_task.take(),
                inner.writer_task.take(),
            )
        };

        if let Some(handle) = old_reader {
            handle.abort();
        }
        if let Some(handle) = old_writer {
            handle.abort();
        }

        let reader_shared = Arc::clone(self);
        let reader_task = tokio::spawn(async move {
            reader_loop(reader_shared, reader, generation).await;
        });
        let writer_shared = Arc::clone(self);
        let writer_task = tokio::spawn(async move {
            writer_loop(writer_shared, writer, rx, generation).await;
        });

        {
            let mut inner = self.lock_inner();
            if inner.closed || inner.generation != generation {
                reader_task.abort();
                writer_task.abort();
                return Err(ConsumerError::Closed);
            }
            inner.reader_task = Some(reader_task);
            inner.writer_task = Some(writer_task);
        }

        Ok((generation, epoch))
    }

    async fn ensure_connected_for_call(self: &Arc<Self>) -> Result<(), CallError> {
        loop {
            let action = {
                let mut inner = self.lock_inner();
                if inner.closed {
                    return Err(CallError::not_sent("consumer closed"));
                }
                if inner.writer.is_some() {
                    return Ok(());
                }
                if inner.reconnecting {
                    EnsureAction::Wait
                } else {
                    inner.reconnecting = true;
                    EnsureAction::Lead
                }
            };

            match action {
                EnsureAction::Wait => self.notify.notified().await,
                EnsureAction::Lead => {
                    let result = self.reconnect_with_retry().await;
                    {
                        let mut inner = self.lock_inner();
                        inner.reconnecting = false;
                    }
                    self.notify.notify_waiters();
                    return result.map_err(|err| CallError::not_sent(err.to_string()));
                }
            }
        }
    }

    fn spawn_reconnect(self: &Arc<Self>) {
        let should_spawn = {
            let mut inner = self.lock_inner();
            if inner.closed || inner.writer.is_some() || inner.reconnecting {
                false
            } else {
                inner.reconnecting = true;
                true
            }
        };
        if !should_spawn {
            return;
        }

        let shared = Arc::clone(self);
        let handle = tokio::spawn(async move {
            let result = shared.reconnect_with_retry().await;
            {
                let mut inner = shared.lock_inner();
                inner.reconnecting = false;
            }
            shared.notify.notify_waiters();
            if let Err(err) = result {
                if !shared.close_token.is_cancelled() {
                    let _ = err;
                }
            }
        });

        let mut inner = self.lock_inner();
        if inner.closed || inner.writer.is_some() {
            handle.abort();
        } else {
            inner.reconnect_task = Some(handle);
        }
    }

    async fn reconnect_with_retry(self: &Arc<Self>) -> Result<(), ConsumerError> {
        let mut last_error: Option<ConsumerError> = None;
        for attempt in 1..=self.opts.reconnect_backoff.max_attempts {
            if self.close_token.is_cancelled() {
                return Err(ConsumerError::Closed);
            }

            match open_connection(&self.connection_file, self.opts.handshake_timeout).await {
                Ok(opened) => {
                    let (generation, epoch) = self.install_reconnected(opened)?;
                    self.schedule_restored(generation, epoch);
                    return Ok(());
                }
                Err(err) => {
                    let transient = is_reconnect_transient(&err);
                    last_error = Some(err);
                    if !transient || attempt >= self.opts.reconnect_backoff.max_attempts {
                        break;
                    }
                    let delay = self.opts.reconnect_backoff.delay_after_attempt(attempt);
                    tokio::select! {
                        () = self.close_token.cancelled() => return Err(ConsumerError::Closed),
                        () = sleep(delay) => {}
                    }
                }
            }
        }
        Err(last_error.unwrap_or(ConsumerError::Closed))
    }

    fn schedule_restored(self: &Arc<Self>, generation: u64, epoch: u64) {
        let token = {
            let mut inner = self.lock_inner();
            inner.restored_token = inner.restored_token.saturating_add(1);
            inner.restored_token
        };
        let shared = Arc::clone(self);
        tokio::spawn(async move {
            tokio::select! {
                () = shared.close_token.cancelled() => {}
                () = sleep(shared.opts.restored_debounce) => {
                    let should_emit = {
                        let inner = shared.lock_inner();
                        !inner.closed
                            && inner.generation == generation
                            && inner.epoch == epoch
                            && inner.restored_token == token
                            && inner.writer.is_some()
                    };
                    if should_emit {
                        shared.emit_connection_state(ConnectionState::Restored { epoch });
                    }
                }
            }
        });
    }

    async fn ensure_route(
        self: &Arc<Self>,
        key: &RouteKey,
        route_open: &RouteOpenParams<'_>,
        opts: &CallOptions,
        call_deadline: Instant,
    ) -> Result<RouteState, CallError> {
        loop {
            let action = {
                let mut inner = self.lock_inner();
                if inner.closed {
                    return Err(CallError::not_sent("consumer closed"));
                }
                if let Some(route) = inner.routes.get(key) {
                    if route.generation == inner.generation && inner.writer.is_some() {
                        return Ok(route.clone());
                    }
                }
                if let Some(opening) = inner.openings.get_mut(key) {
                    let (tx, rx) = oneshot::channel();
                    opening.waiters.push(tx);
                    RouteOpenAction::Wait(rx)
                } else {
                    inner.openings.insert(
                        key.clone(),
                        Opening {
                            waiters: Vec::new(),
                            closed: false,
                        },
                    );
                    RouteOpenAction::Lead
                }
            };

            match action {
                RouteOpenAction::Wait(rx) => match rx.await {
                    Ok(Ok(route)) => return Ok(route),
                    Ok(Err(err)) => return Err(err.into_call_error()),
                    Err(_) => continue,
                },
                RouteOpenAction::Lead => {
                    let mut guard = OpeningGuard::new(Arc::clone(self), key.clone());
                    let result = self
                        .open_route_with_retry(key, route_open, opts, call_deadline)
                        .await
                        .map_err(SharedCallFailure::from);
                    guard.finish(result.clone());
                    return result.map_err(SharedCallFailure::into_call_error);
                }
            }
        }
    }

    async fn open_route_with_retry(
        self: &Arc<Self>,
        key: &RouteKey,
        route_open: &RouteOpenParams<'_>,
        opts: &CallOptions,
        call_deadline: Instant,
    ) -> Result<RouteState, CallError> {
        let route_deadline = (Instant::now() + opts.route_retry_deadline).min(call_deadline);
        let mut attempt = 0usize;
        loop {
            attempt = attempt.saturating_add(1);
            self.ensure_connected_for_call().await?;
            let body = serde_json::to_vec(&ClientControlRequest::RouteOpen {
                target: route_open.target.clone(),
                identity: route_open.identity.clone(),
                consumer_identity: route_open.consumer_identity.clone(),
                consumer_capabilities: route_open.consumer_capabilities.clone(),
            })
            .map_err(|err| CallError::not_sent(format!("failed to encode route.open: {err}")))?;
            let remaining = remaining_duration(route_deadline)?;
            match self
                .send_request(None, 0, body, Priority::Interactive, remaining)
                .await
            {
                Ok(TerminalFrame::Response {
                    generation, body, ..
                }) => {
                    let response =
                        serde_json::from_slice::<ClientControlResponse>(&body).map_err(|err| {
                            CallError::not_sent(format!(
                                "failed to decode route.open response: {err}"
                            ))
                        })?;
                    let ClientControlResponse::RouteOpen { route_channel } = response else {
                        return Err(CallError::not_sent(
                            "route.open returned an unexpected control response",
                        ));
                    };
                    let route = RouteState {
                        channel: route_channel,
                        generation,
                        sem: Arc::new(Semaphore::new(DEFAULT_ROUTE_WINDOW)),
                    };
                    let install = {
                        let mut inner = self.lock_inner();
                        if inner.closed {
                            return Err(CallError::not_sent("consumer closed"));
                        }
                        // Close-beats-reopen guard: a close_route may have flipped this
                        // opening's `closed` flag WHILE this route.open was in flight. If
                        // so, close wins — do NOT cache the channel; GOODBYE it below and
                        // fail as NotSent (the route was closed before the open landed).
                        let closed_during_open = inner.openings.get(key).is_some_and(|o| o.closed);
                        if closed_during_open
                            || inner.generation != generation
                            || inner.writer.is_none()
                        {
                            RouteInstall::Discard {
                                closed: closed_during_open,
                            }
                        } else {
                            RouteInstall::Cached(
                                inner
                                    .routes
                                    .entry(key.clone())
                                    .or_insert_with(|| route.clone())
                                    .clone(),
                            )
                        }
                    };
                    match install {
                        RouteInstall::Cached(cached) => return Ok(cached),
                        RouteInstall::Discard { closed } => {
                            if closed {
                                // GOODBYE the channel we opened so the daemon/module don't
                                // leak it, then report the close as a NotSent failure.
                                self.send_route_goodbye(generation, route.channel);
                                return Err(CallError::not_sent(
                                    "route was closed before route.open completed",
                                ));
                            }
                            // Stale generation / writer gone: fall through to retry.
                        }
                    }
                    self.sleep_until_retry(route_deadline, opts.route_retry.base)
                        .await?;
                }
                Ok(TerminalFrame::Error { body, .. }) => {
                    if is_retryable_route_open_code(&body.code)
                        && attempt < opts.route_retry.max_attempts
                        && Instant::now() < route_deadline
                    {
                        let delay = opts.route_retry.delay_after_attempt(attempt);
                        self.sleep_until_retry(route_deadline, delay).await?;
                        continue;
                    }
                    return Err(CallError::not_sent(format!(
                        "route.open failed for target {}: {} ({})",
                        key.target_label(),
                        body.code,
                        body.message
                    )));
                }
                Ok(TerminalFrame::StreamEnd) => {
                    return Err(CallError::not_sent("route.open returned StreamEnd"));
                }
                Err(err)
                    if err.is_not_sent()
                        && attempt < opts.route_retry.max_attempts
                        && Instant::now() < route_deadline =>
                {
                    let delay = opts.route_retry.delay_after_attempt(attempt);
                    self.sleep_until_retry(route_deadline, delay).await?;
                }
                Err(err) => return Err(err),
            }
        }

        #[allow(unreachable_code)]
        Err(CallError::not_sent(format!(
            "route.open retry deadline elapsed for target {}",
            key.target_label()
        )))
    }

    async fn sleep_until_retry(&self, deadline: Instant, delay: Duration) -> Result<(), CallError> {
        if Instant::now() >= deadline {
            return Err(CallError::not_sent("retry deadline elapsed"));
        }
        let bounded = delay.min(deadline.saturating_duration_since(Instant::now()));
        tokio::select! {
            () = self.close_token.cancelled() => Err(CallError::not_sent("consumer closed")),
            () = sleep(bounded) => Ok(()),
        }
    }

    async fn send_request(
        self: &Arc<Self>,
        expected_generation: Option<u64>,
        channel: u16,
        body: Vec<u8>,
        priority: Priority,
        timeout: Duration,
    ) -> Result<TerminalFrame, CallError> {
        let (generation, corr, writer) = {
            let mut inner = self.lock_inner();
            if inner.closed {
                return Err(CallError::not_sent("consumer closed"));
            }
            let generation = inner.generation;
            if expected_generation.is_some_and(|expected| expected != generation) {
                return Err(CallError::not_sent("route generation is stale before send"));
            }
            let Some(writer) = inner.writer.clone() else {
                return Err(CallError::not_sent("subc connection is down before send"));
            };
            let corr = inner.next_corr;
            inner.next_corr = inner.next_corr.saturating_add(1).max(1);
            (generation, corr, writer)
        };

        let frame = Frame::build(
            FrameType::Request,
            Flags::new(false, priority, false),
            channel,
            corr,
            body,
        )
        .map_err(|err| CallError::not_sent(format!("failed to build request frame: {err}")))?;
        let key = PendingKey {
            generation,
            channel,
            corr,
        };
        let (tx, rx) = oneshot::channel();
        {
            let mut inner = self.lock_inner();
            if inner.closed || inner.generation != generation || inner.writer.is_none() {
                return Err(CallError::not_sent(
                    "connection changed before request registration",
                ));
            }
            inner.pending.insert(key, PendingEntry::unary(tx));
        }
        let mut registration = PendingRegistration::new(Arc::clone(self), key);

        if writer
            .send(WriteCommand {
                frame,
                pending: Some(key),
            })
            .await
            .is_err()
        {
            let accepted = registration.remove_pending().unwrap_or(false);
            return Err(classify_failure(
                accepted,
                "writer task closed before accepting request",
            ));
        }

        tokio::select! {
            result = rx => {
                registration.disarm();
                match result {
                    Ok(result) => result.into_call_result(),
                    Err(_) => Err(CallError::not_sent("pending response channel closed")),
                }
            }
            () = sleep(timeout) => {
                let accepted = registration.remove_pending().unwrap_or(false);
                Err(classify_failure(accepted, format!("request on channel {channel} timed out after {timeout:?}")))
            }
            () = self.close_token.cancelled() => {
                let accepted = registration.remove_pending().unwrap_or(false);
                Err(classify_failure(accepted, "consumer closed while request was pending"))
            }
        }
    }

    async fn send_subscription(
        self: &Arc<Self>,
        expected_generation: Option<u64>,
        channel: u16,
        body: Vec<u8>,
        priority: Priority,
        event_buffer: usize,
        permit: OwnedSemaphorePermit,
    ) -> Result<Subscription, CallError> {
        let (generation, corr, writer) = {
            let mut inner = self.lock_inner();
            if inner.closed {
                return Err(CallError::not_sent("consumer closed"));
            }
            let generation = inner.generation;
            if expected_generation.is_some_and(|expected| expected != generation) {
                return Err(CallError::not_sent("route generation is stale before send"));
            }
            let Some(writer) = inner.writer.clone() else {
                return Err(CallError::not_sent("subc connection is down before send"));
            };
            let corr = inner.next_corr;
            inner.next_corr = inner.next_corr.saturating_add(1).max(1);
            (generation, corr, writer)
        };

        let frame = Frame::build(
            FrameType::Request,
            Flags::new(false, priority, false),
            channel,
            corr,
            body,
        )
        .map_err(|err| CallError::not_sent(format!("failed to build request frame: {err}")))?;
        let key = PendingKey {
            generation,
            channel,
            corr,
        };
        let (events_tx, events_rx) = mpsc::channel(event_buffer.max(1));
        let (closed_tx, closed_rx) = oneshot::channel();
        {
            let mut inner = self.lock_inner();
            if inner.closed || inner.generation != generation || inner.writer.is_none() {
                return Err(CallError::not_sent(
                    "connection changed before subscription registration",
                ));
            }
            inner.pending.insert(
                key,
                PendingEntry::subscription(events_tx, closed_tx, permit, priority),
            );
        }
        let mut registration = PendingRegistration::new(Arc::clone(self), key);

        if writer
            .send(WriteCommand {
                frame,
                pending: Some(key),
            })
            .await
            .is_err()
        {
            let accepted = registration.remove_pending().unwrap_or(false);
            return Err(classify_failure(
                accepted,
                "writer task closed before accepting subscription request",
            ));
        }

        registration.disarm();
        Ok(Subscription {
            events: events_rx,
            closed: SubscriptionClosed { rx: closed_rx },
            cancel: SubscriptionCancel::new(Arc::clone(self), key, priority),
        })
    }

    fn unsubscribe_subscription(&self, key: PendingKey, priority: Priority) {
        let entry = self.lock_inner().pending.remove(&key);
        if let Some(entry) = entry {
            entry.settle_subscription_result(Ok(()));
            self.send_cancel(key.generation, key.channel, key.corr, priority);
        }
    }

    fn route_stream_data(&self, key: PendingKey, body: Vec<u8>) {
        let overflow = {
            let mut inner = self.lock_inner();
            let Some(entry) = inner.pending.get(&key) else {
                return;
            };
            match entry.try_send_stream_data(body) {
                Ok(()) | Err(StreamDataDelivery::NotSubscription) => return,
                Err(StreamDataDelivery::Full) => {
                    let priority = entry
                        .subscription_priority()
                        .unwrap_or(Priority::Interactive);
                    let entry = inner.pending.remove(&key);
                    entry.map(|entry| {
                        (
                            entry,
                            priority,
                            "subscription event channel filled; reader dropped the stream instead of blocking",
                        )
                    })
                }
                Err(StreamDataDelivery::Closed) => {
                    let priority = entry
                        .subscription_priority()
                        .unwrap_or(Priority::Interactive);
                    let entry = inner.pending.remove(&key);
                    entry.map(|entry| {
                        (
                            entry,
                            priority,
                            "subscription event receiver closed before the stream ended",
                        )
                    })
                }
            }
        };

        if let Some((entry, priority, reason)) = overflow {
            entry.settle_call_error(CallError::subscription_backpressure(reason));
            self.send_cancel(key.generation, key.channel, key.corr, priority);
        }
    }

    fn send_cancel(&self, generation: u64, channel: u16, corr: u64, priority: Priority) {
        let writer = {
            let inner = self.lock_inner();
            if inner.closed || inner.generation != generation {
                return;
            }
            inner.writer.clone()
        };
        let Some(writer) = writer else {
            return;
        };
        let Ok(frame) = Frame::build(
            FrameType::Cancel,
            Flags::new(false, priority, false),
            channel,
            corr,
            Vec::new(),
        ) else {
            return;
        };
        let _ = writer.try_send(WriteCommand {
            frame,
            pending: None,
        });
    }

    fn mark_pending_accepted(&self, key: PendingKey) -> bool {
        let mut inner = self.lock_inner();
        if inner.closed || inner.generation != key.generation {
            return false;
        }
        let Some(entry) = inner.pending.get_mut(&key) else {
            return false;
        };
        entry.accepted = true;
        true
    }

    fn settle_pending(&self, key: PendingKey, terminal: PendingTerminal) {
        let entry = self.lock_inner().pending.remove(&key);
        if let Some(entry) = entry {
            entry.settle_terminal(terminal);
        }
    }

    fn handle_generation_drop(self: &Arc<Self>, generation: u64, reason: String) {
        let (should_emit, pending, openings, callbacks) = {
            let mut inner = self.lock_inner();
            if inner.closed || inner.generation != generation || inner.writer.is_none() {
                return;
            }
            inner.writer = None;
            inner.restored_token = inner.restored_token.saturating_add(1);
            close_routes(&mut inner.routes);
            let pending = drain_pending_generation(&mut inner.pending, generation);
            let openings = drain_openings(&mut inner.openings);
            let callbacks = inner.callbacks.clone();
            (true, pending, openings, callbacks)
        };

        if should_emit {
            settle_pending_entries(pending, reason.clone());
            fail_openings(openings, SharedCallFailure::not_sent(reason.clone()));
            emit_callbacks(callbacks, ConnectionState::Dropped);
            self.notify.notify_waiters();
            self.spawn_reconnect();
        }
    }

    fn close_sync(&self, reason: &str) {
        let (pending, openings, routes, reader, writer, reconnect) = {
            let mut inner = self.lock_inner();
            if inner.closed {
                return;
            }
            inner.closed = true;
            inner.writer = None;
            self.close_token.cancel();
            (
                inner
                    .pending
                    .drain()
                    .map(|(_, entry)| entry)
                    .collect::<Vec<_>>(),
                inner
                    .openings
                    .drain()
                    .map(|(_, opening)| opening.waiters)
                    .collect::<Vec<_>>(),
                inner
                    .routes
                    .drain()
                    .map(|(_, route)| route)
                    .collect::<Vec<_>>(),
                inner.reader_task.take(),
                inner.writer_task.take(),
                inner.reconnect_task.take(),
            )
        };
        for route in routes {
            route.sem.close();
        }
        if let Some(handle) = reader {
            handle.abort();
        }
        if let Some(handle) = writer {
            handle.abort();
        }
        if let Some(handle) = reconnect {
            handle.abort();
        }
        settle_pending_entries(pending, reason.to_string());
        fail_openings(openings, SharedCallFailure::not_sent(reason.to_string()));
        self.notify.notify_waiters();
    }

    fn route_is_current(&self, key: &RouteKey, route: &RouteState) -> bool {
        let inner = self.lock_inner();
        if inner.closed || inner.generation != route.generation || inner.writer.is_none() {
            return false;
        }
        inner.routes.get(key).is_some_and(|cached| {
            cached.generation == route.generation
                && cached.channel == route.channel
                && Arc::ptr_eq(&cached.sem, &route.sem)
        })
    }

    fn invalidate_route(&self, key: &RouteKey, generation: Option<u64>) {
        let removed = {
            let mut inner = self.lock_inner();
            match inner.routes.get(key) {
                Some(route) if generation.is_none_or(|expected| expected == route.generation) => {
                    inner.routes.remove(key)
                }
                _ => None,
            }
        };
        if let Some(route) = removed {
            route.sem.close();
        }
    }

    fn finish_opening(&self, key: &RouteKey, result: Result<RouteState, SharedCallFailure>) {
        let opening = self.lock_inner().openings.remove(key);
        for waiter in opening.map(|o| o.waiters).unwrap_or_default() {
            let _ = waiter.send(result.clone());
        }
    }

    /// Tear down one route by key. See [`SubcConsumer::close_route`].
    async fn close_route(self: &Arc<Self>, key: &RouteKey, opts: &CloseRouteOptions) {
        // Under the lock: flip the close-beats-reopen flag on any in-flight open for
        // this key (so a lead-opener whose channel hasn't been cached yet refuses to
        // install it), and remove the cached route if one exists.
        let route = {
            let mut inner = self.lock_inner();
            if let Some(opening) = inner.openings.get_mut(key) {
                opening.closed = true;
            }
            inner.routes.remove(key)
        };

        // Nothing cached: either never opened (idempotent no-op) or still opening (the
        // racing lead-opener will see the flag and GOODBYE whatever channel it opens).
        let Some(route) = route else {
            return;
        };

        if opts.drain {
            // Wait for in-flight UNARY requests on this channel to settle naturally,
            // bounded by drain_timeout, before tearing the route down.
            self.drain_channel(route.generation, route.channel, opts.drain_timeout)
                .await;
        }

        // Closing the semaphore makes any not-yet-sent acquire() return Err -> the
        // caller classifies it NotSent. Already-sent pending requests are settled
        // at-most-once (OutcomeUnknown if the writer accepted their bytes).
        route.sem.close();
        self.fail_channel_pending(
            route.generation,
            route.channel,
            "route closed by close_route",
        );

        // Best-effort route GOODBYE: the daemon releases the route + relays the module
        // route-gone GOODBYE the module's reaper consumes. One-way, no ack.
        self.send_route_goodbye(route.generation, route.channel);
    }

    /// Settle every in-flight pending request on `channel` (this generation) as an
    /// at-most-once failure: OutcomeUnknown if the writer already accepted its bytes,
    /// NotSent otherwise. Mirrors the connection-drop path, scoped to one channel.
    fn fail_channel_pending(&self, generation: u64, channel: u16, reason: &str) {
        let entries = {
            let mut inner = self.lock_inner();
            drain_pending_channel(&mut inner.pending, generation, channel, true)
        };
        settle_pending_entries(entries, reason.to_string());
    }

    /// Resolve once every in-flight unary pending on `channel` has settled, or the
    /// timeout elapses. Polls the pending map (entries are removed on settle); the
    /// volume here is tiny (a route window is small) so a short poll is adequate.
    async fn drain_channel(&self, generation: u64, channel: u16, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            let has_inflight = {
                let inner = self.lock_inner();
                inner.pending.iter().any(|(key, entry)| {
                    key.generation == generation
                        && key.channel == channel
                        && !entry.is_subscription()
                })
            };
            if !has_inflight || Instant::now() >= deadline {
                return;
            }
            sleep(Duration::from_millis(5)).await;
        }
    }

    /// Send a best-effort header-only route GOODBYE for `channel` (this generation).
    /// One-way: the daemon releases the route and relays a route-gone GOODBYE to the
    /// module. Dropped silently if the connection is gone (the route died with it).
    fn send_route_goodbye(&self, generation: u64, channel: u16) {
        let writer = {
            let inner = self.lock_inner();
            if inner.closed || inner.generation != generation {
                return;
            }
            inner.writer.clone()
        };
        let Some(writer) = writer else {
            return;
        };
        let Ok(frame) = Frame::build(
            FrameType::Goodbye,
            Flags::new(false, Priority::Interactive, false),
            channel,
            0,
            Vec::new(),
        ) else {
            return;
        };
        // try_send: best-effort, never block the caller; a full egress means the
        // connection is saturated and the route is being torn down regardless.
        let _ = writer.try_send(WriteCommand {
            frame,
            pending: None,
        });
    }

    fn emit_connection_state(&self, state: ConnectionState) {
        let callbacks = self.lock_inner().callbacks.clone();
        emit_callbacks(callbacks, state);
    }
}

#[derive(Clone, Copy)]
enum InstallKind {
    Initial,
    Reconnect,
}

enum EnsureAction {
    Wait,
    Lead,
}

/// Outcome of the install decision after a route.open response arrives, taken under
/// the inner lock so a racing close_route is observed atomically.
enum RouteInstall {
    /// Install (or reuse) the cached route and return it.
    Cached(RouteState),
    /// Do not install. `closed` => a close_route won the race (GOODBYE + NotSent);
    /// otherwise the generation moved (retry the open).
    Discard { closed: bool },
}

enum RouteOpenAction {
    Wait(oneshot::Receiver<Result<RouteState, SharedCallFailure>>),
    Lead,
}

struct RouteOpenParams<'a> {
    target: &'a RouteTarget,
    identity: &'a BindIdentity,
    consumer_identity: &'a Option<ConsumerIdentity>,
    consumer_capabilities: &'a Option<Vec<String>>,
}

struct OpeningGuard {
    shared: Arc<Shared>,
    key: RouteKey,
    finished: bool,
}

impl OpeningGuard {
    fn new(shared: Arc<Shared>, key: RouteKey) -> Self {
        Self {
            shared,
            key,
            finished: false,
        }
    }

    fn finish(&mut self, result: Result<RouteState, SharedCallFailure>) {
        self.shared.finish_opening(&self.key, result);
        self.finished = true;
    }
}

impl Drop for OpeningGuard {
    fn drop(&mut self) {
        if !self.finished {
            self.shared.finish_opening(
                &self.key,
                Err(SharedCallFailure::not_sent(
                    "route.open future was cancelled",
                )),
            );
        }
    }
}

#[derive(Clone)]
struct RouteState {
    channel: u16,
    generation: u64,
    sem: Arc<Semaphore>,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct RouteKey {
    target: RouteTargetKey,
    project_root: PathBuf,
    harness: String,
    session: String,
    consumer_identity: Option<ConsumerIdentityKey>,
    consumer_capabilities: Option<ConsumerCapabilitiesKey>,
}

impl RouteKey {
    fn new(
        target: &RouteTarget,
        identity: &BindIdentity,
        consumer_identity: Option<&ConsumerIdentity>,
        consumer_capabilities: Option<&[String]>,
    ) -> Self {
        Self {
            target: RouteTargetKey::from(target),
            project_root: identity.project_root.clone(),
            harness: identity.harness.clone(),
            session: identity.session.clone(),
            consumer_identity: consumer_identity.map(ConsumerIdentityKey::from),
            consumer_capabilities: consumer_capabilities.map(ConsumerCapabilitiesKey::from_slice),
        }
    }

    fn target_label(&self) -> String {
        match &self.target {
            RouteTargetKey::ToolProvider { module_id } => format!("tool_provider:{module_id}"),
            RouteTargetKey::ManagementSurface { module_id } => {
                format!("management_surface:{module_id}")
            }
            RouteTargetKey::InternalService {
                module_id,
                service_id,
            } => format!("internal_service:{module_id}:{service_id}"),
        }
    }
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct ConsumerIdentityKey {
    module_id: String,
    launch_nonce: String,
}

impl From<&ConsumerIdentity> for ConsumerIdentityKey {
    fn from(value: &ConsumerIdentity) -> Self {
        Self {
            module_id: value.module_id.clone(),
            launch_nonce: value.launch_nonce.clone(),
        }
    }
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct ConsumerCapabilitiesKey {
    values: Vec<String>,
}

impl ConsumerCapabilitiesKey {
    fn from_slice(values: &[String]) -> Self {
        let mut values = values.to_vec();
        values.sort();
        values.dedup();
        Self { values }
    }
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
enum RouteTargetKey {
    ToolProvider {
        module_id: String,
    },
    ManagementSurface {
        module_id: String,
    },
    InternalService {
        module_id: String,
        service_id: String,
    },
}

impl From<&RouteTarget> for RouteTargetKey {
    fn from(value: &RouteTarget) -> Self {
        match value {
            RouteTarget::ToolProvider { module_id } => Self::ToolProvider {
                module_id: module_id.clone(),
            },
            RouteTarget::ManagementSurface { module_id } => Self::ManagementSurface {
                module_id: module_id.clone(),
            },
            RouteTarget::InternalService {
                module_id,
                service_id,
            } => Self::InternalService {
                module_id: module_id.clone(),
                service_id: service_id.clone(),
            },
        }
    }
}

#[derive(Debug, Clone)]
struct SharedCallFailure {
    kind: FailureKind,
    message: String,
}

impl SharedCallFailure {
    fn not_sent(message: impl Into<String>) -> Self {
        Self {
            kind: FailureKind::NotSent,
            message: message.into(),
        }
    }

    fn into_call_error(self) -> CallError {
        match self.kind {
            FailureKind::NotSent => CallError::not_sent(self.message),
            FailureKind::OutcomeUnknown => CallError::outcome_unknown(self.message),
        }
    }
}

impl From<CallError> for SharedCallFailure {
    fn from(value: CallError) -> Self {
        match value {
            CallError::NotSent(err) => Self {
                kind: FailureKind::NotSent,
                message: err.to_string(),
            },
            CallError::OutcomeUnknown(err) => Self {
                kind: FailureKind::OutcomeUnknown,
                message: err.to_string(),
            },
            CallError::Module(body) => Self {
                kind: FailureKind::OutcomeUnknown,
                message: format!(
                    "unexpected module error during route.open: {} ({})",
                    body.code, body.message
                ),
            },
            CallError::SubscriptionBackpressure(err) => Self {
                kind: FailureKind::OutcomeUnknown,
                message: err.to_string(),
            },
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum FailureKind {
    NotSent,
    OutcomeUnknown,
}

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
struct PendingKey {
    generation: u64,
    channel: u16,
    corr: u64,
}

struct PendingEntry {
    accepted: bool,
    completion: PendingCompletion,
}

enum PendingCompletion {
    Unary(oneshot::Sender<PendingResult>),
    Subscription {
        events: mpsc::Sender<Vec<u8>>,
        closed: oneshot::Sender<Result<(), CallError>>,
        _permit: OwnedSemaphorePermit,
        priority: Priority,
    },
}

enum StreamDataDelivery {
    NotSubscription,
    Full,
    Closed,
}

impl PendingEntry {
    fn unary(tx: oneshot::Sender<PendingResult>) -> Self {
        Self {
            accepted: false,
            completion: PendingCompletion::Unary(tx),
        }
    }

    fn subscription(
        events: mpsc::Sender<Vec<u8>>,
        closed: oneshot::Sender<Result<(), CallError>>,
        permit: OwnedSemaphorePermit,
        priority: Priority,
    ) -> Self {
        Self {
            accepted: false,
            completion: PendingCompletion::Subscription {
                events,
                closed,
                _permit: permit,
                priority,
            },
        }
    }

    fn is_subscription(&self) -> bool {
        matches!(&self.completion, PendingCompletion::Subscription { .. })
    }

    fn subscription_priority(&self) -> Option<Priority> {
        match &self.completion {
            PendingCompletion::Subscription { priority, .. } => Some(*priority),
            PendingCompletion::Unary(_) => None,
        }
    }

    fn try_send_stream_data(&self, body: Vec<u8>) -> Result<(), StreamDataDelivery> {
        let PendingCompletion::Subscription { events, .. } = &self.completion else {
            return Err(StreamDataDelivery::NotSubscription);
        };
        events.try_send(body).map_err(|err| match err {
            mpsc::error::TrySendError::Full(_) => StreamDataDelivery::Full,
            mpsc::error::TrySendError::Closed(_) => StreamDataDelivery::Closed,
        })
    }

    fn settle_terminal(self, terminal: PendingTerminal) {
        match self.completion {
            PendingCompletion::Unary(tx) => {
                let _ = tx.send(PendingResult::Terminal(terminal));
            }
            PendingCompletion::Subscription { closed, .. } => {
                let result = match terminal {
                    PendingTerminal::Response { .. } | PendingTerminal::StreamEnd => Ok(()),
                    PendingTerminal::Error { body } => Err(CallError::Module(body)),
                };
                let _ = closed.send(result);
            }
        }
    }

    fn settle_failure(self, reason: String) {
        let accepted = self.accepted;
        self.settle_call_error(classify_failure(accepted, reason));
    }

    fn settle_call_error(self, err: CallError) {
        match self.completion {
            PendingCompletion::Unary(tx) => {
                let _ = tx.send(PendingResult::Failure {
                    accepted: self.accepted,
                    reason: err.to_string(),
                });
            }
            PendingCompletion::Subscription { closed, .. } => {
                let _ = closed.send(Err(err));
            }
        }
    }

    fn settle_subscription_result(self, result: Result<(), CallError>) {
        match self.completion {
            PendingCompletion::Subscription { closed, .. } => {
                let _ = closed.send(result);
            }
            PendingCompletion::Unary(tx) => {
                let _ = tx.send(PendingResult::Failure {
                    accepted: self.accepted,
                    reason: "subscription cancel matched a unary request".to_string(),
                });
            }
        }
    }
}

struct PendingRegistration {
    shared: Arc<Shared>,
    key: PendingKey,
    active: bool,
}

impl PendingRegistration {
    fn new(shared: Arc<Shared>, key: PendingKey) -> Self {
        Self {
            shared,
            key,
            active: true,
        }
    }

    fn remove_pending(&mut self) -> Option<bool> {
        if !self.active {
            return None;
        }
        self.active = false;
        self.shared
            .lock_inner()
            .pending
            .remove(&self.key)
            .map(|entry| entry.accepted)
    }

    fn disarm(&mut self) {
        self.active = false;
    }
}

impl Drop for PendingRegistration {
    fn drop(&mut self) {
        let _ = self.remove_pending();
    }
}

enum PendingResult {
    Terminal(PendingTerminal),
    Failure { accepted: bool, reason: String },
}

impl PendingResult {
    fn into_call_result(self) -> Result<TerminalFrame, CallError> {
        match self {
            Self::Terminal(terminal) => Ok(terminal.into_terminal_frame()),
            Self::Failure { accepted, reason } => Err(classify_failure(accepted, reason)),
        }
    }
}

enum PendingTerminal {
    Response { generation: u64, body: Vec<u8> },
    Error { body: ErrorBody },
    StreamEnd,
}

impl PendingTerminal {
    fn into_terminal_frame(self) -> TerminalFrame {
        match self {
            Self::Response { generation, body } => TerminalFrame::Response { generation, body },
            Self::Error { body } => TerminalFrame::Error { body },
            Self::StreamEnd => TerminalFrame::StreamEnd,
        }
    }
}

enum TerminalFrame {
    Response { generation: u64, body: Vec<u8> },
    Error { body: ErrorBody },
    StreamEnd,
}

struct WriteCommand {
    frame: Frame,
    pending: Option<PendingKey>,
}

struct OpenedConnection {
    stream: TcpStream,
}

async fn open_connection(
    path: &Path,
    deadline: Duration,
) -> Result<OpenedConnection, ConsumerError> {
    let conn = connection_file::read(path).map_err(|source| ConsumerError::ConnectionFile {
        path: path.to_path_buf(),
        source,
    })?;
    let endpoint = conn
        .endpoints
        .first()
        .ok_or_else(|| ConsumerError::NoEndpoint {
            path: path.to_path_buf(),
        })?;
    let endpoint_label = format!("{}:{}", endpoint.host, endpoint.port);
    let mut stream = TcpStream::connect(&endpoint_label)
        .await
        .map_err(|source| ConsumerError::Connect {
            path: path.to_path_buf(),
            endpoint: endpoint_label.clone(),
            source,
        })?;
    authenticate_client(&mut stream, &conn, deadline)
        .await
        .map_err(|source| ConsumerError::Auth {
            path: path.to_path_buf(),
            endpoint: endpoint_label,
            source,
        })?;
    Ok(OpenedConnection { stream })
}

async fn reader_loop(shared: Arc<Shared>, mut reader: OwnedReadHalf, generation: u64) {
    loop {
        match read_frame(&mut reader).await {
            Ok(Some(frame)) => {
                if !dispatch_frame(&shared, generation, frame).await {
                    return;
                }
            }
            Ok(None) => {
                shared.handle_generation_drop(generation, "subc connection closed".to_string());
                return;
            }
            Err(err) => {
                shared.handle_generation_drop(generation, err.to_string());
                return;
            }
        }
    }
}

async fn dispatch_frame(shared: &Arc<Shared>, generation: u64, frame: Frame) -> bool {
    if !shared.generation_is_current(generation) {
        return false;
    }
    let key = PendingKey {
        generation,
        channel: frame.header.channel,
        corr: frame.header.corr,
    };
    match frame.header.ty {
        FrameType::Response => shared.settle_pending(
            key,
            PendingTerminal::Response {
                generation,
                body: frame.body,
            },
        ),
        FrameType::Error => {
            let body =
                serde_json::from_slice::<ErrorBody>(&frame.body).unwrap_or_else(|err| ErrorBody {
                    code: "invalid_error_body".to_string(),
                    message: err.to_string(),
                });
            shared.settle_pending(key, PendingTerminal::Error { body });
        }
        FrameType::StreamEnd => shared.settle_pending(key, PendingTerminal::StreamEnd),
        FrameType::StreamData => shared.route_stream_data(key, frame.body),
        FrameType::Push => {}
        FrameType::Goodbye if frame.header.channel == 0 => {
            shared.handle_generation_drop(generation, "subc sent GOODBYE".to_string());
            return false;
        }
        FrameType::Goodbye => {
            shared.invalidate_routes_for_channel(generation, frame.header.channel);
            let pending = {
                let mut inner = shared.lock_inner();
                drain_pending_channel(&mut inner.pending, generation, frame.header.channel, true)
            };
            settle_pending_entries(pending, "route closed by subc".to_string());
        }
        FrameType::Ping if frame.header.channel == 0 => {
            if let Ok(pong) = Frame::build_with_version(
                frame.header.ver,
                FrameType::Pong,
                frame.header.flags,
                0,
                frame.header.corr,
                Vec::new(),
            ) {
                let writer = shared.lock_inner().writer.clone();
                if let Some(writer) = writer {
                    let _ = writer
                        .send(WriteCommand {
                            frame: pong,
                            pending: None,
                        })
                        .await;
                }
            }
        }
        _ => {}
    }
    true
}

impl Shared {
    fn generation_is_current(&self, generation: u64) -> bool {
        let inner = self.lock_inner();
        !inner.closed && inner.generation == generation && inner.writer.is_some()
    }

    fn invalidate_routes_for_channel(&self, generation: u64, channel: u16) {
        let removed = {
            let mut inner = self.lock_inner();
            let keys = inner
                .routes
                .iter()
                .filter_map(|(key, route)| {
                    (route.generation == generation && route.channel == channel)
                        .then_some(key.clone())
                })
                .collect::<Vec<_>>();
            keys.into_iter()
                .filter_map(|key| inner.routes.remove(&key))
                .collect::<Vec<_>>()
        };
        for route in removed {
            route.sem.close();
        }
    }
}

async fn writer_loop(
    shared: Arc<Shared>,
    writer: OwnedWriteHalf,
    mut rx: mpsc::Receiver<WriteCommand>,
    generation: u64,
) {
    let mut writer = BufWriter::new(writer);
    while let Some(command) = rx.recv().await {
        if let Some(key) = command.pending {
            if !shared.mark_pending_accepted(key) {
                continue;
            }
        }
        if let Err(err) = write_one_and_flush(&mut writer, &command.frame).await {
            shared.handle_generation_drop(generation, err.to_string());
            return;
        }
        while let Ok(command) = rx.try_recv() {
            if let Some(key) = command.pending {
                if !shared.mark_pending_accepted(key) {
                    continue;
                }
            }
            if let Err(err) = write_one_and_flush(&mut writer, &command.frame).await {
                shared.handle_generation_drop(generation, err.to_string());
                return;
            }
        }
    }
}

async fn write_one_and_flush(
    writer: &mut BufWriter<OwnedWriteHalf>,
    frame: &Frame,
) -> Result<(), FrameIoError> {
    write_frame(writer, frame).await?;
    writer.flush().await.map_err(FrameIoError::Io)
}

fn route_open_consumer_identity(opts: &CallOptions) -> Option<ConsumerIdentity> {
    opts.consumer_identity
        .clone()
        .or_else(consumer_identity_from_env)
}

fn close_route_consumer_identity(opts: &CloseRouteOptions) -> Option<ConsumerIdentity> {
    opts.consumer_identity
        .clone()
        .or_else(consumer_identity_from_env)
}

fn route_open_consumer_capabilities(opts: &CallOptions) -> Option<Vec<String>> {
    opts.consumer_capabilities.clone()
}

fn close_route_consumer_capabilities(opts: &CloseRouteOptions) -> Option<Vec<String>> {
    opts.consumer_capabilities.clone()
}

fn consumer_identity_from_env() -> Option<ConsumerIdentity> {
    let module_id = std::env::var(SUBC_MODULE_ID_ENV)
        .ok()
        .filter(|value| !value.is_empty())?;
    let launch_nonce = std::env::var(SUBC_LAUNCH_NONCE_ENV)
        .ok()
        .filter(|value| !value.is_empty())?;
    Some(ConsumerIdentity {
        module_id,
        launch_nonce,
    })
}

fn classify_failure(accepted: bool, reason: impl Into<String>) -> CallError {
    if accepted {
        CallError::outcome_unknown(reason)
    } else {
        CallError::not_sent(reason)
    }
}

fn settle_pending_entries(entries: Vec<PendingEntry>, reason: String) {
    for entry in entries {
        entry.settle_failure(reason.clone());
    }
}

fn drain_pending_generation(
    pending: &mut HashMap<PendingKey, PendingEntry>,
    generation: u64,
) -> Vec<PendingEntry> {
    let keys = pending
        .keys()
        .copied()
        .filter(|key| key.generation == generation)
        .collect::<Vec<_>>();
    keys.into_iter()
        .filter_map(|key| pending.remove(&key))
        .collect()
}

fn drain_pending_channel(
    pending: &mut HashMap<PendingKey, PendingEntry>,
    generation: u64,
    channel: u16,
    include_subscriptions: bool,
) -> Vec<PendingEntry> {
    let keys = pending
        .iter()
        .filter_map(|(key, entry)| {
            (key.generation == generation
                && key.channel == channel
                && (include_subscriptions || !entry.is_subscription()))
            .then_some(*key)
        })
        .collect::<Vec<_>>();
    keys.into_iter()
        .filter_map(|key| pending.remove(&key))
        .collect()
}

fn drain_openings(openings: &mut HashMap<RouteKey, Opening>) -> Vec<Vec<OpeningWaiter>> {
    openings
        .drain()
        .map(|(_, opening)| opening.waiters)
        .collect()
}

fn fail_openings(openings: Vec<Vec<OpeningWaiter>>, failure: SharedCallFailure) {
    for waiters in openings {
        for waiter in waiters {
            let _ = waiter.send(Err(failure.clone()));
        }
    }
}

fn close_routes(routes: &mut HashMap<RouteKey, RouteState>) {
    for route in routes.drain().map(|(_, route)| route) {
        route.sem.close();
    }
}

fn emit_callbacks(callbacks: Vec<Callback>, state: ConnectionState) {
    for callback in callbacks {
        if let Ok(callback) = callback.lock() {
            callback(state.clone());
        }
    }
}

fn remaining_duration(deadline: Instant) -> Result<Duration, CallError> {
    let now = Instant::now();
    if now >= deadline {
        Err(CallError::not_sent(
            "call deadline elapsed before request was sent",
        ))
    } else {
        Ok(deadline - now)
    }
}

fn is_retryable_route_open_code(code: &str) -> bool {
    matches!(
        code,
        "unknown_module" | "module_reloading" | "target_unavailable" | "module_timeout"
    )
}

fn is_reconnect_transient(err: &ConsumerError) -> bool {
    match err {
        ConsumerError::Connect { source, .. } => matches!(
            source.kind(),
            io::ErrorKind::ConnectionRefused
                | io::ErrorKind::ConnectionReset
                | io::ErrorKind::ConnectionAborted
                | io::ErrorKind::TimedOut
                | io::ErrorKind::NotConnected
                | io::ErrorKind::AddrNotAvailable
        ),
        ConsumerError::ConnectionFile { source, .. } => match source {
            ConnectionFileError::Io { source, .. } => source.kind() == io::ErrorKind::NotFound,
            _ => false,
        },
        // Auth failure is transient during reconnect: the daemon rotates its key
        // on every restart, and with a fixed port a client racing the restart can
        // read the pre-rotation connection file yet still connect — the proof
        // mismatch then means "stale key mid-rotation", not "impostor". Each
        // retry re-reads the connection file (open_connection), so the next
        // attempt picks up the rotated key, and server-proves-first protects
        // every attempt. First-connect auth failures stay permanent: connect()
        // surfaces them directly without entering the reconnect classifier.
        ConsumerError::Auth { .. } => true,
        ConsumerError::NoEndpoint { .. } | ConsumerError::Closed => false,
    }
}

impl From<FrameBuildError> for CallError {
    fn from(err: FrameBuildError) -> Self {
        Self::not_sent(err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconnect_classifier_treats_auth_failure_as_transient() {
        // Key rotation across a daemon restart on the fixed port: a client racing
        // the restart reads the pre-rotation file, connects, and fails the proof.
        // That must be retryable — each retry re-reads the file, so the next
        // attempt picks up the rotated key. Treating it as a permanent impostor
        // verdict would turn every daemon restart into a permanent client wedge.
        let auth = ConsumerError::Auth {
            path: PathBuf::from("/tmp/subc-connection.json"),
            endpoint: "127.0.0.1:8757".to_string(),
            source: subc_transport::AuthError::InvalidServerProof,
        };
        assert!(is_reconnect_transient(&auth), "rotation race must retry");

        // Absent file mid-restart stays transient; malformed file stays permanent.
        let absent = ConsumerError::ConnectionFile {
            path: PathBuf::from("/tmp/subc-connection.json"),
            source: ConnectionFileError::Io {
                op: "read",
                path: PathBuf::from("/tmp/subc-connection.json"),
                source: io::Error::new(io::ErrorKind::NotFound, "gone"),
            },
        };
        assert!(is_reconnect_transient(&absent));
    }

    #[test]
    fn retryable_route_open_codes_are_code_specific() {
        for code in [
            "unknown_module",
            "module_reloading",
            "target_unavailable",
            "module_timeout",
        ] {
            assert!(is_retryable_route_open_code(code), "{code} should retry");
        }
        assert!(!is_retryable_route_open_code("invalid_project_root"));
        assert!(!is_retryable_route_open_code("route_rejected"));
    }

    #[tokio::test]
    async fn close_route_flips_inflight_opening_so_a_racing_open_discards() {
        // The load-bearing close-beats-reopen guard, in isolation: a close that lands
        // while a route.open is in flight (channel not yet cached) must flip the
        // opening's `closed` flag, so the lead opener re-checks it before installing and
        // GOODBYEs the channel it opened instead of caching it.
        let shared = Arc::new(Shared::new(
            PathBuf::from("/tmp/does-not-exist"),
            ConsumerOptions::default(),
        ));
        let key = RouteKey::new(
            &RouteTarget::ToolProvider {
                module_id: "m".into(),
            },
            &BindIdentity {
                project_root: PathBuf::from("/tmp/p"),
                harness: "h".into(),
                session: "s".into(),
            },
            None,
            None,
        );
        // Simulate an in-flight lead open: an openings entry exists, not yet closed,
        // with no cached route (channel hasn't been installed yet).
        shared.lock_inner().openings.insert(
            key.clone(),
            Opening {
                waiters: Vec::new(),
                closed: false,
            },
        );

        // close_route with no cached route is an idempotent no-op on routes, but MUST
        // flip the in-flight opening's flag so the racing open discards.
        shared
            .close_route(&key, &CloseRouteOptions::default())
            .await;
        assert!(
            shared
                .lock_inner()
                .openings
                .get(&key)
                .is_some_and(|o| o.closed),
            "close_route must flip the in-flight opening's closed flag (close-beats-reopen)"
        );

        // And closing a key with neither a route nor an in-flight open is a no-op.
        let absent = RouteKey::new(
            &RouteTarget::ToolProvider {
                module_id: "absent".into(),
            },
            &BindIdentity {
                project_root: PathBuf::from("/tmp/p"),
                harness: "h".into(),
                session: "s".into(),
            },
            None,
            None,
        );
        shared
            .close_route(&absent, &CloseRouteOptions::default())
            .await;
    }

    #[test]
    fn route_key_is_structured() {
        let target = RouteTarget::InternalService {
            module_id: "a\0b".into(),
            service_id: "svc".into(),
        };
        let identity = BindIdentity {
            project_root: PathBuf::from("/tmp/project"),
            harness: "h".into(),
            session: "s".into(),
        };
        let key = RouteKey::new(&target, &identity, None, None);
        assert_eq!(key.project_root, PathBuf::from("/tmp/project"));
        assert!(matches!(key.target, RouteTargetKey::InternalService { .. }));
    }

    #[test]
    fn route_key_canonicalizes_consumer_capabilities() {
        let target = RouteTarget::ToolProvider {
            module_id: "aft".into(),
        };
        let identity = BindIdentity {
            project_root: PathBuf::from("/tmp/project"),
            harness: "h".into(),
            session: "s".into(),
        };
        let left = RouteKey::new(
            &target,
            &identity,
            None,
            Some(&["sampling".to_string(), "elicitation".to_string()]),
        );
        let right = RouteKey::new(
            &target,
            &identity,
            None,
            Some(&[
                "elicitation".to_string(),
                "sampling".to_string(),
                "sampling".to_string(),
            ]),
        );
        assert_eq!(left, right);
    }

    #[test]
    fn drain_pending_channel_can_skip_subscriptions() {
        let mut pending = HashMap::new();
        let generation = 7;
        let channel = 11;
        let unary_key = PendingKey {
            generation,
            channel,
            corr: 1,
        };
        let subscription_key = PendingKey {
            generation,
            channel,
            corr: 2,
        };
        let (unary_tx, _unary_rx) = oneshot::channel();
        pending.insert(unary_key, PendingEntry::unary(unary_tx));

        let (events_tx, _events_rx) = mpsc::channel(1);
        let (closed_tx, _closed_rx) = oneshot::channel();
        let permit = Arc::new(Semaphore::new(1))
            .try_acquire_owned()
            .expect("test semaphore permit should be available");
        pending.insert(
            subscription_key,
            PendingEntry::subscription(events_tx, closed_tx, permit, Priority::Interactive),
        );

        let drained = drain_pending_channel(&mut pending, generation, channel, false);
        assert_eq!(drained.len(), 1);
        assert!(pending.contains_key(&subscription_key));

        let drained = drain_pending_channel(&mut pending, generation, channel, true);
        assert_eq!(drained.len(), 1);
        assert!(pending.is_empty());
    }
}
