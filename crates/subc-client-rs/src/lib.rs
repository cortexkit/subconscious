#![forbid(unsafe_code)]

pub mod consumer;
pub mod policy_cache;
#[cfg(any(test, feature = "test-support"))]
mod stand_in;
#[cfg(feature = "test-support")]
pub mod test_support;
pub use consumer::{
    is_retryable_route_open_code, CallError, CallOptions, CatalogList, CloseRouteOptions,
    ConnectionState, ConsumerError, ConsumerOptions, ControlPush, OutcomeUnknownCause, PushEvent,
    RetryBackoff, ReverseRequestContext, ReverseRequestError, ReverseRequestRegistrationError,
    ReverseRequestRegistry, RouteCloseDisposition, RouteCloseReason, RouteEndReason,
    RoutePollResult, ScopeSelector, SpawnStreamError, SpawnSubscription, SubcConsumer,
    SubscribeOptions, Subscription, SubscriptionClosed, DEFAULT_CALL_TIMEOUT,
    DEFAULT_LIVENESS_PROBE_WINDOW, DEFAULT_ROUTE_RETRY_DEADLINE, SPAWN_CURSOR_INCARNATION_MISMATCH,
    SPAWN_CURSOR_TOO_OLD, SPAWN_SUBSCRIBER_LAGGED,
};
pub use policy_cache::{
    PolicyResolveError, PolicyResolver, PolicyResolverConfig, PolicyResolverFootprint,
    PolicyVerdict, ProjectRef, Subject, DEFAULT_POLICY_RESOLVER_MODULE_ID,
};

use std::{
    collections::HashMap,
    env,
    error::Error,
    ffi::OsString,
    fmt,
    future::Future,
    io,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub use async_trait::async_trait;
pub use subc_control::{CatalogEntry, ConsumerIdentity};
/// The one launch-nonce reader for a module process: `launch_nonce()` reads
/// the inherited descriptor (or, while the daemon still sets it, the
/// `SUBC_LAUNCH_NONCE` environment copy) once and caches it. The SDK's HELLO and route opens go through it too; a module's
/// own readers must as well, and it should call it before spawning anything.
pub use subc_os::launch_nonce;
use subc_protocol::{
    error_codes,
    manifest::ModuleManifest,
    scope::{
        ScopeEnded, ScopeRecord, ScopeRecordResult, ScopeStamp, ScopeStatus, SCOPE_DESCRIBE_OP,
        SCOPE_SYNC_OP,
    },
    session::{
        ModuleControlCommand, ModuleControlRequest, ModuleControlRequestFromModule,
        ModuleControlResponse, ModuleControlResponseToModule, OperatorConfirmReply,
        OperatorConfirmRequest, MODULE_CONTROL_OP_HEALTH_CHECK, MODULE_TO_SUBC_OP_CATALOG_UPDATE,
    },
    BindIdentity, ErrorBody, Flags, Frame, FrameBuildError, FrameType, ModuleHelloAckBody,
    ModuleHelloBody, Principal, Priority, RouteTarget, PROTOCOL_VERSION, SUBC_MODULE_ID_ENV,
};
pub use subc_protocol::{
    manifest::{
        build_provenance, CapabilityDeclarations, CapabilityNeed, CapabilityRequirement,
        ExecutionMode, LaunchNonceSource, ManifestProvenance, ProvenanceFormError, ProviderRole,
        Tool, PROVENANCE_SENTINELS,
    },
    session::{HealthReport, HealthStatus},
    AdmissionClass, MachineId, SUBC_PROTOCOL_CRATE_VERSION,
};
pub use subc_transport::connection_file::{
    discover, discovery_candidates, Discovered, DiscoveryError,
};

use subc_transport::{
    authenticate_client, connection_file, read_frame, write_frame, AuthError, ConnectionFileError,
    FrameIoError,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufWriter},
    net::TcpStream,
    sync::{mpsc, oneshot, Semaphore},
    time::{timeout, timeout_at, Instant},
};
use tokio_util::sync::{CancellationToken, WaitForCancellationFuture};

const AUTH_DEADLINE: Duration = Duration::from_secs(2);
const CATALOG_UPDATE_TIMEOUT: Duration = Duration::from_secs(10);
const OPERATOR_CONFIRM_TIMEOUT: Duration = Duration::from_secs(290);
const OPERATOR_CONFIRM_OP: &str = "operator.confirm";
const EGRESS_BUFFER: usize = 64;
const HANDLER_TASK_CAPACITY: usize = 64;
/// A full dispatcher is normal during a burst. Report dispatch impairment only
/// once a data request has waited more than two seconds for a handler slot.
const DISPATCH_SATURATION_WAIT: Duration = Duration::from_secs(2);
const HELLO_CORR: u64 = 1;
/// How long a closing module waits for its writer to flush the frames still
/// queued before aborting it; see the serve future in `serve_with_handle`.
const WRITER_DRAIN_LIMIT: Duration = Duration::from_secs(2);
/// When a module's daemon connection ends (GOODBYE, EOF, or the module closing
/// it), how long the serve loop waits for each `on_draining` hook it already
/// spawned to start running before it cancels requests and calls
/// `on_connection_end`. The wait is only for the hook's first step, which is
/// normally immediate; the bound matters because the daemon kills a module
/// that has not exited within its stop timeout after GOODBYE, and a hook that
/// blocks its thread must not hold the shutdown up that long.
const DRAINING_HOOK_START_LIMIT: Duration = Duration::from_secs(2);
static NEXT_MODULE_CONNECTION_TOKEN: AtomicU64 = AtomicU64::new(1);
/// The connection token of a [`RouteHandle::detached`] handle. Module
/// connection tokens start at 1 (above) and a consumer's generation starts at
/// 1 and only grows, so no live connection ever has this token.
const DETACHED_CONNECTION_TOKEN: u64 = 0;

type RequestKey = (u16, u32, u64);
type InFlight = Arc<Mutex<HashMap<RequestKey, CancellationToken>>>;

/// Immutable identity of one route binding on one live connection.
///
/// Only `channel` and `epoch` are serialized. The private connection token prevents
/// work retained from an earlier connection from acting on a later connection that
/// happens to reuse the same wire pair.
#[derive(Clone, Copy)]
pub struct RouteHandle {
    pub channel: u16,
    pub epoch: u32,
    connection_token: u64,
    reverse_request_registry_id: u64,
}

impl RouteHandle {
    /// A handle for `channel` and `epoch` that belongs to no connection, for
    /// building values in tests, such as a [`RouteBindRequest`] passed to a
    /// module's own `on_bind`. It is never bound to a connection and cannot
    /// become one.
    ///
    /// Every operation that would reach a connection fails with the error
    /// it returns for a closed or stale route, and sends nothing:
    /// [`ModuleHandle::push`] returns [`SubcModuleError::StaleRouteHandle`];
    /// [`SubcConsumer::request`], [`SubcConsumer::subscribe_route`],
    /// [`SubcConsumer::poll_route`], [`SubcConsumer::push_events`] and
    /// [`SubcConsumer::close_handle`] return [`CallError::StaleRouteHandle`];
    /// [`Self::on_request`] and [`Self::on_request_fallible`] return
    /// [`ReverseRequestRegistrationError::NotConsumerRoute`].
    pub fn detached(channel: u16, epoch: u32) -> Self {
        Self::new(channel, epoch, DETACHED_CONNECTION_TOKEN)
    }

    pub(crate) fn new(channel: u16, epoch: u32, connection_token: u64) -> Self {
        Self {
            channel,
            epoch,
            connection_token,
            reverse_request_registry_id: 0,
        }
    }

    pub(crate) fn new_consumer(
        channel: u16,
        epoch: u32,
        connection_token: u64,
        reverse_requests: ReverseRequestRegistry,
    ) -> Self {
        reverse_requests.seal();
        Self {
            channel,
            epoch,
            connection_token,
            reverse_request_registry_id: consumer::install_reverse_request_registry(
                reverse_requests,
            ),
        }
    }

    pub(crate) fn connection_token(self) -> u64 {
        self.connection_token
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
        consumer::route_reverse_request_registry(self.reverse_request_registry_id)?
            .on_request(method_family, handler)
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
        consumer::route_reverse_request_registry(self.reverse_request_registry_id)?
            .on_request_fallible(method_family, handler)
    }

    pub(crate) fn reverse_request_registry_id(self) -> u64 {
        self.reverse_request_registry_id
    }
}

impl PartialEq for RouteHandle {
    fn eq(&self, other: &Self) -> bool {
        self.channel == other.channel
            && self.epoch == other.epoch
            && self.connection_token == other.connection_token
    }
}

impl Eq for RouteHandle {}

impl std::hash::Hash for RouteHandle {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::hash::Hash::hash(&self.channel, state);
        std::hash::Hash::hash(&self.epoch, state);
        std::hash::Hash::hash(&self.connection_token, state);
    }
}

impl fmt::Debug for RouteHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RouteHandle")
            .field("channel", &self.channel)
            .field("epoch", &self.epoch)
            .finish_non_exhaustive()
    }
}
/// A single read-lock snapshot of bound and pending project routes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveRootsSnapshot {
    pub roots: Vec<subc_protocol::session::LiveRoot>,
    pub unknown_root_bindings: u64,
    pub total_bindings: u64,
}

/// The daemon's answer to an accepted [`ModuleHandle::scope_sync`].
///
/// A refusal of the whole sync is never a reply: it is
/// [`ScopeCallError::Refused`], and the daemon changed nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ScopeSyncReply {
    /// The generation the daemon accepted, echoed from the request.
    pub generation: u64,
    /// One result per record sent, in request order. A record refused on its
    /// own merits is here with outcome `refused` and its code; the rest of the
    /// sync still applied.
    pub results: Vec<ScopeRecordResult>,
    /// Scopes of this owner that the sync ended, by leaving them out or by
    /// sending a higher epoch for the same ref.
    pub ended: Vec<ScopeEnded>,
}

/// The daemon's answer to [`ModuleHandle::scope_describe`] about one
/// `(owner, ref)`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ScopeDescribeReply {
    pub status: ScopeStatus,
    /// The live epoch, or for `ended` the most recent epoch that ended.
    pub scope_epoch: Option<u64>,
    /// Identifies the daemon process that answered. A different value means
    /// the daemon restarted, and owners re-sync after a restart.
    pub daemon_incarnation: String,
    /// Whether the owner has synced since this daemon incarnation started.
    pub owner_synced: bool,
    /// Whether the owner is a module in the daemon's supervised roster.
    pub owner_configured: bool,
    /// The stamped fields, present only when `status` is `live`.
    pub scope: Option<ScopeStamp>,
}

type CatalogUpdateReply =
    oneshot::Sender<Result<ModuleControlResponseToModule, CatalogUpdateError>>;
type CatalogUpdateWaiter =
    oneshot::Receiver<Result<ModuleControlResponseToModule, CatalogUpdateError>>;
type CatalogUpdateRequest = (u64, mpsc::Sender<Frame>, CatalogUpdateWaiter);
type OperatorConfirmRequestWaiter = (u64, mpsc::Sender<Frame>, oneshot::Receiver<Frame>);

/// Future returned by [`serve_with_handle`] that runs the module until GOODBYE or EOF.
pub type ModuleServeFuture = Pin<Box<dyn Future<Output = Result<(), SubcModuleError>> + Send>>;

#[derive(Clone)]
struct RequestDispatcher {
    in_flight: InFlight,
    permits: Arc<Semaphore>,
    /// Stamped by data dispatch before spawning a task, cleared on acquisition
    /// or task exit. This lock is never held over an await or handler work, so
    /// health can read it without queueing behind the work it is measuring.
    waiting: Arc<Mutex<HashMap<RequestKey, Instant>>>,
    /// One receiver per `on_draining` hook spawned on this connection, resolved
    /// once that hook has started running. The connection waits on them before
    /// acting on its end, so a hook is always called before the GOODBYE that
    /// follows its drain is handled.
    draining_hooks: Arc<Mutex<Vec<oneshot::Receiver<()>>>>,
    /// Set after the first channel-0 push this SDK could not decode has been
    /// logged, so a daemon that repeats a newer push cannot flood the log.
    undecodable_push_logged: Arc<AtomicBool>,
}

impl RequestDispatcher {
    fn new() -> Self {
        Self {
            in_flight: Arc::new(Mutex::new(HashMap::new())),
            permits: Arc::new(Semaphore::new(HANDLER_TASK_CAPACITY)),
            waiting: Arc::new(Mutex::new(HashMap::new())),
            draining_hooks: Arc::new(Mutex::new(Vec::new())),
            undecodable_push_logged: Arc::new(AtomicBool::new(false)),
        }
    }

    fn start_waiting(&self, key: RequestKey) -> PermitWait {
        self.waiting
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(key, Instant::now());
        PermitWait {
            waiting: Arc::clone(&self.waiting),
            key,
        }
    }

    fn fold_health(&self, mut report: HealthReport) -> HealthReport {
        let waiting = self
            .waiting
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let available = self.permits.available_permits();
        let oldest_wait = waiting.values().min().map(Instant::elapsed);
        drop(waiting);
        if let Some(age) =
            oldest_wait.filter(|age| available == 0 && *age > DISPATCH_SATURATION_WAIT)
        {
            if report.status == HealthStatus::Ok {
                report.status = HealthStatus::Degraded;
            }
            let saturation = format!(
                "request dispatch saturated: {}/{} in use, oldest waiting {:.1} s",
                HANDLER_TASK_CAPACITY - available,
                HANDLER_TASK_CAPACITY,
                age.as_secs_f64(),
            );
            report.detail = Some(match report.detail {
                Some(detail) => format!("{saturation}; {detail}"),
                None => saturation,
            });
        }
        report
    }

    /// Wait, within [`DRAINING_HOOK_START_LIMIT`], until every `on_draining`
    /// hook spawned on this connection has started. Only the start is awaited:
    /// a hook may still be finishing its work after the connection has ended.
    async fn wait_for_draining_hooks_to_start(&self) {
        let started = std::mem::take(
            &mut *self
                .draining_hooks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
        let deadline = tokio::time::Instant::now() + DRAINING_HOOK_START_LIMIT;
        for hook in started {
            // A dropped sender means the hook task ended (or panicked) before
            // reporting; either way there is nothing left to wait for.
            if tokio::time::timeout_at(deadline, hook).await.is_err() {
                return;
            }
        }
    }

    /// Cancel every request still in flight and refuse ones still waiting for a
    /// handler slot, as a route's GOODBYE does for that route's requests. Run
    /// when the connection ends: each request task holds a sender to the egress
    /// channel, so until they finish the writer never sees the channel close.
    fn cancel_all(&self) {
        self.permits.close();
        let cancelled = match self.in_flight.lock() {
            Ok(mut guard) => guard.drain().map(|(_, token)| token).collect::<Vec<_>>(),
            Err(poisoned) => poisoned
                .into_inner()
                .drain()
                .map(|(_, token)| token)
                .collect(),
        };
        for cancellation in cancelled {
            cancellation.cancel();
        }
    }
}

/// Holds this request's entry in the dispatcher's `waiting` map (when it started
/// waiting for a handler slot, which health reports as the oldest wait) and removes
/// it on drop, so the entry goes even if the queued task exits on connection close
/// or panics.
struct PermitWait {
    waiting: Arc<Mutex<HashMap<RequestKey, Instant>>>,
    key: RequestKey,
}

impl Drop for PermitWait {
    fn drop(&mut self) {
        self.waiting
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&self.key);
    }
}

/// Cloneable handle for module-originated control RPCs on channel 0.
#[derive(Clone)]
pub struct ModuleHandle {
    shared: Arc<ModuleHandleShared>,
}

struct ModuleHandleShared {
    negotiated_ver: u8,
    supports_catalog_update: bool,
    supports_live_roots: bool,
    supports_scope_sync: bool,
    supports_scope_describe: bool,
    supports_operator_confirm: bool,
    connection_runtime: tokio::runtime::Handle,
    connection_token: u64,
    machine_id: Option<MachineId>,
    live_routes: Mutex<HashMap<u16, RouteHandle>>,
    dropped_route_frames: AtomicU64,
    close_token: CancellationToken,
    inner: Mutex<ModuleHandleState>,
}

struct ModuleHandleState {
    writer: Option<mpsc::Sender<Frame>>,
    next_corr: Option<u64>,
    pending_catalog_updates: HashMap<u64, CatalogUpdateReply>,
    pending_operator_confirms: HashMap<u64, oneshot::Sender<Frame>>,
    closed: bool,
}

impl ModuleHandle {
    fn new(
        ack: &ModuleHelloAckBody,
        writer: mpsc::Sender<Frame>,
        connection_token: u64,
        close_token: CancellationToken,
        connection_runtime: tokio::runtime::Handle,
    ) -> Self {
        Self {
            shared: Arc::new(ModuleHandleShared {
                negotiated_ver: ack.negotiated_ver,
                supports_catalog_update: ack
                    .subc_ops
                    .iter()
                    .any(|op| op == MODULE_TO_SUBC_OP_CATALOG_UPDATE),
                supports_live_roots: ack.subc_ops.iter().any(|op| op == "supervisor.live_roots"),
                supports_scope_sync: ack.subc_ops.iter().any(|op| op == SCOPE_SYNC_OP),
                supports_scope_describe: ack.subc_ops.iter().any(|op| op == SCOPE_DESCRIBE_OP),
                supports_operator_confirm: ack.subc_ops.iter().any(|op| op == OPERATOR_CONFIRM_OP),
                connection_runtime,
                connection_token,
                // A value that does not parse is treated like an absent one:
                // the module learns nothing rather than a name that is wrong.
                machine_id: ack
                    .machine_id
                    .as_deref()
                    .and_then(|id| MachineId::parse(id).ok()),
                live_routes: Mutex::new(HashMap::new()),
                dropped_route_frames: AtomicU64::new(0),
                close_token,
                inner: Mutex::new(ModuleHandleState {
                    writer: Some(writer),
                    next_corr: Some(HELLO_CORR + 1),
                    pending_catalog_updates: HashMap::new(),
                    pending_operator_confirms: HashMap::new(),
                    closed: false,
                }),
            }),
        }
    }

    /// The daemon's machine id, as delivered on HELLO_ACK.
    ///
    /// `None` means the daemon did not send one (it predates the machine id, or
    /// was embedded without one) or sent a malformed value. Read that as "the
    /// daemon has not named this machine", never as "no machine", and never mint
    /// a substitute. The id is a name, never an authority: see [`MachineId`].
    pub fn machine_id(&self) -> Option<&MachineId> {
        self.shared.machine_id.as_ref()
    }

    /// Resolves once this module's connection to the daemon has closed: after a
    /// channel-0 GOODBYE, EOF, or a connection error. A module ends its own
    /// background work on it, so nothing outlives the connection.
    pub fn closed(&self) -> impl Future<Output = ()> + Send + 'static {
        self.shared.close_token.clone().cancelled_owned()
    }

    /// Whether this module's connection to the daemon has closed.
    pub fn is_closed(&self) -> bool {
        self.shared.close_token.is_cancelled()
    }

    /// Ask the daemon to replace this module's advertised provider roles in place.
    ///
    /// The returned result resolves when the daemon ACKs the update, rejects it with
    /// a typed channel-0 Error frame, the request times out, or the connection dies.
    pub async fn catalog_update(
        &self,
        provides: Vec<ProviderRole>,
    ) -> Result<(), CatalogUpdateError> {
        self.catalog_update_inner(provides, None).await
    }

    /// Replace provider roles and attest a new static capability declaration.
    ///
    /// The declaration must be the same static metadata emitted by the module's
    /// current manifest; this update exists so the daemon can reconcile live routes
    /// when that attested metadata changes.
    pub async fn catalog_update_with_capabilities(
        &self,
        provides: Vec<ProviderRole>,
        capabilities: CapabilityDeclarations,
    ) -> Result<(), CatalogUpdateError> {
        self.catalog_update_inner(provides, Some(capabilities))
            .await
    }

    async fn catalog_update_inner(
        &self,
        provides: Vec<ProviderRole>,
        capabilities: Option<CapabilityDeclarations>,
    ) -> Result<(), CatalogUpdateError> {
        if !self.shared.supports_catalog_update {
            return Err(CatalogUpdateError::NotSupported);
        }

        let body = serde_json::to_vec(&ModuleControlRequestFromModule::CatalogUpdate {
            provides,
            capabilities,
            ready: None,
        })
        .map_err(|err| {
            CatalogUpdateError::Protocol(format!(
                "failed to encode catalog.update request body: {err}"
            ))
        })?;
        let (corr, writer, rx) = self.shared.begin_catalog_update()?;
        let frame = Frame::build_with_version(
            self.shared.negotiated_ver,
            FrameType::Request,
            control_flags(),
            0,
            0,
            corr,
            body,
        )
        .map_err(|err| {
            self.shared.remove_pending_catalog_update(corr);
            CatalogUpdateError::Protocol(format!(
                "failed to build catalog.update request frame: {err}"
            ))
        })?;

        if writer.send(frame).await.is_err() {
            self.shared.remove_pending_catalog_update(corr);
            return Err(CatalogUpdateError::ConnectionClosed);
        }

        match timeout(CATALOG_UPDATE_TIMEOUT, rx).await {
            Ok(Ok(Ok(ModuleControlResponseToModule::CatalogUpdate {}))) => Ok(()),
            Ok(Ok(Ok(_))) => Err(CatalogUpdateError::Protocol(
                "unexpected catalog.update response".into(),
            )),
            Ok(Ok(Err(err))) => Err(err),
            Ok(Err(_)) => Err(CatalogUpdateError::ConnectionClosed),
            Err(_) => {
                self.shared.remove_pending_catalog_update(corr);
                Err(CatalogUpdateError::Timeout)
            }
        }
    }

    /// Snapshot the roots served by this module's routable endpoint.
    pub async fn live_roots(&self) -> Result<LiveRootsSnapshot, CatalogUpdateError> {
        if !self.shared.supports_live_roots {
            return Err(CatalogUpdateError::NotSupported);
        }
        let body = serde_json::to_vec(&ModuleControlRequestFromModule::LiveRoots {})
            .map_err(|err| CatalogUpdateError::Protocol(err.to_string()))?;
        let (corr, writer, rx) = self.shared.begin_catalog_update()?;
        let frame = Frame::build_with_version(
            self.shared.negotiated_ver,
            FrameType::Request,
            control_flags(),
            0,
            0,
            corr,
            body,
        )
        .map_err(|err| {
            self.shared.remove_pending_catalog_update(corr);
            CatalogUpdateError::Protocol(err.to_string())
        })?;
        if writer.send(frame).await.is_err() {
            self.shared.remove_pending_catalog_update(corr);
            return Err(CatalogUpdateError::ConnectionClosed);
        }
        match timeout(CATALOG_UPDATE_TIMEOUT, rx).await {
            Ok(Ok(Ok(ModuleControlResponseToModule::LiveRoots {
                roots,
                unknown_root_bindings,
                total_bindings,
            }))) => Ok(LiveRootsSnapshot {
                roots,
                unknown_root_bindings,
                total_bindings,
            }),
            Ok(Ok(Ok(_))) => Err(CatalogUpdateError::Protocol(
                "unexpected live_roots response".into(),
            )),
            Ok(Ok(Err(err))) => Err(err),
            Ok(Err(_)) => Err(CatalogUpdateError::ConnectionClosed),
            Err(_) => {
                self.shared.remove_pending_catalog_update(corr);
                Err(CatalogUpdateError::Timeout)
            }
        }
    }

    /// Register this module's full scope set with the daemon (`scope.sync`).
    ///
    /// The owner is this module; nothing in the request names it. `scopes` is
    /// the whole set: a live scope left out ends. `generation` must be larger
    /// than the last one the daemon accepted from this owner's sync authority,
    /// or the sync is refused with `scope_sync_stale`.
    ///
    /// A refusal of the whole sync comes back as [`ScopeCallError::Refused`]
    /// carrying the daemon's code (see `subc_protocol::error_codes`, e.g.
    /// `SCOPE_SYNC_STALE`, `SCOPE_SYNC_NOT_AUTHORITY`), and nothing changed.
    /// Only a module the daemon itself launched can hold sync authority.
    pub async fn scope_sync(
        &self,
        generation: u64,
        scopes: Vec<ScopeRecord>,
    ) -> Result<ScopeSyncReply, ScopeCallError> {
        if !self.shared.supports_scope_sync {
            return Err(ScopeCallError::NotSupported { op: SCOPE_SYNC_OP });
        }
        let request = ModuleControlRequestFromModule::ScopeSync { generation, scopes };
        match self.scope_call(SCOPE_SYNC_OP, &request).await? {
            ModuleControlResponseToModule::ScopeSync {
                generation,
                results,
                ended,
            } => Ok(ScopeSyncReply {
                generation,
                results,
                ended,
            }),
            other => Err(ScopeCallError::Protocol(format!(
                "unexpected {SCOPE_SYNC_OP} response: {other:?}"
            ))),
        }
    }

    /// Read one scope's current state (`scope.describe`). Any registered
    /// module may read any owner's scope.
    ///
    /// An unknown ref is not an error: the daemon answers it with status
    /// `not_live`. `owner_synced` says whether the owner has synced since this
    /// daemon started (if so, the scope is gone), and `owner_configured` whether
    /// the owner is a module the daemon supervises (if not, it never will sync).
    pub async fn scope_describe(
        &self,
        owner: Principal,
        scope_ref: String,
    ) -> Result<ScopeDescribeReply, ScopeCallError> {
        if !self.shared.supports_scope_describe {
            return Err(ScopeCallError::NotSupported {
                op: SCOPE_DESCRIBE_OP,
            });
        }
        let request = ModuleControlRequestFromModule::ScopeDescribe { owner, scope_ref };
        match self.scope_call(SCOPE_DESCRIBE_OP, &request).await? {
            ModuleControlResponseToModule::ScopeDescribe {
                status,
                scope_epoch,
                daemon_incarnation,
                owner_synced,
                owner_configured,
                scope,
            } => Ok(ScopeDescribeReply {
                status,
                scope_epoch,
                daemon_incarnation,
                owner_synced,
                owner_configured,
                scope,
            }),
            other => Err(ScopeCallError::Protocol(format!(
                "unexpected {SCOPE_DESCRIBE_OP} response: {other:?}"
            ))),
        }
    }

    /// Ask the operator to authorise one in-flight, unchanged write.
    ///
    /// Every error means the module must refuse the write. `Ok(())` authorises
    /// only the write this call was made for; it must never be cached or reused.
    /// The daemon checks the summary and whether this module may ask for the
    /// caller's route. No failure is retried.
    ///
    /// A consumer call that may trigger confirmation must set
    /// [`CallOptions::timeout`] to at least 300 seconds. This helper waits up to
    /// 290 seconds, including egress backpressure, and dropping it withdraws an
    /// accepted request.
    pub async fn confirm_operator(
        &self,
        summary: &str,
        caller_route: &RouteHandle,
    ) -> Result<(), OperatorConfirmError> {
        self.confirm_operator_with_deadline(summary, caller_route, OPERATOR_CONFIRM_TIMEOUT)
            .await
    }

    pub(crate) async fn confirm_operator_with_deadline(
        &self,
        summary: &str,
        caller_route: &RouteHandle,
        duration: Duration,
    ) -> Result<(), OperatorConfirmError> {
        if caller_route.connection_token() != self.shared.connection_token {
            return Err(OperatorConfirmError::NotPermitted);
        }
        if !self.shared.supports_operator_confirm {
            return Err(OperatorConfirmError::Unsupported);
        }
        // Use the same absolute deadline for both capacity and the reply: a
        // slow writer must not extend the time a confirmation can remain live.
        let deadline = Instant::now() + duration;
        let body = serde_json::to_vec(&OperatorConfirmRequest::new(
            summary,
            caller_route.channel,
            caller_route.epoch,
        ))
        .map_err(|_| OperatorConfirmError::PresenceUnavailable)?;
        let (corr, writer, rx) = self.shared.begin_operator_confirm()?;
        let mut guard = OperatorConfirmGuard {
            shared: Arc::clone(&self.shared),
            runtime: self.shared.connection_runtime.clone(),
            writer: writer.clone(),
            corr,
            accepted: false,
            armed: true,
        };
        let frame = Frame::build_with_version(
            self.shared.negotiated_ver,
            FrameType::Request,
            control_flags(),
            0,
            0,
            corr,
            body,
        )
        .map_err(|_| OperatorConfirmError::PresenceUnavailable)?;
        tokio::select! {
            result = timeout_at(deadline, writer.send(frame)) => {
                result.map_err(|_| OperatorConfirmError::PresenceUnavailable)?
                    .map_err(|_| OperatorConfirmError::PresenceUnavailable)?;
            }
            _ = self.shared.close_token.cancelled() => {
                return Err(OperatorConfirmError::PresenceUnavailable);
            }
        }
        guard.accepted = true;
        let reply = timeout_at(deadline, rx)
            .await
            .map_err(|_| OperatorConfirmError::PresenceUnavailable)?
            .map_err(|_| OperatorConfirmError::PresenceUnavailable)?;
        guard.armed = false;
        match reply.header.ty {
            FrameType::Response => {
                match serde_json::from_slice::<OperatorConfirmReply>(&reply.body) {
                    Ok(body) if body.outcome == "confirmed" => Ok(()),
                    _ => Err(OperatorConfirmError::PresenceUnavailable),
                }
            }
            FrameType::Error => match serde_json::from_slice::<ErrorBody>(&reply.body) {
                Ok(body) => Err(match body.code.as_str() {
                    error_codes::OPERATOR_DECLINED => OperatorConfirmError::Declined,
                    error_codes::OPERATOR_SUMMARY_INVALID => OperatorConfirmError::SummaryInvalid,
                    error_codes::OPERATOR_REQUEST_NOT_PERMITTED | "not_registered" => {
                        OperatorConfirmError::NotPermitted
                    }
                    "unsupported_control_frame" => OperatorConfirmError::Unsupported,
                    _ => OperatorConfirmError::PresenceUnavailable,
                }),
                Err(_) => Err(OperatorConfirmError::PresenceUnavailable),
            },
            _ => Err(OperatorConfirmError::PresenceUnavailable),
        }
    }

    /// Send one channel-0 control request and wait for its reply, through the
    /// same correlation table, writer and timeout as `catalog_update` and
    /// `live_roots`. The caller checks that the reply is the right variant.
    async fn scope_call(
        &self,
        op: &'static str,
        request: &ModuleControlRequestFromModule,
    ) -> Result<ModuleControlResponseToModule, ScopeCallError> {
        let body = serde_json::to_vec(request).map_err(|err| {
            ScopeCallError::Protocol(format!("failed to encode {op} request body: {err}"))
        })?;
        let (corr, writer, rx) = self
            .shared
            .begin_catalog_update()
            .map_err(ScopeCallError::from_control_error)?;
        let frame = Frame::build_with_version(
            self.shared.negotiated_ver,
            FrameType::Request,
            control_flags(),
            0,
            0,
            corr,
            body,
        )
        .map_err(|err| {
            self.shared.remove_pending_catalog_update(corr);
            ScopeCallError::Protocol(format!("failed to build {op} request frame: {err}"))
        })?;
        if writer.send(frame).await.is_err() {
            self.shared.remove_pending_catalog_update(corr);
            return Err(ScopeCallError::ConnectionClosed);
        }
        match timeout(CATALOG_UPDATE_TIMEOUT, rx).await {
            Ok(Ok(reply)) => reply.map_err(ScopeCallError::from_control_error),
            Ok(Err(_)) => Err(ScopeCallError::ConnectionClosed),
            Err(_) => {
                self.shared.remove_pending_catalog_update(corr);
                Err(ScopeCallError::Timeout)
            }
        }
    }

    /// Emit an uncorrelated Push on a live route.
    pub async fn push(
        &self,
        handle: &RouteHandle,
        body: Vec<u8>,
        admission_class: Option<AdmissionClass>,
    ) -> Result<(), SubcModuleError> {
        self.validate_route(*handle)?;
        let writer = self
            .shared
            .lock_inner()
            .writer
            .clone()
            .ok_or(SubcModuleError::WriterClosed)?;
        let frame = Frame::build_with_version(
            self.shared.negotiated_ver,
            FrameType::Push,
            data_flags().with_admission_class(admission_class.unwrap_or(AdmissionClass::Normal)),
            handle.channel,
            handle.epoch,
            0,
            body,
        )
        .map_err(SubcModuleError::FrameBuild)?;
        send_outbound(&writer, frame).await
    }

    /// Number of unknown or stale route frames silently dropped by endpoint validation.
    pub fn dropped_route_frames(&self) -> u64 {
        self.shared.dropped_route_frames.load(Ordering::Relaxed)
    }

    fn validate_route(&self, handle: RouteHandle) -> Result<(), SubcModuleError> {
        if handle.connection_token() != self.shared.connection_token {
            return Err(SubcModuleError::StaleRouteHandle(handle));
        }
        let routes = self
            .shared
            .live_routes
            .lock()
            .map_err(|_| SubcModuleError::InFlightPoisoned)?;
        if routes.get(&handle.channel) == Some(&handle) {
            Ok(())
        } else {
            Err(SubcModuleError::StaleRouteHandle(handle))
        }
    }

    fn install_route(&self, handle: RouteHandle) -> Result<(), SubcModuleError> {
        self.shared
            .live_routes
            .lock()
            .map_err(|_| SubcModuleError::InFlightPoisoned)?
            .insert(handle.channel, handle);
        Ok(())
    }

    fn installed_route(&self, channel: u16) -> Result<Option<RouteHandle>, SubcModuleError> {
        Ok(self
            .shared
            .live_routes
            .lock()
            .map_err(|_| SubcModuleError::InFlightPoisoned)?
            .get(&channel)
            .copied())
    }

    fn remove_route(&self, handle: RouteHandle) -> Result<bool, SubcModuleError> {
        let mut routes = self
            .shared
            .live_routes
            .lock()
            .map_err(|_| SubcModuleError::InFlightPoisoned)?;
        if routes.get(&handle.channel) == Some(&handle) {
            routes.remove(&handle.channel);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn validate_ingress(&self, channel: u16, epoch: u32) -> Result<bool, SubcModuleError> {
        let handle = RouteHandle::new(channel, epoch, self.shared.connection_token);
        let valid = self
            .shared
            .live_routes
            .lock()
            .map_err(|_| SubcModuleError::InFlightPoisoned)?
            .get(&channel)
            == Some(&handle);
        if !valid {
            self.shared
                .dropped_route_frames
                .fetch_add(1, Ordering::Relaxed);
        }
        Ok(valid)
    }

    fn route_handle(&self, channel: u16, epoch: u32) -> RouteHandle {
        RouteHandle::new(channel, epoch, self.shared.connection_token)
    }

    fn handle_control_reply(&self, frame: Frame) -> bool {
        let operator_reply = self
            .shared
            .lock_inner()
            .pending_operator_confirms
            .remove(&frame.header.corr);
        if let Some(reply) = operator_reply {
            let _ = reply.send(frame);
            return true;
        }
        let Some(reply) = self.shared.take_pending_catalog_update(frame.header.corr) else {
            return false;
        };
        let result = match frame.header.ty {
            FrameType::Response => serde_json::from_slice::<ModuleControlResponseToModule>(
                &frame.body,
            )
            .map_err(|err| {
                CatalogUpdateError::Protocol(format!("invalid module control response body: {err}"))
            }),
            FrameType::Error => match serde_json::from_slice::<ErrorBody>(&frame.body) {
                Ok(body) => Err(match body.code.as_str() {
                    "catalog_update_frozen_field" => CatalogUpdateError::FrozenField(body),
                    "not_registered" => CatalogUpdateError::NotRegistered(body),
                    _ => CatalogUpdateError::Rejected(body),
                }),
                Err(err) => Err(CatalogUpdateError::Protocol(format!(
                    "invalid catalog.update error body: {err}"
                ))),
            },
            ty => Err(CatalogUpdateError::Protocol(format!(
                "unexpected catalog.update terminal frame: {ty:?}"
            ))),
        };
        let _ = reply.send(result);
        true
    }

    fn close_connection(&self) {
        self.shared.close_connection();
    }
}

impl ModuleHandleShared {
    fn begin_catalog_update(&self) -> Result<CatalogUpdateRequest, CatalogUpdateError> {
        let mut inner = self.lock_inner();
        if inner.closed {
            return Err(CatalogUpdateError::ConnectionClosed);
        }
        let Some(writer) = inner.writer.clone() else {
            drop(inner);
            self.close_connection();
            return Err(CatalogUpdateError::ConnectionClosed);
        };
        let Some(corr) = next_module_control_corr(&mut inner) else {
            drop(inner);
            self.close_connection();
            return Err(CatalogUpdateError::ConnectionClosed);
        };
        let (tx, rx) = oneshot::channel();
        inner.pending_catalog_updates.insert(corr, tx);
        Ok((corr, writer, rx))
    }

    fn begin_operator_confirm(&self) -> Result<OperatorConfirmRequestWaiter, OperatorConfirmError> {
        let mut inner = self.lock_inner();
        if inner.closed {
            return Err(OperatorConfirmError::PresenceUnavailable);
        }
        let Some(writer) = inner.writer.clone() else {
            drop(inner);
            self.close_connection();
            return Err(OperatorConfirmError::PresenceUnavailable);
        };
        let Some(corr) = next_module_control_corr(&mut inner) else {
            drop(inner);
            self.close_connection();
            return Err(OperatorConfirmError::PresenceUnavailable);
        };
        let (tx, rx) = oneshot::channel();
        inner.pending_operator_confirms.insert(corr, tx);
        Ok((corr, writer, rx))
    }

    fn take_pending_catalog_update(&self, corr: u64) -> Option<CatalogUpdateReply> {
        self.lock_inner().pending_catalog_updates.remove(&corr)
    }

    fn remove_pending_catalog_update(&self, corr: u64) {
        self.lock_inner().pending_catalog_updates.remove(&corr);
    }

    fn close_connection(&self) {
        let pending = {
            let mut inner = self.lock_inner();
            if inner.closed {
                return;
            }
            inner.closed = true;
            inner.writer = None;
            self.close_token.cancel();
            // Dropping these senders wakes raw-frame waiters immediately.
            inner.pending_operator_confirms.clear();
            inner
                .pending_catalog_updates
                .drain()
                .map(|(_, reply)| reply)
                .collect::<Vec<_>>()
        };
        self.clear_live_routes();
        for reply in pending {
            let _ = reply.send(Err(CatalogUpdateError::ConnectionClosed));
        }
    }

    fn clear_live_routes(&self) {
        if let Ok(mut routes) = self.live_routes.lock() {
            routes.clear();
        }
    }

    fn lock_inner(&self) -> std::sync::MutexGuard<'_, ModuleHandleState> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn next_module_control_corr(inner: &mut ModuleHandleState) -> Option<u64> {
    let corr = inner.next_corr?;
    inner.next_corr = corr.checked_add(1);
    Some(corr)
}

struct OperatorConfirmGuard {
    shared: Arc<ModuleHandleShared>,
    runtime: tokio::runtime::Handle,
    writer: mpsc::Sender<Frame>,
    corr: u64,
    accepted: bool,
    armed: bool,
}

impl Drop for OperatorConfirmGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.shared
            .lock_inner()
            .pending_operator_confirms
            .remove(&self.corr);
        if !self.accepted {
            return;
        }
        let Ok(cancel) = Frame::build_with_version(
            self.shared.negotiated_ver,
            FrameType::Cancel,
            control_flags(),
            0,
            0,
            self.corr,
            Vec::new(),
        ) else {
            return;
        };
        match self.writer.try_send(cancel) {
            Ok(()) | Err(mpsc::error::TrySendError::Closed(_)) => {}
            Err(mpsc::error::TrySendError::Full(cancel)) => {
                // The connection owns this task even if the future was polled
                // on another runtime or dropped from a plain thread. A full
                // egress buffer delays withdrawal; it must not discard it.
                let writer = self.writer.clone();
                self.runtime.spawn(async move {
                    let _ = writer.send(cancel).await;
                });
            }
        }
    }
}

/// Failure to authorise a write through [`ModuleHandle::confirm_operator`].
/// Every variant requires the module to refuse the write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum OperatorConfirmError {
    Declined,
    PresenceUnavailable,
    SummaryInvalid,
    NotPermitted,
    Unsupported,
}

impl fmt::Display for OperatorConfirmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Declined => "operator declined the write",
            Self::PresenceUnavailable => "operator confirmation is unavailable",
            Self::SummaryInvalid => "operator confirmation summary is invalid",
            Self::NotPermitted => "operator confirmation is not permitted for this route",
            Self::Unsupported => "daemon does not support operator confirmation",
        })
    }
}

impl Error for OperatorConfirmError {}

/// Errors returned by [`ModuleHandle::catalog_update`].
#[derive(Debug)]
pub enum CatalogUpdateError {
    NotSupported,
    FrozenField(ErrorBody),
    NotRegistered(ErrorBody),
    Rejected(ErrorBody),
    Timeout,
    ConnectionClosed,
    Protocol(String),
}

impl fmt::Display for CatalogUpdateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotSupported => write!(
                f,
                "daemon HELLO_ACK did not advertise catalog.update support"
            ),
            Self::FrozenField(body) => {
                write!(f, "catalog.update rejected frozen field: {}", body.message)
            }
            Self::NotRegistered(body) => write!(
                f,
                "catalog.update requires a registered module: {}",
                body.message
            ),
            Self::Rejected(body) => write!(
                f,
                "catalog.update rejected by subc: {} ({})",
                body.code, body.message
            ),
            Self::Timeout => write!(f, "catalog.update timed out waiting for an ACK"),
            Self::ConnectionClosed => {
                write!(f, "subc connection closed before catalog.update completed")
            }
            Self::Protocol(message) => write!(f, "catalog.update protocol error: {message}"),
        }
    }
}

impl Error for CatalogUpdateError {}

/// Errors returned by [`ModuleHandle::scope_sync`] and
/// [`ModuleHandle::scope_describe`].
///
/// Non-exhaustive so a later failure kind can be added without breaking
/// callers' matches; match the variants you handle and treat the rest as an
/// unverifiable answer.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ScopeCallError {
    /// The daemon's HELLO_ACK did not list `op` in `subc_ops`, so nothing was
    /// sent.
    NotSupported { op: &'static str },
    /// The daemon refused the request with a channel-0 Error frame. `code` is
    /// the daemon's code as sent, to compare against the constants in
    /// `subc_protocol::error_codes` (`SCOPE_SYNC_STALE`,
    /// `SCOPE_SYNC_NOT_AUTHORITY`, `SCOPE_LIVE_LIMIT_EXCEEDED`, ...).
    Refused { code: String, message: String },
    /// No reply arrived in time. The daemon may still have applied a sync.
    Timeout,
    /// The connection closed before a reply arrived.
    ConnectionClosed,
    /// The reply could not be read, or was a reply to a different op.
    Protocol(String),
}

impl ScopeCallError {
    /// The daemon's error code, when the daemon refused the request.
    pub fn code(&self) -> Option<&str> {
        match self {
            Self::Refused { code, .. } => Some(code),
            _ => None,
        }
    }

    /// Convert an error from the pending-reply handling that scope calls share
    /// with `catalog_update`, which reports failures as `CatalogUpdateError`.
    /// For a daemon Error frame, keep the daemon's code and message whatever
    /// the code is.
    fn from_control_error(err: CatalogUpdateError) -> Self {
        match err {
            CatalogUpdateError::FrozenField(body)
            | CatalogUpdateError::NotRegistered(body)
            | CatalogUpdateError::Rejected(body) => Self::Refused {
                code: body.code,
                message: body.message,
            },
            CatalogUpdateError::Timeout => Self::Timeout,
            CatalogUpdateError::ConnectionClosed => Self::ConnectionClosed,
            CatalogUpdateError::Protocol(message) => Self::Protocol(message),
            // The shared reply handling never returns this: whether the
            // daemon supports an op is checked before the request is sent.
            CatalogUpdateError::NotSupported => {
                Self::Protocol("unexpected NotSupported from the control reply table".into())
            }
        }
    }
}

impl fmt::Display for ScopeCallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotSupported { op } => {
                write!(f, "daemon HELLO_ACK did not advertise {op} support")
            }
            Self::Refused { code, message } => {
                write!(f, "subc refused the request: {code} ({message})")
            }
            Self::Timeout => write!(f, "timed out waiting for the daemon's reply"),
            Self::ConnectionClosed => {
                write!(f, "subc connection closed before the daemon replied")
            }
            Self::Protocol(message) => write!(f, "scope call protocol error: {message}"),
        }
    }
}

impl Error for ScopeCallError {}

/// Trait implemented by a module for its business logic. The serve functions in
/// this crate own all wire-protocol plumbing.
///
/// # Stopping cleanly: drains
///
/// Before the daemon stops a module (restart, reload, disable, swap, or its own
/// shutdown) it closes route admission, tells the module with
/// [`ModuleHandler::on_draining`], waits for the module's in-flight requests to
/// finish, and only then sends GOODBYE and closes the connection. The wait ends
/// at the drain deadline whether or not the work is done.
///
/// Requests the daemon forwarded are counted by the daemon itself. Work it
/// cannot see (a job a request started in the background, a batch being
/// flushed) holds the drain open only if the module says so. The module does
/// that with a counter it reports in its health answer (a "health gauge"),
/// and a manifest entry telling the daemon that the counter means "busy": a
/// `Busy` self-signal anchored to those health gauges. Declare it, and report those
/// gauges in [`ModuleHandler::health`]'s `metrics` above zero until the work
/// has finished. During a drain the daemon probes `health` and waits until
/// every declared gauge reads 0 (or the deadline passes); a declared gauge
/// missing from the report counts as busy. The manifest entry, as built by the
/// `echo-module` example in this crate:
///
/// ```
/// use subc_protocol::manifest::{
///     ModuleManifest, SelfSignalDeclaration, SelfSignalEffect, SelfSignalKind, SignalAnchor,
/// };
///
/// let manifest = ModuleManifest::builder("my-module", "1.0.0")
///     .self_signals(Some(vec![SelfSignalDeclaration {
///         name: "background_work".to_string(),
///         kind: SelfSignalKind::Busy,
///         effect: SelfSignalEffect::Observe,
///         anchored_to: SignalAnchor::HealthGauges {
///             gauges: vec!["background_jobs".to_string()],
///         },
///         cadence: None,
///         domain: None,
///         note: None,
///     }]))
///     .build();
/// # let _ = manifest;
/// ```
///
/// and `health` then reports `{"background_jobs": <count>}` in `metrics`. Count
/// only work whose result would still be delivered if it finished during the
/// drain; see [`SelfSignalDeclaration::kind`](subc_protocol::manifest::SelfSignalDeclaration::kind).
#[async_trait]
pub trait ModuleHandler: Send + Sync + 'static {
    /// Handle a data-plane request on a route channel. Return a unary response, a
    /// typed error, or stream interim events via [`RequestCtx::emit`] and return
    /// [`HandlerOutcome::Streamed`]. Each request runs in its own task so one slow
    /// handler cannot head-of-line-block another route.
    async fn handle(&self, ctx: RequestCtx, body: Vec<u8>) -> HandlerOutcome;

    /// Called once after HELLO_ACK so the module can inspect the ack body for
    /// negotiated capabilities and any storage descriptor supplied by the daemon.
    async fn on_hello_ack(&self, _ack: &ModuleHelloAckBody) {}

    /// Decide a route.bind. This hook is decision-only and must not emit route traffic.
    async fn on_bind(&self, _req: &RouteBindRequest) -> BindDecision {
        BindDecision::accept()
    }

    /// Called after an accepted bind ACK is queued and the handle is installed.
    async fn on_bound(&self, _handle: &RouteHandle) {}

    /// Return cheap in-memory health for the module.
    ///
    /// The default reports `Ok` with the detail "no health implementation;
    /// inherited default". The detail lets an operator reading
    /// `ck health <module>` tell "measured, nothing wrong" from "nobody
    /// measured", but the daemon decides on the status alone and never parses
    /// the detail, so to the daemon the default looks like a module that checked
    /// itself and found nothing wrong. It stays a default because health is
    /// optional: a module that advertises no health capability is never probed,
    /// so the value is never read. A module that does advertise health and
    /// keeps this default can report no fault beyond the serve helper's own
    /// saturation check.
    ///
    /// The serve helper calls this when the daemon sends `health.check`, on a
    /// task of its own rather than one of the 64 data-request slots, so a health
    /// check never waits behind the requests it reports on. It then folds in
    /// dispatch saturation: if all 64 slots are in use and a request has waited
    /// more than two seconds for one, an `Ok` reply becomes `Degraded`, and the
    /// slot count and oldest wait are prepended to `detail`. A `Degraded` or
    /// `Failing` status, the module's own detail and its metrics are kept.
    ///
    /// An implementation must not block: no blocking lock, no disk access and
    /// no subprocess on this path. Derive the status from signals the dispatch
    /// path already records in memory, such as a monotonic heartbeat or the age
    /// of the oldest queued item. A health reply that waits on a slow shared
    /// resource stalls under exactly the conditions it is meant to report,
    /// because that resource is what degrades first.
    async fn health(&self) -> HealthReport {
        // Say that nobody measured, rather than that everything is fine.
        //
        // The status stays Ok because a module advertising no health capability
        // is never probed, and one that advertises health but has nothing to
        // report is not unhealthy. What changes is that the report identifies
        // itself as the inherited default, so an operator reading
        // `ck health <module>` can tell "measured, nothing wrong" from "nobody
        // wrote a health path" -- which were previously the same bytes.
        //
        // `detail` is carried verbatim by the daemon and rendered for humans;
        // nothing parses it, so this is display-only and cannot change any
        // supervision decision.
        HealthReport {
            detail: Some("no health implementation; inherited default".to_string()),
            ..HealthReport::ok()
        }
    }

    /// A route was torn down, rejected, or abandoned before its bind ACK was queued.
    async fn on_route_gone(&self, _handle: &RouteHandle) {}

    /// The daemon has started draining this module: it will stop it for
    /// `reason` (a restart, reload, disable, or the daemon's own shutdown), and
    /// has already stopped admitting new routes to it. Stop taking new work
    /// here and finish or hand off what is in flight; see the trait docs for
    /// how to keep the drain open while background work finishes.
    ///
    /// `deadline` is the wall-clock time by which the module must be done. It
    /// is "no later than", never a grant of that much time: the daemon enforces
    /// its own ceiling on a monotonic clock and tears the connection down when
    /// that runs out, and after the host sleeps the ceiling can fall before
    /// this wall-clock time does. Aim to finish early.
    ///
    /// Called once for each drain notice the daemon sends, on its own task, so
    /// a slow hook never holds up pings, requests, or the GOODBYE that ends the
    /// drain. It is always called before that GOODBYE is handled (and before
    /// [`ModuleHandler::on_connection_end`]); it may still be running after
    /// them.
    async fn on_draining(&self, _reason: RouteCloseReason, _deadline: SystemTime) {}

    /// The daemon connection ended without a protocol error, and serving is
    /// about to return `Ok(())`. `end` says how: a daemon GOODBYE is a planned
    /// stop, a clean EOF or a reset means the daemon went away without one.
    /// Called once, after every in-flight request has been cancelled. Not
    /// called when serving returns an error.
    async fn on_connection_end(&self, _end: ConnectionEnd) {}
}

/// How a module's daemon connection ended, when it ended without a protocol
/// error. `serve` returns `Ok(())` for all of these; a module that wants to
/// log or act on the difference reads it in
/// [`ModuleHandler::on_connection_end`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ConnectionEnd {
    /// The daemon sent GOODBYE on channel 0: a planned stop.
    Goodbye,
    /// The daemon closed the connection without GOODBYE.
    Eof,
    /// The connection was reset or aborted (how a killed daemon's socket ends
    /// on Windows).
    Reset,
    /// The module closed the connection itself through its [`ModuleHandle`].
    Closed,
}

impl ConnectionEnd {
    /// Stable lower-case name for logs: `goodbye`, `eof`, `reset`, `closed`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Goodbye => "goodbye",
            Self::Eof => "eof",
            Self::Reset => "reset",
            Self::Closed => "closed",
        }
    }
}

impl std::fmt::Display for ConnectionEnd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The terminal result of a module request handler.
#[derive(Debug, Clone, PartialEq)]
pub enum HandlerOutcome {
    /// Send a Response frame carrying these bytes.
    Response(Vec<u8>),
    /// Send an Error frame carrying an [`ErrorBody`] with this code and message.
    Error { code: String, message: String },
    /// Send an Error frame with stable machine-readable detail.
    ErrorWithDetail {
        code: String,
        message: String,
        detail: serde_json::Value,
    },
    /// The handler emitted stream data with [`RequestCtx::emit`]; the serve code
    /// sends the StreamEnd terminal frame.
    Streamed,
}

/// Per-request context. Retains the full route handle and correlation id, provides
/// interim stream emission, and exposes a cancellation signal.
#[derive(Clone)]
pub struct RequestCtx {
    handle: RouteHandle,
    corr: u64,
    ver: u8,
    egress: mpsc::Sender<Frame>,
    module_handle: ModuleHandle,
    cancelled: CancellationToken,
}

impl RequestCtx {
    /// Full route handle retained from ingress.
    pub fn route_handle(&self) -> RouteHandle {
        self.handle
    }

    /// Correlation id for this request.
    pub fn corr(&self) -> u64 {
        self.corr
    }

    /// Emit an interim StreamData frame on this request's `(channel, corr)`. Once
    /// the request is cancelled or its route is gone, late emits are dropped.
    pub async fn emit(&self, body: Vec<u8>) -> Result<(), SubcModuleError> {
        self.emit_with_admission(body, None).await
    }

    /// Emit StreamData with an explicit admission class. `None` means NORMAL.
    pub async fn emit_with_admission(
        &self,
        body: Vec<u8>,
        admission_class: Option<AdmissionClass>,
    ) -> Result<(), SubcModuleError> {
        self.module_handle.validate_route(self.handle)?;
        if self.cancelled.is_cancelled() {
            return Ok(());
        }
        self.send_frame(
            FrameType::StreamData,
            data_flags().with_admission_class(admission_class.unwrap_or(AdmissionClass::Normal)),
            body,
        )
        .await
    }

    /// Completes when the other side sends Cancel for this request or the route
    /// is torn down.
    pub fn cancelled(&self) -> WaitForCancellationFuture<'_> {
        self.cancelled.cancelled()
    }

    /// Return a cloneable cancellation token for code that prefers token polling.
    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancelled.clone()
    }

    async fn send_frame(
        &self,
        frame_type: FrameType,
        flags: Flags,
        body: Vec<u8>,
    ) -> Result<(), SubcModuleError> {
        self.module_handle.validate_route(self.handle)?;
        let frame = Frame::build_with_version(
            self.ver,
            frame_type,
            flags,
            self.handle.channel,
            self.handle.epoch,
            self.corr,
            body,
        )
        .map_err(SubcModuleError::FrameBuild)?;
        send_outbound(&self.egress, frame).await
    }
}

/// Adds one to `counter` and returns the value it held before, or `None` once
/// the counter is at `u64::MAX`. Written as a compare-exchange loop rather than
/// with `fetch_update` (deprecated in Rust 1.99) or its replacement
/// `try_update` (absent before 1.99), so it builds on both toolchains.
fn checked_increment(counter: &AtomicU64) -> Option<u64> {
    let mut current = counter.load(Ordering::Relaxed);
    loop {
        let next = current.checked_add(1)?;
        match counter.compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(previous) => return Some(previous),
            Err(actual) => current = actual,
        }
    }
}

/// Route-bind request delivered on channel 0.
///
/// `#[non_exhaustive]` so it can gain fields in a later release without
/// breaking the handlers that read it. The SDK builds it from the daemon's
/// bind; code outside this crate (a module's own tests of its `on_bind`)
/// builds one with [`RouteBindRequest::new`] and the `with_*` setters.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct RouteBindRequest {
    pub handle: RouteHandle,
    pub target: RouteTarget,
    pub identity: BindIdentity,
    pub principal: Option<Principal>,
    /// Consumer-declared reverse-request capabilities for this bind. This is a
    /// declaration, not a verified privilege; providers treat an absent field as
    /// no reverse-request capability. Known MCP method-family values today are
    /// "elicitation", "sampling", and "roots".
    pub consumer_capabilities: Option<Vec<String>>,
    /// The versions of provider roles the consumer declared it speaks on this
    /// route, role name to version (`{"tool-provider": "v1"}`), as the daemon
    /// forwarded them. A declaration, not a verified privilege: use it to pick
    /// which version of a role's wire shape to speak. `None` means the consumer
    /// declared none (a legacy consumer, or a daemon that predates the field);
    /// it is never an empty map.
    pub role_versions: Option<std::collections::BTreeMap<String, String>>,
    /// Opaque admission facts relayed by subc from its configured carrier.
    pub admission_facts: Option<serde_json::Value>,
    /// The daemon's stamp of the scope the route was admitted under (owner,
    /// ref, epoch, kind, attributes), copied from the bind unchanged. It is
    /// the daemon's, never the opener's, so a provider may act on it, and it
    /// is fixed for the route's life: a change that revokes authority closes
    /// the route. `None` means the route was opened without a scope, or by a
    /// daemon that predates scopes.
    pub scope: Option<ScopeStamp>,
}

impl RouteBindRequest {
    /// A bind with the members every bind has and every optional one absent:
    /// no principal, no consumer capabilities, no role versions, no admission
    /// facts and no scope. Set those with the `with_*` methods.
    pub fn new(handle: RouteHandle, target: RouteTarget, identity: BindIdentity) -> Self {
        Self {
            handle,
            target,
            identity,
            principal: None,
            consumer_capabilities: None,
            role_versions: None,
            admission_facts: None,
            scope: None,
        }
    }

    /// Set [`Self::principal`].
    pub fn with_principal(mut self, principal: Principal) -> Self {
        self.principal = Some(principal);
        self
    }

    /// Set [`Self::consumer_capabilities`].
    pub fn with_consumer_capabilities(mut self, consumer_capabilities: Vec<String>) -> Self {
        self.consumer_capabilities = Some(consumer_capabilities);
        self
    }

    /// Set [`Self::role_versions`].
    pub fn with_role_versions(
        mut self,
        role_versions: std::collections::BTreeMap<String, String>,
    ) -> Self {
        self.role_versions = Some(role_versions);
        self
    }

    /// Set [`Self::admission_facts`].
    pub fn with_admission_facts(mut self, admission_facts: serde_json::Value) -> Self {
        self.admission_facts = Some(admission_facts);
        self
    }

    /// Set [`Self::scope`].
    pub fn with_scope(mut self, scope: ScopeStamp) -> Self {
        self.scope = Some(scope);
        self
    }
}

/// Decision returned by [`ModuleHandler::on_bind`].
#[derive(Debug, Clone)]
pub struct BindDecision {
    kind: BindDecisionKind,
}

impl BindDecision {
    /// Accept the route.bind request.
    pub fn accept() -> Self {
        Self {
            kind: BindDecisionKind::Accept,
        }
    }

    /// Reject the route.bind request with a typed Error frame.
    pub fn reject(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            kind: BindDecisionKind::Reject {
                code: code.into(),
                message: message.into(),
            },
        }
    }
}

#[derive(Debug, Clone)]
enum BindDecisionKind {
    Accept,
    Reject { code: String, message: String },
}

/// Run a module to completion. Reads `--subc <connection-file>` from args, uses
/// `SUBC_MODULE_ID` when set by the process that launched the module, connects,
/// authenticates, sends HELLO, waits for HELLO_ACK, then serves frames until
/// GOODBYE or clean EOF. Which of those ended it is passed to
/// [`ModuleHandler::on_connection_end`].
pub async fn serve<H>(manifest: ModuleManifest, handler: H) -> Result<(), SubcModuleError>
where
    H: ModuleHandler,
{
    let (_handle, serve_future) = serve_from_env_with_handle(manifest, handler).await?;
    serve_future.await
}

/// Connect and register using the same launch inputs as [`serve`], returning a
/// cloneable handle and the future that keeps the module serving.
///
/// Reads `--subc <connection-file>` (or `--subc=<connection-file>`) from argv
/// and overrides the manifest's module id with `SUBC_MODULE_ID` when present.
/// Authentication, the launch-secret pipe, HELLO and their errors are identical
/// to [`serve`]. Unlike [`serve_with_handle`], the caller need not duplicate
/// argument parsing or launch-environment handling.
///
/// The returned serve future **must be awaited or spawned**. A module can give
/// the returned handle to its handler's shared state before starting that future,
/// so the handler can call [`ModuleHandle::confirm_operator`].
pub async fn serve_from_env_with_handle<H>(
    mut manifest: ModuleManifest,
    handler: H,
) -> Result<(ModuleHandle, ModuleServeFuture), SubcModuleError>
where
    H: ModuleHandler,
{
    let connection_file = parse_subc_arg(env::args_os().skip(1))?;
    if let Some(module_id) = module_id_from_env()? {
        manifest.module_id = module_id;
    }
    serve_with_handle(&connection_file, manifest, handler).await
}

/// Run a module with an explicit connection-file path. The manifest is sent as
/// provided; callers that need a nonstandard module id should set it before calling.
pub async fn serve_with<H>(
    connection_file: &Path,
    manifest: ModuleManifest,
    handler: H,
) -> Result<(), SubcModuleError>
where
    H: ModuleHandler,
{
    let (_handle, serve_future) = serve_with_handle(connection_file, manifest, handler).await?;
    serve_future.await
}

/// Connect, register the module, and return a cloneable handle plus the future that
/// must be awaited or spawned to keep serving the connection.
pub async fn serve_with_handle<H>(
    connection_file: &Path,
    manifest: ModuleManifest,
    handler: H,
) -> Result<(ModuleHandle, ModuleServeFuture), SubcModuleError>
where
    H: ModuleHandler,
{
    let stream = connect_to_subc(connection_file).await?;
    let (mut read_half, write_half) = tokio::io::split(stream);
    let (tx, rx) = mpsc::channel::<Frame>(EGRESS_BUFFER);
    let connection_runtime = tokio::runtime::Handle::current();
    let writer = tokio::spawn(drain_writer(write_half, rx));
    let handler = Arc::new(handler);

    send_hello(&tx, manifest).await?;
    let ack = expect_hello_ack(&mut read_half).await?;
    handler.on_hello_ack(&ack).await;

    let connection_token = checked_increment(&NEXT_MODULE_CONNECTION_TOKEN)
        .ok_or(SubcModuleError::ConnectionTokenExhausted)?;
    let close_token = CancellationToken::new();
    let handle = ModuleHandle::new(
        &ack,
        tx.clone(),
        connection_token,
        close_token,
        connection_runtime,
    );
    let serve_future = connection_serve_future(read_half, tx, handler, handle.clone(), writer);
    Ok((handle, serve_future))
}

// Both real connections and the in-memory test stand-in run this lifecycle,
// including request cancellation, connection-end hooks and the writer drain.
fn connection_serve_future<R, H>(
    read_half: R,
    tx: mpsc::Sender<Frame>,
    handler: Arc<H>,
    handle: ModuleHandle,
    writer: tokio::task::JoinHandle<Result<(), FrameIoError>>,
) -> ModuleServeFuture
where
    R: AsyncRead + Unpin + Send + 'static,
    H: ModuleHandler,
{
    let serve_handle = handle.clone();
    Box::pin(async move {
        // Connection loss ends this serve future. Module serving retains no
        // reconnect task or in-flight reconnect gate; a supervisor that needs
        // recovery starts a fresh serve_with_handle invocation.
        let loop_result =
            module_loop(read_half, tx, Arc::clone(&handler), serve_handle.clone()).await;
        serve_handle.close_connection();
        let loop_result = match loop_result {
            Ok(end) => {
                handler.on_connection_end(end).await;
                Ok(())
            }
            Err(error) => Err(error),
        };

        // module_loop cancelled every request in flight, so a handler that
        // honours cancellation lets the writer drain and finish. One that
        // ignores it keeps an egress sender alive, and the writer would wait on
        // it forever. It must not keep the process alive past its stop: the
        // daemon gives a module only its stop budget before killing it. So the
        // writer gets a bounded drain, then is aborted with whatever it holds.
        let mut writer = writer;
        let writer_result = match timeout(WRITER_DRAIN_LIMIT, &mut writer).await {
            Ok(joined) => joined.map_err(SubcModuleError::WriterTask),
            Err(_) => {
                writer.abort();
                Ok(Ok(()))
            }
        };
        match (loop_result, writer_result) {
            (Err(loop_err), _) => Err(loop_err),
            (Ok(()), Ok(Ok(()))) => Ok(()),
            // The read loop already saw the daemon go away; the writer failing
            // to flush its remaining frames to that dead socket (BrokenPipe on
            // Unix, ConnectionReset on Windows) is part of the same terminal,
            // not a distinct fault.
            (Ok(()), Ok(Err(FrameIoError::Io(err))))
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::ConnectionAborted
                        | std::io::ErrorKind::BrokenPipe
                ) =>
            {
                Ok(())
            }
            (Ok(()), Ok(Err(writer_err))) => Err(SubcModuleError::FrameIo(writer_err)),
            (Ok(()), Err(join_err)) => Err(join_err),
        }
    })
}

async fn module_loop<R, H>(
    mut reader: R,
    egress: mpsc::Sender<Frame>,
    handler: Arc<H>,
    module_handle: ModuleHandle,
) -> Result<ConnectionEnd, SubcModuleError>
where
    R: AsyncRead + Unpin,
    H: ModuleHandler,
{
    let dispatcher = RequestDispatcher::new();
    let result = serve_frames(&mut reader, &egress, &handler, &module_handle, &dispatcher).await;
    // A drain notice is followed shortly by the daemon's GOODBYE, and the two
    // can arrive in one read. Let every drain hook start before the
    // connection's end (GOODBYE or otherwise) is acted on, so a module always
    // hears about its drain before it hears that the connection is over.
    dispatcher.wait_for_draining_hooks_to_start().await;
    dispatcher.cancel_all();
    result
}

async fn serve_frames<R, H>(
    reader: &mut R,
    egress: &mpsc::Sender<Frame>,
    handler: &Arc<H>,
    module_handle: &ModuleHandle,
    dispatcher: &RequestDispatcher,
) -> Result<ConnectionEnd, SubcModuleError>
where
    R: AsyncRead + Unpin,
    H: ModuleHandler,
{
    loop {
        let read = tokio::select! {
            () = module_handle.shared.close_token.cancelled() => return Ok(ConnectionEnd::Closed),
            read = read_frame(reader) => read,
        };
        let frame = match read {
            Ok(Some(frame)) => frame,
            // Clean EOF: the daemon closed the connection.
            Ok(None) => return Ok(ConnectionEnd::Eof),
            // A reset/abort on the read path also means the daemon is gone. On
            // Unix a killed daemon closes the socket with FIN (clean EOF above),
            // but Windows sends RST on process death, surfacing here as
            // ConnectionReset. Both are the same "serve until the daemon goes
            // away" terminal, so normalize to a clean exit for a
            // platform-independent serve() contract.
            Err(FrameIoError::Io(err))
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
                ) =>
            {
                return Ok(ConnectionEnd::Reset);
            }
            Err(err) => return Err(SubcModuleError::FrameIo(err)),
        };
        if !handle_frame(
            frame,
            egress,
            Arc::clone(handler),
            dispatcher.clone(),
            module_handle.clone(),
        )
        .await?
        {
            // handle_frame returns false only for a channel-0 GOODBYE.
            return Ok(ConnectionEnd::Goodbye);
        }
    }
}

async fn handle_frame<H>(
    frame: Frame,
    egress: &mpsc::Sender<Frame>,
    handler: Arc<H>,
    dispatcher: RequestDispatcher,
    module_handle: ModuleHandle,
) -> Result<bool, SubcModuleError>
where
    H: ModuleHandler,
{
    if frame.header.channel != 0
        && !module_handle.validate_ingress(frame.header.channel, frame.header.epoch)?
    {
        return Ok(true);
    }
    match frame.header.ty {
        FrameType::Ping if frame.header.channel == 0 => {
            let pong = Frame::build_with_version(
                frame.header.ver,
                FrameType::Pong,
                frame.header.flags,
                0,
                0,
                frame.header.corr,
                Vec::new(),
            )
            .map_err(SubcModuleError::FrameBuild)?;
            send_outbound(egress, pong).await?;
            Ok(true)
        }
        FrameType::Goodbye if frame.header.channel == 0 => Ok(false),
        FrameType::Goodbye => {
            let handle = module_handle.route_handle(frame.header.channel, frame.header.epoch);
            if module_handle.remove_route(handle)? {
                cancel_handle(&dispatcher.in_flight, handle)?;
                handler.on_route_gone(&handle).await;
            }
            Ok(true)
        }
        FrameType::Response if frame.header.channel == 0 => {
            let _ = module_handle.handle_control_reply(frame);
            Ok(true)
        }
        FrameType::Error if frame.header.channel == 0 => {
            let _ = module_handle.handle_control_reply(frame);
            Ok(true)
        }
        FrameType::Cancel => {
            handle_cancel(frame, &dispatcher.in_flight)?;
            Ok(true)
        }
        FrameType::Request if frame.header.channel == 0 => {
            handle_control_request(frame, egress, handler, dispatcher, module_handle.clone())
                .await?;
            Ok(true)
        }
        FrameType::Request => {
            spawn_data_request(frame, egress.clone(), handler, dispatcher, module_handle)?;
            Ok(true)
        }
        FrameType::Push if frame.header.channel == 0 => {
            handle_control_push(&frame, handler, &dispatcher);
            Ok(true)
        }
        _ => Ok(true),
    }
}

/// Act on a one-way channel-0 command from the daemon. Never fails the
/// connection: a push this SDK cannot decode is most likely a newer daemon's
/// command, and ignoring it is what an older module did before this SDK
/// decoded any of them.
fn handle_control_push<H>(frame: &Frame, handler: Arc<H>, dispatcher: &RequestDispatcher)
where
    H: ModuleHandler,
{
    match serde_json::from_slice::<ModuleControlCommand>(&frame.body) {
        Ok(ModuleControlCommand::Draining {
            reason,
            deadline_ms,
        }) => {
            let started = spawn_draining_hook(
                handler,
                sdk_route_close_reason(reason),
                drain_deadline(deadline_ms),
            );
            dispatcher
                .draining_hooks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(started);
        }
        Err(error) => {
            if !dispatcher
                .undecodable_push_logged
                .swap(true, Ordering::Relaxed)
            {
                let op = serde_json::from_slice::<serde_json::Value>(&frame.body)
                    .ok()
                    .and_then(|body| body.get("op")?.as_str().map(str::to_string));
                tracing::warn!(
                    op = op.as_deref().unwrap_or("<none>"),
                    error = %error,
                    "ignoring a channel-0 push from subc that this SDK cannot decode; \
                     further undecodable pushes on this connection are ignored without logging"
                );
            }
        }
    }
}

/// Run `on_draining` on its own task, so a slow hook cannot stall the frame
/// reader. The returned receiver resolves once the hook has been polled for
/// the first time, i.e. has run up to its first await.
fn spawn_draining_hook<H>(
    handler: Arc<H>,
    reason: RouteCloseReason,
    deadline: SystemTime,
) -> oneshot::Receiver<()>
where
    H: ModuleHandler,
{
    let (started_tx, started_rx) = oneshot::channel();
    tokio::spawn(async move {
        let mut hook = handler.on_draining(reason, deadline);
        let mut started_tx = Some(started_tx);
        std::future::poll_fn(|cx| {
            let poll = hook.as_mut().poll(cx);
            if let Some(started) = started_tx.take() {
                let _ = started.send(());
            }
            poll
        })
        .await;
    });
    started_rx
}

/// Convert the wire reason into this crate's public [`RouteCloseReason`], via
/// its wire name so a reason this crate has no variant for arrives as
/// [`RouteCloseReason::Unknown`] rather than being dropped.
fn sdk_route_close_reason(reason: subc_protocol::RouteCloseReason) -> RouteCloseReason {
    match serde_json::to_value(reason) {
        Ok(serde_json::Value::String(wire)) => RouteCloseReason::from_wire(&wire),
        _ => RouteCloseReason::Unknown(format!("{reason:?}")),
    }
}

/// The drain deadline as a wall-clock time. A value too large for this
/// platform's clock becomes "now": the deadline is a "no later than", so
/// reading it early is the safe direction.
fn drain_deadline(deadline_ms: u64) -> SystemTime {
    UNIX_EPOCH
        .checked_add(Duration::from_millis(deadline_ms))
        .unwrap_or_else(SystemTime::now)
}

fn spawn_data_request<H>(
    frame: Frame,
    egress: mpsc::Sender<Frame>,
    handler: Arc<H>,
    dispatcher: RequestDispatcher,
    module_handle: ModuleHandle,
) -> Result<(), SubcModuleError>
where
    H: ModuleHandler,
{
    let handle = module_handle.route_handle(frame.header.channel, frame.header.epoch);
    let corr = frame.header.corr;
    let cancellation = CancellationToken::new();
    {
        let mut guard = lock_in_flight(&dispatcher.in_flight)?;
        guard.insert((handle.channel, handle.epoch, corr), cancellation.clone());
    }

    let ctx = RequestCtx {
        handle,
        corr,
        ver: frame.header.ver,
        egress,
        module_handle,
        cancelled: cancellation,
    };
    let body = frame.body;
    let in_flight = Arc::clone(&dispatcher.in_flight);
    let permits = Arc::clone(&dispatcher.permits);
    let waiting = dispatcher.start_waiting((handle.channel, handle.epoch, corr));
    tokio::spawn(async move {
        let Ok(_permit) = permits.acquire_owned().await else {
            // A closed dispatcher means connection teardown will release every route credit.
            if let Ok(mut guard) = in_flight.lock() {
                guard.remove(&(handle.channel, handle.epoch, corr));
            }
            return;
        };
        drop(waiting);
        if ctx.cancelled.is_cancelled() {
            let _ = send_handler_outcome(
                &ctx,
                HandlerOutcome::Error {
                    code: "cancelled".to_string(),
                    message: "request cancelled".to_string(),
                },
            )
            .await;
            if let Ok(mut guard) = in_flight.lock() {
                guard.remove(&(handle.channel, handle.epoch, corr));
            }
            return;
        }
        let outcome = handler.handle(ctx.clone(), body).await;
        let _ = send_handler_outcome(&ctx, outcome).await;
        if let Ok(mut guard) = in_flight.lock() {
            guard.remove(&(handle.channel, handle.epoch, corr));
        }
    });
    Ok(())
}

fn spawn_health_request<H>(
    frame: Frame,
    egress: mpsc::Sender<Frame>,
    handler: Arc<H>,
    dispatcher: RequestDispatcher,
) -> Result<(), SubcModuleError>
where
    H: ModuleHandler,
{
    let channel = frame.header.channel;
    let epoch = frame.header.epoch;
    let corr = frame.header.corr;
    let ver = frame.header.ver;
    let cancellation = CancellationToken::new();
    {
        let mut guard = lock_in_flight(&dispatcher.in_flight)?;
        guard.insert((channel, epoch, corr), cancellation.clone());
    }

    let in_flight = Arc::clone(&dispatcher.in_flight);
    tokio::spawn(async move {
        // Health must not queue behind the data work whose liveness it reports.
        // Registration and cancellation still use the same in-flight registry.
        if !cancellation.is_cancelled() {
            let report = dispatcher.fold_health(handler.health().await);
            let response = ModuleControlResponse::from(report);
            if let Ok(body) = serde_json::to_vec(&response) {
                if let Ok(frame) = Frame::build_with_version(
                    ver,
                    FrameType::Response,
                    control_flags(),
                    channel,
                    epoch,
                    corr,
                    body,
                ) {
                    let _ = send_outbound(&egress, frame).await;
                }
            }
        }
        if let Ok(mut guard) = in_flight.lock() {
            guard.remove(&(channel, epoch, corr));
        }
    });
    Ok(())
}

async fn send_handler_outcome(
    ctx: &RequestCtx,
    outcome: HandlerOutcome,
) -> Result<(), SubcModuleError> {
    match outcome {
        HandlerOutcome::Response(body) => {
            ctx.send_frame(FrameType::Response, data_flags(), body)
                .await
        }
        HandlerOutcome::Error { code, message } => {
            let body = serde_json::to_vec(&ErrorBody::new(code, message))
                .map_err(SubcModuleError::Json)?;
            ctx.send_frame(FrameType::Error, data_flags(), body).await
        }
        HandlerOutcome::ErrorWithDetail {
            code,
            message,
            detail,
        } => {
            let body = serde_json::to_vec(&ErrorBody::new(code, message).with_detail(detail))
                .map_err(SubcModuleError::Json)?;
            ctx.send_frame(FrameType::Error, data_flags(), body).await
        }
        HandlerOutcome::Streamed => {
            ctx.send_frame(FrameType::StreamEnd, data_flags(), Vec::new())
                .await
        }
    }
}

fn handle_cancel(frame: Frame, in_flight: &InFlight) -> Result<(), SubcModuleError> {
    let cancellation = {
        let guard = lock_in_flight(in_flight)?;
        guard
            .get(&(frame.header.channel, frame.header.epoch, frame.header.corr))
            .cloned()
    };
    if let Some(cancellation) = cancellation {
        cancellation.cancel();
    }
    Ok(())
}

fn cancel_handle(in_flight: &InFlight, handle: RouteHandle) -> Result<(), SubcModuleError> {
    let cancelled = {
        let mut guard = lock_in_flight(in_flight)?;
        let keys = guard
            .keys()
            .copied()
            .filter(|(channel, epoch, _)| *channel == handle.channel && *epoch == handle.epoch)
            .collect::<Vec<_>>();
        keys.into_iter()
            .filter_map(|key| guard.remove(&key))
            .collect::<Vec<_>>()
    };
    for cancellation in cancelled {
        cancellation.cancel();
    }
    Ok(())
}

async fn handle_control_request<H>(
    frame: Frame,
    egress: &mpsc::Sender<Frame>,
    handler: Arc<H>,
    dispatcher: RequestDispatcher,
    module_handle: ModuleHandle,
) -> Result<(), SubcModuleError>
where
    H: ModuleHandler,
{
    // A control request this SDK cannot decode (most likely a newer daemon's
    // field or operation) is refused on its own correlation id, and the
    // connection stays up. Returning the decode error would end the serve loop,
    // so one unknown field on one route.bind would stop the whole module, and
    // the next such bind after its respawn would stop it again.
    let request = match serde_json::from_slice::<ModuleControlRequest>(&frame.body) {
        Ok(request) => request,
        Err(error) => {
            let body = serde_json::to_vec(&ErrorBody::new(
                "invalid_request",
                format!("control request could not be decoded by this module: {error}"),
            ))
            .map_err(SubcModuleError::Json)?;
            let refusal = Frame::build_with_version(
                frame.header.ver,
                FrameType::Error,
                control_flags(),
                0,
                0,
                frame.header.corr,
                body,
            )
            .map_err(SubcModuleError::FrameBuild)?;
            send_outbound(egress, refusal).await?;
            return Ok(());
        }
    };
    match request {
        ModuleControlRequest::RouteBind {
            route_channel,
            epoch,
            target,
            identity,
            principal,
            consumer_capabilities,
            role_versions,
            admission_facts,
            scope,
        } => {
            // Implicit-replace rule (wire spec 3.3.0): the daemon never rebinds a live
            // channel, but its route-gone GOODBYE to modules is best-effort, so a bind
            // can arrive for a channel this endpoint still believes installed. A
            // strictly higher epoch proves the daemon freed the old binding: tear the
            // stale install down locally and proceed. Equal or lower epoch is a
            // protocol violation the daemon cannot produce: reject the bind.
            if let Some(stale) = module_handle.installed_route(route_channel)? {
                if epoch <= stale.epoch {
                    let body = serde_json::to_vec(&ErrorBody::new(
                        "route_rejected",
                        format!(
                            "route.bind epoch {epoch} does not supersede installed epoch {} on channel {route_channel}",
                            stale.epoch
                        ),
                    ))
                    .map_err(SubcModuleError::Json)?;
                    let reject = Frame::build_with_version(
                        frame.header.ver,
                        FrameType::Error,
                        control_flags(),
                        0,
                        0,
                        frame.header.corr,
                        body,
                    )
                    .map_err(SubcModuleError::FrameBuild)?;
                    send_outbound(egress, reject).await?;
                    return Ok(());
                }
                if module_handle.remove_route(stale)? {
                    cancel_handle(&dispatcher.in_flight, stale)?;
                    handler.on_route_gone(&stale).await;
                }
            }
            let handle = module_handle.route_handle(route_channel, epoch);
            let req = RouteBindRequest {
                handle,
                target,
                identity,
                principal,
                consumer_capabilities,
                role_versions,
                admission_facts,
                scope,
            };
            let decision = handler.on_bind(&req).await;
            match decision.kind {
                BindDecisionKind::Accept => {
                    let response = match serde_json::to_vec(&ModuleControlResponse::RouteBindAck {})
                        .map_err(SubcModuleError::Json)
                        .and_then(|body| {
                            Frame::build_with_version(
                                frame.header.ver,
                                FrameType::Response,
                                control_flags(),
                                0,
                                0,
                                frame.header.corr,
                                body,
                            )
                            .map_err(SubcModuleError::FrameBuild)
                        }) {
                        Ok(response) => response,
                        Err(err) => {
                            handler.on_route_gone(&handle).await;
                            return Err(err);
                        }
                    };
                    if let Err(err) = send_outbound(egress, response).await {
                        handler.on_route_gone(&handle).await;
                        return Err(err);
                    }
                    if let Err(err) = module_handle.install_route(handle) {
                        handler.on_route_gone(&handle).await;
                        return Err(err);
                    }
                    handler.on_bound(&handle).await;
                }
                BindDecisionKind::Reject { code, message } => {
                    let result = serde_json::to_vec(&ErrorBody::new(code, message))
                        .map_err(SubcModuleError::Json)
                        .and_then(|body| {
                            Frame::build_with_version(
                                frame.header.ver,
                                FrameType::Error,
                                control_flags(),
                                0,
                                0,
                                frame.header.corr,
                                body,
                            )
                            .map_err(SubcModuleError::FrameBuild)
                        });
                    let result = match result {
                        Ok(response) => send_outbound(egress, response).await,
                        Err(err) => Err(err),
                    };
                    handler.on_route_gone(&handle).await;
                    result?;
                }
            }
        }
        ModuleControlRequest::HealthCheck {} => {
            spawn_health_request(frame, egress.clone(), handler, dispatcher)?;
        }
    }
    Ok(())
}

async fn send_hello(
    egress: &mpsc::Sender<Frame>,
    mut manifest: ModuleManifest,
) -> Result<(), SubcModuleError> {
    let launch_nonce = match retained_launch_nonce() {
        Some(retained) => Some(retained),
        None => launch_nonce()
            .map_err(SubcModuleError::LaunchNonce)?
            .map(|nonce| nonce.value().to_string()),
    };
    stamp_launch_nonce_source(&mut manifest, launch_nonce.as_deref());
    let body = serde_json::to_vec(&ModuleHelloBody {
        manifest,
        protocol_ver: PROTOCOL_VERSION,
        control_ops: Some(vec![MODULE_CONTROL_OP_HEALTH_CHECK.to_string()]),
        launch_nonce,
    })
    .map_err(SubcModuleError::Json)?;
    let frame = Frame::build(FrameType::Hello, control_flags(), 0, 0, HELLO_CORR, body)
        .map_err(SubcModuleError::FrameBuild)?;
    send_outbound(egress, frame).await
}

async fn expect_hello_ack<R>(reader: &mut R) -> Result<ModuleHelloAckBody, SubcModuleError>
where
    R: AsyncRead + Unpin,
{
    let Some(frame) = read_frame(reader).await.map_err(SubcModuleError::FrameIo)? else {
        return Err(SubcModuleError::ConnectionClosedBeforeHelloAck);
    };
    match frame.header.ty {
        FrameType::HelloAck => serde_json::from_slice(&frame.body).map_err(SubcModuleError::Json),
        FrameType::Error => {
            let body =
                serde_json::from_slice::<ErrorBody>(&frame.body).map_err(SubcModuleError::Json)?;
            Err(SubcModuleError::HelloRejected { body })
        }
        ty => Err(SubcModuleError::UnexpectedHelloAck { ty }),
    }
}

async fn connect_to_subc(connection_file_path: &Path) -> Result<TcpStream, SubcModuleError> {
    let conn = connection_file::read_for_client(connection_file_path).map_err(|source| {
        SubcModuleError::ConnectionFile {
            path: connection_file_path.to_path_buf(),
            source,
        }
    })?;
    let endpoint = conn
        .endpoints
        .first()
        .ok_or_else(|| SubcModuleError::NoEndpoint {
            path: connection_file_path.to_path_buf(),
        })?;
    let endpoint_label = format!("{}:{}", endpoint.host, endpoint.port);
    let mut stream = TcpStream::connect(&endpoint_label)
        .await
        .map_err(|source| SubcModuleError::Connect {
            path: connection_file_path.to_path_buf(),
            endpoint: endpoint_label.clone(),
            source,
        })?;
    // This socket carries the module's replies back to the daemon, so Nagle here
    // delays every response rather than every request -- the same cost on the
    // return leg. Both ends of the hop have to disable it for either to help.
    //
    // The result is deliberately dropped rather than logged: this crate takes no
    // logging dependency, and the only ways setting a socket option on a
    // just-connected stream fail leave the socket unusable, which the handshake on
    // the very next line reports as a typed Auth error. Swallowing it here would
    // hide nothing that stays hidden.
    let _ = stream.set_nodelay(true);
    authenticate_client(&mut stream, &conn, AUTH_DEADLINE)
        .await
        .map_err(|source| SubcModuleError::Auth {
            path: connection_file_path.to_path_buf(),
            endpoint: endpoint_label,
            source,
        })?;
    Ok(stream)
}

async fn drain_writer<W>(write_half: W, mut rx: mpsc::Receiver<Frame>) -> Result<(), FrameIoError>
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

async fn send_outbound(egress: &mpsc::Sender<Frame>, frame: Frame) -> Result<(), SubcModuleError> {
    egress
        .send(frame)
        .await
        .map_err(|_| SubcModuleError::WriterClosed)
}

fn parse_subc_arg(args: impl IntoIterator<Item = OsString>) -> Result<PathBuf, SubcModuleError> {
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == "--subc" {
            let value = args.next().ok_or(SubcModuleError::MissingSubcValue)?;
            return Ok(PathBuf::from(value));
        }
        if let Some(raw) = arg.to_str().and_then(|arg| arg.strip_prefix("--subc=")) {
            if raw.is_empty() {
                return Err(SubcModuleError::MissingSubcValue);
            }
            return Ok(PathBuf::from(raw));
        }
    }
    Err(SubcModuleError::MissingSubcArg)
}

fn module_id_from_env() -> Result<Option<String>, SubcModuleError> {
    match env::var(SUBC_MODULE_ID_ENV) {
        Ok(value) if !value.trim().is_empty() => Ok(Some(value)),
        Ok(_) => Err(SubcModuleError::EmptyModuleIdEnv),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(env::VarError::NotUnicode(value)) => {
            Err(SubcModuleError::NonUnicodeModuleIdEnv { value })
        }
    }
}

fn lock_in_flight(
    in_flight: &InFlight,
) -> Result<std::sync::MutexGuard<'_, HashMap<RequestKey, CancellationToken>>, SubcModuleError> {
    in_flight
        .lock()
        .map_err(|_| SubcModuleError::InFlightPoisoned)
}

fn control_flags() -> Flags {
    Flags::new(false, Priority::Passive, false)
}

fn data_flags() -> Flags {
    Flags::new(false, Priority::Interactive, false)
}

#[derive(Debug)]
pub enum SubcModuleError {
    MissingSubcArg,
    MissingSubcValue,
    EmptyModuleIdEnv,
    NonUnicodeModuleIdEnv {
        value: OsString,
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
    FrameIo(FrameIoError),
    FrameBuild(FrameBuildError),
    Json(serde_json::Error),
    WriterClosed,
    StaleRouteHandle(RouteHandle),
    ConnectionTokenExhausted,
    WriterTask(tokio::task::JoinError),
    InFlightPoisoned,
    ConnectionClosedBeforeHelloAck,
    UnexpectedHelloAck {
        ty: FrameType,
    },
    HelloRejected {
        body: ErrorBody,
    },
    /// The descriptor named for the launch nonce could not be read. HELLO is
    /// not sent without it, and the environment copy is not tried instead.
    LaunchNonce(launch_nonce::LaunchNonceError),
}

impl fmt::Display for SubcModuleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingSubcArg => write!(f, "missing required --subc <connection-file> argument"),
            Self::MissingSubcValue => write!(f, "--subc requires a connection-file path value"),
            Self::EmptyModuleIdEnv => write!(f, "{SUBC_MODULE_ID_ENV} must not be empty when set"),
            Self::NonUnicodeModuleIdEnv { value } => write!(
                f,
                "{SUBC_MODULE_ID_ENV} must be valid UTF-8, got '{}'",
                value.to_string_lossy()
            ),
            Self::ConnectionFile { path, source } => write!(
                f,
                "failed to read subc connection file '{}': {source}",
                path.display()
            ),
            Self::NoEndpoint { path } => write!(
                f,
                "subc connection file '{}' has no endpoints",
                path.display()
            ),
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
            Self::FrameIo(err) => write!(f, "frame I/O error: {err}"),
            Self::FrameBuild(err) => write!(f, "frame build error: {err}"),
            Self::Json(err) => write!(f, "JSON error: {err}"),
            Self::WriterClosed => write!(f, "module writer task closed"),
            Self::StaleRouteHandle(handle) => write!(f, "stale route handle: {handle:?}"),
            Self::ConnectionTokenExhausted => write!(f, "module connection token exhausted"),
            Self::WriterTask(err) => write!(f, "module writer task failed: {err}"),
            Self::InFlightPoisoned => write!(f, "in-flight registry lock poisoned"),
            Self::ConnectionClosedBeforeHelloAck => write!(f, "connection closed before HELLO_ACK"),
            Self::UnexpectedHelloAck { ty } => write!(f, "expected HELLO_ACK, got {ty:?}"),
            Self::HelloRejected { body } => write!(
                f,
                "HELLO rejected by subc: {} ({})",
                body.code, body.message
            ),
            Self::LaunchNonce(err) => write!(f, "launch nonce unavailable: {err}"),
        }
    }
}

impl Error for SubcModuleError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ConnectionFile { source, .. } => Some(source),
            Self::Connect { source, .. } => Some(source),
            Self::Auth { source, .. } => Some(source),
            Self::FrameIo(err) => Some(err),
            Self::FrameBuild(err) => Some(err),
            Self::Json(err) => Some(err),
            Self::LaunchNonce(err) => Some(err),
            Self::WriterTask(err) => Some(err),
            Self::MissingSubcArg
            | Self::MissingSubcValue
            | Self::EmptyModuleIdEnv
            | Self::NonUnicodeModuleIdEnv { .. }
            | Self::NoEndpoint { .. }
            | Self::WriterClosed
            | Self::StaleRouteHandle(_)
            | Self::ConnectionTokenExhausted
            | Self::InFlightPoisoned
            | Self::ConnectionClosedBeforeHelloAck
            | Self::UnexpectedHelloAck { .. }
            | Self::HelloRejected { .. } => None,
        }
    }
}

impl From<serde_json::Error> for SubcModuleError {
    fn from(err: serde_json::Error) -> Self {
        Self::Json(err)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde_json::json;

    use subc_protocol::manifest::{Concurrency, ExecutionMode, IdentityScope, Tool};
    use tokio::time::{advance, timeout};

    use super::*;

    struct EchoHandler;

    #[async_trait]
    impl ModuleHandler for EchoHandler {
        async fn handle(&self, _ctx: RequestCtx, body: Vec<u8>) -> HandlerOutcome {
            HandlerOutcome::Response(body)
        }
    }

    struct BlockingHandler {
        entered: Arc<AtomicUsize>,
        release: Semaphore,
        report: HealthReport,
    }

    #[async_trait]
    impl ModuleHandler for BlockingHandler {
        async fn handle(&self, _ctx: RequestCtx, _body: Vec<u8>) -> HandlerOutcome {
            self.entered.fetch_add(1, Ordering::SeqCst);
            self.release.acquire().await.unwrap().forget();
            HandlerOutcome::Streamed
        }

        async fn health(&self) -> HealthReport {
            self.report.clone()
        }
    }

    fn health_request(corr: u64) -> Frame {
        Frame::build(
            FrameType::Request,
            control_flags(),
            0,
            0,
            corr,
            serde_json::to_vec(&ModuleControlRequest::HealthCheck {}).unwrap(),
        )
        .unwrap()
    }

    fn data_request(channel: u16, corr: u64) -> Frame {
        Frame::build(
            FrameType::Request,
            data_flags(),
            channel,
            1,
            corr,
            b"opaque".to_vec(),
        )
        .unwrap()
    }

    fn catalog_update_response(corr: u64) -> Frame {
        Frame::build(
            FrameType::Response,
            control_flags(),
            0,
            0,
            corr,
            serde_json::to_vec(&ModuleControlResponseToModule::CatalogUpdate {}).unwrap(),
        )
        .unwrap()
    }

    fn test_module_handle(subc_ops: &[&str]) -> (ModuleHandle, mpsc::Receiver<Frame>) {
        let (tx, rx) = mpsc::channel(4);
        let ack = ModuleHelloAckBody {
            negotiated_ver: PROTOCOL_VERSION,
            subc_ops: subc_ops.iter().map(|op| (*op).to_string()).collect(),
            subc_capabilities: Vec::new(),
            storage: None,
            machine_id: None,
        };
        (
            ModuleHandle::new(
                &ack,
                tx,
                1,
                CancellationToken::new(),
                tokio::runtime::Handle::current(),
            ),
            rx,
        )
    }

    #[test]
    fn module_handle_exposes_the_acked_machine_id_and_none_otherwise() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let handle_for = |machine_id: Option<&str>| {
            let (tx, _rx) = mpsc::channel(1);
            let ack = ModuleHelloAckBody {
                negotiated_ver: PROTOCOL_VERSION,
                subc_ops: Vec::new(),
                subc_capabilities: Vec::new(),
                storage: None,
                machine_id: machine_id.map(str::to_owned),
            };
            ModuleHandle::new(
                &ack,
                tx,
                1,
                CancellationToken::new(),
                runtime.handle().clone(),
            )
        };
        assert_eq!(
            handle_for(Some("0123456789abcdef0123456789abcdef"))
                .machine_id()
                .map(MachineId::as_str),
            Some("0123456789abcdef0123456789abcdef")
        );
        // An older daemon sends nothing, and nothing is invented in its place.
        assert_eq!(handle_for(None).machine_id(), None);
        assert_eq!(handle_for(Some("not-a-machine-id")).machine_id(), None);
    }

    fn test_provider_role(tool_names: &[&str]) -> ProviderRole {
        ProviderRole::ToolProvider {
            tools: tool_names
                .iter()
                .map(|name| Tool {
                    name: (*name).to_string(),
                    description: None,
                    execution_mode: ExecutionMode::Pure,
                    schema: json!({"type": "object"}),
                })
                .collect(),
            identity_scope: vec![IdentityScope::Project],
            concurrency: Concurrency::ModuleManaged,
            emits_push: false,
            sub_supervises: false,
        }
    }

    #[tokio::test]
    async fn catalog_update_fails_fast_when_hello_ack_does_not_advertise_support() {
        let (handle, mut rx) = test_module_handle(&[]);

        let error = handle
            .catalog_update(vec![test_provider_role(&["a"])])
            .await
            .unwrap_err();
        assert!(matches!(error, CatalogUpdateError::NotSupported));
        assert!(timeout(Duration::from_millis(75), rx.recv()).await.is_err());
    }

    #[tokio::test]
    async fn catalog_update_demuxes_multiple_in_flight_requests() {
        let (handle, mut rx) = test_module_handle(&[MODULE_TO_SUBC_OP_CATALOG_UPDATE]);
        let first_handle = handle.clone();
        let second_handle = handle.clone();
        let first = tokio::spawn(async move {
            first_handle
                .catalog_update(vec![test_provider_role(&["a"])])
                .await
        });
        let second = tokio::spawn(async move {
            second_handle
                .catalog_update(vec![test_provider_role(&["b"])])
                .await
        });

        let first_frame = timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        let second_frame = timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first_frame.header.ty, FrameType::Request);
        assert_eq!(first_frame.header.channel, 0);
        assert_eq!(second_frame.header.ty, FrameType::Request);
        assert_eq!(second_frame.header.channel, 0);
        assert_ne!(first_frame.header.corr, second_frame.header.corr);

        assert!(handle.handle_control_reply(catalog_update_response(second_frame.header.corr)));
        assert!(handle.handle_control_reply(catalog_update_response(first_frame.header.corr)));
        assert!(first.await.unwrap().is_ok());
        assert!(second.await.unwrap().is_ok());
    }

    fn control_reply(ty: FrameType, corr: u64, body: Vec<u8>) -> Frame {
        Frame::build(ty, control_flags(), 0, 0, corr, body).unwrap()
    }

    fn owner() -> Principal {
        Principal::Reserved {
            module_id: "owner".to_string(),
        }
    }

    /// Each scope op is gated on its own `subc_ops` entry: a daemon that
    /// lists one but not the other gets only the one it listed, and a gated
    /// call sends nothing.
    #[tokio::test]
    async fn scope_ops_fail_fast_when_hello_ack_does_not_advertise_them() {
        let (handle, mut rx) = test_module_handle(&[]);
        assert_eq!(
            handle.scope_sync(1, Vec::new()).await.unwrap_err(),
            ScopeCallError::NotSupported { op: SCOPE_SYNC_OP }
        );
        assert_eq!(
            handle
                .scope_describe(owner(), "s".to_string())
                .await
                .unwrap_err(),
            ScopeCallError::NotSupported {
                op: SCOPE_DESCRIBE_OP
            }
        );
        assert!(timeout(Duration::from_millis(75), rx.recv()).await.is_err());

        let (describe_only, mut rx) = test_module_handle(&[SCOPE_DESCRIBE_OP]);
        assert_eq!(
            describe_only.scope_sync(1, Vec::new()).await.unwrap_err(),
            ScopeCallError::NotSupported { op: SCOPE_SYNC_OP }
        );
        assert!(timeout(Duration::from_millis(75), rx.recv()).await.is_err());

        let (sync_only, mut rx) = test_module_handle(&[SCOPE_SYNC_OP]);
        assert_eq!(
            sync_only
                .scope_describe(owner(), "s".to_string())
                .await
                .unwrap_err(),
            ScopeCallError::NotSupported {
                op: SCOPE_DESCRIBE_OP
            }
        );
        assert!(timeout(Duration::from_millis(75), rx.recv()).await.is_err());
    }

    /// A reply carrying another op's body is not taken as an answer.
    #[tokio::test]
    async fn a_scope_reply_of_another_op_is_a_protocol_error() {
        let (handle, mut rx) = test_module_handle(&[SCOPE_SYNC_OP, SCOPE_DESCRIBE_OP]);
        let sync = tokio::spawn({
            let handle = handle.clone();
            async move { handle.scope_sync(1, Vec::new()).await }
        });
        let request = timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        let describe_body = serde_json::to_vec(&ModuleControlResponseToModule::ScopeDescribe {
            status: ScopeStatus::NotLive,
            scope_epoch: None,
            daemon_incarnation: "i".to_string(),
            owner_synced: false,
            owner_configured: false,
            scope: None,
        })
        .unwrap();
        assert!(handle.handle_control_reply(control_reply(
            FrameType::Response,
            request.header.corr,
            describe_body
        )));
        assert!(matches!(
            sync.await.unwrap(),
            Err(ScopeCallError::Protocol(_))
        ));

        let describe = tokio::spawn({
            let handle = handle.clone();
            async move { handle.scope_describe(owner(), "s".to_string()).await }
        });
        let request = timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        let sync_body = serde_json::to_vec(&ModuleControlResponseToModule::ScopeSync {
            generation: 1,
            results: Vec::new(),
            ended: Vec::new(),
        })
        .unwrap();
        assert!(handle.handle_control_reply(control_reply(
            FrameType::Response,
            request.header.corr,
            sync_body
        )));
        assert!(matches!(
            describe.await.unwrap(),
            Err(ScopeCallError::Protocol(_))
        ));
    }

    /// Every Error-frame code reaches the caller as sent, including codes the
    /// reply handling shared with `catalog_update` sorts into its own
    /// `CatalogUpdateError` variants (`not_registered`).
    #[tokio::test]
    async fn a_scope_error_frame_keeps_the_daemons_code_and_message() {
        let (handle, mut rx) = test_module_handle(&[SCOPE_SYNC_OP]);
        for code in [
            subc_protocol::error_codes::SCOPE_SYNC_STALE,
            subc_protocol::error_codes::SCOPE_SYNC_NOT_AUTHORITY,
            "not_registered",
        ] {
            let sync = tokio::spawn({
                let handle = handle.clone();
                async move { handle.scope_sync(1, Vec::new()).await }
            });
            let request = timeout(Duration::from_secs(1), rx.recv())
                .await
                .unwrap()
                .unwrap();
            let body = serde_json::to_vec(&ErrorBody::new(code, "why")).unwrap();
            assert!(handle.handle_control_reply(control_reply(
                FrameType::Error,
                request.header.corr,
                body
            )));
            let error = sync.await.unwrap().unwrap_err();
            assert_eq!(
                error,
                ScopeCallError::Refused {
                    code: code.to_string(),
                    message: "why".to_string()
                }
            );
            assert_eq!(error.code(), Some(code));
        }
    }

    /// A route.bind whose scope stamp carries a field this SDK does not know
    /// (a newer daemon's attribute) must be refused on its own correlation id,
    /// and the module must keep serving: the next control request is answered.
    #[tokio::test]
    async fn an_undecodable_control_request_is_refused_and_the_module_keeps_serving() {
        let (tx, mut rx) = mpsc::channel(4);
        let handler = Arc::new(EchoHandler);
        let dispatcher = RequestDispatcher::new();
        let (module_handle, _unused_rx) = test_module_handle(&[]);
        // A well-formed bind from this SDK's own types, then one scope
        // attribute this SDK does not know, so that is the only defect.
        let mut body: serde_json::Value =
            serde_json::from_slice(&route_bind_frame(9, 1, 41).body).unwrap();
        body["scope"] = serde_json::json!({
            "owner": { "kind": "reserved", "module_id": "prefrontal-core" },
            "ref": "s",
            "scope_epoch": 1,
            "kind": "head",
            "attributes": { "a_field_from_a_newer_daemon": "x" },
            "owner_authorized": true
        });
        let body = serde_json::to_vec(&body).unwrap();
        let bind = Frame::build(FrameType::Request, control_flags(), 0, 0, 41, body).unwrap();

        let kept_serving = handle_frame(
            bind,
            &tx,
            Arc::clone(&handler),
            dispatcher.clone(),
            module_handle.clone(),
        )
        .await
        .expect("an undecodable control request must not end the module");
        assert!(kept_serving);
        let refusal = timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(refusal.header.ty, FrameType::Error);
        assert_eq!(refusal.header.corr, 41);
        let error: ErrorBody = serde_json::from_slice(&refusal.body).unwrap();
        assert_eq!(error.code, "invalid_request");
        // Prove the refusal is about the unknown attribute, not some other
        // defect in this fixture.
        assert!(
            error.message.contains("a_field_from_a_newer_daemon"),
            "{}",
            error.message
        );

        assert!(
            handle_frame(health_request(42), &tx, handler, dispatcher, module_handle)
                .await
                .unwrap()
        );
        let health = timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(health.header.ty, FrameType::Response);
        assert_eq!(health.header.corr, 42);
    }

    #[tokio::test]
    async fn default_health_check_answers_ok() {
        let (tx, mut rx) = mpsc::channel(4);
        let handler = Arc::new(EchoHandler);
        let dispatcher = RequestDispatcher::new();
        let (module_handle, _unused_rx) = test_module_handle(&[]);

        assert!(
            handle_frame(health_request(77), &tx, handler, dispatcher, module_handle)
                .await
                .unwrap()
        );

        let response = timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(response.header.ty, FrameType::Response);
        assert_eq!(response.header.channel, 0);
        assert_eq!(response.header.corr, 77);

        // Assert the PROPERTIES that matter rather than byte-equality with
        // HealthReport::ok(). Comparing against the constructor made this test a
        // restatement of the implementation: it reddened on any change to the
        // default without saying which property had broken.
        let parsed = serde_json::from_slice::<ModuleControlResponse>(&response.body).unwrap();
        let ModuleControlResponse::HealthCheck { status, detail, .. } = parsed else {
            panic!("expected a health.check response");
        };
        // A module that never implemented health is not UNHEALTHY -- the daemon
        // must not escalate on it.
        assert_eq!(status, HealthStatus::Ok);
        // ...but the report must SAY that nobody measured, so an operator can
        // tell it from a real all-clear. Substring rather than exact text: the
        // wording is for humans and nothing parses it.
        assert!(
            detail
                .as_deref()
                .is_some_and(|d| d.contains("no health implementation")),
            "the inherited default must identify itself, got {detail:?}"
        );
    }

    struct DispatchHarness {
        tx: mpsc::Sender<Frame>,
        rx: mpsc::Receiver<Frame>,
        handler: Arc<BlockingHandler>,
        dispatcher: RequestDispatcher,
        module_handle: ModuleHandle,
    }

    impl DispatchHarness {
        fn new(report: HealthReport) -> Self {
            let (tx, rx) = mpsc::channel(HANDLER_TASK_CAPACITY + 4);
            let (module_handle, _unused_rx) = test_module_handle(&[]);
            module_handle
                .install_route(RouteHandle::new(7, 1, 1))
                .unwrap();
            Self {
                tx,
                rx,
                handler: Arc::new(BlockingHandler {
                    entered: Arc::new(AtomicUsize::new(0)),
                    release: Semaphore::new(0),
                    report,
                }),
                dispatcher: RequestDispatcher::new(),
                module_handle,
            }
        }

        async fn request(&self, corr: u64) {
            handle_frame(
                data_request(7, corr),
                &self.tx,
                Arc::clone(&self.handler),
                self.dispatcher.clone(),
                self.module_handle.clone(),
            )
            .await
            .unwrap();
        }

        async fn wait_for_entered(&self, count: usize) {
            timeout(Duration::from_secs(1), async {
                while self.handler.entered.load(Ordering::SeqCst) != count {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("data handlers should acquire the available slots");
        }

        async fn saturate(&self) {
            for corr in 1..=HANDLER_TASK_CAPACITY as u64 {
                self.request(corr).await;
            }
            self.wait_for_entered(HANDLER_TASK_CAPACITY).await;
            self.request(HANDLER_TASK_CAPACITY as u64 + 1).await;
            tokio::task::yield_now().await;
            assert_eq!(self.dispatcher.permits.available_permits(), 0);
            assert_eq!(self.dispatcher.waiting.lock().unwrap().len(), 1);
            assert_eq!(
                self.handler.entered.load(Ordering::SeqCst),
                HANDLER_TASK_CAPACITY
            );
        }

        async fn health(&mut self, corr: u64) -> HealthReport {
            handle_frame(
                health_request(corr),
                &self.tx,
                Arc::clone(&self.handler),
                self.dispatcher.clone(),
                self.module_handle.clone(),
            )
            .await
            .unwrap();
            let frame = timeout(Duration::from_millis(100), self.rx.recv())
                .await
                .expect("health.check must answer within 100 ms while data slots are busy")
                .unwrap();
            assert_eq!(frame.header.ty, FrameType::Response);
            assert_eq!(frame.header.channel, 0);
            assert_eq!(frame.header.corr, corr);
            serde_json::from_slice::<ModuleControlResponse>(&frame.body)
                .unwrap()
                .health_report()
                .unwrap()
        }

        async fn release(&mut self, count: usize) {
            self.handler.release.add_permits(count);
            for _ in 0..count {
                let frame = timeout(Duration::from_secs(1), self.rx.recv())
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(frame.header.ty, FrameType::StreamEnd);
                assert_eq!(frame.header.channel, 7);
            }
            assert!(self.dispatcher.waiting.lock().unwrap().is_empty());
            assert!(self.dispatcher.in_flight.lock().unwrap().is_empty());
            assert_eq!(
                self.dispatcher.permits.available_permits(),
                HANDLER_TASK_CAPACITY
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn health_check_answers_degraded_under_saturation_and_recovers() {
        let own_report = HealthReport {
            status: HealthStatus::Ok,
            detail: Some("module gauges healthy".to_string()),
            metrics: Some(json!({"transforms": 64})),
        };
        let mut harness = DispatchHarness::new(own_report.clone());
        harness.saturate().await;
        // A brief burst is not dispatch impairment, even with no slot free.
        assert_eq!(harness.health(899).await, own_report);
        // Advance the real dispatch stamp's monotonic clock, not a test-only
        // health gauge. Keeping handlers blocked proves the transport isolation.
        advance(Duration::from_millis(7300)).await;
        let report = harness.health(900).await;
        assert_eq!(report.status, HealthStatus::Degraded);
        assert_eq!(report.metrics, own_report.metrics);
        assert_eq!(
            report.detail.as_deref(),
            Some("request dispatch saturated: 64/64 in use, oldest waiting 7.3 s; module gauges healthy")
        );

        harness.release(HANDLER_TASK_CAPACITY + 1).await;
        assert_eq!(harness.health(901).await, own_report);
    }

    #[tokio::test(start_paused = true)]
    async fn health_check_preserves_failing_under_saturation() {
        let mut harness = DispatchHarness::new(HealthReport {
            status: HealthStatus::Failing,
            detail: Some("module dispatch heartbeat stale".to_string()),
            metrics: Some(json!({"heartbeat_age_s": 12})),
        });
        harness.saturate().await;
        advance(Duration::from_millis(7300)).await;
        let report = harness.health(900).await;
        assert_eq!(report.status, HealthStatus::Failing);
        assert_eq!(report.metrics, Some(json!({"heartbeat_age_s": 12})));
        assert_eq!(
            report.detail.as_deref(),
            Some("request dispatch saturated: 64/64 in use, oldest waiting 7.3 s; module dispatch heartbeat stale")
        );
        harness.release(HANDLER_TASK_CAPACITY + 1).await;
    }

    #[tokio::test(start_paused = true)]
    async fn health_check_passes_module_report_through_with_free_permits() {
        for status in [
            HealthStatus::Ok,
            HealthStatus::Degraded,
            HealthStatus::Failing,
        ] {
            let own_report = HealthReport {
                status,
                detail: Some("module's own assessment".to_string()),
                metrics: Some(json!({"gauge": 42})),
            };
            let mut harness = DispatchHarness::new(own_report.clone());
            harness.request(1).await;
            harness.wait_for_entered(1).await;
            advance(Duration::from_millis(7300)).await;
            assert_eq!(harness.health(900).await, own_report);
            harness.release(1).await;
        }
    }

    struct CorrBlockingHandler {
        entered: Arc<Mutex<Vec<u64>>>,
        release_first: Arc<Semaphore>,
    }

    #[async_trait]
    impl ModuleHandler for CorrBlockingHandler {
        async fn handle(&self, ctx: RequestCtx, _body: Vec<u8>) -> HandlerOutcome {
            let corr = ctx.corr();
            self.entered.lock().unwrap().push(corr);
            if corr == 1 {
                self.release_first.acquire().await.unwrap().forget();
            }
            HandlerOutcome::Response(Vec::new())
        }
    }

    #[tokio::test]
    async fn cancelled_capacity_queued_data_request_emits_terminal_and_skips_handler() {
        let (tx, mut rx) = mpsc::channel(4);
        let entered = Arc::new(Mutex::new(Vec::new()));
        let release_first = Arc::new(Semaphore::new(0));
        let handler = Arc::new(CorrBlockingHandler {
            entered: Arc::clone(&entered),
            release_first: Arc::clone(&release_first),
        });
        let dispatcher = RequestDispatcher {
            permits: Arc::new(Semaphore::new(1)),
            ..RequestDispatcher::new()
        };
        let (module_handle, _unused_rx) = test_module_handle(&[]);
        module_handle
            .install_route(RouteHandle::new(7, 1, 1))
            .unwrap();

        handle_frame(
            data_request(7, 1),
            &tx,
            Arc::clone(&handler),
            dispatcher.clone(),
            module_handle.clone(),
        )
        .await
        .unwrap();
        timeout(Duration::from_secs(1), async {
            while entered.lock().unwrap().as_slice() != [1] {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        handle_frame(
            data_request(7, 2),
            &tx,
            Arc::clone(&handler),
            dispatcher.clone(),
            module_handle.clone(),
        )
        .await
        .unwrap();
        timeout(Duration::from_secs(1), async {
            while !dispatcher
                .in_flight
                .lock()
                .unwrap()
                .contains_key(&(7, 1, 2))
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        handle_frame(
            Frame::build(FrameType::Cancel, data_flags(), 7, 1, 2, Vec::new()).unwrap(),
            &tx,
            Arc::clone(&handler),
            dispatcher.clone(),
            module_handle,
        )
        .await
        .unwrap();
        assert!(
            dispatcher
                .in_flight
                .lock()
                .unwrap()
                .get(&(7, 1, 2))
                .unwrap()
                .is_cancelled(),
            "cancel must land while the second request waits for handler capacity"
        );

        release_first.add_permits(1);
        timeout(Duration::from_secs(1), async {
            while !dispatcher.in_flight.lock().unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        assert_eq!(*entered.lock().unwrap(), vec![1]);
        assert!(dispatcher.waiting.lock().unwrap().is_empty());
        let response = timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(response.header.ty, FrameType::Response);
        assert_eq!(response.header.corr, 1);

        let cancelled = timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(cancelled.header.ty, FrameType::Error);
        assert_eq!(cancelled.header.channel, 7);
        assert_eq!(cancelled.header.epoch, 1);
        assert_eq!(cancelled.header.corr, 2);
        assert_eq!(
            serde_json::from_slice::<ErrorBody>(&cancelled.body).unwrap(),
            ErrorBody {
                code: "cancelled".to_string(),
                message: "request cancelled".to_string(),
                detail: None,
            }
        );
        assert!(timeout(Duration::from_millis(50), rx.recv()).await.is_err());
    }

    #[tokio::test]
    async fn cancelled_terminal_is_not_sent_after_route_teardown() {
        let (tx, mut rx) = mpsc::channel(1);
        let (module_handle, _unused_rx) = test_module_handle(&[]);
        let handle = RouteHandle::new(7, 1, 1);
        module_handle.install_route(handle).unwrap();
        let ctx = RequestCtx {
            handle,
            corr: 2,
            ver: PROTOCOL_VERSION,
            egress: tx,
            module_handle: module_handle.clone(),
            cancelled: CancellationToken::new(),
        };
        assert!(module_handle.remove_route(handle).unwrap());

        let result = send_handler_outcome(
            &ctx,
            HandlerOutcome::Error {
                code: "cancelled".to_string(),
                message: "request cancelled".to_string(),
            },
        )
        .await;

        assert!(matches!(
            result,
            Err(SubcModuleError::StaleRouteHandle(stale)) if stale == handle
        ));
        assert!(rx.try_recv().is_err());
    }

    struct CountingHandler {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl ModuleHandler for CountingHandler {
        async fn handle(&self, _ctx: RequestCtx, _body: Vec<u8>) -> HandlerOutcome {
            self.calls.fetch_add(1, Ordering::SeqCst);
            HandlerOutcome::Response(Vec::new())
        }
    }

    #[tokio::test]
    async fn endpoint_validation_drops_stale_request_before_handler_dispatch() {
        let (tx, mut rx) = mpsc::channel(4);
        let calls = Arc::new(AtomicUsize::new(0));
        let handler = Arc::new(CountingHandler {
            calls: Arc::clone(&calls),
        });
        let dispatcher = RequestDispatcher::new();
        let (module_handle, _unused_rx) = test_module_handle(&[]);
        module_handle
            .install_route(RouteHandle::new(7, 2, 1))
            .unwrap();
        let stale = Frame::build(FrameType::Request, data_flags(), 7, 1, 55, Vec::new()).unwrap();

        assert!(
            handle_frame(stale, &tx, handler, dispatcher, module_handle.clone())
                .await
                .unwrap()
        );
        tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(module_handle.dropped_route_frames(), 1);
        assert!(rx.try_recv().is_err());
    }

    struct BindOrderingHandler {
        module_handle: ModuleHandle,
        bind_emit_rejected: Arc<AtomicUsize>,
        bound: Arc<AtomicUsize>,
        cleanup: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl ModuleHandler for BindOrderingHandler {
        async fn handle(&self, _ctx: RequestCtx, _body: Vec<u8>) -> HandlerOutcome {
            HandlerOutcome::Response(Vec::new())
        }

        async fn on_bind(&self, req: &RouteBindRequest) -> BindDecision {
            if matches!(
                self.module_handle
                    .push(&req.handle, b"too-early".to_vec(), None)
                    .await,
                Err(SubcModuleError::StaleRouteHandle(_))
            ) {
                self.bind_emit_rejected.fetch_add(1, Ordering::SeqCst);
            }
            BindDecision::accept()
        }

        async fn on_bound(&self, handle: &RouteHandle) {
            self.bound.fetch_add(1, Ordering::SeqCst);
            self.module_handle
                .push(handle, b"bound".to_vec(), Some(AdmissionClass::Expedite))
                .await
                .unwrap();
        }

        async fn on_route_gone(&self, _handle: &RouteHandle) {
            self.cleanup.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn route_bind_frame(channel: u16, epoch: u32, corr: u64) -> Frame {
        let body = serde_json::to_vec(&ModuleControlRequest::RouteBind {
            route_channel: channel,
            epoch,
            target: RouteTarget::ToolProvider {
                module_id: "provider".to_string(),
            },
            identity: BindIdentity::new(
                PathBuf::from("/tmp/project"),
                "test".to_string(),
                "bind".to_string(),
            ),
            principal: None,
            consumer_capabilities: None,
            role_versions: None,
            admission_facts: None,
            scope: None,
        })
        .unwrap();
        Frame::build(FrameType::Request, control_flags(), 0, 0, corr, body).unwrap()
    }

    #[tokio::test]
    async fn on_bound_runs_only_after_ack_queue_and_handle_install() {
        let (module_handle, mut rx) = test_module_handle(&[]);
        let tx = module_handle.shared.lock_inner().writer.clone().unwrap();
        let bind_emit_rejected = Arc::new(AtomicUsize::new(0));
        let bound = Arc::new(AtomicUsize::new(0));
        let cleanup = Arc::new(AtomicUsize::new(0));
        let handler = Arc::new(BindOrderingHandler {
            module_handle: module_handle.clone(),
            bind_emit_rejected: Arc::clone(&bind_emit_rejected),
            bound: Arc::clone(&bound),
            cleanup: Arc::clone(&cleanup),
        });

        assert!(handle_frame(
            route_bind_frame(8, 4, 90),
            &tx,
            handler,
            RequestDispatcher::new(),
            module_handle.clone(),
        )
        .await
        .unwrap());
        assert_eq!(bind_emit_rejected.load(Ordering::SeqCst), 1);
        assert_eq!(bound.load(Ordering::SeqCst), 1);
        assert_eq!(cleanup.load(Ordering::SeqCst), 0);

        let ack = rx.recv().await.unwrap();
        let push = rx.recv().await.unwrap();
        assert_eq!(ack.header.ty, FrameType::Response);
        assert_eq!(ack.header.channel, 0);
        assert_eq!(push.header.ty, FrameType::Push);
        assert_eq!((push.header.channel, push.header.epoch), (8, 4));
        assert_eq!(
            push.header.flags.admission_class(),
            Some(AdmissionClass::Expedite)
        );

        let captured = RouteHandle::new(8, 4, 1);
        assert!(module_handle.remove_route(captured).unwrap());
        let stale = module_handle
            .push(&captured, Vec::new(), None)
            .await
            .unwrap_err();
        assert!(matches!(stale, SubcModuleError::StaleRouteHandle(_)));
        assert!(rx.try_recv().is_err());
    }

    struct RejectingHandler {
        bound: Arc<AtomicUsize>,
        cleanup: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl ModuleHandler for RejectingHandler {
        async fn handle(&self, _ctx: RequestCtx, _body: Vec<u8>) -> HandlerOutcome {
            HandlerOutcome::Response(Vec::new())
        }

        async fn on_bind(&self, _req: &RouteBindRequest) -> BindDecision {
            BindDecision::reject("no", "rejected")
        }

        async fn on_bound(&self, _handle: &RouteHandle) {
            self.bound.fetch_add(1, Ordering::SeqCst);
        }

        async fn on_route_gone(&self, _handle: &RouteHandle) {
            self.cleanup.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn rejected_bind_cleans_up_without_installing_or_calling_on_bound() {
        let (module_handle, mut rx) = test_module_handle(&[]);
        let tx = module_handle.shared.lock_inner().writer.clone().unwrap();
        let bound = Arc::new(AtomicUsize::new(0));
        let cleanup = Arc::new(AtomicUsize::new(0));
        let handler = Arc::new(RejectingHandler {
            bound: Arc::clone(&bound),
            cleanup: Arc::clone(&cleanup),
        });
        handle_frame(
            route_bind_frame(6, 3, 91),
            &tx,
            handler,
            RequestDispatcher::new(),
            module_handle.clone(),
        )
        .await
        .unwrap();
        assert_eq!(rx.recv().await.unwrap().header.ty, FrameType::Error);
        assert_eq!(bound.load(Ordering::SeqCst), 0);
        assert_eq!(cleanup.load(Ordering::SeqCst), 1);
        assert!(matches!(
            module_handle.validate_route(RouteHandle::new(6, 3, 1)),
            Err(SubcModuleError::StaleRouteHandle(_))
        ));
    }

    struct RebindCountingHandler {
        bound: Arc<AtomicUsize>,
        cleanup: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl ModuleHandler for RebindCountingHandler {
        async fn handle(&self, _ctx: RequestCtx, _body: Vec<u8>) -> HandlerOutcome {
            HandlerOutcome::Response(Vec::new())
        }

        async fn on_bound(&self, _handle: &RouteHandle) {
            self.bound.fetch_add(1, Ordering::SeqCst);
        }

        async fn on_route_gone(&self, _handle: &RouteHandle) {
            self.cleanup.fetch_add(1, Ordering::SeqCst);
        }
    }

    // Wire spec 3.3.0: a bind on an installed channel with a strictly higher epoch
    // replaces the stale install (the daemon freed that binding; its route-gone
    // GOODBYE is best-effort and can be dropped), firing the replaced install's
    // route-gone teardown. Equal-or-lower epoch is a protocol violation: rejected,
    // installed route untouched.
    #[tokio::test]
    async fn rebind_on_installed_channel_replaces_on_higher_epoch_only() {
        let (module_handle, mut rx) = test_module_handle(&[]);
        let tx = module_handle.shared.lock_inner().writer.clone().unwrap();
        let bound = Arc::new(AtomicUsize::new(0));
        let cleanup = Arc::new(AtomicUsize::new(0));
        let handler = Arc::new(RebindCountingHandler {
            bound: Arc::clone(&bound),
            cleanup: Arc::clone(&cleanup),
        });

        // Install epoch 4 on channel 8.
        handle_frame(
            route_bind_frame(8, 4, 90),
            &tx,
            Arc::clone(&handler),
            RequestDispatcher::new(),
            module_handle.clone(),
        )
        .await
        .unwrap();
        assert_eq!(rx.recv().await.unwrap().header.ty, FrameType::Response);
        assert_eq!(
            (bound.load(Ordering::SeqCst), cleanup.load(Ordering::SeqCst)),
            (1, 0)
        );

        // Same epoch: rejected, install untouched, no teardown fired.
        handle_frame(
            route_bind_frame(8, 4, 91),
            &tx,
            Arc::clone(&handler),
            RequestDispatcher::new(),
            module_handle.clone(),
        )
        .await
        .unwrap();
        let reject = rx.recv().await.unwrap();
        assert_eq!(reject.header.ty, FrameType::Error);
        assert_eq!(
            (bound.load(Ordering::SeqCst), cleanup.load(Ordering::SeqCst)),
            (1, 0)
        );
        module_handle
            .validate_route(RouteHandle::new(8, 4, 1))
            .expect("epoch-4 install must survive the rejected rebind");

        // Lower epoch: same rejection shape.
        handle_frame(
            route_bind_frame(8, 3, 92),
            &tx,
            Arc::clone(&handler),
            RequestDispatcher::new(),
            module_handle.clone(),
        )
        .await
        .unwrap();
        assert_eq!(rx.recv().await.unwrap().header.ty, FrameType::Error);
        assert_eq!(
            (bound.load(Ordering::SeqCst), cleanup.load(Ordering::SeqCst)),
            (1, 0)
        );

        // Strictly higher epoch: implicit replace — stale install torn down
        // (route-gone fired exactly once), new epoch installed and bound.
        handle_frame(
            route_bind_frame(8, 5, 93),
            &tx,
            Arc::clone(&handler),
            RequestDispatcher::new(),
            module_handle.clone(),
        )
        .await
        .unwrap();
        assert_eq!(rx.recv().await.unwrap().header.ty, FrameType::Response);
        assert_eq!(
            (bound.load(Ordering::SeqCst), cleanup.load(Ordering::SeqCst)),
            (2, 1)
        );
        assert!(matches!(
            module_handle.validate_route(RouteHandle::new(8, 4, 1)),
            Err(SubcModuleError::StaleRouteHandle(_))
        ));
        module_handle
            .validate_route(RouteHandle::new(8, 5, 1))
            .expect("epoch-5 install must be live after implicit replace");
    }

    #[tokio::test]
    async fn module_control_corr_is_monotonic_and_exhausts_without_wrap() {
        let (module_handle, _rx) = test_module_handle(&[MODULE_TO_SUBC_OP_CATALOG_UPDATE]);
        let mut inner = module_handle.shared.lock_inner();
        inner.next_corr = Some(u64::MAX);
        assert_eq!(next_module_control_corr(&mut inner), Some(u64::MAX));
        assert_eq!(next_module_control_corr(&mut inner), None);
    }
}

/// How a served module ends when its connection closes, driven against a stand-in
/// daemon on a framed in-memory connection with the real writer task and drain.
#[cfg(test)]
mod module_close_tests {
    use std::time::Instant;

    use tokio::{io::DuplexStream, sync::Notify};

    use super::stand_in::{serve_against_stand_in_with_ops, Served};
    use super::*;

    /// How the test handler treats the one request it is given.
    #[derive(Clone, Copy)]
    enum Hold {
        /// Hold the request open until it is cancelled, then answer.
        UntilCancelled,
        /// Hold the request open forever, whatever happens.
        IgnoringCancellation,
    }

    struct HoldingHandler {
        hold: Hold,
        entered: Arc<Notify>,
        token: Arc<Mutex<Option<CancellationToken>>>,
    }

    #[async_trait]
    impl ModuleHandler for HoldingHandler {
        async fn handle(&self, ctx: RequestCtx, _body: Vec<u8>) -> HandlerOutcome {
            *self.token.lock().unwrap() = Some(ctx.cancellation_token());
            self.entered.notify_one();
            match self.hold {
                Hold::UntilCancelled => {
                    ctx.cancelled().await;
                    HandlerOutcome::Error {
                        code: "cancelled".to_string(),
                        message: "request cancelled".to_string(),
                    }
                }
                Hold::IgnoringCancellation => std::future::pending().await,
            }
        }
    }

    /// Serve `handler` on the shared stand-in and hand its framed connection to the test.
    async fn serve_against_stand_in<H: ModuleHandler>(handler: H) -> Served {
        serve_against_stand_in_with_ops(|_| handler, &[]).await
    }

    async fn send(daemon: &mut DuplexStream, frame: Frame) {
        write_frame(daemon, &frame).await.unwrap();
        daemon.flush().await.unwrap();
    }

    /// Bind route 7/1 and send one data request on it, returning once the
    /// handler holds it.
    async fn hold_one_request(served: &mut Served, entered: &Notify) {
        let bind = serde_json::to_vec(&ModuleControlRequest::RouteBind {
            route_channel: 7,
            epoch: 1,
            target: RouteTarget::ToolProvider {
                module_id: "close-test".to_string(),
            },
            identity: BindIdentity::new(
                PathBuf::from("/tmp/project"),
                "test".to_string(),
                "close".to_string(),
            ),
            principal: None,
            consumer_capabilities: None,
            role_versions: None,
            admission_facts: None,
            scope: None,
        })
        .unwrap();
        send(
            &mut served.daemon,
            Frame::build(FrameType::Request, control_flags(), 0, 0, 2, bind).unwrap(),
        )
        .await;
        loop {
            let frame = read_frame(&mut served.daemon).await.unwrap().unwrap();
            if frame.header.channel == 0 && frame.header.corr == 2 {
                assert_eq!(
                    frame.header.ty,
                    FrameType::Response,
                    "route.bind must be acked"
                );
                break;
            }
        }
        send(
            &mut served.daemon,
            Frame::build(FrameType::Request, data_flags(), 7, 1, 10, b"hold".to_vec()).unwrap(),
        )
        .await;
        timeout(Duration::from_secs(2), entered.notified())
            .await
            .expect("the handler must receive the request");
    }

    fn goodbye() -> Frame {
        Frame::build(FrameType::Goodbye, control_flags(), 0, 0, 0, Vec::new()).unwrap()
    }

    fn holding(
        hold: Hold,
    ) -> (
        HoldingHandler,
        Arc<Notify>,
        Arc<Mutex<Option<CancellationToken>>>,
    ) {
        let entered = Arc::new(Notify::new());
        let token = Arc::new(Mutex::new(None));
        let handler = HoldingHandler {
            hold,
            entered: Arc::clone(&entered),
            token: Arc::clone(&token),
        };
        (handler, entered, token)
    }

    #[tokio::test]
    async fn goodbye_cancels_an_open_request_and_the_serve_future_completes() {
        let (handler, entered, token) = holding(Hold::UntilCancelled);
        let mut served = serve_against_stand_in(handler).await;
        hold_one_request(&mut served, &entered).await;

        let started = Instant::now();
        send(&mut served.daemon, goodbye()).await;
        let result = timeout(Duration::from_secs(5), &mut served.serve)
            .await
            .expect("a held request must not keep the serve future alive after GOODBYE");
        result.unwrap().unwrap();
        assert!(
            token.lock().unwrap().as_ref().unwrap().is_cancelled(),
            "GOODBYE must cancel the request's token"
        );
        // Well inside the writer's drain limit: the handler answered its
        // cancellation, so nothing had to be aborted.
        assert!(
            started.elapsed() < WRITER_DRAIN_LIMIT,
            "closing took {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn a_handler_ignoring_cancellation_cannot_keep_the_serve_future_alive() {
        let (handler, entered, _token) = holding(Hold::IgnoringCancellation);
        let mut served = serve_against_stand_in(handler).await;
        hold_one_request(&mut served, &entered).await;

        let started = Instant::now();
        send(&mut served.daemon, goodbye()).await;
        let result = timeout(
            WRITER_DRAIN_LIMIT + Duration::from_secs(3),
            &mut served.serve,
        )
        .await
        .expect("the writer drain must be bounded");
        result.unwrap().unwrap();
        assert!(started.elapsed() >= WRITER_DRAIN_LIMIT);
    }

    #[tokio::test]
    async fn closed_resolves_after_goodbye() {
        let served = serve_against_stand_in(EchoHandler).await;
        let mut served = served;
        assert!(!served.handle.is_closed());
        let closed = served.handle.closed();
        send(&mut served.daemon, goodbye()).await;
        timeout(Duration::from_secs(2), closed)
            .await
            .expect("closed() must resolve after GOODBYE");
        assert!(served.handle.is_closed());
    }

    #[tokio::test]
    async fn closed_resolves_after_eof() {
        let Served {
            daemon,
            handle,
            serve: _serve,
        } = serve_against_stand_in(EchoHandler).await;
        assert!(!handle.is_closed());
        drop(daemon);
        timeout(Duration::from_secs(2), handle.closed())
            .await
            .expect("closed() must resolve after EOF");
        assert!(handle.is_closed());
    }

    struct EchoHandler;

    #[async_trait]
    impl ModuleHandler for EchoHandler {
        async fn handle(&self, _ctx: RequestCtx, body: Vec<u8>) -> HandlerOutcome {
            HandlerOutcome::Response(body)
        }
    }

    /// Records every `on_connection_end` call, so a test can assert both the
    /// cause and that it was reported exactly once.
    #[derive(Clone, Default)]
    struct EndRecorder(Arc<Mutex<Vec<ConnectionEnd>>>);

    #[async_trait]
    impl ModuleHandler for EndRecorder {
        async fn handle(&self, _ctx: RequestCtx, body: Vec<u8>) -> HandlerOutcome {
            HandlerOutcome::Response(body)
        }
        async fn on_connection_end(&self, end: ConnectionEnd) {
            self.0.lock().unwrap().push(end);
        }
    }

    async fn reported_end(recorder: &EndRecorder, served: &mut Served) -> Vec<ConnectionEnd> {
        timeout(Duration::from_secs(5), &mut served.serve)
            .await
            .expect("serving must end")
            .unwrap()
            .unwrap();
        recorder.0.lock().unwrap().clone()
    }

    #[tokio::test]
    async fn a_daemon_goodbye_is_reported_as_goodbye() {
        let recorder = EndRecorder::default();
        let mut served = serve_against_stand_in(recorder.clone()).await;
        send(&mut served.daemon, goodbye()).await;
        assert_eq!(
            reported_end(&recorder, &mut served).await,
            vec![ConnectionEnd::Goodbye]
        );
    }

    #[tokio::test]
    async fn a_daemon_closing_without_goodbye_is_reported_as_eof() {
        let recorder = EndRecorder::default();
        let mut served = serve_against_stand_in(recorder.clone()).await;
        served.daemon.shutdown().await.unwrap();
        assert_eq!(
            reported_end(&recorder, &mut served).await,
            vec![ConnectionEnd::Eof]
        );
    }

    /// What a drain-recording handler saw, in the order it saw it.
    #[derive(Debug, Clone, PartialEq)]
    enum Seen {
        Draining(RouteCloseReason, SystemTime),
        End(ConnectionEnd),
    }

    /// Records drain notices and the connection end. With `park` set, the
    /// drain hook never returns after recording, to stand for a slow hook.
    #[derive(Clone, Default)]
    struct DrainRecorder {
        seen: Arc<Mutex<Vec<Seen>>>,
        park: bool,
    }

    #[async_trait]
    impl ModuleHandler for DrainRecorder {
        async fn handle(&self, _ctx: RequestCtx, body: Vec<u8>) -> HandlerOutcome {
            HandlerOutcome::Response(body)
        }
        async fn on_draining(&self, reason: RouteCloseReason, deadline: SystemTime) {
            self.seen
                .lock()
                .unwrap()
                .push(Seen::Draining(reason, deadline));
            if self.park {
                std::future::pending::<()>().await;
            }
        }
        async fn on_connection_end(&self, end: ConnectionEnd) {
            self.seen.lock().unwrap().push(Seen::End(end));
        }
    }

    fn control_push(body: &[u8]) -> Frame {
        Frame::build(FrameType::Push, control_flags(), 0, 0, 0, body.to_vec()).unwrap()
    }

    fn draining_push(reason: subc_protocol::RouteCloseReason, deadline_ms: u64) -> Frame {
        control_push(
            &serde_json::to_vec(&ModuleControlCommand::Draining {
                reason,
                deadline_ms,
            })
            .unwrap(),
        )
    }

    /// Send a channel-0 PING and require the PONG, proving the frame reader
    /// is still serving.
    async fn ping_is_answered(served: &mut Served, corr: u64) {
        send(
            &mut served.daemon,
            Frame::build(FrameType::Ping, control_flags(), 0, 0, corr, Vec::new()).unwrap(),
        )
        .await;
        let pong = timeout(Duration::from_secs(2), read_frame(&mut served.daemon))
            .await
            .expect("the module must answer PING")
            .unwrap()
            .expect("the module must not close the connection");
        assert_eq!(pong.header.ty, FrameType::Pong);
        assert_eq!(pong.header.corr, corr);
    }

    /// The daemon sends `module.draining` and, once the drain settles, GOODBYE;
    /// with nothing to wait for, both can land in one read. The hook runs on its
    /// own task, and must still be called before the GOODBYE is acted on.
    #[tokio::test]
    async fn a_drain_notice_calls_on_draining_before_the_goodbye_that_follows_it() {
        let recorder = DrainRecorder::default();
        let mut served = serve_against_stand_in(recorder.clone()).await;
        // Any fixed value works; this one (2100-01-01T00:00:00Z) is far enough
        // ahead to be plainly a future deadline.
        let deadline_ms = 4_102_444_800_000;
        write_frame(
            &mut served.daemon,
            &draining_push(subc_protocol::RouteCloseReason::Restart, deadline_ms),
        )
        .await
        .unwrap();
        write_frame(&mut served.daemon, &goodbye()).await.unwrap();
        served.daemon.flush().await.unwrap();

        timeout(Duration::from_secs(5), &mut served.serve)
            .await
            .expect("serving must end after GOODBYE")
            .unwrap()
            .unwrap();
        assert_eq!(
            *recorder.seen.lock().unwrap(),
            vec![
                Seen::Draining(
                    RouteCloseReason::Restart,
                    UNIX_EPOCH + Duration::from_millis(deadline_ms)
                ),
                Seen::End(ConnectionEnd::Goodbye),
            ]
        );
    }

    /// A drain lasts until the daemon's deadline, and the daemon keeps pinging
    /// through it; a hook still busy must not stop the module answering.
    #[tokio::test]
    async fn a_drain_hook_that_never_returns_does_not_stop_the_frame_reader() {
        let recorder = DrainRecorder {
            park: true,
            ..DrainRecorder::default()
        };
        let mut served = serve_against_stand_in(recorder.clone()).await;
        send(
            &mut served.daemon,
            draining_push(subc_protocol::RouteCloseReason::Reload, 1),
        )
        .await;
        ping_is_answered(&mut served, 41).await;
        assert!(matches!(
            recorder.seen.lock().unwrap().as_slice(),
            [Seen::Draining(RouteCloseReason::Reload, _)]
        ));

        send(&mut served.daemon, goodbye()).await;
        timeout(Duration::from_secs(5), &mut served.serve)
            .await
            .expect("a parked drain hook must not keep the serve future alive")
            .unwrap()
            .unwrap();
    }

    /// A channel-0 push this SDK cannot decode (a newer daemon's command, or a
    /// malformed body) is ignored: no hook is called and the connection keeps
    /// serving.
    #[tokio::test]
    async fn an_undecodable_channel_zero_push_is_ignored_and_the_connection_stays_up() {
        let recorder = DrainRecorder::default();
        let mut served = serve_against_stand_in(recorder.clone()).await;
        send(
            &mut served.daemon,
            control_push(br#"{"op":"module.some_future_command","x":1}"#),
        )
        .await;
        send(&mut served.daemon, control_push(b"not json")).await;
        send(
            &mut served.daemon,
            control_push(br#"{"op":"module.draining","reason":"restart"}"#),
        )
        .await;
        ping_is_answered(&mut served, 42).await;
        assert!(!served.handle.is_closed());
        assert!(!served.serve.is_finished());
        assert!(recorder.seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_module_closing_its_own_connection_is_reported_as_closed() {
        let recorder = EndRecorder::default();
        let mut served = serve_against_stand_in(recorder.clone()).await;
        served.handle.close_connection();
        assert_eq!(
            reported_end(&recorder, &mut served).await,
            vec![ConnectionEnd::Closed]
        );
    }
}

#[cfg(test)]
mod operator_confirm_tests;

#[cfg(test)]
mod detailed_error_body_tests {
    use serde_json::json;

    use subc_protocol::ErrorBody;

    /// The wire bytes a detail-carrying module error serializes to are pinned
    /// here because subc-mcp and route consumers parse them; the shape predates
    /// ErrorBody.detail and must not drift now that ErrorBody subsumes it.
    #[test]
    fn detailed_error_body_keeps_code_message_and_detail() {
        let body = ErrorBody::new("bad_request", "invalid envelope")
            .with_detail(json!({"reason": "missing_server"}));

        assert_eq!(
            serde_json::to_value(body).unwrap(),
            json!({
                "code": "bad_request",
                "message": "invalid envelope",
                "detail": {"reason": "missing_server"},
            })
        );
    }

    /// A detail-less body serializes byte-identically to the pre-detail wire:
    /// deserializing the old two-field shape and re-serializing adds nothing.
    #[test]
    fn detail_less_error_body_is_byte_identical_to_the_pre_detail_wire() {
        let old_wire = r#"{"code":"cancelled","message":"caller cancelled"}"#;
        let parsed: ErrorBody = serde_json::from_str(old_wire).unwrap();
        assert_eq!(parsed.detail, None);
        assert_eq!(serde_json::to_string(&parsed).unwrap(), old_wire);
    }
}

// A reserved module may scrub its launch nonce before it opens a daemon
// connection. The retained copy exists only for encoding that module's HELLO.
static RETAINED_LAUNCH_NONCE: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Retain a reserved module's launch nonce for its HELLO after the module has
/// scrubbed the nonce from the process environment.
pub fn retain_launch_nonce_for_hello(launch_nonce: String) -> Result<(), String> {
    RETAINED_LAUNCH_NONCE
        .set(launch_nonce)
        .map_err(|_| "launch nonce was already retained for this process".to_string())
}

fn retained_launch_nonce() -> Option<String> {
    RETAINED_LAUNCH_NONCE.get().cloned()
}

/// Where this process read its launch nonce, in the form module provenance
/// reports it (`fd` or `env`), or `None` when it has none or could not read
/// it. For a module that builds its provenance block itself:
/// `.with_launch_nonce_source(subc_client_rs::launch_nonce_source())`.
pub fn launch_nonce_source() -> Option<LaunchNonceSource> {
    launch_nonce()
        .ok()
        .flatten()
        .map(|nonce| LaunchNonceSource::from_wire_name(nonce.source().as_str()))
}

/// Fill in `launch_nonce_source` on a module's declared provenance, so
/// `ck provenance` can tell a module reading the pipe from one still reading
/// the environment variable. Only when the module declared provenance and left the field
/// unset, and only when the nonce HELLO carries is the one the accessor read:
/// a nonce retained by the module itself came from somewhere this SDK cannot
/// vouch for.
fn stamp_launch_nonce_source(manifest: &mut ModuleManifest, sent: Option<&str>) {
    let Some(provenance) = manifest.provenance.as_mut() else {
        return;
    };
    if provenance.launch_nonce_source.is_some() {
        return;
    }
    if let Ok(Some(nonce)) = launch_nonce() {
        if sent == Some(nonce.value()) {
            provenance.launch_nonce_source =
                Some(LaunchNonceSource::from_wire_name(nonce.source().as_str()));
        }
    }
}

#[cfg(test)]
mod checked_increment_tests {
    use super::checked_increment;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn returns_the_previous_value_and_stops_at_the_maximum() {
        let counter = AtomicU64::new(7);
        assert_eq!(checked_increment(&counter), Some(7));
        assert_eq!(counter.load(Ordering::Relaxed), 8);

        let full = AtomicU64::new(u64::MAX);
        assert_eq!(checked_increment(&full), None);
        assert_eq!(
            full.load(Ordering::Relaxed),
            u64::MAX,
            "an exhausted counter is left unchanged"
        );
    }
}
