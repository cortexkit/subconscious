#![forbid(unsafe_code)]

pub mod consumer;
pub use consumer::{
    CallError, CallOptions, CloseRouteOptions, ConnectionState, ConsumerError, ConsumerOptions,
    RetryBackoff, SubcConsumer, SubscribeOptions, Subscription, SubscriptionClosed,
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
    sync::{Arc, Mutex},
    time::Duration,
};

pub use async_trait::async_trait;
pub use subc_control::ConsumerIdentity;
pub use subc_protocol::session::{HealthReport, HealthStatus};
use subc_protocol::{
    manifest::{ModuleManifest, ProviderRole},
    session::{
        ModuleControlRequest, ModuleControlRequestFromModule, ModuleControlResponse,
        ModuleControlResponseToModule, MODULE_CONTROL_OP_HEALTH_CHECK,
        MODULE_TO_SUBC_OP_CATALOG_UPDATE,
    },
    BindIdentity, ErrorBody, Flags, Frame, FrameBuildError, FrameType, ModuleHelloAckBody,
    ModuleHelloBody, Principal, Priority, RouteTarget, PROTOCOL_VERSION, SUBC_LAUNCH_NONCE_ENV,
    SUBC_MODULE_ID_ENV,
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

type RequestKey = (u16, u64);
type InFlight = Arc<Mutex<HashMap<RequestKey, CancellationToken>>>;
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
    inner: Mutex<ModuleHandleState>,
}

struct ModuleHandleState {
    writer: Option<mpsc::Sender<Frame>>,
    next_corr: u64,
    pending_catalog_updates: HashMap<u64, CatalogUpdateReply>,
    closed: bool,
}

impl ModuleHandle {
    fn new(ack: &ModuleHelloAckBody, writer: mpsc::Sender<Frame>) -> Self {
        Self {
            shared: Arc::new(ModuleHandleShared {
                negotiated_ver: ack.negotiated_ver,
                supports_catalog_update: ack
                    .subc_ops
                    .iter()
                    .any(|op| op == MODULE_TO_SUBC_OP_CATALOG_UPDATE),
                inner: Mutex::new(ModuleHandleState {
                    writer: Some(writer),
                    next_corr: HELLO_CORR + 1,
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
        if !self.shared.supports_catalog_update {
            return Err(CatalogUpdateError::NotSupported);
        }

        let body = serde_json::to_vec(&ModuleControlRequestFromModule::CatalogUpdate { provides })
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
            return Err(CatalogUpdateError::ConnectionClosed);
        };
        let corr = next_module_control_corr(&mut inner);
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
            inner
                .pending_catalog_updates
                .drain()
                .map(|(_, reply)| reply)
                .collect::<Vec<_>>()
        };
        for reply in pending {
            let _ = reply.send(Err(CatalogUpdateError::ConnectionClosed));
        }
    }

    fn lock_inner(&self) -> std::sync::MutexGuard<'_, ModuleHandleState> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn next_module_control_corr(inner: &mut ModuleHandleState) -> u64 {
    loop {
        let corr = inner.next_corr;
        inner.next_corr = inner.next_corr.wrapping_add(1).max(HELLO_CORR + 1);
        if corr != HELLO_CORR && !inner.pending_catalog_updates.contains_key(&corr) {
            return corr;
        }
    }
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

    /// Decide a route.bind. The default accepts every route.
    async fn on_bind(&self, _req: &RouteBindRequest) -> BindDecision {
        BindDecision::accept()
    }

    /// Return cheap in-memory health for the module. The default reports healthy.
    async fn health(&self) -> HealthReport {
        HealthReport::ok()
    }

    /// A route channel was torn down by a per-route GOODBYE. The default is a no-op.
    async fn on_route_gone(&self, _channel: u16) {}
}

/// The terminal result of a module request handler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandlerOutcome {
    /// Send a Response frame carrying these bytes.
    Response(Vec<u8>),
    /// Send an Error frame carrying an [`ErrorBody`] with this code and message.
    Error { code: String, message: String },
    /// The handler emitted stream data with [`RequestCtx::emit`]; the serve code
    /// sends the StreamEnd terminal frame.
    Streamed,
}

/// Per-request context. Provides the route channel and correlation id, a way to
/// emit interim stream data, and a cancellation signal.
#[derive(Clone)]
pub struct RequestCtx {
    channel: u16,
    corr: u64,
    ver: u8,
    egress: mpsc::Sender<Frame>,
    cancelled: CancellationToken,
}

impl RequestCtx {
    /// Route channel for this request.
    pub fn channel(&self) -> u16 {
        self.channel
    }

    /// Correlation id for this request.
    pub fn corr(&self) -> u64 {
        self.corr
    }

    /// Emit an interim StreamData frame on this request's `(channel, corr)`. Once
    /// the request is cancelled or its route is gone, late emits are dropped.
    pub async fn emit(&self, body: Vec<u8>) -> Result<(), SubcModuleError> {
        if self.cancelled.is_cancelled() {
            return Ok(());
        }
        self.send_frame(FrameType::StreamData, data_flags(), body)
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
        let frame =
            Frame::build_with_version(self.ver, frame_type, flags, self.channel, self.corr, body)
                .map_err(SubcModuleError::FrameBuild)?;
        send_outbound(&self.egress, frame).await
    }
}

/// Route-bind request delivered on channel 0.
#[derive(Debug, Clone)]
pub struct RouteBindRequest {
    pub route_channel: u16,
    pub target: RouteTarget,
    pub identity: BindIdentity,
    pub principal: Option<Principal>,
    /// Consumer-declared reverse-request capabilities for this bind. This is a
    /// declaration, not a verified privilege; providers treat an absent field as
    /// no reverse-request capability. Known MCP method-family values today are
    /// "elicitation", "sampling", and "roots".
    pub consumer_capabilities: Option<Vec<String>>,
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

    let handle = ModuleHandle::new(&ack, tx.clone());
    let serve_handle = handle.clone();
    let serve_future = Box::pin(async move {
        let loop_result =
            module_loop(read_half, tx, Arc::clone(&handler), serve_handle.clone()).await;
        serve_handle.close_connection();

        let writer_result = writer.await.map_err(SubcModuleError::WriterTask);
        match (loop_result, writer_result) {
            (Err(loop_err), _) => Err(loop_err),
            (Ok(()), Ok(Ok(()))) => Ok(()),
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
        let Some(frame) = read_frame(&mut reader)
            .await
            .map_err(SubcModuleError::FrameIo)?
        else {
            return Ok(());
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
    match frame.header.ty {
        FrameType::Ping if frame.header.channel == 0 => {
            let pong = Frame::build_with_version(
                frame.header.ver,
                FrameType::Pong,
                frame.header.flags,
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
            cancel_channel(&dispatcher.in_flight, frame.header.channel)?;
            handler.on_route_gone(frame.header.channel).await;
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
            handle_control_request(frame, egress, handler, dispatcher).await?;
            Ok(true)
        }
        FrameType::Request => {
            spawn_data_request(frame, egress.clone(), handler, dispatcher)?;
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
) -> Result<(), SubcModuleError>
where
    H: ModuleHandler,
{
    let channel = frame.header.channel;
    let corr = frame.header.corr;
    let cancellation = CancellationToken::new();
    {
        let mut guard = lock_in_flight(&dispatcher.in_flight)?;
        guard.insert((channel, corr), cancellation.clone());
    }

    let ctx = RequestCtx {
        channel,
        corr,
        ver: frame.header.ver,
        egress,
        cancelled: cancellation,
    };
    let body = frame.body;
    let in_flight = Arc::clone(&dispatcher.in_flight);
    let permits = Arc::clone(&dispatcher.permits);
    tokio::spawn(async move {
        let Ok(_permit) = permits.acquire_owned().await else {
            if let Ok(mut guard) = in_flight.lock() {
                guard.remove(&(channel, corr));
            }
            return;
        };
        let outcome = handler.handle(ctx.clone(), body).await;
        let _ = send_handler_outcome(&ctx, outcome).await;
        if let Ok(mut guard) = in_flight.lock() {
            guard.remove(&(channel, corr));
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
    let corr = frame.header.corr;
    let ver = frame.header.ver;
    let cancellation = CancellationToken::new();
    {
        let mut guard = lock_in_flight(&dispatcher.in_flight)?;
        guard.insert((channel, corr), cancellation.clone());
    }

    let in_flight = Arc::clone(&dispatcher.in_flight);
    let permits = Arc::clone(&dispatcher.permits);
    tokio::spawn(async move {
        let Ok(_permit) = permits.acquire_owned().await else {
            if let Ok(mut guard) = in_flight.lock() {
                guard.remove(&(channel, corr));
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
                    corr,
                    body,
                ) {
                    let _ = send_outbound(&egress, frame).await;
                }
            }
        }
        if let Ok(mut guard) = in_flight.lock() {
            guard.remove(&(channel, corr));
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
            let body =
                serde_json::to_vec(&ErrorBody { code, message }).map_err(SubcModuleError::Json)?;
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
            .get(&(frame.header.channel, frame.header.corr))
            .cloned()
    };
    if let Some(cancellation) = cancellation {
        cancellation.cancel();
    }
    Ok(())
}

fn cancel_channel(in_flight: &InFlight, channel: u16) -> Result<(), SubcModuleError> {
    let cancelled = {
        let mut guard = lock_in_flight(in_flight)?;
        let keys = guard
            .keys()
            .copied()
            .filter(|(request_channel, _)| *request_channel == channel)
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
) -> Result<(), SubcModuleError>
where
    H: ModuleHandler,
{
    let request = serde_json::from_slice::<ModuleControlRequest>(&frame.body)
        .map_err(SubcModuleError::Json)?;
    match request {
        ModuleControlRequest::RouteBind {
            route_channel,
            target,
            identity,
            principal,
            consumer_capabilities,
        } => {
            let req = RouteBindRequest {
                route_channel,
                target,
                identity,
                principal,
                consumer_capabilities,
            };
            let decision = handler.on_bind(&req).await;
            match decision.kind {
                BindDecisionKind::Accept => {
                    let body = serde_json::to_vec(&ModuleControlResponse::RouteBindAck {})
                        .map_err(SubcModuleError::Json)?;
                    let response = Frame::build_with_version(
                        frame.header.ver,
                        FrameType::Response,
                        control_flags(),
                        0,
                        frame.header.corr,
                        body,
                    )
                    .map_err(SubcModuleError::FrameBuild)?;
                    send_outbound(egress, response).await?;
                }
                BindDecisionKind::Reject { code, message } => {
                    let body = serde_json::to_vec(&ErrorBody { code, message })
                        .map_err(SubcModuleError::Json)?;
                    let response = Frame::build_with_version(
                        frame.header.ver,
                        FrameType::Error,
                        control_flags(),
                        0,
                        frame.header.corr,
                        body,
                    )
                    .map_err(SubcModuleError::FrameBuild)?;
                    send_outbound(egress, response).await?;
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
        launch_nonce: env::var(SUBC_LAUNCH_NONCE_ENV)
            .ok()
            .filter(|value| !value.is_empty()),
    })
    .map_err(SubcModuleError::Json)?;
    let frame = Frame::build(FrameType::Hello, control_flags(), 0, HELLO_CORR, body)
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
    let conn = connection_file::read(connection_file_path).map_err(|source| {
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
        (ModuleHandle::new(&ack, tx), rx)
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
        assert_eq!(
            serde_json::from_slice::<ModuleControlResponse>(&response.body).unwrap(),
            ModuleControlResponse::from(HealthReport::ok())
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
}
