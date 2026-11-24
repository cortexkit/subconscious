//! Stack command implementation shared by the CLI and window revert.

pub(crate) mod host;
pub(crate) mod manager;
mod report;

pub use report::*;

use std::{
    collections::BTreeMap,
    fs::{self, File, TryLockError},
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use host::{Host, ProcessReading};
use manager::BoundedRunner;
use subc_control::{ModuleProtocol, SupervisorEntry, SupervisorHealthStatus};
use subc_daemon::{LiveChild, LiveChildrenRoster};
use subc_presence::Outcome;

use crate::setup::{
    self,
    install_lock::{open_lock_file, read_regular, write_holder, InstallLock},
    runtime::{self, CommandRunner, RuntimePaths, RuntimePlatform, SystemCommandRunner},
};

pub const STOP_TIMEOUT: Duration = Duration::from_secs(70);
pub const START_TIMEOUT: Duration = Duration::from_secs(120);
pub const STATUS_TIMEOUT: Duration = Duration::from_secs(2);
pub const STOP_DURATION: &str = "A stop holds until `ck stack start`, logout or reboot.";
pub const STOP_HELP: &str = "A stop holds until `ck stack start`, logout or reboot.\n\
Standalone stop tells modules to stop and give up long-running work.\n\
`ck setup`, `ck upgrade` (including app upgrade), window revert and fleet placement tools can also start the stack.\n\
A service manager call already in progress cannot be interrupted and can run past the timeout.";
pub const START_HELP: &str = "Start the stack and wait for the daemon and configured modules.\n\
A service manager call already in progress cannot be interrupted and can run past the timeout.";
pub const STOP_PROMPT: &str = "Allow `ck stack stop` to stop the stack? Modules will be told to stop and give up long-running work.";
pub const START_PROMPT: &str = "Allow `ck stack start` to start the stack?";

const POLL: Duration = Duration::from_millis(100);
const DAEMON_STOP_BOUND: Duration = Duration::from_secs(60);
const DAEMON_REQUEST_BOUND: Duration = Duration::from_secs(2);

/// Resolve from the same install and session paths as setup. Operations with
/// deadlines and presence prompts run synchronously; call off the async executor.
pub struct Stack {
    root: PathBuf,
    paths: RuntimePaths,
    platform: RuntimePlatform,
    config: PathBuf,
    connection: Option<PathBuf>,
}

/// Start must consult the window journal after observing the manager and before
/// looking for an answering daemon. An open window with no running revert does
/// not block start. A revert caller excludes only its own window from its check.
#[derive(Debug, PartialEq, Eq)]
pub enum WindowCheck {
    NoWindow,
    Open,
    Reverting { id: String },
    RevertInterrupted { id: String },
    Unreadable { id: String, error: String },
}

/// A revert may announce a restart so modules can keep long-running work.
/// Standalone stop and revert on installs that require operator-approved
/// configuration use ordinary shutdown without an announcement.
#[derive(Clone, Copy)]
pub enum StopIntent {
    Stop,
    Restart,
}

impl Stack {
    pub fn current(connection: Option<&Path>) -> Result<Self, String> {
        let root = setup::data_directory()?;
        let platform = RuntimePlatform::current();
        let paths =
            runtime::session_runtime_paths(platform, &root.join("bin"), &setup::user_home()?);
        Ok(Self {
            root,
            paths,
            platform,
            config: subc_daemon::daemon_config::default_config_path(),
            connection: connection.map(Path::to_path_buf),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn marker(&self) -> PathBuf {
        self.root.join("stack-stopped")
    }

    fn engine(&self) -> Result<host::SystemHost<'_>, String> {
        Ok(host::SystemHost {
            stack: self,
            runtime: tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| format!("could not prepare daemon connection: {error}"))?,
        })
    }

    pub fn stop(&self, timeout: Duration) -> StackResult {
        let clock = RealClock::new();
        let budget = Budget::new(&clock, timeout);
        match self.confirm_and_lock(STOP_PROMPT, "ck stack stop", &mut SystemPresence) {
            Ok(locked) => locked.stop_with_budget(budget, StopIntent::Stop),
            Err(error) => StackResult::refused(error),
        }
    }

    pub fn start(&self, timeout: Duration, window: impl FnOnce() -> WindowCheck) -> StackResult {
        let clock = RealClock::new();
        let budget = Budget::new(&clock, timeout);
        match self.confirm_and_lock(START_PROMPT, "ck stack start", &mut SystemPresence) {
            Ok(locked) => locked.start_with_budget(budget, window),
            Err(error) => StackResult::refused(error),
        }
    }

    pub fn status(&self, timeout: Duration) -> Result<StackStatus, String> {
        self.check_readable()?;
        let clock = RealClock::new();
        Ok(self.status_inner(
            &clock,
            timeout,
            &mut self.engine()?,
            &mut SystemCommandRunner,
        ))
    }

    /// Revert uses this after confirming its own plan. Standalone mutations
    /// always use the presence-checked entry points instead.
    pub fn lock(&self, command: &str) -> Result<LockedStack<'_>, String> {
        let installer = InstallLock::acquire_within(
            &self.root.join("installer-manifest.json"),
            command,
            Duration::ZERO,
        )?;
        let path = self.root.join("stack.lock");
        let note = self.root.join("stack.lock.holder");
        let file = open_lock_file(&path)?;
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                let holder = read_regular(&note).unwrap_or_else(|| "another stack command".into());
                return Err(format!("{} holds {}", holder.trim(), path.display()));
            }
            Err(TryLockError::Error(error)) => {
                return Err(format!("could not lock {}: {error}", path.display()))
            }
        }
        let _ = write_holder(&note, &format!("pid {}, {command}\n", std::process::id()));
        let stack = StackHold { file, note };
        self.check_binding()?;
        Ok(LockedStack {
            stack: self,
            _stack: stack,
            _installer: installer,
            command: command.into(),
        })
    }

    fn confirm_and_lock(
        &self,
        text: &str,
        command: &str,
        presence: &mut impl Presence,
    ) -> Result<LockedStack<'_>, String> {
        confirm_with(text, presence)?;
        self.lock(command)
    }

    fn check_binding(&self) -> Result<(), String> {
        let read = || -> Result<(), String> {
            let expected = runtime::desired_definition_checked(self.platform, &self.paths)?;
            let mut file = subc_os::open_regular_nofollow(&self.paths.definition)
                .map_err(|error| error.to_string())?;
            use std::io::Read as _;
            let mut definition = String::new();
            file.read_to_string(&mut definition)
                .map_err(|error| error.to_string())?;
            if definition == expected {
                Ok(())
            } else {
                Err("contents differ from the setup definition".into())
            }
        };
        read().map_err(|error| {
            format!(
                "job binding refused: definition {} for install home {}: {error}",
                self.paths.definition.display(),
                self.root.display()
            )
        })
    }

    fn check_readable(&self) -> Result<(), String> {
        fs::read_dir(&self.root).map(|_| ()).map_err(|error| {
            format!(
                "could not read install home {}: {error}",
                self.root.display()
            )
        })
    }
}

trait Presence {
    fn prompt(&mut self, text: &str) -> Outcome;
}
struct SystemPresence;
impl Presence for SystemPresence {
    fn prompt(&mut self, text: &str) -> Outcome {
        subc_presence::prompt(text, Box::new(|_| {}))
    }
}

fn confirm_with(text: &str, presence: &mut impl Presence) -> Result<(), String> {
    match presence.prompt(text) {
        Outcome::Approved => Ok(()),
        outcome => Err(format!("operator confirmation refused: {outcome:?}")),
    }
}

/// Confirm a window command without contacting the daemon.
pub fn confirm(text: &str) -> Result<(), String> {
    confirm_with(text, &mut SystemPresence)
}

struct StackHold {
    file: File,
    note: PathBuf,
}
impl Drop for StackHold {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.note);
        let _ = self.file.unlock();
    }
}

/// Both advisory locks are held and the service definition matches this install.
/// Revert keeps the locks through preflight, stop, restore, and start without
/// taking them again or prompting again.
pub struct LockedStack<'a> {
    stack: &'a Stack,
    _stack: StackHold,
    _installer: InstallLock,
    command: String,
}

impl LockedStack<'_> {
    pub fn stop(&self, timeout: Duration, intent: StopIntent) -> StackResult {
        self.stop_with_budget(Budget::new(&RealClock::new(), timeout), intent)
    }

    pub fn start(&self, timeout: Duration, window: impl FnOnce() -> WindowCheck) -> StackResult {
        self.start_with_budget(Budget::new(&RealClock::new(), timeout), window)
    }

    /// Check whether revert can reuse a running daemon. Only Linux can prove
    /// the image it is running; the caller stops and starts on other platforms.
    pub(crate) fn revert_daemon_live(&self) -> Result<bool, Box<StackResult>> {
        let platform = self.stack.platform;
        let job = manager::observe(platform, &mut SystemCommandRunner).map_err(|error| {
            Box::new(StackResult {
                exit: 1,
                manager_error: Some(error),
                ..StackResult::default()
            })
        })?;
        let mut host = self
            .stack
            .engine()
            .map_err(|error| Box::new(StackResult::refused(error)))?;
        if !job.live() && !host.connects(DAEMON_REQUEST_BOUND) {
            return Ok(false);
        }
        if platform == RuntimePlatform::Linux {
            let expected = subc_os::file_identity(&self.stack.paths.daemon);
            let proven = host
                .list(DAEMON_REQUEST_BOUND)
                .and_then(|list| list.daemon)
                .is_some_and(|daemon| {
                    let reading = ProcessReading::Alive(daemon.start_time_ms);
                    expected.is_some()
                        && host.daemon_process(daemon.pid) == reading
                        && subc_os::Process::open(daemon.pid)
                            .ok()
                            .flatten()
                            .and_then(|process| process.observe())
                            .is_some_and(|process| process.executable == expected)
                        && host.daemon_process(daemon.pid) == reading
                        && job.pid.is_none_or(|pid| pid == daemon.pid)
                });
            if !proven {
                return Err(Box::new(StackResult::refused(format!(
                    "cannot prove the running daemon image matches {}",
                    self.stack.paths.daemon.display()
                ))));
            }
        }
        Ok(true)
    }

    pub(crate) fn revert_running_health(&self, timeout: Duration) -> StackResult {
        let clock = RealClock::new();
        let budget = Budget::new(&clock, timeout);
        let mut host = match self.stack.engine() {
            Ok(host) => host,
            Err(error) => return StackResult::refused(error),
        };
        let mut result = StackResult::default();
        remove_marker(self.stack, &mut result);
        self.wait_for_health(budget, &mut host, result)
    }

    fn stop_with_budget(&self, budget: Budget<'_>, intent: StopIntent) -> StackResult {
        match self.stack.engine() {
            Ok(mut host) => self.stop_inner(budget, intent, &mut host, &mut SystemCommandRunner),
            Err(error) => StackResult::refused(error),
        }
    }

    fn start_with_budget(
        &self,
        budget: Budget<'_>,
        window: impl FnOnce() -> WindowCheck,
    ) -> StackResult {
        match self.stack.engine() {
            Ok(mut host) => self.start_inner(budget, window, &mut host, &mut SystemCommandRunner),
            Err(error) => StackResult::refused(error),
        }
    }
}

trait Clock {
    fn elapsed(&self) -> Duration;
    fn sleep(&self, duration: Duration);
}
struct RealClock(Instant);
impl RealClock {
    fn new() -> Self {
        Self(Instant::now())
    }
}
impl Clock for RealClock {
    fn elapsed(&self) -> Duration {
        self.0.elapsed()
    }
    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

#[derive(Clone, Copy)]
struct Budget<'a> {
    clock: &'a dyn Clock,
    deadline: Duration,
}
impl<'a> Budget<'a> {
    fn new(clock: &'a dyn Clock, timeout: Duration) -> Self {
        Self {
            clock,
            deadline: clock.elapsed().saturating_add(timeout),
        }
    }
    fn slice(self, timeout: Duration) -> Self {
        Self {
            deadline: self
                .deadline
                .min(self.clock.elapsed().saturating_add(timeout)),
            ..self
        }
    }
    fn remaining(self) -> Duration {
        self.deadline.saturating_sub(self.clock.elapsed())
    }
    fn overdue(self) -> bool {
        self.clock.elapsed() > self.deadline
    }
    fn tick(self) -> bool {
        let sleep = self.remaining().min(POLL);
        if sleep.is_zero() {
            return false;
        }
        self.clock.sleep(sleep);
        true
    }
}

impl LockedStack<'_> {
    fn stop_inner(
        &self,
        budget: Budget<'_>,
        intent: StopIntent,
        host: &mut impl Host,
        runner: &mut dyn CommandRunner,
    ) -> StackResult {
        let platform = self.stack.platform;
        let reading = host.list(budget.remaining().min(DAEMON_REQUEST_BOUND));
        let answered = reading.is_some();
        let daemon = reading
            .and_then(|list| list.daemon)
            .filter(|daemon| daemon.pid > 0);
        let mut result = StackResult::default();
        let mut children = Vec::new();
        snapshot_roster(host, &mut children, &mut result);
        if budget.remaining().is_zero() {
            return StackResult::refused("deadline passed before stopping the stack");
        }
        if answered
            && matches!(intent, StopIntent::Restart)
            && budget.remaining() >= DAEMON_REQUEST_BOUND
        {
            let _ = runner.announce_restart(self.stack.connection.as_deref());
        }
        let mut manager = BoundedRunner::new(runner, budget);
        if let Err(error) = manager::stop(platform, &self.stack.paths, &mut manager) {
            result.exit = if manager.mutation_started || manager.overrun.is_some() {
                1
            } else {
                2
            };
            result.manager_error = Some(error);
            if let Some(overrun) = manager.overrun {
                result.note(overrun);
            }
            return result;
        }
        let stopped_at_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let marker = format!("stopped_at_ms={stopped_at_ms}\ncommand={}\n", self.command);
        if let Err(error) = write_holder(&self.stack.marker(), &marker) {
            result.exit = 1;
            result.note(format!("could not write the stopped marker: {error}"));
        }

        let daemon_budget = budget.slice(DAEMON_STOP_BOUND);
        if let Some(daemon) = daemon {
            loop {
                let gone = match host.daemon_process(daemon.pid) {
                    ProcessReading::Gone => true,
                    ProcessReading::Alive(start) => start != daemon.start_time_ms,
                    ProcessReading::Unreadable => false,
                };
                if gone {
                    break;
                }
                if !daemon_budget.tick() {
                    result.daemon_past_bound = true;
                    result.note(format!(
                        "Daemon pid {} (start time {}) was not proven gone.",
                        daemon.pid, daemon.start_time_ms
                    ));
                    break;
                }
            }
        } else {
            loop {
                if manager::observe(platform, &mut manager).is_ok_and(|job| !job.live()) {
                    let probe = daemon_budget.slice(DAEMON_REQUEST_BOUND);
                    let mut answered = false;
                    loop {
                        if host.connects(probe.remaining()) {
                            answered = true;
                            break;
                        }
                        if !probe.tick() {
                            break;
                        }
                    }
                    if !answered {
                        break;
                    }
                }
                if !daemon_budget.tick() {
                    result.daemon_past_bound = true;
                    break;
                }
            }
        }

        snapshot_roster(host, &mut children, &mut result);
        loop {
            result.survivors.clear();
            children.retain(|(child, retained)| {
                let Some(reason) = survivor_reason(host, child, *retained) else {
                    return false;
                };
                result.survivors.push(Survivor {
                    name: child.module_id.clone(),
                    pid: child.pid,
                    start_time: child.start_time,
                    executable: child.executable,
                    state: if reason.is_some() {
                        SurvivorState::Indeterminate
                    } else {
                        SurvivorState::Alive
                    },
                    reason,
                });
                true
            });
            if children.is_empty() || !budget.tick() {
                break;
            }
        }
        match manager::observe(platform, &mut manager) {
            Ok(job) if job.live() => {
                result.exit = 1;
                result.relaunched_pid = job.pid;
                result.note(job.pid.map_or_else(
                    || "The manager reports the job live after stop (no pid reported).".into(),
                    |pid| format!("The manager reports the job live after stop (pid {pid})."),
                ));
            }
            Err(error) => {
                result.exit = 1;
                result.manager_error = Some(error);
            }
            _ => {}
        }
        if let Some(overrun) = manager.overrun {
            result.exit = 1;
            result.note(overrun);
        }
        if result.daemon_past_bound || result.roster_error.is_some() || !result.survivors.is_empty()
        {
            result.exit = 1;
        }
        result
    }

    fn start_inner(
        &self,
        budget: Budget<'_>,
        window: impl FnOnce() -> WindowCheck,
        host: &mut impl Host,
        runner: &mut dyn CommandRunner,
    ) -> StackResult {
        let platform = self.stack.platform;
        let mut manager = BoundedRunner::new(runner, budget);
        let observed = manager::observe(platform, &mut manager);
        if let Some(overrun) = manager.overrun.take() {
            return StackResult {
                exit: 1,
                manager_error: observed.err(),
                message: Some(overrun),
                ..StackResult::default()
            };
        }
        let job = match observed {
            Ok(job) => job,
            Err(error) => {
                return StackResult {
                    exit: 2,
                    manager_error: Some(error),
                    ..StackResult::default()
                }
            }
        };
        match window() {
            WindowCheck::NoWindow | WindowCheck::Open => {}
            WindowCheck::Reverting { id } => {
                return StackResult::refused(format!("window {id} is reverting"))
            }
            WindowCheck::RevertInterrupted { id } => {
                return StackResult::refused(format!("window {id} has an interrupted revert"))
            }
            WindowCheck::Unreadable { id, error } => {
                return StackResult::refused(format!("window {id} is unreadable: {error}"))
            }
        }
        if job.live() {
            let mut result = StackResult::refused(job.pid.map_or_else(
                || "the service manager reports the daemon live".into(),
                |pid| format!("the service manager reports the daemon live (pid {pid})"),
            ));
            remove_marker(self.stack, &mut result);
            return result;
        }
        if host.connects(budget.remaining().min(DAEMON_REQUEST_BOUND)) {
            return StackResult::refused("the daemon is already answering connections");
        }
        if budget.remaining().is_zero() {
            return StackResult::refused("deadline passed before starting the stack");
        }
        let started = manager::start(platform, &self.stack.paths, job, &mut manager);
        if let Err(error) = started {
            let mut result = StackResult {
                exit: 1,
                manager_error: Some(error),
                ..StackResult::default()
            };
            if let Some(overrun) = manager.overrun {
                result.note(overrun);
            }
            return result;
        }
        let mut result = StackResult::default();
        remove_marker(self.stack, &mut result);
        if let Some(overrun) = manager.overrun {
            result.exit = 1;
            result.note(overrun);
            return result;
        }

        self.wait_for_health(budget, host, result)
    }

    fn wait_for_health(
        &self,
        budget: Budget<'_>,
        host: &mut impl Host,
        mut result: StackResult,
    ) -> StackResult {
        let config = host.configured();
        let mut read = false;
        let mut details = Vec::new();
        let mut degraded_notes: Option<BTreeMap<String, String>> = None;
        loop {
            if let Some(list) = host.list(budget.remaining().min(DAEMON_REQUEST_BOUND)) {
                read = true;
                let (roster, error) = module_roster(host);
                result.roster_error = error;
                result.unhealthy.clear();
                result.not_health_checked.clear();
                result.degraded.clear();
                details.clear();
                for module in list
                    .modules
                    .iter()
                    .filter(|module| module.configured_enabled.unwrap_or(module.enabled))
                {
                    let (state, detail) =
                        combined_state(module, &roster, result.roster_error.as_ref());
                    if let Some(detail) = detail {
                        details.push(format!("{}: {detail}", module.module_id));
                    }
                    match health_class(module, state, config.as_ref().ok()) {
                        HealthClass::Healthy => {}
                        HealthClass::Unhealthy => result.unhealthy.push(module.module_id.clone()),
                        HealthClass::NotChecked => {
                            result.not_health_checked.push(module.module_id.clone())
                        }
                        HealthClass::Degraded => result.degraded.push(DegradedModule {
                            name: module.module_id.clone(),
                            detail: None,
                        }),
                    }
                }
                if result.unhealthy.is_empty()
                    && result.not_health_checked.is_empty()
                    && result.degraded.is_empty()
                {
                    return result;
                }
                let limit = budget.remaining().min(DAEMON_REQUEST_BOUND);
                if !result.degraded.is_empty() && degraded_notes.is_none() && !limit.is_zero() {
                    // The list determines the health class but does not carry
                    // the module's note. A separate cached read can supply prose,
                    // never a replacement verdict.
                    degraded_notes = Some(
                        host.cached_health(limit)
                            .into_iter()
                            .filter(|entry| entry.status == SupervisorHealthStatus::Degraded)
                            .filter_map(|entry| Some((entry.module_id, entry.detail?)))
                            .collect(),
                    );
                }
            }
            if !budget.tick() {
                break;
            }
        }
        if !read {
            result.daemon_unreachable = true;
        }
        if result.daemon_unreachable || !result.unhealthy.is_empty() {
            result.exit = 1;
        }
        if let Some(mut notes) = degraded_notes {
            for module in &mut result.degraded {
                module.detail = notes.remove(&module.name);
            }
        }
        for detail in details {
            result.note(detail);
        }
        if let Err(error) = config {
            result.note(format!(
                "could not read module health configuration: {error}"
            ));
        }
        result
    }
}

fn remove_marker(stack: &Stack, result: &mut StackResult) {
    match fs::remove_file(stack.marker()) {
        Ok(()) => result.marker_removed = true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            if result.exit == 0 {
                result.exit = 1;
            }
            result.note(format!("could not remove the stopped marker: {error}"));
        }
    }
}

fn snapshot_roster(
    host: &mut impl Host,
    children: &mut Vec<(LiveChild, bool)>,
    result: &mut StackResult,
) {
    match host.roster() {
        Ok(Some(roster)) => {
            for child in roster.children {
                let retained = roster.retained_reasons.contains_key(&child.pid);
                if let Some((_, earlier)) =
                    children.iter_mut().find(|(snapshot, _)| *snapshot == child)
                {
                    *earlier |= retained;
                } else {
                    children.push((child, retained));
                }
            }
        }
        Ok(None) => {}
        Err(error) => {
            let error = RosterError::from(error);
            if let Some(earlier) = &result.roster_error {
                result.note(format!("Earlier roster error: {}", earlier.message));
            }
            result.roster_error = Some(error);
        }
    }
}

fn survivor_reason(
    host: &mut impl Host,
    child: &LiveChild,
    retained: bool,
) -> Option<Option<SurvivorReason>> {
    let process = match host::roster_liveness(host, child) {
        ProcessReading::Gone => None,
        ProcessReading::Unreadable => Some(Some(SurvivorReason::IdentityUnreadable)),
        ProcessReading::Alive(_) => Some(host.executable(child)),
    };
    if let Some(Some(reason)) = process {
        return Some(Some(reason));
    }
    if (retained || child.container.is_some() || child.cgroup_name.is_some())
        && !host.container_empty(child)
    {
        return Some(Some(SurvivorReason::ContainerUnproven));
    }
    process
}

fn module_roster(host: &mut impl Host) -> (BTreeMap<String, ModuleState>, Option<RosterError>) {
    let roster = match host.roster() {
        Ok(Some(roster)) => roster,
        Ok(None) => return (BTreeMap::new(), None),
        Err(error) => return (BTreeMap::new(), Some(error.into())),
    };
    (roster_states(host, roster), None)
}

fn roster_states(
    host: &mut impl Host,
    roster: LiveChildrenRoster,
) -> BTreeMap<String, ModuleState> {
    let mut states = BTreeMap::new();
    for child in roster.children {
        let state = match host::roster_liveness(host, &child) {
            ProcessReading::Alive(_) => ModuleState::Running,
            ProcessReading::Gone => ModuleState::Stopped,
            ProcessReading::Unreadable => ModuleState::Indeterminate,
        };
        let existing = states.entry(child.module_id).or_insert(state);
        if matches!(
            (*existing, state),
            (_, ModuleState::Indeterminate) | (ModuleState::Stopped, ModuleState::Running)
        ) {
            *existing = state;
        }
    }
    states
}

fn combined_state(
    module: &SupervisorEntry,
    roster: &BTreeMap<String, ModuleState>,
    error: Option<&RosterError>,
) -> (ModuleState, Option<String>) {
    // supervisor.list has no module PID, and its live bit means serving, not
    // process existence. Confirm process identity in the roster read after it.
    let state = roster
        .get(&module.module_id)
        .copied()
        .unwrap_or(ModuleState::Stopped);
    let daemon = if module.live {
        "serving"
    } else if module.tree_unavailable.as_deref() == Some("not_running") {
        "not running"
    } else {
        "not serving"
    };
    if error.is_some()
        || state == ModuleState::Indeterminate
        || (module.live && state != ModuleState::Running)
        || (daemon == "not running" && state == ModuleState::Running)
    {
        let roster = if error.is_some() {
            "unreadable"
        } else {
            match state {
                ModuleState::Running => "running",
                ModuleState::Stopped => "no live entry",
                ModuleState::Indeterminate => "identity unreadable",
            }
        };
        return (
            ModuleState::Indeterminate,
            Some(format!(
                "inconclusive: supervisor.list reports {daemon}; roster reports {roster}"
            )),
        );
    }
    (state, None)
}

#[derive(PartialEq, Eq)]
enum HealthClass {
    Healthy,
    NotChecked,
    Degraded,
    Unhealthy,
}

fn health_class(
    module: &SupervisorEntry,
    state: ModuleState,
    config: Option<&BTreeMap<String, bool>>,
) -> HealthClass {
    if state != ModuleState::Running {
        return HealthClass::Unhealthy;
    }
    if module.last_probe_ms.is_some() {
        return match module.health {
            SupervisorHealthStatus::Ok => HealthClass::Healthy,
            SupervisorHealthStatus::Degraded => HealthClass::Degraded,
            SupervisorHealthStatus::Failing
            | SupervisorHealthStatus::Unresponsive
            | SupervisorHealthStatus::Unknown => HealthClass::Unhealthy,
        };
    }
    if module.protocol != ModuleProtocol::None {
        return HealthClass::NotChecked;
    }
    if config.is_some_and(|config| !config.get(&module.module_id).copied().unwrap_or(false)) {
        HealthClass::Healthy
    } else {
        HealthClass::Unhealthy
    }
}

impl Stack {
    fn status_inner(
        &self,
        clock: &dyn Clock,
        timeout: Duration,
        host: &mut impl Host,
        runner: &mut dyn CommandRunner,
    ) -> StackStatus {
        // The status timeout governs the daemon wait, not read-only manager observation.
        let observed = manager::observe(
            self.platform,
            &mut BoundedRunner::new(runner, Budget::new(clock, Duration::MAX)),
        );
        let budget = Budget::new(clock, timeout);
        let manager = observed
            .as_ref()
            .map_or(ManagerState::Unknown, |job| job.state);
        let marker = match fs::symlink_metadata(self.marker()) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => MarkerState::Absent,
            Err(_) => MarkerState::Unknown,
            Ok(_) => match manager {
                ManagerState::Unknown => MarkerState::Unknown,
                ManagerState::Running => MarkerState::Stale,
                _ => MarkerState::Present,
            },
        };
        let mut report = StackStatus::new(manager, marker);
        report.daemon_pid = observed.as_ref().ok().and_then(|job| job.pid);
        report.manager_error = observed.err();
        let list = loop {
            let list = host.list(budget.remaining());
            if list.is_some() || !budget.tick() {
                break list;
            }
        };
        let (mut roster, error) = module_roster(host);
        report.roster_error = error;
        if let Some(list) = list {
            report.source = Source::Daemon;
            if let Some(daemon) = list.daemon.filter(|daemon| daemon.pid > 0) {
                report.daemon_pid = Some(daemon.pid);
                report.daemon_start_time = Some(daemon.start_time_ms);
                report.daemon_version = daemon.version;
            }
            for module in list.modules {
                let (state, detail) =
                    combined_state(&module, &roster, report.roster_error.as_ref());
                let health = if state != ModuleState::Running {
                    Health::Unknown
                } else {
                    match module.health {
                        SupervisorHealthStatus::Ok => Health::Healthy,
                        SupervisorHealthStatus::Unknown => Health::Unknown,
                        _ => Health::Unhealthy,
                    }
                };
                report.modules.push(ModuleStatus {
                    name: module.module_id,
                    state,
                    health,
                    detail,
                });
            }
        } else {
            if let Ok(config) = host.configured() {
                for name in config.into_keys() {
                    roster.entry(name).or_insert(ModuleState::Stopped);
                }
            }
            for (name, state) in roster {
                let state = if report.roster_error.is_some() {
                    ModuleState::Indeterminate
                } else {
                    state
                };
                report.modules.push(ModuleStatus {
                    name,
                    state,
                    health: Health::Unknown,
                    detail: None,
                });
            }
        }
        report
    }
}

#[cfg(all(test, feature = "test-support"))]
mod tests;
