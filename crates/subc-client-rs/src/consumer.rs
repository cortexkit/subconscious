use std::{
    collections::{BTreeSet, HashMap},
    error::Error,
    fmt,
    future::Future,
    io,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex, MutexGuard, OnceLock,
    },
    task::{Context, Poll},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use subc_control::{
    CatalogEntry, ClientControlRequest, ClientControlResponse, ConsumerIdentity, PollKind,
};
/// The spawn snapshot types, re-exported so a caller of
/// [`SubcConsumer::spawn_snapshot`] needs no direct `subc-control` dependency.
pub use subc_control::{LiveSpawn, SpawnCursor, SpawnSnapshot};
use subc_protocol::{
    error_codes, manifest::is_valid_capability_identifier, AdmissionClass, BindIdentity, ErrorBody,
    Flags, Frame, FrameBuildError, FrameType, Priority, RouteTarget, SUBC_LAUNCH_NONCE_ENV,
    SUBC_MODULE_ID_ENV,
};

use crate::RouteHandle;
use subc_transport::{
    authenticate_client, connection_file, read_frame, write_frame, AuthError, ConnectionFileError,
    FrameIoError,
};
use tokio::{
    io::{AsyncWrite, AsyncWriteExt, BufWriter},
    net::{tcp::OwnedReadHalf, TcpStream},
    sync::{mpsc, oneshot, Notify, OwnedSemaphorePermit, Semaphore},
    task::JoinHandle,
    time::{sleep, sleep_until, timeout_at, Instant},
};
use tokio_util::sync::CancellationToken;

const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);
pub const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(30);
// Sized against the daemon's route.bind relay timeout (12s default): one
// load-stalled bind relay burns ~12s before the daemon rejects with
// module_timeout, so a 10s budget could be exhausted by a SINGLE slow bind.
// 30s leaves room for ~2 full relay waits plus backoff (still clamped by the
// overall call timeout below).
pub const DEFAULT_ROUTE_RETRY_DEADLINE: Duration = Duration::from_secs(30);
const DEFAULT_RESTORED_DEBOUNCE: Duration = Duration::from_millis(250);
pub const DEFAULT_LIVENESS_PROBE_WINDOW: Duration = Duration::from_secs(2);
const EGRESS_BUFFER: usize = 128;
const DEFAULT_ROUTE_WINDOW: usize = 1024;
const DEFAULT_SUBSCRIPTION_EVENT_BUFFER: usize = 128;
const DEFAULT_PUSH_EVENT_BUFFER: usize = 128;
const REVERSE_REQUEST_UNHANDLED: &str = "reverse_request_unhandled";

type ReverseRequestFuture =
    Pin<Box<dyn Future<Output = Result<Vec<u8>, ReverseRequestError>> + Send + 'static>>;
type ReverseRequestHandler =
    Arc<dyn Fn(Vec<u8>, ReverseRequestContext) -> ReverseRequestFuture + Send + Sync + 'static>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReverseRequestContext {
    pub corr: u64,
    pub method: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReverseRequestError {
    message: String,
}

impl ReverseRequestError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for ReverseRequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for ReverseRequestError {}

#[derive(Default)]
struct ReverseRequestRegistryState {
    handlers: HashMap<String, ReverseRequestHandler>,
    declared_families: Option<BTreeSet<String>>,
}

/// Handler registry prepared before route.open. Its method families are the
/// route's derived consumer_capabilities declaration.
#[derive(Clone, Default)]
pub struct ReverseRequestRegistry {
    inner: Arc<Mutex<ReverseRequestRegistryState>>,
}

impl fmt::Debug for ReverseRequestRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReverseRequestRegistry")
            .field("capabilities", &self.capabilities())
            .finish()
    }
}

impl PartialEq for ReverseRequestRegistry {
    fn eq(&self, other: &Self) -> bool {
        self.capabilities() == other.capabilities()
    }
}

impl Eq for ReverseRequestRegistry {}

impl ReverseRequestRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn on_request<F, Fut>(
        &self,
        method_family: impl Into<String>,
        handler: F,
    ) -> Result<(), ReverseRequestRegistrationError>
    where
        F: Fn(Vec<u8>, ReverseRequestContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Vec<u8>> + Send + 'static,
    {
        self.on_request_fallible(method_family, move |body, ctx| {
            let future = handler(body, ctx);
            async move { Ok(future.await) }
        })
    }

    pub fn on_request_fallible<F, Fut>(
        &self,
        method_family: impl Into<String>,
        handler: F,
    ) -> Result<(), ReverseRequestRegistrationError>
    where
        F: Fn(Vec<u8>, ReverseRequestContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Vec<u8>, ReverseRequestError>> + Send + 'static,
    {
        let method_family = method_family.into();
        if !is_valid_method_family(&method_family) {
            return Err(ReverseRequestRegistrationError::InvalidFamily(
                method_family,
            ));
        }
        let mut state = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state
            .declared_families
            .as_ref()
            .is_some_and(|families| !families.contains(&method_family))
        {
            return Err(ReverseRequestRegistrationError::NotDeclared(method_family));
        }
        state.handlers.insert(
            method_family,
            Arc::new(move |body, ctx| Box::pin(handler(body, ctx))),
        );
        Ok(())
    }

    fn capabilities(&self) -> Vec<String> {
        let state = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut capabilities = state.handlers.keys().cloned().collect::<Vec<_>>();
        capabilities.sort();
        capabilities
    }

    fn handler(&self, method_family: &str) -> Option<ReverseRequestHandler> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .handlers
            .get(method_family)
            .cloned()
    }

    pub(crate) fn seal(&self) {
        let mut state = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.declared_families.is_none() {
            state.declared_families = Some(state.handlers.keys().cloned().collect());
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReverseRequestRegistrationError {
    InvalidFamily(String),
    NotDeclared(String),
    NotConsumerRoute,
}

impl fmt::Display for ReverseRequestRegistrationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidFamily(family) => {
                write!(f, "invalid reverse-request method family {family:?}")
            }
            Self::NotDeclared(family) => write!(
                f,
                "reverse-request capability {family:?} was not registered before route.open"
            ),
            Self::NotConsumerRoute => {
                f.write_str("route handle does not belong to a consumer route")
            }
        }
    }
}

impl Error for ReverseRequestRegistrationError {}

fn is_valid_method_family(value: &str) -> bool {
    let mut segments = value.split(['.', '_', '-']);
    segments.next().is_some_and(|segment| {
        segment
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_lowercase)
            && segment
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    }) && segments.all(|segment| {
        !segment.is_empty()
            && segment
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    })
}

/// Reverse-request handlers live in a process-global map keyed by an id the
/// `RouteHandle` carries, rather than inside the handle itself.
///
/// WHY, so nobody "simplifies" this into the handle later: `RouteHandle` is
/// `Copy`, deliberately and publicly. Every route-scoped call in this SDK takes
/// it by value, and the fleet's Rust consumers pass it around freely on that
/// assumption. An `Arc<ReverseRequestRegistry>` field would make it non-`Copy`
/// and break every one of those call sites across twelve repositories, for a
/// lane most of them do not use.
///
/// The cost is real and bounded: entries are removed on every teardown path
/// (`remove_route`, `remove_route_by_handle`, `drain_routes`,
/// `uninstall_route_handle`, `install_ingress_handle`), so a consumer that opens
/// and closes routes for its lifetime does not grow. A consumer DROPPED without
/// closing leaks its remaining entries until process exit, which is the one gap
/// and is acceptable because nothing else about a dropped consumer is reclaimed
/// either.
///
/// If `RouteHandle` ever stops being `Copy` for another reason, move this into
/// the handle and delete the map — the indirection exists only to preserve that
/// property.
static NEXT_REVERSE_REQUEST_REGISTRY_ID: AtomicU64 = AtomicU64::new(1);
static REVERSE_REQUEST_REGISTRIES: OnceLock<Mutex<HashMap<u64, ReverseRequestRegistry>>> =
    OnceLock::new();

fn reverse_request_registries() -> &'static Mutex<HashMap<u64, ReverseRequestRegistry>> {
    REVERSE_REQUEST_REGISTRIES.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn install_reverse_request_registry(registry: ReverseRequestRegistry) -> u64 {
    let id = NEXT_REVERSE_REQUEST_REGISTRY_ID
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
        .expect("reverse-request route registry id exhausted");
    reverse_request_registries()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(id, registry);
    id
}

pub(crate) fn route_reverse_request_registry(
    id: u64,
) -> Result<ReverseRequestRegistry, ReverseRequestRegistrationError> {
    if id == 0 {
        return Err(ReverseRequestRegistrationError::NotConsumerRoute);
    }
    reverse_request_registries()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&id)
        .cloned()
        .ok_or(ReverseRequestRegistrationError::NotConsumerRoute)
}

fn release_reverse_request_registry(handle: RouteHandle) {
    let id = handle.reverse_request_registry_id();
    if id != 0 {
        reverse_request_registries()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&id);
    }
}

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
    /// Deadline for channel-0 calls that do not take per-call options.
    pub call_timeout: Duration,
    pub reconnect_backoff: RetryBackoff,
    pub restored_debounce: Duration,
    /// Window a post-deadline Ping waits for any inbound frame before the connection is
    /// treated as half-open. Exposed so callers can use a shorter deterministic test window.
    pub liveness_probe_window: Duration,
}

impl Default for ConsumerOptions {
    fn default() -> Self {
        Self {
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
            call_timeout: DEFAULT_CALL_TIMEOUT,
            reconnect_backoff: RetryBackoff::default(),
            restored_debounce: DEFAULT_RESTORED_DEBOUNCE,
            liveness_probe_window: DEFAULT_LIVENESS_PROBE_WINDOW,
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
    /// The registry whose derived capability set identifies the route being closed.
    pub reverse_requests: ReverseRequestRegistry,
}

impl Default for CloseRouteOptions {
    fn default() -> Self {
        Self {
            drain: false,
            drain_timeout: DEFAULT_CALL_TIMEOUT,
            consumer_identity: None,
            reverse_requests: ReverseRequestRegistry::new(),
        }
    }
}

/// Per-call options for [`SubcConsumer::call`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallOptions {
    /// Deadline for the whole managed call, including route-open retry and the response wait.
    pub timeout: Duration,
    pub priority: Priority,
    /// Admission behavior stamped into the request frame. Defaults to NORMAL.
    pub admission_class: AdmissionClass,
    pub route_retry: RetryBackoff,
    /// Maximum real-time limit for retrying route.open attempts when the target is temporarily absent.
    pub route_retry_deadline: Duration,
    /// Explicit consumer identity for route.open; when absent, non-empty SUBC_MODULE_ID and SUBC_LAUNCH_NONCE environment variables are used.
    pub consumer_identity: Option<ConsumerIdentity>,
    /// Reverse-request handlers for this route. consumer_capabilities is derived
    /// from the registered method families and omitted when this registry is empty.
    pub reverse_requests: ReverseRequestRegistry,
}

impl Default for CallOptions {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_CALL_TIMEOUT,
            priority: Priority::Interactive,
            admission_class: AdmissionClass::Normal,
            route_retry: RetryBackoff::default(),
            route_retry_deadline: DEFAULT_ROUTE_RETRY_DEADLINE,
            consumer_identity: None,
            reverse_requests: ReverseRequestRegistry::new(),
        }
    }
}

/// Options for [`SubcConsumer::subscribe`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubscribeOptions {
    pub priority: Priority,
    /// Admission behavior stamped into the subscription request. Defaults to NORMAL.
    pub admission_class: AdmissionClass,
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
    /// Reverse-request handlers for this route. consumer_capabilities is derived
    /// from the registered method families and omitted when this registry is empty.
    pub reverse_requests: ReverseRequestRegistry,
}

impl Default for SubscribeOptions {
    fn default() -> Self {
        Self {
            priority: Priority::Interactive,
            admission_class: AdmissionClass::Normal,
            event_buffer: DEFAULT_SUBSCRIPTION_EVENT_BUFFER,
            route_retry: RetryBackoff::default(),
            route_retry_deadline: DEFAULT_ROUTE_RETRY_DEADLINE,
            route_open_timeout: DEFAULT_CALL_TIMEOUT,
            consumer_identity: None,
            reverse_requests: ReverseRequestRegistry::new(),
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

/// Result of a route-scoped status or liveness poll.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutePollResult {
    pub handle: RouteHandle,
    pub status: Option<String>,
    pub live: Option<bool>,
}

/// Typed response from the daemon's channel-0 `catalog.list` operation.
///
/// Each module entry exposes the provider roles and tool definitions advertised by
/// that module.
#[derive(Debug, Clone, serde::Deserialize, PartialEq)]
pub struct CatalogList {
    pub generation: u64,
    #[serde(default)]
    pub modules: Vec<CatalogEntry>,
    #[serde(default)]
    pub subc_ops: Vec<String>,
}

/// A provider-originated push delivered on one live route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushEvent {
    /// The connection-fenced route identity on which the push arrived.
    pub handle: RouteHandle,
    /// Opaque payload bytes sent by the provider.
    pub body: Vec<u8>,
}

/// Disposition of one provider push at delivery time, so the two drop causes
/// land on their own counters (issue #40): the remedies differ.
enum DroppedPush {
    Delivered,
    NoReceiver,
    ReceiverFull,
}

/// A parsed daemon-originated channel-0 control push.
#[derive(Debug, Clone)]
pub struct ControlPush {
    /// The push discriminator, e.g. `route.closing` / `route.closed`.
    pub op: String,
    /// The full parsed body, `op` included, for op-specific fields.
    pub body: serde_json::Value,
}

/// A route-close reason accepted from the daemon control-push wire.
///
/// The protocol may add reasons before this SDK is upgraded. Preserve that fact
/// as [`Self::Unknown`] and classify it conservatively rather than refusing to
/// deliver the enclosing control push.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteCloseReason {
    Reload,
    Restart,
    Disable,
    Crash,
    CapabilityDenied,
    Unknown(String),
}

/// Whether a closed route may be reopened automatically from its reason alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteCloseDisposition {
    MayReopen,
    MustNotReopen,
}

impl RouteCloseReason {
    /// Decode a wire reason without making a new daemon reason fatal to push delivery.
    pub fn from_wire(reason: &str) -> Self {
        match reason {
            "reload" => Self::Reload,
            "restart" => Self::Restart,
            "disable" => Self::Disable,
            "crash" => Self::Crash,
            "capability_denied" => Self::CapabilityDenied,
            other => Self::Unknown(other.to_string()),
        }
    }

    /// Unknown reasons take the strictest action: never reopen on their behalf.
    pub fn disposition(&self) -> RouteCloseDisposition {
        match self {
            Self::Reload | Self::Restart => RouteCloseDisposition::MayReopen,
            Self::Disable | Self::Crash | Self::CapabilityDenied | Self::Unknown(_) => {
                RouteCloseDisposition::MustNotReopen
            }
        }
    }
}

impl ControlPush {
    /// Decode the close reason from a route lifecycle push, if this is one.
    pub fn route_close_reason(&self) -> Option<RouteCloseReason> {
        matches!(self.op.as_str(), "route.closing" | "route.closed")
            .then(|| {
                self.body
                    .get("reason")?
                    .as_str()
                    .map(RouteCloseReason::from_wire)
            })
            .flatten()
    }
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
    pub fn unsubscribe(&self) -> Result<(), CallError> {
        self.cancel.unsubscribe()
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        let _ = self.cancel.unsubscribe();
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

    fn unsubscribe(&self) -> Result<(), CallError> {
        let handle = RouteHandle::new(self.key.channel, self.key.epoch, self.key.generation);
        self.shared.validate_current_handle(handle)?;
        if self.cancelled.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        self.shared
            .unsubscribe_subscription(self.key, self.priority)
    }
}

impl SubcConsumer {
    /// Connect through an explicit connection-file path. The initial connection
    /// generation uses epoch 1; each reconnect advances it.
    pub async fn connect(
        connection_file: &Path,
        opts: ConsumerOptions,
    ) -> Result<Self, ConsumerError> {
        let opened = open_connection(connection_file, opts.handshake_timeout).await?;
        let shared = Arc::new(Shared::new(connection_file.to_path_buf(), opts));
        shared.install_initial(opened)?;
        Ok(Self { shared })
    }

    /// Discover the daemon's connection file, authenticate, and start the I/O loop.
    pub async fn connect_default(opts: ConsumerOptions) -> Result<Self, ConsumerError> {
        let discovered = connection_file::discover(None)
            .map_err(|source| ConsumerError::Discovery { source })?;
        let opened =
            open_connection_with_info(&discovered.path, &discovered.info, opts.handshake_timeout)
                .await?;
        let shared = Arc::new(Shared::new(discovered.path, opts));
        shared.install_initial(opened)?;
        Ok(Self { shared })
    }

    /// Open or reuse a managed route and return its connection-fenced handle.
    pub async fn open_route(
        &self,
        target: RouteTarget,
        identity: BindIdentity,
        opts: CallOptions,
    ) -> Result<RouteHandle, CallError> {
        let deadline = Instant::now() + opts.timeout;
        let consumer_identity = route_open_consumer_identity(&opts);
        let consumer_capabilities = route_open_consumer_capabilities(&opts);
        let key = RouteKey::new(
            &target,
            &identity,
            consumer_identity.as_ref(),
            consumer_capabilities.as_deref(),
        );
        let params = RouteOpenParams {
            target: &target,
            identity: &identity,
            consumer_identity: &consumer_identity,
            consumer_capabilities: &consumer_capabilities,
            reverse_requests: &opts.reverse_requests,
        };
        self.shared
            .ensure_route(&key, &params, &opts, deadline)
            .await
            .map(|route| route.handle)
    }

    /// Open one admitted route without entering the managed route cache.
    ///
    /// Admitted routes are never cached or reopened after a connection drop. If
    /// this call fails or the route later closes, the caller must perform admission
    /// again and call this method with fresh facts.
    ///
    /// The route declares no reverse-request capabilities, so the provider cannot
    /// send it requests. Use [`Self::open_route_with_admission_facts_and_options`]
    /// when the provider asks the consumer questions on the route (elicitation).
    pub async fn open_route_with_admission_facts(
        &self,
        target: RouteTarget,
        identity: BindIdentity,
        facts: serde_json::Value,
    ) -> Result<RouteHandle, CallError> {
        let deadline = Instant::now() + self.shared.opts.call_timeout;
        self.open_admitted_route(
            target,
            identity,
            facts,
            &CallOptions::default(),
            None,
            deadline,
        )
        .await
    }

    /// Open one admitted route, with the reverse-request handlers and timeout in
    /// `opts`.
    ///
    /// The route declares the capabilities of the handlers registered on
    /// `opts.reverse_requests`, and requests the provider sends back on it
    /// (elicitation, sampling) reach those handlers, exactly as on a managed
    /// route. The handlers must be registered before this call: the registry is
    /// sealed when the route opens, because the capabilities it declares are what
    /// the provider was told.
    ///
    /// Like [`Self::open_route_with_admission_facts`], the route is never cached
    /// or reopened; after a failure or a close the caller performs admission again.
    pub async fn open_route_with_admission_facts_and_options(
        &self,
        target: RouteTarget,
        identity: BindIdentity,
        facts: serde_json::Value,
        opts: CallOptions,
    ) -> Result<RouteHandle, CallError> {
        let deadline = Instant::now() + opts.timeout;
        let reverse_requests = opts.reverse_requests.clone();
        self.open_admitted_route(
            target,
            identity,
            facts,
            &opts,
            Some(reverse_requests),
            deadline,
        )
        .await
    }

    /// One admitted route.open. With `reverse_requests`, the route is opened the
    /// way the managed path opens one: the capabilities are declared, the socket
    /// reader installs the handle before the open resolves (so a request the
    /// provider sends right after binding is not lost), and the handle carries
    /// the registry. Without it, the route has no reverse-request handling.
    async fn open_admitted_route(
        &self,
        target: RouteTarget,
        identity: BindIdentity,
        facts: serde_json::Value,
        opts: &CallOptions,
        reverse_requests: Option<ReverseRequestRegistry>,
        deadline: Instant,
    ) -> Result<RouteHandle, CallError> {
        let consumer_capabilities = if reverse_requests.is_some() {
            route_open_consumer_capabilities(opts)
        } else {
            None
        };
        let target_label = route_target_label(&target);
        let body = serde_json::to_vec(&ClientControlRequest::RouteOpen {
            target,
            identity,
            consumer_identity: route_open_consumer_identity(opts),
            consumer_capabilities,
            admission_facts: Some(facts),
        })
        .map_err(|err| CallError::not_sent(format!("failed to encode route.open: {err}")))?;

        let terminal = self
            .shared
            .control_call(body, deadline, true, reverse_requests.clone())
            .await?;
        let (generation, body) = match terminal {
            TerminalFrame::Response {
                generation, body, ..
            } => (generation, body),
            // The daemon refused the open. Keep its code and detail, as the
            // plain route.open does, so a caller can tell "retry shortly"
            // (module_warming) from "this configuration will never work"
            // (admission_facts_not_permitted).
            TerminalFrame::Error { body, .. } => {
                return Err(CallError::route_open_refused(target_label, body));
            }
            _ => {
                return Err(CallError::not_sent(
                    "route.open returned a non-response frame",
                ));
            }
        };
        let ClientControlResponse::RouteOpen {
            route_channel,
            route_epoch,
        } = serde_json::from_slice(&body).map_err(|err| {
            CallError::not_sent(format!("failed to decode route.open response: {err}"))
        })?
        else {
            return Err(CallError::not_sent(
                "route.open returned an unexpected control response",
            ));
        };
        // Adopt the handle the socket reader installed for this response, which
        // carries the registry passed to control_call; build one only if the
        // reader did not (the connection moved on before the response landed).
        let handle = self
            .shared
            .ingress_handle(generation, route_channel, route_epoch)
            .unwrap_or_else(|| match reverse_requests {
                Some(reverse_requests) => RouteHandle::new_consumer(
                    route_channel,
                    route_epoch,
                    generation,
                    reverse_requests,
                ),
                None => RouteHandle::new(route_channel, route_epoch, generation),
            });
        let route = RouteState {
            handle,
            sem: Arc::new(Semaphore::new(DEFAULT_ROUTE_WINDOW)),
        };
        self.shared.install_one_shot_route(route.clone())?;
        Ok(route.handle)
    }

    /// Fetch the daemon's module catalog over channel 0.
    pub async fn catalog_list(&self) -> Result<CatalogList, CallError> {
        let deadline = Instant::now() + self.shared.opts.call_timeout;
        let body = serde_json::to_vec(&serde_json::json!({
            "op": subc_control::ops::CATALOG_LIST,
        }))
        .map_err(|err| CallError::not_sent(format!("failed to encode catalog.list: {err}")))?;

        loop {
            match self
                .shared
                .control_call(body.clone(), deadline, false, None)
                .await
            {
                Ok(TerminalFrame::Response { body, .. }) => {
                    let response =
                        serde_json::from_slice::<ClientControlResponse>(&body).map_err(|err| {
                            CallError::not_sent(format!(
                                "failed to decode catalog.list response: {err}"
                            ))
                        })?;
                    let ClientControlResponse::CatalogList {
                        generation,
                        modules,
                        subc_ops,
                    } = response
                    else {
                        return Err(CallError::not_sent(
                            "catalog.list returned an unexpected control response",
                        ));
                    };
                    return Ok(CatalogList {
                        generation,
                        modules,
                        subc_ops,
                    });
                }
                Ok(TerminalFrame::Error { body, .. }) => return Err(CallError::Module(body)),
                Ok(TerminalFrame::StreamEnd) => {
                    return Err(CallError::not_sent("catalog.list returned StreamEnd"));
                }
                Err(err)
                    if is_retryable_catalog_transport_error(&err) && Instant::now() < deadline =>
                {
                    continue;
                }
                Err(err) => return Err(err),
            }
        }
    }

    /// Fetch the supervisor's atomic spawn snapshot (`supervisor.spawn_snapshot`)
    /// over channel 0: the live processes with their spawn generations, and the
    /// cursor at which they were observed.
    ///
    /// A daemon refusal comes back as [`CallError::Module`], so its code is readable
    /// through [`CallError::code`]. Transport failures are retried until the
    /// consumer's call deadline, as for [`SubcConsumer::catalog_list`].
    pub async fn spawn_snapshot(&self) -> Result<SpawnSnapshot, CallError> {
        let deadline = Instant::now() + self.shared.opts.call_timeout;
        let body = serde_json::to_vec(&ClientControlRequest::SupervisorSpawnSnapshot {})
            .map_err(|err| {
                CallError::not_sent(format!("failed to encode supervisor.spawn_snapshot: {err}"))
            })?;

        loop {
            match self
                .shared
                .control_call(body.clone(), deadline, false, None)
                .await
            {
                Ok(TerminalFrame::Response { body, .. }) => {
                    let response =
                        serde_json::from_slice::<ClientControlResponse>(&body).map_err(|err| {
                            CallError::not_sent(format!(
                                "failed to decode supervisor.spawn_snapshot response: {err}"
                            ))
                        })?;
                    let ClientControlResponse::SupervisorSpawnSnapshot { snapshot } = response
                    else {
                        return Err(CallError::not_sent(
                            "supervisor.spawn_snapshot returned an unexpected control response",
                        ));
                    };
                    return Ok(snapshot);
                }
                Ok(TerminalFrame::Error { body, .. }) => return Err(CallError::Module(body)),
                Ok(TerminalFrame::StreamEnd) => {
                    return Err(CallError::not_sent(
                        "supervisor.spawn_snapshot returned StreamEnd",
                    ));
                }
                Err(err)
                    if is_retryable_catalog_transport_error(&err) && Instant::now() < deadline =>
                {
                    continue;
                }
                Err(err) => return Err(err),
            }
        }
    }

    /// Resolve the sole catalog claimant for a capability.
    ///
    /// Resolution is deliberately based only on the static capabilities mirror in
    /// `catalog.list`; module ids and role names are not fallback claims. Calling
    /// this method expresses singular intent, so a plural catalog result is an
    /// explicit ambiguity rather than an arbitrary choice.
    pub async fn resolve_provider(&self, capability: &str) -> Result<String, CallError> {
        let claimants = self.resolve_providers(capability).await?;
        match claimants.as_slice() {
            [] => Err(CallError::CapabilityUnprovided {
                capability: capability.to_string(),
            }),
            [claimant] => Ok(claimant.clone()),
            _ => Err(CallError::CapabilityAmbiguous {
                capability: capability.to_string(),
                claimants,
            }),
        }
    }

    /// Resolve every catalog claimant for a capability in module-id order.
    ///
    /// The identifier is validated before any channel-0 request is made, so a
    /// typo cannot become a network-dependent "unprovided" result.
    pub async fn resolve_providers(&self, capability: &str) -> Result<Vec<String>, CallError> {
        validate_capability_for_resolution(capability)?;
        let catalog = self.catalog_list().await?;
        Ok(capability_claimants(&catalog, capability))
    }

    /// Send one request using an already-opened route handle.
    pub async fn request(
        &self,
        handle: &RouteHandle,
        body: Vec<u8>,
        opts: CallOptions,
    ) -> Result<Vec<u8>, CallError> {
        let deadline = Instant::now() + opts.timeout;
        let route = self.shared.route_state(*handle)?;
        let permit = match timeout_at(deadline, Arc::clone(&route.sem).acquire_owned()).await {
            Ok(Ok(permit)) => permit,
            Ok(Err(_)) => return Err(CallError::StaleRouteHandle(*handle)),
            Err(_) => {
                return Err(CallError::not_sent(
                    "call deadline elapsed waiting for route flow-control",
                ))
            }
        };
        let result = self
            .shared
            .send_request(RequestSend {
                expected_handle: Some(*handle),
                channel: handle.channel,
                epoch: handle.epoch,
                body,
                priority: opts.priority,
                admission_class: opts.admission_class,
                deadline,
                retain_late_route_open: false,
                route_open_reverse_requests: None,
            })
            .await;
        drop(permit);
        match result? {
            TerminalFrame::Response { body, .. } => Ok(body),
            TerminalFrame::StreamEnd => Ok(Vec::new()),
            TerminalFrame::Error { body, .. } => Err(CallError::Module(body)),
        }
    }

    /// Start a held-open request using an already-opened route handle.
    pub async fn subscribe_route(
        &self,
        handle: &RouteHandle,
        body: Vec<u8>,
        opts: SubscribeOptions,
    ) -> Result<Subscription, CallError> {
        let deadline = Instant::now() + opts.route_open_timeout;
        let route = self.shared.route_state(*handle)?;
        let permit = match timeout_at(deadline, Arc::clone(&route.sem).acquire_owned()).await {
            Ok(Ok(permit)) => permit,
            Ok(Err(_)) => return Err(CallError::StaleRouteHandle(*handle)),
            Err(_) => {
                return Err(CallError::not_sent(
                    "subscription deadline elapsed waiting for route flow-control",
                ))
            }
        };
        self.shared
            .send_subscription(SubscriptionSend {
                expected_handle: Some(*handle),
                channel: handle.channel,
                epoch: handle.epoch,
                body,
                priority: opts.priority,
                admission_class: opts.admission_class,
                event_buffer: opts.event_buffer,
                deadline,
                permit,
            })
            .await
    }

    /// Poll status or liveness for exactly this route handle.
    pub async fn poll_route(
        &self,
        handle: &RouteHandle,
        kind: PollKind,
        timeout: Duration,
    ) -> Result<RoutePollResult, CallError> {
        let deadline = Instant::now() + timeout;
        let body = serde_json::to_vec(&ClientControlRequest::RoutePoll {
            route_channel: handle.channel,
            route_epoch: handle.epoch,
            kind,
        })
        .map_err(|err| CallError::not_sent(format!("failed to encode route.poll: {err}")))?;
        let terminal = self
            .shared
            .send_request(RequestSend {
                expected_handle: Some(*handle),
                channel: 0,
                epoch: 0,
                body,
                priority: Priority::Interactive,
                admission_class: AdmissionClass::Normal,
                deadline,
                retain_late_route_open: false,
                route_open_reverse_requests: None,
            })
            .await?;
        let TerminalFrame::Response { body, .. } = terminal else {
            return Err(CallError::not_sent(
                "route.poll returned a non-response frame",
            ));
        };
        let ClientControlResponse::RoutePoll {
            route_channel,
            route_epoch,
            status,
            live,
        } = serde_json::from_slice(&body)
            .map_err(|err| CallError::not_sent(format!("failed to decode route.poll: {err}")))?
        else {
            return Err(CallError::not_sent(
                "route.poll returned an unexpected control response",
            ));
        };
        if route_channel != handle.channel || route_epoch != handle.epoch {
            return Err(CallError::not_sent(
                "route.poll response echoed a different route handle",
            ));
        }
        self.shared.validate_current_handle(*handle)?;
        Ok(RoutePollResult {
            handle: *handle,
            status,
            live,
        })
    }

    /// Locally observed count of unknown or stale route frames dropped by layer-2 validation.
    pub fn dropped_route_frames(&self) -> u64 {
        self.shared.lock_inner().dropped_route_frames
    }

    /// Register a receiver for provider-originated Push frames on exactly one live route.
    ///
    /// Registering another receiver for the same route replaces and closes the prior receiver.
    /// The receiver closes when its route closes or the connection drops. A FULL buffer drops
    /// the overflowing push and counts it on `pushes_dropped_receiver_full` while the
    /// subscription survives (issue #40; push is a lossy latency optimization and polling
    /// remains the correctness backstop). The reader never waits for an application that is
    /// not draining pushes.
    pub fn push_events(
        &self,
        handle: &RouteHandle,
    ) -> Result<mpsc::Receiver<PushEvent>, CallError> {
        self.shared.register_push_events(*handle)
    }

    /// Number of Push frames dropped because their live route has no active receiver.
    ///
    /// Push is a one-way latency optimization, not a durable feed: the client does not
    /// acknowledge it, and callers retain polling as their correctness backstop. Counting
    /// intentional default-path drops makes an application that has not opted in observable.
    pub fn pushes_dropped_no_receiver(&self) -> u64 {
        self.shared
            .pushes_dropped_no_receiver
            .load(Ordering::Relaxed)
    }

    /// Pushes dropped because the registered receiver's bounded buffer was full.
    /// The subscription SURVIVES a burst (the receiver stays registered); this
    /// counter is the trace the burst leaves. Distinct from
    /// `pushes_dropped_no_receiver` because the remedies differ: full means
    /// drain faster or register with more capacity, no-receiver means nobody
    /// subscribed.
    pub fn pushes_dropped_receiver_full(&self) -> u64 {
        self.shared
            .pushes_dropped_receiver_full
            .load(Ordering::Relaxed)
    }

    /// Register the consumer-level receiver for daemon-originated channel-0
    /// control pushes (`route.closing`, `route.closed`, and any op added
    /// later). Advisory by contract: GOODBYE remains the load-bearing
    /// route-death signal and nothing in the client's own lifecycle consumes
    /// these. Unrecognized ops are DELIVERED (the must-ignore choice belongs
    /// to the consumer); unparseable bodies are dropped and counted. The
    /// receiver survives reconnects. Registering again replaces the prior
    /// receiver; a full or closed receiver drops the push and counts it
    /// rather than blocking the reader.
    pub fn control_pushes(&self, capacity: usize) -> mpsc::Receiver<ControlPush> {
        let (sender, receiver) = mpsc::channel(capacity.max(1));
        self.shared.lock_inner().control_push_receiver = Some(sender);
        receiver
    }

    /// Always-present count of dropped channel-0 control pushes (no receiver
    /// registered, receiver full or closed, or unparseable body). Emitted as a
    /// counter rather than silence so an application that has not opted in is
    /// observable, mirroring `pushes_dropped_no_receiver`.
    pub fn control_pushes_dropped(&self) -> u64 {
        self.shared.control_pushes_dropped.load(Ordering::Relaxed)
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
            reverse_requests: &opts.reverse_requests,
        };

        loop {
            let route = self
                .shared
                .ensure_route(&route_key, &route_open, &opts, call_deadline)
                .await
                .map_err(request_not_sent_after_route_open_failure)?;
            let permit =
                match timeout_at(call_deadline, Arc::clone(&route.sem).acquire_owned()).await {
                    Ok(Ok(permit)) => permit,
                    Ok(Err(_)) => {
                        return Err(CallError::not_sent("route flow-control semaphore closed"));
                    }
                    Err(_) => {
                        return Err(CallError::not_sent(
                            "call deadline elapsed waiting for route flow-control",
                        ));
                    }
                };

            if !self.shared.route_is_current(&route_key, &route) {
                drop(permit);
                self.shared
                    .sleep_until_retry(call_deadline, opts.route_retry.base)
                    .await?;
                continue;
            }

            let response = self
                .shared
                .send_request(RequestSend {
                    expected_handle: Some(route.handle),
                    channel: route.handle.channel,
                    epoch: route.handle.epoch,
                    body: body.clone(),
                    priority: opts.priority,
                    admission_class: opts.admission_class,
                    deadline: call_deadline,
                    retain_late_route_open: false,
                    route_open_reverse_requests: None,
                })
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
                // stale_route_epoch is the same class with a sharper cause (issue
                // #39): channel known, epoch released mid-flight. Its documented
                // contract is NOT-FORWARDED (dropped before delivery), so the retry
                // is safe by construction; the remedy is identical.
                // Parity with the TS client's retry-once in call().
                Ok(TerminalFrame::Error { body, flags })
                    if error_codes::is_established_route_dead(flags, &body.code)
                        && !retried_unknown_channel
                        && Instant::now() < call_deadline =>
                {
                    retried_unknown_channel = true;
                    self.shared.invalidate_route(&route_key, Some(route.handle));
                    continue;
                }
                Ok(TerminalFrame::Error { body, .. }) => return Err(CallError::Module(body)),
                Err(err) if err.is_not_sent() && Instant::now() < call_deadline => {
                    self.shared.invalidate_route(&route_key, Some(route.handle));
                    self.shared.ensure_connected_for_call(call_deadline).await?;
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
            admission_class: opts.admission_class,
            route_retry: opts.route_retry,
            route_retry_deadline: opts.route_retry_deadline,
            consumer_identity: opts.consumer_identity.clone(),
            reverse_requests: opts.reverse_requests.clone(),
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
            reverse_requests: &opts.reverse_requests,
        };

        loop {
            let route = self
                .shared
                .ensure_route(&route_key, &route_open, &route_opts, open_deadline)
                .await
                .map_err(request_not_sent_after_route_open_failure)?;
            let permit =
                match timeout_at(open_deadline, Arc::clone(&route.sem).acquire_owned()).await {
                    Ok(Ok(permit)) => permit,
                    Ok(Err(_)) => {
                        return Err(CallError::not_sent("route flow-control semaphore closed"));
                    }
                    Err(_) => {
                        return Err(CallError::not_sent(
                            "subscription open deadline elapsed waiting for route flow-control",
                        ));
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
                .send_subscription(SubscriptionSend {
                    expected_handle: Some(route.handle),
                    channel: route.handle.channel,
                    epoch: route.handle.epoch,
                    body: body.clone(),
                    priority: opts.priority,
                    admission_class: opts.admission_class,
                    event_buffer: opts.event_buffer,
                    deadline: open_deadline,
                    permit,
                })
                .await
            {
                Ok(subscription) => return Ok(subscription),
                Err(err) if err.is_not_sent() && Instant::now() < open_deadline => {
                    self.shared.invalidate_route(&route_key, Some(route.handle));
                    self.shared.ensure_connected_for_call(open_deadline).await?;
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

    /// Close exactly this route handle. A stale connection token fails locally and emits no frame.
    pub async fn close_handle(
        &self,
        handle: &RouteHandle,
        opts: CloseRouteOptions,
    ) -> Result<(), CallError> {
        self.shared.close_handle(*handle, &opts).await
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

/// Error returned by [`SubcConsumer::connect`] or [`SubcConsumer::connect_default`].
#[derive(Debug)]
pub enum ConsumerError {
    Discovery {
        source: connection_file::DiscoveryError,
    },
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
            Self::Discovery { source } => source.fmt(f),
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
            Self::Discovery { source } => Some(source),
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
    /// The handle belongs to an earlier connection and no frame was emitted.
    StaleRouteHandle(RouteHandle),
    /// No registered catalog entry claims the requested capability.
    CapabilityUnprovided { capability: String },
    /// More than one registered catalog entry claims a singularly requested capability.
    CapabilityAmbiguous {
        capability: String,
        claimants: Vec<String>,
    },
    /// The resolver rejected a malformed capability identifier before querying the daemon.
    InvalidCapabilityIdentifier { capability: String },
}

impl CallError {
    /// Return the stable machine-readable code for typed errors.
    pub fn code(&self) -> Option<&str> {
        match self {
            Self::Module(body) => Some(&body.code),
            Self::CapabilityUnprovided { .. } => Some("capability_unprovided"),
            Self::CapabilityAmbiguous { .. } => Some("capability_ambiguous"),
            Self::InvalidCapabilityIdentifier { .. } => Some("invalid_capability_identifier"),
            Self::NotSent(_)
            | Self::OutcomeUnknown(_)
            | Self::SubscriptionBackpressure(_)
            | Self::StaleRouteHandle(_) => None,
        }
    }

    fn not_sent(reason: impl Into<String>) -> Self {
        Self::NotSent(Box::new(SimpleError(reason.into())))
    }

    fn outcome_unknown(reason: impl Into<String>) -> Self {
        Self::OutcomeUnknown(Box::new(SimpleError(reason.into())))
    }

    fn is_not_sent(&self) -> bool {
        matches!(self, Self::NotSent(_))
    }

    /// The daemon's refusal body when this failure is a route.open the daemon
    /// refused, with its `code`, `message` and machine-readable `detail`
    /// intact, for example `module_warming` with
    /// `detail.reason = "required_capability_unprovided"`.
    ///
    /// When retryable refusals ran out the route-retry deadline, this is the
    /// LAST refusal seen, so a caller can tell "the target stayed warming for
    /// the whole window" from "the connection failed". The failure is still
    /// `NotSent`: a refused route.open never delivered the request.
    pub fn route_open_refusal(&self) -> Option<&ErrorBody> {
        match self {
            Self::NotSent(err) => err
                .downcast_ref::<RouteOpenRefused>()
                .map(|refused| &refused.body),
            _ => None,
        }
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
            Self::StaleRouteHandle(handle) => write!(f, "stale route handle: {handle:?}"),
            Self::CapabilityUnprovided { capability } => {
                write!(
                    f,
                    "capability_unprovided: no catalog claimant for {capability}"
                )
            }
            Self::CapabilityAmbiguous {
                capability,
                claimants,
            } => write!(
                f,
                "capability_ambiguous: multiple catalog claimants for {capability}: {claimants:?}"
            ),
            Self::InvalidCapabilityIdentifier { capability } => write!(
                f,
                "invalid_capability_identifier: malformed capability identifier {capability:?}"
            ),
        }
    }
}

impl Error for CallError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::NotSent(err)
            | Self::OutcomeUnknown(err)
            | Self::SubscriptionBackpressure(err) => Some(err.as_ref()),
            Self::Module(_)
            | Self::StaleRouteHandle(_)
            | Self::CapabilityUnprovided { .. }
            | Self::CapabilityAmbiguous { .. }
            | Self::InvalidCapabilityIdentifier { .. } => None,
        }
    }
}

/// A route.open the daemon refused, kept typed inside [`CallError::NotSent`]
/// so the refusal's code and `detail` survive. Read it through
/// [`CallError::route_open_refusal`].
#[derive(Debug)]
struct RouteOpenRefused {
    target: String,
    body: ErrorBody,
}

impl fmt::Display for RouteOpenRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "route.open failed for target {}: {} ({})",
            self.target, self.body.code, self.body.message
        )
    }
}

impl Error for RouteOpenRefused {}

/// The label a route.open refusal names its target by, the same spelling the
/// managed route cache uses.
fn route_target_label(target: &RouteTarget) -> String {
    match target {
        RouteTarget::ToolProvider { module_id } => format!("tool_provider:{module_id}"),
        RouteTarget::ManagementSurface { module_id } => format!("management_surface:{module_id}"),
        RouteTarget::InternalService {
            module_id,
            service_id,
        } => format!("internal_service:{module_id}:{service_id}"),
    }
}

impl CallError {
    fn route_open_refused(target: String, body: ErrorBody) -> Self {
        Self::NotSent(Box::new(RouteOpenRefused { target, body }))
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
    #[cfg(test)]
    before_wait: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    close_token: CancellationToken,
    /// Epoch milliseconds of the last frame dispatched from the current connection.
    /// The liveness probe reads this to distinguish a healthy slow request from a
    /// socket that has silently stopped delivering inbound traffic.
    last_inbound_ms: AtomicU64,
    /// Only one deadline-triggered liveness probe may be active across generations.
    liveness_probe_running: AtomicBool,
    pushes_dropped_no_receiver: AtomicU64,
    /// Always-present drop counter for channel-0 control pushes (no receiver,
    /// receiver full/closed, or unparseable body). Before this surface existed
    /// the drop was SILENT -- not even a counter -- which is how #31's push
    /// family shipped unreachable to every Rust consumer (issue #35).
    control_pushes_dropped: AtomicU64,
    /// Pushes dropped because a registered receiver's bounded buffer was full
    /// at delivery time. Distinct from `pushes_dropped_no_receiver` because the
    /// operator remedies differ: full means the consumer is too slow (widen the
    /// buffer or drain faster), no-receiver means it never subscribed.
    pushes_dropped_receiver_full: AtomicU64,
}

struct Inner {
    generation: u64,
    epoch: u64,
    next_corr: Option<u64>,
    writer: Option<mpsc::Sender<WriteCommand>>,
    pending: HashMap<PendingKey, PendingEntry>,
    routes: HashMap<RouteKey, RouteState>,
    route_by_channel: HashMap<u16, RouteKey>,
    one_shot_routes: HashMap<u16, RouteState>,
    route_epochs: HashMap<u16, RouteHandle>,
    push_event_receivers: HashMap<RouteHandle, mpsc::Sender<PushEvent>>,
    /// Consumer-level receiver for daemon-originated channel-0 control pushes
    /// (`route.closing`, `route.closed`, future ops). Connection-independent:
    /// control pushes are advisory daemon events, so the receiver survives
    /// reconnects rather than being keyed by generation.
    control_push_receiver: Option<mpsc::Sender<ControlPush>>,
    dropped_route_frames: u64,
    openings: HashMap<RouteKey, Opening>,
    callbacks: Vec<Callback>,
    closed: bool,
    reconnect: ReconnectState,
    restored_token: u64,
    reader_task: Option<JoinHandle<()>>,
    writer_task: Option<JoinHandle<()>>,
}

impl Inner {
    fn cache_route(&mut self, key: RouteKey, route: RouteState) -> RouteState {
        let cached = self.routes.entry(key.clone()).or_insert(route).clone();
        let previous = self
            .route_by_channel
            .insert(cached.handle.channel, key.clone());
        debug_assert!(previous.as_ref().is_none_or(|previous| previous == &key));
        self.route_epochs
            .insert(cached.handle.channel, cached.handle);
        cached
    }

    fn remove_route(&mut self, key: &RouteKey) -> Option<RouteState> {
        let route = self.routes.remove(key)?;
        let indexed = self.route_by_channel.remove(&route.handle.channel);
        debug_assert_eq!(indexed.as_ref(), Some(key));
        release_reverse_request_registry(route.handle);
        Some(route)
    }

    fn remove_route_by_handle(&mut self, handle: RouteHandle) -> Option<RouteState> {
        if let Some(key) = self.route_by_channel.get(&handle.channel).cloned() {
            let matches = self
                .routes
                .get(&key)
                .is_some_and(|route| route.handle == handle);
            debug_assert!(matches);
            if matches {
                return self.remove_route(&key);
            }
        }
        let route = self
            .one_shot_routes
            .get(&handle.channel)
            .is_some_and(|route| route.handle == handle)
            .then(|| self.one_shot_routes.remove(&handle.channel))
            .flatten();
        if let Some(route) = &route {
            release_reverse_request_registry(route.handle);
        }
        route
    }

    fn drain_routes(&mut self) -> Vec<RouteState> {
        self.route_by_channel.clear();
        let routes = self
            .routes
            .drain()
            .map(|(_, route)| route)
            .chain(self.one_shot_routes.drain().map(|(_, route)| route))
            .collect::<Vec<_>>();
        for route in &routes {
            release_reverse_request_registry(route.handle);
        }
        routes
    }

    fn close_routes(&mut self) {
        self.push_event_receivers.clear();
        for route in self.drain_routes() {
            route.sem.close();
        }
    }
}

impl Shared {
    fn new(connection_file: PathBuf, opts: ConsumerOptions) -> Self {
        Self {
            connection_file,
            opts,
            inner: Mutex::new(Inner {
                generation: 1,
                epoch: 1,
                next_corr: Some(1),
                writer: None,
                pending: HashMap::new(),
                routes: HashMap::new(),
                route_by_channel: HashMap::new(),
                one_shot_routes: HashMap::new(),
                route_epochs: HashMap::new(),
                push_event_receivers: HashMap::new(),
                control_push_receiver: None,
                dropped_route_frames: 0,
                openings: HashMap::new(),
                callbacks: Vec::new(),
                closed: false,
                reconnect: ReconnectState::Idle,
                restored_token: 0,
                reader_task: None,
                writer_task: None,
            }),
            notify: Notify::new(),
            #[cfg(test)]
            before_wait: Mutex::new(None),
            close_token: CancellationToken::new(),
            last_inbound_ms: AtomicU64::new(0),
            liveness_probe_running: AtomicBool::new(false),
            pushes_dropped_no_receiver: AtomicU64::new(0),
            control_pushes_dropped: AtomicU64::new(0),
            pushes_dropped_receiver_full: AtomicU64::new(0),
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
                    inner.generation = inner
                        .generation
                        .checked_add(1)
                        .ok_or(ConsumerError::Closed)?;
                    inner.epoch = inner.epoch.checked_add(1).ok_or(ConsumerError::Closed)?;
                    (inner.generation, inner.epoch)
                }
            };
            inner.close_routes();
            inner.route_epochs.clear();
            inner.next_corr = Some(1);
            inner.writer = Some(tx);
            self.last_inbound_ms.store(0, Ordering::Release);
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

    async fn ensure_connected_for_call(
        self: &Arc<Self>,
        deadline: Instant,
    ) -> Result<(), CallError> {
        loop {
            if Instant::now() >= deadline {
                return Err(CallError::not_sent(
                    "call deadline elapsed waiting for reconnection",
                ));
            }
            // Created BEFORE the state is read, and that ordering is the fix.
            // Reconnect completion wakes waiters with `notify_waiters()`, which
            // reaches only `Notified` futures that already exist and stores no
            // permit. A future created after the lock is released misses a
            // reconnect that finishes in between, and the caller then sleeps
            // until its deadline on a healthy connection. (`enable()` is kept
            // for `notify_one` semantics; for `notify_waiters` creation alone
            // is what registers the waiter.)
            let notified = self.notify.notified();
            tokio::pin!(notified);
            let action = {
                let mut inner = self.lock_inner();
                notified.as_mut().enable();
                if inner.closed {
                    return Err(CallError::not_sent("consumer closed"));
                }
                if inner.writer.is_some() {
                    return Ok(());
                }
                let generation = inner.generation;
                let reconnect_is_live = match &inner.reconnect {
                    ReconnectState::Idle => false,
                    ReconnectState::Inline { generation: active } => *active == generation,
                    ReconnectState::Background {
                        generation: active,
                        task,
                    } => *active == generation && !task.is_finished(),
                };
                if reconnect_is_live {
                    EnsureAction::Wait
                } else {
                    let stale_task =
                        match std::mem::replace(&mut inner.reconnect, ReconnectState::Idle) {
                            ReconnectState::Background { task, .. } => Some(task),
                            ReconnectState::Idle | ReconnectState::Inline { .. } => None,
                        };
                    inner.reconnect = ReconnectState::Inline { generation };
                    EnsureAction::Lead {
                        generation,
                        stale_task,
                    }
                }
            };

            match action {
                EnsureAction::Wait => {
                    #[cfg(test)]
                    {
                        let hook = self.before_wait.lock().unwrap().take();
                        if let Some(hook) = hook {
                            hook();
                        }
                    }
                    timeout_at(deadline, notified).await.map_err(|_| {
                        CallError::not_sent("call deadline elapsed waiting for reconnection")
                    })?;
                }
                EnsureAction::Lead {
                    generation,
                    stale_task,
                } => {
                    if let Some(handle) = stale_task {
                        handle.abort();
                    }
                    let mut guard = InlineReconnectGuard::new(Arc::clone(self), generation);
                    let result = timeout_at(deadline, self.reconnect_with_retry(generation)).await;
                    guard.finish();
                    return match result {
                        Ok(Ok(())) => Ok(()),
                        Ok(Err(err)) => Err(CallError::not_sent(err.to_string())),
                        Err(_) => Err(CallError::not_sent(
                            "call deadline elapsed waiting for reconnection",
                        )),
                    };
                }
            }
        }
    }

    fn spawn_reconnect(self: &Arc<Self>, generation: u64) -> bool {
        // Reconnect ownership is fenced by the dropped transport generation. A
        // newer drop replaces an older attempt instead of letting that attempt
        // block recovery for the newer transport.
        let stale_task = {
            let mut inner = self.lock_inner();
            if inner.closed || inner.writer.is_some() || inner.generation != generation {
                return false;
            }
            let should_spawn = match &inner.reconnect {
                ReconnectState::Idle => true,
                ReconnectState::Inline { generation: active } => *active < generation,
                ReconnectState::Background {
                    generation: active,
                    task,
                } => *active < generation || (*active == generation && task.is_finished()),
            };
            if !should_spawn {
                return false;
            }

            let stale_task = match std::mem::replace(&mut inner.reconnect, ReconnectState::Idle) {
                ReconnectState::Background { task, .. } => Some(task),
                ReconnectState::Idle | ReconnectState::Inline { .. } => None,
            };
            let shared = Arc::clone(self);
            let handle = tokio::spawn(async move {
                let _result = shared.reconnect_with_retry(generation).await;
                shared.finish_background_reconnect(generation);
            });
            inner.reconnect = ReconnectState::Background {
                generation,
                task: handle,
            };
            stale_task
        };
        if let Some(handle) = stale_task {
            handle.abort();
        }
        true
    }

    async fn reconnect_with_retry(
        self: &Arc<Self>,
        reconnect_generation: u64,
    ) -> Result<(), ConsumerError> {
        let mut last_error: Option<ConsumerError> = None;
        for attempt in 1..=self.opts.reconnect_backoff.max_attempts {
            if self.close_token.is_cancelled() {
                return Err(ConsumerError::Closed);
            }
            if !self.reconnect_attempt_is_current(reconnect_generation) {
                return Ok(());
            }

            match open_connection(&self.connection_file, self.opts.handshake_timeout).await {
                Ok(opened) => {
                    // A newer drop may have installed its own attempt while this
                    // connection was opening. Do not let the stale attempt replace it.
                    if !self.reconnect_attempt_is_current(reconnect_generation) {
                        return Ok(());
                    }
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

    fn reconnect_attempt_is_current(&self, generation: u64) -> bool {
        let inner = self.lock_inner();
        !inner.closed
            && inner.writer.is_none()
            && inner.generation == generation
            && matches!(
                &inner.reconnect,
                ReconnectState::Inline { generation: active }
                    | ReconnectState::Background {
                        generation: active,
                        ..
                    } if *active == generation
            )
    }

    fn finish_inline_reconnect(&self, generation: u64) {
        let finished = {
            let mut inner = self.lock_inner();
            if matches!(
                &inner.reconnect,
                ReconnectState::Inline { generation: active } if *active == generation
            ) {
                inner.reconnect = ReconnectState::Idle;
                true
            } else {
                false
            }
        };
        if finished {
            self.notify.notify_waiters();
        }
    }

    fn finish_background_reconnect(&self, generation: u64) {
        let completed_task = {
            let mut inner = self.lock_inner();
            if !matches!(
                &inner.reconnect,
                ReconnectState::Background { generation: active, .. } if *active == generation
            ) {
                None
            } else {
                match std::mem::replace(&mut inner.reconnect, ReconnectState::Idle) {
                    ReconnectState::Background { task, .. } => Some(task),
                    ReconnectState::Idle | ReconnectState::Inline { .. } => unreachable!(),
                }
            }
        };
        if completed_task.is_some() {
            drop(completed_task);
            self.notify.notify_waiters();
        }
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

    fn install_one_shot_route(&self, route: RouteState) -> Result<(), CallError> {
        let handle = route.handle;
        let mut inner = self.lock_inner();
        if inner.closed || inner.generation != handle.connection_token() || inner.writer.is_none() {
            return Err(CallError::StaleRouteHandle(handle));
        }
        // The socket reader installs the handle for a route.open response before
        // the waiter resolves (so a request the provider sends right after the
        // bind is routed), so this route's own channel is normally present
        // already, holding exactly this handle. Only a different handle on the
        // channel is a collision.
        if inner
            .route_epochs
            .get(&handle.channel)
            .is_some_and(|existing| *existing != handle)
        {
            return Err(CallError::not_sent(
                "daemon returned a route channel already in use",
            ));
        }
        inner.one_shot_routes.insert(handle.channel, route);
        inner.route_epochs.insert(handle.channel, handle);
        Ok(())
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
                    if route.handle.connection_token() == inner.generation && inner.writer.is_some()
                    {
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
                RouteOpenAction::Wait(rx) => match timeout_at(call_deadline, rx).await {
                    Ok(Ok(Ok(route))) => return Ok(route),
                    Ok(Ok(Err(err))) => return Err(err.into_call_error()),
                    Ok(Err(_)) => continue,
                    Err(_) => {
                        return Err(CallError::not_sent(
                            "call deadline elapsed waiting for route.open",
                        ));
                    }
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
        // The most recent retryable refusal. When the deadline ends the retries,
        // the caller gets this refusal rather than a bare "deadline elapsed", so
        // "the target stayed warming the whole time" stays distinguishable from
        // "the connection failed".
        let mut last_refusal: Option<ErrorBody> = None;
        let expired =
            |err: CallError, last_refusal: &mut Option<ErrorBody>| match last_refusal.take() {
                Some(body) if err.is_not_sent() && Instant::now() >= route_deadline => {
                    CallError::route_open_refused(key.target_label(), body)
                }
                _ => err,
            };
        loop {
            attempt = attempt.saturating_add(1);
            let body = serde_json::to_vec(&ClientControlRequest::RouteOpen {
                target: route_open.target.clone(),
                identity: route_open.identity.clone(),
                consumer_identity: route_open.consumer_identity.clone(),
                consumer_capabilities: route_open.consumer_capabilities.clone(),
                admission_facts: None,
            })
            .map_err(|err| CallError::not_sent(format!("failed to encode route.open: {err}")))?;
            match self
                .control_call(
                    body,
                    route_deadline,
                    true,
                    Some(route_open.reverse_requests.clone()),
                )
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
                    let ClientControlResponse::RouteOpen {
                        route_channel,
                        route_epoch,
                    } = response
                    else {
                        return Err(CallError::not_sent(
                            "route.open returned an unexpected control response",
                        ));
                    };
                    let handle = self
                        .ingress_handle(generation, route_channel, route_epoch)
                        .unwrap_or_else(|| {
                            RouteHandle::new_consumer(
                                route_channel,
                                route_epoch,
                                generation,
                                route_open.reverse_requests.clone(),
                            )
                        });
                    let route = RouteState {
                        handle,
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
                            let cached = inner.cache_route(key.clone(), route.clone());
                            RouteInstall::Cached(cached)
                        }
                    };
                    match install {
                        RouteInstall::Cached(cached) => return Ok(cached),
                        RouteInstall::Discard { closed } => {
                            if closed {
                                // GOODBYE the channel we opened so the daemon/module don't
                                // leak it, then report the close as a NotSent failure.
                                self.send_route_goodbye(route.handle, true);
                                self.uninstall_route_handle(route.handle);
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
                    // The deadline is the ONLY binder for retryable refusals.
                    // An attempt cap used to share this condition, and because
                    // the capped backoff sums to seconds it strictly dominated
                    // the deadline — the advertised reload patience was never
                    // delivered, and module restarts whose reload exceeded a
                    // few seconds failed every managed caller. Reloads
                    // legitimately take tens of seconds (drain alone defaults
                    // to 30s); capped per-attempt backoff bounds pressure.
                    if is_retryable_route_open_code(&body.code) && Instant::now() < route_deadline {
                        let delay = opts.route_retry.delay_after_attempt(attempt);
                        last_refusal = Some(body);
                        if let Err(err) = self.sleep_until_retry(route_deadline, delay).await {
                            return Err(expired(err, &mut last_refusal));
                        }
                        continue;
                    }
                    return Err(CallError::route_open_refused(key.target_label(), body));
                }
                Ok(TerminalFrame::StreamEnd) => {
                    return Err(CallError::not_sent("route.open returned StreamEnd"));
                }
                Err(err) if err.is_not_sent() && Instant::now() < route_deadline => {
                    let delay = opts.route_retry.delay_after_attempt(attempt);
                    if let Err(err) = self.sleep_until_retry(route_deadline, delay).await {
                        return Err(expired(err, &mut last_refusal));
                    }
                }
                Err(err) => return Err(expired(err, &mut last_refusal)),
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

    async fn control_call(
        self: &Arc<Self>,
        body: Vec<u8>,
        deadline: Instant,
        retain_late_route_open: bool,
        route_open_reverse_requests: Option<ReverseRequestRegistry>,
    ) -> Result<TerminalFrame, CallError> {
        self.ensure_connected_for_call(deadline).await?;
        self.send_request(RequestSend {
            expected_handle: None,
            channel: 0,
            epoch: 0,
            body,
            priority: Priority::Interactive,
            admission_class: AdmissionClass::Normal,
            deadline,
            retain_late_route_open,
            route_open_reverse_requests,
        })
        .await
    }

    async fn send_request(
        self: &Arc<Self>,
        request: RequestSend,
    ) -> Result<TerminalFrame, CallError> {
        let RequestSend {
            expected_handle,
            channel,
            epoch,
            body,
            priority,
            admission_class,
            deadline,
            retain_late_route_open,
            route_open_reverse_requests,
        } = request;
        if Instant::now() >= deadline {
            return Err(CallError::not_sent(
                "call deadline elapsed before request was sent",
            ));
        }
        let (generation, corr, writer) = {
            let mut inner = self.lock_inner();
            if inner.closed {
                return Err(CallError::not_sent("consumer closed"));
            }
            let generation = inner.generation;
            if let Some(expected) = expected_handle {
                let route_pair_matches =
                    channel == 0 || (expected.channel == channel && expected.epoch == epoch);
                if expected.connection_token() != generation
                    || !route_pair_matches
                    || inner.route_epochs.get(&expected.channel) != Some(&expected)
                {
                    return Err(CallError::StaleRouteHandle(expected));
                }
            }
            let Some(writer) = inner.writer.clone() else {
                return Err(CallError::not_sent("subc connection is down before send"));
            };
            let Some(corr) = inner.next_corr else {
                drop(inner);
                self.handle_generation_drop(
                    generation,
                    "channel-0 correlation allocator exhausted".to_string(),
                );
                return Err(CallError::not_sent(
                    "correlation allocator exhausted; connection closed",
                ));
            };
            inner.next_corr = corr.checked_add(1);
            (generation, corr, writer)
        };

        let frame = Frame::build(
            FrameType::Request,
            Flags::new(false, priority, false).with_admission_class(admission_class),
            channel,
            epoch,
            corr,
            body,
        )
        .map_err(|err| CallError::not_sent(format!("failed to build request frame: {err}")))?;
        let key = PendingKey {
            generation,
            channel,
            epoch,
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
            if let Some(expected) = expected_handle {
                if inner.route_epochs.get(&expected.channel) != Some(&expected) {
                    return Err(CallError::StaleRouteHandle(expected));
                }
            }
            let expected_control_handle = (channel == 0 && !retain_late_route_open)
                .then_some(expected_handle)
                .flatten();
            inner.pending.insert(
                key,
                PendingEntry::unary(
                    tx,
                    retain_late_route_open,
                    expected_control_handle,
                    route_open_reverse_requests,
                ),
            );
        }
        let mut registration =
            PendingRegistration::new(Arc::clone(self), key, retain_late_route_open);

        match timeout_at(
            deadline,
            writer.send(WriteCommand {
                frame,
                pending: Some(key),
            }),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(_)) => {
                let accepted = registration.remove_pending().unwrap_or(false);
                return Err(classify_failure(
                    accepted,
                    "writer task closed before accepting request",
                ));
            }
            Err(_) => {
                let _ = registration.remove_pending();
                return Err(CallError::not_sent(
                    "call deadline elapsed waiting for writer capacity",
                ));
            }
        }

        tokio::select! {
            result = timeout_at(deadline, rx) => match result {
                Ok(Ok(result)) => {
                    registration.disarm();
                    result.into_call_result()
                }
                Ok(Err(_)) => {
                    registration.disarm();
                    Err(CallError::not_sent("pending response channel closed"))
                }
                Err(_) => {
                    let accepted = if retain_late_route_open {
                        registration.disarm();
                        self.pending_accepted(key).unwrap_or(false)
                    } else {
                        registration.remove_pending().unwrap_or(false)
                    };
                    if accepted {
                        self.spawn_liveness_probe();
                    }
                    Err(classify_failure(
                        accepted,
                        format!("request on channel {channel} timed out at its deadline"),
                    ))
                }
            },
            () = self.close_token.cancelled() => {
                let accepted = registration.remove_pending().unwrap_or(false);
                Err(classify_failure(accepted, "consumer closed while request was pending"))
            }
        }
    }

    /// Probe a connection retained after an accepted request reaches its reply deadline.
    ///
    /// Deadline timeouts deliberately keep a connection because scheduler pressure can delay
    /// an otherwise healthy reply. A channel-0 Ping plus any later inbound frame separates that
    /// case from a half-open socket, which would otherwise pin every subsequent request.
    fn spawn_liveness_probe(self: &Arc<Self>) {
        if self.liveness_probe_running.swap(true, Ordering::AcqRel) {
            return;
        }

        let allocation = {
            let mut inner = self.lock_inner();
            // A pending CHANNEL-0 request suspends the probe: the daemon's
            // connection loop is FIFO and some channel-0 handlers park it
            // inline for seconds (route.open awaits the module bind ack for up
            // to route_bind_relay_timeout, ~12s in production), during which
            // our Ping sits unread in the daemon's socket buffer. Silence is
            // then explained by our own control op, and convicting would tear
            // down a healthy connection mid-bind. The gate is local knowledge
            // (we always know our own pendings) and re-arms on the next
            // deadline settle; the same check runs again before conviction.
            let control_pending = inner
                .pending
                .keys()
                .any(|key| key.generation == inner.generation && key.channel == 0);
            if inner.closed || control_pending || !matches!(&inner.reconnect, ReconnectState::Idle)
            {
                None
            } else {
                match (inner.next_corr, inner.writer.clone()) {
                    (Some(corr), Some(writer)) => {
                        inner.next_corr = corr.checked_add(1);
                        Some((inner.generation, corr, writer))
                    }
                    (None, _) | (_, None) => None,
                }
            }
        };
        let Some((generation, corr, writer)) = allocation else {
            self.liveness_probe_running.store(false, Ordering::Release);
            return;
        };

        let t0 = epoch_millis();
        let ping = match Frame::build(
            FrameType::Ping,
            Flags::new(false, Priority::Interactive, false),
            0,
            0,
            corr,
            Vec::new(),
        ) {
            Ok(ping) => ping,
            Err(_) => {
                self.liveness_probe_running.store(false, Ordering::Release);
                return;
            }
        };
        let window = self.opts.liveness_probe_window;
        let shared = Arc::clone(self);
        tokio::spawn(async move {
            let window_end = Instant::now() + window;
            // A write failure proves nothing in the healthy direction: it may be the
            // first sign of the same broken transport. Let the inbound window decide.
            let _ = timeout_at(
                window_end,
                writer.send(WriteCommand {
                    frame: ping,
                    pending: None,
                }),
            )
            .await;
            sleep_until(window_end).await;

            let control_pending_now = {
                let inner = shared.lock_inner();
                inner
                    .pending
                    .keys()
                    .any(|key| key.generation == generation && key.channel == 0)
            };
            if !shared.close_token.is_cancelled()
                && !control_pending_now // a control op begun during the window explains the silence
                && shared.generation_is_current(generation)
                && shared.last_inbound_ms.load(Ordering::Acquire) < t0
            {
                shared.handle_generation_drop(
                    generation,
                    format!(
                        "liveness probe convicted a half-open socket: no inbound frame for {}ms after a channel-0 Ping",
                        window.as_millis()
                    ),
                );
            }
            shared
                .liveness_probe_running
                .store(false, Ordering::Release);
        });
    }

    async fn send_subscription(
        self: &Arc<Self>,
        subscription: SubscriptionSend,
    ) -> Result<Subscription, CallError> {
        let SubscriptionSend {
            expected_handle,
            channel,
            epoch,
            body,
            priority,
            admission_class,
            event_buffer,
            deadline,
            permit,
        } = subscription;
        if Instant::now() >= deadline {
            return Err(CallError::not_sent(
                "subscription deadline elapsed before request was sent",
            ));
        }
        let (generation, corr, writer) = {
            let mut inner = self.lock_inner();
            if inner.closed {
                return Err(CallError::not_sent("consumer closed"));
            }
            let generation = inner.generation;
            if let Some(expected) = expected_handle {
                if expected.connection_token() != generation
                    || expected.channel != channel
                    || expected.epoch != epoch
                    || inner.route_epochs.get(&channel) != Some(&expected)
                {
                    return Err(CallError::StaleRouteHandle(expected));
                }
            }
            let Some(writer) = inner.writer.clone() else {
                return Err(CallError::not_sent("subc connection is down before send"));
            };
            let Some(corr) = inner.next_corr else {
                drop(inner);
                self.handle_generation_drop(
                    generation,
                    "correlation allocator exhausted".to_string(),
                );
                return Err(CallError::not_sent(
                    "correlation allocator exhausted; connection closed",
                ));
            };
            inner.next_corr = corr.checked_add(1);
            (generation, corr, writer)
        };

        // BIT 7 IS NOT EMITTED YET, DELIBERATELY. `FLAG_SUBSCRIPTION` is
        // allocated and the daemon reads it, but every decoder built against
        // subc-protocol <= 0.20.0 treats bit 7 as a RESERVED-BIT TRIPWIRE and
        // REFUSES the frame rather than ignoring a bit it does not know. The
        // splice forwards the flags byte verbatim, so the rejection lands at the
        // far end, inside a module whose author opted into nothing, as a decode
        // error on a frame the daemon considered well-formed.
        //
        // Restoring the `| FLAG_SUBSCRIPTION` below is PHASE 2 of a two-phase
        // fleet operation and must not happen until Phase 1 is done and
        // CENSUSED: every module linking a subc-protocol whose decoder ACCEPTS
        // bit 7. Bit 6 (DAEMON_ORIGIN) was done exactly this way, in separate
        // PRs, for exactly this reason.
        //
        // Reached from `PolicyResolver::install_push_receiver` without any
        // caller writing `subscribe`, which is why the blast radius is wider
        // than the subscribe call sites.
        let frame = Frame::build(
            FrameType::Request,
            Flags(
                Flags::new(false, priority, false)
                    .with_admission_class(admission_class)
                    .0,
            ),
            channel,
            epoch,
            corr,
            body,
        )
        .map_err(|err| CallError::not_sent(format!("failed to build request frame: {err}")))?;
        let key = PendingKey {
            generation,
            channel,
            epoch,
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
            if let Some(expected) = expected_handle {
                if inner.route_epochs.get(&expected.channel) != Some(&expected) {
                    return Err(CallError::StaleRouteHandle(expected));
                }
            }
            inner.pending.insert(
                key,
                PendingEntry::subscription(events_tx, closed_tx, permit, priority),
            );
        }
        let mut registration = PendingRegistration::new(Arc::clone(self), key, false);

        match timeout_at(
            deadline,
            writer.send(WriteCommand {
                frame,
                pending: Some(key),
            }),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(_)) => {
                let accepted = registration.remove_pending().unwrap_or(false);
                return Err(classify_failure(
                    accepted,
                    "writer task closed before accepting subscription request",
                ));
            }
            Err(_) => {
                let _ = registration.remove_pending();
                return Err(CallError::not_sent(
                    "subscription deadline elapsed waiting for writer capacity",
                ));
            }
        }

        registration.disarm();
        Ok(Subscription {
            events: events_rx,
            closed: SubscriptionClosed { rx: closed_rx },
            cancel: SubscriptionCancel::new(Arc::clone(self), key, priority),
        })
    }

    fn unsubscribe_subscription(
        &self,
        key: PendingKey,
        priority: Priority,
    ) -> Result<(), CallError> {
        let handle = RouteHandle::new(key.channel, key.epoch, key.generation);
        self.validate_current_handle(handle)?;
        let entry = self.lock_inner().pending.remove(&key);
        if let Some(entry) = entry {
            entry.settle_subscription_result(Ok(()));
            self.send_cancel(handle, key.corr, priority);
        }
        Ok(())
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
            self.send_cancel(
                RouteHandle::new(key.channel, key.epoch, key.generation),
                key.corr,
                priority,
            );
        }
    }

    fn register_push_events(
        &self,
        handle: RouteHandle,
    ) -> Result<mpsc::Receiver<PushEvent>, CallError> {
        let (events_tx, events_rx) = mpsc::channel(DEFAULT_PUSH_EVENT_BUFFER);
        let mut inner = self.lock_inner();
        if inner.closed
            || inner.generation != handle.connection_token()
            || inner.writer.is_none()
            || inner.route_epochs.get(&handle.channel) != Some(&handle)
        {
            return Err(CallError::StaleRouteHandle(handle));
        }
        inner.push_event_receivers.insert(handle, events_tx);
        Ok(events_rx)
    }

    fn route_push(&self, handle: RouteHandle, body: Vec<u8>) {
        let should_count_drop = {
            let mut inner = self.lock_inner();
            if inner.closed
                || inner.generation != handle.connection_token()
                || inner.route_epochs.get(&handle.channel) != Some(&handle)
            {
                // Deliberately uncounted: a push against a stale epoch or dead
                // generation has no live subscriber by definition, and the
                // counters below claim delivery loss on LIVE routes only.
                return;
            }
            match inner.push_event_receivers.get(&handle) {
                None => DroppedPush::NoReceiver,
                Some(events) => match events.try_send(PushEvent { handle, body }) {
                    Ok(()) => DroppedPush::Delivered,
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        inner.push_event_receivers.remove(&handle);
                        DroppedPush::NoReceiver
                    }
                    // A burst KEEPS the subscription (issue #40). This used to
                    // remove the receiver -- documented as loss-signaling via
                    // recv()->None -- but the fleet's real push consumers are
                    // idempotent wake nudges, where one missed event costs a
                    // poll cycle and a lost subscription costs every later wake
                    // until a re-register path that history says is where bugs
                    // live. Matches control_push below; the drop is counted on
                    // its own counter so a too-slow consumer is diagnosable as
                    // such rather than filed under "never subscribed".
                    Err(mpsc::error::TrySendError::Full(_)) => DroppedPush::ReceiverFull,
                },
            }
        };
        match should_count_drop {
            DroppedPush::Delivered => {}
            DroppedPush::NoReceiver => {
                self.pushes_dropped_no_receiver
                    .fetch_add(1, Ordering::Relaxed);
            }
            DroppedPush::ReceiverFull => {
                self.pushes_dropped_receiver_full
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Deliver a daemon-originated channel-0 control push to the registered
    /// consumer receiver, or count the drop. Never blocks the reader.
    fn control_push(&self, body: &[u8]) {
        let parsed = serde_json::from_slice::<serde_json::Value>(body)
            .ok()
            .and_then(|value| {
                let op = value.get("op")?.as_str()?.to_string();
                Some(ControlPush { op, body: value })
            });
        let delivered = match parsed {
            None => false,
            Some(push) => {
                let mut inner = self.lock_inner();
                match inner.control_push_receiver.as_ref() {
                    None => false,
                    Some(sender) => match sender.try_send(push) {
                        Ok(()) => true,
                        Err(mpsc::error::TrySendError::Closed(_)) => {
                            inner.control_push_receiver = None;
                            false
                        }
                        Err(mpsc::error::TrySendError::Full(_)) => false,
                    },
                }
            }
        };
        if !delivered {
            self.control_pushes_dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn send_cancel(&self, handle: RouteHandle, corr: u64, priority: Priority) {
        let writer = {
            let inner = self.lock_inner();
            if inner.closed
                || inner.generation != handle.connection_token()
                || inner.route_epochs.get(&handle.channel) != Some(&handle)
            {
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
            handle.channel,
            handle.epoch,
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

    fn settle_pending(self: &Arc<Self>, key: PendingKey, terminal: PendingTerminal) {
        let entry = self.lock_inner().pending.remove(&key);
        let Some(entry) = entry else {
            return;
        };
        if entry.retain_late_route_open && entry.completion_is_closed() {
            if let PendingTerminal::Response { generation, body } = &terminal {
                if let Ok(ClientControlResponse::RouteOpen {
                    route_channel,
                    route_epoch,
                }) = serde_json::from_slice::<ClientControlResponse>(body)
                {
                    let handle = RouteHandle::new(route_channel, route_epoch, *generation);
                    self.send_route_goodbye(handle, true);
                    self.uninstall_route_handle(handle);
                }
            }
            return;
        }
        entry.settle_terminal(terminal);
    }

    fn pending_accepted(&self, key: PendingKey) -> Option<bool> {
        self.lock_inner()
            .pending
            .get(&key)
            .map(|entry| entry.accepted)
    }

    fn handle_generation_drop(self: &Arc<Self>, generation: u64, reason: String) {
        let (should_emit, pending, openings, callbacks) = {
            let mut inner = self.lock_inner();
            if inner.closed || inner.generation != generation || inner.writer.is_none() {
                return;
            }
            inner.writer = None;
            inner.restored_token = inner.restored_token.saturating_add(1);
            inner.close_routes();
            inner.route_epochs.clear();
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
            let _ = self.spawn_reconnect(generation);
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
            inner.route_epochs.clear();
            inner.push_event_receivers.clear();
            self.close_token.cancel();
            let reconnect = match std::mem::replace(&mut inner.reconnect, ReconnectState::Idle) {
                ReconnectState::Background { task, .. } => Some(task),
                ReconnectState::Idle | ReconnectState::Inline { .. } => None,
            };
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
                inner.drain_routes(),
                inner.reader_task.take(),
                inner.writer_task.take(),
                reconnect,
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

    fn validate_current_handle(&self, handle: RouteHandle) -> Result<(), CallError> {
        let inner = self.lock_inner();
        if inner.closed
            || inner.generation != handle.connection_token()
            || inner.writer.is_none()
            || inner.route_epochs.get(&handle.channel) != Some(&handle)
        {
            Err(CallError::StaleRouteHandle(handle))
        } else {
            Ok(())
        }
    }

    fn route_state(&self, handle: RouteHandle) -> Result<RouteState, CallError> {
        let inner = self.lock_inner();
        if inner.closed
            || inner.generation != handle.connection_token()
            || inner.writer.is_none()
            || inner.route_epochs.get(&handle.channel) != Some(&handle)
        {
            return Err(CallError::StaleRouteHandle(handle));
        }

        let route = inner
            .route_by_channel
            .get(&handle.channel)
            .and_then(|key| inner.routes.get(key))
            .or_else(|| inner.one_shot_routes.get(&handle.channel));
        debug_assert!(route.is_none_or(|route| route.handle == handle));
        route
            .filter(|route| route.handle == handle)
            .cloned()
            .ok_or(CallError::StaleRouteHandle(handle))
    }

    fn route_is_current(&self, key: &RouteKey, route: &RouteState) -> bool {
        let inner = self.lock_inner();
        if inner.closed
            || inner.generation != route.handle.connection_token()
            || inner.writer.is_none()
        {
            return false;
        }
        inner.routes.get(key).is_some_and(|cached| {
            cached.handle == route.handle && Arc::ptr_eq(&cached.sem, &route.sem)
        })
    }

    fn invalidate_route(&self, key: &RouteKey, expected_handle: Option<RouteHandle>) {
        let removed = {
            let mut inner = self.lock_inner();
            match inner.routes.get(key) {
                Some(route) if expected_handle.is_none_or(|expected| expected == route.handle) => {
                    let removed = inner.remove_route(key);
                    if let Some(route) = &removed {
                        if inner.route_epochs.get(&route.handle.channel) == Some(&route.handle) {
                            inner.route_epochs.remove(&route.handle.channel);
                        }
                        inner.push_event_receivers.remove(&route.handle);
                    }
                    removed
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

    async fn close_handle(
        self: &Arc<Self>,
        handle: RouteHandle,
        opts: &CloseRouteOptions,
    ) -> Result<(), CallError> {
        self.validate_current_handle(handle)?;
        let routes = {
            let mut inner = self.lock_inner();
            inner
                .remove_route_by_handle(handle)
                .into_iter()
                .collect::<Vec<_>>()
        };
        if opts.drain {
            self.drain_channel(handle, opts.drain_timeout).await;
        }
        for route in routes {
            route.sem.close();
        }
        self.fail_channel_pending(handle, "route closed by close_handle");
        self.send_route_goodbye(handle, false);
        self.uninstall_route_handle(handle);
        Ok(())
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
            inner.remove_route(key)
        };

        // Nothing cached: either never opened (idempotent no-op) or still opening (the
        // racing lead-opener will see the flag and GOODBYE whatever channel it opens).
        let Some(route) = route else {
            return;
        };

        if opts.drain {
            // Wait for in-flight UNARY requests on this channel to settle naturally,
            // bounded by drain_timeout, before tearing the route down.
            self.drain_channel(route.handle, opts.drain_timeout).await;
        }

        // Closing the semaphore makes any not-yet-sent acquire() return Err -> the
        // caller classifies it NotSent. Already-sent pending requests are settled
        // at-most-once (OutcomeUnknown if the writer accepted their bytes).
        route.sem.close();
        self.fail_channel_pending(route.handle, "route closed by close_route");

        // Best-effort route GOODBYE: the daemon releases the route + relays the module
        // route-gone GOODBYE the module's reaper consumes. One-way, no ack.
        self.send_route_goodbye(route.handle, false);
        self.uninstall_route_handle(route.handle);
    }

    fn uninstall_route_handle(&self, handle: RouteHandle) {
        let mut inner = self.lock_inner();
        if inner.route_epochs.get(&handle.channel) == Some(&handle) {
            inner.route_epochs.remove(&handle.channel);
            inner.push_event_receivers.remove(&handle);
            release_reverse_request_registry(handle);
        }
    }

    /// Settle every in-flight pending request on `channel` (this generation) as an
    /// at-most-once failure: OutcomeUnknown if the writer already accepted its bytes,
    /// NotSent otherwise. Mirrors the connection-drop path, scoped to one channel.
    fn fail_channel_pending(&self, handle: RouteHandle, reason: &str) {
        let entries = {
            let mut inner = self.lock_inner();
            drain_pending_handle(&mut inner.pending, handle, true)
        };
        settle_pending_entries(entries, reason.to_string());
    }

    /// Resolve once every in-flight unary pending on `channel` has settled, or the
    /// timeout elapses. Polls the pending map (entries are removed on settle); the
    /// volume here is tiny (a route window is small) so a short poll is adequate.
    async fn drain_channel(&self, handle: RouteHandle, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            let has_inflight = {
                let inner = self.lock_inner();
                inner.pending.iter().any(|(key, entry)| {
                    key.generation == handle.connection_token()
                        && key.channel == handle.channel
                        && key.epoch == handle.epoch
                        && !entry.is_subscription()
                })
            };
            if !has_inflight || Instant::now() >= deadline {
                return;
            }
            sleep(Duration::from_millis(5)).await;
        }
    }

    /// Queue a header-only route GOODBYE if `handle` is still live on this connection.
    /// Late successful route.open cleanup sets `close_on_failure`: orphan prevention then
    /// requires closing the connection when the GOODBYE cannot enter the writer queue.
    fn send_route_goodbye(self: &Arc<Self>, handle: RouteHandle, close_on_failure: bool) -> bool {
        let writer = {
            let inner = self.lock_inner();
            if inner.closed
                || inner.generation != handle.connection_token()
                || inner.route_epochs.get(&handle.channel) != Some(&handle)
            {
                return false;
            }
            inner.writer.clone()
        };
        let Some(writer) = writer else {
            return false;
        };
        let Ok(frame) = Frame::build(
            FrameType::Goodbye,
            Flags::new(false, Priority::Interactive, false),
            handle.channel,
            handle.epoch,
            0,
            Vec::new(),
        ) else {
            return false;
        };
        if writer
            .try_send(WriteCommand {
                frame,
                pending: None,
            })
            .is_ok()
        {
            return true;
        }
        if close_on_failure {
            self.handle_generation_drop(
                handle.connection_token(),
                "failed to queue late route.open cleanup GOODBYE".to_string(),
            );
        }
        false
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
    Lead {
        generation: u64,
        stale_task: Option<JoinHandle<()>>,
    },
}

/// The reconnect state is fenced by the generation whose transport failed. A
/// newer generation can replace an older attempt, and completion only changes
/// the state when its generation still owns the slot.
enum ReconnectState {
    Idle,
    Inline {
        generation: u64,
    },
    Background {
        generation: u64,
        task: JoinHandle<()>,
    },
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
    reverse_requests: &'a ReverseRequestRegistry,
}

struct RequestSend {
    expected_handle: Option<RouteHandle>,
    channel: u16,
    epoch: u32,
    body: Vec<u8>,
    priority: Priority,
    admission_class: AdmissionClass,
    deadline: Instant,
    retain_late_route_open: bool,
    route_open_reverse_requests: Option<ReverseRequestRegistry>,
}

struct SubscriptionSend {
    expected_handle: Option<RouteHandle>,
    channel: u16,
    epoch: u32,
    body: Vec<u8>,
    priority: Priority,
    admission_class: AdmissionClass,
    event_buffer: usize,
    deadline: Instant,
    permit: OwnedSemaphorePermit,
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

struct InlineReconnectGuard {
    shared: Arc<Shared>,
    generation: u64,
    finished: bool,
}

impl InlineReconnectGuard {
    fn new(shared: Arc<Shared>, generation: u64) -> Self {
        Self {
            shared,
            generation,
            finished: false,
        }
    }

    fn finish(&mut self) {
        self.shared.finish_inline_reconnect(self.generation);
        self.finished = true;
    }
}

impl Drop for InlineReconnectGuard {
    fn drop(&mut self) {
        if !self.finished {
            self.shared.finish_inline_reconnect(self.generation);
        }
    }
}

#[derive(Clone)]
struct RouteState {
    handle: RouteHandle,
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

#[derive(Clone, Hash, PartialEq, Eq)]
struct ConsumerIdentityKey {
    module_id: String,
    launch_nonce: String,
}

// Hand-written so the launch nonce is never printed. The nonce is the credential
// that attributes a connection to a supervised module, and a derived Debug would
// write it into any log line or panic message that formats this value. This key sits
// inside the route cache key, which is formatted in logs.
impl fmt::Debug for ConsumerIdentityKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConsumerIdentityKey")
            .field("module_id", &self.module_id)
            .field(
                "launch_nonce",
                &format_args!("<{} bytes redacted>", self.launch_nonce.len()),
            )
            .finish()
    }
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
    /// A route.open refusal travels typed, so every caller sharing this
    /// failure (the single-flight leader and its waiters) still gets
    /// `CallError::route_open_refusal`, not just the message text.
    refusal: Option<(String, ErrorBody)>,
}

impl SharedCallFailure {
    fn not_sent(message: impl Into<String>) -> Self {
        Self {
            kind: FailureKind::NotSent,
            message: message.into(),
            refusal: None,
        }
    }

    fn into_call_error(self) -> CallError {
        match (self.kind, self.refusal) {
            (FailureKind::NotSent, Some((target, body))) => {
                CallError::route_open_refused(target, body)
            }
            (FailureKind::NotSent, None) => CallError::not_sent(self.message),
            (FailureKind::OutcomeUnknown, _) => CallError::outcome_unknown(self.message),
        }
    }
}

impl From<CallError> for SharedCallFailure {
    fn from(value: CallError) -> Self {
        match value {
            CallError::NotSent(err) => Self {
                kind: FailureKind::NotSent,
                message: err.to_string(),
                refusal: err
                    .downcast_ref::<RouteOpenRefused>()
                    .map(|refused| (refused.target.clone(), refused.body.clone())),
            },
            CallError::OutcomeUnknown(err) => Self {
                kind: FailureKind::OutcomeUnknown,
                message: err.to_string(),
                refusal: None,
            },
            CallError::Module(body) => Self {
                kind: FailureKind::OutcomeUnknown,
                message: format!(
                    "unexpected module error during route.open: {} ({})",
                    body.code, body.message
                ),
                refusal: None,
            },
            CallError::SubscriptionBackpressure(err) => Self {
                kind: FailureKind::OutcomeUnknown,
                message: err.to_string(),
                refusal: None,
            },
            CallError::StaleRouteHandle(handle) => Self {
                kind: FailureKind::NotSent,
                message: format!("stale route handle: {handle:?}"),
                refusal: None,
            },
            error @ (CallError::CapabilityUnprovided { .. }
            | CallError::CapabilityAmbiguous { .. }
            | CallError::InvalidCapabilityIdentifier { .. }) => Self {
                kind: FailureKind::NotSent,
                message: error.to_string(),
                refusal: None,
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
    epoch: u32,
    corr: u64,
}

struct PendingEntry {
    accepted: bool,
    retain_late_route_open: bool,
    expected_control_handle: Option<RouteHandle>,
    route_open_reverse_requests: Option<ReverseRequestRegistry>,
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
    fn unary(
        tx: oneshot::Sender<PendingResult>,
        retain_late_route_open: bool,
        expected_control_handle: Option<RouteHandle>,
        route_open_reverse_requests: Option<ReverseRequestRegistry>,
    ) -> Self {
        Self {
            accepted: false,
            retain_late_route_open,
            expected_control_handle,
            route_open_reverse_requests,
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
            retain_late_route_open: false,
            expected_control_handle: None,
            route_open_reverse_requests: None,
            completion: PendingCompletion::Subscription {
                events,
                closed,
                _permit: permit,
                priority,
            },
        }
    }

    fn completion_is_closed(&self) -> bool {
        match &self.completion {
            PendingCompletion::Unary(tx) => tx.is_closed(),
            PendingCompletion::Subscription { closed, .. } => closed.is_closed(),
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
                    PendingTerminal::Error { body, .. } => Err(CallError::Module(body)),
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
    retain_on_drop: bool,
}

impl PendingRegistration {
    fn new(shared: Arc<Shared>, key: PendingKey, retain_on_drop: bool) -> Self {
        Self {
            shared,
            key,
            active: true,
            retain_on_drop,
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
        if self.retain_on_drop {
            self.disarm();
        } else {
            let _ = self.remove_pending();
        }
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
    Error { body: ErrorBody, flags: Flags },
    StreamEnd,
}

impl PendingTerminal {
    fn into_terminal_frame(self) -> TerminalFrame {
        match self {
            Self::Response { generation, body } => TerminalFrame::Response { generation, body },
            Self::Error { body, flags } => TerminalFrame::Error { body, flags },
            Self::StreamEnd => TerminalFrame::StreamEnd,
        }
    }
}

#[derive(Debug)]
enum TerminalFrame {
    Response { generation: u64, body: Vec<u8> },
    Error { body: ErrorBody, flags: Flags },
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
    let conn =
        connection_file::read_for_client(path).map_err(|source| ConsumerError::ConnectionFile {
            path: path.to_path_buf(),
            source,
        })?;
    open_connection_with_info(path, &conn, deadline).await
}

async fn open_connection_with_info(
    path: &Path,
    conn: &connection_file::ConnectionInfo,
    deadline: Duration,
) -> Result<OpenedConnection, ConsumerError> {
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
    // Consumers send a request and wait for its reply, so there is no following
    // write for Nagle to coalesce with -- it can only hold the request back until
    // an ACK returns. Both ends of the hop must disable it for either to help.
    //
    // Dropped rather than logged for the same reason as the module path: no logging
    // dependency here, and a socket too broken to take the option fails the
    // handshake on the next line with a typed error.
    let _ = stream.set_nodelay(true);
    authenticate_client(&mut stream, conn, deadline)
        .await
        .map_err(|source| ConsumerError::Auth {
            path: path.to_path_buf(),
            endpoint: endpoint_label,
            source,
        })?;
    Ok(OpenedConnection { stream })
}

fn dispatch_reverse_request(shared: &Arc<Shared>, frame: Frame, handle: RouteHandle) {
    let method = serde_json::from_slice::<serde_json::Value>(&frame.body)
        .ok()
        .and_then(|body| body.get("method")?.as_str().map(str::to_string));
    let method_family = method
        .as_deref()
        .and_then(|method| method.split('/').next())
        .filter(|family| !family.is_empty())
        .map(str::to_string);
    let handler = method_family.as_deref().and_then(|family| {
        route_reverse_request_registry(handle.reverse_request_registry_id())
            .ok()?
            .handler(family)
    });
    let generation = handle.connection_token();
    let shared = Arc::clone(shared);

    tokio::spawn(async move {
        let (frame_type, body) = if let (Some(handler), Some(method)) = (handler, method) {
            let request_body = frame.body.clone();
            let corr = frame.header.corr;
            match tokio::spawn(async move {
                handler(request_body, ReverseRequestContext { corr, method }).await
            })
            .await
            {
                Ok(Ok(body)) => (FrameType::Response, body),
                Ok(Err(err)) => (
                    FrameType::Error,
                    serde_json::to_vec(&ErrorBody::new(REVERSE_REQUEST_UNHANDLED, err.to_string()))
                        .unwrap_or_default(),
                ),
                Err(err) => (
                    FrameType::Error,
                    serde_json::to_vec(&ErrorBody::new(
                        REVERSE_REQUEST_UNHANDLED,
                        join_error_message(err),
                    ))
                    .unwrap_or_default(),
                ),
            }
        } else {
            let message = method_family.map_or_else(
                || "reverse request has no valid method".to_string(),
                |family| format!("no reverse-request handler is registered for {family}"),
            );
            (
                FrameType::Error,
                serde_json::to_vec(&ErrorBody::new(REVERSE_REQUEST_UNHANDLED, message))
                    .unwrap_or_default(),
            )
        };

        if shared.ingress_handle(generation, handle.channel, handle.epoch) != Some(handle) {
            return;
        }
        let reply = match Frame::build_with_version(
            frame.header.ver,
            frame_type,
            Flags::new(false, Priority::Interactive, false),
            handle.channel,
            handle.epoch,
            frame.header.corr,
            body,
        ) {
            Ok(reply) => reply,
            Err(_) => return,
        };
        let writer = shared.lock_inner().writer.clone();
        if let Some(writer) = writer {
            let _ = writer
                .send(WriteCommand {
                    frame: reply,
                    pending: None,
                })
                .await;
        }
    });
}

fn join_error_message(error: tokio::task::JoinError) -> String {
    if !error.is_panic() {
        return error.to_string();
    }
    let payload = error.into_panic();
    if let Some(message) = payload.downcast_ref::<String>() {
        return message.clone();
    }
    if let Some(message) = payload.downcast_ref::<&str>() {
        return (*message).to_string();
    }
    "reverse-request handler panicked".to_string()
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
    if !shared.record_inbound_if_current(generation) {
        return false;
    }
    if frame.header.channel != 0
        && !shared.validate_ingress_handle(generation, frame.header.channel, frame.header.epoch)
    {
        return true;
    }

    if frame.header.ty == FrameType::Request && frame.header.channel != 0 {
        if let Some(handle) =
            shared.ingress_handle(generation, frame.header.channel, frame.header.epoch)
        {
            dispatch_reverse_request(shared, frame, handle);
        }
        return true;
    }

    let key = PendingKey {
        generation,
        channel: frame.header.channel,
        epoch: frame.header.epoch,
        corr: frame.header.corr,
    };

    if frame.header.channel == 0 && frame.header.ty == FrameType::Response {
        if let Some(expected) = shared.pending_expected_control_handle(key) {
            let echoes_expected = matches!(
                serde_json::from_slice::<ClientControlResponse>(&frame.body),
                Ok(ClientControlResponse::RoutePoll {
                    route_channel,
                    route_epoch,
                    ..
                }) if route_channel == expected.channel && route_epoch == expected.epoch
            );
            if !echoes_expected {
                shared.count_dropped_route_frame();
                return true;
            }
        }
    }

    // A route.open handle is published before its waiter is resolved. The socket reader
    // cannot consume a following same-route frame until this synchronous install finishes.
    if frame.header.channel == 0
        && frame.header.ty == FrameType::Response
        && shared.pending_expects_route_open(key)
    {
        if let Ok(ClientControlResponse::RouteOpen {
            route_channel,
            route_epoch,
        }) = serde_json::from_slice::<ClientControlResponse>(&frame.body)
        {
            let reverse_requests = shared
                .pending_route_open_reverse_requests(key)
                .unwrap_or_default();
            shared.install_ingress_handle(RouteHandle::new_consumer(
                route_channel,
                route_epoch,
                generation,
                reverse_requests,
            ));
        }
    }

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
                    detail: None,
                });
            shared.settle_pending(
                key,
                PendingTerminal::Error {
                    body,
                    flags: frame.header.flags,
                },
            );
        }
        FrameType::StreamEnd => shared.settle_pending(key, PendingTerminal::StreamEnd),
        FrameType::StreamData => shared.route_stream_data(key, frame.body),
        FrameType::Push if frame.header.channel == 0 => {
            shared.control_push(&frame.body);
        }
        FrameType::Push => shared.route_push(
            RouteHandle::new(frame.header.channel, frame.header.epoch, generation),
            frame.body,
        ),
        FrameType::Goodbye if frame.header.channel == 0 => {
            shared.handle_generation_drop(generation, "subc sent GOODBYE".to_string());
            return false;
        }
        FrameType::Goodbye => {
            let handle = RouteHandle::new(frame.header.channel, frame.header.epoch, generation);
            shared.invalidate_routes_for_handle(handle);
            let pending = {
                let mut inner = shared.lock_inner();
                drain_pending_handle(&mut inner.pending, handle, true)
            };
            settle_pending_entries(pending, "route closed by subc".to_string());
        }
        FrameType::Ping if frame.header.channel == 0 => {
            if let Ok(pong) = Frame::build_with_version(
                frame.header.ver,
                FrameType::Pong,
                frame.header.flags,
                0,
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
    /// Stamp after holding the same state lock that guards generation changes, so a
    /// late frame from an older reader can never vouch for a newly installed socket.
    ///
    /// PLACEMENT IS LOAD-BEARING: the single caller sits on the frame-read
    /// return, the one point every inbound frame passes before demux -- which
    /// is why one stamp suffices. A future fast path or drain-and-dispatch
    /// refactor that hands frames onward without crossing that point makes the
    /// stamp skippable, and the liveness watermark quietly stops meaning "the
    /// link delivered bytes": the cheapest correctness property in this file
    /// and the easiest to lose in a refactor.
    fn record_inbound_if_current(&self, generation: u64) -> bool {
        let inner = self.lock_inner();
        if inner.closed || inner.generation != generation || inner.writer.is_none() {
            return false;
        }
        self.last_inbound_ms
            .store(epoch_millis(), Ordering::Release);
        true
    }

    fn generation_is_current(&self, generation: u64) -> bool {
        let inner = self.lock_inner();
        !inner.closed && inner.generation == generation && inner.writer.is_some()
    }

    fn pending_expected_control_handle(&self, key: PendingKey) -> Option<RouteHandle> {
        self.lock_inner()
            .pending
            .get(&key)
            .and_then(|entry| entry.expected_control_handle)
    }

    fn count_dropped_route_frame(&self) {
        let mut inner = self.lock_inner();
        inner.dropped_route_frames = inner.dropped_route_frames.saturating_add(1);
    }

    fn pending_expects_route_open(&self, key: PendingKey) -> bool {
        self.lock_inner()
            .pending
            .get(&key)
            .is_some_and(|entry| entry.retain_late_route_open)
    }

    fn pending_route_open_reverse_requests(
        &self,
        key: PendingKey,
    ) -> Option<ReverseRequestRegistry> {
        self.lock_inner()
            .pending
            .get(&key)
            .and_then(|entry| entry.route_open_reverse_requests.clone())
    }

    fn ingress_handle(&self, generation: u64, channel: u16, epoch: u32) -> Option<RouteHandle> {
        let inner = self.lock_inner();
        let handle = inner.route_epochs.get(&channel).copied()?;
        (handle.connection_token() == generation && handle.epoch == epoch).then_some(handle)
    }

    fn validate_ingress_handle(&self, generation: u64, channel: u16, epoch: u32) -> bool {
        let mut inner = self.lock_inner();
        let expected = RouteHandle::new(channel, epoch, generation);
        if inner.route_epochs.get(&channel) == Some(&expected) {
            true
        } else {
            inner.dropped_route_frames = inner.dropped_route_frames.saturating_add(1);
            false
        }
    }

    fn install_ingress_handle(&self, handle: RouteHandle) {
        let mut inner = self.lock_inner();
        if !inner.closed && inner.generation == handle.connection_token() && inner.writer.is_some()
        {
            if let Some(previous) = inner.route_epochs.insert(handle.channel, handle) {
                if previous.reverse_request_registry_id() != handle.reverse_request_registry_id() {
                    release_reverse_request_registry(previous);
                }
            }
        }
    }

    fn invalidate_routes_for_handle(&self, handle: RouteHandle) {
        let removed = {
            let mut inner = self.lock_inner();
            if inner.route_epochs.get(&handle.channel) != Some(&handle) {
                return;
            }
            inner.route_epochs.remove(&handle.channel);
            inner.push_event_receivers.remove(&handle);
            inner
                .remove_route_by_handle(handle)
                .into_iter()
                .collect::<Vec<_>>()
        };
        for route in removed {
            route.sem.close();
        }
    }
}

async fn writer_loop<W>(
    shared: Arc<Shared>,
    writer: W,
    mut rx: mpsc::Receiver<WriteCommand>,
    generation: u64,
) where
    W: AsyncWrite + Unpin,
{
    let mut writer = BufWriter::new(writer);
    while let Some(command) = rx.recv().await {
        if let Some(key) = command.pending {
            if !shared.mark_pending_accepted(key) {
                continue;
            }
        }
        if let Err(err) = write_frame(&mut writer, &command.frame).await {
            shared.handle_generation_drop(generation, err.to_string());
            return;
        }
        while let Ok(command) = rx.try_recv() {
            if let Some(key) = command.pending {
                if !shared.mark_pending_accepted(key) {
                    continue;
                }
            }
            if let Err(err) = write_frame(&mut writer, &command.frame).await {
                shared.handle_generation_drop(generation, err.to_string());
                return;
            }
        }
        if let Err(err) = writer.flush().await.map_err(FrameIoError::Io) {
            shared.handle_generation_drop(generation, err.to_string());
            return;
        }
    }
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
    opts.reverse_requests.seal();
    let capabilities = opts.reverse_requests.capabilities();
    (!capabilities.is_empty()).then_some(capabilities)
}

fn close_route_consumer_capabilities(opts: &CloseRouteOptions) -> Option<Vec<String>> {
    opts.reverse_requests.seal();
    let capabilities = opts.reverse_requests.capabilities();
    (!capabilities.is_empty()).then_some(capabilities)
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

fn validate_capability_for_resolution(capability: &str) -> Result<(), CallError> {
    if is_valid_capability_identifier(capability) {
        Ok(())
    } else {
        Err(CallError::InvalidCapabilityIdentifier {
            capability: capability.to_string(),
        })
    }
}

fn capability_claimants(catalog: &CatalogList, capability: &str) -> Vec<String> {
    let mut claimants = catalog
        .modules
        .iter()
        .filter(|module| {
            module.capabilities.as_ref().is_some_and(|capabilities| {
                capabilities
                    .provides
                    .iter()
                    .any(|claim| claim == capability)
            })
        })
        .map(|module| module.module_id.clone())
        .collect::<Vec<_>>();
    claimants.sort();
    claimants
}

fn epoch_millis() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(u64::MAX)
}

/// Reclassify a route-open failure seen by a managed `call` or `subscribe`.
///
/// `ensure_route` reports the route.open control request's own outcome, which
/// is `OutcomeUnknown` when that request was written but its reply did not
/// arrive before the deadline: the daemon may or may not have opened the
/// channel. The caller's request body, however, is only written after a route
/// is in hand, so when no route came back the body provably never left the
/// client. From the caller's point of view that is `NotSent`, and reporting
/// `OutcomeUnknown` would wrongly tell it a non-idempotent operation might
/// have run. A route.open reply that arrives after its waiter gave up is still
/// cleaned up: `settle_pending` sends that channel a GOODBYE instead of caching
/// it, so reporting `NotSent` here leaves no route open behind.
fn request_not_sent_after_route_open_failure(err: CallError) -> CallError {
    match err {
        CallError::OutcomeUnknown(source) => CallError::not_sent(format!(
            "request not sent: route.open did not complete ({source})"
        )),
        other => other,
    }
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

fn drain_pending_handle(
    pending: &mut HashMap<PendingKey, PendingEntry>,
    handle: RouteHandle,
    include_subscriptions: bool,
) -> Vec<PendingEntry> {
    let keys = pending
        .iter()
        .filter_map(|(key, entry)| {
            (key.generation == handle.connection_token()
                && key.channel == handle.channel
                && key.epoch == handle.epoch
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

fn emit_callbacks(callbacks: Vec<Callback>, state: ConnectionState) {
    for callback in callbacks {
        if let Ok(callback) = callback.lock() {
            callback(state.clone());
        }
    }
}

/// The route-open retry predicate, defined once in `subc_protocol::error_codes`
/// so consumers with their own connection layer share it rather than copy it.
pub fn is_retryable_route_open_code(code: &str) -> bool {
    error_codes::is_retryable_route_open(code)
}

fn is_retryable_catalog_transport_error(err: &CallError) -> bool {
    matches!(err, CallError::NotSent(_) | CallError::OutcomeUnknown(_))
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
        ConsumerError::Discovery { .. }
        | ConsumerError::NoEndpoint { .. }
        | ConsumerError::Closed => false,
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
    // Imported HERE rather than at the crate root because production code no
    // longer emits bit 7 (see the send_subscription comment); only the guard test
    // that keeps it un-emitted still names it. Phase 2 moves it back up.
    use subc_protocol::FLAG_SUBSCRIPTION;

    #[derive(Clone)]
    struct InstrumentedWriter {
        state: Arc<InstrumentedWriterState>,
        fail_flush: bool,
    }

    #[derive(Default)]
    struct InstrumentedWriterState {
        bytes: Mutex<Vec<u8>>,
        flushes: std::sync::atomic::AtomicUsize,
    }

    impl InstrumentedWriter {
        fn new(fail_flush: bool) -> Self {
            Self {
                state: Arc::new(InstrumentedWriterState::default()),
                fail_flush,
            }
        }

        fn bytes(&self) -> Vec<u8> {
            self.state
                .bytes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
        }

        fn flush_count(&self) -> usize {
            self.state.flushes.load(Ordering::SeqCst)
        }
    }

    impl AsyncWrite for InstrumentedWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            self.state
                .bytes
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .extend_from_slice(buf);
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            self.state.flushes.fetch_add(1, Ordering::SeqCst);
            if self.fail_flush {
                Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "instrumented flush failure",
                )))
            } else {
                Poll::Ready(Ok(()))
            }
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    fn writer_test_shared() -> Arc<Shared> {
        Arc::new(Shared::new(
            PathBuf::from("/tmp/does-not-exist"),
            ConsumerOptions {
                reconnect_backoff: RetryBackoff {
                    max_attempts: 1,
                    ..RetryBackoff::default()
                },
                ..ConsumerOptions::default()
            },
        ))
    }

    fn reverse_test_shared(
        reverse_requests: ReverseRequestRegistry,
    ) -> (Arc<Shared>, mpsc::Receiver<WriteCommand>, RouteHandle) {
        let shared = writer_test_shared();
        let (writer, receiver) = mpsc::channel(16);
        let handle = RouteHandle::new_consumer(17, 3, 1, reverse_requests);
        {
            let mut inner = shared.lock_inner();
            inner.writer = Some(writer);
            inner.route_epochs.insert(handle.channel, handle);
        }
        (shared, receiver, handle)
    }

    fn reverse_request_frame(handle: RouteHandle, corr: u64, body: Vec<u8>) -> Frame {
        Frame::build(
            FrameType::Request,
            Flags::new(false, Priority::Interactive, false),
            handle.channel,
            handle.epoch,
            corr,
            body,
        )
        .unwrap()
    }

    async fn capture_route_open(
        reverse_requests: ReverseRequestRegistry,
    ) -> (serde_json::Value, RouteHandle) {
        let shared = writer_test_shared();
        let (writer, mut receiver) = mpsc::channel(4);
        shared.lock_inner().writer = Some(writer);
        let consumer = SubcConsumer {
            shared: Arc::clone(&shared),
        };
        let opts = CallOptions {
            reverse_requests,
            ..CallOptions::default()
        };
        let task = tokio::spawn(async move {
            consumer
                .open_route(
                    RouteTarget::ToolProvider {
                        module_id: "reverse-provider".to_string(),
                    },
                    BindIdentity::new(
                        PathBuf::from("/tmp/project"),
                        "test".to_string(),
                        "reverse".to_string(),
                    ),
                    opts,
                )
                .await
        });
        let command = receiver.recv().await.expect("route.open frame");
        let request = serde_json::from_slice(&command.frame.body).unwrap();
        let response_body = serde_json::to_vec(&ClientControlResponse::RouteOpen {
            route_channel: 17,
            route_epoch: 3,
        })
        .unwrap();
        assert!(
            dispatch_frame(
                &shared,
                1,
                response_frame(0, 0, command.frame.header.corr, response_body),
            )
            .await
        );
        let handle = task.await.unwrap().unwrap();
        (request, handle)
    }

    /// NEITHER a call NOR a subscribe emits bit 7 yet, and the subscribe arm is
    /// the load-bearing one.
    ///
    /// This test previously asserted the OPPOSITE for subscribes. It was
    /// inverted when the emission retreated, because a decoder built against
    /// subc-protocol <= 0.20.0 refuses bit 7 outright and the daemon forwards the
    /// flags byte verbatim to a module that never opted in. Emission is Phase 2
    /// of a two-phase fleet operation and the census has not happened.
    ///
    /// So this test EXISTS TO RED when someone restores `| FLAG_SUBSCRIPTION`
    /// without doing Phase 1 first. Its name says what it protects; do not
    /// "fix" it by flipping the assertion back.
    #[tokio::test]
    async fn neither_call_nor_subscribe_emits_bit_7_until_the_fleet_tolerates_it() {
        let (shared, mut receiver, handle) = reverse_test_shared(ReverseRequestRegistry::new());
        let call_shared = Arc::clone(&shared);
        let call = tokio::spawn(async move {
            call_shared
                .send_request(RequestSend {
                    expected_handle: Some(handle),
                    channel: handle.channel,
                    epoch: handle.epoch,
                    body: b"ordinary".to_vec(),
                    priority: Priority::Interactive,
                    admission_class: AdmissionClass::Normal,
                    deadline: Instant::now() + Duration::from_secs(1),
                    retain_late_route_open: false,
                    route_open_reverse_requests: None,
                })
                .await
        });
        let ordinary = receiver.recv().await.unwrap();
        assert_eq!(ordinary.frame.header.encode()[6] & FLAG_SUBSCRIPTION, 0);
        assert!(
            dispatch_frame(
                &shared,
                1,
                response_frame(
                    handle.channel,
                    handle.epoch,
                    ordinary.frame.header.corr,
                    Vec::new()
                ),
            )
            .await
        );
        assert!(call.await.unwrap().is_ok());

        let permit = Arc::new(Semaphore::new(1)).acquire_owned().await.unwrap();
        let subscription = Arc::clone(&shared)
            .send_subscription(SubscriptionSend {
                expected_handle: Some(handle),
                channel: handle.channel,
                epoch: handle.epoch,
                body: b"held".to_vec(),
                priority: Priority::Interactive,
                admission_class: AdmissionClass::Normal,
                event_buffer: 1,
                deadline: Instant::now() + Duration::from_secs(1),
                permit,
            })
            .await
            .unwrap();
        let held = receiver.recv().await.unwrap();
        assert_eq!(
            held.frame.header.encode()[6] & FLAG_SUBSCRIPTION,
            0,
            "a held-open subscribe must NOT set bit 7 until every module's decoder \
             accepts it; restoring emission here is Phase 2 and needs a fleet census"
        );
        drop(subscription);
        release_reverse_request_registry(handle);
    }

    #[tokio::test]
    async fn registered_handlers_derive_route_open_consumer_capabilities() {
        let registry = ReverseRequestRegistry::new();
        registry
            .on_request("elicitation", |_body, _ctx| async { Vec::new() })
            .unwrap();
        let (request, handle) = capture_route_open(registry).await;
        assert_eq!(
            request.get("consumer_capabilities"),
            Some(&serde_json::json!(["elicitation"]))
        );
        release_reverse_request_registry(handle);
    }

    #[tokio::test]
    async fn no_handlers_omit_consumer_capabilities_from_route_open() {
        let (request, handle) = capture_route_open(ReverseRequestRegistry::new()).await;
        assert!(!request
            .as_object()
            .unwrap()
            .contains_key("consumer_capabilities"));
        release_reverse_request_registry(handle);
    }

    #[tokio::test]
    async fn reverse_request_without_handler_returns_typed_error_instead_of_silence() {
        let (shared, mut receiver, handle) = reverse_test_shared(ReverseRequestRegistry::new());
        let body = serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "elicitation/create",
            "params": {}
        }))
        .unwrap();
        assert!(dispatch_frame(&shared, 1, reverse_request_frame(handle, 800, body)).await);
        let reply = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
            .await
            .unwrap()
            .unwrap()
            .frame;
        assert_eq!(reply.header.ty, FrameType::Error);
        assert_eq!(reply.header.corr, 800);
        let error: ErrorBody = serde_json::from_slice(&reply.body).unwrap();
        assert_eq!(error.code, REVERSE_REQUEST_UNHANDLED);
        release_reverse_request_registry(handle);
    }

    #[tokio::test]
    async fn reverse_request_round_trip_preserves_raw_body_corr_and_response() {
        let registry = ReverseRequestRegistry::new();
        let seen = Arc::new(Mutex::new(None));
        let seen_by_handler = Arc::clone(&seen);
        let response = br#"{"jsonrpc":"2.0","id":9,"result":{"accepted":true}}"#.to_vec();
        let expected_response = response.clone();
        registry
            .on_request("elicitation", move |body, ctx| {
                *seen_by_handler
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                    Some((body, ctx.corr, ctx.method));
                let response = response.clone();
                async move { response }
            })
            .unwrap();
        let (shared, mut receiver, handle) = reverse_test_shared(registry);
        let request_body =
            br#"{"jsonrpc":"2.0","id":9,"method":"elicitation/create","params":{"z":1,"a":2}}"#
                .to_vec();
        assert!(
            dispatch_frame(
                &shared,
                1,
                reverse_request_frame(handle, 801, request_body.clone()),
            )
            .await
        );
        let reply = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
            .await
            .unwrap()
            .unwrap()
            .frame;
        assert_eq!(reply.header.ty, FrameType::Response);
        assert_eq!(reply.header.corr, 801);
        assert_eq!(reply.body, expected_response);
        assert_eq!(
            seen.lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone(),
            Some((request_body, 801, "elicitation/create".to_string()))
        );
        release_reverse_request_registry(handle);
    }

    #[tokio::test]
    async fn panicking_reverse_handler_returns_typed_refusal_and_sibling_request_resolves() {
        let registry = ReverseRequestRegistry::new();
        registry
            .on_request("sampling", |_body, _ctx| async move {
                panic!("person prompt exploded");
            })
            .unwrap();
        let (shared, mut receiver, handle) = reverse_test_shared(registry);
        let sibling_shared = Arc::clone(&shared);
        let sibling = tokio::spawn(async move {
            sibling_shared
                .send_request(RequestSend {
                    expected_handle: Some(handle),
                    channel: handle.channel,
                    epoch: handle.epoch,
                    body: b"sibling".to_vec(),
                    priority: Priority::Interactive,
                    admission_class: AdmissionClass::Normal,
                    deadline: Instant::now() + Duration::from_secs(1),
                    retain_late_route_open: false,
                    route_open_reverse_requests: None,
                })
                .await
        });
        let sibling_request = receiver.recv().await.unwrap();
        let reverse_body = serde_json::to_vec(&serde_json::json!({
            "method": "sampling/createMessage",
            "params": {}
        }))
        .unwrap();
        assert!(
            dispatch_frame(&shared, 1, reverse_request_frame(handle, 802, reverse_body),).await
        );
        assert!(
            dispatch_frame(
                &shared,
                1,
                response_frame(
                    handle.channel,
                    handle.epoch,
                    sibling_request.frame.header.corr,
                    b"ok".to_vec(),
                ),
            )
            .await
        );
        assert!(matches!(
            sibling.await.unwrap().unwrap(),
            TerminalFrame::Response { body, .. } if body == b"ok"
        ));
        let refusal = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
            .await
            .unwrap()
            .unwrap()
            .frame;
        assert_eq!(refusal.header.ty, FrameType::Error);
        let error: ErrorBody = serde_json::from_slice(&refusal.body).unwrap();
        assert_eq!(error.code, REVERSE_REQUEST_UNHANDLED);
        assert!(error.message.contains("person prompt exploded"));
        assert!(shared.generation_is_current(1));
        release_reverse_request_registry(handle);
    }

    #[tokio::test]
    async fn writer_batches_ready_frames_into_one_flush() {
        const FRAME_COUNT: usize = 8;

        let shared = writer_test_shared();
        let (live_writer, _live_rx) = mpsc::channel(1);
        let (tx, rx) = mpsc::channel(FRAME_COUNT + 1);
        let instrumented = InstrumentedWriter::new(false);
        let observer = instrumented.clone();
        let mut expected = Vec::with_capacity(FRAME_COUNT);
        let mut keys = Vec::with_capacity(FRAME_COUNT);

        {
            let mut inner = shared.lock_inner();
            inner.writer = Some(live_writer);
            for index in 0..FRAME_COUNT {
                let corr = index as u64 + 1;
                let frame = response_frame(7, 1, corr, vec![index as u8; index + 1]);
                let key = PendingKey {
                    generation: 1,
                    channel: 7,
                    epoch: 1,
                    corr,
                };
                let (pending_tx, _pending_rx) = oneshot::channel();
                inner
                    .pending
                    .insert(key, PendingEntry::unary(pending_tx, false, None, None));
                expected.push(frame.clone());
                keys.push(key);
                tx.try_send(WriteCommand {
                    frame,
                    pending: Some(key),
                })
                .expect("the burst should fit in the writer queue");
                if index == 0 {
                    tx.try_send(WriteCommand {
                        frame: response_frame(7, 1, 999, b"skip".to_vec()),
                        pending: Some(PendingKey {
                            generation: 1,
                            channel: 7,
                            epoch: 1,
                            corr: 999,
                        }),
                    })
                    .expect("the skipped command should fit in the writer queue");
                }
            }
        }
        drop(tx);

        writer_loop(Arc::clone(&shared), instrumented, rx, 1).await;

        {
            let inner = shared.lock_inner();
            for key in keys {
                assert!(
                    inner.pending.get(&key).is_some_and(|entry| entry.accepted),
                    "every written command must be marked accepted"
                );
            }
        }

        let mut wire = std::io::Cursor::new(observer.bytes());
        for expected_frame in expected {
            let actual = read_frame(&mut wire)
                .await
                .expect("the emitted frame should decode")
                .expect("the emitted frame should be present");
            assert_eq!(actual, expected_frame);
        }
        assert!(
            read_frame(&mut wire)
                .await
                .expect("the end of the emitted burst should be clean")
                .is_none(),
            "the writer must not emit extra frames"
        );

        let flush_count = observer.flush_count();
        assert_eq!(
            flush_count, 1,
            "a ready burst must be coalesced into one flush"
        );
        shared.close_sync("test complete");
    }

    #[tokio::test]
    async fn writer_flush_failure_drops_generation_and_preserves_acceptance_classification() {
        let shared = writer_test_shared();
        let (live_writer, _live_rx) = mpsc::channel(1);
        let accepted_key = PendingKey {
            generation: 1,
            channel: 3,
            epoch: 1,
            corr: 1,
        };
        let not_sent_key = PendingKey {
            corr: 2,
            ..accepted_key
        };
        let (accepted_tx, accepted_rx) = oneshot::channel();
        let (not_sent_tx, not_sent_rx) = oneshot::channel();
        {
            let mut inner = shared.lock_inner();
            inner.writer = Some(live_writer);
            inner.pending.insert(
                accepted_key,
                PendingEntry::unary(accepted_tx, false, None, None),
            );
            inner.pending.insert(
                not_sent_key,
                PendingEntry::unary(not_sent_tx, false, None, None),
            );
        }

        let (tx, rx) = mpsc::channel(1);
        tx.send(WriteCommand {
            frame: response_frame(3, 1, accepted_key.corr, b"accepted".to_vec()),
            pending: Some(accepted_key),
        })
        .await
        .unwrap();
        drop(tx);

        writer_loop(Arc::clone(&shared), InstrumentedWriter::new(true), rx, 1).await;

        assert!(
            shared.lock_inner().writer.is_none(),
            "a flush failure must drop the active generation"
        );
        let accepted_error = accepted_rx
            .await
            .expect("the accepted request should be settled")
            .into_call_result()
            .unwrap_err();
        assert!(matches!(accepted_error, CallError::OutcomeUnknown(_)));
        let not_sent_error = not_sent_rx
            .await
            .expect("the unwritten request should be settled")
            .into_call_result()
            .unwrap_err();
        assert!(matches!(not_sent_error, CallError::NotSent(_)));
        shared.close_sync("test complete");
    }

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

    #[tokio::test]
    async fn newer_drop_supersedes_reconnect_and_ignores_stale_completion() {
        let shared = Arc::new(Shared::new(
            PathBuf::from("/tmp/does-not-exist"),
            ConsumerOptions::default(),
        ));
        let stale_task = tokio::spawn(std::future::pending::<()>());
        {
            let mut inner = shared.lock_inner();
            inner.generation = 2;
            inner.reconnect = ReconnectState::Background {
                generation: 1,
                task: stale_task,
            };
        }

        assert!(shared.spawn_reconnect(2));
        assert!(matches!(
            &shared.lock_inner().reconnect,
            ReconnectState::Background { generation, .. } if *generation == 2
        ));

        shared.finish_background_reconnect(1);
        assert!(matches!(
            &shared.lock_inner().reconnect,
            ReconnectState::Background { generation, .. } if *generation == 2
        ));
        shared.close_sync("test complete");
    }

    #[test]
    fn retryable_route_open_codes_are_code_specific() {
        for code in [
            "module_reloading",
            "module_warming",
            "target_unavailable",
            "module_timeout",
        ] {
            assert!(is_retryable_route_open_code(code), "{code} should retry");
        }
        // "No module of this id is registered or supervised here" is a typo or
        // an undeployed peer: the daemon reports a configured-but-late target
        // with module_warming/target_unavailable, so retrying unknown_module
        // in place only papers over an unsupervised module's HELLO race.
        assert!(
            !is_retryable_route_open_code(error_codes::UNKNOWN_MODULE),
            "unknown_module is terminal"
        );
        assert!(!is_retryable_route_open_code(error_codes::MODULE_REMOVED));
        assert!(!is_retryable_route_open_code("invalid_project_root"));
        assert!(!is_retryable_route_open_code("route_rejected"));
        assert!(
            !is_retryable_route_open_code("capability_forbidden"),
            "a capability policy refusal must not enter the route.open retry set"
        );
    }

    #[test]
    fn route_close_reason_classifier_accepts_capability_denied_and_fails_closed_for_unknown() {
        assert_eq!(
            RouteCloseReason::from_wire("capability_denied"),
            RouteCloseReason::CapabilityDenied
        );
        assert_eq!(
            RouteCloseReason::from_wire("capability_denied").disposition(),
            RouteCloseDisposition::MustNotReopen
        );
        assert_eq!(
            RouteCloseReason::from_wire("future_policy_reason").disposition(),
            RouteCloseDisposition::MustNotReopen,
            "an unknown close reason must receive the strictest handling"
        );
        assert_eq!(
            RouteCloseReason::from_wire("reload").disposition(),
            RouteCloseDisposition::MayReopen,
            "control proves the classifier can distinguish a conservative default"
        );
    }

    #[tokio::test]
    async fn module_removed_fails_fast_while_module_reloading_retries_at_the_same_route_open_call_site(
    ) {
        // Serves rejections with `code` until the caller settles, returning
        // (attempts_served, result). The retryable arm must keep retrying past
        // any attempt count until the DEADLINE binds — the attempt cap in the
        // options below is deliberately tiny so a regression that re-couples
        // attempts into the retry condition (the 3.1s-effective-budget defect)
        // stops the loop at 2 and fails the `> 2` assertion by name.
        async fn reject_route_open_attempts(code: &str, deadline: Duration) -> (usize, CallError) {
            let shared = Arc::new(Shared::new(
                PathBuf::from("/tmp/does-not-exist"),
                ConsumerOptions::default(),
            ));
            let (writer, mut receiver) = mpsc::channel(4);
            {
                let mut inner = shared.lock_inner();
                inner.writer = Some(writer);
            }
            let consumer = SubcConsumer {
                shared: Arc::clone(&shared),
            };
            let target = RouteTarget::ToolProvider {
                module_id: "retry-polarity".to_string(),
            };
            let identity = BindIdentity::new(
                PathBuf::from("/tmp/project"),
                "test".to_string(),
                code.to_string(),
            );
            let options = CallOptions {
                timeout: Duration::from_secs(2),
                route_retry: RetryBackoff {
                    base: Duration::ZERO,
                    cap: Duration::ZERO,
                    max_attempts: 2,
                },
                route_retry_deadline: deadline,
                ..CallOptions::default()
            };
            let mut task =
                tokio::spawn(async move { consumer.open_route(target, identity, options).await });

            let mut attempts = 0usize;
            let result = loop {
                tokio::select! {
                    command = receiver.recv() => {
                        // The consumer tears the writer down when the open
                        // settles, so a closed channel here means the task is
                        // finishing — join it rather than treating the race as
                        // a broken harness.
                        let Some(command) = command else {
                            break task.await.unwrap().expect_err("route.open must reject");
                        };
                        attempts += 1;
                        let body =
                            serde_json::to_vec(&ErrorBody::new(code, "test rejection")).unwrap();
                        assert!(
                            dispatch_frame(
                                &shared,
                                1,
                                Frame::build(
                                    FrameType::Error,
                                    Flags::new(false, Priority::Interactive, false),
                                    0,
                                    0,
                                    command.frame.header.corr,
                                    body,
                                )
                                .unwrap(),
                            )
                            .await
                        );
                    }
                    joined = &mut task => {
                        break joined.unwrap().expect_err("route.open must reject");
                    }
                }
            };
            assert!(
                receiver.try_recv().is_err(),
                "a settled route.open must not queue another attempt"
            );
            (attempts, result)
        }

        let (reloading_attempts, reloading) =
            reject_route_open_attempts(error_codes::MODULE_RELOADING, Duration::from_millis(150))
                .await;
        assert!(matches!(reloading, CallError::NotSent(_)));
        assert!(
            reloading_attempts > 2,
            "reloading retries must run until the deadline, not an attempt cap \
             (served {reloading_attempts} attempts against max_attempts=2)"
        );

        let (removed_attempts, removed) =
            reject_route_open_attempts(error_codes::MODULE_REMOVED, Duration::from_millis(150))
                .await;
        assert!(matches!(removed, CallError::NotSent(_)));
        assert_eq!(
            removed_attempts, 1,
            "terminal codes settle on the first answer"
        );
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
            &BindIdentity::new(PathBuf::from("/tmp/p"), "h", "s"),
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
            &BindIdentity::new(PathBuf::from("/tmp/p"), "h", "s"),
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
        let identity = BindIdentity::new(PathBuf::from("/tmp/project"), "h", "s");
        let key = RouteKey::new(&target, &identity, None, None);
        assert_eq!(key.project_root, PathBuf::from("/tmp/project"));
        assert!(matches!(key.target, RouteTargetKey::InternalService { .. }));
    }

    #[tokio::test]
    async fn route_channel_index_tracks_lookup_close_and_generation_drop() {
        let shared = writer_test_shared();
        let (writer, _rx) = mpsc::channel(32);
        let mut expected = Vec::new();
        {
            let mut inner = shared.lock_inner();
            inner.writer = Some(writer);
            for channel in 1..=8 {
                let key = RouteKey::new(
                    &RouteTarget::ToolProvider {
                        module_id: format!("module-{channel}"),
                    },
                    &BindIdentity::new(
                        PathBuf::from("/tmp/project"),
                        "test",
                        format!("session-{channel}"),
                    ),
                    None,
                    None,
                );
                let route = RouteState {
                    handle: RouteHandle::new(channel, channel.into(), 1),
                    sem: Arc::new(Semaphore::new(DEFAULT_ROUTE_WINDOW)),
                };
                inner.cache_route(key.clone(), route.clone());
                expected.push((key, route));
            }
        }

        for (key, expected_route) in &expected {
            let resolved = shared
                .route_state(expected_route.handle)
                .expect("an indexed route handle should resolve");
            assert_eq!(resolved.handle, expected_route.handle);
            assert!(Arc::ptr_eq(&resolved.sem, &expected_route.sem));

            let inner = shared.lock_inner();
            assert_eq!(
                inner.route_by_channel.get(&expected_route.handle.channel),
                Some(key)
            );
            assert_eq!(
                inner.route_epochs.get(&expected_route.handle.channel),
                Some(&expected_route.handle)
            );
            assert!(inner
                .routes
                .get(key)
                .is_some_and(|route| route.handle == expected_route.handle));
        }

        let (closed_key, closed_route) = &expected[3];
        shared
            .close_route(closed_key, &CloseRouteOptions::default())
            .await;
        {
            let inner = shared.lock_inner();
            assert!(!inner.routes.contains_key(closed_key));
            assert!(!inner
                .route_by_channel
                .contains_key(&closed_route.handle.channel));
            assert!(!inner
                .route_epochs
                .contains_key(&closed_route.handle.channel));
        }
        assert!(matches!(
            shared.route_state(closed_route.handle),
            Err(CallError::StaleRouteHandle(handle)) if handle == closed_route.handle
        ));

        shared.handle_generation_drop(1, "test generation dropped".into());
        {
            let inner = shared.lock_inner();
            assert!(inner.routes.is_empty());
            assert!(inner.route_by_channel.is_empty());
            assert!(inner.route_epochs.is_empty());
        }
        shared.close_sync("test complete");
    }

    #[tokio::test]
    async fn stale_push_is_not_delivered_after_connection_generation_changes() {
        let shared = writer_test_shared();
        let old_handle = RouteHandle::new(7, 1, 1);
        let (writer, _writer_rx) = mpsc::channel(1);
        {
            let mut inner = shared.lock_inner();
            inner.writer = Some(writer);
            inner.route_epochs.insert(old_handle.channel, old_handle);
        }
        let mut pushes = shared
            .register_push_events(old_handle)
            .expect("the old live route should accept a receiver");

        {
            let mut inner = shared.lock_inner();
            inner.generation = 2;
            inner.close_routes();
            inner.route_epochs.clear();
        }
        shared.route_push(old_handle, b"stale".to_vec());

        assert!(
            pushes.recv().await.is_none(),
            "connection teardown must end the old receiver before a stale Push can arrive"
        );
        shared.close_sync("test complete");
    }

    #[test]
    fn route_key_canonicalizes_consumer_capabilities() {
        let target = RouteTarget::ToolProvider {
            module_id: "aft".into(),
        };
        let identity = BindIdentity::new(PathBuf::from("/tmp/project"), "h", "s");
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
        let handle = RouteHandle::new(channel, 3, generation);
        let unary_key = PendingKey {
            generation,
            channel,
            epoch: handle.epoch,
            corr: 1,
        };
        let subscription_key = PendingKey {
            generation,
            channel,
            epoch: handle.epoch,
            corr: 2,
        };
        let (unary_tx, _unary_rx) = oneshot::channel();
        pending.insert(unary_key, PendingEntry::unary(unary_tx, false, None, None));

        let (events_tx, _events_rx) = mpsc::channel(1);
        let (closed_tx, _closed_rx) = oneshot::channel();
        let permit = Arc::new(Semaphore::new(1))
            .try_acquire_owned()
            .expect("test semaphore permit should be available");
        pending.insert(
            subscription_key,
            PendingEntry::subscription(events_tx, closed_tx, permit, Priority::Interactive),
        );

        let drained = drain_pending_handle(&mut pending, handle, false);
        assert_eq!(drained.len(), 1);
        assert!(pending.contains_key(&subscription_key));

        let drained = drain_pending_handle(&mut pending, handle, true);
        assert_eq!(drained.len(), 1);
        assert!(pending.is_empty());
    }

    fn response_frame(channel: u16, epoch: u32, corr: u64, body: Vec<u8>) -> Frame {
        Frame::build(
            FrameType::Response,
            Flags::new(false, Priority::Interactive, false),
            channel,
            epoch,
            corr,
            body,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn stale_epoch_ingress_drops_without_settling_matching_corr() {
        let shared = Arc::new(Shared::new(
            PathBuf::from("/tmp/does-not-exist"),
            ConsumerOptions::default(),
        ));
        let (writer, _rx) = mpsc::channel(4);
        let current = RouteHandle::new(9, 2, 1);
        let stale_key = PendingKey {
            generation: 1,
            channel: 9,
            epoch: 1,
            corr: 77,
        };
        let key = PendingKey {
            generation: 1,
            channel: 9,
            epoch: 2,
            corr: 77,
        };
        let (stale_tx, mut stale_response) = oneshot::channel();
        let (tx, mut response) = oneshot::channel();
        {
            let mut inner = shared.lock_inner();
            inner.writer = Some(writer);
            inner.route_epochs.insert(9, current);
            inner
                .pending
                .insert(stale_key, PendingEntry::unary(stale_tx, false, None, None));
            inner
                .pending
                .insert(key, PendingEntry::unary(tx, false, None, None));
        }

        assert!(dispatch_frame(&shared, 1, response_frame(9, 1, 77, b"stale".to_vec())).await);
        assert!(matches!(
            response.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert!(matches!(
            stale_response.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert!(shared.lock_inner().pending.contains_key(&stale_key));
        assert!(shared.lock_inner().pending.contains_key(&key));
        assert_eq!(shared.lock_inner().dropped_route_frames, 1);

        assert!(dispatch_frame(&shared, 1, response_frame(9, 2, 77, b"current".to_vec())).await);
        let PendingResult::Terminal(PendingTerminal::Response { body, .. }) =
            response.await.unwrap()
        else {
            panic!("current epoch must settle its own pending request");
        };
        assert_eq!(body, b"current");
        assert!(shared.lock_inner().pending.contains_key(&stale_key));
    }

    #[tokio::test]
    async fn route_poll_response_must_echo_expected_handle_before_settling() {
        let shared = Arc::new(Shared::new(
            PathBuf::from("/tmp/does-not-exist"),
            ConsumerOptions::default(),
        ));
        let (writer, _rx) = mpsc::channel(4);
        let handle = RouteHandle::new(3, 9, 1);
        let key = PendingKey {
            generation: 1,
            channel: 0,
            epoch: 0,
            corr: 88,
        };
        let (tx, mut response) = oneshot::channel();
        {
            let mut inner = shared.lock_inner();
            inner.writer = Some(writer);
            inner.route_epochs.insert(handle.channel, handle);
            inner
                .pending
                .insert(key, PendingEntry::unary(tx, false, Some(handle), None));
        }
        let wrong = serde_json::to_vec(&ClientControlResponse::RoutePoll {
            route_channel: handle.channel,
            route_epoch: handle.epoch + 1,
            status: Some("wrong".to_string()),
            live: Some(true),
        })
        .unwrap();
        assert!(dispatch_frame(&shared, 1, response_frame(0, 0, key.corr, wrong)).await);
        assert!(matches!(
            response.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert!(shared.lock_inner().pending.contains_key(&key));

        let correct = serde_json::to_vec(&ClientControlResponse::RoutePoll {
            route_channel: handle.channel,
            route_epoch: handle.epoch,
            status: Some("ready".to_string()),
            live: Some(true),
        })
        .unwrap();
        assert!(dispatch_frame(&shared, 1, response_frame(0, 0, key.corr, correct)).await);
        assert!(matches!(
            response.await.unwrap(),
            PendingResult::Terminal(PendingTerminal::Response { .. })
        ));
    }

    #[tokio::test]
    async fn stale_connection_handle_emits_no_request_cancel_or_goodbye() {
        let shared = Arc::new(Shared::new(
            PathBuf::from("/tmp/does-not-exist"),
            ConsumerOptions::default(),
        ));
        let (writer, mut rx) = mpsc::channel(4);
        let stale = RouteHandle::new(4, 1, 1);
        let current = RouteHandle::new(4, 1, 2);
        {
            let mut inner = shared.lock_inner();
            inner.generation = 2;
            inner.writer = Some(writer);
            inner.route_epochs.insert(4, current);
        }

        let err = shared
            .send_request(RequestSend {
                expected_handle: Some(stale),
                channel: stale.channel,
                epoch: stale.epoch,
                body: b"request".to_vec(),
                priority: Priority::Interactive,
                admission_class: AdmissionClass::Normal,
                deadline: Instant::now() + Duration::from_millis(10),
                retain_late_route_open: false,
                route_open_reverse_requests: None,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, CallError::StaleRouteHandle(handle) if handle == stale));
        shared.send_cancel(stale, 8, Priority::Interactive);
        assert!(!shared.send_route_goodbye(stale, false));
        assert!(
            rx.try_recv().is_err(),
            "stale operations must not queue frames"
        );
    }

    #[tokio::test]
    async fn late_route_open_queues_goodbye_and_full_queue_closes_connection() {
        let shared = Arc::new(Shared::new(
            PathBuf::from("/tmp/does-not-exist"),
            ConsumerOptions::default(),
        ));
        let (writer, mut rx) = mpsc::channel(2);
        let key = PendingKey {
            generation: 1,
            channel: 0,
            epoch: 0,
            corr: 41,
        };
        let (tx, response) = oneshot::channel();
        drop(response);
        {
            let mut inner = shared.lock_inner();
            inner.writer = Some(writer);
            inner
                .pending
                .insert(key, PendingEntry::unary(tx, true, None, None));
        }
        let body = serde_json::to_vec(&ClientControlResponse::RouteOpen {
            route_channel: 12,
            route_epoch: 7,
        })
        .unwrap();
        assert!(dispatch_frame(&shared, 1, response_frame(0, 0, 41, body)).await);
        let cleanup = rx.recv().await.unwrap().frame;
        assert_eq!(cleanup.header.ty, FrameType::Goodbye);
        assert_eq!((cleanup.header.channel, cleanup.header.epoch), (12, 7));

        let shared = Arc::new(Shared::new(
            PathBuf::from("/tmp/does-not-exist"),
            ConsumerOptions {
                reconnect_backoff: RetryBackoff {
                    max_attempts: 1,
                    ..RetryBackoff::default()
                },
                ..ConsumerOptions::default()
            },
        ));
        let (writer, _rx) = mpsc::channel(1);
        let filler = response_frame(0, 0, 1, Vec::new());
        writer
            .try_send(WriteCommand {
                frame: filler,
                pending: None,
            })
            .unwrap();
        let key = PendingKey {
            generation: 1,
            channel: 0,
            epoch: 0,
            corr: 42,
        };
        let (tx, response) = oneshot::channel();
        drop(response);
        {
            let mut inner = shared.lock_inner();
            inner.writer = Some(writer);
            inner
                .pending
                .insert(key, PendingEntry::unary(tx, true, None, None));
        }
        let body = serde_json::to_vec(&ClientControlResponse::RouteOpen {
            route_channel: 13,
            route_epoch: 8,
        })
        .unwrap();
        assert!(dispatch_frame(&shared, 1, response_frame(0, 0, 42, body)).await);
        assert!(shared.lock_inner().writer.is_none());
    }

    #[tokio::test]
    async fn correlation_allocator_emits_max_once_then_closes_without_reuse() {
        let shared = Arc::new(Shared::new(
            PathBuf::from("/tmp/does-not-exist"),
            ConsumerOptions {
                reconnect_backoff: RetryBackoff {
                    max_attempts: 1,
                    ..RetryBackoff::default()
                },
                ..ConsumerOptions::default()
            },
        ));
        let (writer, mut rx) = mpsc::channel(4);
        {
            let mut inner = shared.lock_inner();
            inner.writer = Some(writer);
            inner.next_corr = Some(u64::MAX);
        }
        let request_shared = Arc::clone(&shared);
        let request = tokio::spawn(async move {
            request_shared
                .send_request(RequestSend {
                    expected_handle: None,
                    channel: 0,
                    epoch: 0,
                    body: Vec::new(),
                    priority: Priority::Interactive,
                    admission_class: AdmissionClass::Normal,
                    deadline: Instant::now() + Duration::from_secs(1),
                    retain_late_route_open: false,
                    route_open_reverse_requests: None,
                })
                .await
        });
        let command = rx.recv().await.unwrap();
        assert_eq!(command.frame.header.corr, u64::MAX);
        assert!(dispatch_frame(&shared, 1, response_frame(0, 0, u64::MAX, Vec::new()),).await);
        assert!(request.await.unwrap().is_ok());

        let exhausted = shared
            .send_request(RequestSend {
                expected_handle: None,
                channel: 0,
                epoch: 0,
                body: Vec::new(),
                priority: Priority::Interactive,
                admission_class: AdmissionClass::Normal,
                deadline: Instant::now() + Duration::from_millis(10),
                retain_late_route_open: false,
                route_open_reverse_requests: None,
            })
            .await
            .unwrap_err();
        assert!(matches!(exhausted, CallError::NotSent(_)));
        assert!(rx.try_recv().is_err());
        assert!(shared.lock_inner().writer.is_none());
    }

    #[tokio::test]
    async fn managed_call_deadline_bounds_flow_control_wait() {
        let shared = Arc::new(Shared::new(
            PathBuf::from("/tmp/does-not-exist"),
            ConsumerOptions::default(),
        ));
        let (writer, mut rx) = mpsc::channel(4);
        let target = RouteTarget::ToolProvider {
            module_id: "flow-controlled".to_string(),
        };
        let identity = BindIdentity::new(
            PathBuf::from("/tmp/project"),
            "test".to_string(),
            "deadline".to_string(),
        );
        let consumer_identity = Some(ConsumerIdentity {
            module_id: "caller".to_string(),
            launch_nonce: "nonce".to_string(),
        });
        let first_opts = CallOptions {
            timeout: Duration::from_secs(1),
            consumer_identity: consumer_identity.clone(),
            ..CallOptions::default()
        };
        let second_opts = CallOptions {
            timeout: Duration::from_millis(25),
            consumer_identity,
            ..CallOptions::default()
        };
        let key = RouteKey::new(
            &target,
            &identity,
            first_opts.consumer_identity.as_ref(),
            None,
        );
        let handle = RouteHandle::new(5, 3, 1);
        {
            let mut inner = shared.lock_inner();
            inner.writer = Some(writer);
            inner.cache_route(
                key,
                RouteState {
                    handle,
                    sem: Arc::new(Semaphore::new(1)),
                },
            );
        }

        let first = tokio::spawn({
            let consumer = SubcConsumer {
                shared: Arc::clone(&shared),
            };
            let target = target.clone();
            let identity = identity.clone();
            async move {
                consumer
                    .call(target, identity, b"first".to_vec(), first_opts)
                    .await
            }
        });
        let first_frame = rx
            .recv()
            .await
            .expect("the first request should enter the fake daemon queue");
        assert_eq!(first_frame.frame.header.ty, FrameType::Request);
        assert_eq!(first_frame.frame.body, b"first");
        assert!(shared.mark_pending_accepted(
            first_frame
                .pending
                .expect("request commands retain their pending key"),
        ));

        let consumer = SubcConsumer {
            shared: Arc::clone(&shared),
        };
        let result = tokio::time::timeout(
            Duration::from_millis(250),
            consumer.call(target, identity, b"second".to_vec(), second_opts),
        )
        .await
        .expect("a flow-controlled call must finish at its own deadline")
        .unwrap_err();
        assert!(matches!(result, CallError::NotSent(_)));
        assert!(
            rx.try_recv().is_err(),
            "the timed-out second request must not reach the fake daemon"
        );

        first.abort();
        let _ = first.await;
    }

    #[tokio::test]
    async fn admitted_route_open_emits_one_frame_without_retrying_daemon_errors() {
        let shared = Arc::new(Shared::new(
            PathBuf::from("/tmp/does-not-exist"),
            ConsumerOptions {
                call_timeout: Duration::from_secs(1),
                ..ConsumerOptions::default()
            },
        ));
        let (writer, mut rx) = mpsc::channel(4);
        {
            let mut inner = shared.lock_inner();
            inner.writer = Some(writer);
        }

        let consumer = SubcConsumer {
            shared: Arc::clone(&shared),
        };
        let target = RouteTarget::ToolProvider {
            module_id: "admitted-target".to_string(),
        };
        let identity = BindIdentity::new(
            PathBuf::from("/tmp/project"),
            "test".to_string(),
            "admitted".to_string(),
        );
        let task = tokio::spawn(async move {
            consumer
                .open_route_with_admission_facts(
                    target,
                    identity,
                    serde_json::json!({"schema": 1, "verified_class": "member"}),
                )
                .await
        });

        let command = rx.recv().await.expect("one route.open must be queued");
        let request: ClientControlRequest = serde_json::from_slice(&command.frame.body).unwrap();
        let ClientControlRequest::RouteOpen {
            admission_facts, ..
        } = request
        else {
            panic!("expected route.open")
        };
        assert_eq!(
            admission_facts,
            Some(serde_json::json!({"schema": 1, "verified_class": "member"}))
        );

        let error_body = serde_json::to_vec(&ErrorBody {
            code: "admission_facts_not_permitted".to_string(),
            message: "not permitted".to_string(),
            detail: None,
        })
        .unwrap();
        assert!(
            dispatch_frame(
                &shared,
                1,
                Frame::build(
                    FrameType::Error,
                    Flags::new(false, Priority::Interactive, false),
                    0,
                    0,
                    command.frame.header.corr,
                    error_body,
                )
                .unwrap(),
            )
            .await
        );
        let result = task.await.unwrap();
        assert!(matches!(result, Err(CallError::NotSent(_))));
        assert!(rx.try_recv().is_err(), "one-shot route.open must not retry");
    }

    /// Drive one admitted route.open to success on a test writer and return the
    /// route.open request that was sent, the resolved handle, and the writer.
    async fn open_admitted_route_for_test(
        opts: Option<CallOptions>,
    ) -> (
        Arc<Shared>,
        SubcConsumer,
        ClientControlRequest,
        Result<RouteHandle, CallError>,
        mpsc::Receiver<WriteCommand>,
    ) {
        let shared = writer_test_shared();
        let (writer, mut rx) = mpsc::channel(8);
        let generation = {
            let mut inner = shared.lock_inner();
            inner.writer = Some(writer);
            inner.generation
        };
        let consumer = SubcConsumer {
            shared: Arc::clone(&shared),
        };
        let target = RouteTarget::ToolProvider {
            module_id: "cerebellum".to_string(),
        };
        let identity = BindIdentity::new(
            PathBuf::from("/tmp/project"),
            "test".to_string(),
            "admitted".to_string(),
        );
        let facts = serde_json::json!({"schema": 1, "verified_class": "member"});
        // The consumer is returned from the task and kept alive by the caller:
        // dropping it closes the connection, after which no frame is dispatched.
        let task = tokio::spawn(async move {
            let result = match opts {
                Some(opts) => {
                    consumer
                        .open_route_with_admission_facts_and_options(target, identity, facts, opts)
                        .await
                }
                None => {
                    consumer
                        .open_route_with_admission_facts(target, identity, facts)
                        .await
                }
            };
            (consumer, result)
        });
        let command = rx.recv().await.expect("one route.open must be queued");
        let request: ClientControlRequest = serde_json::from_slice(&command.frame.body).unwrap();
        let response = serde_json::to_vec(&ClientControlResponse::RouteOpen {
            route_channel: 21,
            route_epoch: 4,
        })
        .unwrap();
        assert!(
            dispatch_frame(
                &shared,
                generation,
                Frame::build(
                    FrameType::Response,
                    Flags::new(false, Priority::Interactive, false),
                    0,
                    0,
                    command.frame.header.corr,
                    response,
                )
                .unwrap(),
            )
            .await
        );
        let (consumer, result) = tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("admitted route.open resolves")
            .unwrap();
        (shared, consumer, request, result, rx)
    }

    #[tokio::test]
    async fn admitted_route_open_refusal_keeps_the_daemon_code_and_detail() {
        let shared = writer_test_shared();
        let (writer, mut rx) = mpsc::channel(8);
        let generation = {
            let mut inner = shared.lock_inner();
            inner.writer = Some(writer);
            inner.generation
        };
        let consumer = SubcConsumer {
            shared: Arc::clone(&shared),
        };
        let task = tokio::spawn(async move {
            let result = consumer
                .open_route_with_admission_facts(
                    RouteTarget::ToolProvider {
                        module_id: "cerebellum".to_string(),
                    },
                    BindIdentity::new(
                        PathBuf::from("/tmp/project"),
                        "test".to_string(),
                        "admitted".to_string(),
                    ),
                    serde_json::json!({"schema": 1, "verified_class": "member"}),
                )
                .await;
            (consumer, result)
        });
        let command = rx.recv().await.expect("one route.open must be queued");
        let refusal = ErrorBody {
            code: "module_warming".to_string(),
            message: "cerebellum is not ready".to_string(),
            detail: Some(serde_json::json!({"reason": "declared_not_ready"})),
        };
        assert!(
            dispatch_frame(
                &shared,
                generation,
                Frame::build(
                    FrameType::Error,
                    Flags::new(false, Priority::Interactive, false),
                    0,
                    0,
                    command.frame.header.corr,
                    serde_json::to_vec(&refusal).unwrap(),
                )
                .unwrap(),
            )
            .await
        );
        let (_consumer, result) = tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("admitted route.open resolves")
            .unwrap();
        let error = result.expect_err("a refused open is an error");
        assert!(
            error.is_not_sent(),
            "a refused open never reached the module: {error}"
        );
        let body = error
            .route_open_refusal()
            .unwrap_or_else(|| panic!("the refusal's code must survive: {error}"));
        assert_eq!(body.code, "module_warming");
        assert_eq!(
            body.detail,
            Some(serde_json::json!({"reason": "declared_not_ready"}))
        );
        assert!(
            error.to_string().contains("tool_provider:cerebellum"),
            "the error names its target: {error}"
        );
    }

    #[tokio::test]
    async fn admitted_route_without_options_opens_and_declares_no_reverse_capabilities() {
        let (_shared, _consumer, request, result, _rx) = open_admitted_route_for_test(None).await;
        let ClientControlRequest::RouteOpen {
            consumer_capabilities,
            admission_facts,
            ..
        } = request
        else {
            panic!("expected route.open")
        };
        assert_eq!(consumer_capabilities, None);
        assert!(admission_facts.is_some());
        let handle = result.expect("admitted route opens");
        assert_eq!((handle.channel, handle.epoch), (21, 4));
    }

    // An admitted route opened with handlers declares their capabilities and
    // delivers the provider's reverse requests to them, replying under the
    // provider's own correlation id: the browser plane's consent prompts arrive
    // as elicitation on exactly this kind of route.
    #[tokio::test]
    async fn admitted_route_with_options_delivers_elicitation_to_its_handler() {
        let opts = CallOptions::default();
        let seen = Arc::new(Mutex::new(None));
        let seen_by_handler = Arc::clone(&seen);
        let reply_body = br#"{"jsonrpc":"2.0","id":3,"result":{"action":"accept"}}"#.to_vec();
        let expected_reply = reply_body.clone();
        opts.reverse_requests
            .on_request("elicitation", move |body, ctx| {
                *seen_by_handler
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some((body, ctx.corr));
                let reply_body = reply_body.clone();
                async move { reply_body }
            })
            .unwrap();

        let (shared, _consumer, request, result, mut rx) =
            open_admitted_route_for_test(Some(opts)).await;
        let ClientControlRequest::RouteOpen {
            consumer_capabilities,
            admission_facts,
            ..
        } = request
        else {
            panic!("expected route.open")
        };
        assert_eq!(consumer_capabilities, Some(vec!["elicitation".to_string()]));
        assert_eq!(
            admission_facts,
            Some(serde_json::json!({"schema": 1, "verified_class": "member"}))
        );
        let handle = result.expect("admitted route opens");

        let request_body =
            br#"{"jsonrpc":"2.0","id":3,"method":"elicitation/create","params":{}}"#.to_vec();
        assert!(
            dispatch_frame(
                &shared,
                handle.connection_token(),
                reverse_request_frame(handle, 900, request_body.clone()),
            )
            .await
        );
        let reply = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("the handler replies")
            .unwrap()
            .frame;
        assert_eq!(reply.header.ty, FrameType::Response);
        assert_eq!(reply.header.corr, 900);
        assert_eq!(reply.header.channel, 21);
        assert_eq!(reply.body, expected_reply);
        assert_eq!(
            seen.lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone(),
            Some((request_body, 900))
        );
    }

    #[tokio::test]
    async fn route_open_waiter_deadline_is_not_sent_without_writing() {
        let shared = Arc::new(Shared::new(
            PathBuf::from("/tmp/does-not-exist"),
            ConsumerOptions::default(),
        ));
        let (writer, mut rx) = mpsc::channel(4);
        let target = RouteTarget::ToolProvider {
            module_id: "single-flight".to_string(),
        };
        let identity = BindIdentity::new(
            PathBuf::from("/tmp/project"),
            "test".to_string(),
            "route-open".to_string(),
        );
        let opts = CallOptions {
            timeout: Duration::from_millis(25),
            consumer_identity: Some(ConsumerIdentity {
                module_id: "caller".to_string(),
                launch_nonce: "nonce".to_string(),
            }),
            ..CallOptions::default()
        };
        let key = RouteKey::new(&target, &identity, opts.consumer_identity.as_ref(), None);
        {
            let mut inner = shared.lock_inner();
            inner.writer = Some(writer);
            inner.openings.insert(
                key,
                Opening {
                    waiters: Vec::new(),
                    closed: false,
                },
            );
        }

        let consumer = SubcConsumer { shared };
        let result = tokio::time::timeout(
            Duration::from_millis(250),
            consumer.open_route(target, identity, opts),
        )
        .await
        .expect("a route.open waiter must finish at its own deadline")
        .unwrap_err();
        assert!(matches!(result, CallError::NotSent(_)));
        assert!(
            rx.try_recv().is_err(),
            "a timed-out route.open waiter must not write a control frame"
        );
    }

    // A route.open whose control reply never arrives before the deadline has an
    // unknown outcome of its own (the daemon may have opened the channel), but
    // the managed call's request body was never written. The call and the
    // subscription must therefore report NotSent, not OutcomeUnknown: callers
    // use that distinction to decide whether a non-idempotent retry is safe.
    async fn managed_op_with_unanswered_route_open(subscribe: bool) -> CallError {
        let shared = Arc::new(Shared::new(
            PathBuf::from("/tmp/does-not-exist"),
            ConsumerOptions::default(),
        ));
        let (writer, mut rx) = mpsc::channel(4);
        {
            let mut inner = shared.lock_inner();
            inner.writer = Some(writer);
        }
        let consumer = SubcConsumer {
            shared: Arc::clone(&shared),
        };
        let target = RouteTarget::ToolProvider {
            module_id: "slow-route-open".to_string(),
        };
        let identity = BindIdentity::new(
            PathBuf::from("/tmp/project"),
            "test".to_string(),
            "slow-route-open".to_string(),
        );
        let task = tokio::spawn(async move {
            if subscribe {
                consumer
                    .subscribe(
                        target,
                        identity,
                        b"{}".to_vec(),
                        SubscribeOptions {
                            route_open_timeout: Duration::from_millis(200),
                            route_retry_deadline: Duration::from_millis(100),
                            ..SubscribeOptions::default()
                        },
                    )
                    .await
                    .map(|_| ())
            } else {
                consumer
                    .call(
                        target,
                        identity,
                        b"{}".to_vec(),
                        CallOptions {
                            timeout: Duration::from_millis(200),
                            route_retry_deadline: Duration::from_millis(100),
                            ..CallOptions::default()
                        },
                    )
                    .await
                    .map(|_| ())
            }
        });

        // Play the writer task: accept the route.open onto the wire (so the
        // consumer knows the control frame left) and then never answer it,
        // which is what a daemon under load looks like from the client.
        let command = rx.recv().await.expect("route.open must be queued");
        assert_eq!(command.frame.header.channel, 0);
        let request: ClientControlRequest = serde_json::from_slice(&command.frame.body).unwrap();
        assert!(matches!(request, ClientControlRequest::RouteOpen { .. }));
        assert!(shared.mark_pending_accepted(command.pending.expect("route.open is tracked")));

        let err = tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("the managed op must settle at its own deadline")
            .unwrap()
            .expect_err("an unanswered route.open cannot yield a route");
        while let Ok(command) = rx.try_recv() {
            assert_eq!(
                command.frame.header.channel, 0,
                "no data-plane frame may be written without an open route"
            );
        }
        err
    }

    #[tokio::test]
    async fn call_whose_route_open_times_out_after_send_is_not_sent() {
        let err = managed_op_with_unanswered_route_open(false).await;
        assert!(
            matches!(err, CallError::NotSent(_)),
            "call body never left the client, got {err:?}"
        );
    }

    #[tokio::test]
    async fn subscribe_whose_route_open_times_out_after_send_is_not_sent() {
        let err = managed_op_with_unanswered_route_open(true).await;
        assert!(
            matches!(err, CallError::NotSent(_)),
            "subscription request never left the client, got {err:?}"
        );
    }

    #[tokio::test]
    async fn route_poll_deadline_bounds_writer_capacity() {
        let shared = Arc::new(Shared::new(
            PathBuf::from("/tmp/does-not-exist"),
            ConsumerOptions::default(),
        ));
        let (writer, mut rx) = mpsc::channel(1);
        let handle = RouteHandle::new(8, 4, 1);
        writer
            .try_send(WriteCommand {
                frame: response_frame(0, 0, 99, Vec::new()),
                pending: None,
            })
            .expect("the fake daemon queue should accept its filler frame");
        {
            let mut inner = shared.lock_inner();
            inner.writer = Some(writer);
            inner.route_epochs.insert(handle.channel, handle);
        }

        let consumer = SubcConsumer { shared };
        let result = tokio::time::timeout(
            Duration::from_millis(250),
            consumer.poll_route(&handle, PollKind::Liveness, Duration::from_millis(25)),
        )
        .await
        .expect("a control request must finish at its own deadline")
        .unwrap_err();
        assert!(matches!(result, CallError::NotSent(_)));
        assert_eq!(
            rx.recv()
                .await
                .expect("the filler must still be the only queued frame")
                .frame
                .header
                .corr,
            99
        );
        assert!(
            rx.try_recv().is_err(),
            "the timed-out control request must not reach the fake daemon"
        );
    }

    #[tokio::test]
    async fn subscription_deadline_bounds_writer_capacity() {
        let shared = Arc::new(Shared::new(
            PathBuf::from("/tmp/does-not-exist"),
            ConsumerOptions::default(),
        ));
        let (writer, mut rx) = mpsc::channel(1);
        let handle = RouteHandle::new(9, 2, 1);
        let route_sem = Arc::new(Semaphore::new(1));
        writer
            .try_send(WriteCommand {
                frame: response_frame(0, 0, 100, Vec::new()),
                pending: None,
            })
            .expect("the fake daemon queue should accept its filler frame");
        {
            let mut inner = shared.lock_inner();
            inner.writer = Some(writer);
            inner.cache_route(
                RouteKey::new(
                    &RouteTarget::ToolProvider {
                        module_id: "subscriptions".to_string(),
                    },
                    &BindIdentity::new(
                        PathBuf::from("/tmp/project"),
                        "test".to_string(),
                        "subscription".to_string(),
                    ),
                    None,
                    None,
                ),
                RouteState {
                    handle,
                    sem: Arc::clone(&route_sem),
                },
            );
        }

        let consumer = SubcConsumer { shared };
        let result = tokio::time::timeout(
            Duration::from_millis(250),
            consumer.subscribe_route(
                &handle,
                b"subscribe".to_vec(),
                SubscribeOptions {
                    route_open_timeout: Duration::from_millis(25),
                    ..SubscribeOptions::default()
                },
            ),
        )
        .await
        .expect("a subscription must finish at its route-open deadline");
        let result = match result {
            Ok(_) => panic!("a subscription blocked before writing must time out"),
            Err(err) => err,
        };
        assert!(matches!(result, CallError::NotSent(_)));
        assert_eq!(
            rx.recv()
                .await
                .expect("the filler must still be the only queued frame")
                .frame
                .header
                .corr,
            100
        );
        assert!(
            rx.try_recv().is_err(),
            "the timed-out subscription must not reach the fake daemon"
        );
        assert!(
            route_sem.try_acquire().is_ok(),
            "a pre-write subscription timeout must release its route credit"
        );
    }

    #[test]
    fn catalog_list_deserializes_golden_reply_and_ignores_unknown_fields() {
        let mut reply: serde_json::Value = serde_json::from_str(include_str!(
            "../../subc-control/tests/golden/client_control_response_catalog_list.json"
        ))
        .expect("the catalog.list golden reply must be valid JSON");
        reply["future_top_level"] = serde_json::json!(true);
        reply["modules"][0]["future_module_field"] = serde_json::json!("ignored");

        let catalog: CatalogList =
            serde_json::from_value(reply).expect("catalog.list should tolerate additive fields");
        assert_eq!(catalog.generation, 7);
        assert_eq!(catalog.modules.len(), 1);
        assert!(catalog.subc_ops.iter().any(|op| op == "catalog.list"));

        let tools = catalog.modules[0]
            .roles
            .iter()
            .find_map(|role| match role {
                subc_protocol::manifest::ProviderRole::ToolProvider { tools, .. } => Some(tools),
                _ => None,
            })
            .expect("the golden module must advertise a tool_provider role");
        let tool = tools
            .first()
            .expect("the golden tool_provider role must advertise a tool");
        assert!(!tool.name.is_empty());
        assert_eq!(
            tool.schema.get("type").and_then(serde_json::Value::as_str),
            Some("object")
        );
        assert!(matches!(
            tool.execution_mode,
            subc_protocol::manifest::ExecutionMode::Pure
        ));
    }

    #[tokio::test]
    async fn catalog_list_sends_an_unfiltered_channel_zero_request() {
        let shared = Arc::new(Shared::new(
            PathBuf::from("/tmp/does-not-exist"),
            ConsumerOptions::default(),
        ));
        let (writer, mut rx) = mpsc::channel(1);
        shared.lock_inner().writer = Some(writer);

        let consumer = SubcConsumer {
            shared: Arc::clone(&shared),
        };
        let request = tokio::spawn(async move { consumer.catalog_list().await });
        let command = rx
            .recv()
            .await
            .expect("catalog.list must queue a channel-0 request");
        assert_eq!(command.frame.header.channel, 0);
        let body: serde_json::Value = serde_json::from_slice(&command.frame.body).unwrap();
        assert_eq!(body["op"], "catalog.list");
        assert!(
            body.get("module_id").is_none(),
            "catalog.list must request the complete catalog without a module filter"
        );

        let response = serde_json::to_vec(&ClientControlResponse::CatalogList {
            generation: 9,
            modules: Vec::new(),
            subc_ops: vec!["catalog.list".to_string()],
        })
        .unwrap();
        assert!(
            dispatch_frame(
                &shared,
                1,
                response_frame(0, 0, command.frame.header.corr, response),
            )
            .await
        );
        let catalog = request.await.unwrap().unwrap();
        assert_eq!(catalog.generation, 9);
        assert!(catalog.modules.is_empty());
    }

    #[tokio::test]
    async fn spawn_snapshot_sends_a_channel_zero_request_and_returns_the_snapshot() {
        let shared = Arc::new(Shared::new(
            PathBuf::from("/tmp/does-not-exist"),
            ConsumerOptions::default(),
        ));
        let (writer, mut rx) = mpsc::channel(1);
        shared.lock_inner().writer = Some(writer);

        let consumer = SubcConsumer {
            shared: Arc::clone(&shared),
        };
        let request = tokio::spawn(async move { consumer.spawn_snapshot().await });
        let command = rx
            .recv()
            .await
            .expect("supervisor.spawn_snapshot must queue a channel-0 request");
        assert_eq!(command.frame.header.channel, 0);
        let body: serde_json::Value = serde_json::from_slice(&command.frame.body).unwrap();
        assert_eq!(body["op"], "supervisor.spawn_snapshot");

        let snapshot = SpawnSnapshot {
            cursor: SpawnCursor {
                daemon_incarnation: "incarnation-a".to_string(),
                seq: 41,
            },
            ring_bound: 256,
            live: vec![LiveSpawn {
                module_id: "participant".to_string(),
                spawn_generation: 3,
                pid: 4242,
                spawned_at_ms: 1_700_000_000_000,
            }],
        };
        let response = serde_json::to_vec(&ClientControlResponse::SupervisorSpawnSnapshot {
            snapshot: snapshot.clone(),
        })
        .unwrap();
        assert!(
            dispatch_frame(
                &shared,
                1,
                response_frame(0, 0, command.frame.header.corr, response),
            )
            .await
        );
        assert_eq!(request.await.unwrap().unwrap(), snapshot);
    }

    #[tokio::test]
    async fn spawn_snapshot_refusal_keeps_the_daemon_code() {
        let shared = Arc::new(Shared::new(
            PathBuf::from("/tmp/does-not-exist"),
            ConsumerOptions::default(),
        ));
        let (writer, mut rx) = mpsc::channel(1);
        shared.lock_inner().writer = Some(writer);

        let consumer = SubcConsumer {
            shared: Arc::clone(&shared),
        };
        let request = tokio::spawn(async move { consumer.spawn_snapshot().await });
        let command = rx
            .recv()
            .await
            .expect("supervisor.spawn_snapshot must queue a channel-0 request");
        let refusal = serde_json::to_vec(&ErrorBody {
            code: "op_not_permitted".to_string(),
            message: "refused by the test".to_string(),
            detail: None,
        })
        .unwrap();
        let frame = Frame::build(
            FrameType::Error,
            Flags::new(false, Priority::Interactive, false),
            0,
            0,
            command.frame.header.corr,
            refusal,
        )
        .unwrap();
        assert!(dispatch_frame(&shared, 1, frame).await);
        let error = request.await.unwrap().unwrap_err();
        assert_eq!(error.code(), Some("op_not_permitted"));
    }

    #[tokio::test]
    async fn reconnect_completion_between_wait_decision_and_registration_wakes_caller() {
        let shared = Arc::new(Shared::new(
            PathBuf::from("/tmp/subc-client-rs-reconnect-wakeup"),
            ConsumerOptions::default(),
        ));
        let (sender, _receiver) = mpsc::channel(EGRESS_BUFFER);
        {
            let mut inner = shared.lock_inner();
            inner.reconnect = ReconnectState::Inline {
                generation: inner.generation,
            };
        }
        let completing = Arc::clone(&shared);
        *shared.before_wait.lock().unwrap() = Some(Box::new(move || {
            completing.lock_inner().writer = Some(sender);
            completing.notify.notify_waiters();
        }));

        let result = shared
            .ensure_connected_for_call(Instant::now() + Duration::from_millis(30))
            .await;
        assert!(
            result.is_ok(),
            "connected caller should not time out: {result:?}"
        );
        assert!(shared.before_wait.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn catalog_list_deadline_is_not_sent_when_reconnection_stays_down() {
        let shared = Arc::new(Shared::new(
            PathBuf::from("/tmp/subc-client-rs-catalog-list-unavailable"),
            ConsumerOptions {
                call_timeout: Duration::from_millis(25),
                reconnect_backoff: RetryBackoff {
                    base: Duration::from_millis(1),
                    cap: Duration::from_millis(1),
                    max_attempts: 100,
                },
                ..ConsumerOptions::default()
            },
        ));
        let consumer = SubcConsumer { shared };
        let result = tokio::time::timeout(Duration::from_millis(250), consumer.catalog_list())
            .await
            .expect("catalog.list must finish at its configured deadline")
            .unwrap_err();
        assert!(matches!(result, CallError::NotSent(_)));
    }
}

#[cfg(test)]
mod launch_nonce_redaction_tests {
    use super::*;

    const NONCE: &str = "nonce-f00dfeed1234abcd";

    #[test]
    fn option_types_and_route_keys_never_print_the_nonce() {
        let identity = ConsumerIdentity {
            module_id: "wernicke".to_string(),
            launch_nonce: NONCE.to_string(),
        };
        let call = CallOptions {
            consumer_identity: Some(identity.clone()),
            ..CallOptions::default()
        };
        let key = ConsumerIdentityKey::from(&identity);
        for printed in [format!("{call:?}"), format!("{key:?}")] {
            assert!(!printed.contains(NONCE), "launch nonce printed: {printed}");
        }
    }
}
