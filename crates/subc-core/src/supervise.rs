use std::{
    collections::HashMap,
    error::Error,
    fmt, io,
    path::PathBuf,
    process::ExitStatus,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};

use tokio::{
    process::{Child, Command},
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
    time::{sleep, timeout, Instant},
};
use tracing::{debug, error, warn};

use subc_protocol::{Flags, FrameType, Priority, SUBC_LAUNCH_NONCE_ENV, SUBC_MODULE_ID_ENV};

use crate::{
    forwarding::{CloseReason, ForwardingError, ForwardingTable, GoodbyeTarget, ModuleDrainTarget},
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

/// Typed lifecycle state for a supervised module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModuleState {
    Starting,
    Running,
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
#[derive(Debug, Clone, PartialEq, Eq)]
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SupervisorSnapshot {
    state: ModuleState,
    enabled: bool,
    process_alive: bool,
    restart_count: u32,
    pid: Option<u32>,
    last_exit: Option<ExitReport>,
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
    connection_file_path: Option<PathBuf>,
    forwarding: Option<Arc<ForwardingTable>>,
    /// The shared handle, so every spawn path (initial, restart, reload) records the
    /// reserved-module launch nonce the HELLO verifier checks against.
    supervisor_handle: Option<SupervisorHandle>,
}

/// Shared daemon lookup table for supervised module handles.
///
/// Shared by clone between the [`Supervisor`] (which spawns processes) and the
/// channel-0 control handler (which verifies HELLOs), so the launch nonce the
/// supervisor records on spawn is the same one the HELLO verifier checks against.
#[derive(Debug, Clone, Default)]
pub struct SupervisorHandle {
    modules: Arc<Mutex<HashMap<String, SupervisedModule>>>,
    /// The current expected launch nonce for each reserved module_id. Set when the
    /// supervisor spawns the reserved module; checked when a HELLO claims that id. A
    /// non-reserved module never has an entry here and is never nonce-checked.
    reserved_nonces: Arc<Mutex<HashMap<String, String>>>,
}

impl SupervisorHandle {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the launch nonce expected from the next HELLO for a reserved module,
    /// replacing any prior nonce (a respawn invalidates the previous one).
    pub fn set_reserved_nonce(&self, module_id: &str, nonce: String) {
        self.reserved_nonces
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(module_id.to_string(), nonce);
    }

    /// Whether a HELLO claiming `module_id` is authorized. A module with no reserved
    /// nonce entry is not reserved and is always authorized (`true`). A reserved
    /// module is authorized only when `presented` matches the expected nonce,
    /// compared in constant time so a mismatch leaks no timing signal.
    pub fn reserved_hello_authorized(&self, module_id: &str, presented: Option<&str>) -> bool {
        let nonces = self
            .reserved_nonces
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match nonces.get(module_id) {
            None => true,
            Some(expected) => {
                presented.is_some_and(|p| constant_time_eq(expected.as_bytes(), p.as_bytes()))
            }
        }
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

    fn runtime_config(&self) -> SupervisorRuntimeConfig {
        SupervisorRuntimeConfig {
            restart_policy: self.restart_policy,
            drain_timeout: self.drain_timeout,
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

        let module = SupervisedModule {
            inner: Arc::new(SupervisedModuleInner {
                module_id: spec.module_id,
                registry: Arc::clone(&self.registry),
                snapshot,
                commands: tx,
                monitor: Mutex::new(Some(monitor)),
            }),
        };
        if let Some(supervisor_handle) = &self.supervisor_handle {
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
        })
    }

    /// Drain the module and stop monitoring it.
    pub async fn drain(&self) -> Result<(), SuperviseError> {
        self.stop().await
    }

    pub async fn stop(&self) -> Result<(), SuperviseError> {
        match self.state()? {
            ModuleState::Stopped | ModuleState::Failed => return Ok(()),
            ModuleState::Starting
            | ModuleState::Running
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

fn validate_spec(spec: &ModuleSpec) -> Result<(), SuperviseError> {
    if spec.module_id.trim().is_empty() {
        return Err(SuperviseError::InvalidSpec {
            reason: "module_id must not be empty".to_string(),
        });
    }

    Ok(())
}

async fn supervise_loop(
    spec: ModuleSpec,
    runtime: SupervisorRuntimeConfig,
    registry: Arc<Registry>,
    process_liveness: Arc<SupervisorProcessLiveness>,
    snapshot: SharedSnapshot,
    mut child: Option<Child>,
    mut commands: mpsc::Receiver<SupervisorCommand>,
) {
    loop {
        if child.is_some() {
            let active_child = child.as_mut().expect("child checked above");
            tokio::select! {
                wait_result = active_child.wait() => {
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
                            return;
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
                            return;
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
                                return;
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
                                    return;
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
                        &spec,
                        &runtime,
                        &registry,
                        &process_liveness,
                        &snapshot,
                        &mut child,
                    ).await {
                        return;
                    }
                }
            }
        } else {
            let Some(command) = commands.recv().await else {
                return;
            };
            if !handle_supervisor_command(
                command,
                &spec,
                &runtime,
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
    spec: &ModuleSpec,
    runtime: &SupervisorRuntimeConfig,
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
    let current_enabled = lock_snapshot(snapshot)?.enabled;
    if current_enabled == enabled {
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
        let next_child = spawn_and_mark_running(spec, runtime, snapshot)?;
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

    // For a reserved module, mint a fresh one-time launch nonce, record it as the
    // expected nonce for this id (replacing any prior, so a stale child cannot
    // re-register), and inject it. Only the process this spawn launches knows it, so
    // only that process can register the reserved module_id. A respawn rotates it.
    if spec.reserved {
        let nonce = generate_launch_nonce()?;
        if let Some(handle) = handle {
            handle.set_reserved_nonce(&spec.module_id, nonce.clone());
        }
        command.env(SUBC_LAUNCH_NONCE_ENV, nonce);
    }

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
                forwarding.request_connection_close(
                    released.connection_id,
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
