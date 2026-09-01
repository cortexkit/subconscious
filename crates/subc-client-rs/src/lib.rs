#![forbid(unsafe_code)]

pub mod consumer;
pub mod policy_cache;
pub use consumer::{
    CallError, CallOptions, CatalogList, CloseRouteOptions, ConnectionState, ConsumerError,
    ConsumerOptions, ControlPush, PushEvent, RetryBackoff, RouteCloseDisposition, RouteCloseReason,
    RoutePollResult, SubcConsumer, SubscribeOptions, Subscription, SubscriptionClosed,
};
pub use policy_cache::{
    PolicyResolveError, PolicyResolver, PolicyResolverConfig, PolicyVerdict, ProjectRef, Subject,
    DEFAULT_POLICY_RESOLVER_MODULE_ID,
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
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

pub use async_trait::async_trait;
pub use subc_control::{CatalogEntry, ConsumerIdentity};
use subc_protocol::{
    manifest::ModuleManifest,
    session::{
        ModuleControlRequest, ModuleControlRequestFromModule, ModuleControlResponse,
        ModuleControlResponseToModule, MODULE_CONTROL_OP_HEALTH_CHECK,
        MODULE_TO_SUBC_OP_CATALOG_UPDATE,
    },
    BindIdentity, ErrorBody, Flags, Frame, FrameBuildError, FrameType, ModuleHelloAckBody,
    ModuleHelloBody, Principal, Priority, RouteTarget, PROTOCOL_VERSION, SUBC_LAUNCH_NONCE_ENV,
    SUBC_MODULE_ID_ENV,
};
pub use subc_protocol::{
    manifest::{
        build_provenance, CapabilityDeclarations, CapabilityNeed, CapabilityRequirement,
        ExecutionMode, ManifestProvenance, ProvenanceFormError, ProviderRole, Tool,
        PROVENANCE_SENTINELS,
    },
    session::{HealthReport, HealthStatus},
    AdmissionClass, SUBC_PROTOCOL_CRATE_VERSION,
};

use subc_transport::{
    authenticate_client, connection_file, read_frame, write_frame, AuthError, ConnectionFileError,
    FrameIoError,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufWriter},
    net::TcpStream,
    sync::{mpsc, oneshot, Semaphore},
    time::timeout,
};
use tokio_util::sync::{CancellationToken, WaitForCancellationFuture};

const AUTH_DEADLINE: Duration = Duration::from_secs(2);
const CATALOG_UPDATE_TIMEOUT: Duration = Duration::from_secs(10);
const EGRESS_BUFFER: usize = 64;
const HANDLER_TASK_CAPACITY: usize = 64;
const HELLO_CORR: u64 = 1;
static NEXT_MODULE_CONNECTION_TOKEN: AtomicU64 = AtomicU64::new(1);

type RequestKey = (u16, u32, u64);
type InFlight = Arc<Mutex<HashMap<RequestKey, CancellationToken>>>;

/// Immutable identity of one route binding on one live connection.
///
/// Only `channel` and `epoch` are serialized. The private connection token prevents
/// work retained from an earlier connection from acting on a later connection that
/// happens to reuse the same wire pair.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct RouteHandle {
    pub channel: u16,
    pub epoch: u32,
    connection_token: u64,
}

impl RouteHandle {
    pub(crate) fn new(channel: u16, epoch: u32, connection_token: u64) -> Self {
        Self {
            channel,
            epoch,
            connection_token,
        }
    }

    pub(crate) fn connection_token(self) -> u64 {
        self.connection_token
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
type CatalogUpdateReply = oneshot::Sender<Result<(), CatalogUpdateError>>;
type CatalogUpdateWaiter = oneshot::Receiver<Result<(), CatalogUpdateError>>;
type CatalogUpdateRequest = (u64, mpsc::Sender<Frame>, CatalogUpdateWaiter);

/// Future returned by [`serve_with_handle`] that runs the module until GOODBYE or EOF.
pub type ModuleServeFuture = Pin<Box<dyn Future<Output = Result<(), SubcModuleError>> + Send>>;

#[derive(Clone)]
struct RequestDispatcher {
    in_flight: InFlight,
    permits: Arc<Semaphore>,
}

impl RequestDispatcher {
    fn new() -> Self {
        Self {
            in_flight: Arc::new(Mutex::new(HashMap::new())),
            permits: Arc::new(Semaphore::new(HANDLER_TASK_CAPACITY)),
        }
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
    connection_token: u64,
    live_routes: Mutex<HashMap<u16, RouteHandle>>,
    dropped_route_frames: AtomicU64,
    close_token: CancellationToken,
    inner: Mutex<ModuleHandleState>,
}

struct ModuleHandleState {
    writer: Option<mpsc::Sender<Frame>>,
    next_corr: Option<u64>,
    pending_catalog_updates: HashMap<u64, CatalogUpdateReply>,
    closed: bool,
}

impl ModuleHandle {
    fn new(
        ack: &ModuleHelloAckBody,
        writer: mpsc::Sender<Frame>,
        connection_token: u64,
        close_token: CancellationToken,
    ) -> Self {
        Self {
            shared: Arc::new(ModuleHandleShared {
                negotiated_ver: ack.negotiated_ver,
                supports_catalog_update: ack
                    .subc_ops
                    .iter()
                    .any(|op| op == MODULE_TO_SUBC_OP_CATALOG_UPDATE),
                connection_token,
                live_routes: Mutex::new(HashMap::new()),
                dropped_route_frames: AtomicU64::new(0),
                close_token,
                inner: Mutex::new(ModuleHandleState {
                    writer: Some(writer),
                    next_corr: Some(HELLO_CORR + 1),
                    pending_catalog_updates: HashMap::new(),
                    closed: false,
                }),
            }),
        }
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
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(CatalogUpdateError::ConnectionClosed),
            Err(_) => {
                self.shared.remove_pending_catalog_update(corr);
                Err(CatalogUpdateError::Timeout)
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
        let Some(reply) = self.shared.take_pending_catalog_update(frame.header.corr) else {
            return false;
        };
        let result = match frame.header.ty {
            FrameType::Response => {
                match serde_json::from_slice::<ModuleControlResponseToModule>(&frame.body) {
                    Ok(ModuleControlResponseToModule::CatalogUpdate {}) => Ok(()),
                    Err(err) => Err(CatalogUpdateError::Protocol(format!(
                        "invalid catalog.update response body: {err}"
                    ))),
                }
            }
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
            inner.closed = true;
            self.close_token.cancel();
            drop(inner);
            self.clear_live_routes();
            return Err(CatalogUpdateError::ConnectionClosed);
        };
        let Some(corr) = next_module_control_corr(&mut inner) else {
            inner.closed = true;
            inner.writer = None;
            let pending = inner
                .pending_catalog_updates
                .drain()
                .map(|(_, reply)| reply)
                .collect::<Vec<_>>();
            self.close_token.cancel();
            drop(inner);
            self.clear_live_routes();
            for reply in pending {
                let _ = reply.send(Err(CatalogUpdateError::ConnectionClosed));
            }
            return Err(CatalogUpdateError::ConnectionClosed);
        };
        let (tx, rx) = oneshot::channel();
        inner.pending_catalog_updates.insert(corr, tx);
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

/// Trait implemented by a module for its business logic. The serve functions in
/// this crate own all wire-protocol plumbing.
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
    /// THE DEFAULT ASSERTS HEALTH ON BEHALF OF A MODULE THAT NEVER WROTE ANY.
    /// A module that has not implemented this is indistinguishable on the wire
    /// from one that measured itself and found nothing wrong -- and the daemon
    /// acts on the difference, since a healthy report suppresses escalation
    /// while an absent implementation means nothing was ever checked.
    ///
    /// It stays a default because health is genuinely optional: a module that
    /// advertises no health capability is never probed, so the value is unread
    /// for those. The hazard is the module that DOES advertise health and
    /// inherits this -- it answers "ok" forever, including while wedged.
    ///
    /// Per Health-Path-Rule v3 an implementation must derive its status
    /// mechanically from signals the dispatch path stamps (a monotonic
    /// heartbeat, oldest-queued age), never from its own opinion, and must not
    /// take a blocking lock, touch disk, or spawn a subprocess on this path.
    /// A health reply that execs queues behind the host's slowest shared
    /// resource -- which is exactly the resource degraded under the conditions
    /// being probed.
    async fn health(&self) -> HealthReport {
        // SAY THAT NOBODY MEASURED, rather than that everything is fine.
        //
        // The status stays Ok because a module advertising no health capability
        // is never probed, and one that advertises health but has nothing to
        // report is not unhealthy. What changes is that the report now
        // IDENTIFIES ITSELF as the inherited default, so an operator reading
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

/// Route-bind request delivered on channel 0.
#[derive(Debug, Clone)]
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
    /// Opaque admission facts relayed by subc from its configured carrier.
    pub admission_facts: Option<serde_json::Value>,
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
/// GOODBYE or clean EOF.
pub async fn serve<H>(mut manifest: ModuleManifest, handler: H) -> Result<(), SubcModuleError>
where
    H: ModuleHandler,
{
    let connection_file = parse_subc_arg(env::args_os().skip(1))?;
    if let Some(module_id) = module_id_from_env()? {
        manifest.module_id = module_id;
    }
    serve_with(&connection_file, manifest, handler).await
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
    let writer = tokio::spawn(drain_writer(write_half, rx));
    let handler = Arc::new(handler);

    send_hello(&tx, manifest).await?;
    let ack = expect_hello_ack(&mut read_half).await?;
    handler.on_hello_ack(&ack).await;

    let connection_token = NEXT_MODULE_CONNECTION_TOKEN
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |token| {
            token.checked_add(1)
        })
        .map_err(|_| SubcModuleError::ConnectionTokenExhausted)?;
    let close_token = CancellationToken::new();
    let handle = ModuleHandle::new(&ack, tx.clone(), connection_token, close_token);
    let serve_handle = handle.clone();
    let serve_future = Box::pin(async move {
        // Connection loss ends this serve future. Module serving retains no
        // reconnect task or in-flight reconnect gate; a supervisor that needs
        // recovery starts a fresh serve_with_handle invocation.
        let loop_result =
            module_loop(read_half, tx, Arc::clone(&handler), serve_handle.clone()).await;
        serve_handle.close_connection();

        let writer_result = writer.await.map_err(SubcModuleError::WriterTask);
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
    });
    Ok((handle, serve_future))
}

async fn module_loop<R, H>(
    mut reader: R,
    egress: mpsc::Sender<Frame>,
    handler: Arc<H>,
    module_handle: ModuleHandle,
) -> Result<(), SubcModuleError>
where
    R: AsyncRead + Unpin,
    H: ModuleHandler,
{
    let dispatcher = RequestDispatcher::new();
    loop {
        let read = tokio::select! {
            () = module_handle.shared.close_token.cancelled() => return Ok(()),
            read = read_frame(&mut reader) => read,
        };
        let frame = match read {
            Ok(Some(frame)) => frame,
            // Clean EOF: the daemon closed the connection.
            Ok(None) => return Ok(()),
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
                return Ok(());
            }
            Err(err) => return Err(SubcModuleError::FrameIo(err)),
        };
        if !handle_frame(
            frame,
            &egress,
            Arc::clone(&handler),
            dispatcher.clone(),
            module_handle.clone(),
        )
        .await?
        {
            return Ok(());
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
        _ => Ok(true),
    }
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
    tokio::spawn(async move {
        let Ok(_permit) = permits.acquire_owned().await else {
            // A closed dispatcher means connection teardown will release every route credit.
            if let Ok(mut guard) = in_flight.lock() {
                guard.remove(&(handle.channel, handle.epoch, corr));
            }
            return;
        };
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
    let permits = Arc::clone(&dispatcher.permits);
    tokio::spawn(async move {
        let Ok(_permit) = permits.acquire_owned().await else {
            if let Ok(mut guard) = in_flight.lock() {
                guard.remove(&(channel, epoch, corr));
            }
            return;
        };
        if !cancellation.is_cancelled() {
            let report = handler.health().await;
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
    let request = serde_json::from_slice::<ModuleControlRequest>(&frame.body)
        .map_err(SubcModuleError::Json)?;
    match request {
        ModuleControlRequest::RouteBind {
            route_channel,
            epoch,
            target,
            identity,
            principal,
            consumer_capabilities,
            admission_facts,
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
                admission_facts,
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
    manifest: ModuleManifest,
) -> Result<(), SubcModuleError> {
    let body = serde_json::to_vec(&ModuleHelloBody {
        manifest,
        protocol_ver: PROTOCOL_VERSION,
        control_ops: Some(vec![MODULE_CONTROL_OP_HEALTH_CHECK.to_string()]),
        launch_nonce: retained_launch_nonce().or_else(|| {
            env::var(SUBC_LAUNCH_NONCE_ENV)
                .ok()
                .filter(|value| !value.is_empty())
        }),
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
    use tokio::{sync::Notify, time::timeout};

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
        release: Arc<Notify>,
    }

    #[async_trait]
    impl ModuleHandler for BlockingHandler {
        async fn handle(&self, _ctx: RequestCtx, _body: Vec<u8>) -> HandlerOutcome {
            self.entered.fetch_add(1, Ordering::SeqCst);
            self.release.notified().await;
            HandlerOutcome::Streamed
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
        };
        (ModuleHandle::new(&ack, tx, 1, CancellationToken::new()), rx)
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

    #[tokio::test]
    async fn health_check_waits_behind_saturated_request_dispatcher() {
        let (tx, mut rx) = mpsc::channel(4);
        let entered = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(Notify::new());
        let handler = Arc::new(BlockingHandler {
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
        });
        let dispatcher = RequestDispatcher::new();
        let (module_handle, _unused_rx) = test_module_handle(&[]);
        module_handle
            .install_route(RouteHandle::new(7, 1, 1))
            .unwrap();

        for corr in 0..HANDLER_TASK_CAPACITY as u64 {
            handle_frame(
                data_request(7, corr + 1),
                &tx,
                Arc::clone(&handler),
                dispatcher.clone(),
                module_handle.clone(),
            )
            .await
            .unwrap();
        }

        timeout(Duration::from_secs(1), async {
            while entered.load(Ordering::SeqCst) < HANDLER_TASK_CAPACITY {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        handle_frame(health_request(900), &tx, handler, dispatcher, module_handle)
            .await
            .unwrap();
        assert!(
            timeout(Duration::from_millis(75), rx.recv()).await.is_err(),
            "health.check must share the same saturated request dispatch capacity as data requests"
        );

        release.notify_waiters();
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
            in_flight: Arc::new(Mutex::new(HashMap::new())),
            permits: Arc::new(Semaphore::new(1)),
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
            identity: BindIdentity {
                project_root: PathBuf::from("/tmp/project"),
                harness: "test".to_string(),
                session: "bind".to_string(),
            },
            principal: None,
            consumer_capabilities: None,
            admission_facts: None,
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

    #[test]
    fn module_control_corr_is_monotonic_and_exhausts_without_wrap() {
        let (module_handle, _rx) = test_module_handle(&[MODULE_TO_SUBC_OP_CATALOG_UPDATE]);
        let mut inner = module_handle.shared.lock_inner();
        inner.next_corr = Some(u64::MAX);
        assert_eq!(next_module_control_corr(&mut inner), Some(u64::MAX));
        assert_eq!(next_module_control_corr(&mut inner), None);
    }
}

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
