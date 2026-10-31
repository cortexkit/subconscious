//! `app.open`: start a known operator app from its installed location.
//!
//! The decision order is fixed and checked top to bottom: `app_open_not_used`
//! on macOS (which never launches through the daemon) and on platforms without
//! a launcher, `app_open_disabled` when the platform switch is off,
//! `app_unknown` for an app the compiled table does not name,
//! `already_running` while a launch entry is live, `app_not_installed` when
//! the installed file is missing, and only then verification and launch.
//!
//! Launch entries live in one table behind one lock. The lock is a leaf: take
//! no other lock while holding it. An entry holds the daemon-generated incarnation id, the launched
//! pid, and the platform liveness handle: a pidfd on Linux, the owned process
//! handle on Windows. Liveness is read from that handle, never by reopening
//! the numeric pid, so a reused pid confers nothing. On Windows the entry is
//! published before the image is resumed, so a child that connects at once
//! is matched. On Linux the entry is published after spawn and tracking, so
//! the image may run first, which is safe because Linux connections are
//! never stamped. Entries retire when no longer live. The table is memory-only: a daemon or installed-service
//! restart drops it, and a relaunched daemon never mistakes an older process
//! for its own launch. Connections from such a process are plain `direct`.
//!
//! Starting the app grants nothing by itself: a shortcut still needs a
//! stamped connection. (A Windows stamp records the live entry's incarnation
//! at connection time; nothing revalidates it yet.) So `app.open` is classified
//! `None` in the privileged-operation classifier and neither prompts in
//! `enforce` mode nor logs in `log` mode.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex, MutexGuard,
    },
};

/// Filesystem paths only exist on platforms with a launcher; the decision and
/// table code below names locations through `PathBuf` alone.
#[cfg(any(windows, target_os = "linux"))]
use std::path::Path;

use semver::Version;

use crate::{
    control::ControlHandler,
    operator_apps::{AppOpenPlatform, OperatorAppPolicy},
    router::{RouteCtx, RouterError},
    Frame,
};

/// One live-or-recently-live launch slot. At most one entry per app exists.
pub(crate) struct LaunchEntry {
    incarnation: u64,
    pid: u32,
    start_time: Option<u64>,
    process: subc_os::Process,
    child: Option<std::process::Child>,
}

impl LaunchEntry {
    /// Whether the retained handle still reports this exact process alive.
    /// The stored start time must match the current observation, so a pid the
    /// kernel has since handed to another process reads as dead even on
    /// platforms whose handle cannot pin the incarnation by itself.
    fn is_live(&self) -> bool {
        self.process
            .observe()
            .is_some_and(|observed| Some(observed.start_time) == self.start_time)
    }

    /// Release a retired entry's handles without signalling anything:
    /// dropping the entry never stops a live child, and an exited direct
    /// child is reaped so it never lingers as a zombie. The daemon never signals here: liveness-triggered retirement
    /// follows an observed exit, and `retire` or a `publish` replacement only
    /// drops the handles and reaps what already exited.
    fn reap(self) {
        if let Some(mut child) = self.child {
            // Our own direct child, if it already exited: release its pid.
            // A scope child is not ours; the error is ignored.
            let _ = child.try_wait();
        }
        #[cfg(target_os = "linux")]
        {
            // Our own zombie keeps its numeric pid until reaped. The pidfd
            // already reported it dead, so this wait only releases the number
            // and never blocks: a non-child errors, ignored the same way.
            let _ = rustix::process::waitpid(
                rustix::process::Pid::from_raw(self.pid as i32),
                rustix::process::WaitOptions::NOHANG,
            );
        }
    }

    /// Stop a launch that failed after the child existed but before it could
    /// run: kill the process the daemon still holds, then release it. Used
    /// only on paths that refuse the launch, never on a running app.
    #[cfg(windows)]
    fn stop_rejected(mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            while matches!(child.try_wait(), Ok(None)) {
                std::thread::yield_now();
            }
        }
        // Dropping `process` closes the retained handle; a suspended child
        // killed above is already gone, and killing an exited child is a
        // harmless error.
    }
}

/// The pid and incarnation of a live launch entry, for replies and stamps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LiveLaunch {
    pub incarnation: u64,
    pub pid: u32,
}

/// One launch table for the daemon, behind one leaf lock. No other lock is
/// taken while it is held.
pub(crate) struct LaunchTable {
    inner: Mutex<HashMap<String, LaunchEntry>>,
    next_incarnation: AtomicU64,
}

/// The methods run per platform and in tests: `publish` and friends launch
/// on Windows and Linux, while other builds only exercise them under test.
/// (Check builds without test code would otherwise call the idle ones dead.)
#[allow(dead_code)]
impl LaunchTable {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            next_incarnation: AtomicU64::new(1),
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, LaunchEntry>> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn retire_locked(inner: &mut HashMap<String, LaunchEntry>, app: &str) {
        if let Some(entry) = inner.remove(app) {
            entry.reap();
        }
    }

    /// The live entry for `app`, retiring it first when its handle reports
    /// exit. A dead entry never matches again, even through an inherited
    /// socket or pipe handle, and even after `app.open` launches a
    /// replacement with a new incarnation.
    pub fn live_entry(&self, app: &str) -> Option<LiveLaunch> {
        let mut inner = self.lock();
        let entry = inner.get(app)?;
        if !entry.is_live() {
            Self::retire_locked(&mut inner, app);
            return None;
        }
        Some(LiveLaunch {
            incarnation: entry.incarnation,
            pid: entry.pid,
        })
    }

    /// The incarnation whose entry is live for `pid`: a pipe client is
    /// eligible only when its pid is that of a live launch entry.
    pub fn match_live_pid(&self, app: &str, pid: u32) -> Option<u64> {
        let live = self.live_entry(app)?;
        (live.pid == pid).then_some(live.incarnation)
    }

    /// Whether `incarnation` is still the live entry for `app`. A later entry
    /// for the same app never revalidates an older stamp.
    pub fn is_live_incarnation(&self, app: &str, incarnation: u64) -> bool {
        self.with_live_incarnation(app, incarnation, || ())
            .is_some()
    }

    /// Keep retirement and replacement excluded until the caller commits its
    /// decision. The caller already holds every other lock it needs.
    pub(crate) fn with_live_incarnation<T>(
        &self,
        app: &str,
        incarnation: u64,
        commit: impl FnOnce() -> T,
    ) -> Option<T> {
        let mut inner = self.lock();
        let entry = inner.get(app)?;
        if !entry.is_live() {
            Self::retire_locked(&mut inner, app);
            return None;
        }
        (entry.incarnation == incarnation).then(commit)
    }

    /// Publish a launch entry, replacing any entry for the app. Callers retire
    /// dead entries through the liveness check first, so a replaced entry was
    /// already retired and its handles are closed here.
    pub fn publish(
        &self,
        app: &str,
        pid: u32,
        process: subc_os::Process,
        child: Option<std::process::Child>,
    ) -> u64 {
        let incarnation = self.next_incarnation.fetch_add(1, Ordering::Relaxed);
        let start_time = process.observe().map(|observed| observed.start_time);
        let mut inner = self.lock();
        Self::retire_locked(&mut inner, app);
        inner.insert(
            app.to_owned(),
            LaunchEntry {
                incarnation,
                pid,
                start_time,
                process,
                child,
            },
        );
        incarnation
    }

    /// Forget the entry without signalling the process: the launched app
    /// survives a daemon or installed-service restart by design.
    pub fn retire(&self, app: &str) {
        Self::retire_locked(&mut self.lock(), app);
    }

    /// Take the entry for a launch that must be stopped after publishing,
    /// such as a child that cannot be resumed. The caller stops the returned
    /// entry; nothing else in the table refers to the pid afterwards.
    #[cfg(windows)]
    pub fn take(&self, app: &str) -> Option<LaunchEntry> {
        self.lock().remove(app)
    }
}

/// Why `app.open` refused to launch.
#[derive(Debug)]
pub(crate) struct AppOpenRefusal {
    pub code: &'static str,
    pub message: String,
}

impl AppOpenRefusal {
    fn not_used(message: impl Into<String>) -> Self {
        Self {
            code: subc_control::APP_OPEN_NOT_USED,
            message: message.into(),
        }
    }

    fn disabled(message: impl Into<String>) -> Self {
        Self {
            code: subc_control::APP_OPEN_DISABLED,
            message: message.into(),
        }
    }

    fn unknown(message: impl Into<String>) -> Self {
        Self {
            code: subc_control::APP_UNKNOWN,
            message: message.into(),
        }
    }

    pub fn not_installed(message: impl Into<String>) -> Self {
        Self {
            code: subc_control::APP_NOT_INSTALLED,
            message: message.into(),
        }
    }

    /// Refusal when the installed binary fails the daemon's checks. Only the
    /// Windows launch path verifies, and only with an injected test check:
    /// production carries no verifier and fails closed here. Where the daemon
    /// verifies, the test hook receives the expected publisher, the daemon
    /// enforces the floor itself on the returned version, and the held
    /// handle pins the file throughout.
    #[cfg(windows)]
    pub fn verification_failed(message: impl Into<String>) -> Self {
        Self {
            code: subc_control::APP_VERIFICATION_FAILED,
            message: message.into(),
        }
    }
}

/// What the ordered checks decided, before any verification or process starts.
pub(crate) enum AppOpenDecision {
    Refuse(AppOpenRefusal),
    AlreadyRunning { pid: u32 },
    Launch { path: PathBuf },
}

/// The fixed order: `app_open_not_used` (macOS, or a platform without a
/// launcher), `app_open_disabled` (platform switch off), `app_unknown`,
/// retire-or-`already_running`, `app_not_installed`, then launch.
pub(crate) fn decide_app_open(
    policy: &OperatorAppPolicy,
    table: &LaunchTable,
    platform: AppOpenPlatform,
    app: &str,
) -> AppOpenDecision {
    if matches!(platform, AppOpenPlatform::MacOS | AppOpenPlatform::Other) {
        return AppOpenDecision::Refuse(AppOpenRefusal::not_used(
            "this platform never launches apps through the daemon",
        ));
    }
    if !policy.switch_enabled(platform) {
        return AppOpenDecision::Refuse(AppOpenRefusal::disabled(
            "app.open is switched off on this platform",
        ));
    }
    if policy.app_id().is_none_or(|id| id != app) {
        return AppOpenDecision::Refuse(AppOpenRefusal::unknown(format!(
            "no compiled app named {app:?} on this platform"
        )));
    }
    if let Some(live) = table.live_entry(app) {
        // A live entry answers `already_running` even when the installed file
        // has since vanished: the running process is what the caller asked
        // about, and the next call after its exit relaunches and re-checks
        // the install. The daemon starts nothing and relays nothing here.
        return AppOpenDecision::AlreadyRunning { pid: live.pid };
    }
    let Some(path) = policy.installed_path(platform) else {
        return AppOpenDecision::Refuse(AppOpenRefusal::not_installed(format!(
            "no install location is compiled in for {app:?}"
        )));
    };
    if !path.exists() {
        return AppOpenDecision::Refuse(AppOpenRefusal::not_installed(format!(
            "{} is not installed at {}",
            app,
            path.display()
        )));
    }
    AppOpenDecision::Launch { path }
}
impl ControlHandler {
    /// Serve one `app.open` call: run the ordered checks, then verify and
    /// launch, or report the already-running pid. Like `operator.confirm`
    /// this is probed before the module branch, so any connection can ask;
    /// the launch itself grants nothing, and a shortcut still needs a stamped
    /// connection.
    pub(crate) async fn handle_app_open(
        &self,
        _ctx: &RouteCtx,
        frame: Frame,
    ) -> Result<Vec<Frame>, RouterError> {
        // One launch at a time per handler: the liveness check in the
        // decision and the later publish must not interleave across
        // concurrent calls, or two calls could both launch and the second
        // publish would orphan the first process. The guard covers the
        // whole call; `app.open` is rare, so refusals sharing it cost
        // nothing.
        let _launch_guard = self.app_open_launch_lock.lock().await;
        let request = match serde_json::from_slice::<subc_control::AppOpenRequest>(&frame.body) {
            Ok(request) => request,
            Err(_) => {
                return Ok(vec![crate::control::control_error_frame(
                    &frame,
                    "invalid_control_body",
                    "malformed app.open body",
                )?]);
            }
        };
        let app = request.app.clone();
        let platform = AppOpenPlatform::current();
        #[cfg(test)]
        let platform = self.app_open_test_platform.unwrap_or(platform);
        match decide_app_open(
            &self.operator_app_policy,
            &self.launch_table,
            platform,
            &app,
        ) {
            AppOpenDecision::Refuse(refusal) => Ok(vec![crate::control::control_error_frame(
                &frame,
                refusal.code,
                refusal.message,
            )?]),
            AppOpenDecision::AlreadyRunning { pid } => {
                // `already_running` starts nothing and relays nothing: on
                // Windows it calls the optional attention hook (production
                // only logs), and nothing at all happens on Linux.
                #[cfg(windows)]
                self.flash_running_app(pid);
                Ok(vec![crate::control::control_response_body_frame(
                    &frame,
                    &subc_control::AppOpenReply::already_running(pid),
                    "app.open",
                )?])
            }
            AppOpenDecision::Launch { path } => {
                let outcome = self.launch_app(&app, &path).await;
                match outcome {
                    Ok(pid) => Ok(vec![crate::control::control_response_body_frame(
                        &frame,
                        &subc_control::AppOpenReply::launched(pid),
                        "app.open",
                    )?]),
                    Err(refusal) => Ok(vec![crate::control::control_error_frame(
                        &frame,
                        refusal.code,
                        refusal.message,
                    )?]),
                }
            }
        }
    }

    async fn launch_app(&self, app: &str, path: &std::path::Path) -> Result<u32, AppOpenRefusal> {
        #[cfg(test)]
        if let Some(launch) = self.app_open_test_launch {
            return launch();
        }
        #[cfg(windows)]
        {
            self.windows_launch_app(app, path).await
        }
        #[cfg(target_os = "linux")]
        {
            self.linux_launch_app(app, path).await
        }
        #[cfg(not(any(windows, target_os = "linux")))]
        {
            // The ordered platform check refuses before launching on these hosts.
            let _ = (app, path);
            Err(AppOpenRefusal::not_used(
                "this platform has no app launcher",
            ))
        }
    }
}

/// How a Linux launch is started. With `systemd-run` and a user bus the app
/// runs in its own transient scope, so the daemon unit's `KillMode=mixed`
/// cleanup never reaches it. Otherwise the child only gets its own process
/// group: that separates signals but does not move it out of a service
/// cgroup, so service cleanup can still stop it. Either way the daemon holds
/// the pidfd for its launch entry.
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LinuxLaunchStyle {
    Scope,
    Direct,
}

/// Pick the launch style from what the machine offers: a `systemd-run`
/// binary plus a reachable user bus means a user manager is present.
#[cfg(target_os = "linux")]
pub(crate) fn select_linux_launch_style(
    systemd_run_present: bool,
    user_bus_present: bool,
) -> LinuxLaunchStyle {
    if systemd_run_present && user_bus_present {
        LinuxLaunchStyle::Scope
    } else {
        LinuxLaunchStyle::Direct
    }
}

/// Whether the current user has a systemd user manager: the runtime
/// directory's bus socket exists. The system per-user path is checked too, so
/// a detection made before `XDG_RUNTIME_DIR` is set still finds the manager.
#[cfg(target_os = "linux")]
fn user_bus_present() -> bool {
    use std::os::unix::fs::FileTypeExt as _;

    let uid = rustix::process::getuid().as_raw();
    [
        std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from),
        Some(PathBuf::from(format!("/run/user/{uid}"))),
    ]
    .into_iter()
    .flatten()
    .any(|dir| {
        // The socket's presence is the manager's presence: no manager leaves
        // no bus behind, and a missing path fails the check harmlessly.
        std::fs::symlink_metadata(dir.join("bus"))
            .is_ok_and(|metadata| metadata.file_type().is_socket())
    })
}

/// Whether `name` names an executable regular file on `PATH`.
#[cfg(target_os = "linux")]
fn find_on_path(name: &str) -> bool {
    use std::os::unix::fs::PermissionsExt as _;

    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        let candidate = dir.join(name);
        candidate.is_file()
            && candidate
                .metadata()
                .is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0)
    })
}

/// The scope wrapper: `systemd-run` starts the scope and exits, so the app's
/// pid is read from the wrapper's stdout. The wrapper echoes its own pid and
/// execs the app, which keeps the pid; what the daemon opens the pidfd on is
/// the app itself.
#[cfg(target_os = "linux")]
pub(crate) fn systemd_run_command(path: &Path) -> tokio::process::Command {
    let mut command = tokio::process::Command::new("systemd-run");
    command.args([
        "--user",
        "--scope",
        "--",
        "/bin/sh",
        "-c",
        "echo $$; exec \"$0\" \"$@\"",
        &path.display().to_string(),
    ]);
    command
}

/// A clean launch environment still carries everything except the loader
/// overrides: dropping `LD_PRELOAD`, `LD_LIBRARY_PATH` and `LD_AUDIT` keeps
/// a same-user process from injecting code into the app, while keeping `HOME`,
/// `DISPLAY`, `XDG_RUNTIME_DIR` and the rest the app needs to run. A fully
/// cleared environment would break the app the same way it once broke
/// supervised modules (which lost `HOME` and `XDG_RUNTIME_DIR` under an
/// `env_clear` until the daemon stopped clearing).
#[cfg(target_os = "linux")]
fn scrub_app_environment(command: &mut std::process::Command) {
    for key in ["LD_PRELOAD", "LD_LIBRARY_PATH", "LD_AUDIT"] {
        command.env_remove(key);
    }
}

#[cfg(target_os = "linux")]
impl ControlHandler {
    /// Launch the installed Linux app and record its entry. Under a user
    /// manager the app runs in its own transient scope; without one it gets
    /// its own process group. The entry is recorded but never stamped: no
    /// connection from a launched process is ever eligible.
    async fn linux_launch_app(&self, app: &str, path: &Path) -> Result<u32, AppOpenRefusal> {
        let style = if self.operator_app_policy.linux_force_direct {
            LinuxLaunchStyle::Direct
        } else {
            select_linux_launch_style(find_on_path("systemd-run"), user_bus_present())
        };
        match style {
            LinuxLaunchStyle::Direct => self.linux_launch_direct(app, path, &[]),
            LinuxLaunchStyle::Scope => self.linux_launch_scoped(app, path).await,
        }
    }

    /// Start the app as our own child in its own process group, with a clean
    /// environment, and hold its pidfd. The child is never waited on until
    /// its entry retires, so its pid cannot be reused while tracked.
    ///
    /// `extra_env` exists for tests, which poison the loader overrides on the
    /// command to prove the scrub removes them. Production always passes an
    /// empty slice: the command otherwise inherits the daemon environment.
    fn linux_launch_direct(
        &self,
        app: &str,
        path: &Path,
        extra_env: &[(&str, &str)],
    ) -> Result<u32, AppOpenRefusal> {
        use std::os::unix::process::CommandExt as _;

        let mut command = std::process::Command::new(path);
        // Test-only poison rides the command, never the process environment,
        // so parallel tests spawning children cannot observe it.
        for (key, value) in extra_env {
            command.env(key, value);
        }
        // Our own process group, so nothing signaling the daemon's group
        // reaches the app. This does not move the app out of a service
        // cgroup, so a service manager's cleanup can still stop it (a session
        // would need a post-fork `setsid` no safe code here can run). The
        // group plus the pidfd are the containment.
        command.process_group(0);
        scrub_app_environment(&mut command);
        command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let mut child = command.spawn().map_err(|error| {
            AppOpenRefusal::not_installed(format!(
                "installed file {} could not be started: {error}",
                path.display()
            ))
        })?;
        let pid = child.id();
        let process = subc_os::Process::open(pid).map_err(|error| {
            AppOpenRefusal::not_installed(format!(
                "launched process {pid} cannot be tracked: {error}"
            ))
        })?;
        let Some(process) = process else {
            // Exited before it could be tracked: stop anything left and
            // refuse, rather than report a launch with no entry behind it.
            let _ = child.kill();
            while matches!(child.try_wait(), Ok(None)) {
                std::thread::yield_now();
            }
            return Err(AppOpenRefusal::not_installed(format!(
                "installed file {} exited before it could be tracked",
                path.display()
            )));
        };
        self.launch_table.publish(app, pid, process, Some(child));
        Ok(pid)
    }

    /// Start the app in its own transient user scope and open a pidfd on the
    /// reported pid. A scope child is not ours, so no `Child` is retained and
    /// the user manager reaps it; the pidfd still reports its exit.
    async fn linux_launch_scoped(&self, app: &str, path: &Path) -> Result<u32, AppOpenRefusal> {
        use tokio::io::AsyncBufReadExt as _;

        let mut command = systemd_run_command(path);
        scrub_app_environment_std(&mut command);
        command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|error| {
            AppOpenRefusal::not_installed(format!("transient scope could not start: {error}"))
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            AppOpenRefusal::not_installed("transient scope reported no pid".to_string())
        })?;
        let mut lines = tokio::io::BufReader::new(stdout).lines();
        let pid_line = tokio::time::timeout(std::time::Duration::from_secs(10), lines.next_line())
            .await
            .map_err(|_| {
                AppOpenRefusal::not_installed(
                    "transient scope did not report its pid in time".to_string(),
                )
            })?
            .map_err(|error| {
                AppOpenRefusal::not_installed(format!(
                    "transient scope pid could not be read: {error}"
                ))
            })?
            .ok_or_else(|| {
                AppOpenRefusal::not_installed(
                    "transient scope exited without reporting its pid".to_string(),
                )
            })?;
        let pid: u32 = pid_line.trim().parse().map_err(|_| {
            AppOpenRefusal::not_installed(format!(
                "transient scope reported an unparsable pid: {pid_line:?}"
            ))
        })?;
        // Reap the scope starter: it exits once the scope runs. A timeout here
        // kills the starter, never the scope, and refuses a launch whose
        // bookkeeping did not finish.
        let status = tokio::time::timeout(std::time::Duration::from_secs(10), child.wait())
            .await
            .map_err(|_| {
                AppOpenRefusal::not_installed(
                    "transient scope starter did not exit in time".to_string(),
                )
            })?
            .map_err(|error| {
                AppOpenRefusal::not_installed(format!(
                    "transient scope starter could not be reaped: {error}"
                ))
            })?;
        if !status.success() {
            return Err(AppOpenRefusal::not_installed(format!(
                "transient scope starter exited with {status}"
            )));
        }
        let process = subc_os::Process::open(pid).map_err(|error| {
            AppOpenRefusal::not_installed(format!(
                "scoped process {pid} cannot be tracked: {error}"
            ))
        })?;
        let Some(process) = process else {
            return Err(AppOpenRefusal::not_installed(format!(
                "scoped process {pid} exited before it could be tracked"
            )));
        };
        self.launch_table.publish(app, pid, process, None);
        Ok(pid)
    }
}

/// Mirror of [`scrub_app_environment`] for a Tokio command: the scope starter
/// passes its environment to the app, so the loader overrides are removed
/// from the starter itself.
#[cfg(target_os = "linux")]
fn scrub_app_environment_std(command: &mut tokio::process::Command) {
    for key in ["LD_PRELOAD", "LD_LIBRARY_PATH", "LD_AUDIT"] {
        command.env_remove(key);
    }
}
/// The request passed to the injected Windows check, beside the held
/// capture: the publisher the file's signature must name, the floor its
/// signed version must meet, and the installed path. The check receives the
/// held capture and must read that, never reopen `path`, which is for
/// messages only. Production carries no verifier yet and fails closed.
///
/// Public like `crate::operator_apps::TestOperatorAppPolicy`: it crosses
/// the test-support boundary in `with_test_windows_verifier`.
#[allow(dead_code)]
#[derive(Clone, Debug)]
pub struct WindowsVerifyRequest {
    pub app: String,
    pub publisher: Option<String>,
    pub floor: Version,
    pub path: PathBuf,
}

/// The Windows signature check, injected in tests. It answers the verified
/// version string or the reason verification failed; the daemon enforces the
/// floor itself on the returned version, so a below-floor binary is refused
/// even when the checker passes it through. The check runs on the held
/// capture; the test hook may ignore the handle. Production builds carry no
/// checker: with the publisher unset, the production table never reaches
/// verification, so an unverified binary never launches.
#[allow(dead_code)]
#[cfg(windows)]
pub type WindowsVerifyFn = std::sync::Arc<
    dyn Fn(&subc_os::ExecutableCapture, &WindowsVerifyRequest) -> Result<String, String>
        + Send
        + Sync,
>;

/// A test-only fault hook for opening the tracking handle on a launched pid.
/// The daemon stops a launch it cannot track; the hook makes that open fail
/// so a test can prove nothing is left running.
#[allow(dead_code)]
#[cfg(windows)]
pub type WindowsTrackFn =
    std::sync::Arc<dyn Fn(u32) -> std::io::Result<Option<subc_os::Process>> + Send + Sync>;

/// The already-running attention request, injected in tests. The default only
/// logs: starting nothing and relaying nothing is the guarantee, and the
/// flash itself is best effort.
#[allow(dead_code)]
#[cfg(windows)]
pub type WindowsFlashFn = std::sync::Arc<dyn Fn(u32) + Send + Sync>;

/// What the Windows launch puts in `creation_flags`: suspended so the child
/// cannot run before it is bound, without a console window, and breaking
/// away so it never joins the daemon's root job (`winbase.h`).
/// Tests can omit the breakaway bit where the sandbox job forbids it: a test
/// handler creates no job, so a plain spawn is already outside one,
/// while production always sets it and fails closed when the job forbids it.
#[cfg(windows)]
const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;

#[cfg(windows)]
pub(crate) fn windows_creation_flags(breakaway: bool) -> u32 {
    let mut flags = subc_jobobject::CONTAINMENT_CREATION_FLAGS;
    if breakaway {
        flags |= CREATE_BREAKAWAY_FROM_JOB;
    }
    flags
}

#[cfg(windows)]
impl ControlHandler {
    /// Call the optional attention hook for an already-running app. Only
    /// Windows has one, and production only logs; Linux does nothing, and
    /// macOS never reaches `already_running`.
    fn flash_running_app(&self, pid: u32) {
        #[cfg(windows)]
        if let Some(flash) = self.windows_flash.as_ref() {
            flash(pid);
            return;
        }
        tracing::debug!(
            pid,
            "app already running; no window-flash primitive in this build"
        );
    }

    /// Verify the held file's signature and floor. With no checker configured
    /// verification fails closed: an unverified binary never launches, so a
    /// substitution, a wrong publisher, an unsigned binary and a below-floor
    /// binary are all refused with `app_verification_failed` and stamp
    /// nothing. The check runs on the held capture; production carries no
    /// checker and fails closed here.
    fn verify_windows_image(
        &self,
        capture: &subc_os::ExecutableCapture,
        request: WindowsVerifyRequest,
    ) -> Result<String, AppOpenRefusal> {
        #[cfg(windows)]
        if let Some(verify) = self.windows_verify.as_ref() {
            let version = verify(capture, &request).map_err(AppOpenRefusal::verification_failed)?;
            if !crate::operator_apps::meets_release_floor(Some(&version), &request.floor) {
                return Err(AppOpenRefusal::verification_failed(format!(
                    "verified version {version} is below the release floor"
                )));
            }
            return Ok(version);
        }
        Err(AppOpenRefusal::verification_failed(format!(
            "this build carries no Authenticode verifier for {}",
            request.app
        )))
    }

    /// Open a tracking handle on the launched pid. Tests can fail this step
    /// through `with_test_windows_track` to prove a launch that cannot be
    /// tracked is stopped, never left running.
    fn track_launched_process(&self, pid: u32) -> std::io::Result<Option<subc_os::Process>> {
        #[cfg(windows)]
        if let Some(track) = self.windows_track.as_ref() {
            return track(pid);
        }
        subc_os::Process::open(pid)
    }

    /// Verify, launch suspended outside the daemon's root job, compare the image
    /// identity, and resume or refuse. Every failure after the child exists
    /// stops it and refuses, and a failure before that refuses with no child
    /// to stop: no process is ever left running unverified, and
    /// a substitution of the path or a parent directory between verification
    /// and process creation fails the image comparison and stamps nothing.
    async fn windows_launch_app(&self, app: &str, path: &Path) -> Result<u32, AppOpenRefusal> {
        use std::os::windows::process::CommandExt as _;

        // Pin the installed file with read sharing only: no write or delete
        // sharing, so the name cannot be renamed or replaced while held. The
        // canonicalization plus double open inside rejects a swapped leaf
        // before anything runs.
        let capture = subc_os::ExecutableCapture::open(path).map_err(|error| {
            AppOpenRefusal::not_installed(format!(
                "installed file {} cannot be opened: {error}",
                path.display()
            ))
        })?;
        let request = WindowsVerifyRequest {
            app: app.to_owned(),
            publisher: self
                .operator_app_policy
                .windows_publisher()
                .map(str::to_owned),
            floor: self.operator_app_policy.release_floor().clone(),
            path: path.to_path_buf(),
        };
        // The signature and floor are checked on the held capture, before
        // any process exists.
        let _verified_version = self.verify_windows_image(&capture, request)?;
        let mut command = std::process::Command::new(capture.path());
        command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        // `creation_flags` replaces the whole mask; the composition lives
        // in `windows_creation_flags` so tests can pin every bit.
        command.creation_flags(windows_creation_flags(
            self.operator_app_policy.windows_breakaway,
        ));
        // `app.open` sends no launch arguments: the wire carries only the app id.
        #[cfg(test)]
        self.windows_spawn_attempts.fetch_add(1, Ordering::Relaxed);
        let mut child = command.spawn().map_err(|error| {
            AppOpenRefusal::verification_failed(format!(
                "installed file {} could not be started: {error}",
                path.display()
            ))
        })?;
        let pid = child.id();
        // Bind the spawn handle while the child is still suspended: the file
        // stays pinned across process creation, and the capture's identity is
        // tied to the process handle returned by spawn, never a reopened pid.
        let image = match capture.bind(&child) {
            Ok(image) => image,
            Err(error) => {
                stop_rejected_child(&mut child);
                return Err(AppOpenRefusal::verification_failed(format!(
                    "launched process {pid} could not be bound to its image: {error}"
                )));
            }
        };
        // Compare the new process's image with the held handle. The disk
        // identity must still name the verified file object, checked while
        // the child is suspended and pinned to its start time on both sides,
        // so a pid reused after exit can never pass. A mismatch stops the
        // child and refuses: the replacement is never resumed and never
        // stamped.
        if !matches!(
            image.agreement(image.start_time()),
            subc_os::ImageAgreement::Match(_)
        ) {
            stop_rejected_child(&mut child);
            return Err(AppOpenRefusal::verification_failed(format!(
                "launched process {pid} does not match its verified image"
            )));
        }
        let process = match self.track_launched_process(pid) {
            Ok(Some(process)) => process,
            Ok(None) => {
                stop_rejected_child(&mut child);
                return Err(AppOpenRefusal::verification_failed(format!(
                    "launched process {pid} exited before it could be tracked"
                )));
            }
            Err(error) => {
                stop_rejected_child(&mut child);
                return Err(AppOpenRefusal::verification_failed(format!(
                    "launched process {pid} cannot be tracked: {error}"
                )));
            }
        };
        // Published before the image runs, so a child that connects at once
        // is matched against this entry.
        self.launch_table.publish(app, pid, process, Some(child));
        // Resume the verified child. A child that cannot resume is stopped and
        // its published entry retired, so it never matches: nothing unverified
        // ever runs.
        if let Err(error) = subc_jobobject::resume_main_thread(pid) {
            if let Some(entry) = self.launch_table.take(app) {
                entry.stop_rejected();
            }
            return Err(AppOpenRefusal::verification_failed(format!(
                "launched process {pid} could not be resumed and was stopped: {error}"
            )));
        }
        Ok(pid)
    }

    /// A pipe client is eligible only when its pid is that of a live launch
    /// entry. The stamp records the entry's incarnation at connection time.
    pub(crate) fn stamp_windows_launch(
        &self,
        peer: &subc_os::local_peer::CapturedPeer,
    ) -> Option<crate::operator_apps::OperatorAppStamp> {
        use subc_os::local_peer::ObservedPeer as OsPeer;

        if !self.operator_app_policy.windows_enabled {
            return None;
        }
        let app = self.operator_app_policy.app_id()?;
        let OsPeer::Observed(observation) = peer.observation() else {
            return None;
        };
        let incarnation = self.launch_table.match_live_pid(app, observation.pid)?;
        Some(crate::operator_apps::OperatorAppStamp::launch_stamp(
            subc_protocol::peer::OperatorApp::new(app, observation.pid),
            peer.clone(),
            incarnation,
        ))
    }
}

/// Stop a rejected Windows child synchronously: a suspended or freshly bound
/// child that failed verification must not run, and relying on drop to reap
/// it would report a failure while the process still exists.
#[cfg(windows)]
fn stop_rejected_child(child: &mut std::process::Child) {
    let _ = child.kill();
    while matches!(child.try_wait(), Ok(None)) {
        std::thread::yield_now();
    }
}

#[cfg(test)]
mod tests;
