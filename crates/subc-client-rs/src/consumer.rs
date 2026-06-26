use std::{
    collections::HashMap,
    error::Error,
    fmt, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use subc_control::{ClientControlRequest, ClientControlResponse};
use subc_protocol::{
    BindIdentity, ErrorBody, Flags, Frame, FrameBuildError, FrameType, Priority, RouteTarget,
};
use subc_transport::{
    authenticate_client, connection_file, read_frame, write_frame, AuthError, ConnectionFileError,
    FrameIoError,
};
use tokio::{
    io::{AsyncWriteExt, BufWriter},
    net::{tcp::OwnedReadHalf, tcp::OwnedWriteHalf, TcpStream},
    sync::{mpsc, oneshot, Notify, Semaphore},
    task::JoinHandle,
    time::{sleep, Instant},
};
use tokio_util::sync::CancellationToken;

const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);
const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_ROUTE_RETRY_DEADLINE: Duration = Duration::from_secs(10);
const DEFAULT_RESTORED_DEBOUNCE: Duration = Duration::from_millis(250);
const EGRESS_BUFFER: usize = 128;
const DEFAULT_ROUTE_WINDOW: usize = 1024;

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

/// Per-call options for [`SubcConsumer::call`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallOptions {
    /// Deadline for the whole managed call, including route-open retry and the response wait.
    pub timeout: Duration,
    pub priority: Priority,
    pub route_retry: RetryBackoff,
    /// Maximum real-time limit for retrying route.open attempts when the target is temporarily absent.
    pub route_retry_deadline: Duration,
}

impl Default for CallOptions {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_CALL_TIMEOUT,
            priority: Priority::Interactive,
            route_retry: RetryBackoff::default(),
            route_retry_deadline: DEFAULT_ROUTE_RETRY_DEADLINE,
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
        let route_key = RouteKey::new(&target, &identity);

        loop {
            let route = self
                .shared
                .ensure_route(&route_key, &target, &identity, &opts, call_deadline)
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

/// Managed call failure that distinguishes whether the request body was sent.
#[derive(Debug)]
pub enum CallError {
    /// The request body was not accepted by the writer path, or route.open failed before data send.
    NotSent(Box<dyn Error + Send + Sync>),
    /// The request body was accepted by the writer path, but no terminal response was observed.
    OutcomeUnknown(Box<dyn Error + Send + Sync>),
    /// The target module handler returned an Error frame. Application-level rejections
    /// are returned as ordinary successful response bytes and do not produce this variant.
    Module(ErrorBody),
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
}

impl fmt::Display for CallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotSent(err) => write!(f, "request not sent: {err}"),
            Self::OutcomeUnknown(err) => write!(f, "request outcome unknown: {err}"),
            Self::Module(body) => write!(f, "module error {}: {}", body.code, body.message),
        }
    }
}

impl Error for CallError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::NotSent(err) | Self::OutcomeUnknown(err) => Some(err.as_ref()),
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
    openings: HashMap<RouteKey, Vec<OpeningWaiter>>,
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
        target: &RouteTarget,
        identity: &BindIdentity,
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
                if let Some(waiters) = inner.openings.get_mut(key) {
                    let (tx, rx) = oneshot::channel();
                    waiters.push(tx);
                    RouteOpenAction::Wait(rx)
                } else {
                    inner.openings.insert(key.clone(), Vec::new());
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
                        .open_route_with_retry(key, target, identity, opts, call_deadline)
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
        target: &RouteTarget,
        identity: &BindIdentity,
        opts: &CallOptions,
        call_deadline: Instant,
    ) -> Result<RouteState, CallError> {
        let route_deadline = (Instant::now() + opts.route_retry_deadline).min(call_deadline);
        let mut attempt = 0usize;
        loop {
            attempt = attempt.saturating_add(1);
            self.ensure_connected_for_call().await?;
            let body = serde_json::to_vec(&ClientControlRequest::RouteOpen {
                target: target.clone(),
                identity: identity.clone(),
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
                    let cached = {
                        let mut inner = self.lock_inner();
                        if inner.closed {
                            return Err(CallError::not_sent("consumer closed"));
                        }
                        if inner.generation != generation || inner.writer.is_none() {
                            None
                        } else {
                            Some(
                                inner
                                    .routes
                                    .entry(key.clone())
                                    .or_insert_with(|| route.clone())
                                    .clone(),
                            )
                        }
                    };
                    if let Some(cached) = cached {
                        return Ok(cached);
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
            inner.pending.insert(
                key,
                PendingEntry {
                    accepted: false,
                    tx,
                },
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
            let _ = entry.tx.send(PendingResult::Terminal(terminal));
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
                    .map(|(_, waiters)| waiters)
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
        let waiters = self.lock_inner().openings.remove(key).unwrap_or_default();
        for waiter in waiters {
            let _ = waiter.send(result.clone());
        }
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

enum RouteOpenAction {
    Wait(oneshot::Receiver<Result<RouteState, SharedCallFailure>>),
    Lead,
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
}

impl RouteKey {
    fn new(target: &RouteTarget, identity: &BindIdentity) -> Self {
        Self {
            target: RouteTargetKey::from(target),
            project_root: identity.project_root.clone(),
            harness: identity.harness.clone(),
            session: identity.session.clone(),
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
    tx: oneshot::Sender<PendingResult>,
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
        FrameType::StreamData | FrameType::Push => {}
        FrameType::Goodbye if frame.header.channel == 0 => {
            shared.handle_generation_drop(generation, "subc sent GOODBYE".to_string());
            return false;
        }
        FrameType::Goodbye => {
            shared.invalidate_routes_for_channel(generation, frame.header.channel);
            let pending = {
                let mut inner = shared.lock_inner();
                drain_pending_channel(&mut inner.pending, generation, frame.header.channel)
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

fn classify_failure(accepted: bool, reason: impl Into<String>) -> CallError {
    if accepted {
        CallError::outcome_unknown(reason)
    } else {
        CallError::not_sent(reason)
    }
}

fn settle_pending_entries(entries: Vec<PendingEntry>, reason: String) {
    for entry in entries {
        let _ = entry.tx.send(PendingResult::Failure {
            accepted: entry.accepted,
            reason: reason.clone(),
        });
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
) -> Vec<PendingEntry> {
    let keys = pending
        .keys()
        .copied()
        .filter(|key| key.generation == generation && key.channel == channel)
        .collect::<Vec<_>>();
    keys.into_iter()
        .filter_map(|key| pending.remove(&key))
        .collect()
}

fn drain_openings(openings: &mut HashMap<RouteKey, Vec<OpeningWaiter>>) -> Vec<Vec<OpeningWaiter>> {
    openings.drain().map(|(_, waiters)| waiters).collect()
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
        ConsumerError::NoEndpoint { .. } | ConsumerError::Auth { .. } | ConsumerError::Closed => {
            false
        }
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
        let key = RouteKey::new(&target, &identity);
        assert_eq!(key.project_root, PathBuf::from("/tmp/project"));
        assert!(matches!(key.target, RouteTargetKey::InternalService { .. }));
    }
}
