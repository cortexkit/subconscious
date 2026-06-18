use std::{
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

/// Environment variable convention used by supervised modules to find subc.
///
/// subc owns the Unix listener. A supervised child process connects back to that
/// already-bound socket and registers itself with a channel-0 `HELLO`. The
/// supervisor passes the socket path to children through this environment
/// variable; callers add `(SUBC_SOCKET_ENV, socket_path)` to [`ModuleSpec::env`].
pub const SUBC_SOCKET_ENV: &str = "SUBC_SOCKET";

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
    process_alive: bool,
    restart_count: u32,
    pid: Option<u32>,
    last_exit: Option<ExitReport>,
}

impl SupervisorSnapshot {
    fn starting() -> Self {
        Self {
            state: ModuleState::Starting,
            process_alive: false,
            restart_count: 0,
            pid: None,
            last_exit: None,
        }
    }
}

type SharedSnapshot = Arc<Mutex<SupervisorSnapshot>>;

/// Process supervisor for subc-owned singleton modules.
#[derive(Debug, Clone)]
pub struct Supervisor {
    registry: Arc<Registry>,
    restart_policy: RestartPolicy,
    drain_timeout: Duration,
}

impl Supervisor {
    pub fn new(registry: Arc<Registry>, restart_policy: RestartPolicy) -> Self {
        Self {
            registry,
            restart_policy,
            drain_timeout: DEFAULT_DRAIN_TIMEOUT,
        }
    }

    pub fn with_drain_timeout(mut self, drain_timeout: Duration) -> Self {
        self.drain_timeout = drain_timeout;
        self
    }

    /// Spawn `spec.program` and start monitoring it.
    ///
    /// The child is expected to read [`SUBC_SOCKET_ENV`], connect back to subc's
    /// already-running listener, and register with channel-0 `HELLO` using
    /// `spec.module_id` as its manifest id.
    pub fn spawn(&self, spec: ModuleSpec) -> Result<SupervisedModule, SuperviseError> {
        if spec.module_id.trim().is_empty() {
            return Err(SuperviseError::InvalidSpec {
                reason: "module_id must not be empty".to_string(),
            });
        }

        let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::starting()));
        let child = spawn_child(&spec)?;
        set_running(&snapshot, child.id())?;

        let (tx, rx) = mpsc::channel(4);
        let monitor = tokio::spawn(supervise_loop(
            spec.clone(),
            self.restart_policy,
            self.drain_timeout,
            Arc::clone(&self.registry),
            Arc::clone(&snapshot),
            child,
            rx,
        ));

        Ok(SupervisedModule {
            module_id: spec.module_id,
            registry: Arc::clone(&self.registry),
            snapshot,
            commands: tx,
            monitor,
        })
    }
}

impl Default for Supervisor {
    fn default() -> Self {
        Self::new(Arc::new(Registry::default()), RestartPolicy::default())
    }
}

/// Handle to one supervised child process.
pub struct SupervisedModule {
    module_id: String,
    registry: Arc<Registry>,
    snapshot: SharedSnapshot,
    commands: mpsc::Sender<SupervisorCommand>,
    monitor: JoinHandle<()>,
}

impl fmt::Debug for SupervisedModule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SupervisedModule")
            .field("module_id", &self.module_id)
            .field("status", &self.status())
            .finish_non_exhaustive()
    }
}

impl SupervisedModule {
    pub fn module_id(&self) -> &str {
        &self.module_id
    }

    pub fn state(&self) -> Result<ModuleState, SuperviseError> {
        Ok(lock_snapshot(&self.snapshot)?.state)
    }

    pub fn status(&self) -> Result<ModuleStatus, SuperviseError> {
        let snapshot = lock_snapshot(&self.snapshot)?.clone();
        let registration_active = self
            .registry
            .get_module(&self.module_id)
            .map_err(SuperviseError::Registry)?
            .is_some();
        let live =
            snapshot.state == ModuleState::Running && snapshot.process_alive && registration_active;

        Ok(ModuleStatus {
            module_id: self.module_id.clone(),
            state: snapshot.state,
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
            | ModuleState::Draining => {}
        }

        let (reply_tx, reply_rx) = oneshot::channel();
        self.commands
            .send(SupervisorCommand::Drain { reply: reply_tx })
            .await
            .map_err(|_| SuperviseError::CommandClosed {
                module_id: self.module_id.clone(),
            })?;
        reply_rx.await.map_err(|_| SuperviseError::CommandClosed {
            module_id: self.module_id.clone(),
        })?
    }
}

impl Drop for SupervisedModule {
    fn drop(&mut self) {
        if !self.monitor.is_finished() {
            self.monitor.abort();
        }
    }
}

#[derive(Debug)]
enum SupervisorCommand {
    Drain {
        reply: oneshot::Sender<Result<(), SuperviseError>>,
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
    policy: RestartPolicy,
    drain_timeout: Duration,
    registry: Arc<Registry>,
    snapshot: SharedSnapshot,
    mut child: Child,
    mut commands: mpsc::Receiver<SupervisorCommand>,
) {
    loop {
        tokio::select! {
            wait_result = child.wait() => {
                let exit_report = match wait_result {
                    Ok(status) => classify_exit(&status),
                    Err(err) => {
                        fail_snapshot(&snapshot, Some(&spec.module_id), None);
                        error!(module_id = %spec.module_id, error = %err, "failed to wait for supervised module");
                        return;
                    }
                };

                match on_child_exit(
                    &spec,
                    policy,
                    &registry,
                    &snapshot,
                    exit_report,
                ).await {
                    NextAction::Stop => return,
                    NextAction::Restart => {
                        sleep(policy.backoff).await;
                        if let Err(err) = wait_for_registration_release(
                            &registry,
                            &spec.module_id,
                            REGISTRY_RELEASE_TIMEOUT,
                        ).await {
                            fail_snapshot(&snapshot, Some(&spec.module_id), None);
                            error!(module_id = %spec.module_id, error = %err, "registration did not release before restart");
                            return;
                        }

                        match spawn_child(&spec) {
                            Ok(next_child) => {
                                child = next_child;
                                if let Err(err) = set_running(&snapshot, child.id()) {
                                    error!(module_id = %spec.module_id, error = %err, "failed to update supervisor state after restart");
                                    return;
                                }
                                debug!(module_id = %spec.module_id, "supervised module restarted");
                            }
                            Err(err) => {
                                fail_snapshot(&snapshot, Some(&spec.module_id), None);
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
                match command {
                    SupervisorCommand::Drain { reply } => {
                        let result = drain_child(
                            &spec.module_id,
                            &registry,
                            &snapshot,
                            &mut child,
                            drain_timeout,
                        ).await;
                        let _ = reply.send(result);
                        return;
                    }
                }
            }
        }
    }
}

enum NextAction {
    Stop,
    Restart,
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
            if let Err(err) =
                wait_for_registration_release(registry, &spec.module_id, REGISTRY_RELEASE_TIMEOUT)
                    .await
            {
                warn!(module_id = %spec.module_id, error = %err, "registration still active after clean exit");
            }
            NextAction::Stop
        }
        ExitKind::Crash => {
            let mut should_restart = false;
            if let Err(err) = update_snapshot(snapshot, Some(&spec.module_id), |state| {
                state.process_alive = false;
                state.pid = None;
                state.last_exit = Some(exit_report);
                if state.restart_count >= policy.max_restarts {
                    state.state = ModuleState::Failed;
                } else {
                    state.restart_count += 1;
                    state.state = ModuleState::Restarting;
                    should_restart = true;
                }
            }) {
                error!(module_id = %spec.module_id, error = %err, "failed to record crashed module exit");
                return NextAction::Stop;
            }

            if should_restart {
                NextAction::Restart
            } else {
                if let Err(err) = wait_for_registration_release(
                    registry,
                    &spec.module_id,
                    REGISTRY_RELEASE_TIMEOUT,
                )
                .await
                {
                    warn!(module_id = %spec.module_id, error = %err, "registration still active after failed module");
                }
                NextAction::Stop
            }
        }
    }
}

fn spawn_child(spec: &ModuleSpec) -> Result<Child, SuperviseError> {
    let mut command = Command::new(&spec.program);
    command.args(&spec.args);
    for (key, value) in &spec.env {
        command.env(key, value);
    }
    command.kill_on_drop(true);
    command.spawn().map_err(|source| SuperviseError::Spawn {
        program: spec.program.clone(),
        source,
    })
}

async fn drain_child(
    module_id: &str,
    registry: &Registry,
    snapshot: &SharedSnapshot,
    child: &mut Child,
    drain_timeout: Duration,
) -> Result<(), SuperviseError> {
    update_snapshot(snapshot, Some(module_id), |state| {
        state.state = ModuleState::Draining;
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
        state.state = ModuleState::Stopped;
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
