use std::{
    collections::HashMap,
    error::Error,
    fmt, io,
    path::PathBuf,
    process::ExitStatus,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde_json::Value;
use subc_control::SupervisorHealthStatus;
use subc_protocol::{
    session::{HealthReport, HealthStatus, ModuleControlRequest, MODULE_CONTROL_OP_HEALTH_CHECK},
    Flags, FrameType, Priority, SUBC_LAUNCH_NONCE_ENV, SUBC_MODULE_ID_ENV,
};
use tokio::{
    process::{Child, Command},
    sync::{mpsc, oneshot, watch, Mutex as AsyncMutex},
    task::JoinHandle,
    time::{sleep, timeout, timeout_at, Instant},
};
use tracing::{debug, error, info, warn};

use crate::{
    forwarding::{
        CloseReason, ForwardingError, ForwardingTable, GoodbyeTarget, ModuleControlRpcOutcome,
        ModuleDrainTarget, PendingModuleControlRpc,
    },
    registry::RegistryError,
    Frame, Registry,
};

/// Command-line flag used by supervised modules to find subc.
///
/// subc launches module-mode children as `<module> --subc <connection-file-path>`.
/// The path points at the TCP+key connection file; it is not an ambient signal and
/// is never inherited by standalone children.
pub const SUBC_ARG: &str = "--subc";

const DEFAULT_MAX_RESTARTS: u32 = 3;
const DEFAULT_BACKOFF: Duration = Duration::from_millis(100);
const DEFAULT_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
const REGISTRY_RELEASE_TIMEOUT: Duration = Duration::from_secs(1);
const REGISTRY_RELEASE_POLL: Duration = Duration::from_millis(10);

fn registration_release_events() -> &'static watch::Sender<u64> {
    static EVENTS: OnceLock<watch::Sender<u64>> = OnceLock::new();
    EVENTS.get_or_init(|| {
        let (sender, _receiver) = watch::channel(0);
        sender
    })
}

pub(crate) fn notify_registration_release() {
    let events = registration_release_events();
    let next_generation = (*events.borrow()).wrapping_add(1);
    events.send_replace(next_generation);
}

/// How to launch one singleton module process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleSpec {
    pub module_id: String,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    /// When true this is a reserved module: each spawn gets a fresh one-time launch
    /// nonce that the child must echo in its HELLO, so only the daemon-spawned
    /// process can register this module_id (a security-boundary module like the
    /// credential vault must not be impersonable while it is down/restarting).
    pub reserved: bool,
    /// Module-id prefixes this supervised module owns for reserved HELLO checks.
    /// Prefixes come from daemon config and must end in `:` before they reach the
    /// supervisor; the owner module's current spawn nonce authorizes claims under
    /// each prefix.
    pub reserved_prefixes: Vec<String>,
}

/// Bounded restart policy for crash exits.
///
/// `max_restarts` is the number of replacement processes allowed after the
/// initial spawn. After that many crash restarts the module enters
/// [`ModuleState::Failed`] and the supervisor stops the crash loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestartPolicy {
    pub max_restarts: u32,
    pub backoff: Duration,
}

impl RestartPolicy {
    pub fn new(max_restarts: u32, backoff: Duration) -> Self {
        Self {
            max_restarts,
            backoff,
        }
    }
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            max_restarts: DEFAULT_MAX_RESTARTS,
            backoff: DEFAULT_BACKOFF,
        }
    }
}

const DEFAULT_HEALTH_CADENCE: Duration = Duration::from_secs(30);
const DEFAULT_HEALTH_DEADLINE: Duration = Duration::from_secs(5);
const DEFAULT_HEALTH_FAILURE_THRESHOLD: u32 = 3;
const MAX_HEALTH_METRICS_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthAction {
    Report,
    Restart,
    Alert,
}

impl fmt::Display for HealthAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Report => "report",
            Self::Restart => "restart",
            Self::Alert => "alert",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HealthConfig {
    pub cadence: Duration,
    pub deadline: Duration,
    pub failure_threshold: u32,
    pub on_degraded: HealthAction,
    pub on_failing: HealthAction,
    pub critical: bool,
}

impl Default for HealthConfig {
    fn default() -> Self {
        Self {
            cadence: DEFAULT_HEALTH_CADENCE,
            deadline: DEFAULT_HEALTH_DEADLINE,
            failure_threshold: DEFAULT_HEALTH_FAILURE_THRESHOLD,
            on_degraded: HealthAction::Report,
            on_failing: HealthAction::Report,
            critical: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ModuleHealthStatus {
    pub status: SupervisorHealthStatus,
    pub last_probe_ms: Option<u64>,
    pub detail: Option<String>,
    pub metrics: Option<Value>,
    pub consecutive_failures: u32,
    pub last_action: Option<String>,
    pub last_action_ms: Option<u64>,
}

impl Default for ModuleHealthStatus {
    fn default() -> Self {
        Self {
            status: SupervisorHealthStatus::Unknown,
            last_probe_ms: None,
            detail: None,
            metrics: None,
            consecutive_failures: 0,
            last_action: None,
            last_action_ms: None,
        }
    }
}

/// Typed lifecycle state for a supervised module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModuleState {
    Starting,
    Running,
    Unresponsive,
    Restarting,
    Draining,
    Stopped,
    Failed,
    Disabled,
}

impl fmt::Display for ModuleState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Unresponsive => "unresponsive",
            Self::Restarting => "restarting",
            Self::Draining => "draining",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
            Self::Disabled => "disabled",
        })
    }
}

/// Supervisor classification of a child-process exit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitKind {
    Clean,
    Crash,
}

/// Last observed child exit, if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitReport {
    pub kind: ExitKind,
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

/// Point-in-time module status answerable by subc without forwarding to the
/// module process.
#[derive(Debug, Clone, PartialEq)]
pub struct ModuleStatus {
    pub module_id: String,
    pub state: ModuleState,
    pub enabled: bool,
    pub process_alive: bool,
    pub registration_active: bool,
    pub live: bool,
    pub restart_count: u32,
    pub pid: Option<u32>,
    pub last_exit: Option<ExitReport>,
    pub health: ModuleHealthStatus,
}

#[derive(Debug, Clone, PartialEq)]
struct SupervisorSnapshot {
    state: ModuleState,
    enabled: bool,
    process_alive: bool,
    restart_count: u32,
    pid: Option<u32>,
    last_exit: Option<ExitReport>,
    health: ModuleHealthStatus,
}

impl SupervisorSnapshot {
    fn starting() -> Self {
        Self::new(ModuleState::Starting, true)
    }

    fn disabled() -> Self {
        Self::new(ModuleState::Disabled, false)
    }

    fn failed() -> Self {
        Self::new(ModuleState::Failed, true)
    }

    fn new(state: ModuleState, enabled: bool) -> Self {
        Self {
            state,
            enabled,
            process_alive: false,
            restart_count: 0,
            pid: None,
            last_exit: None,
            health: ModuleHealthStatus::default(),
        }
    }
}

type SharedSnapshot = Arc<Mutex<SupervisorSnapshot>>;

/// Narrow process-liveness signal published by supervisors and consumed by passive liveness polls.
pub trait ModuleProcessLiveness: Send + Sync {
    fn process_live(&self, module_id: &str) -> Option<bool>;
}

/// Shared process-liveness registry keyed by supervised `module_id`.
#[derive(Debug, Clone, Default)]
pub struct SupervisorProcessLiveness {
    snapshots: Arc<Mutex<HashMap<String, SharedSnapshot>>>,
}

impl SupervisorProcessLiveness {
    pub fn new() -> Self {
        Self::default()
    }

    fn track(&self, module_id: String, snapshot: SharedSnapshot) {
        let mut snapshots = self
            .snapshots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        snapshots.insert(module_id, snapshot);
    }

    fn untrack_if_current(&self, module_id: &str, snapshot: &SharedSnapshot) {
        let mut snapshots = self
            .snapshots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let is_current = snapshots
            .get(module_id)
            .map(|tracked| Arc::ptr_eq(tracked, snapshot))
            .unwrap_or(false);
        if is_current {
            snapshots.remove(module_id);
        }
    }
}

impl ModuleProcessLiveness for SupervisorProcessLiveness {
    fn process_live(&self, module_id: &str) -> Option<bool> {
        let snapshot = {
            let snapshots = self
                .snapshots
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            snapshots.get(module_id).cloned()
        }?;
        let snapshot = snapshot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Some(snapshot.state == ModuleState::Running && snapshot.process_alive)
    }
}

#[derive(Debug, Clone)]
struct SupervisorRuntimeConfig {
    restart_policy: RestartPolicy,
    drain_timeout: Duration,
    health: HealthConfig,
    connection_file_path: Option<PathBuf>,
    forwarding: Option<Arc<ForwardingTable>>,
    /// The shared handle, so every spawn path (initial, restart, reload) records the
    /// reserved-module launch nonce the HELLO verifier checks against.
    supervisor_handle: Option<SupervisorHandle>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SupervisedConfiguration {
    spec: ModuleSpec,
    health: HealthConfig,
}

/// Shared daemon lookup table for supervised module handles.
///
/// Shared by clone between the [`Supervisor`] (which spawns processes) and the
/// channel-0 control handler (which verifies HELLOs and consumer route opens), so
/// launch nonces recorded at spawn are checked by the same daemon instance.
#[derive(Debug, Clone, Default)]
pub struct SupervisorHandle {
    modules: Arc<Mutex<HashMap<String, SupervisedModule>>>,
    /// The current expected launch nonce for each reserved module_id. Set when the
    /// supervisor spawns the reserved module; checked when a HELLO claims that id. A
    /// non-reserved module never has an entry here and is never nonce-checked.
    reserved_nonces: Arc<Mutex<HashMap<String, String>>>,
    /// The current launch nonce for every supervised spawn. This is separate from
    /// reserved_nonces because consumer route.open attestation applies to all spawned
    /// modules, while HELLO id-squatting protection remains opt-in via `reserved`.
    spawn_nonces: Arc<Mutex<HashMap<String, String>>>,
    /// Reserved namespace prefixes mapped to the supervised owner module whose
    /// current spawn nonce authorizes HELLO claims below the prefix.
    ///
    /// Per §2.6 this is not a same-user security barrier: a same-user process can
    /// read the key file and launch nonce env. Like exact reserved ids, it prevents
    /// accidental collisions and lower-trust processes from squatting protected
    /// namespaces.
    reserved_prefix_owners: Arc<Mutex<HashMap<String, String>>>,
    /// Serializes module-set reconciliation with operator lifecycle commands. Without
    /// this daemon-wide ordering, a rescan could retire or update a module while a
    /// concurrent reload still held its old handle and launch specification.
    operation_lock: Arc<AsyncMutex<()>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReservedHelloRejection {
    Exact {
        module_id: String,
    },
    Prefix {
        prefix: String,
        owner_module_id: String,
    },
}

impl SupervisorHandle {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the launch nonce from a supervised spawn, replacing any prior nonce so
    /// a respawn invalidates stale consumer identities.
    pub fn set_spawn_nonce(&self, module_id: &str, nonce: String) {
        self.spawn_nonces
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(module_id.to_string(), nonce);
    }

    /// Record the launch nonce expected from the next HELLO for a reserved module,
    /// replacing any prior nonce (a respawn invalidates the previous one).
    pub fn set_reserved_nonce(&self, module_id: &str, nonce: String) {
        self.reserved_nonces
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(module_id.to_string(), nonce);
    }

    /// Record namespace prefixes owned by a supervised module.
    pub fn set_reserved_prefixes(&self, owner_module_id: &str, prefixes: &[String]) {
        let mut owners = self
            .reserved_prefix_owners
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        owners.retain(|_, owner| owner != owner_module_id);
        for prefix in prefixes {
            owners.insert(prefix.clone(), owner_module_id.to_string());
        }
    }

    fn apply_identity_configuration(&self, spec: &ModuleSpec) {
        self.set_reserved_prefixes(&spec.module_id, &spec.reserved_prefixes);
        let spawn_nonce = self
            .spawn_nonces
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&spec.module_id)
            .cloned();
        let mut reserved_nonces = self
            .reserved_nonces
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if spec.reserved {
            if let Some(nonce) = spawn_nonce {
                reserved_nonces.insert(spec.module_id.clone(), nonce);
            }
        } else {
            reserved_nonces.remove(&spec.module_id);
        }
    }

    /// Whether a HELLO claiming `module_id` is authorized. An exact reserved id is
    /// authorized only by its expected nonce; otherwise a matching reserved prefix
    /// is authorized by the owner module's current spawn nonce. Non-reserved ids
    /// with no matching prefix are always authorized.
    pub fn reserved_hello_authorized(&self, module_id: &str, presented: Option<&str>) -> bool {
        self.reserved_hello_rejection(module_id, presented)
            .is_none()
    }

    pub(crate) fn reserved_hello_rejection(
        &self,
        module_id: &str,
        presented: Option<&str>,
    ) -> Option<ReservedHelloRejection> {
        let nonces = self
            .reserved_nonces
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(expected) = nonces.get(module_id) {
            if presented.is_some_and(|p| constant_time_eq(expected.as_bytes(), p.as_bytes())) {
                return None;
            }
            return Some(ReservedHelloRejection::Exact {
                module_id: module_id.to_string(),
            });
        }
        drop(nonces);

        let matched_prefix = self
            .reserved_prefix_owners
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .filter(|(prefix, _)| module_id.starts_with(prefix.as_str()))
            .max_by_key(|(prefix, _)| prefix.len())
            .map(|(prefix, owner)| (prefix.clone(), owner.clone()));
        let (prefix, owner_module_id) = matched_prefix?;

        let authorized = presented.is_some_and(|presented| {
            self.spawn_nonces
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(&owner_module_id)
                .is_some_and(|expected| constant_time_eq(expected.as_bytes(), presented.as_bytes()))
        });
        if authorized {
            None
        } else {
            Some(ReservedHelloRejection::Prefix {
                prefix,
                owner_module_id,
            })
        }
    }

    /// Whether a consumer connection proved it came from a daemon-spawned module.
    ///
    /// Absence of an expected spawn nonce is a hard failure: consumer_identity is
    /// accepted only for module ids the supervisor has spawned.
    pub fn spawned_consumer_authorized(&self, module_id: &str, presented: &str) -> bool {
        if presented.is_empty() {
            return false;
        }
        let nonces = self
            .spawn_nonces
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        nonces
            .get(module_id)
            .is_some_and(|expected| constant_time_eq(expected.as_bytes(), presented.as_bytes()))
    }

    /// Test/support lookup for the current launch nonce of a supervised spawn.
    pub fn spawn_launch_nonce_for(&self, module_id: &str) -> Option<String> {
        self.spawn_nonces
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(module_id)
            .cloned()
    }

    /// Test/support lookup for the HELLO-gating nonce of a reserved module.
    pub fn reserved_launch_nonce_for(&self, module_id: &str) -> Option<String> {
        self.reserved_nonces
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(module_id)
            .cloned()
    }

    pub fn insert(&self, module: SupervisedModule) -> Option<SupervisedModule> {
        let mut modules = self
            .modules
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        modules.insert(module.module_id().to_string(), module)
    }

    pub fn get(&self, module_id: &str) -> Option<SupervisedModule> {
        let modules = self
            .modules
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        modules.get(module_id).cloned()
    }

    pub fn list(&self) -> Vec<SupervisedModule> {
        let modules = self
            .modules
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut modules = modules.values().cloned().collect::<Vec<_>>();
        modules.sort_by(|left, right| left.module_id().cmp(right.module_id()));
        modules
    }

    pub(crate) fn retire(&self, module_id: &str) -> Option<SupervisedModule> {
        self.spawn_nonces
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(module_id);
        self.reserved_nonces
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(module_id);
        self.reserved_prefix_owners
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|_, owner| owner != module_id);
        self.modules
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(module_id)
    }

    pub(crate) fn operation_lock(&self) -> Arc<AsyncMutex<()>> {
        Arc::clone(&self.operation_lock)
    }
}

/// Process supervisor for subc-owned singleton modules.
#[derive(Debug, Clone)]
pub struct Supervisor {
    registry: Arc<Registry>,
    restart_policy: RestartPolicy,
    drain_timeout: Duration,
    connection_file_path: Option<PathBuf>,
    forwarding: Option<Arc<ForwardingTable>>,
    process_liveness: Arc<SupervisorProcessLiveness>,
    supervisor_handle: Option<SupervisorHandle>,
    health: HealthConfig,
}

impl Supervisor {
    pub fn new(registry: Arc<Registry>, restart_policy: RestartPolicy) -> Self {
        Self {
            registry,
            restart_policy,
            drain_timeout: DEFAULT_DRAIN_TIMEOUT,
            connection_file_path: None,
            forwarding: None,
            process_liveness: Arc::new(SupervisorProcessLiveness::default()),
            supervisor_handle: None,
            health: HealthConfig::default(),
        }
    }

    pub fn with_drain_timeout(mut self, drain_timeout: Duration) -> Self {
        self.drain_timeout = drain_timeout;
        self
    }

    pub fn with_process_liveness(
        mut self,
        process_liveness: Arc<SupervisorProcessLiveness>,
    ) -> Self {
        self.process_liveness = process_liveness;
        self
    }

    pub fn with_connection_file_path(mut self, connection_file_path: impl Into<PathBuf>) -> Self {
        self.connection_file_path = Some(connection_file_path.into());
        self
    }

    pub fn with_forwarding(mut self, forwarding: Arc<ForwardingTable>) -> Self {
        self.forwarding = Some(forwarding);
        self
    }

    pub fn with_handle(mut self, supervisor_handle: SupervisorHandle) -> Self {
        self.supervisor_handle = Some(supervisor_handle);
        self
    }

    pub fn with_health_config(mut self, health: HealthConfig) -> Self {
        self.health = health;
        self
    }

    /// Spawn `spec.program` and start monitoring it.
    ///
    /// The child is expected to parse `--subc <connection-file-path>`, read the
    /// TCP+key connection file, authenticate to the already-running listener, and
    /// register with channel-0 `HELLO` using `spec.module_id` as its manifest id.
    pub fn spawn(&self, spec: ModuleSpec) -> Result<SupervisedModule, SuperviseError> {
        validate_spec(&spec)?;

        let runtime = self.runtime_config();
        let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::starting()));
        let child = spawn_child(
            &spec,
            runtime.connection_file_path.as_deref(),
            self.supervisor_handle.as_ref(),
        )?;
        set_running(&snapshot, child.id())?;
        self.process_liveness
            .track(spec.module_id.clone(), Arc::clone(&snapshot));

        Ok(self.supervised_module(spec, runtime, snapshot, Some(child)))
    }

    /// Start supervising a module declared in daemon configuration.
    ///
    /// Unlike [`Self::spawn`], this records disabled modules and immediate spawn
    /// failures in the supervisor handle so operator-facing `supervisor.list`
    /// reflects every configured module while daemon startup continues.
    pub fn supervise_configured(
        &self,
        spec: ModuleSpec,
        enabled: bool,
    ) -> Result<SupervisedModule, SuperviseError> {
        validate_spec(&spec)?;

        let runtime = self.runtime_config();
        if !enabled {
            let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::disabled()));
            return Ok(self.supervised_module(spec, runtime, snapshot, None));
        }

        let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::starting()));
        match spawn_child(
            &spec,
            runtime.connection_file_path.as_deref(),
            self.supervisor_handle.as_ref(),
        ) {
            Ok(child) => {
                set_running(&snapshot, child.id())?;
                self.process_liveness
                    .track(spec.module_id.clone(), Arc::clone(&snapshot));
                Ok(self.supervised_module(spec, runtime, snapshot, Some(child)))
            }
            Err(err) => {
                error!(
                    module_id = %spec.module_id,
                    program = %spec.program.display(),
                    error = %err,
                    "configured module failed to spawn; marking failed and continuing"
                );
                let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::failed()));
                Ok(self.supervised_module(spec, runtime, snapshot, None))
            }
        }
    }

    pub fn supervise_configured_with_health(
        &self,
        spec: ModuleSpec,
        enabled: bool,
        health: HealthConfig,
    ) -> Result<SupervisedModule, SuperviseError> {
        validate_spec(&spec)?;

        let mut runtime = self.runtime_config();
        runtime.health = health;
        if !enabled {
            let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::disabled()));
            return Ok(self.supervised_module(spec, runtime, snapshot, None));
        }

        let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::starting()));
        match spawn_child(
            &spec,
            runtime.connection_file_path.as_deref(),
            self.supervisor_handle.as_ref(),
        ) {
            Ok(child) => {
                set_running(&snapshot, child.id())?;
                self.process_liveness
                    .track(spec.module_id.clone(), Arc::clone(&snapshot));
                Ok(self.supervised_module(spec, runtime, snapshot, Some(child)))
            }
            Err(err) => {
                if health.critical {
                    error!(
                        module_id = %spec.module_id,
                        program = %spec.program.display(),
                        error = %err,
                        "critical configured module failed to spawn; marking failed and alerting"
                    );
                } else {
                    error!(
                        module_id = %spec.module_id,
                        program = %spec.program.display(),
                        error = %err,
                        "configured module failed to spawn; marking failed and continuing"
                    );
                }
                let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::failed()));
                Ok(self.supervised_module(spec, runtime, snapshot, None))
            }
        }
    }

    fn runtime_config(&self) -> SupervisorRuntimeConfig {
        SupervisorRuntimeConfig {
            restart_policy: self.restart_policy,
            drain_timeout: self.drain_timeout,
            health: self.health,
            connection_file_path: self.connection_file_path.clone(),
            forwarding: self.forwarding.clone(),
            supervisor_handle: self.supervisor_handle.clone(),
        }
    }

    fn supervised_module(
        &self,
        spec: ModuleSpec,
        runtime: SupervisorRuntimeConfig,
        snapshot: SharedSnapshot,
        child: Option<Child>,
    ) -> SupervisedModule {
        let configuration = Arc::new(Mutex::new(SupervisedConfiguration {
            spec: spec.clone(),
            health: runtime.health,
        }));
        let (tx, rx) = mpsc::channel(4);
        let monitor = tokio::spawn(supervise_loop(
            spec.clone(),
            runtime,
            Arc::clone(&self.registry),
            Arc::clone(&self.process_liveness),
            Arc::clone(&snapshot),
            child,
            rx,
        ));

        let module_id = spec.module_id.clone();
        let module = SupervisedModule {
            inner: Arc::new(SupervisedModuleInner {
                module_id: module_id.clone(),
                registry: Arc::clone(&self.registry),
                snapshot,
                configuration,
                commands: tx,
                monitor: Mutex::new(Some(monitor)),
            }),
        };
        if let Some(supervisor_handle) = &self.supervisor_handle {
            supervisor_handle.apply_identity_configuration(&spec);
            supervisor_handle.insert(module.clone());
        }
        module
    }
}

impl Default for Supervisor {
    fn default() -> Self {
        Self::new(Arc::new(Registry::default()), RestartPolicy::default())
    }
}

/// Handle to one supervised child process.
#[derive(Clone)]
pub struct SupervisedModule {
    inner: Arc<SupervisedModuleInner>,
}

struct SupervisedModuleInner {
    module_id: String,
    registry: Arc<Registry>,
    snapshot: SharedSnapshot,
    configuration: Arc<Mutex<SupervisedConfiguration>>,
    commands: mpsc::Sender<SupervisorCommand>,
    monitor: Mutex<Option<JoinHandle<()>>>,
}

impl fmt::Debug for SupervisedModule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SupervisedModule")
            .field("module_id", &self.inner.module_id)
            .field("status", &self.status())
            .finish_non_exhaustive()
    }
}

impl SupervisedModule {
    pub fn module_id(&self) -> &str {
        &self.inner.module_id
    }

    pub fn state(&self) -> Result<ModuleState, SuperviseError> {
        Ok(lock_snapshot(&self.inner.snapshot)?.state)
    }

    pub fn status(&self) -> Result<ModuleStatus, SuperviseError> {
        let snapshot = lock_snapshot(&self.inner.snapshot)?.clone();
        let registration_active = self
            .inner
            .registry
            .get_module(&self.inner.module_id)
            .map_err(SuperviseError::Registry)?
            .is_some();
        let live = snapshot.enabled
            && snapshot.state == ModuleState::Running
            && snapshot.process_alive
            && registration_active;

        Ok(ModuleStatus {
            module_id: self.inner.module_id.clone(),
            state: snapshot.state,
            enabled: snapshot.enabled,
            process_alive: snapshot.process_alive,
            registration_active,
            live,
            restart_count: snapshot.restart_count,
            pid: snapshot.pid,
            last_exit: snapshot.last_exit,
            health: snapshot.health,
        })
    }

    /// Drain the module and stop monitoring it.
    pub async fn drain(&self) -> Result<(), SuperviseError> {
        self.stop().await
    }

    pub(crate) async fn retire(&self) -> Result<(), SuperviseError> {
        match self.state()? {
            ModuleState::Stopped | ModuleState::Failed => return Ok(()),
            ModuleState::Starting
            | ModuleState::Running
            | ModuleState::Unresponsive
            | ModuleState::Restarting
            | ModuleState::Draining
            | ModuleState::Disabled => {}
        }

        let (reply_tx, reply_rx) = oneshot::channel();
        self.inner
            .commands
            .send(SupervisorCommand::Retire { reply: reply_tx })
            .await
            .map_err(|_| SuperviseError::CommandClosed {
                module_id: self.inner.module_id.clone(),
            })?;
        reply_rx.await.map_err(|_| SuperviseError::CommandClosed {
            module_id: self.inner.module_id.clone(),
        })?
    }

    pub async fn stop(&self) -> Result<(), SuperviseError> {
        match self.state()? {
            ModuleState::Stopped | ModuleState::Failed => return Ok(()),
            ModuleState::Starting
            | ModuleState::Running
            | ModuleState::Unresponsive
            | ModuleState::Restarting
            | ModuleState::Draining
            | ModuleState::Disabled => {}
        }

        let (reply_tx, reply_rx) = oneshot::channel();
        self.inner
            .commands
            .send(SupervisorCommand::Drain { reply: reply_tx })
            .await
            .map_err(|_| SuperviseError::CommandClosed {
                module_id: self.inner.module_id.clone(),
            })?;
        reply_rx.await.map_err(|_| SuperviseError::CommandClosed {
            module_id: self.inner.module_id.clone(),
        })?
    }

    pub async fn restart(&self) -> Result<(), SuperviseError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.inner
            .commands
            .send(SupervisorCommand::Restart { reply: reply_tx })
            .await
            .map_err(|_| SuperviseError::CommandClosed {
                module_id: self.inner.module_id.clone(),
            })?;
        reply_rx.await.map_err(|_| SuperviseError::CommandClosed {
            module_id: self.inner.module_id.clone(),
        })?
    }

    pub async fn reload(&self) -> Result<(), SuperviseError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.inner
            .commands
            .send(SupervisorCommand::Reload { reply: reply_tx })
            .await
            .map_err(|_| SuperviseError::CommandClosed {
                module_id: self.inner.module_id.clone(),
            })?;
        reply_rx.await.map_err(|_| SuperviseError::CommandClosed {
            module_id: self.inner.module_id.clone(),
        })?
    }

    pub async fn set_enabled(&self, enabled: bool) -> Result<bool, SuperviseError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.inner
            .commands
            .send(SupervisorCommand::SetEnabled {
                enabled,
                reply: reply_tx,
            })
            .await
            .map_err(|_| SuperviseError::CommandClosed {
                module_id: self.inner.module_id.clone(),
            })?;
        reply_rx.await.map_err(|_| SuperviseError::CommandClosed {
            module_id: self.inner.module_id.clone(),
        })?
    }

    pub(crate) fn configuration(&self) -> Result<(ModuleSpec, HealthConfig), SuperviseError> {
        let configuration =
            self.inner
                .configuration
                .lock()
                .map_err(|_| SuperviseError::StatePoisoned {
                    module_id: Some(self.inner.module_id.clone()),
                })?;
        Ok((configuration.spec.clone(), configuration.health))
    }

    pub(crate) async fn update_configuration(
        &self,
        spec: ModuleSpec,
        health: HealthConfig,
    ) -> Result<(), SuperviseError> {
        if spec.module_id != self.inner.module_id {
            return Err(SuperviseError::InvalidSpec {
                reason: "a supervised module's module_id cannot be changed".to_string(),
            });
        }
        validate_spec(&spec)?;
        let (reply_tx, reply_rx) = oneshot::channel();
        self.inner
            .commands
            .send(SupervisorCommand::UpdateConfiguration {
                spec: spec.clone(),
                health,
                reply: reply_tx,
            })
            .await
            .map_err(|_| SuperviseError::CommandClosed {
                module_id: self.inner.module_id.clone(),
            })?;
        reply_rx.await.map_err(|_| SuperviseError::CommandClosed {
            module_id: self.inner.module_id.clone(),
        })?;
        let mut configuration =
            self.inner
                .configuration
                .lock()
                .map_err(|_| SuperviseError::StatePoisoned {
                    module_id: Some(self.inner.module_id.clone()),
                })?;
        configuration.spec = spec;
        configuration.health = health;
        Ok(())
    }
}

impl Drop for SupervisedModuleInner {
    fn drop(&mut self) {
        let Ok(mut monitor) = self.monitor.lock() else {
            return;
        };
        if let Some(monitor) = monitor.as_ref().filter(|monitor| !monitor.is_finished()) {
            let _ = update_snapshot(&self.snapshot, Some(&self.module_id), |state| {
                state.state = ModuleState::Stopped;
                state.process_alive = false;
                state.pid = None;
            });
            monitor.abort();
        }
        let _ = monitor.take();
    }
}

#[derive(Debug)]
enum SupervisorCommand {
    Drain {
        reply: oneshot::Sender<Result<(), SuperviseError>>,
    },
    Retire {
        reply: oneshot::Sender<Result<(), SuperviseError>>,
    },
    Restart {
        reply: oneshot::Sender<Result<(), SuperviseError>>,
    },
    Reload {
        reply: oneshot::Sender<Result<(), SuperviseError>>,
    },
    SetEnabled {
        enabled: bool,
        reply: oneshot::Sender<Result<bool, SuperviseError>>,
    },
    UpdateConfiguration {
        spec: ModuleSpec,
        health: HealthConfig,
        reply: oneshot::Sender<()>,
    },
}

#[derive(Debug)]
pub enum SuperviseError {
    InvalidSpec {
        reason: String,
    },
    Spawn {
        program: PathBuf,
        source: io::Error,
    },
    /// CSPRNG failure generating a reserved module's launch nonce. Fail loud rather
    /// than spawn a reserved module without its identity binding.
    LaunchNonce {
        reason: String,
    },
    Wait {
        module_id: String,
        source: io::Error,
    },
    Kill {
        module_id: String,
        source: io::Error,
    },
    Forwarding(ForwardingError),
    Registry(RegistryError),
    ReloadUnavailable {
        module_id: String,
        reason: String,
    },
    /// An operator restart/reload was requested for a module that is currently
    /// disabled. Restart/reload cycle a *running* module; a disabled module must
    /// be explicitly re-enabled (set_enabled(true)) rather than silently started
    /// by a restart, so these commands are rejected instead of re-enabling it.
    Disabled {
        module_id: String,
    },
    ReloadFailed {
        module_id: String,
        reason: String,
    },
    RegistrationStillActive {
        module_id: String,
        waited: Duration,
    },
    StatePoisoned {
        module_id: Option<String>,
    },
    CommandClosed {
        module_id: String,
    },
}

impl fmt::Display for SuperviseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSpec { reason } => write!(f, "invalid module spec: {reason}"),
            Self::Spawn { program, source } => {
                write!(
                    f,
                    "failed to spawn module '{}': {source}",
                    program.display()
                )
            }
            Self::LaunchNonce { reason } => {
                write!(
                    f,
                    "failed to generate reserved-module launch nonce: {reason}"
                )
            }
            Self::Wait { module_id, source } => {
                write!(f, "failed to wait for module '{module_id}': {source}")
            }
            Self::Kill { module_id, source } => {
                write!(f, "failed to kill module '{module_id}': {source}")
            }
            Self::Forwarding(err) => write!(f, "forwarding error: {err}"),
            Self::Registry(err) => write!(f, "registry error: {err}"),
            Self::ReloadUnavailable { module_id, reason } => {
                write!(f, "reload unavailable for module '{module_id}': {reason}")
            }
            Self::Disabled { module_id } => {
                write!(
                    f,
                    "module '{module_id}' is disabled; enable it before restart or reload"
                )
            }
            Self::ReloadFailed { module_id, reason } => {
                write!(f, "reload failed for module '{module_id}': {reason}")
            }
            Self::RegistrationStillActive { module_id, waited } => write!(
                f,
                "module '{module_id}' registration remained active after waiting {waited:?}"
            ),
            Self::StatePoisoned { module_id } => match module_id {
                Some(module_id) => {
                    write!(f, "supervisor state for module '{module_id}' was poisoned")
                }
                None => write!(f, "supervisor state was poisoned"),
            },
            Self::CommandClosed { module_id } => {
                write!(
                    f,
                    "supervisor command channel for module '{module_id}' is closed"
                )
            }
        }
    }
}

impl Error for SuperviseError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Spawn { source, .. } | Self::Wait { source, .. } | Self::Kill { source, .. } => {
                Some(source)
            }
            Self::Forwarding(err) => Some(err),
            Self::Registry(err) => Some(err),
            Self::LaunchNonce { .. }
            | Self::InvalidSpec { .. }
            | Self::ReloadUnavailable { .. }
            | Self::Disabled { .. }
            | Self::ReloadFailed { .. }
            | Self::RegistrationStillActive { .. }
            | Self::StatePoisoned { .. }
            | Self::CommandClosed { .. } => None,
        }
    }
}

pub(crate) fn validate_spec(spec: &ModuleSpec) -> Result<(), SuperviseError> {
    if spec.module_id.trim().is_empty() {
        return Err(SuperviseError::InvalidSpec {
            reason: "module_id must not be empty".to_string(),
        });
    }

    Ok(())
}

#[derive(Debug, Default)]
struct HealthProbeRuntime {
    registered_connection: Option<crate::ConnectionId>,
    advertised: bool,
    next_probe_at: Option<Instant>,
    probe_index: u64,
}

impl HealthProbeRuntime {
    fn refresh_registration(
        &mut self,
        spec: &ModuleSpec,
        runtime: &SupervisorRuntimeConfig,
        registry: &Registry,
        snapshot: &SharedSnapshot,
    ) {
        let registration = match registry.get_module(&spec.module_id) {
            Ok(registration) => registration,
            Err(err) => {
                warn!(module_id = %spec.module_id, error = %err, "health prober could not read registry");
                self.advertised = false;
                self.next_probe_at = None;
                return;
            }
        };

        let Some(registration) = registration else {
            self.registered_connection = None;
            self.advertised = false;
            self.next_probe_at = None;
            return;
        };

        let advertised = registration
            .control_ops
            .iter()
            .any(|op| op == MODULE_CONTROL_OP_HEALTH_CHECK);
        if !advertised {
            self.registered_connection = Some(registration.connection_id);
            self.advertised = false;
            self.next_probe_at = None;
            let _ = update_snapshot(snapshot, Some(&spec.module_id), |state| {
                state.health.status = SupervisorHealthStatus::Unknown;
                state.health.consecutive_failures = 0;
                state.health.last_probe_ms = None;
                state.health.detail = None;
                state.health.metrics = None;
            });
            return;
        }

        let reregistered = self.registered_connection != Some(registration.connection_id);
        self.registered_connection = Some(registration.connection_id);
        self.advertised = true;
        if reregistered || self.next_probe_at.is_none() {
            self.probe_index = 0;
            self.next_probe_at = Some(
                Instant::now() + jittered_health_delay(&spec.module_id, 0, runtime.health.cadence),
            );
            let _ = update_snapshot(snapshot, Some(&spec.module_id), |state| {
                state.health.status = SupervisorHealthStatus::Unknown;
                state.health.consecutive_failures = 0;
                state.health.detail = None;
                state.health.metrics = None;
            });
        }
    }

    fn wake_after(&self) -> Duration {
        if !self.advertised {
            return REGISTRY_RELEASE_POLL;
        }
        self.next_probe_at
            .map(|next| next.saturating_duration_since(Instant::now()))
            .unwrap_or(REGISTRY_RELEASE_POLL)
    }

    fn due(&self) -> bool {
        self.advertised
            && self
                .next_probe_at
                .is_some_and(|next| Instant::now() >= next)
    }

    fn schedule_next(&mut self, spec: &ModuleSpec, cadence: Duration) {
        self.probe_index = self.probe_index.wrapping_add(1);
        self.next_probe_at = Some(
            Instant::now() + jittered_health_delay(&spec.module_id, self.probe_index, cadence),
        );
    }
}

#[derive(Debug)]
struct HealthProbeError {
    message: String,
}

impl HealthProbeError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for HealthProbeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

async fn run_health_probe_cycle(
    spec: &ModuleSpec,
    runtime: &SupervisorRuntimeConfig,
    registry: &Registry,
    process_liveness: &SupervisorProcessLiveness,
    snapshot: &SharedSnapshot,
    child: &mut Option<Child>,
) {
    let now_ms = unix_ms_now();
    match probe_module_health(spec, runtime).await {
        Ok(report) => {
            handle_health_report(
                spec,
                runtime,
                registry,
                process_liveness,
                snapshot,
                child,
                report,
                now_ms,
            )
            .await;
        }
        Err(err) => {
            handle_health_probe_failure(
                spec,
                runtime,
                registry,
                process_liveness,
                snapshot,
                child,
                err,
                now_ms,
            )
            .await;
        }
    }
}

async fn probe_module_health(
    spec: &ModuleSpec,
    runtime: &SupervisorRuntimeConfig,
) -> Result<HealthReport, HealthProbeError> {
    let Some(forwarding) = runtime.forwarding.as_ref() else {
        return Err(HealthProbeError::new(
            "supervisor was not configured with a forwarding table",
        ));
    };
    let deadline = Instant::now() + runtime.health.deadline;
    let pending = forwarding
        .begin_module_control_rpc_for(&spec.module_id, MODULE_CONTROL_OP_HEALTH_CHECK, deadline)
        .map_err(|err| HealthProbeError::new(format!("failed to begin health.check RPC: {err}")))?;
    let PendingModuleControlRpc {
        endpoint,
        module_sink,
        negotiated_ver,
        corr,
        receiver,
    } = pending;
    let body = serde_json::to_vec(&ModuleControlRequest::HealthCheck {})
        .map_err(|err| HealthProbeError::new(format!("failed to encode health.check: {err}")))?;
    let frame = Frame::build_with_version(
        negotiated_ver,
        FrameType::Request,
        control_flags(),
        0,
        0,
        corr,
        body,
    )
    .map_err(|err| HealthProbeError::new(format!("failed to build health.check frame: {err}")))?;

    // The enqueue itself must be bounded by the probe deadline: FrameSink.send
    // blocks waiting for capacity when the module's egress queue is full, and an
    // unbounded await here freezes the whole supervision actor (it stops polling
    // Child::wait and supervisor commands), making the module unrecoverable
    // in-band. On timeout the probe fails like any transport failure.
    match timeout_at(deadline, module_sink.send(frame)).await {
        Ok(Ok(())) => {}
        Ok(Err(err)) => {
            let _ = forwarding.cancel_module_control_rpc(endpoint, corr);
            return Err(HealthProbeError::new(format!(
                "failed to send health.check: {err}"
            )));
        }
        Err(_elapsed) => {
            let _ = forwarding.cancel_module_control_rpc(endpoint, corr);
            return Err(HealthProbeError::new(
                "health.check send timed out before enqueue (module egress full)",
            ));
        }
    }

    match timeout_at(deadline, receiver).await {
        Ok(Ok(ModuleControlRpcOutcome::Response(response))) => {
            response.health_report().ok_or_else(|| {
                HealthProbeError::new("health.check RPC returned a non-health response")
            })
        }
        Ok(Ok(ModuleControlRpcOutcome::Rejected(body))) => Err(HealthProbeError::new(format!(
            "health.check rejected: {}",
            body.message
        ))),
        Ok(Ok(ModuleControlRpcOutcome::ModuleGone(message))) => Err(HealthProbeError::new(message)),
        Ok(Ok(ModuleControlRpcOutcome::MalformedResponse(message))) => {
            Err(HealthProbeError::new(message))
        }
        Ok(Ok(ModuleControlRpcOutcome::UnexpectedOp { expected, actual })) => {
            Err(HealthProbeError::new(format!(
                "expected module-control op '{expected}', got '{actual}'"
            )))
        }
        Ok(Ok(ModuleControlRpcOutcome::DeadlineElapsed)) => Err(HealthProbeError::new(
            "module answered health.check after its daemon deadline",
        )),
        Ok(Err(_)) => Err(HealthProbeError::new(
            "health.check waiter was canceled before the module responded",
        )),
        Err(_) => {
            let _ = forwarding.cancel_module_control_rpc(endpoint, corr);
            Err(HealthProbeError::new(format!(
                "module did not answer health.check within {:?}",
                runtime.health.deadline
            )))
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_health_report(
    spec: &ModuleSpec,
    runtime: &SupervisorRuntimeConfig,
    registry: &Registry,
    process_liveness: &SupervisorProcessLiveness,
    snapshot: &SharedSnapshot,
    child: &mut Option<Child>,
    report: HealthReport,
    now_ms: u64,
) {
    let status = supervisor_health_status(report.status);
    let detail = report.detail.clone();
    let metrics = truncate_health_metrics(report.metrics);
    let _ = update_snapshot(snapshot, Some(&spec.module_id), |state| {
        state.health.status = status;
        state.health.last_probe_ms = Some(now_ms);
        state.health.detail = detail.clone();
        state.health.metrics = metrics.clone();
        state.health.consecutive_failures = 0;
    });

    let action = match report.status {
        HealthStatus::Ok => return,
        HealthStatus::Degraded => runtime.health.on_degraded,
        HealthStatus::Failing => runtime.health.on_failing,
    };
    apply_l3_health_action(
        spec,
        runtime,
        registry,
        process_liveness,
        snapshot,
        child,
        status,
        detail.as_deref(),
        action,
        now_ms,
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
async fn handle_health_probe_failure(
    spec: &ModuleSpec,
    runtime: &SupervisorRuntimeConfig,
    registry: &Registry,
    process_liveness: &SupervisorProcessLiveness,
    snapshot: &SharedSnapshot,
    child: &mut Option<Child>,
    err: HealthProbeError,
    now_ms: u64,
) {
    let threshold = runtime.health.failure_threshold.max(1);
    let mut failures = 0;
    let detail = err.to_string();
    let _ = update_snapshot(snapshot, Some(&spec.module_id), |state| {
        state.health.last_probe_ms = Some(now_ms);
        state.health.consecutive_failures = state.health.consecutive_failures.saturating_add(1);
        state.health.detail = Some(detail.clone());
        state.health.metrics = None;
        failures = state.health.consecutive_failures;
    });

    if failures < threshold {
        warn!(
            module_id = %spec.module_id,
            consecutive_failures = failures,
            threshold,
            detail = %detail,
            "health.check probe failed"
        );
        return;
    }

    let _ = update_snapshot(snapshot, Some(&spec.module_id), |state| {
        state.state = ModuleState::Unresponsive;
        state.health.status = SupervisorHealthStatus::Unresponsive;
    });
    if runtime.health.critical {
        error!(
            module_id = %spec.module_id,
            status = "unresponsive",
            detail = %detail,
            "critical module health alert"
        );
    } else {
        warn!(
            module_id = %spec.module_id,
            status = "unresponsive",
            detail = %detail,
            "module health threshold breached"
        );
    }
    if let Err(err) = health_restart_child(
        spec,
        runtime,
        registry,
        process_liveness,
        snapshot,
        child,
        SupervisorHealthStatus::Unresponsive,
        Some(&detail),
        now_ms,
    )
    .await
    {
        error!(module_id = %spec.module_id, error = %err, "health-triggered restart failed");
    }
}

#[allow(clippy::too_many_arguments)]
async fn apply_l3_health_action(
    spec: &ModuleSpec,
    runtime: &SupervisorRuntimeConfig,
    registry: &Registry,
    process_liveness: &SupervisorProcessLiveness,
    snapshot: &SharedSnapshot,
    child: &mut Option<Child>,
    status: SupervisorHealthStatus,
    detail: Option<&str>,
    action: HealthAction,
    now_ms: u64,
) {
    record_health_action(snapshot, &spec.module_id, action.to_string(), now_ms);
    match action {
        HealthAction::Report => {
            info!(
                module_id = %spec.module_id,
                status = ?status,
                detail,
                "module reported non-ok health"
            );
        }
        HealthAction::Alert => {
            error!(
                module_id = %spec.module_id,
                status = ?status,
                detail,
                "module health alert"
            );
        }
        HealthAction::Restart => {
            if let Err(err) = health_restart_child(
                spec,
                runtime,
                registry,
                process_liveness,
                snapshot,
                child,
                status,
                detail,
                now_ms,
            )
            .await
            {
                error!(module_id = %spec.module_id, error = %err, "health-triggered restart failed");
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn health_restart_child(
    spec: &ModuleSpec,
    runtime: &SupervisorRuntimeConfig,
    registry: &Registry,
    process_liveness: &SupervisorProcessLiveness,
    snapshot: &SharedSnapshot,
    child: &mut Option<Child>,
    status: SupervisorHealthStatus,
    detail: Option<&str>,
    now_ms: u64,
) -> Result<(), SuperviseError> {
    if !lock_snapshot(snapshot)?.enabled {
        return Err(SuperviseError::Disabled {
            module_id: spec.module_id.clone(),
        });
    }

    if lock_snapshot(snapshot)?.restart_count >= runtime.restart_policy.max_restarts {
        record_health_action(snapshot, &spec.module_id, "disabled".to_string(), now_ms);
        error!(
            module_id = %spec.module_id,
            status = ?status,
            detail,
            restart_count = runtime.restart_policy.max_restarts,
            "health restart budget exhausted; disabling module"
        );
        begin_forwarding_drain_if_configured(
            spec,
            runtime,
            snapshot,
            Some(false),
            "health-disable",
        )
        .await?;
        drain_optional_child(
            &spec.module_id,
            registry,
            snapshot,
            child,
            runtime.drain_timeout,
            ModuleState::Disabled,
            Some(false),
        )
        .await?;
        process_liveness.untrack_if_current(&spec.module_id, snapshot);
        return Ok(());
    }

    update_snapshot(snapshot, Some(&spec.module_id), |state| {
        state.restart_count += 1;
        state.state = ModuleState::Unresponsive;
        state.health.status = status;
        state.health.last_action = Some(HealthAction::Restart.to_string());
        state.health.last_action_ms = Some(now_ms);
    })?;
    warn!(
        module_id = %spec.module_id,
        status = ?status,
        detail,
        restart_count = lock_snapshot(snapshot)?.restart_count,
        "health-triggered module restart"
    );

    begin_forwarding_drain_if_configured(spec, runtime, snapshot, Some(true), "health-restart")
        .await?;
    drain_optional_child(
        &spec.module_id,
        registry,
        snapshot,
        child,
        runtime.drain_timeout,
        ModuleState::Restarting,
        Some(true),
    )
    .await?;
    sleep(runtime.restart_policy.backoff).await;
    process_liveness.track(spec.module_id.clone(), Arc::clone(snapshot));
    match spawn_and_mark_running(spec, runtime, snapshot) {
        Ok(next_child) => {
            *child = Some(next_child);
            Ok(())
        }
        Err(err) => {
            fail_snapshot(snapshot, Some(&spec.module_id), None);
            process_liveness.untrack_if_current(&spec.module_id, snapshot);
            *child = None;
            Err(err)
        }
    }
}

fn record_health_action(snapshot: &SharedSnapshot, module_id: &str, action: String, now_ms: u64) {
    let _ = update_snapshot(snapshot, Some(module_id), |state| {
        state.health.last_action = Some(action);
        state.health.last_action_ms = Some(now_ms);
    });
}

fn supervisor_health_status(status: HealthStatus) -> SupervisorHealthStatus {
    match status {
        HealthStatus::Ok => SupervisorHealthStatus::Ok,
        HealthStatus::Degraded => SupervisorHealthStatus::Degraded,
        HealthStatus::Failing => SupervisorHealthStatus::Failing,
    }
}

fn truncate_health_metrics(metrics: Option<Value>) -> Option<Value> {
    let metrics = metrics?;
    match serde_json::to_vec(&metrics) {
        Ok(encoded) if encoded.len() > MAX_HEALTH_METRICS_BYTES => Some(serde_json::json!({
            "truncated": true,
            "original_bytes": encoded.len(),
        })),
        Ok(_) | Err(_) => Some(metrics),
    }
}

/// Spread health probes so a fleet-wide restart does not converge them.
///
/// The delay is derived from the module id and probe index rather than a random
/// source, so it is deterministic per module: a module keeps its own offset
/// across daemon restarts instead of re-rolling into a collision.
fn jittered_health_delay(module_id: &str, probe_index: u64, cadence: Duration) -> Duration {
    if cadence.is_zero() {
        return Duration::ZERO;
    }
    let cadence_ms = cadence.as_millis() as u64;
    // This early return is REDUNDANT, deliberately, and a mutation run will show
    // it surviving removal. Recording why here so the next person to notice does
    // not have to re-derive it:
    //
    // - It is unreachable in practice. `positive_millis` in daemon_config rejects
    //   a zero cadence and builds the Duration from whole milliseconds, so a
    //   sub-millisecond cadence cannot come from config.
    // - Even if reached it changes no answer. The `.max(1)` below makes the span
    //   1, and `hash % 1` is 0, so the fall-through returns `cadence` unchanged
    //   -- exactly what this returns.
    //
    // Kept as a guard against a future widening of the config parser (accepting
    // microseconds, say), which would make the sub-millisecond case reachable.
    // The `.max(1)` is the load-bearing half TODAY: remove it and the modulo
    // divides by zero. Remove this and nothing changes.
    if cadence_ms == 0 {
        return cadence;
    }
    let jitter_span = (cadence_ms / 10).max(1);
    let hash = module_id.as_bytes().iter().fold(
        probe_index.wrapping_mul(0x9E37_79B9_7F4A_7C15),
        |acc, byte| {
            acc.wrapping_mul(1099511628211)
                .wrapping_add(u64::from(*byte))
        },
    );
    cadence + Duration::from_millis(hash % jitter_span)
}

fn unix_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

async fn supervise_loop(
    mut spec: ModuleSpec,
    mut runtime: SupervisorRuntimeConfig,
    registry: Arc<Registry>,
    process_liveness: Arc<SupervisorProcessLiveness>,
    snapshot: SharedSnapshot,
    mut child: Option<Child>,
    mut commands: mpsc::Receiver<SupervisorCommand>,
) {
    let mut health_probe = HealthProbeRuntime::default();
    loop {
        if child.is_some() {
            health_probe.refresh_registration(&spec, &runtime, &registry, &snapshot);
            let probe_sleep = sleep(health_probe.wake_after());
            tokio::pin!(probe_sleep);
            let active_child = child.as_mut().expect("child checked above");
            tokio::select! {
                wait_result = active_child.wait() => {
                    // Every arm below that gives up on the CHILD must keep the
                    // supervision task itself alive (child = None, loop
                    // continues into command-serving mode). Returning here
                    // closes the command channel, which makes the module
                    // permanently unrestartable in-band: a clean child exit
                    // of an enabled module once wedged the fleet this way
                    // ('supervisor command channel is closed') and required a
                    // full daemon restart to recover.
                    let exit_report = match wait_result {
                        Ok(status) => classify_exit(&status),
                        Err(err) => {
                            fail_snapshot(&snapshot, Some(&spec.module_id), None);
                            untrack_if_registration_released(
                                &process_liveness,
                                &registry,
                                &spec.module_id,
                                &snapshot,
                            );
                            error!(module_id = %spec.module_id, error = %err, "failed to wait for supervised module");
                            child = None;
                            continue;
                        }
                    };

                    match on_child_exit(
                        &spec,
                        runtime.restart_policy,
                        &registry,
                        &snapshot,
                        exit_report,
                    ).await {
                        NextAction::Stop { registration_released } => {
                            if registration_released {
                                process_liveness.untrack_if_current(&spec.module_id, &snapshot);
                            }
                            child = None;
                        }
                        NextAction::Restart => {
                            sleep(runtime.restart_policy.backoff).await;
                            if let Err(err) = wait_for_registration_release(
                                &registry,
                                &spec.module_id,
                                REGISTRY_RELEASE_TIMEOUT,
                            ).await {
                                fail_snapshot(&snapshot, Some(&spec.module_id), None);
                                error!(module_id = %spec.module_id, error = %err, "registration did not release before restart");
                                child = None;
                                continue;
                            }

                            match spawn_and_mark_running(&spec, &runtime, &snapshot) {
                                Ok(next_child) => {
                                    child = Some(next_child);
                                    debug!(module_id = %spec.module_id, "supervised module restarted after crash");
                                }
                                Err(err) => {
                                    fail_snapshot(&snapshot, Some(&spec.module_id), None);
                                    process_liveness.untrack_if_current(&spec.module_id, &snapshot);
                                    error!(module_id = %spec.module_id, error = %err, "failed to restart supervised module");
                                    child = None;
                                }
                            }
                        }
                    }
                }
                command = commands.recv() => {
                    let Some(command) = command else {
                        return;
                    };
                    if !handle_supervisor_command(
                        command,
                        &mut spec,
                        &mut runtime,
                        &registry,
                        &process_liveness,
                        &snapshot,
                        &mut child,
                    ).await {
                        return;
                    }
                }
                _ = &mut probe_sleep => {
                    if health_probe.due() {
                        run_health_probe_cycle(
                            &spec,
                            &runtime,
                            &registry,
                            &process_liveness,
                            &snapshot,
                            &mut child,
                        ).await;
                        if child.is_some() {
                            health_probe.schedule_next(&spec, runtime.health.cadence);
                        }
                    }
                }
            }
        } else {
            let Some(command) = commands.recv().await else {
                return;
            };
            if !handle_supervisor_command(
                command,
                &mut spec,
                &mut runtime,
                &registry,
                &process_liveness,
                &snapshot,
                &mut child,
            )
            .await
            {
                return;
            }
        }
    }
}

enum NextAction {
    Stop { registration_released: bool },
    Restart,
}

async fn handle_supervisor_command(
    command: SupervisorCommand,
    spec: &mut ModuleSpec,
    runtime: &mut SupervisorRuntimeConfig,
    registry: &Registry,
    process_liveness: &SupervisorProcessLiveness,
    snapshot: &SharedSnapshot,
    child: &mut Option<Child>,
) -> bool {
    match command {
        SupervisorCommand::Drain { reply } => {
            let result = drain_optional_child(
                &spec.module_id,
                registry,
                snapshot,
                child,
                runtime.drain_timeout,
                ModuleState::Stopped,
                None,
            )
            .await;
            let registration_released = result.is_ok();
            let _ = reply.send(result);
            if registration_released {
                process_liveness.untrack_if_current(&spec.module_id, snapshot);
            }
            false
        }
        SupervisorCommand::Retire { reply } => {
            let result = async {
                begin_forwarding_drain_if_configured(
                    spec,
                    runtime,
                    snapshot,
                    None,
                    "supervisor retire",
                )
                .await?;
                drain_optional_child(
                    &spec.module_id,
                    registry,
                    snapshot,
                    child,
                    runtime.drain_timeout,
                    ModuleState::Stopped,
                    None,
                )
                .await
            }
            .await;
            let registration_released = result.is_ok();
            let _ = reply.send(result);
            if registration_released {
                process_liveness.untrack_if_current(&spec.module_id, snapshot);
            }
            false
        }
        SupervisorCommand::Restart { reply } => {
            let result =
                restart_child(spec, runtime, registry, process_liveness, snapshot, child).await;
            let _ = reply.send(result);
            true
        }
        SupervisorCommand::Reload { reply } => {
            let result =
                reload_child(spec, runtime, registry, process_liveness, snapshot, child).await;
            let _ = reply.send(result);
            true
        }
        SupervisorCommand::SetEnabled { enabled, reply } => {
            let result = set_child_enabled(
                spec,
                runtime,
                registry,
                process_liveness,
                snapshot,
                child,
                enabled,
            )
            .await;
            let _ = reply.send(result);
            true
        }
        SupervisorCommand::UpdateConfiguration {
            spec: next_spec,
            health,
            reply,
        } => {
            if let Some(handle) = &runtime.supervisor_handle {
                handle.apply_identity_configuration(&next_spec);
            }
            *spec = next_spec;
            runtime.health = health;
            let _ = reply.send(());
            true
        }
    }
}

async fn restart_child(
    spec: &ModuleSpec,
    runtime: &SupervisorRuntimeConfig,
    registry: &Registry,
    process_liveness: &SupervisorProcessLiveness,
    snapshot: &SharedSnapshot,
    child: &mut Option<Child>,
) -> Result<(), SuperviseError> {
    // Restart cycles a running module; it must not silently start a disabled one.
    if !lock_snapshot(snapshot)?.enabled {
        return Err(SuperviseError::Disabled {
            module_id: spec.module_id.clone(),
        });
    }
    begin_forwarding_drain_if_configured(spec, runtime, snapshot, None, "restart").await?;

    if child.is_some() {
        drain_optional_child(
            &spec.module_id,
            registry,
            snapshot,
            child,
            runtime.drain_timeout,
            ModuleState::Restarting,
            Some(true),
        )
        .await?;
    } else {
        update_snapshot(snapshot, Some(&spec.module_id), |state| {
            state.enabled = true;
            state.state = ModuleState::Restarting;
            state.process_alive = false;
            state.pid = None;
        })?;
        wait_for_registration_release(registry, &spec.module_id, REGISTRY_RELEASE_TIMEOUT).await?;
    }

    reset_restart_count(snapshot, &spec.module_id)?;
    sleep(runtime.restart_policy.backoff).await;
    process_liveness.track(spec.module_id.clone(), Arc::clone(snapshot));
    let next_child = spawn_and_mark_running(spec, runtime, snapshot)?;
    *child = Some(next_child);
    debug!(module_id = %spec.module_id, "supervised module restarted by operator request");
    Ok(())
}

async fn reload_child(
    spec: &ModuleSpec,
    runtime: &SupervisorRuntimeConfig,
    registry: &Registry,
    process_liveness: &SupervisorProcessLiveness,
    snapshot: &SharedSnapshot,
    child: &mut Option<Child>,
) -> Result<(), SuperviseError> {
    // Reload cycles a running module; it must not silently start a disabled one.
    if !lock_snapshot(snapshot)?.enabled {
        return Err(SuperviseError::Disabled {
            module_id: spec.module_id.clone(),
        });
    }
    begin_forwarding_drain(spec, runtime, snapshot, Some(true), "reload").await?;

    if child.is_some() {
        drain_optional_child(
            &spec.module_id,
            registry,
            snapshot,
            child,
            runtime.drain_timeout,
            ModuleState::Restarting,
            Some(true),
        )
        .await?;
    } else {
        update_snapshot(snapshot, Some(&spec.module_id), |state| {
            state.enabled = true;
            state.state = ModuleState::Restarting;
            state.process_alive = false;
            state.pid = None;
        })?;
        wait_for_registration_release(registry, &spec.module_id, REGISTRY_RELEASE_TIMEOUT).await?;
    }

    reset_restart_count(snapshot, &spec.module_id)?;
    sleep(runtime.restart_policy.backoff).await;
    process_liveness.track(spec.module_id.clone(), Arc::clone(snapshot));
    let next_child = match spawn_and_mark_running(spec, runtime, snapshot) {
        Ok(next_child) => next_child,
        Err(err) => {
            return handle_reload_spawn_failure(
                spec,
                runtime,
                process_liveness,
                snapshot,
                child,
                format!("new child failed to spawn: {err}"),
            )
            .await;
        }
    };
    *child = Some(next_child);

    let wait_outcome = {
        let active_child = child.as_mut().expect("new reload child was just stored");
        wait_for_registration_after_reload(
            registry,
            &spec.module_id,
            active_child,
            REGISTRY_RELEASE_TIMEOUT,
        )
        .await?
    };

    match wait_outcome {
        RegistrationWaitOutcome::Registered => {
            debug!(module_id = %spec.module_id, "supervised module reloaded and registered");
            Ok(())
        }
        RegistrationWaitOutcome::Exited(exit_report) => {
            *child = None;
            handle_reload_child_registration_failure(
                spec,
                runtime,
                registry,
                process_liveness,
                snapshot,
                child,
                ReloadRegistrationFailure {
                    exit_report: registration_failure_exit_report(exit_report),
                    reason: "new child exited before registering".to_string(),
                },
            )
            .await
        }
        RegistrationWaitOutcome::TimedOut => {
            let mut timed_out_child = child
                .take()
                .expect("timed-out reload child is still running");
            timed_out_child
                .start_kill()
                .map_err(|source| SuperviseError::Kill {
                    module_id: spec.module_id.clone(),
                    source,
                })?;
            let status = timed_out_child
                .wait()
                .await
                .map_err(|source| SuperviseError::Wait {
                    module_id: spec.module_id.clone(),
                    source,
                })?;
            handle_reload_child_registration_failure(
                spec,
                runtime,
                registry,
                process_liveness,
                snapshot,
                child,
                ReloadRegistrationFailure {
                    exit_report: registration_failure_exit_report(classify_exit(&status)),
                    reason: format!(
                        "new child did not register within {:?}",
                        REGISTRY_RELEASE_TIMEOUT
                    ),
                },
            )
            .await
        }
    }
}

async fn set_child_enabled(
    spec: &ModuleSpec,
    runtime: &SupervisorRuntimeConfig,
    registry: &Registry,
    process_liveness: &SupervisorProcessLiveness,
    snapshot: &SharedSnapshot,
    child: &mut Option<Child>,
    enabled: bool,
) -> Result<bool, SuperviseError> {
    let (current_enabled, current_state) = {
        let state = lock_snapshot(snapshot)?;
        (state.enabled, state.state)
    };
    // `start` (enable on an already-enabled module) heals TERMINAL states instead
    // of no-op'ing: a module whose restart budget exhausted (Failed) or that exited
    // clean (Stopped) has no live process and no other in-band recovery — the
    // operator's start is the explicit recovery act and resets the budget. Without
    // this arm the only revival was subc-probe --supervisor-restart in a terminal,
    // which the 2026-07-14 aft outage proved is a trap when the failed module is
    // the one providing every agent's shell.
    let revive_terminal = enabled
        && current_enabled
        && child.is_none()
        && matches!(current_state, ModuleState::Failed | ModuleState::Stopped);
    if current_enabled == enabled && !revive_terminal {
        return Ok(false);
    }

    if enabled {
        update_snapshot(snapshot, Some(&spec.module_id), |state| {
            state.enabled = true;
            state.state = ModuleState::Starting;
            state.process_alive = false;
            state.pid = None;
        })?;
        wait_for_registration_release(registry, &spec.module_id, REGISTRY_RELEASE_TIMEOUT).await?;
        reset_restart_count(snapshot, &spec.module_id)?;
        process_liveness.track(spec.module_id.clone(), Arc::clone(snapshot));
        let next_child = match spawn_and_mark_running(spec, runtime, snapshot) {
            Ok(next_child) => next_child,
            Err(err) => {
                if let Err(state_err) = update_snapshot(snapshot, Some(&spec.module_id), |state| {
                    state.state = ModuleState::Failed;
                    state.process_alive = false;
                    state.pid = None;
                }) {
                    error!(module_id = %spec.module_id, error = %state_err, "failed to record enable spawn failure");
                }
                process_liveness.untrack_if_current(&spec.module_id, snapshot);
                return Err(err);
            }
        };
        *child = Some(next_child);
        debug!(module_id = %spec.module_id, "supervised module enabled");
        Ok(true)
    } else {
        begin_forwarding_drain_if_configured(spec, runtime, snapshot, Some(false), "disable")
            .await?;
        drain_optional_child(
            &spec.module_id,
            registry,
            snapshot,
            child,
            runtime.drain_timeout,
            ModuleState::Disabled,
            Some(false),
        )
        .await?;
        debug!(module_id = %spec.module_id, "supervised module disabled");
        Ok(true)
    }
}

async fn on_child_exit(
    spec: &ModuleSpec,
    policy: RestartPolicy,
    registry: &Registry,
    snapshot: &SharedSnapshot,
    exit_report: ExitReport,
) -> NextAction {
    match exit_report.kind {
        ExitKind::Clean => {
            info!(
                module_id = %spec.module_id,
                exit_code = ?exit_report.code,
                exit_signal = ?exit_report.signal,
                "supervised module exited cleanly"
            );
            if let Err(err) = update_snapshot(snapshot, Some(&spec.module_id), |state| {
                state.state = ModuleState::Stopped;
                state.process_alive = false;
                state.pid = None;
                state.last_exit = Some(exit_report);
            }) {
                error!(module_id = %spec.module_id, error = %err, "failed to record clean module exit");
            }
            let registration_released = match wait_for_registration_release(
                registry,
                &spec.module_id,
                REGISTRY_RELEASE_TIMEOUT,
            )
            .await
            {
                Ok(()) => true,
                Err(err) => {
                    warn!(module_id = %spec.module_id, error = %err, "registration still active after clean exit");
                    false
                }
            };
            NextAction::Stop {
                registration_released,
            }
        }
        ExitKind::Crash => {
            warn!(
                module_id = %spec.module_id,
                exit_code = ?exit_report.code,
                exit_signal = ?exit_report.signal,
                "supervised module exited abnormally (crash)"
            );
            let mut should_restart = false;
            if let Err(err) = update_snapshot(snapshot, Some(&spec.module_id), |state| {
                state.process_alive = false;
                state.pid = None;
                state.last_exit = Some(exit_report);
                if !state.enabled {
                    state.state = ModuleState::Disabled;
                } else if state.restart_count >= policy.max_restarts {
                    state.state = ModuleState::Failed;
                } else {
                    state.restart_count += 1;
                    state.state = ModuleState::Restarting;
                    should_restart = true;
                }
            }) {
                error!(module_id = %spec.module_id, error = %err, "failed to record crashed module exit");
                return NextAction::Stop {
                    registration_released: false,
                };
            }

            if should_restart {
                NextAction::Restart
            } else {
                let registration_released = match wait_for_registration_release(
                    registry,
                    &spec.module_id,
                    REGISTRY_RELEASE_TIMEOUT,
                )
                .await
                {
                    Ok(()) => true,
                    Err(err) => {
                        warn!(module_id = %spec.module_id, error = %err, "registration still active after failed module");
                        false
                    }
                };
                NextAction::Stop {
                    registration_released,
                }
            }
        }
    }
}

fn untrack_if_registration_released(
    process_liveness: &SupervisorProcessLiveness,
    registry: &Registry,
    module_id: &str,
    snapshot: &SharedSnapshot,
) {
    match registry.get_module(module_id) {
        Ok(None) => process_liveness.untrack_if_current(module_id, snapshot),
        Ok(Some(_)) => {}
        Err(err) => {
            warn!(module_id, error = %err, "could not determine whether supervisor liveness can be untracked");
        }
    }
}

fn spawn_child(
    spec: &ModuleSpec,
    connection_file_path: Option<&std::path::Path>,
    handle: Option<&SupervisorHandle>,
) -> Result<Child, SuperviseError> {
    let mut command = Command::new(&spec.program);
    command.args(&spec.args);
    if let Some(connection_file_path) = connection_file_path {
        command.arg(SUBC_ARG).arg(connection_file_path);
    }
    for (key, value) in &spec.env {
        command.env(key, value);
    }
    command.env(SUBC_MODULE_ID_ENV, &spec.module_id);

    // Every supervised spawn receives a fresh one-time launch nonce for consumer
    // route.open attestation. Reserved modules additionally use the same nonce for
    // HELLO id-squatting protection. A respawn rotates both records.
    let nonce = generate_launch_nonce()?;
    if let Some(handle) = handle {
        handle.set_spawn_nonce(&spec.module_id, nonce.clone());
        if spec.reserved {
            handle.set_reserved_nonce(&spec.module_id, nonce.clone());
        }
    }
    command.env(SUBC_LAUNCH_NONCE_ENV, nonce);

    command.kill_on_drop(true);
    command.spawn().map_err(|source| SuperviseError::Spawn {
        program: spec.program.clone(),
        source,
    })
}

/// A fresh 256-bit CSPRNG launch nonce, lowercase hex. Used to bind a reserved
/// module's registration to the exact process the supervisor spawned.
fn generate_launch_nonce() -> Result<String, SuperviseError> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).map_err(|source| SuperviseError::LaunchNonce {
        reason: source.to_string(),
    })?;
    let mut hex = String::with_capacity(64);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(hex, "{b:02x}");
    }
    Ok(hex)
}

/// Constant-time byte comparison so a reserved-nonce mismatch leaks no timing
/// signal about how many leading bytes matched.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn spawn_and_mark_running(
    spec: &ModuleSpec,
    runtime: &SupervisorRuntimeConfig,
    snapshot: &SharedSnapshot,
) -> Result<Child, SuperviseError> {
    let child = spawn_child(
        spec,
        runtime.connection_file_path.as_deref(),
        runtime.supervisor_handle.as_ref(),
    )?;
    set_running(snapshot, child.id())?;
    Ok(child)
}

enum RegistrationWaitOutcome {
    Registered,
    Exited(ExitReport),
    TimedOut,
}

struct ReloadRegistrationFailure {
    exit_report: ExitReport,
    reason: String,
}

async fn wait_for_forwarding_quiescence(
    forwarding: &ForwardingTable,
    endpoint: crate::ModuleEndpointId,
    wait: Duration,
) -> Result<bool, SuperviseError> {
    let deadline = Instant::now() + wait;
    loop {
        let in_flight = forwarding
            .endpoint_in_flight_count(endpoint)
            .map_err(SuperviseError::Forwarding)?;
        if in_flight == 0 {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        sleep(REGISTRY_RELEASE_POLL).await;
    }
}

fn send_route_goodbyes(forwarding: &ForwardingTable, released_routes: Vec<GoodbyeTarget>) {
    for released in released_routes {
        let frame = match Frame::build_with_version(
            released.negotiated_ver,
            FrameType::Goodbye,
            control_flags(),
            released.channel,
            released.epoch,
            0,
            Vec::new(),
        ) {
            Ok(frame) => frame,
            Err(err) => {
                warn!(
                    route_channel = released.channel,
                    error = %err,
                    "failed to build supervisor drain route GOODBYE frame"
                );
                continue;
            }
        };
        if let Err(err) = released.sink.try_send(frame) {
            if released.close_on_delivery_failure() {
                warn!(
                    target_connection_id = released.connection_id.get(),
                    route_channel = released.channel,
                    error = %err,
                    "supervisor drain route GOODBYE was not delivered to client; closing target connection"
                );
                let _ = forwarding.escalate_client_delivery_failure(
                    released.connection_id,
                    released.channel,
                    released.epoch,
                    CloseReason::new(
                        "route_goodbye_delivery_failed",
                        format!(
                            "failed to enqueue supervisor drain route GOODBYE for channel {}: {err}",
                            released.channel
                        ),
                    ),
                );
            } else {
                warn!(
                    target_connection_id = released.connection_id.get(),
                    route_channel = released.channel,
                    error = %err,
                    "supervisor drain route GOODBYE to module dropped under backpressure; not closing shared module connection"
                );
            }
        }
    }
}

fn send_module_goodbye(module_id: &str, forwarding: &ForwardingTable, target: &ModuleDrainTarget) {
    let frame = match Frame::build_with_version(
        target.negotiated_ver,
        FrameType::Goodbye,
        control_flags(),
        0,
        0,
        0,
        Vec::new(),
    ) {
        Ok(frame) => frame,
        Err(err) => {
            warn!(
                module_id,
                error = %err,
                "failed to build supervisor drain module GOODBYE frame"
            );
            return;
        }
    };
    if let Err(err) = target.sink.try_send(frame) {
        warn!(
            module_id,
            target_connection_id = target.endpoint.connection_id.get(),
            error = %err,
            "supervisor drain module GOODBYE was not delivered to peer; closing module connection"
        );
        forwarding.request_connection_close(
            target.endpoint.connection_id,
            CloseReason::new(
                "module_goodbye_delivery_failed",
                format!("failed to enqueue supervisor drain module GOODBYE for module '{module_id}': {err}"),
            ),
        );
    }
}

async fn begin_forwarding_drain(
    spec: &ModuleSpec,
    runtime: &SupervisorRuntimeConfig,
    snapshot: &SharedSnapshot,
    enabled: Option<bool>,
    operation: &'static str,
) -> Result<(), SuperviseError> {
    let Some(forwarding) = runtime.forwarding.as_ref() else {
        return Err(SuperviseError::ReloadUnavailable {
            module_id: spec.module_id.clone(),
            reason: "supervisor was not configured with a forwarding table".to_string(),
        });
    };

    begin_forwarding_drain_with(forwarding, spec, runtime, snapshot, enabled, operation).await
}

async fn begin_forwarding_drain_if_configured(
    spec: &ModuleSpec,
    runtime: &SupervisorRuntimeConfig,
    snapshot: &SharedSnapshot,
    enabled: Option<bool>,
    operation: &'static str,
) -> Result<(), SuperviseError> {
    let Some(forwarding) = runtime.forwarding.as_ref() else {
        return Ok(());
    };

    begin_forwarding_drain_with(forwarding, spec, runtime, snapshot, enabled, operation).await
}

async fn begin_forwarding_drain_with(
    forwarding: &ForwardingTable,
    spec: &ModuleSpec,
    runtime: &SupervisorRuntimeConfig,
    snapshot: &SharedSnapshot,
    enabled: Option<bool>,
    operation: &'static str,
) -> Result<(), SuperviseError> {
    // Admission gate first: route.open/commit and route REQUEST admission are closed
    // before the first quiescence check, so the outstanding count can only fall.
    let drain_target = forwarding
        .begin_module_drain(&spec.module_id)
        .map_err(SuperviseError::Forwarding)?;
    update_snapshot(snapshot, Some(&spec.module_id), |state| {
        state.state = ModuleState::Draining;
        if let Some(enabled) = enabled {
            state.enabled = enabled;
        }
    })?;

    if let Some(target) = drain_target.as_ref() {
        send_route_goodbyes(forwarding, target.abandoned_bindings.clone());
        if !wait_for_forwarding_quiescence(forwarding, target.endpoint, runtime.drain_timeout)
            .await?
        {
            warn!(
                module_id = %spec.module_id,
                waited = ?runtime.drain_timeout,
                "{operation} drain timed out before request quiescence; forcing teardown"
            );
        }

        let released_routes = forwarding
            .release_module_endpoint_routes(target.endpoint)
            .map_err(SuperviseError::Forwarding)?;
        send_route_goodbyes(forwarding, released_routes);
        send_module_goodbye(&spec.module_id, forwarding, target);
    }

    Ok(())
}

async fn wait_for_registration_after_reload(
    registry: &Registry,
    module_id: &str,
    child: &mut Child,
    wait: Duration,
) -> Result<RegistrationWaitOutcome, SuperviseError> {
    let deadline = Instant::now() + wait;
    loop {
        if registry
            .get_module(module_id)
            .map_err(SuperviseError::Registry)?
            .is_some()
        {
            return Ok(RegistrationWaitOutcome::Registered);
        }

        let now = Instant::now();
        if now >= deadline {
            return Ok(RegistrationWaitOutcome::TimedOut);
        }
        let remaining = deadline.saturating_duration_since(now);
        let poll = remaining.min(REGISTRY_RELEASE_POLL);

        tokio::select! {
            wait_result = child.wait() => {
                let status = wait_result.map_err(|source| SuperviseError::Wait {
                    module_id: module_id.to_string(),
                    source,
                })?;
                return Ok(RegistrationWaitOutcome::Exited(classify_exit(&status)));
            }
            _ = sleep(poll) => {}
        }
    }
}

fn registration_failure_exit_report(mut exit_report: ExitReport) -> ExitReport {
    // A replacement process that exits before HELLO did not provide service, even
    // if it used status 0. Count it against the restart cap as a new-binary failure.
    exit_report.kind = ExitKind::Crash;
    exit_report
}

async fn handle_reload_child_registration_failure(
    spec: &ModuleSpec,
    runtime: &SupervisorRuntimeConfig,
    registry: &Registry,
    process_liveness: &SupervisorProcessLiveness,
    snapshot: &SharedSnapshot,
    child: &mut Option<Child>,
    failure: ReloadRegistrationFailure,
) -> Result<(), SuperviseError> {
    let ReloadRegistrationFailure {
        exit_report,
        reason,
    } = failure;
    match on_child_exit(
        spec,
        runtime.restart_policy,
        registry,
        snapshot,
        exit_report,
    )
    .await
    {
        NextAction::Stop {
            registration_released,
        } => {
            if registration_released {
                process_liveness.untrack_if_current(&spec.module_id, snapshot);
            }
        }
        NextAction::Restart => {
            sleep(runtime.restart_policy.backoff).await;
            if let Err(err) =
                wait_for_registration_release(registry, &spec.module_id, REGISTRY_RELEASE_TIMEOUT)
                    .await
            {
                fail_snapshot(snapshot, Some(&spec.module_id), None);
                process_liveness.untrack_if_current(&spec.module_id, snapshot);
                return Err(SuperviseError::ReloadFailed {
                    module_id: spec.module_id.clone(),
                    reason: format!(
                        "{reason}; registration did not release before policy retry: {err}"
                    ),
                });
            }
            process_liveness.track(spec.module_id.clone(), Arc::clone(snapshot));
            match spawn_and_mark_running(spec, runtime, snapshot) {
                Ok(next_child) => {
                    *child = Some(next_child);
                }
                Err(err) => {
                    fail_snapshot(snapshot, Some(&spec.module_id), None);
                    process_liveness.untrack_if_current(&spec.module_id, snapshot);
                    return Err(SuperviseError::ReloadFailed {
                        module_id: spec.module_id.clone(),
                        reason: format!("{reason}; policy retry spawn failed: {err}"),
                    });
                }
            }
        }
    }

    Err(SuperviseError::ReloadFailed {
        module_id: spec.module_id.clone(),
        reason,
    })
}

async fn handle_reload_spawn_failure(
    spec: &ModuleSpec,
    runtime: &SupervisorRuntimeConfig,
    process_liveness: &SupervisorProcessLiveness,
    snapshot: &SharedSnapshot,
    child: &mut Option<Child>,
    reason: String,
) -> Result<(), SuperviseError> {
    let mut should_retry = false;
    update_snapshot(snapshot, Some(&spec.module_id), |state| {
        state.process_alive = false;
        state.pid = None;
        if !state.enabled {
            state.state = ModuleState::Disabled;
        } else if state.restart_count >= runtime.restart_policy.max_restarts {
            state.state = ModuleState::Failed;
        } else {
            state.restart_count += 1;
            state.state = ModuleState::Restarting;
            should_retry = true;
        }
    })?;

    if should_retry {
        sleep(runtime.restart_policy.backoff).await;
        process_liveness.track(spec.module_id.clone(), Arc::clone(snapshot));
        match spawn_and_mark_running(spec, runtime, snapshot) {
            Ok(next_child) => {
                *child = Some(next_child);
            }
            Err(err) => {
                fail_snapshot(snapshot, Some(&spec.module_id), None);
                process_liveness.untrack_if_current(&spec.module_id, snapshot);
                return Err(SuperviseError::ReloadFailed {
                    module_id: spec.module_id.clone(),
                    reason: format!("{reason}; policy retry spawn failed: {err}"),
                });
            }
        }
    } else {
        process_liveness.untrack_if_current(&spec.module_id, snapshot);
    }

    Err(SuperviseError::ReloadFailed {
        module_id: spec.module_id.clone(),
        reason,
    })
}

fn control_flags() -> Flags {
    Flags::new(false, Priority::Passive, false)
}

async fn drain_optional_child(
    module_id: &str,
    registry: &Registry,
    snapshot: &SharedSnapshot,
    child: &mut Option<Child>,
    drain_timeout: Duration,
    final_state: ModuleState,
    enabled: Option<bool>,
) -> Result<(), SuperviseError> {
    if let Some(child) = child.take() {
        drain_child_to_state(
            module_id,
            registry,
            snapshot,
            child,
            drain_timeout,
            final_state,
            enabled,
        )
        .await
    } else {
        update_snapshot(snapshot, Some(module_id), |state| {
            state.state = final_state;
            if let Some(enabled) = enabled {
                state.enabled = enabled;
            }
            state.process_alive = false;
            state.pid = None;
        })?;
        wait_for_registration_release(registry, module_id, REGISTRY_RELEASE_TIMEOUT).await
    }
}

async fn drain_child_to_state(
    module_id: &str,
    registry: &Registry,
    snapshot: &SharedSnapshot,
    mut child: Child,
    drain_timeout: Duration,
    final_state: ModuleState,
    enabled: Option<bool>,
) -> Result<(), SuperviseError> {
    update_snapshot(snapshot, Some(module_id), |state| {
        state.state = ModuleState::Draining;
        if let Some(enabled) = enabled {
            state.enabled = enabled;
        }
    })?;

    let exit_report = match timeout(drain_timeout, child.wait()).await {
        Ok(Ok(status)) => classify_exit(&status),
        Ok(Err(source)) => {
            fail_snapshot(snapshot, Some(module_id), None);
            return Err(SuperviseError::Wait {
                module_id: module_id.to_string(),
                source,
            });
        }
        Err(_) => {
            child.start_kill().map_err(|source| SuperviseError::Kill {
                module_id: module_id.to_string(),
                source,
            })?;
            let status = child.wait().await.map_err(|source| SuperviseError::Wait {
                module_id: module_id.to_string(),
                source,
            })?;
            classify_exit(&status)
        }
    };

    update_snapshot(snapshot, Some(module_id), |state| {
        state.state = final_state;
        if let Some(enabled) = enabled {
            state.enabled = enabled;
        }
        state.process_alive = false;
        state.pid = None;
        state.last_exit = Some(exit_report);
    })?;

    wait_for_registration_release(registry, module_id, REGISTRY_RELEASE_TIMEOUT).await
}

async fn wait_for_registration_release(
    registry: &Registry,
    module_id: &str,
    wait: Duration,
) -> Result<(), SuperviseError> {
    let deadline = Instant::now() + wait;
    let mut release_events = registration_release_events().subscribe();
    loop {
        let _observed_generation = *release_events.borrow_and_update();
        if registry
            .get_module(module_id)
            .map_err(SuperviseError::Registry)?
            .is_none()
        {
            return Ok(());
        }

        let now = Instant::now();
        if now >= deadline {
            return Err(SuperviseError::RegistrationStillActive {
                module_id: module_id.to_string(),
                waited: wait,
            });
        }

        let remaining = deadline.saturating_duration_since(now);
        match timeout(remaining, release_events.changed()).await {
            Ok(Ok(())) | Ok(Err(_)) => {}
            Err(_) => {
                return Err(SuperviseError::RegistrationStillActive {
                    module_id: module_id.to_string(),
                    waited: wait,
                })
            }
        }
    }
}

fn classify_exit(status: &ExitStatus) -> ExitReport {
    ExitReport {
        kind: if status.success() {
            ExitKind::Clean
        } else {
            ExitKind::Crash
        },
        code: status.code(),
        signal: exit_signal(status),
    }
}

#[cfg(unix)]
fn exit_signal(status: &ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;

    status.signal()
}

#[cfg(not(unix))]
fn exit_signal(_status: &ExitStatus) -> Option<i32> {
    None
}

fn reset_restart_count(snapshot: &SharedSnapshot, module_id: &str) -> Result<(), SuperviseError> {
    update_snapshot(snapshot, Some(module_id), |state| {
        state.restart_count = 0;
    })
}

fn set_running(snapshot: &SharedSnapshot, pid: Option<u32>) -> Result<(), SuperviseError> {
    update_snapshot(snapshot, None, |state| {
        state.state = ModuleState::Running;
        state.enabled = true;
        state.process_alive = true;
        state.pid = pid;
    })
}

fn fail_snapshot(
    snapshot: &SharedSnapshot,
    module_id: Option<&str>,
    last_exit: Option<ExitReport>,
) {
    if let Err(err) = update_snapshot(snapshot, module_id, |state| {
        state.state = ModuleState::Failed;
        state.process_alive = false;
        state.pid = None;
        if let Some(last_exit) = last_exit {
            state.last_exit = Some(last_exit);
        }
    }) {
        error!(error = %err, "failed to mark supervisor state failed");
    }
}

fn update_snapshot(
    snapshot: &SharedSnapshot,
    module_id: Option<&str>,
    update: impl FnOnce(&mut SupervisorSnapshot),
) -> Result<(), SuperviseError> {
    let mut state = snapshot.lock().map_err(|_| SuperviseError::StatePoisoned {
        module_id: module_id.map(ToOwned::to_owned),
    })?;
    update(&mut state);
    Ok(())
}

fn lock_snapshot(
    snapshot: &SharedSnapshot,
) -> Result<std::sync::MutexGuard<'_, SupervisorSnapshot>, SuperviseError> {
    snapshot
        .lock()
        .map_err(|_| SuperviseError::StatePoisoned { module_id: None })
}

#[cfg(test)]
mod jitter_tests {
    use super::jittered_health_delay;
    use std::{collections::HashSet, time::Duration};

    /// The module ids supervised in production, so the dispersal claim is about
    /// the fleet that actually runs rather than about invented names.
    const FLEET: [&str; 14] = [
        "aft",
        "alfonso-core",
        "magic-context",
        "broca",
        "thalamus",
        "quota",
        "engram",
        "plexus",
        "cerebellum",
        "astrocyte",
        "synapse",
        "subc-mcp",
        "cortexkit-credentials",
        "subc-federation",
    ];

    /// Probes must not converge after a fleet-wide restart.
    ///
    /// This is the property the jitter exists for: every module reconnects at
    /// once, and without dispersal all fourteen would then probe on the same
    /// tick forever. Nothing failed visibly when this went untested -- a
    /// convergent fleet still probes correctly, just in a burst, so the symptom
    /// is a periodic load spike that looks like whatever else is running.
    #[test]
    fn probe_delays_disperse_across_the_fleet() {
        let cadence = Duration::from_secs(30);
        let delays: HashSet<Duration> = FLEET
            .iter()
            .map(|id| jittered_health_delay(id, 0, cadence))
            .collect();
        assert_eq!(
            delays.len(),
            FLEET.len(),
            "every supervised module must land on its own probe offset"
        );
    }

    /// The offset may only ever DELAY a probe, never bring it forward.
    ///
    /// A delay below the cadence would probe a module more often than
    /// configured, which is the opposite of what an operator asked for and
    /// would tighten the failure budget without anyone changing it.
    #[test]
    fn jitter_only_delays_and_stays_within_one_tenth_of_cadence() {
        let cadence = Duration::from_secs(30);
        let span = cadence / 10;
        for id in FLEET {
            for probe_index in 0..8 {
                let delay = jittered_health_delay(id, probe_index, cadence);
                assert!(
                    delay >= cadence,
                    "{id}#{probe_index}: jitter must not shorten the cadence"
                );
                assert!(
                    delay < cadence + span,
                    "{id}#{probe_index}: jitter must stay inside one tenth of the cadence"
                );
            }
        }
    }

    /// A module keeps its offset across daemon restarts.
    ///
    /// The delay is derived rather than randomised precisely so a restart does
    /// not re-roll every module into a fresh chance of collision. A random
    /// source would satisfy the dispersal test above and quietly lose this.
    #[test]
    fn a_module_offset_is_stable_across_restarts() {
        let cadence = Duration::from_secs(30);
        for id in FLEET {
            assert_eq!(
                jittered_health_delay(id, 0, cadence),
                jittered_health_delay(id, 0, cadence),
                "{id}: the same module and probe index must produce the same offset"
            );
        }
    }

    /// A zero cadence disables probing rather than producing a busy loop.
    #[test]
    fn zero_cadence_yields_zero_delay() {
        assert_eq!(
            jittered_health_delay("aft", 0, Duration::ZERO),
            Duration::ZERO
        );
    }
}
