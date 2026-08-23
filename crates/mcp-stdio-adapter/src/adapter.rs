use std::{
    collections::BTreeMap,
    env,
    ffi::OsString,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde_json::{json, Map, Value};
use subc_client_rs::{
    async_trait, CallOptions, ConsumerIdentity, ConsumerOptions, HandlerOutcome, ModuleHandler,
    RequestCtx, SubcConsumer,
};
use subc_protocol::{
    session::{HealthReport, HealthStatus},
    BindIdentity, RouteTarget,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::Mutex as AsyncMutex,
    time::{sleep, timeout},
};

use crate::{
    constants::{
        BASE_ENV_KEYS, DEFAULT_MAX_CHILDREN, EVICTION_GRACE_MS, SPAWN_ATTEMPT_BUDGET,
        SPAWN_INITIALIZE_BUDGET_MS, SPAWN_RETRY_COOLDOWN_MS,
    },
    registry::{EnvironmentValue, ServerConfig, ServerRegistry},
};

const BAD_REQUEST: &str = "bad_request";
const CLAUSTRUM_MODULE_ID: &str = "claustrum";
const SPAWN_SHAPED_FIELDS: &[&str] = &[
    "command",
    "argv",
    "args",
    "cwd",
    "env",
    "spawn",
    "spawn_spec",
];

/// Atomics let health checks report lifecycle state without blocking on child state.
#[derive(Debug)]
pub struct HealthMetrics {
    children_live: AtomicU64,
    children_max: AtomicU64,
    spawns_total: AtomicU64,
    spawn_failures_total: AtomicU64,
    idle_evictions_total: AtomicU64,
    calls_in_flight: AtomicU64,
    oldest_in_flight_ms: AtomicU64,
    cache_served_total: AtomicU64,
}

impl Default for HealthMetrics {
    fn default() -> Self {
        Self {
            children_live: AtomicU64::new(0),
            children_max: AtomicU64::new(DEFAULT_MAX_CHILDREN),
            spawns_total: AtomicU64::new(0),
            spawn_failures_total: AtomicU64::new(0),
            idle_evictions_total: AtomicU64::new(0),
            calls_in_flight: AtomicU64::new(0),
            oldest_in_flight_ms: AtomicU64::new(0),
            cache_served_total: AtomicU64::new(0),
        }
    }
}

impl HealthMetrics {
    pub fn snapshot(&self) -> Value {
        json!({
            "children_live": self.children_live.load(Ordering::Relaxed),
            "children_max": self.children_max.load(Ordering::Relaxed),
            "spawns_total": self.spawns_total.load(Ordering::Relaxed),
            "spawn_failures_total": self.spawn_failures_total.load(Ordering::Relaxed),
            "idle_evictions_total": self.idle_evictions_total.load(Ordering::Relaxed),
            "calls_in_flight": self.calls_in_flight.load(Ordering::Relaxed),
            "oldest_in_flight_ms": oldest_in_flight_age_ms(
                self.calls_in_flight.load(Ordering::Relaxed),
                self.oldest_in_flight_ms.load(Ordering::Relaxed),
            ),
            "cache_served_total": self.cache_served_total.load(Ordering::Relaxed),
        })
    }
}

/// Resolves a configured credential handle immediately before a child is spawned.
#[async_trait]
pub trait CredentialResolver: Send + Sync {
    async fn resolve(&self, handle: &str) -> Result<String, CredentialResolutionError>;
}

/// Resolver used when no handle-backed variables were configured.
struct RejectingCredentialResolver;

#[async_trait]
impl CredentialResolver for RejectingCredentialResolver {
    async fn resolve(&self, _handle: &str) -> Result<String, CredentialResolutionError> {
        Err(CredentialResolutionError)
    }
}

/// A route-plane consumer for claustrum's possession-only `credential.get` surface.
///
/// A new read is made for every spawn so a shed child never causes an old credential
/// value to be reused by its replacement.
pub struct ClaustrumCredentialResolver {
    connection_file: PathBuf,
    consumer_identity: ConsumerIdentity,
}

impl ClaustrumCredentialResolver {
    pub fn new(connection_file: PathBuf, consumer_identity: ConsumerIdentity) -> Self {
        Self {
            connection_file,
            consumer_identity,
        }
    }
}

#[async_trait]
impl CredentialResolver for ClaustrumCredentialResolver {
    async fn resolve(&self, handle: &str) -> Result<String, CredentialResolutionError> {
        let consumer = SubcConsumer::connect(&self.connection_file, ConsumerOptions::default())
            .await
            .map_err(|_| CredentialResolutionError)?;
        let body = serde_json::to_vec(&json!({
            "method": "credential.get",
            "params": { "handle": handle },
        }))
        .map_err(|_| CredentialResolutionError)?;
        let identity = BindIdentity {
            project_root: env::current_dir().map_err(|_| CredentialResolutionError)?,
            harness: "mcp-stdio-adapter".to_string(),
            session: "credential-resolution".to_string(),
        };
        let reply = consumer
            .call(
                RouteTarget::ManagementSurface {
                    module_id: CLAUSTRUM_MODULE_ID.to_string(),
                },
                identity,
                body,
                CallOptions {
                    consumer_identity: Some(self.consumer_identity.clone()),
                    ..CallOptions::default()
                },
            )
            .await
            .map_err(|_| CredentialResolutionError)?;
        consumer.close().await;

        let parsed: Value =
            serde_json::from_slice(&reply).map_err(|_| CredentialResolutionError)?;
        parsed
            .get("payload")
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .ok_or(CredentialResolutionError)
    }
}

/// The resolver deliberately reveals neither the handle nor the secret value.
#[derive(Debug, Clone, Copy)]
pub struct CredentialResolutionError;

/// Internal timing controls. Production construction uses the settled constants;
/// tests inject short durations without introducing an operator-facing config knob.
#[derive(Debug, Clone)]
pub struct LifecycleSettings {
    pub spawn_initialize_budget: Duration,
    pub spawn_attempt_budget: u64,
    pub spawn_retry_cooldown: Duration,
    pub eviction_grace: Duration,
    pub idle_ttl_override: Option<Duration>,
}

impl Default for LifecycleSettings {
    fn default() -> Self {
        Self {
            spawn_initialize_budget: Duration::from_millis(SPAWN_INITIALIZE_BUDGET_MS),
            spawn_attempt_budget: SPAWN_ATTEMPT_BUDGET,
            spawn_retry_cooldown: Duration::from_millis(SPAWN_RETRY_COOLDOWN_MS),
            eviction_grace: Duration::from_millis(EVICTION_GRACE_MS),
            idle_ttl_override: None,
        }
    }
}

pub struct AdapterHandler {
    metrics: Arc<HealthMetrics>,
    registry: ServerRegistry,
    lifecycle: Arc<ChildLifecycle>,
}

impl AdapterHandler {
    pub fn new(registry: ServerRegistry) -> Self {
        Self::with_resolver(
            registry,
            Arc::new(RejectingCredentialResolver),
            LifecycleSettings::default(),
        )
    }

    pub fn with_resolver(
        registry: ServerRegistry,
        resolver: Arc<dyn CredentialResolver>,
        settings: LifecycleSettings,
    ) -> Self {
        let metrics = Arc::new(HealthMetrics::default());
        Self {
            metrics: Arc::clone(&metrics),
            registry,
            lifecycle: Arc::new(ChildLifecycle::new(metrics, resolver, settings)),
        }
    }

    pub fn metrics(&self) -> &Arc<HealthMetrics> {
        &self.metrics
    }

    /// Processes one route envelope without requiring a daemon RequestCtx. This keeps
    /// real-child lifecycle tests focused on the adapter boundary rather than the daemon.
    pub async fn route_outcome(&self, body: &[u8]) -> HandlerOutcome {
        let request = match parse_envelope(body) {
            Ok(request) => request,
            Err(error) => return error.into_handler_outcome(),
        };
        let Some(config) = self.registry.servers().get(&request.server).cloned() else {
            return AdapterRefusal::with_detail(
                "server_unknown",
                "MCP server is not configured",
                json!({}),
            )
            .into_handler_outcome();
        };
        if config.disabled {
            return AdapterRefusal::with_detail(
                "server_disabled",
                "MCP server is disabled",
                json!({}),
            )
            .into_handler_outcome();
        }

        if request.op == Operation::ToolsList && config.cache_tools_list {
            if let Some(cached) = self.lifecycle.cached_tools(&request.server) {
                self.metrics
                    .cache_served_total
                    .fetch_add(1, Ordering::Relaxed);
                return success_outcome("cache", cached.observed_at_ms, cached.payload, None);
            }
        }

        let _flight = FlightGuard::new(Arc::clone(&self.metrics));
        match self
            .lifecycle
            .forward(&request.server, config, request.op, request.payload)
            .await
        {
            Ok(forwarded) => {
                if request.op == Operation::ToolsList && forwarded.cacheable {
                    self.lifecycle.cache_tools(
                        &request.server,
                        CachedTools {
                            payload: forwarded.payload.clone(),
                            observed_at_ms: forwarded.observed_at_ms,
                        },
                    );
                }
                success_outcome(
                    "live",
                    forwarded.observed_at_ms,
                    forwarded.payload,
                    forwarded.spawn_elapsed_ms,
                )
            }
            Err(error) => error.into_handler_outcome(),
        }
    }
}

#[async_trait]
impl ModuleHandler for AdapterHandler {
    async fn handle(&self, _ctx: RequestCtx, body: Vec<u8>) -> HandlerOutcome {
        self.route_outcome(&body).await
    }

    async fn health(&self) -> HealthReport {
        // This lane reads only atomics. It neither waits on child state nor executes a
        // subprocess, so a wedged spawn or teardown cannot delay a health response.
        HealthReport {
            status: HealthStatus::Ok,
            detail: Some("stdio MCP child lifecycle metrics".to_string()),
            metrics: Some(self.metrics.snapshot()),
        }
    }
}

struct ChildLifecycle {
    metrics: Arc<HealthMetrics>,
    resolver: Arc<dyn CredentialResolver>,
    settings: LifecycleSettings,
    slots: Mutex<BTreeMap<String, Arc<ServerSlot>>>,
    cached_tools: Mutex<BTreeMap<String, CachedTools>>,
}

impl ChildLifecycle {
    fn new(
        metrics: Arc<HealthMetrics>,
        resolver: Arc<dyn CredentialResolver>,
        settings: LifecycleSettings,
    ) -> Self {
        Self {
            metrics,
            resolver,
            settings,
            slots: Mutex::new(BTreeMap::new()),
            cached_tools: Mutex::new(BTreeMap::new()),
        }
    }

    fn slot(&self, server: &str) -> Arc<ServerSlot> {
        let mut slots = self
            .slots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Arc::clone(
            slots
                .entry(server.to_string())
                .or_insert_with(|| Arc::new(ServerSlot::default())),
        )
    }

    fn cached_tools(&self, server: &str) -> Option<CachedTools> {
        self.cached_tools
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(server)
            .cloned()
    }

    fn cache_tools(&self, server: &str, cached: CachedTools) {
        self.cached_tools
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(server.to_string(), cached);
    }

    async fn forward(
        self: &Arc<Self>,
        server: &str,
        config: ServerConfig,
        operation: Operation,
        payload: Value,
    ) -> Result<ForwardedResponse, LifecycleError> {
        let attempts = if operation == Operation::ToolsList {
            2
        } else {
            1
        };
        for attempt in 0..attempts {
            match self
                .forward_once(server, &config, operation, payload.clone())
                .await
            {
                Err(LifecycleError::CallOutcomeUnknown) if attempt + 1 < attempts => continue,
                result => return result,
            }
        }
        unreachable!("the retry loop always returns its final attempt")
    }

    async fn forward_once(
        self: &Arc<Self>,
        server: &str,
        config: &ServerConfig,
        _operation: Operation,
        payload: Value,
    ) -> Result<ForwardedResponse, LifecycleError> {
        let slot = self.slot(server);
        let mut state = slot.state.lock().await;
        let spawned_at = self.ensure_child(server, config, &mut state).await?;
        let session = state
            .session
            .as_mut()
            .expect("successful ensure_child installs a session");
        let child_id = session.next_id;
        session.next_id = session.next_id.saturating_add(1);
        let request = child_request(payload, child_id);
        if write_json_line(session.stdin.as_mut(), &request)
            .await
            .is_err()
        {
            self.remove_session(&mut state).await;
            return Err(LifecycleError::CallOutcomeUnknown);
        }

        let response = match read_response(session, child_id, config.frame_ceiling_bytes).await {
            Ok(response) => response,
            Err(FrameReadError::Framing { observed_bytes }) => {
                self.remove_session(&mut state).await;
                return Err(LifecycleError::ChildFraming {
                    observed_bytes,
                    ceiling_bytes: config.frame_ceiling_bytes,
                });
            }
            Err(FrameReadError::Closed | FrameReadError::TimedOut | FrameReadError::Io) => {
                self.remove_session(&mut state).await;
                return Err(LifecycleError::CallOutcomeUnknown);
            }
        };
        let cacheable = response.get("result").is_some();
        let Some(payload) = child_payload(response) else {
            self.remove_session(&mut state).await;
            return Err(LifecycleError::ChildFraming {
                observed_bytes: 0,
                ceiling_bytes: config.frame_ceiling_bytes,
            });
        };
        let generation = session.generation;
        session.last_idle = Instant::now();
        let ttl = self
            .settings
            .idle_ttl_override
            .unwrap_or_else(|| Duration::from_millis(config.idle_ttl_ms));
        self.schedule_idle_eviction(Arc::clone(&slot), generation, ttl);

        Ok(ForwardedResponse {
            payload,
            cacheable,
            observed_at_ms: epoch_millis(),
            spawn_elapsed_ms: spawned_at.map(elapsed_since),
        })
    }

    async fn ensure_child(
        self: &Arc<Self>,
        _server: &str,
        config: &ServerConfig,
        state: &mut SlotState,
    ) -> Result<Option<Instant>, LifecycleError> {
        if let Some(session) = state.session.as_mut() {
            match session.child.try_wait() {
                Ok(Some(_)) | Err(_) => {
                    self.remove_session(state).await;
                }
                Ok(None) => return Ok(None),
            }
        }

        let now = Instant::now();
        if let Some(until) = state.cooldown_until {
            if until > now {
                return Err(LifecycleError::SpawnFailed {
                    cause: state.last_failure_cause.unwrap_or(SpawnFailureCause::Exec),
                    retry_after_ms: remaining_ms(until),
                    env_var: None,
                });
            }
            state.cooldown_until = None;
        }

        let child_env = match self.construct_environment(config).await {
            Ok(environment) => environment,
            Err(variable) => {
                let retry_after_ms =
                    self.record_failed_attempt(state, SpawnFailureCause::CredentialResolution);
                return Err(LifecycleError::SpawnFailed {
                    cause: SpawnFailureCause::CredentialResolution,
                    retry_after_ms,
                    env_var: Some(variable),
                });
            }
        };
        let spawn_started = Instant::now();
        let mut command = Command::new(&config.command);
        command
            .args(&config.args)
            .env_clear()
            .envs(child_env)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        if let Some(cwd) = &config.cwd {
            command.current_dir(cwd);
        }
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(_) => {
                let retry_after_ms = self.record_failed_attempt(state, SpawnFailureCause::Exec);
                return Err(LifecycleError::SpawnFailed {
                    cause: SpawnFailureCause::Exec,
                    retry_after_ms,
                    env_var: None,
                });
            }
        };
        self.metrics.spawns_total.fetch_add(1, Ordering::Relaxed);
        self.metrics.children_live.fetch_add(1, Ordering::Relaxed);
        let stdin = child.stdin.take().expect("piped stdin is present");
        let stdout = child.stdout.take().expect("piped stdout is present");
        let generation = state.next_generation;
        state.next_generation = state.next_generation.saturating_add(1);
        let mut session = ChildSession {
            child,
            stdin: Some(stdin),
            stdout: BufReader::new(stdout),
            generation,
            next_id: 1,
            last_idle: Instant::now(),
        };

        if let Err(_error) = initialize_child(
            &mut session,
            config.frame_ceiling_bytes,
            self.settings.spawn_initialize_budget,
        )
        .await
        {
            self.terminate_session(&mut session).await;
            let _ = self.record_failed_attempt(state, SpawnFailureCause::InitializeTimeout);
            return Err(LifecycleError::InitializeFailed);
        }

        state.consecutive_failures = 0;
        state.cooldown_until = None;
        state.last_failure_cause = None;
        state.session = Some(session);
        Ok(Some(spawn_started))
    }

    async fn construct_environment(
        &self,
        config: &ServerConfig,
    ) -> Result<BTreeMap<OsString, OsString>, String> {
        let mut environment = BTreeMap::new();
        for key in BASE_ENV_KEYS {
            if let Some(value) = env::var_os(key) {
                environment.insert(OsString::from(key), value);
            }
        }
        for (variable, value) in &config.env {
            let value = match value {
                EnvironmentValue::Literal(value) => OsString::from(value),
                EnvironmentValue::Handle(handle) => self
                    .resolver
                    .resolve(handle)
                    .await
                    .map(OsString::from)
                    .map_err(|_| variable.clone())?,
            };
            environment.insert(OsString::from(variable), value);
        }
        Ok(environment)
    }

    fn record_failed_attempt(&self, state: &mut SlotState, cause: SpawnFailureCause) -> u64 {
        self.metrics
            .spawn_failures_total
            .fetch_add(1, Ordering::Relaxed);
        state.consecutive_failures = state.consecutive_failures.saturating_add(1);
        state.last_failure_cause = Some(cause);
        if state.consecutive_failures >= self.settings.spawn_attempt_budget {
            let until = Instant::now() + self.settings.spawn_retry_cooldown;
            state.cooldown_until = Some(until);
            remaining_ms(until)
        } else {
            0
        }
    }

    async fn remove_session(&self, state: &mut SlotState) {
        if let Some(mut session) = state.session.take() {
            self.terminate_session(&mut session).await;
        }
    }

    async fn terminate_session(&self, session: &mut ChildSession) {
        session.stdin.take();
        let waited = timeout(self.settings.eviction_grace, session.child.wait()).await;
        if !matches!(waited, Ok(Ok(_))) {
            let _ = session.child.start_kill();
            let _ = session.child.wait().await;
        }
        self.metrics.children_live.fetch_sub(1, Ordering::Relaxed);
    }

    fn schedule_idle_eviction(
        self: &Arc<Self>,
        slot: Arc<ServerSlot>,
        generation: u64,
        ttl: Duration,
    ) {
        let lifecycle = Arc::clone(self);
        tokio::spawn(async move {
            // A zero duration exists only in the injected test clock. Production TTLs
            // are normalized by the registry, so skipping the timer yield here cannot
            // make an operator-configured child immediately idle-eligible.
            if !ttl.is_zero() {
                sleep(ttl).await;
            }
            let mut state = slot.state.lock().await;
            let eligible = state.session.as_ref().is_some_and(|session| {
                session.generation == generation && session.last_idle.elapsed() >= ttl
            });
            if eligible {
                lifecycle.remove_session(&mut state).await;
                lifecycle
                    .metrics
                    .idle_evictions_total
                    .fetch_add(1, Ordering::Relaxed);
            }
        });
    }
}

#[derive(Default)]
struct ServerSlot {
    state: AsyncMutex<SlotState>,
}

#[derive(Default)]
struct SlotState {
    session: Option<ChildSession>,
    next_generation: u64,
    consecutive_failures: u64,
    cooldown_until: Option<Instant>,
    last_failure_cause: Option<SpawnFailureCause>,
}

struct ChildSession {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    generation: u64,
    next_id: u64,
    last_idle: Instant,
}

#[derive(Clone)]
struct CachedTools {
    payload: Value,
    observed_at_ms: u64,
}

struct ForwardedResponse {
    payload: Value,
    cacheable: bool,
    observed_at_ms: u64,
    spawn_elapsed_ms: Option<u64>,
}

#[derive(Debug)]
enum LifecycleError {
    SpawnFailed {
        cause: SpawnFailureCause,
        retry_after_ms: u64,
        env_var: Option<String>,
    },
    InitializeFailed,
    ChildFraming {
        observed_bytes: u64,
        ceiling_bytes: u64,
    },
    CallOutcomeUnknown,
}

impl LifecycleError {
    fn into_handler_outcome(self) -> HandlerOutcome {
        match self {
            Self::SpawnFailed {
                cause,
                retry_after_ms,
                env_var,
            } => {
                let mut detail = json!({
                    "cause": cause.as_str(),
                    "retry_after_ms": retry_after_ms,
                });
                if let Some(variable) = env_var {
                    detail["env_var"] = Value::String(variable);
                }
                AdapterRefusal::with_detail("spawn_failed", "MCP child spawn failed", detail)
                    .into_handler_outcome()
            }
            Self::InitializeFailed => AdapterRefusal::with_detail(
                "initialize_failed",
                "MCP child initialize failed",
                json!({}),
            )
            .into_handler_outcome(),
            Self::ChildFraming {
                observed_bytes,
                ceiling_bytes,
            } => AdapterRefusal::with_detail(
                "child_framing_error",
                "MCP child emitted an invalid or oversized frame",
                json!({
                    "observed_bytes": observed_bytes,
                    "ceiling_bytes": ceiling_bytes,
                }),
            )
            .into_handler_outcome(),
            Self::CallOutcomeUnknown => AdapterRefusal::with_detail(
                "call_outcome_unknown",
                "MCP child ended after the tool request was written",
                json!({}),
            )
            .into_handler_outcome(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum SpawnFailureCause {
    Exec,
    InitializeTimeout,
    CredentialResolution,
}

impl SpawnFailureCause {
    fn as_str(self) -> &'static str {
        match self {
            Self::Exec => "exec",
            Self::InitializeTimeout => "initialize_timeout",
            Self::CredentialResolution => "credential_resolution",
        }
    }
}

#[derive(Debug)]
enum FrameReadError {
    Framing { observed_bytes: u64 },
    Closed,
    TimedOut,
    Io,
}

async fn initialize_child(
    session: &mut ChildSession,
    ceiling_bytes: u64,
    budget: Duration,
) -> Result<(), FrameReadError> {
    let initialize = json!({
        "jsonrpc": "2.0",
        "id": 0,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "ck-mcp-stdio-adapter", "version": env!("CARGO_PKG_VERSION") },
        },
    });
    write_json_line(session.stdin.as_mut(), &initialize)
        .await
        .map_err(|_| FrameReadError::Io)?;
    let response = timeout(budget, read_response(session, 0, ceiling_bytes))
        .await
        .map_err(|_| FrameReadError::TimedOut)??;
    if child_payload(response).is_none() {
        return Err(FrameReadError::Framing { observed_bytes: 0 });
    }
    write_json_line(
        session.stdin.as_mut(),
        &json!({"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}}),
    )
    .await
    .map_err(|_| FrameReadError::Io)
}

async fn write_json_line(stdin: Option<&mut ChildStdin>, value: &Value) -> std::io::Result<()> {
    let stdin = stdin
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "child stdin closed"))?;
    let mut bytes = serde_json::to_vec(value).map_err(std::io::Error::other)?;
    bytes.push(b'\n');
    stdin.write_all(&bytes).await?;
    stdin.flush().await
}

async fn read_response(
    session: &mut ChildSession,
    expected_id: u64,
    ceiling_bytes: u64,
) -> Result<Value, FrameReadError> {
    loop {
        let frame = read_frame(&mut session.stdout, ceiling_bytes).await?;
        let parsed: Value =
            serde_json::from_slice(&frame).map_err(|_| FrameReadError::Framing {
                observed_bytes: frame.len() as u64,
            })?;
        if parsed.get("id").and_then(Value::as_u64) == Some(expected_id) {
            return Ok(parsed);
        }
    }
}

async fn read_frame(
    stdout: &mut BufReader<ChildStdout>,
    ceiling_bytes: u64,
) -> Result<Vec<u8>, FrameReadError> {
    let mut frame = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        match stdout.read(&mut byte).await {
            Ok(0) => return Err(FrameReadError::Closed),
            Ok(_) if byte[0] == b'\n' => return Ok(frame),
            Ok(_) => {
                frame.push(byte[0]);
                if frame.len() as u64 > ceiling_bytes {
                    return Err(FrameReadError::Framing {
                        observed_bytes: frame.len() as u64,
                    });
                }
            }
            Err(_) => return Err(FrameReadError::Io),
        }
    }
}

fn child_request(mut payload: Value, id: u64) -> Value {
    let object = payload
        .as_object_mut()
        .expect("validated route payload is an object");
    object.insert("jsonrpc".to_string(), Value::String("2.0".to_string()));
    object.insert("id".to_string(), Value::from(id));
    payload
}

fn child_payload(response: Value) -> Option<Value> {
    response
        .get("result")
        .cloned()
        .or_else(|| response.get("error").cloned())
}

struct FlightGuard {
    metrics: Arc<HealthMetrics>,
}

impl FlightGuard {
    fn new(metrics: Arc<HealthMetrics>) -> Self {
        if metrics.calls_in_flight.fetch_add(1, Ordering::Relaxed) == 0 {
            metrics
                .oldest_in_flight_ms
                .store(epoch_millis(), Ordering::Relaxed);
        }
        Self { metrics }
    }
}

impl Drop for FlightGuard {
    fn drop(&mut self) {
        if self.metrics.calls_in_flight.fetch_sub(1, Ordering::Relaxed) == 1 {
            self.metrics.oldest_in_flight_ms.store(0, Ordering::Relaxed);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Operation {
    ToolsList,
    ToolsCall,
}

#[derive(Debug)]
struct RouteRequest {
    server: String,
    op: Operation,
    payload: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum EnvelopeError {
    InvalidJson,
    EnvelopeMustBeObject,
    UnsupportedOperation,
    MissingOrNonStringServer,
    NonObjectPayload,
    MethodMismatch,
    SpawnShapedField { field: String },
}

impl EnvelopeError {
    fn into_handler_outcome(self) -> HandlerOutcome {
        let refusal = match self {
            Self::InvalidJson | Self::EnvelopeMustBeObject => {
                AdapterRefusal::new("invalid_envelope", "route envelope must be a JSON object")
            }
            Self::UnsupportedOperation => AdapterRefusal::new(
                "unsupported_op",
                "route envelope op must be tools/list or tools/call",
            ),
            Self::MissingOrNonStringServer => AdapterRefusal::new(
                "missing_server",
                "route envelope must include a string server",
            ),
            Self::NonObjectPayload => AdapterRefusal::new(
                "non_object_payload",
                "route envelope payload must be an object",
            ),
            Self::MethodMismatch => AdapterRefusal::new(
                "method_mismatch",
                "route envelope payload method must agree with op",
            ),
            Self::SpawnShapedField { field } => AdapterRefusal::new(
                "spawn_shaped_field",
                "route envelopes may not contain child spawn fields",
            )
            .with_field(field),
        };
        refusal.into_handler_outcome()
    }
}

struct AdapterRefusal {
    code: &'static str,
    message: &'static str,
    detail: Value,
}

impl AdapterRefusal {
    fn new(reason: &str, message: &'static str) -> Self {
        Self {
            code: BAD_REQUEST,
            message,
            detail: json!({ "reason": reason }),
        }
    }

    fn with_detail(code: &'static str, message: &'static str, detail: Value) -> Self {
        Self {
            code,
            message,
            detail,
        }
    }

    fn with_field(mut self, field: String) -> Self {
        if let Value::Object(detail) = &mut self.detail {
            detail.insert("field".to_string(), Value::String(field));
        }
        self
    }

    fn into_handler_outcome(self) -> HandlerOutcome {
        HandlerOutcome::ErrorWithDetail {
            code: self.code.to_string(),
            message: self.message.to_string(),
            detail: self.detail,
        }
    }
}

fn success_outcome(
    served_from: &str,
    observed_at_ms: u64,
    payload: Value,
    spawn_started: Option<u64>,
) -> HandlerOutcome {
    let mut body = json!({
        "served_from": served_from,
        "observed_at_ms": observed_at_ms,
        "payload": payload,
    });
    if let Some(spawn_elapsed_ms) = spawn_started {
        body["spawn_elapsed_ms"] = Value::from(spawn_elapsed_ms);
    }
    HandlerOutcome::Response(serde_json::to_vec(&body).expect("route response serializes"))
}

fn parse_envelope(body: &[u8]) -> Result<RouteRequest, EnvelopeError> {
    let value: Value = serde_json::from_slice(body).map_err(|_| EnvelopeError::InvalidJson)?;
    let object = value
        .as_object()
        .ok_or(EnvelopeError::EnvelopeMustBeObject)?;
    let op = validate_operation(object)?;
    let server = validate_server(object)?;
    let payload = validate_payload(object, op)?;
    if let Some(field) = find_spawn_shaped_field(&value) {
        return Err(EnvelopeError::SpawnShapedField { field });
    }
    Ok(RouteRequest {
        server,
        op,
        payload,
    })
}

fn validate_operation(object: &Map<String, Value>) -> Result<Operation, EnvelopeError> {
    match object.get("op").and_then(Value::as_str) {
        Some("tools/list") => Ok(Operation::ToolsList),
        Some("tools/call") => Ok(Operation::ToolsCall),
        _ => Err(EnvelopeError::UnsupportedOperation),
    }
}

fn validate_server(object: &Map<String, Value>) -> Result<String, EnvelopeError> {
    object
        .get("server")
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .ok_or(EnvelopeError::MissingOrNonStringServer)
}

fn validate_payload(
    object: &Map<String, Value>,
    operation: Operation,
) -> Result<Value, EnvelopeError> {
    let payload = object
        .get("payload")
        .filter(|payload| payload.is_object())
        .cloned()
        .ok_or(EnvelopeError::NonObjectPayload)?;
    let expected = match operation {
        Operation::ToolsList => "tools/list",
        Operation::ToolsCall => "tools/call",
    };
    if payload.get("method").and_then(Value::as_str) != Some(expected) {
        return Err(EnvelopeError::MethodMismatch);
    }
    Ok(payload)
}

fn find_spawn_shaped_field(value: &Value) -> Option<String> {
    match value {
        Value::Object(object) => object.iter().find_map(|(field, value)| {
            if SPAWN_SHAPED_FIELDS.contains(&field.as_str()) {
                Some(field.clone())
            } else {
                find_spawn_shaped_field(value)
            }
        }),
        Value::Array(items) => items.iter().find_map(find_spawn_shaped_field),
        _ => None,
    }
}

fn oldest_in_flight_age_ms(calls_in_flight: u64, started_at_ms: u64) -> u64 {
    if calls_in_flight == 0 || started_at_ms == 0 {
        0
    } else {
        epoch_millis().saturating_sub(started_at_ms)
    }
}

fn epoch_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn elapsed_since(start: Instant) -> u64 {
    start.elapsed().as_millis().try_into().unwrap_or(u64::MAX)
}

fn remaining_ms(end: Instant) -> u64 {
    end.saturating_duration_since(Instant::now())
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use serde_json::{json, Value};
    use subc_client_rs::{HandlerOutcome, ModuleHandler};

    use super::{parse_envelope, AdapterHandler, EnvelopeError};
    use crate::registry::parse_document;

    fn handler() -> AdapterHandler {
        let (registry, warnings) = parse_document(
            Path::new("registry.jsonc"),
            r#"{ "github": { "command": "mcp" } }"#,
        )
        .unwrap();
        assert!(warnings.is_empty());
        AdapterHandler::new(registry)
    }

    async fn refusal_for(body: Value) -> (String, String, Value, Value, Value) {
        let handler = handler();
        let before = handler.metrics().snapshot();
        let outcome = handler
            .route_outcome(&serde_json::to_vec(&body).expect("test request serializes"))
            .await;
        let after = handler.metrics().snapshot();
        let HandlerOutcome::ErrorWithDetail {
            code,
            message,
            detail,
        } = outcome
        else {
            panic!("invalid envelope must produce a detailed ERROR outcome");
        };
        (code, message, detail, before, after)
    }

    #[tokio::test]
    async fn unknown_op_is_a_typed_bad_request_without_child_side_effect() {
        let (code, _message, detail, before, after) = refusal_for(json!({
            "server": "github",
            "op": "resources/list",
            "payload": {},
        }))
        .await;

        assert_eq!(code, "bad_request");
        assert_eq!(detail["reason"], "unsupported_op");
        assert_eq!(before, after);
    }

    #[tokio::test]
    async fn missing_server_is_a_typed_bad_request_without_child_side_effect() {
        let (code, _message, detail, before, after) =
            refusal_for(json!({ "op": "tools/list", "payload": { "method": "tools/list" } })).await;

        assert_eq!(code, "bad_request");
        assert_eq!(detail["reason"], "missing_server");
        assert_eq!(before, after);
    }

    #[tokio::test]
    async fn non_object_payload_is_a_typed_bad_request_without_child_side_effect() {
        let (code, _message, detail, before, after) = refusal_for(json!({
            "server": "github",
            "op": "tools/list",
            "payload": [],
        }))
        .await;

        assert_eq!(code, "bad_request");
        assert_eq!(detail["reason"], "non_object_payload");
        assert_eq!(before, after);
    }

    #[tokio::test]
    async fn nested_spawn_shaped_field_is_a_typed_bad_request_without_child_side_effect() {
        let (code, _message, detail, before, after) = refusal_for(json!({
            "server": "github",
            "op": "tools/call",
            "payload": { "method": "tools/call", "params": { "command": "/bin/sh" } },
        }))
        .await;

        assert_eq!(code, "bad_request");
        assert_eq!(detail["reason"], "spawn_shaped_field");
        assert_eq!(detail["field"], "command");
        assert_eq!(before, after);
    }

    #[test]
    fn valid_envelope_is_accepted_before_child_forwarding() {
        assert!(parse_envelope(
            br#"{"server":"github","op":"tools/list","payload":{"method":"tools/list"}}"#
        )
        .is_ok());
    }

    #[test]
    fn parser_rejects_method_op_mismatch() {
        assert!(matches!(
            parse_envelope(
                br#"{"server":"github","op":"tools/list","payload":{"method":"tools/call"}}"#
            ),
            Err(EnvelopeError::MethodMismatch)
        ));
    }

    #[test]
    fn parser_has_specific_errors_for_invalid_json_and_non_object_envelope() {
        assert!(matches!(
            parse_envelope(b"{"),
            Err(EnvelopeError::InvalidJson)
        ));
        assert!(matches!(
            parse_envelope(b"[]"),
            Err(EnvelopeError::EnvelopeMustBeObject)
        ));
    }

    #[tokio::test]
    async fn health_reports_every_stable_lifecycle_metric() {
        let report = handler().health().await;
        let metrics = report.metrics.expect("health must carry lifecycle metrics");

        for key in [
            "children_live",
            "children_max",
            "spawns_total",
            "spawn_failures_total",
            "idle_evictions_total",
            "calls_in_flight",
            "oldest_in_flight_ms",
            "cache_served_total",
        ] {
            assert!(metrics.get(key).is_some(), "missing metric {key}");
        }
        assert_eq!(metrics["children_live"], 0);
        assert_eq!(metrics["children_max"], 8);
        assert_eq!(metrics["spawns_total"], 0);
    }
}
