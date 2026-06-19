use std::{
    collections::HashMap,
    error::Error,
    fmt, io,
    path::PathBuf,
    process::ExitStatus,
    sync::{Arc, Mutex},
    time::Duration,
};

use tokio::{
    process::{Child, Command},
    sync::{mpsc, oneshot},
    task::JoinHandle,
    time::{sleep, timeout, Instant},
};
use tracing::{debug, error, warn};

use crate::{registry::RegistryError, Registry};

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

/// How to launch one singleton module process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleSpec {
    pub module_id: String,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
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
        Self {
            state: ModuleState::Starting,
            enabled: true,
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
}

/// Shared daemon lookup table for supervised module handles.
#[derive(Debug, Clone, Default)]
pub struct SupervisorHandle {
    modules: Arc<Mutex<HashMap<String, SupervisedModule>>>,
}

impl SupervisorHandle {
    pub fn new() -> Self {
        Self::default()
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
        if spec.module_id.trim().is_empty() {
            return Err(SuperviseError::InvalidSpec {
                reason: "module_id must not be empty".to_string(),
            });
        }

        let runtime = SupervisorRuntimeConfig {
            restart_policy: self.restart_policy,
            drain_timeout: self.drain_timeout,
            connection_file_path: self.connection_file_path.clone(),
        };
        let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::starting()));
        let child = spawn_child(&spec, runtime.connection_file_path.as_deref())?;
        set_running(&snapshot, child.id())?;
        self.process_liveness
            .track(spec.module_id.clone(), Arc::clone(&snapshot));

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
        Ok(module)
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
    ///
    /// v1 does not yet own a per-module forwarding sink, so graceful drain is
    /// represented as a typed `Draining` state followed by a bounded wait for the
    /// child to exit on its own; if it does not, subc kills the process and waits
    /// for the registry to release the dropped socket registration.
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
    Wait {
        module_id: String,
        source: io::Error,
    },
    Kill {
        module_id: String,
        source: io::Error,
    },
    Registry(RegistryError),
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
            Self::Wait { module_id, source } => {
                write!(f, "failed to wait for module '{module_id}': {source}")
            }
            Self::Kill { module_id, source } => {
                write!(f, "failed to kill module '{module_id}': {source}")
            }
            Self::Registry(err) => write!(f, "registry error: {err}"),
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
            Self::Registry(err) => Some(err),
            Self::InvalidSpec { .. }
            | Self::RegistrationStillActive { .. }
            | Self::StatePoisoned { .. }
            | Self::CommandClosed { .. } => None,
        }
    }
}

async fn supervise_loop(
    spec: ModuleSpec,
    runtime: SupervisorRuntimeConfig,
    registry: Arc<Registry>,
    process_liveness: Arc<SupervisorProcessLiveness>,
    snapshot: SharedSnapshot,
    child: Child,
    mut commands: mpsc::Receiver<SupervisorCommand>,
) {
    let mut child = Some(child);
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

    sleep(runtime.restart_policy.backoff).await;
    process_liveness.track(spec.module_id.clone(), Arc::clone(snapshot));
    let next_child = spawn_and_mark_running(spec, runtime, snapshot)?;
    *child = Some(next_child);
    debug!(module_id = %spec.module_id, "supervised module restarted by operator request");
    Ok(())
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
        process_liveness.track(spec.module_id.clone(), Arc::clone(snapshot));
        let next_child = spawn_and_mark_running(spec, runtime, snapshot)?;
        *child = Some(next_child);
        debug!(module_id = %spec.module_id, "supervised module enabled");
        Ok(true)
    } else {
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
) -> Result<Child, SuperviseError> {
    let mut command = Command::new(&spec.program);
    command.args(&spec.args);
    if let Some(connection_file_path) = connection_file_path {
        command.arg(SUBC_ARG).arg(connection_file_path);
    }
    for (key, value) in &spec.env {
        command.env(key, value);
    }
    command.kill_on_drop(true);
    command.spawn().map_err(|source| SuperviseError::Spawn {
        program: spec.program.clone(),
        source,
    })
}

fn spawn_and_mark_running(
    spec: &ModuleSpec,
    runtime: &SupervisorRuntimeConfig,
    snapshot: &SharedSnapshot,
) -> Result<Child, SuperviseError> {
    let child = spawn_child(spec, runtime.connection_file_path.as_deref())?;
    set_running(snapshot, child.id())?;
    Ok(child)
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
    loop {
        if registry
            .get_module(module_id)
            .map_err(SuperviseError::Registry)?
            .is_none()
        {
            return Ok(());
        }

        if Instant::now() >= deadline {
            return Err(SuperviseError::RegistrationStillActive {
                module_id: module_id.to_string(),
                waited: wait,
            });
        }

        sleep(REGISTRY_RELEASE_POLL).await;
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
