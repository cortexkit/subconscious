//! Identity policy updates read the running-protocol snapshot before taking the
//! nonce locks. Nested identity locks are taken in this order: spawn nonces,
//! reserved nonces, then `SupervisorHandle::implicit_protocol_none`.

#[cfg(all(test, target_os = "linux"))]
#[path = "supervise/audit_identity_tests.rs"]
mod audit_identity_tests;

mod spawn;
#[cfg(target_os = "linux")]
#[cfg(test)]
use spawn::apply_cgroup_placement;
pub(crate) use spawn::apply_child_env;
#[cfg(test)]
use spawn::apply_spawn_role;
#[cfg(test)]
use spawn::apply_wire_spawn_args;
#[cfg(all(test, any(unix, windows)))]
use spawn::apply_wire_spawn_args_for_admission;
use spawn::carries_containment;
#[cfg(unix)]
use spawn::is_launch_deferral;
use spawn::kill_and_reap_rejected_child;
#[cfg(target_os = "linux")]
pub(crate) use spawn::kill_module_cgroup;
#[cfg(all(test, target_os = "linux"))]
use spawn::launch_child_in_slot;
#[cfg(target_os = "linux")]
#[cfg(test)]
use spawn::linux_exec_committed;
#[cfg(not(windows))]
use spawn::observe_spawned_image;
use spawn::observed_launch_nonce_source;
#[cfg(all(test, target_os = "linux"))]
use spawn::prepare_child_in_slot;
#[cfg(all(test, windows))]
use spawn::prepare_windows_registration_pipe;
use spawn::previous_instance_error;
use spawn::recovery_block_error;
#[cfg(target_os = "linux")]
#[cfg(test)]
use spawn::remove_module_cgroup;
#[cfg(target_os = "linux")]
#[cfg(test)]
use spawn::sample_after_linux_exec;
use spawn::set_running;
#[cfg(test)]
use spawn::spawn_and_mark_running;
#[cfg(all(test, unix))]
use spawn::spawn_child;
use spawn::spawn_child_async;
#[cfg(all(test, unix))]
use spawn::spawn_child_in_slot;
use spawn::spawn_child_in_slot_async;
#[cfg(all(test, target_os = "linux"))]
use spawn::spawn_plan_stale;
pub(crate) use spawn::validate_spec;
#[cfg(windows)]
pub(crate) use spawn::windows_spawn_capture;
#[cfg(test)]
use spawn::AfterFirstSpawnHook;
#[cfg(test)]
use spawn::CapturedRegistrationChannels;
#[cfg(test)]
use spawn::CapturedRegistrationEndpoint;
pub(crate) use spawn::LaunchSpecDigest;
pub use spawn::ModuleOverlap;
pub use spawn::ModuleSpec;
#[cfg(any(all(test, unix), target_os = "linux", target_os = "macos"))]
pub(crate) use spawn::RegisteredConfiguredModule;
pub use spawn::ScriptEntry;
pub use spawn::ScriptRuntime;
pub(crate) use spawn::SpawnProgramBinding;
#[cfg(any(unix, windows))]
use spawn::SpawnRegistrationChannel;
use spawn::SpawnRole;
// Only the Unix admission path and its tests read the deadline by this name.
#[cfg(unix)]
use spawn::ADMISSION_TIMEOUT;
#[cfg(feature = "test-support")]
pub use spawn::CHILD_ADMISSION_TIMEOUT;
pub use spawn::SPAWN_ROLE_SWAP_CANDIDATE;
pub use spawn::SUBC_ARG;
pub use spawn::SUBC_SPAWN_ROLE_ENV;

mod privacy_exec;
#[cfg(target_os = "macos")]
use privacy_exec::privacy_command;
#[cfg(all(test, target_os = "macos"))]
use privacy_exec::probe_privacy_trampoline;
#[cfg(all(test, target_os = "macos"))]
pub(crate) use privacy_exec::test_privacy_trampoline;
#[cfg(target_os = "macos")]
use privacy_exec::PrivacyExec;

mod actor;
#[cfg(all(test, unix))]
use actor::handle_supervisor_command;
use actor::supervise_loop;
#[cfg(all(test, unix))]
use actor::supervisor_sleep;
#[cfg(test)]
use actor::ActorSelectCheckpoint;
use actor::NextAction;
use actor::SupervisorCommand;
pub(crate) mod handover;
pub(crate) mod live_upgrade;
mod restart;
use restart::finish_reload_child;
use restart::reload_child;
use restart::restart_child;
use restart::restart_child_with_budget_policy;
use restart::set_child_enabled;
use restart::set_child_enabled_with_budget_policy;
#[cfg(test)]
use restart::ReloadExitRecordGate;

mod health_probe;
#[cfg(test)]
use health_probe::handle_health_probe_failure;
#[cfg(all(test, unix))]
use health_probe::handle_health_report;
#[cfg(test)]
use health_probe::health_restart_child;
#[cfg(test)]
use health_probe::jittered_health_delay;
pub(crate) use health_probe::parse_http_probe_url;
use health_probe::probe_endpoint_health;
#[cfg(all(test, unix))]
use health_probe::probe_http_health;
use health_probe::probe_module_health;
use health_probe::record_health_action;
use health_probe::run_health_probe_cycle;
use health_probe::running_protocol;
pub use health_probe::HealthAction;
pub use health_probe::HealthConfig;
#[cfg(test)]
use health_probe::HealthProbeError;
use health_probe::HealthProbeEvidence;
use health_probe::HealthProbeRuntime;
pub use health_probe::ModuleHealthStatus;

mod drain;
use drain::begin_forwarding_drain;
use drain::begin_forwarding_drain_if_configured;
use drain::begin_forwarding_drain_with;
use drain::begin_forwarding_drain_with_timeout;
#[cfg(test)]
use drain::busy_gauge_observation;
use drain::control_flags;
pub(crate) use drain::declared_busy_gauges;
#[cfg(test)]
use drain::drain_busy_reprobe_interval;
#[cfg(test)]
use drain::drain_child_to_state;
use drain::drain_optional_child;
#[cfg(test)]
use drain::drained_after_quiescence_wait;
#[cfg(test)]
use drain::gauge_verdicts;
use drain::request_graceful_stop;
use drain::terminal_disposition;
#[cfg(test)]
use drain::wait_for_forwarding_quiescence;
#[cfg(test)]
use drain::warn_drain_timed_out;
#[cfg(test)]
use drain::BusyGaugeObservation;
#[cfg(test)]
use drain::DrainGaugeTrace;
use drain::DrainScope;
use drain::ForwardingDrainContext;
#[cfg(test)]
use drain::GaugeVerdict;
use drain::StopNotice;
pub use drain::DEFAULT_DRAIN_TIMEOUT;

mod exit;
#[cfg(test)]
use exit::apply_deliberate_severance_marker;
use exit::classify_exit;
use exit::classify_reaped_child_exit;
use exit::clear_current_process_facts;
use exit::durable_terminal_history_of;
use exit::fail_after_wait_error;
use exit::fail_snapshot;
#[cfg(test)]
use exit::on_child_exit;
use exit::on_child_exit_with_detail;
use exit::publish_after_terminal_record;
#[cfg(test)]
use exit::record_deliberate_severance;
#[cfg(test)]
use exit::record_terminal;
use exit::record_terminal_with_detail;
#[cfg(test)]
use exit::record_wait_error_terminal;
#[cfg(any(unix, test))]
use exit::settle_stderr_pump;
#[cfg(any(unix, test))]
use exit::settle_stdout_pump;
use exit::untrack_if_registration_released;
use exit::wait_error_exit_report;

mod spawn_events;
use spawn_events::SpawnEventFeed;
pub(crate) use spawn_events::SpawnSubscribeRefusal;
pub use spawn_events::SPAWN_EVENT_RING_CAPACITY;
#[cfg(test)]
use spawn_events::SPAWN_SUBSCRIBER_BUFFER;
#[cfg(test)]
use spawn_events::SPAWN_SUBSCRIBER_LAGGED_CODE;

mod roster;
pub use roster::ModuleProcessLiveness;
pub use roster::SupervisorProcessLiveness;
mod resource_launch;
pub(crate) use resource_launch::{ResourceCommitError, ResourceLaunch};

use std::{
    collections::{HashMap, HashSet, VecDeque},
    error::Error,
    fmt, io,
    path::PathBuf,
    process::{ExitStatus, Stdio},
    sync::{Arc, Mutex, OnceLock, TryLockError},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use cortexkit_log::Retention;
use serde_json::Value;
use sha2::{Digest, Sha256};
use subc_control::{
    ClientControlPush, LiveSpawn, ModuleProtocol, RouteCloseReason, SpawnCursor, SpawnEvent,
    SpawnEventKind, SpawnSnapshot, SupervisorHealthStatus, TerminalDisposition, TerminalExitKind,
};
use subc_protocol::{
    manifest::{SelfSignalKind, SignalAnchor},
    session::{
        HealthReport, HealthStatus, ModuleControlCommand, ModuleControlRequest,
        MODULE_CONTROL_OP_HEALTH_CHECK,
    },
    Flags, FrameType, Principal, Priority, SUBC_LAUNCH_NONCE_ENV, SUBC_MODULE_ID_ENV,
};
use tokio::{
    process::{Child, Command},
    sync::{mpsc, oneshot, watch, Mutex as AsyncMutex},
    task::JoinHandle,
    time::{sleep, sleep_until, timeout, timeout_at, Instant},
};
use tracing::{debug, error, info, warn};

use crate::{
    child_output::StderrPump,
    child_roster::ChildRoster,
    daemon_config::{
        CAPTURE_KEEP_ENV, CAPTURE_MAX_AGE_DAYS_ENV, CAPTURE_MAX_FILE_MB_ENV, CK_LOG_ENV,
    },
    forwarding::{
        CloseReason, ForwardingError, ForwardingTable, GoodbyeTarget, ModuleControlRpcOutcome,
        ModuleDrainTarget, PendingModuleControlRpc,
    },
    provenance::{ExecutableIdentityProbe, SpawnedFileIdentity},
    registry::{ConnectionId, RegistrationEndReason, RegistryError},
    stderr_tail::{ChildOutputSink, StderrRing, StderrTailConfig, StderrTailSnapshot},
    terminal_ring::{TerminalHistorySnapshot, TerminalRecord, TerminalRing, TerminalRingConfig},
    Frame, FrameSink, Registry,
};

#[cfg(not(windows))]
use crate::provenance::spawned_file_identity;

#[path = "supervise_swap.rs"]
mod swap;
use swap::OpenSwap;
use swap::PromotionObserverSlot;
pub(crate) use swap::SwapHelloAdmission;
pub(crate) use swap::SwapPromotionObserver;
pub use swap::DEFAULT_SWAP_READY_TIMEOUT;

#[path = "supervise_activity.rs"]
mod activity;

#[cfg(unix)]
#[path = "supervise/admission.rs"]
mod admission;
mod admission_exit;
#[cfg(all(test, target_os = "linux"))]
mod async_spawn_tests;
#[cfg(all(test, windows))]
mod carried_job_tests;
mod config_delivery;
use config_delivery::cancel_deferred_reload;
use config_delivery::defer_config_spawn;
use config_delivery::hold_config_respawn;
use config_delivery::hold_config_spawn;
use config_delivery::record_approved_attempt_failure;
use config_delivery::try_park_config_attempt;
use config_delivery::CONFIG_ADMISSION_RETRY_DELAY;
mod content_binding;
mod program_binding;
mod registration;
pub(crate) mod resume_hello;
use registration::arm_registration_deadline;
use registration::constant_time_eq;
use registration::handle_reload_child_registration_failure;
use registration::handle_reload_spawn_failure;
pub(crate) use registration::notify_registration_release;
use registration::registration_child_is_running;
use registration::registration_failure_exit_report;
use registration::release_dead_registration;
use registration::wait_for_registration_after_reload;
use registration::wait_for_registration_release;
use registration::wait_for_slot_registration_release;
use registration::watch_registration;
#[cfg(test)]
use registration::IdentitySamplePause;
use registration::RegistrationWaitOutcome;
use registration::RegistrationWatch;
use registration::ReloadRegistrationFailure;
pub(crate) use registration::ReservedHelloRejection;
pub(crate) use registration::ReservedHelloVerification;
pub(crate) use registration::DEFAULT_REGISTER_TIMEOUT;
use registration::REGISTRY_RELEASE_POLL;
use registration::REGISTRY_RELEASE_TIMEOUT;
pub(crate) mod script;
/// Each launch nonce, mapped to its launch's module id and the launch.
type NonceLaunches = HashMap<String, (String, std::sync::Weak<RegistrationSpawn>)>;
pub(crate) use registration::NonceAttestation;
#[cfg(all(test, unix))]
pub(crate) use registration::RegistrationProof;
pub(crate) use registration::RegistrationSpawn;
pub(crate) use registration::REGISTRATION_CHANNEL_CLOSED;
#[cfg(all(test, unix))]
pub(crate) mod unix_channel_placement_tests;
use program_binding::ProgramBindingPublication;
mod config_exit_failure;
#[cfg(all(test, any(target_os = "linux", windows)))]
mod missing_path_tests;
#[cfg(all(test, unix))]
mod program_binding_tests;
#[cfg(all(test, windows))]
mod program_binding_windows_tests;
#[cfg(all(test, windows))]
mod windows_channel_tests;
pub use config_exit_failure::ModuleStopReason;
mod unregistered_stop;
use unregistered_stop::stop_unregistered_child;
#[cfg(target_os = "linux")]
#[path = "supervise/cgroup_label.rs"]
mod cgroup_label;
#[cfg(all(test, unix))]
mod config_exit_hold_tests;
pub(crate) mod containment;
#[cfg(all(test, unix))]
mod containment_tests;
#[cfg(target_os = "linux")]
mod ordinary_cgroup;
#[cfg(unix)]
pub(crate) mod plain_group;
#[cfg(all(test, target_os = "linux"))]
mod plain_group_regression;
mod slow_retry;
use slow_retry::{FailedRetry, FailureContext};
mod host_health;
use host_health::HostHealth;

#[cfg(test)]
mod crash_admission_tests;
mod crash_budget;
use crash_budget::budgeted_restart_child;
use crash_budget::daemon_will_restart;
use crash_budget::log_crash_respawn;
use crash_budget::reset_restart_count;
use crash_budget::respawn_still_pending;
use crash_budget::schedule_respawn;
use crash_budget::CrashRestartSchedule;
pub use crash_budget::RestartPolicy;
#[cfg(all(test, unix))]
mod mark_readiness_tests;
#[cfg(all(test, unix))]
mod slow_retry_tests;
use crash_budget::{admit_running_child_async, spawn_and_mark_running_async};

struct SupervisedChild {
    registration_spawn: Option<Arc<RegistrationSpawn>>,
    /// Owned daemon endpoint, retained for the per-spawn registration service.
    #[cfg(any(unix, windows))]
    registration_channel: Option<SpawnRegistrationChannel>,
    live_program_binding: Option<Arc<content_binding::LiveProgramBinding>>,
    registered_config_connection: Option<ConnectionId>,
    admission_base: Option<subc_config::Digest>,
    registration_declaration: Option<String>,
    _program_binding: Option<ProgramBindingPublication>,
    subtree_target: crate::subtree_resources::Target,
    child: containment::ProcessChild,
    exec: crate::activity::ExecOwner,
    #[cfg(windows)]
    nonce_delivery: Option<subc_os::LaunchNoncePipeDelivery>,
    #[cfg(target_os = "macos")]
    privacy_exec: Option<PrivacyExec>,
    /// Set once this launch's exec acknowledgement confirms the module image.
    /// On macOS the pid first runs the `ck-subc` launch trampoline (see
    /// `subc_os::privacy_identity`), which then replaces itself with the
    /// module. The supervisor owns and can kill that pid from spawn, but
    /// status readers report it only after this latch is set, so nothing
    /// reports the trampoline's image as the module's.
    #[cfg(target_os = "macos")]
    report_ready: Arc<OnceLock<()>>,
    /// Supervisor failure detail retained for the terminal record, including
    /// image refusals and first-HELLO timeouts after a successful spawn.
    spawn_failure: Option<String>,
    /// Set only after a kill requested because launch admission failed was
    /// accepted, never for an operator, health, or shutdown kill.
    admission_kill_sent: bool,
    /// The protocol this process was launched with. A reload can store a new
    /// launch spec with a different protocol, but that takes effect only at the
    /// next spawn, so this process keeps being handled by the protocol it
    /// actually speaks.
    protocol: ModuleProtocol,
    /// This container's cgroup name: a bounded module/slot label followed by a
    /// unique suffix (when cgroup placement is on). Carried replacements reuse
    /// the name; ordinary launches each get a fresh container. A
    /// retired process in a slot may still be draining when a later one is
    /// spawned into that slot, so the suffix keeps the later process out of
    /// the retired one's cgroup, which is the domain a kill applies to.
    #[cfg(target_os = "linux")]
    module_id: String,
    #[cfg(target_os = "linux")]
    cgroup_placement: Option<subc_cgroup::Placement>,
    #[cfg(target_os = "linux")]
    unresolved_cgroup: Option<ordinary_cgroup::Unresolved>,
    /// The job for an ordinary, non-carried launch. For carried
    /// launches the actor owns it and the child holds only a weak container link.
    ///
    /// Dropping this handle is what reaps a surviving tree when no supervisor
    /// code runs — a daemon crash — because the job carries
    /// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`.
    ///
    /// That limit is not crash-only, and the difference is worth knowing: a
    /// Windows daemon *stop* is `taskkill` or the scheduler's `/End` — the
    /// SIGTERM handler is `#[cfg(unix)]` — so the daemon dies with no stop
    /// notice and the kernel closes the job handle, `TerminateProcess`ing every
    /// module at once. Before this change they survived that, saw EOF on the
    /// control socket, and ran their own teardown; Unix keeps that path
    /// deliberately, so a module can seal a WAL or close a capture rather than
    /// be killed mid-write. So this trades graceful teardown on every Windows
    /// daemon stop for containment on a crash, which is the right way round
    /// today: orphaned GPU workers are a reported, recurring problem, and the
    /// modules that write most heavily do not run on Windows.
    ///
    /// The fix is a real Windows stop path — the daemon draining before it
    /// exits, the twin of the Unix SIGTERM handler. Once it exists, this limit
    /// reaches only what the drain left behind, which is what it should reach.
    #[cfg(windows)]
    job: Option<Arc<Mutex<subc_jobobject::JobObject>>>,
    stdout_pump: Option<tokio::sync::oneshot::Receiver<()>>,
    stderr_pump: Option<StderrPump>,
    #[cfg(unix)]
    capture_stop: Option<crate::child_output::StopReaders>,
    #[cfg(unix)]
    late_output_lost: bool,
    stderr_ring: Arc<Mutex<StderrRing>>,
    /// Conservative monotonic bound sampled before `command.spawn()`, not when
    /// identity recording finishes or the spawn generation is published.
    spawned_at: Instant,
    config_delivery: Option<crate::config_gate::runtime::DeliverySpawn>,
    /// Ordinary spawns must send their first HELLO within this budget. Once
    /// accepted, a later disconnect must not restart the registration clock.
    registration_deadline: Option<Instant>,
    registration_watch: Option<RegistrationWatch>,
    spawned_at_ms: u64,
    spawned_from: PathBuf,
    launch_spec_digest: LaunchSpecDigest,
    spawned_file_identity: Option<SpawnedFileIdentity>,
    process_start_time: Option<u64>,
    process_identity: Option<ProcessIdentity>,
    pid: u32,
    /// This process's entry in the daemon's child roster, released when the
    /// process is reaped or this handle is dropped.
    roster_guard: Option<crate::child_roster::RosterGuard>,
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
    DeliberateSeverance,
}

impl From<ExitKind> for TerminalExitKind {
    fn from(kind: ExitKind) -> Self {
        match kind {
            ExitKind::Clean => Self::Clean,
            ExitKind::Crash => Self::Crash,
            ExitKind::DeliberateSeverance => Self::DeliberateSeverance,
        }
    }
}

/// Exact process identity retained when a supervised module registers its
/// connection. PID reuse makes a PID alone insufficient evidence of ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProcessIdentity {
    pub(crate) pid: u32,
    pub(crate) start_time: u64,
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
    pub(crate) admission_refused: Option<String>,
    pub(crate) legacy_registration_spawn: bool,
    pub(crate) registration_declaration: Option<String>,
    pub module_id: String,
    /// Retained configuration-exit infrastructure failure. Cleared only after a
    /// successful launch; an ordinary stop has no infrastructure stop reason.
    pub stop_reason: Option<ModuleStopReason>,
    /// A previous daemon's unresolved process, not a child owned by this supervisor.
    pub waiting_for_previous_pid: Option<u32>,
    /// Why the previous instance still prevents a new start.
    pub waiting_for_previous_detail: Option<String>,
    /// The daemon's Windows per-spawn record: `pipe` after the process it
    /// spawned read all nonce bytes, otherwise `env` for the offered copy.
    /// List/status reports use this record, not the source the module sends
    /// in its registration message's manifest.provenance.launch_nonce_source.
    pub launch_nonce_source: Option<String>,
    pub state: ModuleState,
    pub enabled: bool,
    pub process_alive: bool,
    pub registration_active: bool,
    /// The module's declared wire protocol, carried beside `live` because it is
    /// what makes `live` readable: the two fields answer one question together.
    /// While a process is alive this is its launch declaration, not a later
    /// pending-reload edit. When down it is the configured next launch protocol.
    pub protocol: ModuleProtocol,
    /// Whether the module is serving, under the strongest definition the daemon
    /// can assert for its protocol.
    ///
    /// A subc module must also be REGISTERED: its process being alive says
    /// nothing about whether it can take a request. A `protocol: "none"` module
    /// never registers, so that term is dropped and this falls back to "enabled,
    /// running, and the process the daemon launched is alive" -- which is all
    /// the daemon observes about a process that speaks no subc wire. It stays a
    /// `bool` on the wire for compatibility; renderers pair it with `protocol`
    /// rather than printing it bare.
    pub live: bool,
    /// Crash restarts spent INSIDE `restart_window` as of this read. Older
    /// restarts have already released their slot, so this count can go down
    /// without anybody touching the module.
    pub restart_count: u32,
    /// Replacement processes spawned over this module's entire supervisor lifetime;
    /// unlike `restart_count`, this value is never reset by an operator action
    /// and never falls out of a window.
    pub lifetime_restarts: u32,
    pub spawn_generation: u64,
    /// Retained daemon-local outcome; the legacy control response omits it.
    pub swap_outcome: Option<SwapOutcome>,
    /// The budget `restart_count` is spent against. Carried alongside the count
    /// because the count alone does not say how close the module is to being
    /// disabled, and reporting one without the other is what makes an
    /// about-to-be-retired module look ordinary.
    pub max_restarts: u32,
    /// The span `restart_count` is counted over. Carried with the pair above for
    /// the same reason they are carried together: "2 of 3" means one thing for a
    /// ten-minute window and something else entirely for a lifetime.
    pub restart_window: Duration,
    /// Effective drain and restart timing policy used by this running module.
    /// These values are carried together with the restart budget so status
    /// readers can compare configured intent with what the supervisor applied.
    pub drain_timeout: Duration,
    pub restart_backoff: Duration,
    pub restart_max_backoff: Duration,
    /// The module's process. On macOS this stays absent while the `ck-subc`
    /// launch trampoline is still running in that pid, and appears once the
    /// exec acknowledgement confirms the module image has replaced it. Launch
    /// time and the supervisor's own hold on the process are unaffected.
    pub pid: Option<u32>,
    pub spawned_at_ms: Option<u64>,
    pub spawned_from: Option<PathBuf>,
    pub(crate) launch_spec_digest: Option<LaunchSpecDigest>,
    pub process_start_time: Option<u64>,
    pub last_exit: Option<ExitReport>,
    pub health: ModuleHealthStatus,
}

#[derive(Debug, Clone, PartialEq)]
struct SupervisorSnapshot {
    admission_refused: Option<String>,
    /// Distinguishes an admission-parked actor from an unrelated terminal exit.
    config_hold_parked: bool,
    /// Preserve daemon attribution for a paused start so a failing release can
    /// schedule the next slow retry instead of treating the start as operator work.
    config_hold_daemon_initiated: bool,
    /// The approval that owns the parked attempt, not whichever approval is latest.
    config_hold_approval: Option<crate::config_gate::runtime::ApprovalIdentity>,
    /// Retain an approved launch failure across competing reconciliation passes.
    approved_start_failure: Option<(crate::config_gate::runtime::ApprovalIdentity, String)>,
    legacy_registration_spawn: bool,
    registration_declaration: Option<String>,
    stop_reason: Option<ModuleStopReason>,
    config_exit_failed_approval: Option<crate::config_gate::runtime::ApprovalIdentity>,
    subtree_target: crate::subtree_resources::Target,
    failed_retry: FailedRetry,
    exec_outcome: Option<Arc<crate::activity::ExecOutcome>>,
    #[cfg(windows)]
    pipe_consumed: Option<Arc<OnceLock<()>>>,
    /// Counts running-child loop turns, not executor polls of a parked wait.
    #[cfg(test)]
    actor_turns: u64,
    /// Running select after exec confirmation and with no unconsumed registry
    /// event. Recording a spawn or starting a loop turn is not this barrier.
    #[cfg(test)]
    actor_select: Option<ActorSelectCheckpoint>,
    state: ModuleState,
    enabled: bool,
    process_alive: bool,
    spawned_protocol: Option<ModuleProtocol>,
    spawn_failure: Option<String>,
    waiting_for_previous_pid: Option<u32>,
    waiting_for_previous_detail: Option<String>,
    /// When each crash restart was spent, oldest first. This IS the crash
    /// budget: its in-window length is the count an operator sees and the count
    /// the restart decision is made against, so there is no second counter that
    /// can disagree with it. Bounded by `max_restarts`, and cleared by the same
    /// operator actions that used to zero the old lifetime counter.
    crash_restarts: VecDeque<Instant>,
    lifetime_restarts: u32,
    /// Successful child spawns in this daemon incarnation.
    ///
    /// `lifetime_restarts` was considered and rejected: it starts at zero
    /// (line 640), successful initial/operator spawns in `set_running` do not
    /// increment it (lines 5264-5274), and crash/deliberate retry bookkeeping
    /// increments before a successful replacement exists (lines 604, 3846,
    /// and 3921), so a failed spawn can consume it. This counter moves only
    /// when a live PID is accepted below.
    spawn_generation: u64,
    swap_outcome: Option<SwapOutcome>,
    pid: Option<u32>,
    #[cfg(target_os = "macos")]
    report_ready: Option<Arc<OnceLock<()>>>,
    /// Last reaped child, retained after current process facts are cleared.
    reaped_pid: Option<u32>,
    /// Whether a replacement has already been scheduled for the current process.
    respawn_pending: bool,
    /// A second restart is waiting for the replacement already scheduled.
    coalesced_restart_pending: bool,
    spawned_at_ms: Option<u64>,
    spawned_from: Option<PathBuf>,
    launch_spec_digest: Option<LaunchSpecDigest>,
    spawned_file_identity: Option<SpawnedFileIdentity>,
    process_start_time: Option<u64>,
    deliberate_severance: Option<ProcessIdentity>,
    last_exit: Option<ExitReport>,
    /// Diagnostic attached to the next drain's terminal record, if any.
    drain_disposition_detail: Option<String>,
    health: ModuleHealthStatus,
    last_probe_evidence: Option<HealthProbeEvidence>,
    /// Whether the current process was started as a swap candidate and so
    /// lives in the module's alternate cgroup. The next swap's candidate takes
    /// the other one, so the two processes of a swap never share a cgroup. A
    /// plain spawn always uses the primary cgroup.
    in_alternate_slot: bool,
    /// Whether the current `Draining` state ends in a replacement process
    /// (restart, reload, health restart) rather than a stop. Only meaningful
    /// while `state` is `Draining`; every entry into that state rewrites it.
    /// It is what lets route.open answer the retryable `module_reloading` to a
    /// consumer that reaches a still-registered process mid-restart, instead of
    /// the `supervisor_not_live` a stop or disable deserves.
    draining_to_replace: bool,
    /// Whether a configuration update has been applied since the current
    /// process was spawned, so that process runs an older spec than the one
    /// the supervisor now holds. A queued restart is only coalesced into a
    /// fresher process when this is false: a restart requested to pick up a
    /// new configuration must not be satisfied by a process that predates it.
    configuration_updated_since_spawn: bool,
}

impl SupervisorSnapshot {
    /// The pid that status, provenance and resource readings may report. While
    /// the launch trampoline still runs in the pid, reading its executable or
    /// resource use would describe `ck-subc`, not the module, so none is
    /// reported until the exec acknowledgement confirms the module image.
    fn reported_pid(&self) -> Option<u32> {
        #[cfg(target_os = "macos")]
        if self
            .report_ready
            .as_ref()
            .is_some_and(|ready| ready.get().is_none())
        {
            return None;
        }
        self.pid
    }

    fn starting() -> Self {
        Self::new(ModuleState::Starting, true)
    }

    fn disabled() -> Self {
        Self::new(ModuleState::Disabled, false)
    }

    /// Give the module its full budget back, as an operator restart, reload, or
    /// re-enable does. `lifetime_restarts` deliberately does not move: it is the
    /// ledger of what actually happened, and an operator action does not unmake
    /// replacements that already ran.
    fn clear_crash_restarts(&mut self) {
        self.crash_restarts.clear();
    }

    fn new(state: ModuleState, enabled: bool) -> Self {
        Self {
            admission_refused: None,
            config_hold_parked: false,
            config_hold_daemon_initiated: false,
            config_hold_approval: None,
            approved_start_failure: None,
            legacy_registration_spawn: false,
            registration_declaration: None,
            stop_reason: None,
            config_exit_failed_approval: None,
            subtree_target: crate::subtree_resources::Target::Unavailable("not_running"),
            failed_retry: FailedRetry::default(),
            exec_outcome: None,
            #[cfg(test)]
            actor_turns: 0,
            #[cfg(test)]
            actor_select: None,
            state,
            enabled,
            process_alive: false,
            #[cfg(windows)]
            pipe_consumed: None,
            spawned_protocol: None,
            spawn_failure: None,
            waiting_for_previous_pid: None,
            waiting_for_previous_detail: None,
            crash_restarts: VecDeque::new(),
            lifetime_restarts: 0,
            spawn_generation: 0,
            swap_outcome: None,
            pid: None,
            #[cfg(target_os = "macos")]
            report_ready: None,
            reaped_pid: None,
            respawn_pending: false,
            coalesced_restart_pending: false,
            spawned_at_ms: None,
            spawned_from: None,
            launch_spec_digest: None,
            spawned_file_identity: None,
            process_start_time: None,
            deliberate_severance: None,
            last_exit: None,
            drain_disposition_detail: None,
            health: ModuleHealthStatus::default(),
            last_probe_evidence: None,
            in_alternate_slot: false,
            draining_to_replace: false,
            configuration_updated_since_spawn: false,
        }
    }
}

type SharedSnapshot = Arc<Mutex<SupervisorSnapshot>>;

#[derive(Debug, Clone, Copy)]
enum RespawnKind {
    Spawn,
    CrashRestart,
    Reload,
}

#[derive(Debug, Clone)]
struct PendingRespawn {
    deadline: Instant,
    kind: RespawnKind,
    daemon_initiated: bool,
    approval: Option<crate::config_gate::runtime::ApprovalIdentity>,
}

impl PendingRespawn {
    fn failure_context(&self, runtime: &SupervisorRuntimeConfig) -> FailureContext {
        FailureContext {
            approval: self.approval.clone(),
            ..runtime.failure_context(self.daemon_initiated)
        }
    }
}

type ReloadReply = oneshot::Sender<Result<(), SuperviseError>>;

#[derive(Debug, Clone)]
struct SupervisorRuntimeConfig {
    registry: Arc<Registry>,
    config_delivery: Option<config_delivery::ConfigDelivery>,
    /// Retained across an operation's deferred backoff, until its child starts.
    recovery_is_daemon: bool,
    start_requester: Option<Principal>,
    pending_activity_restart: bool,
    provenance_probe: ExecutableIdentityProbe,
    /// Restart operations hand their backoff to the loop, which keeps serving commands.
    scheduled_respawn: Arc<Mutex<Option<PendingRespawn>>>,
    /// A reload acknowledges completion only after its replacement registers.
    deferred_reload_reply: Arc<Mutex<Option<ReloadReply>>>,
    restart_policy: RestartPolicy,
    register_timeout: Duration,
    /// This module's RESOLVED drain budget: per-module config when present,
    /// else `default_drain_timeout`.
    drain_timeout: Duration,
    /// Shared with the status handle so the attested value changes atomically
    /// when a rescan updates the running drain policy.
    effective_drain_timeout: Arc<Mutex<Duration>>,
    /// The supervisor-wide fallback, kept so a configuration update that
    /// REMOVES the per-module override can re-resolve to it.
    default_drain_timeout: Duration,
    health: HealthConfig,
    host_health: HostHealth,
    connection_file_path: Option<PathBuf>,
    capture_logs_dir: Option<PathBuf>,
    forwarding: Option<Arc<ForwardingTable>>,
    supervisor_handle: Option<SupervisorHandle>,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    replacement_ticket: Option<live_upgrade::ReplacementTicket>,
    /// This module's stderr tail, shared with the [`SupervisedModule`] that answers
    /// status queries.
    ///
    /// One ring per module, held across every respawn. The lines explaining an exit
    /// are written BEFORE that exit, so a ring recreated per process would be empty
    /// exactly when it is asked for.
    stderr_ring: Arc<Mutex<StderrRing>>,
    terminal_ring: Arc<Mutex<TerminalRing>>,
    spawn_events: SpawnEventFeed,
    child_roster: ChildRoster,
    #[cfg(target_os = "linux")]
    cgroup_placement: Option<subc_cgroup::Placement>,
    #[cfg(test)]
    test_seed_stale_facts_before_enable_spawn: bool,
    #[cfg(test)]
    test_reload_exit_record_gate: Option<Arc<ReloadExitRecordGate>>,
}

impl SupervisorRuntimeConfig {
    fn failure_context(&self, daemon_initiated: bool) -> FailureContext {
        FailureContext {
            daemon_initiated,
            approval: None,
            shutdown: self.child_roster.shutdown_flag(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SupervisedConfiguration {
    spec: ModuleSpec,
    enabled: bool,
    health: HealthConfig,
    register_timeout: Duration,
    restart_policy: RestartPolicy,
}

/// Shared lookup table for module handles and launch authority. The supervisor
/// records each launch and the control handler verifies HELLOs and route opens.
#[derive(Debug, Clone, Default)]
pub struct SupervisorHandle {
    pub(crate) pending_adoptions: resume_hello::PendingAdoptions,
    upgrade_state: live_upgrade::UpgradeState,
    #[cfg(test)]
    resource_commit_pause: Arc<Mutex<Option<resource_launch::ResourceCommitPause>>>,
    #[cfg(test)]
    registration_test_channels: CapturedRegistrationChannels,
    registration_service: Arc<Mutex<Option<registration::RegistrationService>>>,
    registration_connections: Arc<Mutex<HashMap<ConnectionId, Arc<RegistrationSpawn>>>>,
    registration_launches: Arc<Mutex<Vec<std::sync::Weak<RegistrationSpawn>>>>,
    gate_runtime: Arc<Mutex<Option<std::sync::Weak<crate::config_gate::runtime::RuntimeOwner>>>>,
    program_bindings: program_binding::ProgramBindings,
    #[cfg(test)]
    identity_sample_pause: Arc<Mutex<Option<IdentitySamplePause>>>,
    #[cfg(all(test, unix))]
    plain_identity_policy_pause: Arc<Mutex<Option<(String, IdentitySamplePause)>>>,
    host_health: HostHealth,
    restart_intent: Arc<Mutex<DaemonRestartIntent>>,
    activity: Arc<Mutex<Option<crate::activity::ActivityLedger>>>,
    activity_images: Arc<Mutex<HashMap<String, activity::ActivityImage>>>,
    modules: Arc<Mutex<HashMap<String, SupervisedModule>>>,
    /// Module ids the supervisor has taken on. An id is added BEFORE the
    /// module's first process is spawned and removed only when the module
    /// leaves the roster (see [`Self::retire`]), so it is always a superset of
    /// the keys of `modules`.
    ///
    /// `modules` cannot answer "is this module configured?" on its own: a
    /// [`SupervisedModule`] only exists once its process has been spawned, and
    /// a fast child can connect, register, sync its scopes and ask about them
    /// before the supervisor has inserted it. Answering "not configured" in that
    /// gap makes scope admission refuse with the terminal "will never sync"
    /// instead of the retryable "has not synced yet".
    configured_ids: Arc<Mutex<HashSet<String>>>,
    spawn_events: SpawnEventFeed,
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
    /// Reservations installed only to prevent HELLO for a protocol:none process.
    /// Replacement by a subc process releases them; explicitly reserved ids are
    /// never placed in this set and still require an operator release.
    implicit_protocol_none: Arc<Mutex<HashSet<String>>>,
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
    /// The launch each live launch nonce was minted for, keyed by nonce, with
    /// the launch's module id, so a nonce-attested consumer or registration
    /// resolves to that launch's authority record. An entry whose launch has
    /// ended stays while its nonce still authorizes, so a nonce whose record
    /// is gone is told apart from one that never had a record.
    nonce_launches: Arc<Mutex<NonceLaunches>>,
    /// Reserved namespace prefixes mapped to the supervised owner module whose
    /// current spawn nonce authorizes HELLO claims below the prefix.
    ///
    /// Per §2.6 this is not a same-user security barrier: a same-user process can
    /// read the key file and launch nonce env. Like exact reserved ids, it prevents
    /// accidental collisions and lower-trust processes from squatting protected
    /// namespaces.
    reserved_prefix_owners: Arc<Mutex<HashMap<String, String>>>,
    /// Blue/green swaps in progress, by module id. An entry exists from just
    /// before the candidate process is spawned until the swap has failed, or
    /// has cut over and the old process is gone. While it exists, HELLO for the
    /// id is gated on the swap token (see [`Self::swap_hello_admission`]) and
    /// consumer attestation accepts both processes' nonces.
    swaps: Arc<Mutex<HashMap<String, OpenSwap>>>,
    /// Told when a swap promotes its candidate; see [`SwapPromotionObserver`].
    promotion_observer: PromotionObserverSlot,
    /// Serializes module-set reconciliation with operator lifecycle commands. Without
    /// this daemon-wide ordering, a rescan could retire or update a module while a
    /// concurrent reload still held its old handle and launch specification.
    operation_lock: Arc<AsyncMutex<()>>,
}

#[derive(Debug)]
struct DaemonRestartIntent {
    expires: Option<Duration>,
    window: Duration,
}

impl Default for DaemonRestartIntent {
    fn default() -> Self {
        Self {
            expires: None,
            window: Duration::from_secs(120),
        }
    }
}

impl SupervisorHandle {
    pub(crate) fn module_is_gated(&self, module: &str) -> bool {
        let runtime = {
            let Ok(runtime) = self.gate_runtime.lock() else {
                return true;
            };
            runtime.clone()
        };
        runtime.is_some_and(|runtime| {
            runtime
                .upgrade()
                .is_none_or(|owner| owner.module_is_gated(module))
        })
    }
    pub(crate) fn announce_daemon_restart(&self) -> u64 {
        let mut intent = self
            .restart_intent
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        intent.expires = Some(subc_uptime::suspend_inclusive_now() + intent.window);
        unix_ms_now().saturating_add(intent.window.as_millis() as u64)
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn set_shutdown_intent_window(&self, window: Duration) {
        self.restart_intent
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .window = window;
    }

    #[cfg(unix)]
    fn take_daemon_shutdown_reason(&self) -> RouteCloseReason {
        let expiry = self
            .restart_intent
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .expires
            .take();
        if expiry.is_some_and(|expiry| subc_uptime::suspend_inclusive_now() < expiry) {
            RouteCloseReason::Restart
        } else {
            RouteCloseReason::Disable
        }
    }

    pub fn new() -> Self {
        Self::default()
    }

    pub(crate) fn operation_lock(&self) -> Arc<AsyncMutex<()>> {
        Arc::clone(&self.operation_lock)
    }
}

/// Process supervisor for subc-owned singleton modules.
#[derive(Debug, Clone)]
pub struct Supervisor {
    registry: Arc<Registry>,
    config_delivery: Option<config_delivery::ConfigDelivery>,
    restart_policy: RestartPolicy,
    drain_timeout: Duration,
    connection_file_path: Option<PathBuf>,
    capture_logs_dir: Option<PathBuf>,
    forwarding: Option<Arc<ForwardingTable>>,
    process_liveness: Arc<SupervisorProcessLiveness>,
    supervisor_handle: Option<SupervisorHandle>,
    health: HealthConfig,
    host_health: HostHealth,
    daemon_start_clock: crate::clock::StartClock,
    terminal_journal: Option<Arc<crate::terminal_journal::TerminalJournal>>,
    spawn_events: SpawnEventFeed,
    provenance_probe: ExecutableIdentityProbe,
    /// Every process spawned through this supervisor (and its clones) and not
    /// yet reaped, so daemon shutdown can end them.
    child_roster: ChildRoster,
    #[cfg(target_os = "linux")]
    cgroup_placement: Option<subc_cgroup::Placement>,
    #[cfg(test)]
    test_after_first_spawn: AfterFirstSpawnHook,
}

impl Supervisor {
    pub fn new(registry: Arc<Registry>, restart_policy: RestartPolicy) -> Self {
        Self {
            registry,
            config_delivery: None,
            restart_policy,
            drain_timeout: DEFAULT_DRAIN_TIMEOUT,
            connection_file_path: None,
            capture_logs_dir: None,
            forwarding: None,
            process_liveness: Arc::new(SupervisorProcessLiveness::default()),
            supervisor_handle: None,
            health: HealthConfig::default(),
            host_health: HostHealth::default(),
            daemon_start_clock: crate::clock::StartClock::capture(),
            terminal_journal: None,
            spawn_events: SpawnEventFeed::default(),
            provenance_probe: ExecutableIdentityProbe::default(),
            child_roster: ChildRoster::default(),
            #[cfg(target_os = "linux")]
            cgroup_placement: None,
            #[cfg(test)]
            test_after_first_spawn: AfterFirstSpawnHook::default(),
        }
    }

    pub fn with_drain_timeout(mut self, drain_timeout: Duration) -> Self {
        self.drain_timeout = drain_timeout;
        self
    }

    /// The drain budget a module gets when its config entry sets none.
    pub(crate) fn default_drain_timeout(&self) -> Duration {
        self.drain_timeout
    }

    pub fn with_process_liveness(
        mut self,
        process_liveness: Arc<SupervisorProcessLiveness>,
    ) -> Self {
        self.process_liveness = process_liveness;
        self
    }

    /// Connect configuration delivery to observed process lifecycle events.
    pub fn with_config_runtime(
        mut self,
        owner: Arc<crate::config_gate::runtime::RuntimeOwner>,
    ) -> Self {
        if let Some(handle) = &self.supervisor_handle {
            handle.install_binding_retirement(&owner);
        }
        self.config_delivery = Some(config_delivery::ConfigDelivery(owner));
        self
    }

    pub fn with_connection_file_path(mut self, connection_file_path: impl Into<PathBuf>) -> Self {
        self.connection_file_path = Some(connection_file_path.into());
        self
    }

    /// Enables daemon-owned capture files for supervised stdout and stderr.
    pub fn with_capture_logs_dir(mut self, logs_dir: impl Into<PathBuf>) -> Self {
        self.capture_logs_dir = Some(logs_dir.into());
        self
    }

    /// Names this daemon lifetime in spawn events, independently of whether a
    /// terminal journal is configured.
    pub fn with_daemon_incarnation(self, daemon_incarnation: String) -> Self {
        // A millisecond start stamp can repeat after clock rollback or a rapid
        // restart. Use the connection file's random daemon_id instead: it already
        // identifies this daemon lifetime independently of the wall clock.
        self.spawn_events.configure_incarnation(daemon_incarnation);
        self
    }

    /// Enables best-effort history shared by every supervised module. Without
    /// it, terminal history is kept only in each module's in-memory ring.
    pub fn with_terminal_journal(self, path: PathBuf, daemon_incarnation: String) -> Self {
        let mut this = self.with_daemon_incarnation(daemon_incarnation.clone());
        this.terminal_journal = Some(Arc::new(crate::terminal_journal::TerminalJournal::open(
            path,
            daemon_incarnation,
        )));
        this
    }

    pub fn with_forwarding(mut self, forwarding: Arc<ForwardingTable>) -> Self {
        self.child_roster.counters = forwarding.counters();
        self.forwarding = Some(forwarding);
        self
    }

    pub fn with_handle(mut self, supervisor_handle: SupervisorHandle) -> Self {
        if let Some(delivery) = &self.config_delivery {
            supervisor_handle.install_binding_retirement(&delivery.0);
        }
        self.host_health = supervisor_handle.host_health.clone();
        self.spawn_events = supervisor_handle.spawn_events.clone();
        self.supervisor_handle = Some(supervisor_handle);
        self
    }

    pub fn with_health_config(mut self, health: HealthConfig) -> Self {
        self.health = health;
        self
    }

    /// Keeps a record of every live child at `path`, rewritten on each spawn and
    /// reap, for the orphan sweep a later daemon runs at boot. Without it no
    /// record is kept.
    pub fn with_live_children_record(self, path: impl Into<PathBuf>) -> Self {
        self.child_roster.record_to(path.into());
        self
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn with_resume_roster(
        self,
        path: PathBuf,
        protected: &crate::live_children::AdoptedPids,
    ) -> Self {
        self.child_roster.record_to_with_adopted(path, protected);
        self
    }

    #[cfg(target_os = "linux")]
    pub fn with_cgroup_placement(
        mut self,
        cgroup_placement: Option<subc_cgroup::Placement>,
    ) -> Self {
        self.child_roster
            .set_cgroup_placement(cgroup_placement.clone());
        self.cgroup_placement = cgroup_placement;
        self
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
    retained: crate::child_roster::RetainedView,
    module_id: String,
    registry: Arc<Registry>,
    snapshot: SharedSnapshot,
    configuration: Arc<Mutex<SupervisedConfiguration>>,
    stderr_ring: Arc<Mutex<StderrRing>>,
    terminal_ring: Arc<Mutex<TerminalRing>>,
    commands: mpsc::Sender<SupervisorCommand>,
    monitor: Mutex<Option<JoinHandle<()>>>,
    effective_drain_timeout: Arc<Mutex<Duration>>,
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

    pub fn state(&self) -> Result<ModuleState, SuperviseError> {
        Ok(lock_snapshot(&self.inner.snapshot)?.state)
    }

    pub(crate) fn registration_end_reason(
        &self,
    ) -> Result<Option<RegistrationEndReason>, SuperviseError> {
        let snapshot = lock_snapshot(&self.inner.snapshot)?;
        Ok(match snapshot.state {
            ModuleState::Draining if snapshot.draining_to_replace => {
                Some(RegistrationEndReason::SupervisorRestart)
            }
            ModuleState::Draining => Some(RegistrationEndReason::SupervisorStop),
            ModuleState::Restarting if snapshot.draining_to_replace => {
                Some(RegistrationEndReason::SupervisorRestart)
            }
            _ => None,
        })
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

    /// Retained observations from the current ring and all journal generations.
    ///
    /// Blocking: this reads the journal files. Async callers use
    /// `Self::read_durable_terminal_history`.
    pub fn durable_terminal_history(&self) -> subc_control::TerminalHistory {
        durable_terminal_history_of(&self.inner.terminal_ring, &self.inner.module_id)
    }

    /// [`Self::durable_terminal_history`] on a blocking thread, so the journal
    /// read (up to every retained generation) never occupies a runtime worker.
    /// Fails only if the blocking task could not finish (runtime shutdown or a
    /// panic in the read).
    pub(crate) async fn read_durable_terminal_history(
        &self,
    ) -> Result<subc_control::TerminalHistory, tokio::task::JoinError> {
        let terminal_ring = Arc::clone(&self.inner.terminal_ring);
        let module_id = self.inner.module_id.clone();
        tokio::task::spawn_blocking(move || durable_terminal_history_of(&terminal_ring, &module_id))
            .await
    }

    pub fn status(&self) -> Result<ModuleStatus, SuperviseError> {
        self.status_with_snapshot_lock(&self.inner.snapshot, None)
            .map(|(status, _)| status)
    }

    pub(crate) fn next_retry_at_ms(&self) -> Result<Option<u64>, SuperviseError> {
        let state = lock_snapshot(&self.inner.snapshot)?;
        Ok(state
            .failed_retry
            .pending
            .filter(|_| state.state == ModuleState::Failed && state.enabled)
            .map(|retry| {
                unix_ms_now().saturating_add(
                    u64::try_from(
                        retry
                            .deadline
                            .saturating_duration_since(Instant::now())
                            .as_millis(),
                    )
                    .unwrap_or(u64::MAX),
                )
            }))
    }

    #[cfg(all(test, unix))]
    pub(crate) fn retry_deadline_for_test(&self) -> Option<Instant> {
        lock_snapshot(&self.inner.snapshot)
            .unwrap()
            .failed_retry
            .pending
            .map(|retry| retry.deadline)
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
            .map(|(status, _)| status)
    }

    fn status_with_snapshot_lock(
        &self,
        snapshot: &SharedSnapshot,
        caller: Option<&'static str>,
    ) -> Result<(ModuleStatus, Option<SpawnedFileIdentity>), SuperviseError> {
        let restart_policy = self
            .inner
            .configuration
            .lock()
            .map_err(|_| SuperviseError::StatePoisoned {
                module_id: Some(self.inner.module_id.clone()),
            })?
            .restart_policy;
        let mut guard = match caller {
            Some(caller) => lock_snapshot_for_control(snapshot, &self.inner.module_id, caller)?,
            None => lock_snapshot(snapshot)?,
        };
        // Read the budget through the pruning path so a reader sees the same
        // in-window count the restart decision would use, not a stale total.
        let restart_count = guard.crash_restarts_in_window(restart_policy.window, Instant::now());
        let snapshot = guard.clone();
        drop(guard);
        let drain_timeout = *self.inner.effective_drain_timeout.lock().map_err(|_| {
            SuperviseError::StatePoisoned {
                module_id: Some(self.inner.module_id.clone()),
            }
        })?;
        let registration_active = self
            .inner
            .registry
            .get_module(&self.inner.module_id)
            .map_err(SuperviseError::Registry)?
            .is_some();
        let protocol = snapshot
            .spawned_protocol
            .unwrap_or(self.declared_protocol()?);
        let running_process =
            snapshot.enabled && snapshot.state == ModuleState::Running && snapshot.process_alive;
        // Registration is the difference between the two protocols and the only
        // one: a subc module that has not registered cannot serve a request even
        // though its process is up, and a `none` module never registers at all,
        // so requiring it there would pin `live` to false for the whole life of
        // a perfectly healthy process.
        let live = match protocol {
            ModuleProtocol::Subc => running_process && registration_active,
            ModuleProtocol::None => running_process,
        };
        // Read only the retained cache: a status read must not resend signals,
        // rewrite the record, or keep the containment resource alive.
        let retained = self.inner.retained.waiting_detail(&self.inner.module_id);
        let carry_configured = self
            .inner
            .configuration
            .lock()
            .map_err(|_| SuperviseError::StatePoisoned {
                module_id: Some(self.inner.module_id.clone()),
            })?
            .spec
            .carry_containment;
        let recovery_block = carries_containment(
            carry_configured,
            self.inner
                .retained
                .has_carried_container(&self.inner.module_id),
        )
        .then(|| self.inner.retained.recovery_block())
        .flatten();
        let launch_nonce_source = observed_launch_nonce_source(&snapshot);
        let pid = snapshot.reported_pid();
        let mut health = snapshot.health;
        if let Some(detail) = recovery_block
            .as_ref()
            .or_else(|| retained.as_ref().map(|(_, detail)| detail))
        {
            health.detail = Some(match health.detail {
                Some(previous) => format!("{previous}; {detail}"),
                None => detail.clone(),
            });
        }

        Ok((
            ModuleStatus {
                admission_refused: snapshot.admission_refused,
                legacy_registration_spawn: snapshot.legacy_registration_spawn,
                registration_declaration: snapshot.registration_declaration.clone(),
                stop_reason: snapshot.stop_reason,
                waiting_for_previous_pid: retained
                    .as_ref()
                    .map(|(pid, _)| *pid)
                    .filter(|pid| *pid > 0)
                    .or(snapshot.waiting_for_previous_pid),
                waiting_for_previous_detail: recovery_block
                    .or_else(|| retained.map(|(_, detail)| detail))
                    .or(snapshot.waiting_for_previous_detail),
                module_id: self.inner.module_id.clone(),
                launch_nonce_source,
                state: snapshot.state,
                enabled: snapshot.enabled,
                process_alive: snapshot.process_alive,
                registration_active,
                protocol,
                live,
                restart_count,
                lifetime_restarts: snapshot.lifetime_restarts,
                spawn_generation: snapshot.spawn_generation,
                swap_outcome: snapshot.swap_outcome,
                max_restarts: restart_policy.max_restarts,
                restart_window: restart_policy.window,
                drain_timeout,
                restart_backoff: restart_policy.backoff,
                restart_max_backoff: restart_policy.max_backoff,
                pid,
                spawned_at_ms: snapshot.spawned_at_ms,
                spawned_from: snapshot.spawned_from,
                launch_spec_digest: snapshot.launch_spec_digest,
                process_start_time: snapshot.process_start_time,
                last_exit: snapshot.last_exit,
                health,
            },
            snapshot.spawned_file_identity.clone(),
        ))
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

    /// The status and the running-image check for `supervisor.provenance`,
    /// taken from one status read. The exec acknowledgement can land between
    /// two separate reads, and the reply would then pair "no pid yet" with an
    /// image observed after the module started, which describes no single
    /// moment.
    pub(crate) async fn status_and_running_image_agreement(
        &self,
    ) -> Result<(ModuleStatus, subc_control::RunningImageAgreement), SuperviseError> {
        let (status, identity) = self.status_with_snapshot_lock(&self.inner.snapshot, None)?;
        let image = self
            .inner
            .provenance_probe
            .observe(
                status.pid,
                status.spawned_from.as_deref(),
                identity,
                status.process_start_time,
            )
            .await;
        Ok((status, image))
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
                snapshot.reported_pid(),
                snapshot.spawned_from.as_deref(),
                snapshot.spawned_file_identity.clone(),
                snapshot.process_start_time,
            )
            .await
    }

    /// Memory and CPU time of the module's current process, read now. Only the
    /// process the supervisor spawned is read, not processes it has started.
    pub(crate) fn child_resource_usage(&self) -> subc_control::ChildResourceUsage {
        #[cfg(windows)]
        {
            let image = lock_snapshot(&self.inner.snapshot)
                .ok()
                .and_then(|snapshot| {
                    snapshot
                        .spawned_file_identity
                        .as_ref()
                        .and_then(|identity| identity.windows_image.clone())
                });
            match image {
                Some(image) => {
                    crate::child_resources::read_windows_handle(image.process(), image.start_time())
                }
                None => subc_control::ChildResourceUsage::Unavailable {
                    reason: subc_control::ChildResourceUnavailableReason::NotRunning,
                },
            }
        }
        #[cfg(not(windows))]
        {
            let (pid, start_time) = match lock_snapshot(&self.inner.snapshot) {
                Ok(snapshot) => (snapshot.reported_pid(), snapshot.process_start_time),
                Err(_) => {
                    return subc_control::ChildResourceUsage::Unavailable {
                        reason: subc_control::ChildResourceUnavailableReason::Unreadable,
                    }
                }
            };
            crate::child_resources::read(pid, start_time)
        }
    }

    #[cfg(all(test, any(target_os = "linux", windows)))]
    pub(crate) fn subtree_target(&self) -> crate::subtree_resources::Target {
        self.inner
            .snapshot
            .try_lock()
            .map(|snapshot| snapshot.subtree_target.clone())
            .unwrap_or(crate::subtree_resources::Target::Unavailable("read_failed"))
    }

    #[cfg(test)]
    pub(crate) fn set_subtree_target_for_test(&self, target: crate::subtree_resources::Target) {
        self.inner.snapshot.lock().unwrap().subtree_target = target;
    }

    pub(crate) fn will_recover_after_connection_loss(&self) -> Result<bool, SuperviseError> {
        let mut snapshot = lock_snapshot(&self.inner.snapshot)?;
        Ok(match snapshot.state {
            ModuleState::Restarting => true,
            ModuleState::Failed | ModuleState::Disabled => false,
            _ if snapshot.enabled && snapshot.respawn_pending => true,
            _ => daemon_will_restart(
                &mut snapshot,
                &self
                    .inner
                    .configuration
                    .lock()
                    .map_err(|_| SuperviseError::StatePoisoned {
                        module_id: Some(self.inner.module_id.clone()),
                    })?
                    .restart_policy,
                Instant::now(),
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

    /// The current process's protocol, or the configured protocol when down.
    /// A rescan stores the next launch spec without changing how an existing
    /// process registers, serves routes, is probed, or exits.
    pub(crate) fn declared_protocol(&self) -> Result<ModuleProtocol, SuperviseError> {
        let configured = self
            .inner
            .configuration
            .lock()
            .map_err(|_| SuperviseError::StatePoisoned {
                module_id: Some(self.inner.module_id.clone()),
            })?
            .spec
            .protocol;
        let state = lock_snapshot(&self.inner.snapshot)?;
        Ok(state.spawned_protocol.unwrap_or(configured))
    }

    pub(crate) fn configuration(&self) -> Result<(ModuleSpec, HealthConfig), SuperviseError> {
        let configuration =
            self.inner
                .configuration
                .lock()
                .map_err(|_| SuperviseError::StatePoisoned {
                    module_id: Some(self.inner.module_id.clone()),
                })?;
        Ok((configuration.spec.clone(), configuration.health.clone()))
    }

    pub(crate) fn configured_register_timeout(&self) -> Result<Duration, SuperviseError> {
        self.inner
            .configuration
            .lock()
            .map(|configuration| configuration.register_timeout)
            .map_err(|_| SuperviseError::StatePoisoned {
                module_id: Some(self.inner.module_id.clone()),
            })
    }

    pub(crate) fn configured_enabled(&self) -> Result<bool, SuperviseError> {
        self.inner
            .configuration
            .lock()
            .map(|configuration| configuration.enabled)
            .map_err(|_| SuperviseError::StatePoisoned {
                module_id: Some(self.inner.module_id.clone()),
            })
    }

    /// Replace this module's launch spec, keeping its health and drain policy,
    /// the way a rescan does for a changed config entry. The running process is
    /// untouched; the next spawn (a restart, or a swap's candidate) uses it.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn update_spec_for_test(&self, spec: ModuleSpec) -> Result<(), SuperviseError> {
        let (_, health) = self.configuration()?;
        let drain_timeout_ms = u64::try_from(
            self.inner
                .effective_drain_timeout
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .as_millis(),
        )
        .ok();
        self.update_configuration(spec, health, drain_timeout_ms)
            .await
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
                health: health.clone(),
                drain_timeout_ms,
                reply: reply_tx,
            })
            .await
            .map_err(|_| SuperviseError::CommandClosed {
                module_id: self.inner.module_id.clone(),
            })?;
        reply_rx.await.map_err(|_| SuperviseError::CommandClosed {
            module_id: self.inner.module_id.clone(),
        })??;
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

/// Successful daemon-local swap disposition. Supersession leaves the incumbent
/// serving until an approved replacement starts with the latest configured
/// command; the legacy control acknowledgment does not carry this disposition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SwapOutcome {
    Cutover,
    SupersededByConfigRestart,
}

impl SwapOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cutover => "cutover",
            Self::SupersededByConfigRestart => "superseded_by_config_restart",
        }
    }
}

#[derive(Debug)]
pub enum SuperviseError {
    UpgradeInProgress,
    ConfigHeld {
        module_id: String,
        code: String,
        detail: String,
    },
    ConfigDeliveryDeferred {
        module_id: String,
    },
    InvalidSpec {
        reason: String,
    },
    Spawn {
        program: PathBuf,
        source: io::Error,
        cgroup_path: Option<PathBuf>,
    },
    Cgroup {
        module_id: String,
        source: io::Error,
    },
    /// The cryptographic random-number generator failed while generating a
    /// reserved module's launch nonce. Fail loud rather than spawn a reserved
    /// module without its identity binding.
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
    /// A restart or reload arrived while a swap's candidate was warming. The
    /// swap owns the module until it cuts over or fails; a stop or disable
    /// would have aborted it instead.
    SwapInProgress {
        module_id: String,
    },
    /// A swap was refused before anything was spawned.
    SwapRefused {
        module_id: String,
        reason: SwapRefusal,
    },
    /// A swap spawned a candidate and gave up on it. The candidate has been
    /// killed and its slot freed; the incumbent was left serving and was never
    /// drained, except in the one `CutoverLost` case described on that arm.
    SwapFailed {
        module_id: String,
        arm: SwapFailureArm,
        detail: String,
        /// How the candidate exited, when it exited on its own before the
        /// supervisor gave up on it.
        candidate_exit: Option<ExitReport>,
    },
}

/// Why a swap was refused before a candidate was spawned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapRefusal {
    /// The module's config does not declare `overlap: "safe"`.
    OverlapExclusive,
    /// The module is not registered, so there is no incumbent to keep serving
    /// and nothing a swap would improve on; a plain restart is the tool.
    NotRegistered,
    /// The module does not speak the subc wire, so a candidate could never
    /// register or declare itself ready.
    ProtocolNone,
    /// The supervisor lacks the forwarding table (to cut routes over) or the
    /// shared handle (to admit the candidate's HELLO) that a swap needs.
    NotConfigured,
    /// A swap is already open for this module.
    AlreadySwapping,
}

impl SwapRefusal {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OverlapExclusive => "overlap_exclusive",
            Self::NotRegistered => "not_registered",
            Self::ProtocolNone => "protocol_none",
            Self::NotConfigured => "not_configured",
            Self::AlreadySwapping => "already_swapping",
        }
    }
}

/// Which failure arm ended a swap. Every arm but one leaves the incumbent
/// serving and undrained; see `CutoverLost`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapFailureArm {
    /// The candidate process could not be started.
    SpawnFailed,
    /// The candidate did not register within the readiness budget.
    NeverRegistered,
    /// The candidate registered but did not declare itself ready in time.
    NeverReady,
    /// The candidate exited before cutover.
    CandidateExited,
    /// The candidate declared itself ready but failed its health probe.
    CandidateUnhealthy,
    /// An operator stop, disable or retire arrived while the candidate warmed.
    /// The candidate was killed and the operator's command then carried out on
    /// the incumbent.
    Interrupted,
    /// The candidate's connection closed at the moment of cutover. If it
    /// closed before forwarding moved, the incumbent is untouched. If it closed
    /// between the forwarding and registry halves of cutover, forwarding can no
    /// longer route to the incumbent, so the module is restarted plainly.
    CutoverLost,
}

impl SwapFailureArm {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SpawnFailed => "spawn_failed",
            Self::NeverRegistered => "never_registered",
            Self::NeverReady => "never_ready",
            Self::CandidateExited => "candidate_exited",
            Self::CandidateUnhealthy => "candidate_unhealthy",
            Self::Interrupted => "interrupted",
            Self::CutoverLost => "cutover_lost",
        }
    }
}

impl SuperviseError {
    pub(crate) fn is_config_gate_error(&self) -> bool {
        match self {
            Self::ConfigHeld { .. } => true,
            Self::Spawn { source, .. } => source
                .get_ref()
                .is_some_and(|cause| cause.is::<crate::config_gate::GateError>()),
            _ => false,
        }
    }
}

impl fmt::Display for SuperviseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ConfigHeld { module_id, code, detail } => write!(f, "{code}: module '{module_id}' held: {detail}"),
            Self::ConfigDeliveryDeferred { module_id } => write!(f, "config_gated: module '{module_id}' is waiting for its exited delivery target's failure record"),
            Self::InvalidSpec { reason } => write!(f, "invalid module spec: {reason}"),
            Self::Spawn {
                program,
                source,
                cgroup_path: Some(cgroup_path),
            } => write!(
                f,
                "failed to place module in cgroup '{}' while spawning '{}': {source}",
                cgroup_path.display(),
                program.display()
            ),
            Self::Spawn {
                program,
                source,
                cgroup_path: None,
            } => write!(
                f,
                "failed to spawn module '{}': {source}",
                program.display()
            ),
            Self::Cgroup { module_id, source } => {
                write!(
                    f,
                    "failed to prepare cgroup for module '{module_id}': {source}"
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
            Self::UpgradeInProgress => f.write_str("daemon upgrade is in progress"),
            Self::SwapInProgress { module_id } => write!(
                f,
                "module '{module_id}' is being swapped; retry once the swap has cut over or failed, or stop the module to abort the swap"
            ),
            Self::SwapRefused { module_id, reason } => match reason {
                SwapRefusal::OverlapExclusive => write!(
                    f,
                    "module '{module_id}' is declared overlap: \"exclusive\" (the default): two processes of it must not run at once, so it cannot be swapped; use a plain restart, or declare overlap: \"safe\" in its config if it really tolerates a second process"
                ),
                SwapRefusal::NotRegistered => write!(
                    f,
                    "module '{module_id}' is not registered, so there is no serving process to keep while a replacement warms; use a plain restart"
                ),
                SwapRefusal::ProtocolNone => write!(
                    f,
                    "module '{module_id}' is protocol: \"none\" and never registers, so a swap could never see its replacement become ready; use a plain restart"
                ),
                SwapRefusal::NotConfigured => write!(
                    f,
                    "module '{module_id}' cannot be swapped: the supervisor was built without the forwarding table or shared handle a swap needs"
                ),
                SwapRefusal::AlreadySwapping => {
                    write!(f, "module '{module_id}' is already being swapped")
                }
            },
            Self::SwapFailed {
                module_id,
                arm,
                detail,
                ..
            } => write!(
                f,
                "swap of module '{module_id}' failed ({}): {detail}; the running process was left serving",
                arm.as_str()
            ),
        }
    }
}

impl Error for SuperviseError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Spawn { source, .. }
            | Self::Cgroup { source, .. }
            | Self::Wait { source, .. }
            | Self::Kill { source, .. } => Some(source),
            Self::Forwarding(err) => Some(err),
            Self::Registry(err) => Some(err),
            Self::LaunchNonce { .. }
            | Self::ConfigDeliveryDeferred { .. }
            | Self::ConfigHeld { .. }
            | Self::InvalidSpec { .. }
            | Self::ReloadUnavailable { .. }
            | Self::Disabled { .. }
            | Self::ReloadFailed { .. }
            | Self::RegistrationStillActive { .. }
            | Self::StatePoisoned { .. }
            | Self::CommandClosed { .. }
            | Self::SwapInProgress { .. }
            | Self::UpgradeInProgress
            | Self::SwapRefused { .. }
            | Self::SwapFailed { .. } => None,
        }
    }
}

fn unix_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

fn update_snapshot(
    snapshot: &SharedSnapshot,
    module_id: Option<&str>,
    update: impl FnOnce(&mut SupervisorSnapshot),
) -> Result<(), SuperviseError> {
    let mut state = lock_snapshot(snapshot).map_err(|_| SuperviseError::StatePoisoned {
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
    let guard = match snapshot.try_lock() {
        Ok(guard) => return Ok(guard),
        Err(TryLockError::Poisoned(guard)) => Err(guard),
        Err(TryLockError::WouldBlock) => {
            // A held launch record must not pin a daemon worker: that worker
            // may also have to route and deliver the busy reply to a module.
            if tokio::runtime::Handle::try_current().is_ok_and(|handle| {
                handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread
            }) {
                tokio::task::block_in_place(|| snapshot.lock())
            } else {
                snapshot.lock()
            }
        }
    };
    guard.map_err(|_| SuperviseError::StatePoisoned { module_id: None })
}

#[cfg(all(test, unix))]
mod restart_ordering_tests;

#[cfg(all(test, target_os = "macos"))]
#[path = "supervise/suspended_spawn_tests.rs"]
mod suspended_spawn_tests;

#[cfg(all(test, target_os = "linux"))]
#[path = "supervise/cgroup_reap_tests.rs"]
mod cgroup_reap_tests;

#[cfg(test)]
#[path = "supervise/supervise_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "supervise/spawn_record_tests.rs"]
mod spawn_record_tests;

#[cfg(all(test, target_os = "linux"))]
#[path = "supervise/spawn_image_tests.rs"]
mod spawn_image_tests;

#[cfg(test)]
#[path = "supervise/slot_registration_wait_tests.rs"]
mod slot_registration_wait_tests;

#[cfg(test)]
#[path = "supervise/terminal_history_tests.rs"]
mod terminal_history_tests;

#[cfg(all(test, unix))]
#[path = "supervise/health_event_tests.rs"]
mod health_event_tests;

#[cfg(test)]
#[path = "supervise/drain_busy_reprobe_tests.rs"]
mod drain_busy_reprobe_tests;

#[cfg(test)]
#[path = "supervise/health_evidence_tests.rs"]
mod health_evidence_tests;

#[cfg(test)]
#[path = "supervise/health_tombstone_tests.rs"]
mod health_tombstone_tests;

#[cfg(test)]
#[path = "supervise/child_env_tests.rs"]
mod child_env_tests;

#[cfg(test)]
#[path = "supervise/jitter_tests.rs"]
mod jitter_tests;

#[cfg(all(test, target_os = "linux"))]
#[path = "supervise/cgroup_placement_tests.rs"]
mod cgroup_placement_tests;

#[cfg(test)]
#[path = "supervise/spawn_subscriber_lag_tests.rs"]
mod spawn_subscriber_lag_tests;

#[cfg(test)]
#[path = "supervise/terminal_history_read_concurrency_tests.rs"]
mod terminal_history_read_concurrency_tests;

/// What a restart does with the exited process's output readers. These drive
/// the same settle functions the supervisor calls, with a reader the test
/// holds, so a reader that has not been scheduled by the bound is a controlled
/// input rather than something only a loaded machine produces.
#[cfg(test)]
#[path = "supervise/stderr_settle_tests.rs"]
mod stderr_settle_tests;

/// Containment of a module's process tree.
///
/// The behaviour these defend against is a module helper surviving its module:
/// on a real machine the Synapse embedding module's CUDA worker holds ~2.2 GB of
/// VRAM, so a leaked grandchild is a leaked GPU allocation, and a day of restarts
/// compounds it.
///
/// They run against the SUPERVISOR rather than the job-object crate because the
/// claim is about teardown: a crate-level test proves a job can reap a tree, not
/// that the daemon's drain path reaches it.
///
/// Windows-only, like the mechanism. On Unix this arm compiles out; the cgroup
/// lane there is a separate containment path with its own tests.
#[cfg(all(test, windows))]
#[path = "supervise/job_containment_tests.rs"]
mod job_containment_tests;

#[cfg(test)]
#[path = "supervise/privacy_trampoline_configuration_tests.rs"]
mod privacy_trampoline_configuration_tests;

#[cfg(test)]
#[path = "supervise/privacy_exec_boundary_tests.rs"]
mod privacy_exec_boundary_tests;

/// The daemon's real spawn path hands a subc-wire child its launch nonce on
/// descriptor 3, without an environment copy. The shell records the nonce
/// and its environment after exec so these tests observe the real handover.
#[cfg(all(test, unix))]
#[path = "supervise/launch_nonce_descriptor_tests.rs"]
mod launch_nonce_descriptor_tests;

#[cfg(all(test, target_os = "linux"))]
#[path = "supervise/cgroup_containment_tests.rs"]
mod cgroup_containment_tests;

/// A slow live-children record write on the restart path must not stop the
/// daemon from answering the replacement module's authentication handshake.
#[cfg(all(test, unix))]
#[path = "supervise/record_write_stall_tests.rs"]
mod record_write_stall_tests;

#[cfg(all(test, unix))]
#[path = "supervise/retained_status_tests.rs"]
mod retained_status_tests;
