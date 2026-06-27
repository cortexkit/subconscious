#![forbid(unsafe_code)]

pub mod consumer;
pub use consumer::{
    CallError, CallOptions, CloseRouteOptions, ConnectionState, ConsumerError, ConsumerOptions,
    RetryBackoff, SubcConsumer,
};

use std::{
    collections::HashMap,
    env,
    error::Error,
    ffi::OsString,
    fmt, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

pub use async_trait::async_trait;
use subc_protocol::{
    manifest::ModuleManifest,
    session::{ModuleControlRequest, ModuleControlResponse},
    BindIdentity, ErrorBody, Flags, Frame, FrameBuildError, FrameType, ModuleHelloAckBody,
    ModuleHelloBody, Priority, RouteTarget, PROTOCOL_VERSION, SUBC_LAUNCH_NONCE_ENV,
    SUBC_MODULE_ID_ENV,
};
use subc_transport::{
    authenticate_client, connection_file, read_frame, write_frame, AuthError, ConnectionFileError,
    FrameIoError,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufWriter},
    net::TcpStream,
    sync::mpsc,
};
use tokio_util::sync::{CancellationToken, WaitForCancellationFuture};

const AUTH_DEADLINE: Duration = Duration::from_secs(2);
const EGRESS_BUFFER: usize = 64;
const HELLO_CORR: u64 = 1;

type RequestKey = (u16, u64);
type InFlight = Arc<Mutex<HashMap<RequestKey, CancellationToken>>>;

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
    let stream = connect_to_subc(connection_file).await?;
    let (mut read_half, write_half) = tokio::io::split(stream);
    let (tx, rx) = mpsc::channel::<Frame>(EGRESS_BUFFER);
    let writer = tokio::spawn(drain_writer(write_half, rx));
    let handler = Arc::new(handler);

    let loop_result = module_loop(&mut read_half, tx.clone(), manifest, Arc::clone(&handler)).await;
    drop(tx);

    let writer_result = writer.await.map_err(SubcModuleError::WriterTask);
    match (loop_result, writer_result) {
        (Err(loop_err), _) => Err(loop_err),
        (Ok(()), Ok(Ok(()))) => Ok(()),
        (Ok(()), Ok(Err(writer_err))) => Err(SubcModuleError::FrameIo(writer_err)),
        (Ok(()), Err(join_err)) => Err(join_err),
    }
}

async fn module_loop<R, H>(
    reader: &mut R,
    egress: mpsc::Sender<Frame>,
    manifest: ModuleManifest,
    handler: Arc<H>,
) -> Result<(), SubcModuleError>
where
    R: AsyncRead + Unpin,
    H: ModuleHandler,
{
    send_hello(&egress, manifest).await?;
    let ack = expect_hello_ack(reader).await?;
    handler.on_hello_ack(&ack).await;

    let in_flight = Arc::new(Mutex::new(HashMap::new()));
    loop {
        let Some(frame) = read_frame(reader).await.map_err(SubcModuleError::FrameIo)? else {
            return Ok(());
        };
        if !handle_frame(frame, &egress, Arc::clone(&handler), Arc::clone(&in_flight)).await? {
            return Ok(());
        }
    }
}

async fn handle_frame<H>(
    frame: Frame,
    egress: &mpsc::Sender<Frame>,
    handler: Arc<H>,
    in_flight: InFlight,
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
            cancel_channel(&in_flight, frame.header.channel)?;
            handler.on_route_gone(frame.header.channel).await;
            Ok(true)
        }
        FrameType::Cancel => {
            handle_cancel(frame, &in_flight)?;
            Ok(true)
        }
        FrameType::Request if frame.header.channel == 0 => {
            handle_control_request(frame, egress, handler).await?;
            Ok(true)
        }
        FrameType::Request => {
            spawn_data_request(frame, egress.clone(), handler, in_flight)?;
            Ok(true)
        }
        _ => Ok(true),
    }
}

fn spawn_data_request<H>(
    frame: Frame,
    egress: mpsc::Sender<Frame>,
    handler: Arc<H>,
    in_flight: InFlight,
) -> Result<(), SubcModuleError>
where
    H: ModuleHandler,
{
    let channel = frame.header.channel;
    let corr = frame.header.corr;
    let cancellation = CancellationToken::new();
    {
        let mut guard = lock_in_flight(&in_flight)?;
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
    tokio::spawn(async move {
        let outcome = handler.handle(ctx.clone(), body).await;
        let _ = send_handler_outcome(&ctx, outcome).await;
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
        } => {
            let req = RouteBindRequest {
                route_channel,
                target,
                identity,
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
        control_ops: None,
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
