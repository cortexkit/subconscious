use std::{
    collections::HashMap,
    error::Error,
    fmt, io,
    path::PathBuf,
    process::{ExitStatus, Stdio},
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde_json::Value;
use subc_control::{
    ClientControlPush, RouteCloseReason, SupervisorHealthStatus, TerminalDisposition,
};
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
    provenance::{spawned_file_identity, ExecutableIdentityProbe, SpawnedFileIdentity},
    registry::RegistryError,
    stderr_tail::{pump_stderr, StderrRing, StderrTailConfig, StderrTailSnapshot},
    terminal_ring::{TerminalHistorySnapshot, TerminalRecord, TerminalRing, TerminalRingConfig},
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
/// How long a drain waits for already-dispatched requests to finalize before
/// the child is torn down. Sized for TOOL-SCALE work (bash, inspect, builds),
/// not RPC-scale: the original 2s value silently cut nearly every real tool
/// call at the fence, making the wait-for-finalize design decorative for the
/// workloads it existed for. Quiescence short-circuits, so an idle module
/// restarts immediately regardless of this value; the budget is spent only
/// when a genuine in-flight request is worth finishing. Per-module override:
/// `drain_timeout_ms` in subc.jsonc; per-restart override: the operator's
/// `supervisor.restart{drain_timeout_ms}` (0 = cut now, for wedge bounces
/// where a stuck request will never settle).
const DEFAULT_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);
const REGISTRY_RELEASE_TIMEOUT: Duration = Duration::from_secs(1);
const REGISTRY_RELEASE_POLL: Duration = Duration::from_millis(10);
const STDERR_PUMP_DRAIN_TIMEOUT: Duration = Duration::from_millis(250);

struct SupervisedChild {
    child: Child,
    stderr_pump: Option<JoinHandle<()>>,
    stderr_ring: Arc<Mutex<StderrRing>>,
    spawned_at_ms: u64,
    spawned_from: PathBuf,
    spawned_file_identity: Option<SpawnedFileIdentity>,
    process_start_time: Option<u64>,
}

impl SupervisedChild {
    fn id(&self) -> Option<u32> {
        self.child.id()
    }

    async fn wait(&mut self) -> io::Result<ExitStatus> {
        self.child.wait().await
    }

    fn start_kill(&mut self) -> io::Result<()> {
        self.child.start_kill()
    }

    async fn drain_stderr(&mut self, module_id: &str) {
        let Some(mut pump) = self.stderr_pump.take() else {
            return;
        };
        match timeout(STDERR_PUMP_DRAIN_TIMEOUT, &mut pump).await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                self.stderr_ring
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .mark_incomplete(format!("stderr pump ended unexpectedly: {err}"));
                warn!(module_id, error = %err, "stderr pump ended before clean EOF");
            }
            Err(_) => {
                pump.abort();
                self.stderr_ring
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .mark_incomplete(format!(
                        "stderr pump did not reach EOF within {:?} before restart",
                        STDERR_PUMP_DRAIN_TIMEOUT
                    ));
                warn!(
                    module_id,
                    waited = ?STDERR_PUMP_DRAIN_TIMEOUT,
                    "stderr pump did not drain before restart; stopped it before marking the new process"
                );
            }
        }
    }
}

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

pub(crate) fn daemon_will_restart(enabled: bool, restart_count: u32, max_restarts: u32) -> bool {
    enabled && restart_count < max_restarts
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

/// The supervisor's view of one module's health, relayed to clients over
/// channel-0 and rendered by `ck health`.
///
/// THIS TYPE IS WHERE THE ABSENCE MEANINGS ARE CREATED, which is why they are
/// stated here rather than only at the wire type a consumer reads. A reader can
/// look up what `None` means; only a writer can silently change it, and the
/// writer has no reason to go looking at a downstream contract before editing.
///
/// `last_probe_ms: None` MEANS NEVER PROBED, not probed-long-ago. It is cleared
/// back to `None` on re-registration precisely so a respawned module does not
/// carry its predecessor's timestamp — so an old value and an absent one call for
/// opposite readings, and anything that defaulted this to a number would make a
/// never-probed module indistinguishable from one probed at the epoch.
///
/// `detail` and `metrics` are `None` when the module published none on this
/// probe, which does not mean it reported nothing wrong — it is also the shape
/// when the probe never reached it. `last_probe_ms` is what separates those.
#[derive(Debug, Clone, PartialEq)]
pub struct ModuleHealthStatus {
    pub status: SupervisorHealthStatus,
    pub last_probe_ms: Option<u64>,
    pub detail: Option<String>,
    pub metrics: Option<Value>,
    pub consecutive_failures: u32,
    /// Number of replies received after a recurring health probe's deadline.
    /// Unlike a timeout, every increment proves the module was alive.
    pub late_answer_count: u64,
    /// End-to-end latency of the newest late reply, measured from probe start.
    pub last_late_answer_latency_ms: Option<u64>,
    pub last_action: Option<String>,
    /// Set together with `last_action`; the pair moves as one, and both being
    /// absent means no escalation has ever been taken rather than that the last
    /// one succeeded.
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
            late_answer_count: 0,
            last_late_answer_latency_ms: None,
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
    pub at_ms: u64,
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
    /// The budget `restart_count` is spent against. Carried alongside the count
    /// because the count alone does not say how close the module is to being
    /// disabled, and reporting one without the other is what makes an
    /// about-to-be-retired module look ordinary.
    pub max_restarts: u32,
    pub pid: Option<u32>,
    pub spawned_at_ms: Option<u64>,
    pub spawned_from: Option<PathBuf>,
    pub process_start_time: Option<u64>,
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
    spawned_at_ms: Option<u64>,
    spawned_from: Option<PathBuf>,
    spawned_file_identity: Option<SpawnedFileIdentity>,
    process_start_time: Option<u64>,
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
            spawned_at_ms: None,
            spawned_from: None,
            spawned_file_identity: None,
            process_start_time: None,
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
    /// This module's RESOLVED drain budget: per-module config when present,
    /// else `default_drain_timeout`.
    drain_timeout: Duration,
    /// The supervisor-wide fallback, kept so a configuration update that
    /// REMOVES the per-module override can re-resolve to it.
    default_drain_timeout: Duration,
    health: HealthConfig,
    connection_file_path: Option<PathBuf>,
    forwarding: Option<Arc<ForwardingTable>>,
    /// The shared handle, so every spawn path (initial, restart, reload) records the
    /// reserved-module launch nonce the HELLO verifier checks against.
    supervisor_handle: Option<SupervisorHandle>,
    /// This module's stderr tail, shared with the [`SupervisedModule`] that answers
    /// status queries.
    ///
    /// One ring per module, held across every respawn. The lines explaining an exit
    /// are written BEFORE that exit, so a ring recreated per process would be empty
    /// exactly when it is asked for.
    stderr_ring: Arc<Mutex<StderrRing>>,
    terminal_ring: Arc<Mutex<TerminalRing>>,
    #[cfg(test)]
    test_seed_stale_facts_before_enable_spawn: bool,
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
    /// Reserved module ids and the nonce that authorizes their next HELLO.
    /// `None` means RESERVED WITH NO LEGITIMATE HOLDER — a reserved module that
    /// has never been spawned (e.g. configured `enabled: false`) — and refuses
    /// every HELLO. Before this was expressible, a reserved-but-never-spawned id
    /// had NO entry and admitted anyone: the reservation protected the nonce
    /// holder, not the NAME (found live by CKCRED's canary probe registering
    /// against a reserved scratch id).
    reserved_nonces: Arc<Mutex<HashMap<String, Option<String>>>>,
    /// Module ids removed by an executed rescan and the unix-millisecond removal time.
    ///
    /// This is deliberately in-memory only: subc is state-free across daemon
    /// restarts, and the tombstone only explains the hours-after-removal window
    /// while this executing daemon is still alive. Do not persist it in a store.
    removal_tombstones: Arc<Mutex<HashMap<String, u64>>>,
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
            .insert(module_id.to_string(), Some(nonce));
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
            // `None` (no spawn nonce minted) is INSERTED, not skipped: a
            // reserved name whose module has never spawned has no legitimate
            // holder, and the entry's absence is what used to leave the name
            // open to the first claimant.
            reserved_nonces.insert(spec.module_id.clone(), spawn_nonce);
        }
        drop(reserved_nonces);
        // A later unreserved declaration must not silently unreserve an id that
        // was retained after its reserved configuration was removed. The explicit
        // release ceremony is the only operation that retires that gate.
        self.removal_tombstones
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&spec.module_id);
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
            // `None` = reserved with no legitimate holder: refuse every
            // presentation, because no process can hold a nonce that was never
            // minted. Only a real minted nonce admits, in constant time.
            let authorized = match expected {
                Some(expected) => {
                    presented.is_some_and(|p| constant_time_eq(expected.as_bytes(), p.as_bytes()))
                }
                None => false,
            };
            if authorized {
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
            .flatten()
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

    pub(crate) fn record_late_health_answer(
        &self,
        module_id: &str,
        latency_ms: u64,
    ) -> Result<bool, SuperviseError> {
        let Some(module) = self.get(module_id) else {
            return Ok(false);
        };
        update_snapshot(&module.inner.snapshot, Some(module_id), |state| {
            state.health.late_answer_count = state.health.late_answer_count.saturating_add(1);
            state.health.last_late_answer_latency_ms = Some(latency_ms);
            // A late answer is an answer: the module served the probe, just past
            // the deadline. Leaving the miss streak in place while logging
            // "proves the module is alive" is how a CPU-starved module that
            // answers every probe a few seconds late still marches to the
            // threshold and gets killed — the exact kill class `NoAnswer` is
            // excluded from `is_proof_of_death` to prevent. Slow-but-answering
            // is degradation, and degradation reports; it does not restart.
            state.health.consecutive_failures = 0;
        })?;
        Ok(true)
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
        let mut reserved_nonces = self
            .reserved_nonces
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if reserved_nonces.contains_key(module_id) {
            // The old nonce must die with the removed process, but the exact-id
            // gate remains until an operator explicitly releases it.
            reserved_nonces.insert(module_id.to_string(), None);
        }
        drop(reserved_nonces);
        self.reserved_prefix_owners
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|_, owner| owner != module_id);
        self.modules
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(module_id)
    }

    /// Remember a module removed by a non-preview rescan so route.open can
    /// distinguish that intentional removal from an unknown id.
    pub(crate) fn record_rescan_removal(&self, module_id: &str) {
        self.removal_tombstones
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(module_id.to_string(), unix_ms_now());
    }

    /// Return how long ago a rescan removed this module in milliseconds.
    pub(crate) fn removal_tombstone_age_ms(&self, module_id: &str) -> Option<u64> {
        self.removal_tombstones
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(module_id)
            .copied()
            .map(|removed_at_ms| unix_ms_now().saturating_sub(removed_at_ms))
    }

    /// Retire a reserved-id gate only after its module has left supervision.
    ///
    /// A retained gate has no live nonce (`None`), so releasing any other entry
    /// would weaken a currently configured or otherwise active reservation.
    pub(crate) fn release_retained_reserved_gate(&self, module_id: &str) -> bool {
        if self.get(module_id).is_some() {
            return false;
        }
        let mut reserved_nonces = self
            .reserved_nonces
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !matches!(reserved_nonces.get(module_id), Some(None)) {
            return false;
        }
        reserved_nonces.remove(module_id);
        true
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
    daemon_started_at_ms: u64,
    provenance_probe: ExecutableIdentityProbe,
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
            daemon_started_at_ms: unix_ms_now(),
            provenance_probe: ExecutableIdentityProbe::default(),
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
            &runtime.stderr_ring,
        )?;
        set_running(&snapshot, &child)?;
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
            &runtime.stderr_ring,
        ) {
            Ok(child) => {
                set_running(&snapshot, &child)?;
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
        drain_timeout_ms: Option<u64>,
    ) -> Result<SupervisedModule, SuperviseError> {
        validate_spec(&spec)?;

        let mut runtime = self.runtime_config();
        runtime.health = health;
        if let Some(ms) = drain_timeout_ms {
            runtime.drain_timeout = Duration::from_millis(ms);
        }
        if !enabled {
            let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::disabled()));
            return Ok(self.supervised_module(spec, runtime, snapshot, None));
        }

        let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::starting()));
        match spawn_child(
            &spec,
            runtime.connection_file_path.as_deref(),
            self.supervisor_handle.as_ref(),
            &runtime.stderr_ring,
        ) {
            Ok(child) => {
                set_running(&snapshot, &child)?;
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
            default_drain_timeout: self.drain_timeout,
            health: self.health,
            connection_file_path: self.connection_file_path.clone(),
            forwarding: self.forwarding.clone(),
            supervisor_handle: self.supervisor_handle.clone(),
            stderr_ring: Arc::new(Mutex::new(StderrRing::new(StderrTailConfig::default()))),
            terminal_ring: Arc::new(Mutex::new(TerminalRing::new(
                TerminalRingConfig::default(),
                self.daemon_started_at_ms,
            ))),
            #[cfg(test)]
            test_seed_stale_facts_before_enable_spawn: false,
        }
    }

    fn supervised_module(
        &self,
        spec: ModuleSpec,
        runtime: SupervisorRuntimeConfig,
        snapshot: SharedSnapshot,
        child: Option<SupervisedChild>,
    ) -> SupervisedModule {
        let configuration = Arc::new(Mutex::new(SupervisedConfiguration {
            spec: spec.clone(),
            health: runtime.health,
        }));
        let stderr_ring = Arc::clone(&runtime.stderr_ring);
        let terminal_ring = Arc::clone(&runtime.terminal_ring);
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
                stderr_ring,
                terminal_ring,
                commands: tx,
                monitor: Mutex::new(Some(monitor)),
                max_restarts: self.restart_policy.max_restarts,
                provenance_probe: self.provenance_probe.clone(),
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
    stderr_ring: Arc<Mutex<StderrRing>>,
    terminal_ring: Arc<Mutex<TerminalRing>>,
    commands: mpsc::Sender<SupervisorCommand>,
    monitor: Mutex<Option<JoinHandle<()>>>,
    /// Copied from the supervisor's runtime config at spawn so `status()` can
    /// report the restart budget without reaching back into the supervisor. The
    /// policy is fixed for the process's lifetime, so a copy cannot drift.
    max_restarts: u32,
    provenance_probe: ExecutableIdentityProbe,
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

    /// Test-only: put one probe miss on the streak, the way
    /// `handle_health_probe_failure` does, so tests can assert what a later
    /// event does to the streak without driving the whole probe loop.
    #[cfg(test)]
    pub(crate) fn record_health_probe_failure_for_test(
        &self,
        detail: &str,
    ) -> Result<(), SuperviseError> {
        update_snapshot(&self.inner.snapshot, Some(&self.inner.module_id), |state| {
            state.health.consecutive_failures = state.health.consecutive_failures.saturating_add(1);
            state.health.detail = Some(detail.to_string());
        })
    }

    pub fn state(&self) -> Result<ModuleState, SuperviseError> {
        Ok(lock_snapshot(&self.inner.snapshot)?.state)
    }

    /// The module's retained stderr, newest lines last.
    ///
    /// Deliberately NOT on [`Self::status`]: a bounded tail is kilobytes per
    /// module, `supervisor.list` renders every module, and putting it in the
    /// shared snapshot would make each status read carry a payload almost nobody
    /// asked for. Callers that want the text ask for it.
    pub fn stderr_tail(
        &self,
        max_lines: Option<usize>,
        max_bytes: Option<usize>,
    ) -> StderrTailSnapshot {
        self.inner
            .stderr_ring
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .snapshot(max_lines, max_bytes)
    }

    /// The module's bounded terminal history, oldest retained exit first.
    ///
    /// The daemon-start stamp distinguishes a quiet supervisor from a replacement
    /// daemon whose in-memory history was necessarily reset.
    pub fn terminal_history(&self) -> TerminalHistorySnapshot {
        self.inner
            .terminal_ring
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .snapshot()
    }

    pub fn status(&self) -> Result<ModuleStatus, SuperviseError> {
        self.status_with_snapshot_lock(&self.inner.snapshot, None)
    }

    /// Read status for a channel-0 renderer and report a contended snapshot lock.
    ///
    /// Internal supervision callers use [`Self::status`] so writer-side machinery
    /// does not produce reader-observability logs.
    pub(crate) fn status_for_control(
        &self,
        caller: &'static str,
    ) -> Result<ModuleStatus, SuperviseError> {
        self.status_with_snapshot_lock(&self.inner.snapshot, Some(caller))
    }

    fn status_with_snapshot_lock(
        &self,
        snapshot: &SharedSnapshot,
        caller: Option<&'static str>,
    ) -> Result<ModuleStatus, SuperviseError> {
        let snapshot = match caller {
            Some(caller) => lock_snapshot_for_control(snapshot, &self.inner.module_id, caller)?,
            None => lock_snapshot(snapshot)?,
        }
        .clone();
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
            max_restarts: self.inner.max_restarts,
            pid: snapshot.pid,
            spawned_at_ms: snapshot.spawned_at_ms,
            spawned_from: snapshot.spawned_from,
            process_start_time: snapshot.process_start_time,
            last_exit: snapshot.last_exit,
            health: snapshot.health,
        })
    }

    #[cfg(test)]
    pub(crate) fn hold_snapshot_for_test(
        &self,
        acquired: std::sync::mpsc::Sender<()>,
        hold: Duration,
    ) -> std::thread::JoinHandle<()> {
        let snapshot = Arc::clone(&self.inner.snapshot);
        std::thread::spawn(move || {
            let _guard = snapshot.lock().expect("test snapshot lock is not poisoned");
            acquired
                .send(())
                .expect("test receiver waits for snapshot lock");
            std::thread::sleep(hold);
        })
    }

    pub(crate) async fn running_image_agreement(&self) -> subc_control::RunningImageAgreement {
        let snapshot = match lock_snapshot(&self.inner.snapshot) {
            Ok(snapshot) => snapshot.clone(),
            Err(_) => {
                return subc_control::RunningImageAgreement::Unavailable {
                    reason: subc_control::RunningImageUnavailableReason::NotRunning,
                };
            }
        };
        self.inner
            .provenance_probe
            .observe(
                snapshot.pid,
                snapshot.spawned_from.as_deref(),
                snapshot.spawned_file_identity,
                snapshot.process_start_time,
            )
            .await
    }

    pub(crate) fn will_recover_after_connection_loss(&self) -> Result<bool, SuperviseError> {
        let snapshot = lock_snapshot(&self.inner.snapshot)?.clone();
        Ok(match snapshot.state {
            ModuleState::Restarting => true,
            ModuleState::Failed | ModuleState::Disabled => false,
            _ => daemon_will_restart(
                snapshot.enabled,
                snapshot.restart_count,
                self.inner.max_restarts,
            ),
        })
    }

    #[cfg(test)]
    pub(crate) fn is_warming(&self) -> Result<bool, SuperviseError> {
        self.is_warming_with_snapshot_lock(None)
    }

    pub(crate) fn is_warming_for_control(
        &self,
        caller: &'static str,
    ) -> Result<bool, SuperviseError> {
        self.is_warming_with_snapshot_lock(Some(caller))
    }

    fn is_warming_with_snapshot_lock(
        &self,
        caller: Option<&'static str>,
    ) -> Result<bool, SuperviseError> {
        let snapshot = match caller {
            Some(caller) => {
                lock_snapshot_for_control(&self.inner.snapshot, &self.inner.module_id, caller)?
            }
            None => lock_snapshot(&self.inner.snapshot)?,
        }
        .clone();
        Ok(matches!(
            snapshot.state,
            ModuleState::Starting | ModuleState::Running | ModuleState::Restarting
        ))
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

    pub async fn restart(&self, drain_timeout_ms: Option<u64>) -> Result<(), SuperviseError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.inner
            .commands
            .send(SupervisorCommand::Restart {
                drain_timeout_ms,
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
        drain_timeout_ms: Option<u64>,
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
                drain_timeout_ms,
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
                clear_current_process_facts(state);
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
        /// Operator override for this one restart's drain budget, in ms. `None`
        /// uses the module's configured/default budget; `Some(0)` cuts
        /// immediately (wedge bounce: a stuck request never settles, so
        /// waiting only delays recovery).
        drain_timeout_ms: Option<u64>,
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
        /// Per-module drain override from the new config; `None` re-resolves to
        /// the supervisor-wide default.
        drain_timeout_ms: Option<u64>,
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

/// What a failed health probe actually OBSERVED, kept apart from how it reads.
///
/// This was a struct with a single `message: String`, and every one of the
/// fifteen construction sites collapsed into it. Each site knows exactly what it
/// saw -- the lane is gone, the module did not answer in time, the module
/// answered with the wrong thing -- and `handle_health_probe_failure` then
/// treated all of them identically: increment a counter, compare to a threshold,
/// restart the module. THE DISTINCTION EXISTED AT EVERY CALL SITE AND WAS
/// DESTROYED BEFORE THE DECISION THAT NEEDED IT.
///
/// The distinction that matters is not severity, it is EVIDENTIAL WEIGHT:
///
/// * `LaneDead` is PROOF. The module's control connection is gone; nothing will
///   answer on it again.
/// * `NoAnswer` is ABSENCE OF EVIDENCE. It is consistent with a wedged module
///   AND with a perfectly healthy one that lost a CPU race -- which is what
///   happens under machine load, and is how this supervisor killed a healthy
///   module three times in one day.
/// * `BadAnswer` proves the module is ALIVE. It replied; the reply was wrong.
///   Restarting on it is defensible, but it is not the silence case and should
///   never be counted as one.
/// * `Misconfigured` is a daemon-side fault. The module has not been asked
///   anything, so it cannot be evidence about the module at all.
///
/// The asymmetry is the whole point: under saturation the WEAKEST signal is the
/// one that fires most often, and while every variant collapsed into one string
/// it carried the same weight as the strongest.
///
/// LIVE BEHAVIOUR TODAY, stated here because this doc block describes the
/// DESIGN and a reader stopping at it gets the build backwards: the restart
/// decision does NOT yet consult this classification -- consecutive `NoAnswer`
/// probes still increment the failure streak and drive escalation at the
/// threshold (see `is_proof_of_death` below for why that is deliberate and
/// what gates the change). Absence of evidence restarts modules today.
#[derive(Debug)]
enum HealthProbeEvidence {
    /// The module's control lane is gone. Proof of death.
    LaneDead,
    /// No reply within the deadline. Proves nothing about the module's state.
    NoAnswer,
    /// The module replied, but not with a usable health report. Proves it is alive.
    BadAnswer,
    /// The daemon could not ask. Says nothing about the module.
    Misconfigured,
}

#[derive(Debug)]
struct HealthProbeError {
    evidence: HealthProbeEvidence,
    message: String,
}

impl HealthProbeError {
    fn lane_dead(message: impl Into<String>) -> Self {
        Self::with(HealthProbeEvidence::LaneDead, message)
    }

    fn no_answer(message: impl Into<String>) -> Self {
        Self::with(HealthProbeEvidence::NoAnswer, message)
    }

    fn bad_answer(message: impl Into<String>) -> Self {
        Self::with(HealthProbeEvidence::BadAnswer, message)
    }

    fn misconfigured(message: impl Into<String>) -> Self {
        Self::with(HealthProbeEvidence::Misconfigured, message)
    }

    fn with(evidence: HealthProbeEvidence, message: impl Into<String>) -> Self {
        Self {
            evidence,
            message: message.into(),
        }
    }

    /// Whether this observation is proof the module cannot serve.
    ///
    /// Only `LaneDead` qualifies. `NoAnswer` is deliberately excluded: it is the
    /// variant that fires under CPU starvation, and treating it as proof is the
    /// defect this enum exists to make impossible to reintroduce silently.
    ///
    /// NOT YET CONSULTED BY THE RESTART DECISION, deliberately. Requiring proof
    /// to restart also needs a bound for the case it excludes -- a genuinely
    /// wedged module, alive but never answering -- and that bound must come from
    /// the distribution of real late-answer latencies, which nothing measures
    /// yet. Landing the classification first makes the later change a one-line
    /// decision against evidence that already exists, rather than two unproven
    /// changes at once.
    #[allow(dead_code)]
    fn is_proof_of_death(&self) -> bool {
        matches!(self.evidence, HealthProbeEvidence::LaneDead)
    }

    /// Short stable label for logs and the health snapshot.
    ///
    /// An operator reading `ck health` currently cannot tell "the module is gone"
    /// from "the module did not answer in five seconds", because both render as
    /// prose in the same field. These labels are what make the two
    /// distinguishable at a glance, and they are what a later restart-policy
    /// change will be argued from.
    fn label(&self) -> &'static str {
        match self.evidence {
            HealthProbeEvidence::LaneDead => "lane-dead",
            HealthProbeEvidence::NoAnswer => "no-answer",
            HealthProbeEvidence::BadAnswer => "bad-answer",
            HealthProbeEvidence::Misconfigured => "daemon-misconfigured",
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
    child: &mut Option<SupervisedChild>,
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
        return Err(HealthProbeError::misconfigured(
            "supervisor was not configured with a forwarding table",
        ));
    };
    let probe_started_at = Instant::now();
    let deadline = probe_started_at + runtime.health.deadline;
    let pending = forwarding
        .begin_health_probe_rpc_for(
            &spec.module_id,
            MODULE_CONTROL_OP_HEALTH_CHECK,
            probe_started_at,
            deadline,
        )
        .map_err(|err| {
            // The endpoint is not registered, so there is no live control lane to
            // ask. That is the module being absent, not slow.
            HealthProbeError::lane_dead(format!("failed to begin health.check RPC: {err}"))
        })?;
    let PendingModuleControlRpc {
        endpoint,
        module_sink,
        negotiated_ver,
        corr,
        receiver,
    } = pending;
    let body = serde_json::to_vec(&ModuleControlRequest::HealthCheck {}).map_err(|err| {
        HealthProbeError::misconfigured(format!("failed to encode health.check: {err}"))
    })?;
    let frame = Frame::build_with_version(
        negotiated_ver,
        FrameType::Request,
        control_flags(),
        0,
        0,
        corr,
        body,
    )
    .map_err(|err| {
        HealthProbeError::misconfigured(format!("failed to build health.check frame: {err}"))
    })?;

    // The enqueue itself must be bounded by the probe deadline: FrameSink.send
    // blocks waiting for capacity when the module's egress queue is full, and an
    // unbounded await here freezes the whole supervision actor (it stops polling
    // Child::wait and supervisor commands), making the module unrecoverable
    // in-band. On timeout the probe fails like any transport failure.
    match timeout_at(deadline, module_sink.send(frame)).await {
        Ok(Ok(())) => {}
        Ok(Err(err)) => {
            let _ = forwarding.cancel_module_control_rpc(endpoint, corr);
            // A closed sink means the module's egress channel is gone -- the
            // receiving half is dropped when its connection tears down. Proof.
            return Err(HealthProbeError::lane_dead(format!(
                "failed to send health.check: {err}"
            )));
        }
        Err(_elapsed) => {
            let _ = forwarding.cancel_module_control_rpc(endpoint, corr);
            // A full egress queue means the module is not draining its socket, which
            // is consistent with a wedged module AND with one whose reader is merely
            // starved. Silence, not proof.
            return Err(HealthProbeError::no_answer(
                "health.check send timed out before enqueue (module egress full)",
            ));
        }
    }

    match timeout_at(deadline, receiver).await {
        // Each arm records WHAT WAS OBSERVED. Four of them are the module
        // demonstrably answering -- rejected, non-health, malformed, wrong op --
        // and those prove it is alive even though the probe failed.
        Ok(Ok(ModuleControlRpcOutcome::Response(response))) => {
            response.health_report().ok_or_else(|| {
                HealthProbeError::bad_answer("health.check RPC returned a non-health response")
            })
        }
        Ok(Ok(ModuleControlRpcOutcome::Rejected(body))) => Err(HealthProbeError::bad_answer(
            format!("health.check rejected: {}", body.message),
        )),
        Ok(Ok(ModuleControlRpcOutcome::ModuleGone(message))) => {
            Err(HealthProbeError::lane_dead(message))
        }
        Ok(Ok(ModuleControlRpcOutcome::MalformedResponse(message))) => {
            Err(HealthProbeError::bad_answer(message))
        }
        Ok(Ok(ModuleControlRpcOutcome::UnexpectedOp { expected, actual })) => {
            Err(HealthProbeError::bad_answer(format!(
                "expected module-control op '{expected}', got '{actual}'"
            )))
        }
        // A reply that crosses the deadline before this waiter observes it is
        // still proof of life. The forwarding path records its end-to-end latency
        // before delivering this classification.
        Ok(Ok(ModuleControlRpcOutcome::DeadlineElapsed)) => Err(HealthProbeError::bad_answer(
            "module answered health.check after its daemon deadline",
        )),
        Ok(Err(_)) => Err(HealthProbeError::misconfigured(
            "health.check waiter was canceled before the module responded",
        )),
        Err(_) => {
            let _ = forwarding.tombstone_health_probe_rpc(endpoint, corr);
            Err(HealthProbeError::no_answer(format!(
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
    child: &mut Option<SupervisedChild>,
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
    child: &mut Option<SupervisedChild>,
    err: HealthProbeError,
    now_ms: u64,
) {
    let threshold = runtime.health.failure_threshold.max(1);
    let mut failures = 0;
    // Carry the evidence class into the operator-visible detail. Without it,
    // "module did not answer within 5s" and "the control lane is gone" are two
    // prose strings in the same field, and the reader has to know the codebase to
    // tell which one is proof of anything.
    let detail = format!("[{}] {err}", err.label());
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
            evidence = err.label(),
            detail = %detail,
            "health.check probe failed"
        );
        return;
    }

    let _ = update_snapshot(snapshot, Some(&spec.module_id), |state| {
        state.state = ModuleState::Unresponsive;
        state.health.status = SupervisorHealthStatus::Unresponsive;
    });
    // The evidence class is logged at the kill site because this is the line an
    // operator reads after an unexplained restart. A streak of `no-answer` under
    // machine load is the known false-positive shape; a `lane-dead` is not.
    if runtime.health.critical {
        error!(
            module_id = %spec.module_id,
            status = "unresponsive",
            evidence = err.label(),
            detail = %detail,
            "critical module health alert"
        );
    } else {
        warn!(
            module_id = %spec.module_id,
            status = "unresponsive",
            evidence = err.label(),
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
    child: &mut Option<SupervisedChild>,
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
    child: &mut Option<SupervisedChild>,
    status: SupervisorHealthStatus,
    detail: Option<&str>,
    now_ms: u64,
) -> Result<(), SuperviseError> {
    let (enabled, will_restart) = {
        let state = lock_snapshot(snapshot)?;
        (
            state.enabled,
            daemon_will_restart(
                state.enabled,
                state.restart_count,
                runtime.restart_policy.max_restarts,
            ),
        )
    };

    if !enabled {
        return Err(SuperviseError::Disabled {
            module_id: spec.module_id.clone(),
        });
    }

    if !will_restart {
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
            RouteCloseReason::Disable,
        )
        .await?;
        drain_optional_child(
            &spec.module_id,
            registry,
            snapshot,
            &runtime.terminal_ring,
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

    begin_forwarding_drain_if_configured(
        spec,
        runtime,
        snapshot,
        Some(true),
        RouteCloseReason::Restart,
    )
    .await?;
    drain_optional_child(
        &spec.module_id,
        registry,
        snapshot,
        &runtime.terminal_ring,
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

/// Caps the metrics blob stored in the cached supervisor snapshot, which is
/// returned to every `supervisor.list` and `supervisor.health` caller.
///
/// This cap is deliberately NOT applied on the one-shot `supervisor.health_probe`
/// path: that request exists to return a module's complete metrics object, and
/// `ck health <module-id>` documents it as the way to see what the cached view
/// truncates. The asymmetry is the feature.
///
/// So a new caller must decide which side it is on rather than assume the cap is
/// universal. Reaching for it on a fresh-probe path would silently reintroduce
/// the truncation that path exists to avoid.
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
    // Note that this never returns less than one cadence, including for the FIRST
    // probe. So a freshly registered module reports health `unknown` for a full
    // cadence plus jitter -- 30-33s at the default -- no matter how quickly it is
    // ready to answer.
    //
    // That is a property of the supervisor's schedule, not of any module: an
    // operator watching a restart sees `unknown` and cannot tell it from a module
    // that is slow to warm. Measured on two unrelated modules, both flipping to
    // `ok` between 22s and 32s after restart.
    //
    // Left as-is because spreading the first probe is what keeps a fleet-wide
    // restart from firing fourteen simultaneous probes into a cold machine. The
    // alternative -- probe at t+0 and jitter only from the second onward -- trades
    // that thundering herd for a faster first reading.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readding_a_module_clears_its_rescan_removal_tombstone() {
        let handle = SupervisorHandle::new();
        let module_id = "readded-tombstone";
        handle.record_rescan_removal(module_id);
        assert!(handle.removal_tombstone_age_ms(module_id).is_some());

        handle.apply_identity_configuration(&ModuleSpec {
            module_id: module_id.to_string(),
            program: PathBuf::from("/test/module"),
            args: Vec::new(),
            env: Vec::new(),
            reserved: false,
            reserved_prefixes: Vec::new(),
        });

        assert!(
            handle.removal_tombstone_age_ms(module_id).is_none(),
            "a re-added module must not retain a stale removal tombstone"
        );
    }

    fn stale_process_snapshot(state: ModuleState, enabled: bool) -> SharedSnapshot {
        let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::new(state, enabled)));
        update_snapshot(&snapshot, Some("stale-process-facts"), |snapshot| {
            snapshot.process_alive = true;
            snapshot.pid = Some(41);
            snapshot.spawned_at_ms = Some(42);
            snapshot.spawned_from = Some(PathBuf::from("/spawned/module"));
            snapshot.spawned_file_identity = Some(SpawnedFileIdentity {
                device: 43,
                inode: 44,
            });
        })
        .unwrap();
        snapshot
    }

    fn assert_snapshot_process_facts_cleared(snapshot: &SharedSnapshot) {
        let snapshot = lock_snapshot(snapshot).unwrap();
        assert!(!snapshot.process_alive);
        assert_eq!(snapshot.pid, None);
        assert_eq!(snapshot.spawned_at_ms, None);
        assert_eq!(snapshot.spawned_from, None);
        assert_eq!(snapshot.spawned_file_identity, None);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn failed_enable_spawn_clears_preexisting_current_process_facts() {
        let supervisor = Supervisor::default();
        let mut runtime = supervisor.runtime_config();
        runtime.test_seed_stale_facts_before_enable_spawn = true;
        let snapshot = stale_process_snapshot(ModuleState::Disabled, false);
        let mut child = None;
        let spec = ModuleSpec {
            module_id: "failed-enable-clears-facts".to_string(),
            program: PathBuf::from("/definitely/missing/failed-enable-module"),
            args: Vec::new(),
            env: Vec::new(),
            reserved: false,
            reserved_prefixes: Vec::new(),
        };

        let result = set_child_enabled(
            &spec,
            &runtime,
            &supervisor.registry,
            &supervisor.process_liveness,
            &snapshot,
            &mut child,
            true,
        )
        .await;

        assert!(matches!(result, Err(SuperviseError::Spawn { .. })));
        assert_eq!(lock_snapshot(&snapshot).unwrap().state, ModuleState::Failed);
        assert_snapshot_process_facts_cleared(&snapshot);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn failed_reload_spawn_clears_current_process_facts() {
        let supervisor = Supervisor::default();
        let mut runtime = supervisor.runtime_config();
        runtime.restart_policy = RestartPolicy::new(0, Duration::ZERO);
        let snapshot = stale_process_snapshot(ModuleState::Running, true);
        let mut child = None;
        let spec = ModuleSpec {
            module_id: "failed-reload-clears-facts".to_string(),
            program: PathBuf::from("/unused/failed-reload-module"),
            args: Vec::new(),
            env: Vec::new(),
            reserved: false,
            reserved_prefixes: Vec::new(),
        };

        let result = handle_reload_spawn_failure(
            &spec,
            &runtime,
            &supervisor.process_liveness,
            &snapshot,
            &mut child,
            "forced reload spawn failure".to_string(),
        )
        .await;

        assert!(matches!(result, Err(SuperviseError::ReloadFailed { .. })));
        assert_eq!(lock_snapshot(&snapshot).unwrap().state, ModuleState::Failed);
        assert_snapshot_process_facts_cleared(&snapshot);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_a_module_with_an_active_monitor_clears_current_process_facts() {
        let supervisor = Supervisor::default();
        let snapshot = stale_process_snapshot(ModuleState::Running, true);
        let module = supervisor.supervised_module(
            ModuleSpec {
                module_id: "drop-clears-facts".to_string(),
                program: PathBuf::from("/unused/drop-module"),
                args: Vec::new(),
                env: Vec::new(),
                reserved: false,
                reserved_prefixes: Vec::new(),
            },
            supervisor.runtime_config(),
            Arc::clone(&snapshot),
            None,
        );
        assert!(!module
            .inner
            .monitor
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .is_finished());

        drop(module);

        assert_eq!(
            lock_snapshot(&snapshot).unwrap().state,
            ModuleState::Stopped
        );
        assert_snapshot_process_facts_cleared(&snapshot);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn configuration_update_does_not_replace_captured_running_process_facts() {
        let supervisor = Supervisor::default();
        let snapshot = stale_process_snapshot(ModuleState::Running, true);
        let initial = ModuleSpec {
            module_id: "rescan-preserves-spawn-facts".to_string(),
            program: PathBuf::from("/spawned/module"),
            args: Vec::new(),
            env: Vec::new(),
            reserved: false,
            reserved_prefixes: Vec::new(),
        };
        let module = supervisor.supervised_module(
            initial.clone(),
            supervisor.runtime_config(),
            snapshot,
            None,
        );
        let before = module.status().unwrap();
        let mut replacement = initial;
        replacement.program = PathBuf::from("/rescanned/replacement-module");

        module
            .update_configuration(replacement, HealthConfig::default(), None)
            .await
            .unwrap();

        let after = module.status().unwrap();
        assert_eq!(after.pid, before.pid);
        assert_eq!(after.spawned_at_ms, before.spawned_at_ms);
        assert_eq!(after.spawned_from, before.spawned_from);
        drop(module);
    }
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
    mut child: Option<SupervisedChild>,
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
                            active_child.drain_stderr(&spec.module_id).await;
                            fail_snapshot(&snapshot, Some(&spec.module_id), None);
                            // Every other exit path (on_child_exit's Clean/Crash arms,
                            // the reload-registration-failure path) records a terminal
                            // before moving on. Without one here, a module whose wait()
                            // itself errored (e.g. already reaped) leaves no terminal
                            // record at all -- an empty ring reads as "nothing died".
                            record_terminal(
                                &runtime.terminal_ring,
                                &wait_error_exit_report(),
                                TerminalDisposition::Failed,
                            );
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
                    active_child.drain_stderr(&spec.module_id).await;

                    match on_child_exit(
                        &spec,
                        runtime.restart_policy,
                        &registry,
                        &snapshot,
                        &runtime.terminal_ring,
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
    child: &mut Option<SupervisedChild>,
) -> bool {
    match command {
        SupervisorCommand::Drain { reply } => {
            let result = drain_optional_child(
                &spec.module_id,
                registry,
                snapshot,
                &runtime.terminal_ring,
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
                    RouteCloseReason::Disable,
                )
                .await?;
                drain_optional_child(
                    &spec.module_id,
                    registry,
                    snapshot,
                    &runtime.terminal_ring,
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
        SupervisorCommand::Restart {
            drain_timeout_ms,
            reply,
        } => {
            // ACK AT INITIATION, not completion. The blocking form deadlocked any
            // caller whose own request lane rides the module being restarted: the
            // caller's in-flight request keeps the drain from quiescing, the drain
            // keeps the restart from completing, and the completion keeps the reply
            // from releasing the caller — so the drain always timed out and cut the
            // initiator with a GOODBYE, even on a healthy module. Replying once the
            // restart is validated lets a self-lane caller settle, which is exactly
            // what makes the drain succeed. Completion is observable via
            // supervisor.list / module status; a post-ack failure lands the module
            // in a visible terminal state below rather than in a reply nobody can
            // receive.
            let validation = match lock_snapshot(snapshot) {
                Ok(state) if !state.enabled => Err(SuperviseError::Disabled {
                    module_id: spec.module_id.clone(),
                }),
                Ok(_) => Ok(()),
                Err(err) => Err(err),
            };
            let initiated = validation.is_ok();
            let _ = reply.send(validation);
            if initiated {
                // Precedence: this restart's operator override, else the module's
                // configured budget (already resolved into the runtime).
                let drain_timeout = drain_timeout_ms
                    .map(Duration::from_millis)
                    .unwrap_or(runtime.drain_timeout);
                if let Err(err) = restart_child(
                    spec,
                    runtime,
                    registry,
                    process_liveness,
                    snapshot,
                    child,
                    drain_timeout,
                )
                .await
                {
                    warn!(
                        module_id = %spec.module_id,
                        error = %err,
                        "operator restart failed after initiation ack; module state carries the outcome"
                    );
                    let _ = update_snapshot(snapshot, Some(&spec.module_id), |state| {
                        state.state = ModuleState::Failed;
                        clear_current_process_facts(state);
                    });
                }
            }
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
            drain_timeout_ms,
            reply,
        } => {
            if let Some(handle) = &runtime.supervisor_handle {
                handle.apply_identity_configuration(&next_spec);
            }
            *spec = next_spec;
            runtime.health = health;
            runtime.drain_timeout = drain_timeout_ms
                .map(Duration::from_millis)
                .unwrap_or(runtime.default_drain_timeout);
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
    child: &mut Option<SupervisedChild>,
    drain_timeout: Duration,
) -> Result<(), SuperviseError> {
    // Restart cycles a running module; it must not silently start a disabled one.
    if !lock_snapshot(snapshot)?.enabled {
        return Err(SuperviseError::Disabled {
            module_id: spec.module_id.clone(),
        });
    }
    begin_forwarding_drain_with_timeout(
        spec,
        runtime,
        snapshot,
        None,
        RouteCloseReason::Restart,
        drain_timeout,
    )
    .await?;

    if child.is_some() {
        drain_optional_child(
            &spec.module_id,
            registry,
            snapshot,
            &runtime.terminal_ring,
            child,
            drain_timeout,
            ModuleState::Restarting,
            Some(true),
        )
        .await?;
    } else {
        update_snapshot(snapshot, Some(&spec.module_id), |state| {
            state.enabled = true;
            state.state = ModuleState::Restarting;
            clear_current_process_facts(state);
        })?;
        wait_for_registration_release(registry, &spec.module_id, REGISTRY_RELEASE_TIMEOUT).await?;
    }

    reset_restart_count(snapshot, &spec.module_id)?;
    sleep(runtime.restart_policy.backoff).await;
    process_liveness.track(spec.module_id.clone(), Arc::clone(snapshot));
    // Mirror health_restart_child's spawn-failure handling: of the four
    // spawn-failure sites this was the only one that propagated with the
    // snapshot still reading `Restarting` -- neither running nor failed, and
    // unrevivable by `set_enabled(true)` (issue #34). `Failed` is the state the
    // operator can see and heal.
    match spawn_and_mark_running(spec, runtime, snapshot) {
        Ok(next_child) => {
            *child = Some(next_child);
            debug!(module_id = %spec.module_id, "supervised module restarted by operator request");
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

async fn reload_child(
    spec: &ModuleSpec,
    runtime: &SupervisorRuntimeConfig,
    registry: &Registry,
    process_liveness: &SupervisorProcessLiveness,
    snapshot: &SharedSnapshot,
    child: &mut Option<SupervisedChild>,
) -> Result<(), SuperviseError> {
    // Reload cycles a running module; it must not silently start a disabled one.
    if !lock_snapshot(snapshot)?.enabled {
        return Err(SuperviseError::Disabled {
            module_id: spec.module_id.clone(),
        });
    }
    begin_forwarding_drain(
        spec,
        runtime,
        snapshot,
        Some(true),
        RouteCloseReason::Reload,
    )
    .await?;

    if child.is_some() {
        drain_optional_child(
            &spec.module_id,
            registry,
            snapshot,
            &runtime.terminal_ring,
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
            clear_current_process_facts(state);
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
            if let Some(active_child) = child.as_mut() {
                active_child.drain_stderr(&spec.module_id).await;
            }
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
            timed_out_child.drain_stderr(&spec.module_id).await;
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
    child: &mut Option<SupervisedChild>,
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
            clear_current_process_facts(state);
        })?;
        #[cfg(test)]
        if runtime.test_seed_stale_facts_before_enable_spawn {
            update_snapshot(snapshot, Some(&spec.module_id), |state| {
                state.process_alive = true;
                state.pid = Some(41);
                state.spawned_at_ms = Some(42);
                state.spawned_from = Some(PathBuf::from("/spawned/module"));
                state.spawned_file_identity = Some(SpawnedFileIdentity {
                    device: 43,
                    inode: 44,
                });
            })?;
        }
        wait_for_registration_release(registry, &spec.module_id, REGISTRY_RELEASE_TIMEOUT).await?;
        reset_restart_count(snapshot, &spec.module_id)?;
        process_liveness.track(spec.module_id.clone(), Arc::clone(snapshot));
        let next_child = match spawn_and_mark_running(spec, runtime, snapshot) {
            Ok(next_child) => next_child,
            Err(err) => {
                if let Err(state_err) = update_snapshot(snapshot, Some(&spec.module_id), |state| {
                    state.state = ModuleState::Failed;
                    clear_current_process_facts(state);
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
        begin_forwarding_drain_if_configured(
            spec,
            runtime,
            snapshot,
            Some(false),
            RouteCloseReason::Disable,
        )
        .await?;
        drain_optional_child(
            &spec.module_id,
            registry,
            snapshot,
            &runtime.terminal_ring,
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
    terminal_ring: &Arc<Mutex<TerminalRing>>,
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
                clear_current_process_facts(state);
                state.last_exit = Some(exit_report.clone());
            }) {
                error!(module_id = %spec.module_id, error = %err, "failed to record clean module exit");
            }
            record_terminal(terminal_ring, &exit_report, TerminalDisposition::Stopped);
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
            let mut disposition = TerminalDisposition::Disabled;
            if let Err(err) = update_snapshot(snapshot, Some(&spec.module_id), |state| {
                clear_current_process_facts(state);
                state.last_exit = Some(exit_report.clone());
                if daemon_will_restart(state.enabled, state.restart_count, policy.max_restarts) {
                    state.restart_count += 1;
                    state.state = ModuleState::Restarting;
                    should_restart = true;
                    disposition = TerminalDisposition::Restarting;
                } else if state.enabled {
                    state.state = ModuleState::Failed;
                    disposition = TerminalDisposition::Failed;
                } else {
                    state.state = ModuleState::Disabled;
                    disposition = TerminalDisposition::Disabled;
                }
            }) {
                error!(module_id = %spec.module_id, error = %err, "failed to record crashed module exit");
                return NextAction::Stop {
                    registration_released: false,
                };
            }
            record_terminal(terminal_ring, &exit_report, disposition);

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

fn record_terminal(
    terminal_ring: &Arc<Mutex<TerminalRing>>,
    exit_report: &ExitReport,
    disposition: TerminalDisposition,
) {
    terminal_ring
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(TerminalRecord {
            exit_code: exit_report.code,
            exit_signal: exit_report.signal,
            at_ms: exit_report.at_ms,
            disposition,
        });
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
    ring: &Arc<Mutex<StderrRing>>,
) -> Result<SupervisedChild, SuperviseError> {
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

    // STDOUT STAYS INHERITED; STDERR IS PIPED. The asymmetry is the whole design,
    // so it is worth saying why rather than leaving it to be inferred.
    //
    // Inheriting both was the original choice: every child wrote to the daemon's
    // own descriptors, which put all module output in one log with no per-child
    // reader task. That was correct about interleaving and silent about DURABILITY,
    // and durability is the axis that decides whether a crash can be diagnosed.
    // A module's stderr is the only diagnostic input with no in-memory path --
    // `last_exit` survives a respawn because the supervisor holds it, while the
    // text explaining that exit went to a sink that rotates or fills. Measured on
    // two hosts: a systemd journal at its size cap retaining ~3.2 hours, and a
    // plain log file reaching 908 MB with one module accounting for 98% of it. In
    // both, the noisiest module sets everyone else's retention and the victim has
    // no way to know its window shrank.
    //
    // So stderr is piped into a bounded in-memory ring the supervisor owns, and
    // every line is forwarded on so the daemon log keeps its current content. That
    // forwarding is MANDATORY rather than courteous: the log is overwhelmingly
    // module output (4727 of 5000 sampled lines carried a module tag), so a tap
    // that captured without forwarding would leave it nearly empty and every
    // existing reader would report clean on nothing -- an absence that reads as
    // calm, which is worse than the interleaving it replaces.
    //
    // THE TAP MAKES LINE ATOMICITY THIS DAEMON'S PROBLEM. Previously a module's
    // own write reached the fd in one syscall and the splicing came from emitters
    // that formatted incrementally: two processes on one inherited fd, 1500 lines
    // each, produced 212 spliced lines of 3000 with an incremental emitter and 0
    // of 3000 when each line was formatted first and written once. Reading a pipe
    // and re-emitting can split a line that WAS atomic, so the reader reassembles
    // to a complete line and writes it in a single call -- otherwise this change
    // introduces a defect the previous design did not have.
    //
    // Stdout is left inherited: modules use it for ordinary output rather than
    // diagnostics, and piping it would double the reader tasks for no diagnostic
    // gain.
    command.stderr(Stdio::piped());
    command.kill_on_drop(true);
    let mut child = command.spawn().map_err(|source| SuperviseError::Spawn {
        program: spec.program.clone(),
        source,
    })?;
    let spawned_at_ms = unix_ms_now();
    let spawned_from = spec.program.clone();
    let spawned_file_identity = spawned_file_identity(&spawned_from);
    let process_start_time = child.id().and_then(crate::provenance::process_start_time);

    let stderr_pump = match child.stderr.take() {
        Some(stderr) => {
            ring.lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push_process_start();
            Some(tokio::spawn(pump_stderr(stderr, Arc::clone(ring))))
        }
        None => {
            // Spawning succeeded but the pipe did not materialise. Recording it as
            // uncaptured keeps the tail honest: the alternative is an empty tail
            // that reads as a module which printed nothing.
            ring.lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .mark_not_captured("stderr pipe was not available on spawn");
            warn!(
                module_id = %spec.module_id,
                "spawned child exposed no stderr pipe; tail will be unavailable"
            );
            None
        }
    };

    Ok(SupervisedChild {
        child,
        stderr_pump,
        stderr_ring: Arc::clone(ring),
        spawned_at_ms,
        spawned_from,
        spawned_file_identity,
        process_start_time,
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
) -> Result<SupervisedChild, SuperviseError> {
    let child = spawn_child(
        spec,
        runtime.connection_file_path.as_deref(),
        runtime.supervisor_handle.as_ref(),
        &runtime.stderr_ring,
    )?;
    set_running(snapshot, &child)?;
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

/// The `route.closed` `drained` value implied by a quiescence-wait outcome.
///
/// `Ok` is always honest and passed straight through -- the wait actually measured
/// in-flight state. `Err` means the wait produced no measurement at all (the
/// forwarding table's lock was poisoned), so `false` is reported as the one honest
/// constant: the drain did not complete. Never recomputed from route state, never a
/// third "unknown" value -- the caller must still send a well-formed `route.closed`.
fn drained_after_quiescence_wait(wait_result: &Result<bool, SuperviseError>) -> bool {
    match wait_result {
        Ok(drained) => *drained,
        Err(_) => false,
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
    reason: RouteCloseReason,
) -> Result<(), SuperviseError> {
    let Some(forwarding) = runtime.forwarding.as_ref() else {
        return Err(SuperviseError::ReloadUnavailable {
            module_id: spec.module_id.clone(),
            reason: "supervisor was not configured with a forwarding table".to_string(),
        });
    };

    begin_forwarding_drain_with(
        forwarding,
        spec,
        snapshot,
        enabled,
        reason,
        runtime.drain_timeout,
    )
    .await
}

async fn begin_forwarding_drain_if_configured(
    spec: &ModuleSpec,
    runtime: &SupervisorRuntimeConfig,
    snapshot: &SharedSnapshot,
    enabled: Option<bool>,
    reason: RouteCloseReason,
) -> Result<(), SuperviseError> {
    begin_forwarding_drain_with_timeout(
        spec,
        runtime,
        snapshot,
        enabled,
        reason,
        runtime.drain_timeout,
    )
    .await
}

/// Like [`begin_forwarding_drain_if_configured`] but with an explicit drain
/// budget, for paths where the operator overrides the module's configured one
/// (`supervisor.restart{drain_timeout_ms}`).
async fn begin_forwarding_drain_with_timeout(
    spec: &ModuleSpec,
    runtime: &SupervisorRuntimeConfig,
    snapshot: &SharedSnapshot,
    enabled: Option<bool>,
    reason: RouteCloseReason,
    drain_timeout: Duration,
) -> Result<(), SuperviseError> {
    let Some(forwarding) = runtime.forwarding.as_ref() else {
        return Ok(());
    };

    begin_forwarding_drain_with(forwarding, spec, snapshot, enabled, reason, drain_timeout).await
}

async fn begin_forwarding_drain_with(
    forwarding: &ForwardingTable,
    spec: &ModuleSpec,
    snapshot: &SharedSnapshot,
    enabled: Option<bool>,
    reason: RouteCloseReason,
    drain_timeout: Duration,
) -> Result<(), SuperviseError> {
    debug_assert_ne!(reason, RouteCloseReason::Crash);
    let terminal = matches!(reason, RouteCloseReason::Disable);

    // Admission gate first: route.open/commit and route REQUEST admission are closed
    // before the first quiescence check, so the outstanding count can only fall.
    let drain_target = forwarding
        .begin_module_drain(&spec.module_id, reason)
        .map_err(SuperviseError::Forwarding)?;
    update_snapshot(snapshot, Some(&spec.module_id), |state| {
        state.state = ModuleState::Draining;
        if let Some(enabled) = enabled {
            state.enabled = enabled;
        }
    })?;

    if let Some(target) = drain_target.as_ref() {
        let routes = forwarding
            .endpoint_routes(target.endpoint)
            .map_err(SuperviseError::Forwarding)?;
        let routes_notified = routes.len();
        crate::control::send_route_control_pushes(
            forwarding,
            routes.clone(),
            ClientControlPush::RouteClosing {
                module_id: spec.module_id.clone(),
                reason,
            },
        );
        send_route_goodbyes(forwarding, target.abandoned_bindings.clone());

        // `route.closing` was just sent above: from here on every return path,
        // including an early one, MUST send `route.closed` before propagating
        // anything else. A client holds `closing` as a promise that a verdict is
        // coming; leaving early without `closed` strands it waiting forever, since
        // `closing` carries no timeout of its own.
        let wait_result =
            wait_for_forwarding_quiescence(forwarding, target.endpoint, drain_timeout).await;
        let drained = drained_after_quiescence_wait(&wait_result);
        if let Err(err) = &wait_result {
            error!(
                module_id = %spec.module_id,
                ?reason,
                error = %err,
                "forwarding quiescence wait failed after route.closing; forcing route.closed(drained: false) so the client is not left waiting on an unfulfilled promise"
            );
        } else if !drained {
            warn!(
                module_id = %spec.module_id,
                waited = ?drain_timeout,
                ?reason,
                "route drain timed out before request quiescence; forcing teardown"
            );
        }
        crate::control::send_route_control_pushes(
            forwarding,
            routes,
            ClientControlPush::RouteClosed {
                module_id: spec.module_id.clone(),
                reason,
                drained,
                abandoned: target.abandoned_bindings.len() as u32,
                terminal: Some(terminal),
            },
        );
        wait_result?;

        // `route.closed` has now been sent unconditionally above. From here the
        // remaining steps are cleanup (route + module GOODBYE) rather than a
        // promise the client is waiting on, but a lock-poisoned
        // `release_module_endpoint_routes` would otherwise skip the module
        // GOODBYE silently too -- send it before propagating the error.
        let released_routes = match forwarding.release_module_endpoint_routes(target.endpoint) {
            Ok(routes) => routes,
            Err(err) => {
                warn!(
                    module_id = %spec.module_id,
                    ?reason,
                    error = %err,
                    "failed to release module endpoint routes after route.closed; module GOODBYE will still be sent"
                );
                send_module_goodbye(&spec.module_id, forwarding, target);
                return Err(SuperviseError::Forwarding(err));
            }
        };
        let route_goodbye_count = released_routes.len();
        send_route_goodbyes(forwarding, released_routes);
        send_module_goodbye(&spec.module_id, forwarding, target);

        // The drain's happy path was previously silent: every emission above is
        // best-effort with only its failure arm logged, so "were consumers told"
        // was unprovable from the daemon log (surfaced by a 30-minute consumer
        // hang where the open question was exactly whether teardown notice went
        // out). One summary line makes that class decidable in one grep.
        info!(
            module_id = %spec.module_id,
            ?reason,
            routes_notified,
            route_goodbyes = route_goodbye_count,
            abandoned_reservations = target.abandoned_bindings.len(),
            drained,
            "module drain complete; consumers notified via route.closing/route.closed pushes and per-route GOODBYE frames"
        );
    }

    Ok(())
}

async fn wait_for_registration_after_reload(
    registry: &Registry,
    module_id: &str,
    child: &mut SupervisedChild,
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
    child: &mut Option<SupervisedChild>,
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
        &runtime.terminal_ring,
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
    child: &mut Option<SupervisedChild>,
    reason: String,
) -> Result<(), SuperviseError> {
    let mut should_retry = false;
    update_snapshot(snapshot, Some(&spec.module_id), |state| {
        clear_current_process_facts(state);
        if daemon_will_restart(
            state.enabled,
            state.restart_count,
            runtime.restart_policy.max_restarts,
        ) {
            state.restart_count += 1;
            state.state = ModuleState::Restarting;
            should_retry = true;
        } else if state.enabled {
            state.state = ModuleState::Failed;
        } else {
            state.state = ModuleState::Disabled;
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

#[allow(clippy::too_many_arguments)]
async fn drain_optional_child(
    module_id: &str,
    registry: &Registry,
    snapshot: &SharedSnapshot,
    terminal_ring: &Arc<Mutex<TerminalRing>>,
    child: &mut Option<SupervisedChild>,
    drain_timeout: Duration,
    final_state: ModuleState,
    enabled: Option<bool>,
) -> Result<(), SuperviseError> {
    if let Some(child) = child.take() {
        drain_child_to_state(
            module_id,
            registry,
            snapshot,
            terminal_ring,
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
            clear_current_process_facts(state);
        })?;
        wait_for_registration_release(registry, module_id, REGISTRY_RELEASE_TIMEOUT).await
    }
}

#[allow(clippy::too_many_arguments)]
async fn drain_child_to_state(
    module_id: &str,
    registry: &Registry,
    snapshot: &SharedSnapshot,
    terminal_ring: &Arc<Mutex<TerminalRing>>,
    mut child: SupervisedChild,
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
            // Mirror the sibling arm above: state is already `Draining`, and an
            // error propagated from here would strand it there -- a state
            // `set_enabled(true)` cannot heal (`revive_terminal` matches only
            // `Failed | Stopped`), leaving an operator Restart as the only exit.
            // `Failed` before `?` keeps the module operator-visible and
            // revivable. Trigger is an ESRCH race (process exits between the
            // drain timeout firing and the kill) or a post-kill wait failure
            // (issue #34).
            child.start_kill().map_err(|source| {
                fail_snapshot(snapshot, Some(module_id), None);
                SuperviseError::Kill {
                    module_id: module_id.to_string(),
                    source,
                }
            })?;
            let status = child.wait().await.map_err(|source| {
                fail_snapshot(snapshot, Some(module_id), None);
                SuperviseError::Wait {
                    module_id: module_id.to_string(),
                    source,
                }
            })?;
            classify_exit(&status)
        }
    };

    update_snapshot(snapshot, Some(module_id), |state| {
        state.state = final_state;
        if let Some(enabled) = enabled {
            state.enabled = enabled;
        }
        clear_current_process_facts(state);
        state.last_exit = Some(exit_report.clone());
    })?;
    record_terminal(
        terminal_ring,
        &exit_report,
        terminal_disposition(final_state),
    );
    child.drain_stderr(module_id).await;

    wait_for_registration_release(registry, module_id, REGISTRY_RELEASE_TIMEOUT).await
}

fn terminal_disposition(final_state: ModuleState) -> TerminalDisposition {
    match final_state {
        ModuleState::Stopped => TerminalDisposition::Stopped,
        ModuleState::Disabled => TerminalDisposition::Disabled,
        ModuleState::Restarting => TerminalDisposition::Restarting,
        ModuleState::Failed => TerminalDisposition::Failed,
        ModuleState::Starting
        | ModuleState::Running
        | ModuleState::Unresponsive
        | ModuleState::Draining => {
            unreachable!("terminal exits only finish in terminal or restarting states")
        }
    }
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
        at_ms: unix_ms_now(),
    }
}

/// The terminal record for a module whose `wait()` call itself errored (e.g. the
/// child was already reaped out-of-band). There is no `ExitStatus` to read a code
/// or signal from -- `None`/`None` is the honest shape, not a guess -- but the
/// disposition still must be `Failed` so the terminal ring is not silently missing
/// an entry, matching what `fail_snapshot` records for this same arm.
fn wait_error_exit_report() -> ExitReport {
    ExitReport {
        kind: ExitKind::Crash,
        code: None,
        signal: None,
        at_ms: unix_ms_now(),
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

fn set_running(snapshot: &SharedSnapshot, child: &SupervisedChild) -> Result<(), SuperviseError> {
    update_snapshot(snapshot, None, |state| {
        state.state = ModuleState::Running;
        state.enabled = true;
        state.process_alive = true;
        state.pid = child.id();
        state.spawned_at_ms = Some(child.spawned_at_ms);
        state.spawned_from = Some(child.spawned_from.clone());
        state.spawned_file_identity = child.spawned_file_identity;
        state.process_start_time = child.process_start_time;
    })
}

fn clear_current_process_facts(state: &mut SupervisorSnapshot) {
    state.process_alive = false;
    state.pid = None;
    state.spawned_at_ms = None;
    state.spawned_from = None;
    state.spawned_file_identity = None;
    state.process_start_time = None;
}

fn fail_snapshot(
    snapshot: &SharedSnapshot,
    module_id: Option<&str>,
    last_exit: Option<ExitReport>,
) {
    if let Err(err) = update_snapshot(snapshot, module_id, |state| {
        state.state = ModuleState::Failed;
        clear_current_process_facts(state);
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

const SLOW_SNAPSHOT_LOCK_THRESHOLD: Duration = Duration::from_millis(250);

fn lock_snapshot_for_control<'a>(
    snapshot: &'a SharedSnapshot,
    module_id: &str,
    caller: &'static str,
) -> Result<std::sync::MutexGuard<'a, SupervisorSnapshot>, SuperviseError> {
    let started_at = Instant::now();
    let guard = lock_snapshot(snapshot)?;
    let waited = started_at.elapsed();
    if waited >= SLOW_SNAPSHOT_LOCK_THRESHOLD {
        warn!(
            module_id = %module_id,
            waited_ms = waited.as_millis() as u64,
            caller = %caller,
            "slow snapshot lock"
        );
    }
    Ok(guard)
}

fn lock_snapshot(
    snapshot: &SharedSnapshot,
) -> Result<std::sync::MutexGuard<'_, SupervisorSnapshot>, SuperviseError> {
    snapshot
        .lock()
        .map_err(|_| SuperviseError::StatePoisoned { module_id: None })
}

#[cfg(test)]
mod terminal_history_tests {
    use std::{
        path::PathBuf,
        sync::Arc,
        time::{Duration, Instant},
    };

    use tokio::time::sleep;

    use super::{
        daemon_will_restart, drained_after_quiescence_wait, record_terminal, update_snapshot,
        wait_error_exit_report, ExitKind, ModuleSpec, ModuleState, RestartPolicy, SuperviseError,
        SupervisedModule, Supervisor, SupervisorHandle,
    };
    use crate::{
        registry::Registry,
        terminal_ring::{TerminalRing, TerminalRingConfig},
    };
    use std::sync::Mutex;
    use subc_control::TerminalDisposition;

    /// See the twin in `control.rs` for why this derives the path from
    /// `current_exe()` and why the existence check is here: `--lib` alone does
    /// not build `[[bin]]` targets, and a bare spawn then fails with a raw
    /// `NotFound` that reads as a broken test rather than an unbuilt dependency.
    fn fake_aft_stub_path() -> PathBuf {
        let mut path = std::env::current_exe().expect("current_exe available in tests");
        path.pop();
        path.pop();
        path.push(if cfg!(windows) {
            "fake-aft-stub.exe"
        } else {
            "fake-aft-stub"
        });
        assert!(
            path.exists(),
            "fake-aft-stub not built at {}: run `cargo test -p subc-core` (which builds \
             [[bin]] targets) rather than `cargo test -p subc-core --lib` (which does not)",
            path.display()
        );
        path
    }

    #[test]
    fn reserved_never_spawned_refuses_every_hello() {
        // The canary hole: a reserved id whose module has never spawned had NO
        // gate entry and admitted anyone -- the reservation protected the nonce
        // holder, not the NAME. Now the entry is present with no legitimate
        // holder and refuses all comers.
        let supervisor = SupervisorHandle::default();
        supervisor.apply_identity_configuration(&ModuleSpec {
            module_id: "never-spawned".to_string(),
            program: PathBuf::from("/usr/bin/false"),
            args: Vec::new(),
            env: Vec::new(),
            reserved: true,
            reserved_prefixes: Vec::new(),
        });
        assert!(
            supervisor
                .reserved_hello_rejection("never-spawned", Some("any-forged-nonce"))
                .is_some(),
            "forged nonce must refuse on a reserved never-spawned id"
        );
        assert!(
            supervisor
                .reserved_hello_rejection("never-spawned", None)
                .is_some(),
            "absent nonce must refuse on a reserved never-spawned id"
        );
        // And a real spawn nonce minted later admits exactly that nonce.
        supervisor.set_spawn_nonce("never-spawned", "minted".to_string());
        supervisor.apply_identity_configuration(&ModuleSpec {
            module_id: "never-spawned".to_string(),
            program: PathBuf::from("/usr/bin/false"),
            args: Vec::new(),
            env: Vec::new(),
            reserved: true,
            reserved_prefixes: Vec::new(),
        });
        assert!(supervisor
            .reserved_hello_rejection("never-spawned", Some("minted"))
            .is_none());
        assert!(supervisor
            .reserved_hello_rejection("never-spawned", Some("forged"))
            .is_some());
    }

    #[test]
    fn daemon_owned_recovery_predicate_uses_the_pre_increment_budget() {
        assert!(daemon_will_restart(true, 2, 3));
        assert!(!daemon_will_restart(true, 3, 3));
        assert!(!daemon_will_restart(false, 0, 3));
    }

    fn module_with_recovery_snapshot(
        state: ModuleState,
        enabled: bool,
        restart_count: u32,
    ) -> SupervisedModule {
        let registry = Arc::new(Registry::default());
        let supervisor =
            Supervisor::new(Arc::clone(&registry), RestartPolicy::new(3, Duration::ZERO));
        let module = supervisor
            .spawn(ModuleSpec {
                module_id: "recovery-snapshot".to_string(),
                program: fake_aft_stub_path(),
                args: Vec::new(),
                env: Vec::new(),
                reserved: false,
                reserved_prefixes: Vec::new(),
            })
            .unwrap();
        update_snapshot(
            &module.inner.snapshot,
            Some("recovery-snapshot"),
            |snapshot| {
                snapshot.state = state;
                snapshot.enabled = enabled;
                snapshot.restart_count = restart_count;
            },
        )
        .unwrap();
        module
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn undecided_snapshot_uses_shared_restart_predicate() {
        assert!(module_with_recovery_snapshot(ModuleState::Running, true, 2)
            .will_recover_after_connection_loss()
            .unwrap());
        assert!(
            !module_with_recovery_snapshot(ModuleState::Running, true, 3)
                .will_recover_after_connection_loss()
                .unwrap()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn restarting_snapshot_at_exhausted_budget_is_non_terminal() {
        assert!(
            module_with_recovery_snapshot(ModuleState::Restarting, true, 3)
                .will_recover_after_connection_loss()
                .unwrap()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn terminal_phase_snapshots_are_terminal_before_budget_exhaustion() {
        assert!(!module_with_recovery_snapshot(ModuleState::Failed, true, 0)
            .will_recover_after_connection_loss()
            .unwrap());
        assert!(
            !module_with_recovery_snapshot(ModuleState::Disabled, true, 0)
                .will_recover_after_connection_loss()
                .unwrap()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn warming_snapshot_is_limited_to_startup_phases() {
        for state in [
            ModuleState::Starting,
            ModuleState::Running,
            ModuleState::Restarting,
        ] {
            assert!(
                module_with_recovery_snapshot(state, true, 0)
                    .is_warming()
                    .unwrap(),
                "{state:?} should be warming"
            );
        }
        for state in [
            ModuleState::Unresponsive,
            ModuleState::Draining,
            ModuleState::Stopped,
            ModuleState::Failed,
            ModuleState::Disabled,
        ] {
            assert!(
                !module_with_recovery_snapshot(state, true, 0)
                    .is_warming()
                    .unwrap(),
                "{state:?} should not be warming"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn terminal_history_survives_respawn_and_keeps_both_crashes_in_order() {
        let registry = Arc::new(Registry::default());
        let supervisor =
            Supervisor::new(Arc::clone(&registry), RestartPolicy::new(1, Duration::ZERO));
        let module = supervisor
            .spawn(ModuleSpec {
                module_id: "terminal-history".to_string(),
                program: fake_aft_stub_path(),
                args: Vec::new(),
                env: vec![("FAKE_AFT_EXIT_CODE".to_string(), "23".to_string())],
                reserved: false,
                reserved_prefixes: Vec::new(),
            })
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let history = module.terminal_history();
            if history.entries.len() == 2 {
                assert_eq!(module.status().unwrap().state, ModuleState::Failed);
                assert_eq!(history.dropped, 0);
                assert_eq!(
                    history
                        .entries
                        .iter()
                        .map(|entry| entry.exit_code)
                        .collect::<Vec<_>>(),
                    vec![Some(23), Some(23)]
                );
                assert!(history.entries[0].at_ms <= history.entries[1].at_ms);
                return;
            }
            assert!(
                Instant::now() < deadline,
                "module did not retain two terminal exits: {history:?}"
            );
            sleep(Duration::from_millis(10)).await;
        }
    }

    /// The `route.closed` `drained` value must be the quiescence wait's own
    /// measurement (`Ok`), never invented -- except on `Err`, where there is no
    /// measurement at all and `false` is the one honest constant. This is the exact
    /// logic `begin_forwarding_drain_with` now applies before sending `route.closed`
    /// on every return path, including the one that used to return early via `?`
    /// with `route.closing` already sent and no `route.closed` ever following.
    #[test]
    fn drained_after_quiescence_wait_passes_ok_through_and_forces_false_on_err() {
        assert!(drained_after_quiescence_wait(&Ok(true)));
        assert!(!drained_after_quiescence_wait(&Ok(false)));
        assert!(!drained_after_quiescence_wait(&Err(
            SuperviseError::StatePoisoned { module_id: None }
        )));
    }

    /// `supervise_loop`'s `wait()`-error arm now calls `record_terminal` like every
    /// other exit path does, so a module whose child `wait()` itself errored (e.g.
    /// already reaped out-of-band) still leaves a terminal record rather than none
    /// at all. Triggering the real `wait()` I/O error from an integration test would
    /// need a genuine already-reaped-child race, which is OS-specific and not
    /// something this suite attempts elsewhere; this test instead verifies the
    /// record produced for that arm end-to-end through the real `TerminalRing`, and
    /// the call site itself is verified by inspection to sit in that exact arm.
    #[test]
    fn wait_error_exit_report_records_a_failed_terminal_with_no_code_or_signal() {
        let ring = Arc::new(Mutex::new(TerminalRing::new(
            TerminalRingConfig::default(),
            0,
        )));
        record_terminal(
            &ring,
            &wait_error_exit_report(),
            TerminalDisposition::Failed,
        );

        let snapshot = ring.lock().unwrap().snapshot();
        assert_eq!(snapshot.entries.len(), 1);
        let entry = &snapshot.entries[0];
        assert_eq!(entry.exit_code, None);
        assert_eq!(entry.exit_signal, None);
        assert_eq!(entry.disposition, TerminalDisposition::Failed);
    }

    /// Pins the report's `kind` too: the wait-error arm treats an unwaitable child
    /// as a crash (matching `fail_snapshot`'s `Failed` disposition for this arm),
    /// not a clean exit it never actually observed.
    #[test]
    fn wait_error_exit_report_is_classified_as_a_crash() {
        assert_eq!(wait_error_exit_report().kind, ExitKind::Crash);
    }
}

#[cfg(test)]
mod health_evidence_tests {
    use super::{HealthProbeError, HealthProbeEvidence};
    use std::collections::HashSet;

    /// The evidential asymmetry, asserted rather than described.
    ///
    /// Exactly ONE observation is proof a module cannot serve, and the one that
    /// fires under CPU starvation is not it. Before the split, all fifteen
    /// construction sites collapsed into a single String, so a timeout carried the
    /// same weight as a dead lane -- which is how a healthy module was restarted
    /// three times in one day.
    #[test]
    fn only_a_dead_lane_is_proof_of_death() {
        assert!(HealthProbeError::lane_dead("gone").is_proof_of_death());
        // Three non-proof classes, each for a different reason: silence is
        // consistent with health, a bad answer proves the module ALIVE, and a
        // daemon-side fault never reached the module at all.
        assert!(!HealthProbeError::no_answer("timed out").is_proof_of_death());
        assert!(!HealthProbeError::bad_answer("garbage").is_proof_of_death());
        assert!(!HealthProbeError::misconfigured("no table").is_proof_of_death());
    }

    /// Labels must be distinct, or the operator-facing distinction is cosmetic.
    ///
    /// A shared label renders two different observations identically in the line an
    /// operator reads after an unexplained restart -- the exact confusion this
    /// change removes.
    #[test]
    fn every_evidence_class_has_a_distinct_label() {
        let labels = [
            HealthProbeError::lane_dead("").label(),
            HealthProbeError::no_answer("").label(),
            HealthProbeError::bad_answer("").label(),
            HealthProbeError::misconfigured("").label(),
        ];
        let unique: HashSet<_> = labels.iter().collect();
        assert_eq!(unique.len(), labels.len(), "labels collided: {labels:?}");
    }

    /// The class is additional information, not a replacement.
    ///
    /// An operator needs both "this was silence" and the specific text saying how
    /// long we waited; a classification that swallowed the message would trade one
    /// missing distinction for another.
    #[test]
    fn classification_preserves_the_original_message() {
        let err = HealthProbeError::no_answer("module did not answer within 5s");
        assert_eq!(err.to_string(), "module did not answer within 5s");
        assert!(matches!(err.evidence, HealthProbeEvidence::NoAnswer));
    }
}

#[cfg(test)]
mod health_tombstone_tests {
    use std::{path::PathBuf, sync::Arc, time::Duration};

    use subc_protocol::{
        manifest::Concurrency,
        session::{HealthStatus, ModuleControlResponse},
    };
    use tokio::sync::mpsc;

    use super::{
        probe_module_health, HealthAction, HealthConfig, HealthProbeEvidence, ModuleSpec,
        RestartPolicy, Supervisor, SupervisorRuntimeConfig,
    };
    use crate::{
        control::ControlHandler,
        forwarding::{ForwardingTable, ModuleControlRpcCompletion, ModuleControlRpcOutcome},
        registry::{ConnectionId, Registry},
        router::FrameSink,
    };

    struct ProbeHarness {
        spec: ModuleSpec,
        runtime: SupervisorRuntimeConfig,
        forwarding: Arc<ForwardingTable>,
        module_connection: ConnectionId,
        module_rx: mpsc::Receiver<crate::router::OutboundFrame>,
        handler: ControlHandler,
        module: super::SupervisedModule,
    }

    fn probe_harness() -> ProbeHarness {
        let registry = Arc::new(Registry::default());
        let forwarding = Arc::new(ForwardingTable::default());
        let supervisor_handle = super::SupervisorHandle::new();
        let health = HealthConfig {
            cadence: Duration::from_secs(30),
            deadline: Duration::from_secs(5),
            failure_threshold: 3,
            on_degraded: HealthAction::Report,
            on_failing: HealthAction::Report,
            critical: false,
        };
        let supervisor = Supervisor::new(Arc::clone(&registry), RestartPolicy::default())
            .with_forwarding(Arc::clone(&forwarding))
            .with_handle(supervisor_handle.clone())
            .with_health_config(health);
        let spec = ModuleSpec {
            module_id: "late-health-module".to_string(),
            program: PathBuf::from("disabled-module"),
            args: Vec::new(),
            env: Vec::new(),
            reserved: false,
            reserved_prefixes: Vec::new(),
        };
        let module = supervisor
            .supervise_configured(spec.clone(), false)
            .unwrap();
        let runtime = supervisor.runtime_config();
        let handler = ControlHandler::with_forwarding(registry, Arc::clone(&forwarding))
            .with_supervisor(supervisor_handle);
        let module_connection = ConnectionId::new(700);
        let (module_tx, module_rx) = mpsc::channel(8);
        forwarding
            .register_module_connection(
                module_connection,
                spec.module_id.clone(),
                subc_protocol::PROTOCOL_VERSION,
                Concurrency::ModuleManaged,
                FrameSink::new(module_tx),
            )
            .unwrap();

        ProbeHarness {
            spec,
            runtime,
            forwarding,
            module_connection,
            module_rx,
            handler,
            module,
        }
    }

    async fn finish_after(
        harness: &mut ProbeHarness,
        stall: Duration,
    ) -> ModuleControlRpcCompletion {
        assert!(stall > harness.runtime.health.deadline);
        let deadline = harness.runtime.health.deadline;
        let probe = probe_module_health(&harness.spec, &harness.runtime);
        let answer = async {
            let frame = harness.module_rx.recv().await.expect("health.check frame");
            tokio::time::advance(deadline).await;
            tokio::task::yield_now().await;
            tokio::time::advance(stall - deadline).await;
            harness
                .forwarding
                .complete_module_control_rpc(
                    harness.module_connection,
                    frame.header.corr,
                    Some("health.check"),
                    ModuleControlRpcOutcome::Response(ModuleControlResponse::HealthCheck {
                        status: HealthStatus::Ok,
                        detail: None,
                        metrics: None,
                    }),
                )
                .unwrap()
        };
        let (probe_result, completion) = tokio::join!(probe, answer);
        let err = probe_result.expect_err("probe must miss its deadline");
        assert!(matches!(err.evidence, HealthProbeEvidence::NoAnswer));
        completion
    }

    async fn time_out_without_answer(harness: &mut ProbeHarness) {
        let deadline = harness.runtime.health.deadline;
        let probe = probe_module_health(&harness.spec, &harness.runtime);
        let exhaust_deadline = async {
            let _frame = harness.module_rx.recv().await.expect("health.check frame");
            tokio::time::advance(deadline).await;
            tokio::task::yield_now().await;
        };
        let (probe_result, ()) = tokio::join!(probe, exhaust_deadline);
        let err = probe_result.expect_err("probe must miss its deadline");
        assert!(matches!(err.evidence, HealthProbeEvidence::NoAnswer));
    }

    #[tokio::test(start_paused = true)]
    async fn late_health_answers_record_start_anchored_latency_for_two_stalls() {
        let mut harness = probe_harness();

        let first = finish_after(&mut harness, Duration::from_secs(8)).await;
        let first_latency = match &first {
            ModuleControlRpcCompletion::LateHealthAnswer { latency, .. } => *latency,
            other => panic!("late answer was not retained: {other:?}"),
        };
        assert!(harness.handler.observe_module_control_completion(first));

        let second = finish_after(&mut harness, Duration::from_secs(11)).await;
        let second_latency = match &second {
            ModuleControlRpcCompletion::LateHealthAnswer { latency, .. } => *latency,
            other => panic!("late answer was not retained: {other:?}"),
        };
        assert!(harness.handler.observe_module_control_completion(second));

        assert_eq!(first_latency, Duration::from_secs(8));
        assert_eq!(
            second_latency - first_latency,
            Duration::from_secs(3),
            "latency must grow linearly with the additional stall"
        );
        let health = harness.module.status().unwrap().health;
        assert_eq!(health.late_answer_count, 2);
        assert_eq!(health.last_late_answer_latency_ms, Some(11_000));
    }

    /// A module that answers every probe late must never march to the kill
    /// threshold: the late answer proves it is alive, so it must clear the miss
    /// streak the timeout recorded. Without the reset, a CPU-starved module
    /// that serves every probe seconds past the deadline accumulates
    /// `consecutive_failures` to the threshold and is killed — the exact
    /// sequence from the 2026-08-14 aft disable, where the daemon logged
    /// "proves the module is alive" five times while counting five misses.
    #[tokio::test(start_paused = true)]
    async fn late_answer_clears_the_consecutive_failure_streak() {
        let mut harness = probe_harness();

        // Timeout recorded first: the probe path saw no answer in time.
        time_out_without_answer(&mut harness).await;
        harness
            .module
            .record_health_probe_failure_for_test("[no-answer] test miss")
            .unwrap();
        assert_eq!(
            harness.module.status().unwrap().health.consecutive_failures,
            1,
            "precondition: the miss must be on the streak before the late answer"
        );

        // The stalled reply then lands: proof of life.
        let late = finish_after(&mut harness, Duration::from_secs(9)).await;
        assert!(matches!(
            late,
            ModuleControlRpcCompletion::LateHealthAnswer { .. }
        ));
        assert!(harness.handler.observe_module_control_completion(late));

        let health = harness.module.status().unwrap().health;
        assert_eq!(
            health.consecutive_failures, 0,
            "a late answer is an answer: the streak must reset"
        );
        assert_eq!(health.late_answer_count, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn repeated_serial_probe_cycles_keep_one_tombstone_per_endpoint() {
        let mut harness = probe_harness();

        for _ in 0..20 {
            time_out_without_answer(&mut harness).await;
            assert_eq!(
                harness.forwarding.health_probe_tombstone_count().unwrap(),
                1
            );
        }
    }
}

#[cfg(test)]
mod jitter_tests {
    use super::jittered_health_delay;
    use std::{collections::HashSet, time::Duration};

    /// Module ids drawn from a real fleet, so the dispersal claim is about names
    /// that actually occur rather than invented ones.
    ///
    /// This is a SAMPLE, not a registry: the property under test is that distinct
    /// ids disperse, which holds for any set of distinct strings. Several entries
    /// are already historical (modules get renamed), and that costs nothing here --
    /// but it means a reader must not mistake this for the live module set, and a
    /// rename sweep will match it without there being anything to change.
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
