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
    sync::{Mutex as AsyncMutex, Notify, OwnedSemaphorePermit, Semaphore},
    time::{sleep_until, timeout, Instant as TokioInstant},
};
use tokio_util::sync::CancellationToken;

use crate::{
    constants::{
        BASE_ENV_KEYS, CHILD_EARLY_EXIT_MS, DEFAULT_MAX_CHILDREN, EVICTION_GRACE_MS,
        SPAWN_ATTEMPT_BUDGET, SPAWN_INITIALIZE_BUDGET_MS, SPAWN_RETRY_COOLDOWN_MS,
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

/// Health accounting never takes a child-state lock or waits on subprocess work.
#[derive(Debug)]
pub struct HealthMetrics {
    children_live: AtomicU64,
    children_max: AtomicU64,
    spawns_total: AtomicU64,
    spawn_failures_total: AtomicU64,
    idle_evictions_total: AtomicU64,
    eviction_timers_live: AtomicU64,
    flights: Mutex<BTreeMap<u64, Instant>>,
    next_flight_id: AtomicU64,
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
            eviction_timers_live: AtomicU64::new(0),
            flights: Mutex::new(BTreeMap::new()),
            next_flight_id: AtomicU64::new(0),
            cache_served_total: AtomicU64::new(0),
        }
    }
}

impl HealthMetrics {
    pub fn snapshot(&self) -> Value {
        let flights = self
            .flights
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let oldest_age = flights
            .values()
            .min()
            .copied()
            .map(elapsed_since)
            .unwrap_or(0);
        json!({
            "children_live": self.children_live.load(Ordering::Relaxed),
            "children_max": self.children_max.load(Ordering::Relaxed),
            "spawns_total": self.spawns_total.load(Ordering::Relaxed),
            "spawn_failures_total": self.spawn_failures_total.load(Ordering::Relaxed),
            "idle_evictions_total": self.idle_evictions_total.load(Ordering::Relaxed),
            "eviction_timers_live": self.eviction_timers_live.load(Ordering::Relaxed),
            "calls_in_flight": flights.len(),
            "oldest_in_flight_ms": oldest_age,
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
        let identity = BindIdentity::new(
            env::current_dir().map_err(|_| CredentialResolutionError)?,
            "mcp-stdio-adapter".to_string(),
            "credential-resolution".to_string(),
        );
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
        self.route_outcome_with_cancellation(body, CancellationToken::new())
            .await
    }

    /// Processes one route envelope like [`Self::route_outcome`], ending any in-progress
    /// child wait early when the cancellation token fires (daemon CANCEL or route teardown).
    pub async fn route_outcome_with_cancellation(
        &self,
        body: &[u8],
        cancel: CancellationToken,
    ) -> HandlerOutcome {
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

        // Pagination cursors select different results. Keep their captures separate
        // so following nextCursor cannot return an earlier page indefinitely.
        let cache_key = (
            request.server.clone(),
            request
                .payload
                .pointer("/params/cursor")
                .map(Value::to_string),
        );
        if request.op == Operation::ToolsList && config.cache_tools_list {
            if let Some(cached) = self.lifecycle.cached_tools(&cache_key) {
                self.metrics
                    .cache_served_total
                    .fetch_add(1, Ordering::Relaxed);
                return success_outcome("cache", cached.observed_at_ms, cached.payload, None);
            }
        }

        let _flight = FlightGuard::new(Arc::clone(&self.metrics));
        match self
            .lifecycle
            .forward(
                &request.server,
                config,
                request.op,
                request.payload,
                &cancel,
            )
            .await
        {
            Ok(forwarded) => {
                if request.op == Operation::ToolsList && forwarded.cacheable {
                    self.lifecycle.cache_tools(
                        cache_key,
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
    async fn handle(&self, ctx: RequestCtx, body: Vec<u8>) -> HandlerOutcome {
        self.route_outcome_with_cancellation(&body, ctx.cancellation_token())
            .await
    }

    async fn health(&self) -> HealthReport {
        // The flight bookkeeping lock covers only timestamp insert/remove/snapshot.
        // No child-state lock or subprocess work can delay this health lane.
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
    cached_tools: Mutex<BTreeMap<(String, Option<String>), CachedTools>>,
    capacity: Arc<Semaphore>,
    capacity_gate: AsyncMutex<()>,
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
            capacity: Arc::new(Semaphore::new(DEFAULT_MAX_CHILDREN as usize)),
            capacity_gate: AsyncMutex::new(()),
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

    fn cached_tools(&self, key: &(String, Option<String>)) -> Option<CachedTools> {
        self.cached_tools
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(key)
            .cloned()
    }

    fn cache_tools(&self, key: (String, Option<String>), cached: CachedTools) {
        self.cached_tools
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(key, cached);
    }

    async fn forward(
        self: &Arc<Self>,
        server: &str,
        config: ServerConfig,
        operation: Operation,
        payload: Value,
        cancel: &CancellationToken,
    ) -> Result<ForwardedResponse, LifecycleError> {
        let deadline = Instant::now() + Duration::from_millis(config.deadline_ms);
        let attempts = if operation == Operation::ToolsList {
            2
        } else {
            1
        };
        for attempt in 0..attempts {
            match self
                .forward_once(server, &config, payload.clone(), cancel, deadline)
                .await
            {
                // A cancelled caller is gone for good: respawning a child for a
                // retry nobody waits on would only churn processes.
                Err(LifecycleError::CallOutcomeUnknown)
                    if attempt + 1 < attempts && !cancel.is_cancelled() =>
                {
                    continue
                }
                Err(LifecycleError::CallOutcomeUnknown) if operation == Operation::ToolsList => {
                    return Err(LifecycleError::ChildUnresponsive);
                }
                result => return result,
            }
        }
        unreachable!("the retry loop always returns its final attempt")
    }

    async fn forward_once(
        self: &Arc<Self>,
        server: &str,
        config: &ServerConfig,
        payload: Value,
        cancel: &CancellationToken,
        deadline: Instant,
    ) -> Result<ForwardedResponse, LifecycleError> {
        let slot = self.slot(server);
        let mut state = tokio::select! {
            state = tokio::time::timeout_at(TokioInstant::from_std(deadline), slot.state.lock()) =>
                state.map_err(|_| LifecycleError::ChildUnresponsive)?,
            () = cancel.cancelled() => return Err(LifecycleError::ChildUnresponsive),
        };
        let spawned_at = self
            .ensure_child(server, config, &mut state, deadline)
            .await?;
        // Capture the handshake cost before writing the tool request. Vendor
        // execution latency must not be attributed to the adapter's cold start.
        let spawn_elapsed_ms = spawned_at.map(elapsed_since);
        if Instant::now() >= deadline || cancel.is_cancelled() {
            self.remove_session(&mut state).await;
            return Err(LifecycleError::ChildUnresponsive);
        }
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
            self.record_early_exit(&mut state);
            self.remove_session(&mut state).await;
            return Err(LifecycleError::CallOutcomeUnknown);
        }

        // The per-server lane is held for the whole wait, so the read must be
        // bounded: a child that accepts the request and never replies would
        // otherwise block every later call to this server. Discovery's one
        // crash retry shares this deadline; a timeout never buys a new budget.
        // Cancellation and deadline expiry both abandon and tear down the child,
        // so a late reply cannot be consumed by a subsequent call.
        let read = tokio::time::timeout_at(
            TokioInstant::from_std(deadline),
            read_response(session, child_id, config.frame_ceiling_bytes),
        );
        let response = match tokio::select! {
            result = read => result.unwrap_or(Err(FrameReadError::TimedOut)),
            () = cancel.cancelled() => Err(FrameReadError::TimedOut),
        } {
            Ok(response) => response,
            Err(FrameReadError::Framing { observed_bytes }) => {
                self.remove_session(&mut state).await;
                return Err(LifecycleError::ChildFraming {
                    observed_bytes,
                    ceiling_bytes: config.frame_ceiling_bytes,
                });
            }
            Err(FrameReadError::TimedOut) => {
                self.remove_session(&mut state).await;
                return Err(LifecycleError::ChildUnresponsive);
            }
            Err(FrameReadError::Closed | FrameReadError::Io) => {
                self.record_early_exit(&mut state);
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
        session.last_idle = Instant::now();
        let ttl = self
            .settings
            .idle_ttl_override
            .unwrap_or_else(|| Duration::from_millis(config.idle_ttl_ms));
        self.schedule_idle_eviction(Arc::clone(&slot), &mut state, ttl);

        Ok(ForwardedResponse {
            payload,
            cacheable,
            observed_at_ms: epoch_millis(),
            spawn_elapsed_ms,
        })
    }

    async fn ensure_child(
        self: &Arc<Self>,
        _server: &str,
        config: &ServerConfig,
        state: &mut SlotState,
        deadline: Instant,
    ) -> Result<Option<Instant>, LifecycleError> {
        if let Some(session) = state.session.as_mut() {
            match session.child.try_wait() {
                Ok(Some(_)) | Err(_) => {
                    self.record_early_exit(state);
                    self.remove_session(state).await;
                }
                Ok(None) if session.initialized_at.is_some() => {
                    if session.initialized_at.unwrap().elapsed()
                        >= Duration::from_millis(CHILD_EARLY_EXIT_MS)
                    {
                        state.consecutive_failures = 0;
                        state.last_failure_cause = None;
                    }
                    return Ok(None);
                }
                // An aborted initialization retains its child and capacity slot
                // until a subsequent call can tear it down and reap it.
                Ok(None) => self.remove_session(state).await,
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

        let capacity = self.reserve_capacity().await?;
        let child_env = match tokio::time::timeout_at(
            TokioInstant::from_std(deadline),
            self.construct_environment(config),
        )
        .await
        {
            Ok(Ok(environment)) => environment,
            result => {
                let variable = result.ok().and_then(Result::err);
                let retry_after_ms =
                    self.record_failed_attempt(state, SpawnFailureCause::CredentialResolution);
                return Err(LifecycleError::SpawnFailed {
                    cause: SpawnFailureCause::CredentialResolution,
                    retry_after_ms,
                    env_var: variable,
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
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        if let Some(cwd) = &config.cwd {
            command.current_dir(cwd);
        }
        let (mut child, tree) = match spawn_contained(&mut command).await {
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
        state.session = Some(ChildSession {
            child,
            tree,
            _capacity: capacity,
            stdin: Some(stdin),
            stdout: BufReader::new(stdout),
            next_id: 1,
            last_idle: Instant::now(),
            initialized_at: None,
        });

        if let Err(_error) = initialize_child(
            state.session.as_mut().expect("spawn installed the child"),
            config.frame_ceiling_bytes,
            self.settings
                .spawn_initialize_budget
                .min(deadline.saturating_duration_since(Instant::now())),
        )
        .await
        {
            self.remove_session(state).await;
            let _ = self.record_failed_attempt(state, SpawnFailureCause::InitializeTimeout);
            return Err(LifecycleError::InitializeFailed);
        }

        // An initialize-then-exit loop must not reset its own failure streak.
        // Reset that streak only after the child survives the early-exit window.
        if !matches!(state.last_failure_cause, Some(SpawnFailureCause::EarlyExit)) {
            state.consecutive_failures = 0;
            state.last_failure_cause = None;
        }
        state.cooldown_until = None;
        state.session.as_mut().unwrap().initialized_at = Some(Instant::now());
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
            // Windows environment keys are case-insensitive: a declared
            // override must REPLACE a base entry whose key differs only by
            // case (registry "PATH" vs base allowlist "Path"), or the child
            // receives two case-variant spellings of one logical variable and
            // the OS collapses them unpredictably -- measured on CI, the base
            // value won and the declared override silently lost. Unix keys
            // are case-sensitive; no folding there.
            #[cfg(windows)]
            {
                let case_collisions: Vec<OsString> = environment
                    .keys()
                    .filter(|key| key.eq_ignore_ascii_case(variable))
                    .cloned()
                    .collect();
                for key in case_collisions {
                    environment.remove(&key);
                }
            }
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

    fn record_early_exit(&self, state: &mut SlotState) {
        if state
            .session
            .as_ref()
            .and_then(|session| session.initialized_at)
            .is_some_and(|started| started.elapsed() < Duration::from_millis(CHILD_EARLY_EXIT_MS))
        {
            self.record_failed_attempt(state, SpawnFailureCause::EarlyExit);
        }
    }

    async fn reserve_capacity(&self) -> Result<OwnedSemaphorePermit, LifecycleError> {
        // Serialize eviction with claiming its replacement slot. Never wait for
        // another server's state lock here: a locked lane is busy or initializing.
        let _reservation = self.capacity_gate.lock().await;
        if let Ok(permit) = Arc::clone(&self.capacity).try_acquire_owned() {
            return Ok(permit);
        }
        let slots: Vec<_> = self
            .slots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .cloned()
            .collect();
        let mut idle = Vec::new();
        for slot in slots {
            let last_idle = slot
                .state
                .try_lock()
                .ok()
                .and_then(|state| state.session.as_ref().map(|session| session.last_idle));
            if let Some(last_idle) = last_idle {
                idle.push((last_idle, slot));
            }
        }
        idle.sort_by_key(|(last_idle, _)| *last_idle);
        for (_, slot) in idle {
            if let Ok(mut state) = slot.state.try_lock() {
                if state.session.is_some() {
                    self.remove_session(&mut state).await;
                    self.metrics
                        .idle_evictions_total
                        .fetch_add(1, Ordering::Relaxed);
                    break;
                }
            }
        }
        Arc::clone(&self.capacity)
            .try_acquire_owned()
            .map_err(|_| LifecycleError::ChildCapacity)
    }

    async fn remove_session(&self, state: &mut SlotState) {
        if let Some(session) = state.session.take() {
            self.terminate_session(session).await;
        }
    }

    async fn terminate_session(&self, mut session: ChildSession) {
        let metrics = Arc::clone(&self.metrics);
        let grace = self.settings.eviction_grace;
        // Teardown owns the capacity permit through reaping even if its caller
        // disappears during grace. Cancelling the join does not cancel cleanup.
        let cleanup = tokio::spawn(async move {
            session.stdin.take();
            let waited = timeout(grace, session.child.wait()).await;
            // A normally exiting parent can still leave helpers behind.
            session.tree.terminate();
            if !matches!(waited, Ok(Ok(_))) {
                let _ = session.child.start_kill();
                let _ = session.child.wait().await;
            }
            metrics.children_live.fetch_sub(1, Ordering::Relaxed);
        });
        let _ = cleanup.await;
    }

    /// Re-arms the slot's single eviction timer after a successful call. The
    /// timer task is spawned on first use and then kept: one sleeping task per
    /// server no matter how many calls complete, instead of one extra sleeper
    /// per call for the full TTL.
    fn schedule_idle_eviction(
        self: &Arc<Self>,
        slot: Arc<ServerSlot>,
        state: &mut SlotState,
        ttl: Duration,
    ) {
        state.eviction_ttl = ttl;
        if state.eviction_timer.is_none() {
            let task = tokio::spawn(run_idle_eviction_timer(Arc::clone(self), Arc::clone(&slot)));
            state.eviction_timer = Some(task.abort_handle());
            self.metrics
                .eviction_timers_live
                .fetch_add(1, Ordering::Relaxed);
        }
        slot.eviction_wakeup.notify_one();
    }
}

/// The slot's long-lived eviction timer. Each iteration parks until either the
/// current session's idle deadline passes or a completed call re-arms the
/// deadline, so the task count stays at one per slot instead of growing with
/// call rate.
async fn run_idle_eviction_timer(lifecycle: Arc<ChildLifecycle>, slot: Arc<ServerSlot>) {
    let _live = EvictionTimerGuard::new(Arc::clone(&lifecycle.metrics));
    loop {
        let wake_at = {
            let state = slot.state.lock().await;
            state
                .session
                .as_ref()
                .map(|session| session.last_idle + state.eviction_ttl)
        };
        let Some(wake_at) = wake_at else {
            // No live session: park until the next completed call re-arms.
            slot.eviction_wakeup.notified().await;
            continue;
        };
        tokio::select! {
            () = sleep_until(TokioInstant::from_std(wake_at)) => {
                let mut state = slot.state.lock().await;
                // Re-checking last_idle under the lock covers a call that
                // finished between the deadline passing and this wakeup.
                let eligible = state.session.as_ref().is_some_and(|session| {
                    session.last_idle.elapsed() >= state.eviction_ttl
                });
                if eligible {
                    lifecycle.remove_session(&mut state).await;
                    lifecycle
                        .metrics
                        .idle_evictions_total
                        .fetch_add(1, Ordering::Relaxed);
                }
            }
            // A call completed: recompute the deadline from the fresh last_idle.
            () = slot.eviction_wakeup.notified() => {}
        }
    }
}

/// Keeps the live-timer metric honest if the timer task is ever aborted (for
/// example by runtime shutdown).
struct EvictionTimerGuard {
    metrics: Arc<HealthMetrics>,
}

impl EvictionTimerGuard {
    fn new(metrics: Arc<HealthMetrics>) -> Self {
        Self { metrics }
    }
}

impl Drop for EvictionTimerGuard {
    fn drop(&mut self) {
        self.metrics
            .eviction_timers_live
            .fetch_sub(1, Ordering::Relaxed);
    }
}

#[derive(Default)]
struct ServerSlot {
    state: AsyncMutex<SlotState>,
    eviction_wakeup: Notify,
}

#[derive(Default)]
struct SlotState {
    session: Option<ChildSession>,
    consecutive_failures: u64,
    cooldown_until: Option<Instant>,
    last_failure_cause: Option<SpawnFailureCause>,
    eviction_ttl: Duration,
    eviction_timer: Option<tokio::task::AbortHandle>,
}

struct ChildSession {
    child: Child,
    tree: ProcessTree,
    _capacity: OwnedSemaphorePermit,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    last_idle: Instant,
    initialized_at: Option<Instant>,
}

struct ProcessTree {
    #[cfg(unix)]
    group: rustix::process::Pid,
    #[cfg(windows)]
    job: subc_jobobject::JobObject,
}

impl ProcessTree {
    fn terminate(&self) {
        #[cfg(unix)]
        let _ = rustix::process::kill_process_group(self.group, rustix::process::Signal::Kill);
        #[cfg(windows)]
        let _ = self.job.terminate();
    }
}

impl Drop for ProcessTree {
    fn drop(&mut self) {
        self.terminate();
    }
}

async fn spawn_contained(command: &mut Command) -> std::io::Result<(Child, ProcessTree)> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.as_std_mut().process_group(0);
    }
    #[cfg(windows)]
    let job = {
        let job = subc_jobobject::JobObject::new()?;
        subc_jobobject::suspend_on_create_async(command);
        job
    };
    let child = command.spawn()?;
    #[cfg(windows)]
    let child = {
        let mut child = child;
        let contained = job.assign(&child).and_then(|()| {
            subc_jobobject::resume_main_thread(child.id().expect("new child has a pid"))
        });
        if let Err(error) = contained {
            let _ = job.terminate();
            let _ = child.start_kill();
            let _ = child.wait().await;
            return Err(error);
        }
        child
    };
    let tree = ProcessTree {
        #[cfg(unix)]
        group: rustix::process::Pid::from_raw(child.id().expect("new child has a pid") as i32)
            .expect("child pid is positive"),
        #[cfg(windows)]
        job,
    };
    Ok((child, tree))
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
    ChildUnresponsive,
    ChildCapacity,
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
            Self::ChildUnresponsive => AdapterRefusal::with_detail(
                "child_unresponsive",
                "MCP child did not complete before the call was abandoned",
                json!({}),
            )
            .into_handler_outcome(),
            Self::ChildCapacity => AdapterRefusal::with_detail(
                "child_capacity",
                "all MCP child slots are busy",
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
    EarlyExit,
}

impl SpawnFailureCause {
    fn as_str(self) -> &'static str {
        match self {
            Self::Exec => "exec",
            Self::InitializeTimeout => "initialize_timeout",
            Self::CredentialResolution => "credential_resolution",
            Self::EarlyExit => "early_exit",
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
    if !response.get("result").is_some_and(Value::is_object) || response.get("error").is_some() {
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
        // JSON-RPC requests and responses have independent id spaces. A server's
        // ping can reuse our call id without becoming the call's terminal reply.
        if let Some(method) = parsed.get("method") {
            if let Some(id) = parsed.get("id") {
                let reply = if method.as_str() == Some("ping") {
                    json!({"jsonrpc":"2.0", "id":id, "result":{}})
                } else {
                    json!({"jsonrpc":"2.0", "id":id, "error":{"code":-32601, "message":"client method not supported"}})
                };
                write_json_line(session.stdin.as_mut(), &reply)
                    .await
                    .map_err(|_| FrameReadError::Io)?;
            }
            continue;
        }
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
    id: u64,
}

impl FlightGuard {
    fn new(metrics: Arc<HealthMetrics>) -> Self {
        let id = metrics.next_flight_id.fetch_add(1, Ordering::Relaxed);
        metrics
            .flights
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id, Instant::now());
        Self { metrics, id }
    }
}

impl Drop for FlightGuard {
    fn drop(&mut self) {
        self.metrics
            .flights
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&self.id);
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
            "eviction_timers_live",
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

    #[test]
    fn oldest_flight_moves_to_surviving_call() {
        let metrics = std::sync::Arc::new(super::HealthMetrics::default());
        let first = super::FlightGuard::new(metrics.clone());
        std::thread::sleep(std::time::Duration::from_millis(100));
        let second = super::FlightGuard::new(metrics.clone());
        drop(first);
        assert!(metrics.snapshot()["oldest_in_flight_ms"].as_u64().unwrap() < 100);
        drop(second);
        assert_eq!(metrics.snapshot()["calls_in_flight"], 0);
        assert_eq!(metrics.snapshot()["oldest_in_flight_ms"], 0);
    }

    #[test]
    fn command_tool_arguments_follow_the_documented_spawn_field_fence() {
        assert!(
            matches!(parse_envelope(br#"{"server":"github","op":"tools/call","payload":{"method":"tools/call","params":{"name":"run","arguments":{"command":"ls"}}}}"#),
            Err(EnvelopeError::SpawnShapedField { field }) if field == "command")
        );
    }
}
