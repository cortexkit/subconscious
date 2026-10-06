use std::{
    collections::{HashMap, HashSet, VecDeque},
    error::Error,
    fmt, io,
    path::PathBuf,
    process::{ExitStatus, Stdio},
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use cortexkit_log::Retention;
use serde_json::Value;
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
    Flags, FrameType, Priority, SUBC_LAUNCH_NONCE_ENV, SUBC_MODULE_ID_ENV,
};
use tokio::{
    process::{Child, Command},
    sync::{mpsc, oneshot, watch, Mutex as AsyncMutex},
    task::JoinHandle,
    time::{sleep, sleep_until, timeout, timeout_at, Instant},
};
use tracing::{debug, error, info, warn};

use crate::{
    child_roster::ChildRoster,
    daemon_config::{
        CAPTURE_KEEP_ENV, CAPTURE_MAX_AGE_DAYS_ENV, CAPTURE_MAX_FILE_MB_ENV, CK_LOG_ENV,
    },
    forwarding::{
        CloseReason, ForwardingError, ForwardingTable, GoodbyeTarget, ModuleControlRpcOutcome,
        ModuleDrainTarget, PendingModuleControlRpc,
    },
    provenance::{spawned_file_identity, ExecutableIdentityProbe, SpawnedFileIdentity},
    registry::{ConnectionId, RegistryError},
    stderr_tail::{
        pump_stderr_to, pump_stdout_to, ChildOutputSink, StderrRing, StderrTailConfig,
        StderrTailSnapshot,
    },
    terminal_ring::{TerminalHistorySnapshot, TerminalRecord, TerminalRing, TerminalRingConfig},
    Frame, FrameSink, Registry,
};

#[path = "supervise_swap.rs"]
mod swap;

/// Command-line flag used by supervised modules to find subc.
///
/// subc launches module-mode children as `<module> --subc <connection-file-path>`.
/// The path points at the TCP+key connection file; it is not an ambient signal and
/// is never inherited by standalone children.
pub const SUBC_ARG: &str = "--subc";

const DEFAULT_MAX_RESTARTS: u32 = 3;
const DEFAULT_BACKOFF: Duration = Duration::from_millis(100);
const DEFAULT_MAX_BACKOFF: Duration = Duration::from_secs(30);
/// The span `DEFAULT_MAX_RESTARTS` is counted over. Ten minutes is long enough
/// to contain a real crash loop (which respawns in seconds) and short enough
/// that unrelated crashes hours apart never accumulate into a permanent stop.
const DEFAULT_RESTART_WINDOW: Duration = Duration::from_secs(600);
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
pub const DEFAULT_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);
const REGISTRY_RELEASE_TIMEOUT: Duration = Duration::from_secs(1);
const REGISTRY_RELEASE_POLL: Duration = Duration::from_millis(10);
/// How long a restart waits for the exited process's output readers.
///
/// The restart does not depend on the stderr reader finishing. A reader still
/// running at this bound is left running, and whatever it delivers later goes
/// into the exited process's own section of the stderr ring (see
/// `StderrRing::push_line_from`), ending naturally at EOF on its pipe. So the
/// bound no longer decides whether a crash's last lines are kept: under load
/// the reader may simply not have been scheduled yet, and cutting it there
/// discarded exactly the lines that explained the crash.
///
/// What the bound still decides is when the tail starts reporting
/// `Incomplete`: a pipe open past it usually means a descendant of the exited
/// process still holds it, and the tail cannot claim to be whole until that
/// pipe closes. That is also the only case in which the wait costs the restart
/// anything, because a pipe with no other holder reaches EOF when the process
/// exits. Under load a slow reader can show `Incomplete` briefly; it returns to
/// `Captured` at EOF with nothing lost.
///
/// The stdout reader carries no ring, only the capture file, and is still
/// stopped at this bound so an old process's stdout cannot trail into the file
/// after its successor starts.
const STDERR_PUMP_DRAIN_TIMEOUT: Duration = Duration::from_millis(250);
/// Maximum number of supervised process spawn/exit facts retained per daemon incarnation.
pub const SPAWN_EVENT_RING_CAPACITY: usize = 4096;
const SPAWN_SUBSCRIBER_BUFFER: usize = SPAWN_EVENT_RING_CAPACITY + 1;
/// Terminal Error code for a `supervisor.spawn_subscribe` stream the daemon
/// dropped because the subscriber stopped draining its frames. The detail's
/// `first_undelivered_cursor` names the first event it did not receive; the
/// client resubscribes from the last cursor it did receive.
pub(crate) const SPAWN_SUBSCRIBER_LAGGED_CODE: &str = "spawn_subscriber_lagged";

#[cfg(target_os = "macos")]
struct PrivacyExec {
    reader: tokio::io::unix::AsyncFd<std::io::PipeReader>,
    deadline: tokio::time::Instant,
    expected: Option<subc_os::FileIdentity>,
    trampoline: Option<subc_os::FileIdentity>,
    script: bool,
    module_id: String,
}

#[cfg(all(test, target_os = "macos"))]
pub(crate) fn test_privacy_trampoline() -> PathBuf {
    // Cargo's unit-test executable lives in <profile>/deps; its fixture bin
    // lives beside that directory. This honors custom CARGO_TARGET_DIR too.
    let path = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("privacy-trampoline-fixture");
    // Without the fixture every macOS spawn is refused, and the tests that
    // spawn fail later as a module in state Failed, which names the wrong
    // cause. `cargo test -p subc-daemon --lib` alone does not build it.
    assert!(
        path.exists(),
        "privacy-trampoline-fixture not built at {}: run `cargo build -p subc-daemon \
         --bins --features test-support` or `cargo test -p subc-daemon` first",
        path.display()
    );
    path
}

#[cfg(target_os = "macos")]
fn probe_privacy_trampoline(path: &std::path::Path) -> Result<(), String> {
    use std::io::Read;
    let mut probe = std::process::Command::new(path)
        .args(["__disclaim-exec", "--probe"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            format!(
                "privacy trampoline probe failed for {}: {error}",
                path.display()
            )
        })?;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let status = loop {
        match probe.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5))
            }
            result => {
                let _ = probe.kill();
                let _ = probe.wait();
                return Err(format!(
                    "privacy trampoline probe failed or timed out for {}: {result:?}",
                    path.display()
                ));
            }
        }
    };
    let mut answer = String::new();
    if let Some(stdout) = probe.stdout.take() {
        let _ = stdout.take(256).read_to_string(&mut answer);
    }
    if status.success() && answer.trim() == subc_os::privacy_identity::TRAMPOLINE_PROBE {
        return Ok(());
    }
    let mut diagnostic = String::new();
    if let Some(stderr) = probe.stderr.take() {
        let _ = stderr.take(1024).read_to_string(&mut diagnostic);
    }
    let cause = diagnostic
        .trim()
        .strip_prefix("ck-subc: own privacy identity refused: ")
        .unwrap_or("binary does not implement the privacy trampoline protocol");
    Err(format!(
        "{cause}: probe of {} exited {status}",
        path.display()
    ))
}

#[cfg(target_os = "macos")]
fn privacy_command(
    spec: &ModuleSpec,
    roster: &ChildRoster,
) -> Result<
    (
        Command,
        Option<PrivacyExec>,
        subc_os::privacy_identity::ExecAcknowledgement,
    ),
    SuperviseError,
> {
    let failure = |cause: String| {
        warn!(module_id = %spec.module_id, %cause, "privacy identity trampoline refused module spawn");
        SuperviseError::Spawn {
            program: spec.program.clone(),
            source: io::Error::other(cause),
            cgroup_path: None,
        }
    };
    let trampoline = roster.privacy_trampoline().map_err(failure)?;
    // Resolve PATH with the same environment the Command will receive. For
    // scripts retain the existing orphan-identity rule: the kernel chooses
    // the interpreter, and its observed image is the one recorded. Do not
    // duplicate the kernel's shebang/PATH interpreter resolution in Rust.
    let program = if spec.program.components().count() == 1 && !spec.program.is_absolute() {
        let path = spec
            .env
            .iter()
            .find(|(key, _)| key == "PATH")
            .map(|(_, value)| std::ffi::OsString::from(value))
            .or_else(|| std::env::var_os("PATH"))
            .unwrap_or_else(|| "/usr/bin:/bin".into());
        std::env::split_paths(&path)
            .map(|dir| dir.join(&spec.program))
            .find(|path| path.is_file())
            .unwrap_or_else(|| spec.program.clone())
    } else {
        spec.program.clone()
    };
    let expected = subc_os::file_identity(&program);
    let trampoline_image = subc_os::file_identity(&trampoline);
    let script = {
        use std::io::Read;
        let mut prefix = [0u8; 2];
        std::fs::File::open(&program)
            .is_ok_and(|mut file| file.read_exact(&mut prefix).is_ok() && &prefix == b"#!")
    };
    if expected.is_none() || expected == subc_os::file_identity(&trampoline) {
        return Err(failure(
            "privacy identity module executable is missing or is the trampoline itself".to_string(),
        ));
    }
    let (reader, ack) = subc_os::privacy_identity::ExecAcknowledgement::pipe()
        .map_err(|error| failure(error.to_string()))?;
    let reader =
        tokio::io::unix::AsyncFd::new(reader).map_err(|error| failure(error.to_string()))?;
    let mut command = Command::new(&trampoline);
    command
        .arg("__disclaim-exec")
        .arg(ack.fd().to_string())
        .arg(&program);
    ack.install(command.as_std_mut());
    Ok((
        command,
        Some(PrivacyExec {
            reader,
            deadline: tokio::time::Instant::now() + Duration::from_secs(5),
            expected,
            trampoline: trampoline_image,
            script,
            module_id: spec.module_id.clone(),
        }),
        ack,
    ))
}

struct SupervisedChild {
    child: Child,
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
    /// Refusal before the module image was accepted, retained for terminal records.
    spawn_failure: Option<String>,
    /// The protocol this process was launched with. A reload can store a new
    /// launch spec with a different protocol, but that takes effect only at the
    /// next spawn, so this process keeps being handled by the protocol it
    /// actually speaks.
    protocol: ModuleProtocol,
    /// This process's cgroup name: a bounded module/slot label followed by a
    /// spawn suffix unique to this process (when cgroup placement is on). A
    /// retired process in a slot may still be draining when a later one is
    /// spawned into that slot, so the suffix keeps the later process out of
    /// the retired one's cgroup, which is the domain a kill applies to.
    #[cfg(target_os = "linux")]
    module_id: String,
    #[cfg(target_os = "linux")]
    cgroup_placement: Option<subc_cgroup::Placement>,
    /// The job that contains this child and every process it spawns (issue #109).
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
    job: Option<subc_jobobject::JobObject>,
    stdout_pump: Option<JoinHandle<()>>,
    stderr_pump: Option<StderrPump>,
    stderr_ring: Arc<Mutex<StderrRing>>,
    spawned_at_ms: u64,
    spawned_from: PathBuf,
    spawned_file_identity: Option<SpawnedFileIdentity>,
    process_start_time: Option<u64>,
    process_identity: Option<ProcessIdentity>,
    pid: u32,
    /// This process's entry in the daemon's child roster, released when the
    /// process is reaped or this handle is dropped.
    roster_guard: Option<crate::child_roster::RosterGuard>,
}

impl SupervisedChild {
    fn id(&self) -> Option<u32> {
        Some(self.pid)
    }

    fn process_identity(&self) -> Option<ProcessIdentity> {
        self.process_identity
    }

    async fn wait(&mut self) -> io::Result<ExitStatus> {
        #[cfg(target_os = "macos")]
        self.confirm_privacy_exec().await;
        // The roster entry is NOT released here. A daemon shutdown waits for the
        // roster to empty and then exits the process, so releasing at the reap
        // let it exit before the exit handler wrote this child's terminal record
        // (the stderr drain and snapshot update sit in between), and the
        // shutdown's own `daemon_shutdown` record was intermittently lost. The
        // caller releases it after recording the exit (`release_roster`), and
        // dropping the handle releases it too.
        let result = self.child.wait().await;
        #[cfg(target_os = "linux")]
        if result.is_ok() {
            if let Some(placement) = self.cgroup_placement.as_ref() {
                cleanup_reaped_cgroup(placement, &self.module_id).await;
                // Keep ownership while awaiting kernel population changes: a
                // drain timeout may cancel this wait and then escalate/reap.
                self.cgroup_placement = None;
            }
        }
        result
    }

    #[cfg(target_os = "macos")]
    async fn confirm_privacy_exec(&mut self) {
        let Some(pending) = &mut self.privacy_exec else {
            return;
        };
        let result = tokio::time::timeout_at(pending.deadline, async {
            let mut record = Vec::new();
            loop {
                let mut ready = pending.reader.readable().await?;
                let read = ready.try_io(|reader| {
                    use std::io::Read;
                    let mut reader = reader.get_ref();
                    let mut buffer = [0u8; 256];
                    reader.read(&mut buffer).map(|count| (count, buffer))
                });
                match read {
                    Ok(Ok((0, _))) => return Ok::<_, io::Error>(record),
                    Ok(Ok((count, buffer))) => {
                        if record.len() + count > 1024 {
                            return Err(io::Error::other(
                                "privacy exec refusal record is too long",
                            ));
                        }
                        record.extend_from_slice(&buffer[..count]);
                    }
                    Ok(Err(error)) => return Err(error),
                    Err(_) => continue,
                }
            }
        })
        .await;
        // Keep the reader in self across await: select cancellation must not
        // discard the handshake or reset its original five-second deadline.
        let pending = self.privacy_exec.as_ref().expect("pending exec");
        let cause = match result {
            Err(_) => Some("privacy identity trampoline did not exec within 5s".to_string()),
            Ok(Err(error)) => Some(format!(
                "privacy identity exec acknowledgement failed: {error}"
            )),
            Ok(Ok(record)) if !record.is_empty() => Some(
                std::str::from_utf8(&record)
                    .ok()
                    .and_then(|record| {
                        record
                            .trim()
                            .strip_prefix(subc_os::privacy_identity::EXEC_REFUSAL_TAG)
                    })
                    .filter(|cause| !cause.is_empty())
                    .unwrap_or("invalid privacy exec refusal record")
                    .to_string(),
            ),
            Ok(Ok(_)) => match self.child.try_wait() {
                // Empty EOF is the exec acknowledgement. A real module may exit
                // immediately, including with a reserved trampoline status; no
                // image is admitted, and its ordinary exit contract stays intact.
                Ok(Some(_status)) => None,
                Err(error) => Some(format!("privacy identity trampoline wait failed: {error}")),
                Ok(None) => {
                    let image = observe_spawned_image(self.pid);
                    if let Some(image) = image.filter(|image| {
                        image.executable.is_some()
                            && image.executable != pending.trampoline
                            && (image.executable == pending.expected || pending.script)
                    }) {
                        if let Some(guard) = &self.roster_guard {
                            guard.confirm_executable(image);
                        }
                        let _ = self.report_ready.set(());
                        info!(module_id = %pending.module_id, pid = self.pid,
                            "module spawned with own privacy identity (responsibility disclaimed)");
                        None
                    } else if image.is_none()
                        || image.is_some_and(|image| image.executable.is_none())
                    {
                        // A process can exit between try_wait and the kernel
                        // image read. Empty EOF already acknowledged exec, so
                        // preserve that module's ordinary exit rather than
                        // mislabel a disappearing image as trampoline refusal.
                        // Keep pending in self across await so cancellation does
                        // not discard validation or reset its original deadline.
                        match tokio::time::timeout_at(pending.deadline, self.child.wait()).await {
                            Ok(Ok(_status)) => None,
                            Ok(Err(error)) => Some(format!("privacy identity trampoline wait failed: {error}")),
                            Err(_) => Some("privacy identity module executable remained unreadable until the 5s exec deadline".to_string()),
                        }
                    } else {
                        Some("privacy identity trampoline executable mismatch: expected the module program, not ck-subc".to_string())
                    }
                }
            },
        };
        let pending = self.privacy_exec.take().expect("pending exec");
        if let Some(cause) = cause {
            warn!(module_id = %pending.module_id, pid = self.pid, %cause, "privacy identity trampoline refused module spawn");
            self.spawn_failure = Some(cause);
            // No image is admitted on failure. Reach the entire fresh process
            // group, including a module which spawned a helper before refusal.
            if let Some(pid) = rustix::process::Pid::from_raw(self.pid as i32) {
                let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
            }
            let _ = self.child.start_kill();
        }
    }

    /// Releases this child's daemon-shutdown roster entry once its exit has
    /// been recorded. The pid is already reaped and free for reuse, so the
    /// entry must not outlive the record any longer than that.
    fn release_roster(&mut self) {
        self.roster_guard = None;
    }

    /// Kill the child and, where containment is available, its process tree.
    ///
    /// `Child::kill` is `TerminateProcess` scoped to one pid, so a module with a
    /// helper process leaked the helper — the Synapse embedding module's CUDA
    /// worker holds the GPU allocation, so the leak cost VRAM until the next
    /// restart of something else. Terminating the job reaches grandchildren that
    /// a tree walk cannot, including one whose parent has already exited and
    /// been reparented away.
    ///
    /// Best-effort like `request_graceful_stop`: a job failure is logged and the
    /// direct-child kill still decides the outcome, so containment can never
    /// change whether a module is reported as stopped.
    fn start_kill(&mut self) -> io::Result<()> {
        #[cfg(windows)]
        if let Some(job) = &self.job {
            if let Err(error) = job.terminate() {
                debug!(
                    error = %error,
                    "job termination failed; the direct-child kill still owns the outcome"
                );
            }
        }
        #[cfg(target_os = "linux")]
        kill_module_cgroup(self.cgroup_placement.as_ref(), &self.module_id);
        self.child.start_kill()
    }

    async fn drain_stderr(&mut self, module_id: &str) {
        if let Some(mut pump) = self.stdout_pump.take() {
            match timeout(STDERR_PUMP_DRAIN_TIMEOUT, &mut pump).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    warn!(module_id, error = %error, "stdout pump ended unexpectedly");
                }
                Err(_) => {
                    pump.abort();
                    warn!(
                        module_id,
                        waited = ?STDERR_PUMP_DRAIN_TIMEOUT,
                        "stdout pump did not drain before restart; stopped it before the next process"
                    );
                }
            }
        }

        let Some(pump) = self.stderr_pump.take() else {
            return;
        };
        settle_stderr_pump(
            module_id,
            &self.stderr_ring,
            pump,
            STDERR_PUMP_DRAIN_TIMEOUT,
        )
        .await;
    }
}

/// The reader task for one process's stderr, with the ring generation its
/// lines are attributed to.
struct StderrPump {
    task: JoinHandle<()>,
    generation: u64,
}

/// Retire an exited process's stderr reader and wait up to `bound` for it to
/// reach EOF. A reader still running at the bound is detached, not stopped: it
/// keeps filling the exited process's section of the ring until its pipe
/// closes, and the tail reads `Incomplete` until then. See
/// [`STDERR_PUMP_DRAIN_TIMEOUT`] for why.
async fn settle_stderr_pump(
    module_id: &str,
    ring: &Arc<Mutex<StderrRing>>,
    pump: StderrPump,
    bound: Duration,
) {
    let lock = || ring.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let StderrPump {
        mut task,
        generation,
    } = pump;
    lock().retire_pump(generation);
    match timeout(bound, &mut task).await {
        Ok(Ok(())) => {}
        Ok(Err(err)) => {
            let mut ring = lock();
            ring.mark_incomplete_from(generation, format!("stderr pump ended unexpectedly: {err}"));
            ring.finish_pump(generation);
            warn!(module_id, error = %err, "stderr pump ended before clean EOF");
        }
        Err(_) => {
            // Dropping the handle detaches the task; it ends at EOF on its pipe.
            drop(task);
            lock().mark_pump_late(
                generation,
                format!(
                    "stderr of the exited process had not reached EOF {bound:?} after it was \
                     retired (a descendant may still hold the pipe open); lines it still \
                     writes are kept in that process's section"
                ),
            );
            warn!(
                module_id,
                waited = ?bound,
                "stderr pipe of the exited process is still open; its reader keeps running without delaying the restart"
            );
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
    /// The wire protocol this module speaks, as DECLARED in daemon config.
    ///
    /// [`ModuleProtocol::None`] changes five things and nothing else: wire health
    /// probing is suppressed (an optional HTTP probe can replace it), teardown sends SIGTERM before waiting,
    /// `route.open` is refused, the spawn passes NO `--subc <path>` argument
    /// and NO launch nonce, and a clean exit the daemon did not request is
    /// restarted as a crash rather than recorded as a stop (see `on_child_exit`:
    /// a stock program exits 0 on a stray SIGTERM, and a stop would leave it
    /// down for good). `SUBC_MODULE_ID` still goes into the environment,
    /// because a process ignores an environment variable it does not read.
    ///
    /// The argument is the part that cannot be "harmless to a process that
    /// ignores it": a stock binary exits on an unknown flag before it listens
    /// (`nats-server`: "flag provided but not defined: -subc"), which is how the
    /// first conformance run against this mode found it. The nonce is withheld
    /// because a process that will never present it gains nothing from holding
    /// it, and a secret in the environment of a process that does not need it is
    /// a leak surface for no benefit.
    pub protocol: ModuleProtocol,
    /// Whether two processes of this module may run at once, which is what a
    /// blue/green swap does for the length of its overlap. Declared in daemon
    /// config because the daemon must be able to answer it while the module is
    /// down, and so a module cannot talk itself into it after registering.
    pub overlap: ModuleOverlap,
}

/// Whether a module tolerates a second process of itself running alongside.
///
/// Most modules are single-writer on their store (a WAL, a capture log, a
/// resident index behind a writer barrier), and two processes on one store
/// corrupt it. So a swap, which overlaps the old and new process by design,
/// is refused unless the module's config opts in with `overlap: "safe"`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ModuleOverlap {
    /// Never run two processes of this module at once. The default.
    #[default]
    Exclusive,
    /// The module has said a second process of itself is harmless for the
    /// length of a swap.
    ///
    /// Declare it only if a second instance can run for a few seconds without
    /// touching ANY single-writer store: every database, WAL, index, projector
    /// and scheduled job the module owns. A lease on part of that state is not
    /// enough. broca's session lease guards WAL appends while its run index, its
    /// store projector and its archive fold timer (which unlinks live WAL files)
    /// stay single-writer, so broca is exclusive despite holding a lease. The
    /// refusal only fires after this has been decided, so the decision is the
    /// check.
    Safe,
}

impl ModuleOverlap {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Exclusive => "exclusive",
            Self::Safe => "safe",
        }
    }
}

/// Environment variable telling a spawned module which case it was started
/// for, before it sends HELLO. Only a swap candidate carries it, as
/// [`SPAWN_ROLE_SWAP_CANDIDATE`]; every other spawn has it removed.
///
/// It chooses a warm-up budget, nothing else: a swap candidate can warm for
/// longer because nobody waits on it, while a plain restart must flip ready
/// quickly because callers see `module_warming` until it does. Absence means
/// plain restart, the safe reading. The daemon trusts nothing about it; the
/// candidate is proven by its launch nonce at HELLO.
pub const SUBC_SPAWN_ROLE_ENV: &str = "SUBC_SPAWN_ROLE";
/// The one value of [`SUBC_SPAWN_ROLE_ENV`] the daemon sets.
pub const SPAWN_ROLE_SWAP_CANDIDATE: &str = "swap_candidate";
/// How long a swap waits for its candidate to register and declare itself
/// ready when the operator does not say. A module warming as a swap candidate
/// may take up to 90 s (aft's ceiling, the largest in the fleet), so the
/// daemon allows that plus time to start the process and send HELLO.
pub const DEFAULT_SWAP_READY_TIMEOUT: Duration = Duration::from_secs(100);

/// Bounded restart policy for crash exits.
///
/// `max_restarts` is the number of replacement processes allowed after the
/// initial spawn WITHIN `window`. After that many crash restarts inside one
/// window the module enters [`ModuleState::Failed`] and the supervisor stops
/// the crash loop.
///
/// The budget is a RATE, not a lifetime total. It used to be a lifetime total,
/// and that only survived because crashes were rare: a module that crashed
/// three times across a week was disabled forever by crashes that had nothing
/// to do with each other. That stopped being survivable once modules began
/// exiting non-zero whenever the daemon's connection to them drops, because
/// then every daemon-side connection drop spends a unit of the same budget and
/// one flappy hour permanently stops a healthy module. Restarts older than
/// `window` release their slot, so a module that crashed twice yesterday has a
/// full budget today, while a genuine crash loop -- which is fast by
/// definition -- still reaches the cap and stops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestartPolicy {
    pub max_restarts: u32,
    /// Base delay before a crash replacement. The actual delay escalates with
    /// the number of recent crash replacements and is capped by `max_backoff`.
    pub backoff: Duration,
    /// Maximum delay before a crash replacement.
    pub max_backoff: Duration,
    /// The span `max_restarts` is counted over. `Duration::ZERO` makes the
    /// budget effectively infinite (nothing is ever in-window), which is why
    /// daemon config refuses `window_secs: 0` rather than quietly accepting it.
    pub window: Duration,
}

impl RestartPolicy {
    /// A policy with the default crash window. Callers that care about the
    /// window say so with [`Self::with_window`]; the ones that do not are
    /// asking for the standard rate limit, not for no limit.
    pub fn new(max_restarts: u32, backoff: Duration) -> Self {
        Self {
            max_restarts,
            backoff,
            max_backoff: DEFAULT_MAX_BACKOFF,
            window: DEFAULT_RESTART_WINDOW,
        }
    }

    pub fn with_max_backoff(mut self, max_backoff: Duration) -> Self {
        self.max_backoff = max_backoff;
        self
    }

    pub fn with_window(mut self, window: Duration) -> Self {
        self.window = window;
        self
    }

    /// Calculate the capped exponential delay for the next crash replacement.
    /// `restart_in_window` is zero for the first replacement after an operator
    /// action (restart, reload, re-enable) cleared the crash ring, or after all
    /// older crash replacements have aged out of the window.
    fn delay_for_restart(&self, restart_in_window: u32) -> Duration {
        if self.backoff.is_zero() || self.max_backoff.is_zero() {
            return Duration::ZERO;
        }

        let mut delay = self.backoff;
        for _ in 0..restart_in_window {
            if delay >= self.max_backoff {
                return self.max_backoff;
            }
            delay = delay
                .checked_mul(10)
                .unwrap_or(self.max_backoff)
                .min(self.max_backoff);
        }
        delay.min(self.max_backoff)
    }

    /// The one sentence that explains a budget-exhausted stop, used for both the
    /// log line and the terminal record so the two cannot drift. It names the
    /// window because `max_restarts=3` alone reads as a lifetime cap, which is
    /// exactly what this budget is not.
    fn budget_exhausted_detail(&self) -> String {
        format!(
            "crash budget exhausted: max_restarts={} within window_secs={}",
            self.max_restarts,
            self.window.as_secs()
        )
    }
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            max_restarts: DEFAULT_MAX_RESTARTS,
            backoff: DEFAULT_BACKOFF,
            max_backoff: DEFAULT_MAX_BACKOFF,
            window: DEFAULT_RESTART_WINDOW,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CrashRestartSchedule {
    restart_in_window: u32,
    delay: Duration,
}

/// Whether the daemon itself will bring this module back after the exit being
/// handled: it is enabled AND its in-window crash restarts are below the cap.
///
/// Takes `&mut` because reading the budget prunes it. Instants that fell out of
/// the window are dropped here rather than by a timer, so the count is right
/// the moment somebody asks and no bookkeeping runs for idle modules.
fn daemon_will_restart(
    state: &mut SupervisorSnapshot,
    policy: &RestartPolicy,
    now: Instant,
) -> bool {
    state.enabled && state.crash_restarts_in_window(policy.window, now) < policy.max_restarts
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthConfig {
    /// Optional loopback HTTP endpoint for a managed non-wire process.
    /// Changing it applies live on rescan; the process protocol changes only
    /// at its next spawn.
    pub http: Option<String>,
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
            http: None,
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
    pub module_id: String,
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
    pub process_start_time: Option<u64>,
    pub last_exit: Option<ExitReport>,
    pub health: ModuleHealthStatus,
}

#[derive(Debug, Clone, PartialEq)]
struct SupervisorSnapshot {
    state: ModuleState,
    enabled: bool,
    process_alive: bool,
    spawned_protocol: Option<ModuleProtocol>,
    spawn_failure: Option<String>,
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
    pid: Option<u32>,
    #[cfg(target_os = "macos")]
    report_ready: Option<Arc<OnceLock<()>>>,
    /// Last reaped child, retained after current process facts are cleared.
    reaped_pid: Option<u32>,
    /// Whether the command-serving supervision loop has a scheduled respawn.
    respawn_pending: bool,
    /// A second restart is waiting for the replacement already scheduled.
    coalesced_restart_pending: bool,
    spawned_at_ms: Option<u64>,
    spawned_from: Option<PathBuf>,
    spawned_file_identity: Option<SpawnedFileIdentity>,
    process_start_time: Option<u64>,
    deliberate_severance: Option<ProcessIdentity>,
    last_exit: Option<ExitReport>,
    /// Diagnostic attached to the next drain's terminal record, if any.
    drain_disposition_detail: Option<String>,
    health: ModuleHealthStatus,
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

    fn failed() -> Self {
        Self::new(ModuleState::Failed, true)
    }

    /// Crash restarts still inside `window`, having dropped the ones that are
    /// not. Pruning on read is what makes the budget a rate: an instant older
    /// than the window stops holding a slot the moment anybody counts.
    fn crash_restarts_in_window(&mut self, window: Duration, now: Instant) -> u32 {
        while let Some(oldest) = self.crash_restarts.front() {
            if now.duration_since(*oldest) > window {
                self.crash_restarts.pop_front();
            } else {
                break;
            }
        }
        u32::try_from(self.crash_restarts.len()).unwrap_or(u32::MAX)
    }

    /// Spend one unit of the crash budget and record the restart in the ledger.
    ///
    /// The ring is bounded by the cap because more than `max_restarts` in-window
    /// instants can never be reached (the caller refuses the restart first), so
    /// anything beyond that is an unbounded queue waiting to happen.
    fn record_crash_restart(&mut self, policy: &RestartPolicy, now: Instant) {
        self.crash_restarts.push_back(now);
        while self.crash_restarts.len() > policy.max_restarts as usize {
            self.crash_restarts.pop_front();
        }
        self.lifetime_restarts += 1;
    }

    /// Reserve one crash-restart slot and calculate the delay before respawning.
    /// The count is captured before recording this restart, so the first retry
    /// uses the base delay and each later in-window retry escalates once.
    fn next_crash_restart(
        &mut self,
        policy: &RestartPolicy,
        now: Instant,
    ) -> Option<CrashRestartSchedule> {
        let restart_in_window = self.crash_restarts_in_window(policy.window, now);
        if restart_in_window >= policy.max_restarts {
            return None;
        }
        self.record_crash_restart(policy, now);
        Some(CrashRestartSchedule {
            restart_in_window,
            delay: policy.delay_for_restart(restart_in_window),
        })
    }

    /// Give the module its full budget back, as an operator restart, reload, or
    /// re-enable does. `lifetime_restarts` deliberately does not move: it is the
    /// ledger of what actually happened, and an operator action does not unmake
    /// the crashes.
    fn clear_crash_restarts(&mut self) {
        self.crash_restarts.clear();
    }

    fn new(state: ModuleState, enabled: bool) -> Self {
        Self {
            state,
            enabled,
            process_alive: false,
            spawned_protocol: None,
            spawn_failure: None,
            crash_restarts: VecDeque::new(),
            lifetime_restarts: 0,
            spawn_generation: 0,
            pid: None,
            #[cfg(target_os = "macos")]
            report_ready: None,
            reaped_pid: None,
            respawn_pending: false,
            coalesced_restart_pending: false,
            spawned_at_ms: None,
            spawned_from: None,
            spawned_file_identity: None,
            process_start_time: None,
            deliberate_severance: None,
            last_exit: None,
            drain_disposition_detail: None,
            health: ModuleHealthStatus::default(),
            in_alternate_slot: false,
            draining_to_replace: false,
            configuration_updated_since_spawn: false,
        }
    }
}

type SharedSnapshot = Arc<Mutex<SupervisorSnapshot>>;

type SpawnSubscriberKey = (ConnectionId, u64);

#[derive(Debug)]
struct SpawnSubscriber {
    version: u8,
    frames: mpsc::Sender<Frame>,
    /// Tells this subscriber's forwarder that it was dropped for lagging, and
    /// from which event. The full frame channel cannot carry that news, so it
    /// travels beside it; see `SpawnEventFeed::subscribe`.
    lagged: Option<oneshot::Sender<SpawnCursor>>,
}

#[derive(Debug)]
struct SpawnEventState {
    daemon_incarnation: String,
    seq: u64,
    capacity: usize,
    live: HashMap<String, LiveSpawn>,
    generations: HashMap<String, u64>,
    events: VecDeque<SpawnEvent>,
    subscribers: HashMap<SpawnSubscriberKey, SpawnSubscriber>,
}

impl Default for SpawnEventState {
    fn default() -> Self {
        Self {
            daemon_incarnation: "unconfigured".to_string(),
            seq: 0,
            capacity: SPAWN_EVENT_RING_CAPACITY,
            live: HashMap::new(),
            generations: HashMap::new(),
            events: VecDeque::new(),
            subscribers: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Default)]
struct SpawnEventFeed(Arc<Mutex<SpawnEventState>>);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SpawnSubscribeRefusal {
    ForeignIncarnation { current: String },
    TooOld { oldest: SpawnCursor },
    Frame(String),
}

impl SpawnEventFeed {
    fn configure_incarnation(&self, daemon_incarnation: String) {
        let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        state.daemon_incarnation = daemon_incarnation;
        state.seq = 0;
        state.live.clear();
        state.generations.clear();
        state.events.clear();
        state.subscribers.clear();
    }

    fn cursor(state: &SpawnEventState) -> SpawnCursor {
        SpawnCursor {
            daemon_incarnation: state.daemon_incarnation.clone(),
            seq: state.seq,
        }
    }

    fn snapshot(&self) -> SpawnSnapshot {
        let state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let mut live = state.live.values().cloned().collect::<Vec<_>>();
        live.sort_by(|left, right| left.module_id.cmp(&right.module_id));
        SpawnSnapshot {
            cursor: Self::cursor(&state),
            ring_bound: state.capacity as u64,
            live,
        }
    }

    fn emit_spawned(&self, module_id: &str, pid: u32, spawned_at_ms: u64) -> u64 {
        let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let generation = state
            .generations
            .get(module_id)
            .copied()
            .unwrap_or(0)
            .checked_add(1)
            .expect("spawn generation exhausted");
        state.generations.insert(module_id.to_string(), generation);
        let live = LiveSpawn {
            module_id: module_id.to_string(),
            spawn_generation: generation,
            pid,
            spawned_at_ms,
        };
        state.live.insert(module_id.to_string(), live);
        Self::emit_locked(
            &mut state,
            SpawnEventKind::Spawned,
            module_id.to_string(),
            generation,
            pid,
            None,
            None,
        );
        generation
    }

    fn emit_exited(&self, module_id: &str, exit_code: Option<i32>, exit_signal: Option<i32>) {
        let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let Some(live) = state.live.remove(module_id) else {
            warn!(
                module_id,
                "terminal record had no live spawn event identity"
            );
            return;
        };
        Self::emit_locked(
            &mut state,
            SpawnEventKind::Exited,
            module_id.to_string(),
            live.spawn_generation,
            live.pid,
            exit_code,
            exit_signal,
        );
    }

    /// Report the exit of a process that a swap has already replaced.
    ///
    /// `emit_exited` removes the module's live entry, which after a swap's
    /// cutover describes the promoted candidate, not the old process now
    /// exiting. This emits the old generation's exit and leaves the live entry
    /// alone unless it still names that generation.
    fn emit_superseded_exited(
        &self,
        module_id: &str,
        spawn_generation: u64,
        pid: u32,
        exit_code: Option<i32>,
        exit_signal: Option<i32>,
    ) {
        let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if state
            .live
            .get(module_id)
            .is_some_and(|live| live.spawn_generation == spawn_generation)
        {
            state.live.remove(module_id);
        }
        Self::emit_locked(
            &mut state,
            SpawnEventKind::Exited,
            module_id.to_string(),
            spawn_generation,
            pid,
            exit_code,
            exit_signal,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_locked(
        state: &mut SpawnEventState,
        kind: SpawnEventKind,
        module_id: String,
        spawn_generation: u64,
        pid: u32,
        exit_code: Option<i32>,
        exit_signal: Option<i32>,
    ) {
        state.seq = state
            .seq
            .checked_add(1)
            .expect("spawn event sequence exhausted");
        let event = SpawnEvent {
            cursor: Self::cursor(state),
            kind,
            module_id,
            spawn_generation,
            pid,
            exit_code,
            exit_signal,
        };
        state.events.push_back(event.clone());
        while state.events.len() > state.capacity {
            state.events.pop_front();
        }
        let body = match serde_json::to_vec(&event) {
            Ok(body) => body,
            Err(error) => {
                error!(%error, "failed to serialize supervisor spawn event");
                return;
            }
        };
        state.subscribers.retain(|(connection_id, corr), subscriber| {
            let frame = Frame::build_with_version(
                subscriber.version,
                FrameType::StreamData,
                control_flags(),
                0,
                0,
                *corr,
                body.clone(),
            );
            match frame {
                Ok(frame) => {
                    if subscriber.frames.try_send(frame).is_ok() {
                        true
                    } else {
                        warn!(connection_id = connection_id.get(), corr, "dropping lagged supervisor spawn subscriber");
                        if let Some(lagged) = subscriber.lagged.take() {
                            let _ = lagged.send(event.cursor.clone());
                        }
                        false
                    }
                }
                Err(error) => {
                    warn!(connection_id = connection_id.get(), corr, %error, "dropping supervisor spawn subscriber after frame build failure");
                    false
                }
            }
        });
    }

    fn subscribe(
        &self,
        connection_id: ConnectionId,
        corr: u64,
        version: u8,
        since: Option<SpawnCursor>,
        sink: FrameSink,
    ) -> Result<(), SpawnSubscribeRefusal> {
        let (frames, mut receiver) = mpsc::channel(SPAWN_SUBSCRIBER_BUFFER);
        let (lagged, mut lagged_rx) = oneshot::channel::<SpawnCursor>();
        {
            let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
            let replay = if let Some(since) = since {
                if since.daemon_incarnation != state.daemon_incarnation {
                    return Err(SpawnSubscribeRefusal::ForeignIncarnation {
                        current: state.daemon_incarnation.clone(),
                    });
                }
                if let Some(oldest) = state.events.front().map(|event| event.cursor.clone()) {
                    if since.seq < oldest.seq.saturating_sub(1) {
                        return Err(SpawnSubscribeRefusal::TooOld { oldest });
                    }
                }
                state
                    .events
                    .iter()
                    .filter(|event| event.cursor.seq > since.seq)
                    .cloned()
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };
            for event in replay {
                let body = serde_json::to_vec(&event)
                    .map_err(|error| SpawnSubscribeRefusal::Frame(error.to_string()))?;
                let frame = Frame::build_with_version(
                    version,
                    FrameType::StreamData,
                    control_flags(),
                    0,
                    0,
                    corr,
                    body,
                )
                .map_err(|error| SpawnSubscribeRefusal::Frame(error.to_string()))?;
                frames
                    .try_send(frame)
                    .map_err(|error| SpawnSubscribeRefusal::Frame(error.to_string()))?;
            }
            state.subscribers.insert(
                (connection_id, corr),
                SpawnSubscriber {
                    version,
                    frames,
                    lagged: Some(lagged),
                },
            );
        }
        // The lagged terminal is sent here, by the forwarder, rather than by
        // the emitter: at the moment of the drop the subscriber's own channel
        // is full, and writing to the connection sink directly from the emitter
        // would put the Error AHEAD of the events still queued in that channel
        // (and the emitter holds the feed lock, so it cannot await the sink).
        // Dropping the subscriber drops the only sender, so `recv` drains every
        // queued event and then returns `None`; only then is the Error sent, so
        // the client sees each event it can keep, then the reason it was cut.
        // Cancel and connection removal drop the oneshot unsent, so they end
        // the stream with no Error.
        tokio::spawn(async move {
            while let Some(frame) = receiver.recv().await {
                if sink.send(frame).await.is_err() {
                    return;
                }
            }
            let Ok(first_undelivered) = lagged_rx.try_recv() else {
                return;
            };
            match spawn_subscriber_lagged_frame(version, corr, first_undelivered) {
                Ok(frame) => {
                    let _ = sink.send(frame).await;
                }
                Err(error) => {
                    error!(%error, corr, "failed to build lagged spawn subscriber terminal frame");
                }
            }
        });
        Ok(())
    }

    fn cancel(&self, connection_id: ConnectionId, corr: u64) -> bool {
        let Some(subscriber) = self
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .subscribers
            .remove(&(connection_id, corr))
        else {
            return false;
        };
        if let Ok(frame) = Frame::build_with_version(
            subscriber.version,
            FrameType::StreamEnd,
            control_flags(),
            0,
            0,
            corr,
            Vec::new(),
        ) {
            tokio::spawn(async move {
                let _ = subscriber.frames.send(frame).await;
            });
        }
        true
    }

    fn remove_connection(&self, connection_id: ConnectionId) {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .subscribers
            .retain(|(subscriber_connection, _), _| *subscriber_connection != connection_id);
    }

    #[cfg(any(test, feature = "test-support"))]
    fn set_capacity(&self, capacity: usize) {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).capacity = capacity;
    }

    #[cfg(any(test, feature = "test-support"))]
    fn subscriber_count(&self) -> usize {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .subscribers
            .len()
    }
}

/// Narrow process-liveness signal published by supervisors and consumed by passive liveness polls.
/// The terminal Error a lagged spawn subscriber receives after its queued events.
fn spawn_subscriber_lagged_frame(
    version: u8,
    corr: u64,
    first_undelivered: SpawnCursor,
) -> Result<Frame, String> {
    let body = serde_json::to_vec(&subc_protocol::ErrorBody {
        code: SPAWN_SUBSCRIBER_LAGGED_CODE.to_string(),
        message: "spawn subscriber fell behind and was dropped; resubscribe from the last cursor received"
            .to_string(),
        detail: Some(serde_json::json!({
            "first_undelivered_cursor": first_undelivered
        })),
    })
    .map_err(|error| error.to_string())?;
    Frame::build_with_version(version, FrameType::Error, control_flags(), 0, 0, corr, body)
        .map_err(|error| error.to_string())
}

pub trait ModuleProcessLiveness: Send + Sync {
    fn process_live(&self, module_id: &str) -> Option<bool>;

    /// Whether the supervisor is replacing this module's process right now: an
    /// operator restart or reload, a health restart, or a crash respawn whose
    /// backoff is running. A module in that state is not live, but a consumer
    /// refused now should retry shortly rather than treat the target as gone.
    /// Stopped, failed, and disabled modules are not replacing.
    fn process_replacing(&self, _module_id: &str) -> bool {
        false
    }
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

    fn process_replacing(&self, module_id: &str) -> bool {
        let Some(snapshot) = self
            .snapshots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(module_id)
            .cloned()
        else {
            return false;
        };
        let snapshot = snapshot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        snapshot.enabled
            && match snapshot.state {
                ModuleState::Restarting => true,
                ModuleState::Draining => snapshot.draining_to_replace,
                ModuleState::Starting
                | ModuleState::Running
                | ModuleState::Unresponsive
                | ModuleState::Stopped
                | ModuleState::Failed
                | ModuleState::Disabled => false,
            }
    }
}

#[cfg(test)]
#[derive(Debug, Default)]
struct ReloadExitRecordGate {
    reached: tokio::sync::Notify,
    resume: tokio::sync::Notify,
}

#[derive(Debug, Clone, Copy)]
enum RespawnKind {
    Spawn,
    Reload,
}

#[derive(Debug, Clone, Copy)]
struct PendingRespawn {
    deadline: Instant,
    kind: RespawnKind,
}

type ReloadReply = oneshot::Sender<Result<(), SuperviseError>>;

#[derive(Debug, Clone)]
struct SupervisorRuntimeConfig {
    /// Restart operations hand their backoff to the loop, which keeps serving commands.
    scheduled_respawn: Arc<Mutex<Option<PendingRespawn>>>,
    /// A reload acknowledges completion only after its replacement registers.
    deferred_reload_reply: Arc<Mutex<Option<ReloadReply>>>,
    restart_policy: RestartPolicy,
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
    connection_file_path: Option<PathBuf>,
    capture_logs_dir: Option<PathBuf>,
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
    spawn_events: SpawnEventFeed,
    child_roster: ChildRoster,
    #[cfg(target_os = "linux")]
    cgroup_placement: Option<subc_cgroup::Placement>,
    #[cfg(test)]
    test_seed_stale_facts_before_enable_spawn: bool,
    #[cfg(test)]
    test_reload_exit_record_gate: Option<Arc<ReloadExitRecordGate>>,
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

/// Told when a swap has promoted its candidate to be the module's active
/// registration.
///
/// An ordinary HELLO runs the control plane's registration side effects (the
/// capability cache, the deny census, the requirement recompute) as it
/// registers. A swap candidate's HELLO does not, because it is not routable;
/// promotion is when those must run instead, and promotion happens in the
/// supervisor, which has no other way into the control handler.
pub(crate) trait SwapPromotionObserver: Send + Sync {
    fn swap_promoted(&self, registration: &crate::registry::ModuleRegistration);
}

/// The installed [`SwapPromotionObserver`], held weakly: the observer (the
/// control handler) owns this handle, so a strong reference back would be a
/// cycle that keeps both alive.
#[derive(Clone, Default)]
struct PromotionObserverSlot(Arc<Mutex<Option<std::sync::Weak<dyn SwapPromotionObserver>>>>);

impl fmt::Debug for PromotionObserverSlot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PromotionObserverSlot")
    }
}

/// The nonces of one open swap.
#[derive(Debug, Clone)]
struct OpenSwap {
    /// The launch nonce minted for the candidate process. It is the swap
    /// token: the only thing that admits a HELLO into the candidate slot.
    candidate_nonce: String,
    /// The incumbent's launch nonce, captured when the swap opened. It is kept
    /// here because cutover moves the module's recorded spawn nonce to the
    /// candidate while the incumbent is still draining and its consumers are
    /// still attesting with this one.
    incumbent_nonce: Option<String>,
    /// Set once a HELLO has been admitted with the swap token, so the token
    /// admits one registration and cannot be replayed after cutover empties
    /// the candidate slot.
    candidate_admitted: bool,
}

/// What the swap gate says about a HELLO. See
/// [`SupervisorHandle::swap_hello_admission`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SwapHelloAdmission {
    /// No swap is open for the id (or the HELLO carries the incumbent's own
    /// nonce); the ordinary gates decide.
    NotSwapping,
    /// The HELLO carries the swap token: register it into the candidate slot.
    Candidate,
    /// A swap is open and the HELLO carries a nonce the supervisor did not
    /// mint for this id, no nonce, or a token already used.
    Refused,
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

    pub(crate) fn spawn_snapshot(&self) -> SpawnSnapshot {
        self.spawn_events.snapshot()
    }

    pub(crate) fn subscribe_spawns(
        &self,
        connection_id: ConnectionId,
        corr: u64,
        version: u8,
        since: Option<SpawnCursor>,
        sink: FrameSink,
    ) -> Result<(), SpawnSubscribeRefusal> {
        self.spawn_events
            .subscribe(connection_id, corr, version, since, sink)
    }

    pub(crate) fn cancel_spawn_subscription(&self, connection_id: ConnectionId, corr: u64) -> bool {
        self.spawn_events.cancel(connection_id, corr)
    }

    pub(crate) fn remove_spawn_subscribers(&self, connection_id: ConnectionId) {
        self.spawn_events.remove_connection(connection_id);
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn set_spawn_event_capacity_for_test(&self, capacity: usize) {
        assert!(capacity > 0, "spawn event capacity must be non-zero");
        self.spawn_events.set_capacity(capacity);
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn spawn_subscriber_count_for_test(&self) -> usize {
        self.spawn_events.subscriber_count()
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

    /// The launch nonce most recently minted for a module's spawn, if any.
    #[cfg(test)]
    pub(crate) fn spawn_nonce(&self, module_id: &str) -> Option<String> {
        self.spawn_nonces
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(module_id)
            .cloned()
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
                // While the owner is being swapped, children started by
                // either of its two processes hold that process's nonce.
                || self.swap_nonce_matches(&owner_module_id, presented)
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
        let current = nonces
            .get(module_id)
            .is_some_and(|expected| constant_time_eq(expected.as_bytes(), presented.as_bytes()));
        drop(nonces);
        // During a swap two processes of the module are alive, and a consumer
        // started by either one presents that process's nonce. Accepting only
        // the recorded one would fail the incumbent's consumers for the whole
        // overlap once cutover moves the record to the candidate.
        current || self.swap_nonce_matches(module_id, presented)
    }

    /// Whether `presented` is either nonce of an open swap for `module_id`.
    fn swap_nonce_matches(&self, module_id: &str, presented: &str) -> bool {
        let swaps = self
            .swaps
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        swaps.get(module_id).is_some_and(|swap| {
            constant_time_eq(swap.candidate_nonce.as_bytes(), presented.as_bytes())
                || swap.incumbent_nonce.as_deref().is_some_and(|incumbent| {
                    constant_time_eq(incumbent.as_bytes(), presented.as_bytes())
                })
        })
    }

    /// Open a swap for `module_id` with the candidate's freshly minted nonce.
    /// Called before the candidate process exists.
    pub(crate) fn open_swap(&self, module_id: &str, candidate_nonce: String) {
        let incumbent_nonce = self
            .spawn_nonces
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(module_id)
            .cloned();
        self.swaps
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(
                module_id.to_string(),
                OpenSwap {
                    candidate_nonce,
                    incumbent_nonce,
                    candidate_admitted: false,
                },
            );
    }

    /// Close the swap for `module_id`, releasing whichever nonce is no longer
    /// the module's recorded one.
    pub(crate) fn close_swap(&self, module_id: &str) {
        self.swaps
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(module_id);
    }

    /// Install the observer told about swap promotions, replacing any earlier
    /// one.
    pub(crate) fn set_swap_promotion_observer(
        &self,
        observer: std::sync::Weak<dyn SwapPromotionObserver>,
    ) {
        *self
            .promotion_observer
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(observer);
    }

    /// Tell the installed observer, if it is still alive, that a swap promoted
    /// `registration`.
    fn notify_swap_promoted(&self, registration: &crate::registry::ModuleRegistration) {
        let observer = self
            .promotion_observer
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .and_then(std::sync::Weak::upgrade);
        if let Some(observer) = observer {
            observer.swap_promoted(registration);
        }
    }

    /// Whether a swap is open for `module_id`.
    pub(crate) fn swap_open(&self, module_id: &str) -> bool {
        self.swaps
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains_key(module_id)
    }

    /// Make the candidate's nonce the module's recorded spawn nonce, as a plain
    /// respawn would, once cutover has made the candidate the module's process.
    /// The swap stays open so the incumbent's nonce keeps attesting until the
    /// incumbent has drained and exited.
    fn promote_swap_nonce(&self, module_id: &str, reserved: bool) {
        let candidate_nonce = self
            .swaps
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(module_id)
            .map(|swap| swap.candidate_nonce.clone());
        let Some(nonce) = candidate_nonce else {
            return;
        };
        self.set_spawn_nonce(module_id, nonce.clone());
        if reserved {
            self.set_reserved_nonce(module_id, nonce);
        }
    }

    /// The swap gate for a HELLO claiming `module_id`.
    ///
    /// This runs BEFORE the reserved-module gate. A reserved module's candidate
    /// presents the candidate nonce, which the reserved gate (holding the
    /// incumbent's nonce) would refuse as `reserved_module` before swap
    /// admission was ever reached. And it applies to unreserved ids too: for an
    /// unreserved id the only thing that ever stopped a second process claiming
    /// a live id was the `duplicate_module_id` refusal, which is exactly the
    /// refusal a swap lifts for its candidate.
    ///
    /// The incumbent's own nonce falls through to the ordinary gates, which
    /// treat it as they always have (a live incumbent is refused as a
    /// duplicate). Anything else while a swap is open is refused, including an
    /// absent nonce.
    pub(crate) fn swap_hello_admission(
        &self,
        module_id: &str,
        presented: Option<&str>,
    ) -> SwapHelloAdmission {
        let swaps = self
            .swaps
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(swap) = swaps.get(module_id) else {
            return SwapHelloAdmission::NotSwapping;
        };
        let Some(presented) = presented else {
            return SwapHelloAdmission::Refused;
        };
        if constant_time_eq(swap.candidate_nonce.as_bytes(), presented.as_bytes()) {
            return if swap.candidate_admitted {
                SwapHelloAdmission::Refused
            } else {
                SwapHelloAdmission::Candidate
            };
        }
        if swap
            .incumbent_nonce
            .as_deref()
            .is_some_and(|incumbent| constant_time_eq(incumbent.as_bytes(), presented.as_bytes()))
        {
            return SwapHelloAdmission::NotSwapping;
        }
        SwapHelloAdmission::Refused
    }

    /// Record that the swap token has registered a candidate, so it admits no
    /// second HELLO.
    pub(crate) fn mark_swap_candidate_admitted(&self, module_id: &str) {
        if let Some(swap) = self
            .swaps
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get_mut(module_id)
        {
            swap.candidate_admitted = true;
        }
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
        // Normally already marked before the process was spawned; marking here
        // too keeps `configured_ids` a superset of the roster for any caller
        // that inserts a module directly.
        self.mark_configured(module.module_id());
        let mut modules = self
            .modules
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        modules.insert(module.module_id().to_string(), module)
    }

    /// Record that the supervisor has taken on `module_id`. Called before the
    /// module's first process is spawned, so that by the time that process can
    /// register, [`Self::is_configured`] already answers true.
    fn mark_configured(&self, module_id: &str) {
        self.configured_ids
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(module_id.to_string());
    }

    /// Undo [`Self::mark_configured`] for a module whose first spawn failed
    /// before it was ever put on the roster. A module already on the roster
    /// keeps its mark: only [`Self::retire`] takes a rostered module off.
    fn unmark_configured_unless_rostered(&self, module_id: &str) {
        let modules = self
            .modules
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !modules.contains_key(module_id) {
            self.configured_ids
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(module_id);
        }
    }

    /// Whether `module_id` is a module this daemon supervises: on the roster,
    /// or about to be (its process is being spawned right now).
    ///
    /// This, not `get(..).is_some()`, is what "the owner is configured" means
    /// for scopes: a supervised module's process can register and sync before
    /// [`Self::get`] can return it, and in that window it is still a module
    /// that will sync, not one that never will.
    pub(crate) fn is_configured(&self, module_id: &str) -> bool {
        self.configured_ids
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains(module_id)
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

    /// Arm the one-shot marker for the module process that this caller
    /// deliberately initiated severance against. Generic connection teardown
    /// must not call this:
    /// a surviving process would otherwise retain an exemption for a later
    /// genuine crash.
    pub fn record_deliberate_severance(&self, module_id: &str) -> Result<bool, SuperviseError> {
        let Some(module) = self.get(module_id) else {
            return Ok(false);
        };
        let snapshot = lock_snapshot(&module.inner.snapshot)?;
        let Some((pid, start_time)) = snapshot.pid.zip(snapshot.process_start_time) else {
            return Ok(false);
        };
        drop(snapshot);
        module.record_deliberate_severance(ProcessIdentity { pid, start_time })
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
        self.close_swap(module_id);
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
        let removed = self
            .modules
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(module_id);
        self.configured_ids
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(module_id);
        removed
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
    capture_logs_dir: Option<PathBuf>,
    forwarding: Option<Arc<ForwardingTable>>,
    process_liveness: Arc<SupervisorProcessLiveness>,
    supervisor_handle: Option<SupervisorHandle>,
    health: HealthConfig,
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

/// Test-only hook run on the path that takes on a new module, right after its
/// first `spawn_child` returns (the process exists and could already be
/// registering) and before that process is handed to the module's supervise
/// loop and put on the roster. Lets a test observe what a fast child would see
/// in that window without racing a real one.
#[cfg(test)]
#[derive(Clone, Default)]
struct AfterFirstSpawnHook(Option<AfterFirstSpawnFn>);

#[cfg(test)]
type AfterFirstSpawnFn = Arc<dyn Fn(&str) + Send + Sync>;

#[cfg(test)]
impl fmt::Debug for AfterFirstSpawnHook {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AfterFirstSpawnHook")
    }
}

#[cfg(test)]
impl AfterFirstSpawnHook {
    fn run(&self, module_id: &str) {
        if let Some(hook) = &self.0 {
            hook(module_id);
        }
    }
}

impl Supervisor {
    #[cfg(test)]
    pub(crate) fn new_for_test(registry: Arc<Registry>, policy: RestartPolicy) -> Self {
        let supervisor = Self::new(registry, policy);
        #[cfg(target_os = "macos")]
        let supervisor = supervisor.with_privacy_trampoline(test_privacy_trampoline());
        supervisor
    }
    /// Verify the trampoline once when configured. Missing private OS support
    /// refuses every macOS launch by name but does not stop the daemon's control
    /// server. Embedders must explicitly provide a binary with the subc-os hidden
    /// entry point; the library must not exec an arbitrary hosting program.
    pub fn with_privacy_trampoline(self, path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        #[cfg(target_os = "macos")]
        {
            let result = probe_privacy_trampoline(&path).map(|()| path);
            if let Err(cause) = &result {
                error!(%cause, "macOS privacy identity unavailable; supervised launches will refuse");
            }
            self.child_roster.set_privacy_trampoline(result);
        }
        #[cfg(not(target_os = "macos"))]
        let _ = path;
        self
    }
    /// The first step of an announced daemon shutdown, before the notice and
    /// before any connection is closed.
    ///
    /// Sets the daemon-shutdown flag first: from here on no module is
    /// respawned (crash restart, operator restart, or swap), and every child
    /// exit is recorded as `daemon_shutdown` rather than as a crash, whether
    /// the module exits on the EOF this shutdown gives it or is signalled by a
    /// service manager that kills the whole cgroup. Then writes the journal's
    /// shutdown marker, which records the instant and closes this daemon
    /// incarnation's stretch of the journal.
    #[cfg(unix)]
    pub(crate) fn begin_daemon_shutdown(&self) {
        self.child_roster.close();
        if let Some(journal) = &self.terminal_journal {
            journal.stamp_shutdown();
        }
    }

    /// Announce a cut while established connections can still carry replies.
    /// These budgets promise notice and a bounded wait, not child completion;
    /// they are local policy, not an estimate of launchd's unknown kill ceiling.
    #[cfg(unix)]
    pub(crate) async fn drain_for_daemon_shutdown(&self) -> Result<(), SuperviseError> {
        const NOTICE_BUDGET: Duration = Duration::from_millis(500);
        const DRAIN_BUDGET: Duration = Duration::from_secs(2);
        let Some(forwarding) = &self.forwarding else {
            return Ok(());
        };
        let module_ids = forwarding
            .begin_daemon_drain()
            .map_err(SuperviseError::Forwarding)?;
        let deadline_ms =
            unix_ms_now().saturating_add((NOTICE_BUDGET + DRAIN_BUDGET).as_millis() as u64);
        let mut notices = tokio::task::JoinSet::new();
        let mut drains = Vec::new();
        for module_id in module_ids {
            let Some(target) = forwarding
                .begin_module_drain(&module_id, RouteCloseReason::Restart)
                .map_err(SuperviseError::Forwarding)?
            else {
                continue;
            };
            let routes = forwarding
                .endpoint_routes(target.endpoint)
                .map_err(SuperviseError::Forwarding)?;
            // Restart allows deployed consumers to reopen after the new daemon
            // appears. The wire reason stays `restart`; what tells a daemon cut
            // apart from a module restart afterwards is the terminal record
            // itself, whose disposition is `daemon_shutdown` for every exit
            // observed once `begin_daemon_shutdown` has run.
            let command = serde_json::to_vec(&ModuleControlCommand::Draining {
                reason: RouteCloseReason::Restart,
                deadline_ms,
            })
            .expect("module draining serializes");
            let mut recipients = vec![(target.sink.clone(), target.negotiated_ver, command)];
            let mut clients: Vec<(crate::forwarding::GoodbyeTarget, Vec<u16>)> = Vec::new();
            for route in routes {
                let client = route.goodbye_target;
                if let Some((_, channels)) = clients
                    .iter_mut()
                    .find(|(existing, _)| existing.connection_id == client.connection_id)
                {
                    channels.push(client.channel);
                } else {
                    let channel = client.channel;
                    clients.push((client, vec![channel]));
                }
            }
            for (client, mut channels) in clients {
                channels.sort_unstable();
                channels.dedup();
                let closing = serde_json::to_vec(&ClientControlPush::RouteClosing {
                    module_id: module_id.clone(),
                    channels,
                    reason: RouteCloseReason::Restart,
                })
                .expect("route closing serializes");
                recipients.push((client.sink, client.negotiated_ver, closing));
            }
            for (sink, version, body) in recipients {
                notices.spawn(async move {
                    let frame = Frame::build_with_version(
                        version,
                        FrameType::Push,
                        control_flags(),
                        0,
                        0,
                        0,
                        body,
                    )
                    .expect("bounded lifecycle notice frame builds");
                    sink.send_flushed(frame).await
                });
            }
            let gauges = declared_busy_gauges(&self.registry, &module_id)?;
            drains.push((module_id, target.endpoint, gauges));
        }
        // A quiet forwarding table is not proof that queued notices reached the
        // socket. Wait for writer flush acknowledgements before testing quiescence.
        let notice_deadline = Instant::now() + NOTICE_BUDGET;
        while let Ok(Some(result)) = timeout_at(notice_deadline, notices.join_next()).await {
            if !matches!(result, Ok(Ok(()))) {
                warn!(?result, "daemon shutdown notice delivery failed");
            }
        }
        notices.abort_all();
        let deadline = Instant::now() + DRAIN_BUDGET;
        let mut waits = tokio::task::JoinSet::new();
        for (module_id, endpoint, gauges) in drains {
            let forwarding = Arc::clone(forwarding);
            let mut runtime = self.runtime_config();
            runtime.health.cadence = Duration::from_millis(100);
            waits.spawn(async move {
                wait_for_forwarding_quiescence(
                    &forwarding,
                    &module_id,
                    &runtime,
                    endpoint,
                    deadline,
                    &gauges,
                    DrainScope::Active,
                )
                .await
            });
        }
        while let Ok(Some(result)) = timeout_at(deadline, waits.join_next()).await {
            if !matches!(result, Ok(Ok(true))) {
                warn!(?result, "daemon shutdown drain did not reach quiescence");
            }
        }
        Ok(())
    }

    /// The last step of an announced daemon shutdown, after the notice and the
    /// drain: send every registered module a module GOODBYE, the same planned
    /// stop signal `ck module stop` gives, then close every connection so each
    /// subc module sees EOF and starts its own teardown, then end every
    /// supervised child that has not exited
    /// by its own deadline (its drain budget, capped). Modules lead their own
    /// process groups, so a
    /// service manager's group kill no longer reaches them; without this a
    /// child that does not stop on EOF (every `protocol: "none"` child, which
    /// has no connection) would outlive the daemon. Every wait is bounded (see
    /// `child_roster`), and `escalate` resolving (a second SIGTERM) cuts them.
    #[cfg(unix)]
    pub(crate) async fn end_children_for_daemon_shutdown(
        &self,
        already_escalated: bool,
        escalate: impl std::future::Future<Output = ()>,
    ) {
        tokio::pin!(escalate);
        let mut escalated = already_escalated;
        if let Some(forwarding) = &self.forwarding {
            let reason = CloseReason::new(
                "daemon_shutdown",
                "the daemon is exiting after its shutdown notice and drain",
            );
            if escalated {
                // The operator asked to stop waiting: queue the GOODBYEs but
                // do not wait for them to be written.
                send_module_goodbyes_for_daemon_shutdown(forwarding, &reason, false).await;
            } else {
                tokio::select! {
                    biased;
                    _ = escalate.as_mut() => {
                        info!("second SIGTERM: abandoning module GOODBYE delivery");
                        escalated = true;
                    }
                    _ = send_module_goodbyes_for_daemon_shutdown(forwarding, &reason, true) => {}
                }
            }
            let closed = forwarding.close_all_connections(&reason);
            debug!(closed, "closed established connections for daemon shutdown");
        }
        // If the second SIGTERM arrived during GOODBYE delivery, `escalate` has
        // already completed and must not be polled again; the child shutdown
        // wait is told it is escalated and gets a future that never fires.
        let escalated_here = escalated && !already_escalated;
        let remaining_escalate = async move {
            if escalated_here {
                std::future::pending::<()>().await;
            } else {
                escalate.await;
            }
        };
        crate::child_roster::end_children_for_daemon_shutdown(
            &self.child_roster,
            escalated,
            remaining_escalate,
        )
        .await;
    }

    pub fn new(registry: Arc<Registry>, restart_policy: RestartPolicy) -> Self {
        Self {
            registry,
            restart_policy,
            drain_timeout: DEFAULT_DRAIN_TIMEOUT,
            connection_file_path: None,
            capture_logs_dir: None,
            forwarding: None,
            process_liveness: Arc::new(SupervisorProcessLiveness::default()),
            supervisor_handle: None,
            health: HealthConfig::default(),
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
        self.forwarding = Some(forwarding);
        self
    }

    pub fn with_handle(mut self, supervisor_handle: SupervisorHandle) -> Self {
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

    #[cfg(target_os = "linux")]
    pub fn with_cgroup_placement(
        mut self,
        cgroup_placement: Option<subc_cgroup::Placement>,
    ) -> Self {
        self.cgroup_placement = cgroup_placement;
        self
    }

    /// Spawn `spec.program` and start monitoring it.
    ///
    /// The child is expected to parse `--subc <connection-file-path>`, read the
    /// TCP+key connection file, authenticate to the already-running listener, and
    /// register with channel-0 `HELLO` using `spec.module_id` as its manifest id.
    pub fn spawn(&self, spec: ModuleSpec) -> Result<SupervisedModule, SuperviseError> {
        validate_spec(&spec)?;
        self.establish_identity(&spec);

        let runtime = self.runtime_config();
        let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::starting()));
        let spawned = spawn_child(
            &spec,
            runtime.connection_file_path.as_deref(),
            self.supervisor_handle.as_ref(),
            &runtime.stderr_ring,
            runtime.capture_logs_dir.as_deref(),
            &runtime.child_roster,
            #[cfg(target_os = "linux")]
            runtime.cgroup_placement.as_ref(),
        );
        #[cfg(test)]
        self.test_after_first_spawn.run(&spec.module_id);
        let child = match spawned {
            Ok(child) => child,
            Err(err) => {
                // Unlike the configured paths, a failed `spawn` leaves nothing
                // on the roster, so the module must not stay marked configured.
                self.abandon_unrostered(&spec.module_id);
                return Err(err);
            }
        };
        self.mark_first_process_running(&spec, &runtime, &snapshot, &child)?;

        Ok(self.supervised_module(spec, runtime, snapshot, Some(child)))
    }

    /// Make `spec`'s module count as configured, with its identity gates
    /// (reserved id, reserved prefixes) in place, BEFORE any process of it
    /// exists.
    ///
    /// Every path that takes on a new module calls this before `spawn_child`.
    /// The order is the point: the child can connect, register, sync its
    /// scopes and ask about them as soon as it is spawned, and the module is
    /// only put on the roster after `spawn_child` returns. Were the mark set
    /// with the roster entry, a fast child would see its own owner reported
    /// as not configured, and a scoped `route.open` in that window would be
    /// refused as terminal `scope_not_live` ("will never sync") instead of
    /// retryable `scope_not_synced`.
    fn establish_identity(&self, spec: &ModuleSpec) {
        if let Some(supervisor_handle) = &self.supervisor_handle {
            supervisor_handle.apply_identity_configuration(spec);
            supervisor_handle.mark_configured(&spec.module_id);
        }
    }

    /// Take back [`Self::establish_identity`]'s configured mark when the
    /// module will not be put on the roster after all.
    fn abandon_unrostered(&self, module_id: &str) {
        if let Some(supervisor_handle) = &self.supervisor_handle {
            supervisor_handle.unmark_configured_unless_rostered(module_id);
        }
    }

    /// Record a freshly spawned first process as running. On failure the
    /// module never reaches the roster, so its configured mark is taken back.
    fn mark_first_process_running(
        &self,
        spec: &ModuleSpec,
        runtime: &SupervisorRuntimeConfig,
        snapshot: &SharedSnapshot,
        child: &SupervisedChild,
    ) -> Result<(), SuperviseError> {
        if let Err(err) = set_running(snapshot, child, &spec.module_id, &runtime.spawn_events) {
            self.abandon_unrostered(&spec.module_id);
            return Err(err);
        }
        self.process_liveness
            .track(spec.module_id.clone(), Arc::clone(snapshot));
        Ok(())
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
        self.establish_identity(&spec);

        let runtime = self.runtime_config();
        if !enabled {
            let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::disabled()));
            return Ok(self.supervised_module(spec, runtime, snapshot, None));
        }

        let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::starting()));
        let spawned = spawn_child(
            &spec,
            runtime.connection_file_path.as_deref(),
            self.supervisor_handle.as_ref(),
            &runtime.stderr_ring,
            runtime.capture_logs_dir.as_deref(),
            &runtime.child_roster,
            #[cfg(target_os = "linux")]
            runtime.cgroup_placement.as_ref(),
        );
        #[cfg(test)]
        self.test_after_first_spawn.run(&spec.module_id);
        match spawned {
            Ok(child) => {
                self.mark_first_process_running(&spec, &runtime, &snapshot, &child)?;
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

    /// Supervise a configured module with its own health, drain, and crash
    /// budget. The restart policy is per-module because the config file is:
    /// `modules.<id>.restart` resolves to a full policy at parse time, and a
    /// module that is expensive to restart should not be forced onto the same
    /// budget as one that is cheap.
    pub fn supervise_configured_with_health(
        &self,
        spec: ModuleSpec,
        enabled: bool,
        health: HealthConfig,
        drain_timeout_ms: Option<u64>,
        restart_policy: RestartPolicy,
    ) -> Result<SupervisedModule, SuperviseError> {
        validate_spec(&spec)?;
        self.establish_identity(&spec);

        let mut runtime = self.runtime_config();
        runtime.health = health.clone();
        runtime.restart_policy = restart_policy;
        if let Some(ms) = drain_timeout_ms {
            runtime.drain_timeout = Duration::from_millis(ms);
            *runtime
                .effective_drain_timeout
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = runtime.drain_timeout;
        }
        if !enabled {
            let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::disabled()));
            return Ok(self.supervised_module(spec, runtime, snapshot, None));
        }

        let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::starting()));
        let spawned = spawn_child(
            &spec,
            runtime.connection_file_path.as_deref(),
            self.supervisor_handle.as_ref(),
            &runtime.stderr_ring,
            runtime.capture_logs_dir.as_deref(),
            &runtime.child_roster,
            #[cfg(target_os = "linux")]
            runtime.cgroup_placement.as_ref(),
        );
        #[cfg(test)]
        self.test_after_first_spawn.run(&spec.module_id);
        match spawned {
            Ok(child) => {
                self.mark_first_process_running(&spec, &runtime, &snapshot, &child)?;
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
        let effective_drain_timeout = Arc::new(Mutex::new(self.drain_timeout));
        SupervisorRuntimeConfig {
            scheduled_respawn: Arc::default(),
            deferred_reload_reply: Arc::default(),
            restart_policy: self.restart_policy,
            drain_timeout: self.drain_timeout,
            // Shared with this module's roster copy: daemon shutdown waits on
            // each child for the module's own drain budget, as resolved now.
            child_roster: self
                .child_roster
                .for_module(Arc::clone(&effective_drain_timeout)),
            effective_drain_timeout,
            default_drain_timeout: self.drain_timeout,
            health: self.health.clone(),
            connection_file_path: self.connection_file_path.clone(),
            capture_logs_dir: self.capture_logs_dir.clone(),
            forwarding: self.forwarding.clone(),
            supervisor_handle: self.supervisor_handle.clone(),
            stderr_ring: Arc::new(Mutex::new(StderrRing::new(StderrTailConfig::default()))),
            terminal_ring: Arc::new(Mutex::new(
                TerminalRing::new(
                    TerminalRingConfig::default(),
                    self.daemon_start_clock.started_at_ms(),
                )
                .with_start_clock(self.daemon_start_clock)
                .with_journal(self.terminal_journal.clone())
                .with_daemon_shutdown(self.child_roster.shutdown_flag()),
            )),
            spawn_events: self.spawn_events.clone(),
            #[cfg(target_os = "linux")]
            cgroup_placement: self.cgroup_placement.clone(),
            #[cfg(test)]
            test_seed_stale_facts_before_enable_spawn: false,
            #[cfg(test)]
            test_reload_exit_record_gate: None,
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
            health: runtime.health.clone(),
        }));
        let stderr_ring = Arc::clone(&runtime.stderr_ring);
        let terminal_ring = Arc::clone(&runtime.terminal_ring);
        // The module's OWN policy, which may be its per-module config rather than
        // the supervisor-wide one; status must report the budget the supervise
        // loop actually enforces.
        let restart_policy = runtime.restart_policy;
        let effective_drain_timeout = Arc::clone(&runtime.effective_drain_timeout);
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
                restart_policy,
                effective_drain_timeout,
                provenance_probe: self.provenance_probe.clone(),
            }),
        };
        // The identity gates and the configured mark were set by
        // `establish_identity` before any process was spawned; only the roster
        // entry waits for the module handle, which needs the spawned child.
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
    configuration: Arc<Mutex<SupervisedConfiguration>>,
    stderr_ring: Arc<Mutex<StderrRing>>,
    terminal_ring: Arc<Mutex<TerminalRing>>,
    commands: mpsc::Sender<SupervisorCommand>,
    monitor: Mutex<Option<JoinHandle<()>>>,
    /// Copied from the supervisor's runtime config at spawn so `status()` can
    /// report the restart budget without reaching back into the supervisor. The
    /// policy is fixed for the process's lifetime, so a copy cannot drift.
    restart_policy: RestartPolicy,
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

    /// Retained observations from the current ring and all journal generations.
    ///
    /// Blocking: this reads the journal files. Async callers use
    /// [`Self::read_durable_terminal_history`].
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

    pub(crate) fn record_deliberate_severance(
        &self,
        identity: ProcessIdentity,
    ) -> Result<bool, SuperviseError> {
        let mut snapshot = lock_snapshot(&self.inner.snapshot)?;
        if snapshot.pid != Some(identity.pid)
            || snapshot.process_start_time != Some(identity.start_time)
        {
            return Ok(false);
        }
        snapshot.deliberate_severance = Some(identity);
        Ok(true)
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
        let mut guard = match caller {
            Some(caller) => lock_snapshot_for_control(snapshot, &self.inner.module_id, caller)?,
            None => lock_snapshot(snapshot)?,
        };
        // Read the budget through the pruning path so a reader sees the same
        // in-window count the restart decision would use, not a stale total.
        let restart_count =
            guard.crash_restarts_in_window(self.inner.restart_policy.window, Instant::now());
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

        Ok((
            ModuleStatus {
                module_id: self.inner.module_id.clone(),
                state: snapshot.state,
                enabled: snapshot.enabled,
                process_alive: snapshot.process_alive,
                registration_active,
                protocol,
                live,
                restart_count,
                lifetime_restarts: snapshot.lifetime_restarts,
                spawn_generation: snapshot.spawn_generation,
                max_restarts: self.inner.restart_policy.max_restarts,
                restart_window: self.inner.restart_policy.window,
                drain_timeout,
                restart_backoff: self.inner.restart_policy.backoff,
                restart_max_backoff: self.inner.restart_policy.max_backoff,
                pid: snapshot.reported_pid(),
                spawned_at_ms: snapshot.spawned_at_ms,
                spawned_from: snapshot.spawned_from,
                process_start_time: snapshot.process_start_time,
                last_exit: snapshot.last_exit,
                health: snapshot.health,
            },
            snapshot.spawned_file_identity,
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
                snapshot.spawned_file_identity,
                snapshot.process_start_time,
            )
            .await
    }

    /// Memory and CPU time of the module's current process, read now. Only the
    /// process the supervisor spawned is read, not processes it has started.
    pub(crate) fn child_resource_usage(&self) -> subc_control::ChildResourceUsage {
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

    pub(crate) fn will_recover_after_connection_loss(&self) -> Result<bool, SuperviseError> {
        let mut snapshot = lock_snapshot(&self.inner.snapshot)?;
        Ok(match snapshot.state {
            ModuleState::Restarting => true,
            ModuleState::Failed | ModuleState::Disabled => false,
            _ => daemon_will_restart(&mut snapshot, &self.inner.restart_policy, Instant::now()),
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
        let received_at_generation = lock_snapshot(&self.inner.snapshot)?.spawn_generation;
        let (reply_tx, reply_rx) = oneshot::channel();
        self.inner
            .commands
            .send(SupervisorCommand::Restart {
                drain_timeout_ms,
                received_at_generation,
                queued_at: Instant::now(),
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

    /// Blue/green restart: see [`SupervisorCommand::Swap`] and the
    /// `supervisor_swap` module. Returns once the swap has cut over (the old
    /// process then drains in the background of the supervise loop) or has
    /// failed, leaving the old process serving.
    pub async fn swap(&self, ready_timeout: Option<Duration>) -> Result<(), SuperviseError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.inner
            .commands
            .send(SupervisorCommand::Swap {
                ready_timeout,
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
        /// The module's `spawn_generation` when the request was received, before
        /// it waited in the command queue. A queued restart whose module has
        /// since spawned a newer process is already satisfied (see the handler).
        received_at_generation: u64,
        /// When the request entered the command queue, so the handler can log
        /// how long it waited behind the loop's other work.
        queued_at: Instant,
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
    Swap {
        /// How long the candidate may take to register and declare itself
        /// ready. `None` uses [`DEFAULT_SWAP_READY_TIMEOUT`].
        ready_timeout: Option<Duration>,
        /// Answered at cutover or failure; the incumbent's drain follows.
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
        cgroup_path: Option<PathBuf>,
    },
    Cgroup {
        module_id: String,
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

impl fmt::Display for SuperviseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
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
            | Self::InvalidSpec { .. }
            | Self::ReloadUnavailable { .. }
            | Self::Disabled { .. }
            | Self::ReloadFailed { .. }
            | Self::RegistrationStillActive { .. }
            | Self::StatePoisoned { .. }
            | Self::CommandClosed { .. }
            | Self::SwapInProgress { .. }
            | Self::SwapRefused { .. }
            | Self::SwapFailed { .. } => None,
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
    configured_health: Option<HealthConfig>,
    registered_connection: Option<crate::ConnectionId>,
    advertised: bool,
    next_probe_at: Option<Instant>,
    probe_index: u64,
}

fn running_protocol(spec: &ModuleSpec, snapshot: &SharedSnapshot) -> ModuleProtocol {
    lock_snapshot(snapshot)
        .ok()
        .and_then(|state| state.spawned_protocol)
        .unwrap_or(spec.protocol)
}

impl HealthProbeRuntime {
    fn refresh_registration(
        &mut self,
        spec: &ModuleSpec,
        runtime: &SupervisorRuntimeConfig,
        registry: &Registry,
        snapshot: &SharedSnapshot,
    ) {
        if self.configured_health.as_ref() != Some(&runtime.health) {
            self.configured_health = Some(runtime.health.clone());
            self.next_probe_at = None;
            self.registered_connection = None;
            self.probe_index = 0;
        }
        // A non-wire process never registers. Only an explicitly configured
        // HTTP endpoint can arm its health probe; an absent HELLO is not a
        // health failure for that kind of process.
        if running_protocol(spec, snapshot) == ModuleProtocol::None {
            self.registered_connection = None;
            self.advertised = runtime.health.http.is_some();
            if !self.advertised {
                self.next_probe_at = None;
                let _ = update_snapshot(snapshot, Some(&spec.module_id), |state| {
                    state.health = ModuleHealthStatus::default();
                });
            } else if self.next_probe_at.is_none() {
                self.next_probe_at = Some(
                    Instant::now()
                        + jittered_health_delay(&spec.module_id, 0, runtime.health.cadence),
                );
            }
            return;
        }

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
    let http = (running_protocol(spec, snapshot) == ModuleProtocol::None)
        .then_some(runtime.health.http.as_deref())
        .flatten();
    let result = match http {
        Some(url) => probe_http_health(url, runtime.health.deadline).await,
        None => probe_module_health(&spec.module_id, runtime, None).await,
    };
    match result {
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
            if http.is_some() {
                let _ = update_snapshot(snapshot, Some(&spec.module_id), |state| {
                    state.health.status = SupervisorHealthStatus::Failing;
                });
            }
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

pub(crate) struct HttpProbeTarget<'a> {
    address: std::net::SocketAddr,
    localhost: bool,
    authority: &'a str,
    path: String,
}

/// Resolve only literal loopback endpoints, without DNS, redirects, proxies,
/// or TLS. A URL cannot turn a local health check into an outbound connection.
pub(crate) fn parse_http_probe_url(url: &str) -> Result<HttpProbeTarget<'_>, String> {
    if url.bytes().any(|b| b <= b' ' || b == 127) || url.contains('#') {
        return Err("must not contain whitespace, controls, or a fragment".into());
    }
    let rest = url
        .strip_prefix("http://")
        .ok_or("must use plain http://")?;
    let split = rest.find(['/', '?']).unwrap_or(rest.len());
    let (authority, suffix) = rest.split_at(split);
    let (host, port) = if let Some(rest) = authority.strip_prefix("[::1]") {
        ("::1", rest)
    } else {
        let split = authority.find(':').unwrap_or(authority.len());
        authority.split_at(split)
    };
    let ip: std::net::IpAddr = match host {
        "127.0.0.1" | "localhost" => std::net::Ipv4Addr::LOCALHOST.into(),
        "::1" => std::net::Ipv6Addr::LOCALHOST.into(),
        _ => return Err("host must be 127.0.0.1, [::1], or localhost".into()),
    };
    let port = if port.is_empty() {
        80
    } else {
        port.strip_prefix(':')
            .and_then(|p| p.parse::<u16>().ok())
            .filter(|p| *p > 0)
            .ok_or("must have a valid nonzero TCP port")?
    };
    let path = if suffix.is_empty() {
        "/".into()
    } else if suffix.starts_with('?') {
        format!("/{suffix}")
    } else {
        suffix.into()
    };
    Ok(HttpProbeTarget {
        address: std::net::SocketAddr::new(ip, port),
        localhost: host == "localhost",
        authority,
        path,
    })
}

async fn probe_http_health(
    url: &str,
    deadline: Duration,
) -> Result<HealthReport, HealthProbeError> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
    let target = parse_http_probe_url(url).map_err(HealthProbeError::misconfigured)?;
    // Keep partial diagnostics outside the timed future so cancellation does
    // not discard a status line or body bytes already received.
    let mut response_status = String::new();
    let mut body = Vec::new();
    let probe = async {
        // Resolve localhost ourselves so a hosts-file override cannot turn
        // this into an outbound request, while IPv6-only local servers work.
        let connection = match tokio::net::TcpStream::connect(target.address).await {
            Err(_) if target.localhost => {
                tokio::net::TcpStream::connect((
                    std::net::Ipv6Addr::LOCALHOST,
                    target.address.port(),
                ))
                .await
            }
            result => result,
        };
        let mut stream = connection.map_err(|error| {
            HealthProbeError::no_answer(format!("HTTP connect failed: {error}"))
        })?;
        stream.write_all(format!("GET {} HTTP/1.1\r\nHost: {}\r\nAccept-Encoding: identity\r\nConnection: close\r\n\r\n", target.path, target.authority).as_bytes()).await
            .map_err(|error| HealthProbeError::no_answer(format!("HTTP write failed: {error}")))?;
        let mut reader = BufReader::new(stream);
        let mut budget = 16 * 1024;
        let status = http_line(&mut reader, &mut budget).await?;
        let mut words = status.split_ascii_whitespace();
        let version = words.next();
        let code = words
            .next()
            .filter(|word| word.len() == 3)
            .and_then(|word| word.parse::<u16>().ok());
        if !matches!(version, Some("HTTP/1.1" | "HTTP/1.0"))
            || !code.is_some_and(|code| (100..600).contains(&code))
        {
            return Err(HealthProbeError::bad_answer(format!(
                "invalid HTTP status: {status}"
            )));
        }
        let code = code.expect("validated status code");
        response_status = status.clone();
        let mut length = None;
        let mut chunked = false;
        loop {
            let line = http_line(&mut reader, &mut budget).await?;
            if line.is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':') {
                if name.eq_ignore_ascii_case("content-length") {
                    length = Some(value.trim().parse::<u64>().map_err(|_| {
                        HealthProbeError::bad_answer("invalid HTTP Content-Length")
                    })?);
                } else if name.eq_ignore_ascii_case("transfer-encoding") {
                    chunked = value.trim().eq_ignore_ascii_case("chunked");
                }
            }
        }
        if chunked {
            while body.len() < 200 {
                let line = http_line(&mut reader, &mut budget).await?;
                let size = u64::from_str_radix(line.split(';').next().unwrap_or("").trim(), 16)
                    .map_err(|_| HealthProbeError::bad_answer("invalid HTTP chunk size"))?;
                if size == 0 {
                    break;
                }
                let count = size.min((200 - body.len()) as u64) as usize;
                let start = body.len();
                (&mut reader)
                    .take(count as u64)
                    .read_to_end(&mut body)
                    .await
                    .map_err(|error| {
                        HealthProbeError::no_answer(format!("HTTP body read failed: {error}"))
                    })?;
                if body.len() - start != count {
                    return Err(HealthProbeError::bad_answer("truncated HTTP chunk"));
                }
                if size > count as u64 || body.len() == 200 {
                    break;
                }
                if !http_line(&mut reader, &mut budget).await?.is_empty() {
                    return Err(HealthProbeError::bad_answer("invalid HTTP chunk delimiter"));
                }
            }
        } else {
            reader
                .take(length.unwrap_or(200).min(200))
                .read_to_end(&mut body)
                .await
                .map_err(|error| {
                    HealthProbeError::no_answer(format!("HTTP body read failed: {error}"))
                })?;
        }
        if (200..300).contains(&code) {
            Ok(HealthReport::ok())
        } else {
            Err(HealthProbeError::bad_answer(
                "HTTP health endpoint returned non-2xx",
            ))
        }
    };
    let mut result = timeout(deadline, probe).await.unwrap_or_else(|_| {
        Err(HealthProbeError::no_answer(format!(
            "HTTP probe timed out after {deadline:?}"
        )))
    });
    if let Err(error) = &mut result {
        if !response_status.is_empty() {
            error.message = format!(
                "{}; {response_status}: {}",
                error.message,
                String::from_utf8_lossy(&body)
            );
        }
    }
    result
}

async fn http_line(
    reader: &mut tokio::io::BufReader<tokio::net::TcpStream>,
    remaining: &mut usize,
) -> Result<String, HealthProbeError> {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt};
    let mut line = Vec::new();
    (&mut *reader)
        .take(*remaining as u64)
        .read_until(b'\n', &mut line)
        .await
        .map_err(|error| {
            HealthProbeError::no_answer(format!("HTTP header read failed: {error}"))
        })?;
    *remaining -= line.len();
    if !line.ends_with(b"\r\n") {
        return Err(HealthProbeError::bad_answer(
            "HTTP headers are incomplete or exceed 16 KiB",
        ));
    }
    line.truncate(line.len() - 2);
    String::from_utf8(line).map_err(|_| HealthProbeError::bad_answer("HTTP header is not UTF-8"))
}

async fn probe_module_health(
    module_id: &str,
    runtime: &SupervisorRuntimeConfig,
    drain_deadline: Option<Instant>,
) -> Result<HealthReport, HealthProbeError> {
    let Some(forwarding) = runtime.forwarding.as_ref() else {
        return Err(HealthProbeError::misconfigured(
            "supervisor was not configured with a forwarding table",
        ));
    };
    let probe_started_at = Instant::now();
    let mut deadline = probe_started_at + runtime.health.deadline;
    if let Some(drain_deadline) = drain_deadline {
        deadline = deadline.min(drain_deadline);
    }
    let pending = if drain_deadline.is_some() {
        forwarding.begin_drain_health_probe_rpc_for(
            module_id,
            MODULE_CONTROL_OP_HEALTH_CHECK,
            probe_started_at,
            deadline,
        )
    } else {
        forwarding.begin_health_probe_rpc_for(
            module_id,
            MODULE_CONTROL_OP_HEALTH_CHECK,
            probe_started_at,
            deadline,
        )
    }
    .map_err(|err| {
        // The endpoint is not registered, so there is no live control lane to
        // ask. That is the module being absent, not slow.
        HealthProbeError::lane_dead(format!("failed to begin health.check RPC: {err}"))
    })?;
    await_health_probe(forwarding, pending, deadline, runtime.health.deadline).await
}

/// [`probe_module_health`] for one endpoint rather than the id's active one.
///
/// A swap probes two processes that no by-id lookup reaches: its candidate
/// before cutover, and its superseded incumbent (for busy gauges) while the
/// incumbent drains. `deadline_cap` bounds the probe the way a drain deadline
/// bounds the by-id drain probe.
async fn probe_endpoint_health(
    endpoint: crate::ModuleEndpointId,
    runtime: &SupervisorRuntimeConfig,
    deadline_cap: Option<Instant>,
) -> Result<HealthReport, HealthProbeError> {
    let Some(forwarding) = runtime.forwarding.as_ref() else {
        return Err(HealthProbeError::misconfigured(
            "supervisor was not configured with a forwarding table",
        ));
    };
    let probe_started_at = Instant::now();
    let mut deadline = probe_started_at + runtime.health.deadline;
    if let Some(cap) = deadline_cap {
        deadline = deadline.min(cap);
    }
    let pending = forwarding
        .begin_endpoint_health_probe_rpc_for(
            endpoint,
            MODULE_CONTROL_OP_HEALTH_CHECK,
            probe_started_at,
            deadline,
        )
        .map_err(|err| {
            HealthProbeError::lane_dead(format!("failed to begin health.check RPC: {err}"))
        })?;
    await_health_probe(forwarding, pending, deadline, runtime.health.deadline).await
}

/// Send a begun health probe and classify its answer.
async fn await_health_probe(
    forwarding: &ForwardingTable,
    pending: PendingModuleControlRpc,
    deadline: Instant,
    probe_budget: Duration,
) -> Result<HealthReport, HealthProbeError> {
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
                "module did not answer health.check within {probe_budget:?}"
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
        // A failed wire probe invalidates the last report, even before the
        // restart threshold. HTTP probes already mark failures as Failing.
        if state.spawned_protocol.unwrap_or(spec.protocol) == ModuleProtocol::Subc {
            state.health.status = SupervisorHealthStatus::Unknown;
        }
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
    let (enabled, schedule) = {
        let mut state = lock_snapshot(snapshot)?;
        let enabled = state.enabled;
        let schedule = if enabled {
            state.next_crash_restart(&runtime.restart_policy, Instant::now())
        } else {
            None
        };
        (enabled, schedule)
    };

    if !enabled {
        return Err(SuperviseError::Disabled {
            module_id: spec.module_id.clone(),
        });
    }

    if schedule.is_none() {
        record_health_action(snapshot, &spec.module_id, "failed".to_string(), now_ms);
        error!(
            module_id = %spec.module_id,
            status = ?status,
            detail,
            max_restarts = runtime.restart_policy.max_restarts,
            window_secs = runtime.restart_policy.window.as_secs(),
            reason = %runtime.restart_policy.budget_exhausted_detail(),
            "health restart budget exhausted; marking module failed"
        );
        let stop_notice = begin_forwarding_drain_if_configured(
            spec,
            runtime,
            registry,
            snapshot,
            Some(true),
            RouteCloseReason::Disable,
        )
        .await?;
        update_snapshot(snapshot, Some(&spec.module_id), |state| {
            state.drain_disposition_detail = Some(runtime.restart_policy.budget_exhausted_detail());
        })?;
        drain_optional_child(
            &spec.module_id,
            spec.protocol,
            stop_notice,
            registry,
            runtime.forwarding.as_deref(),
            snapshot,
            &runtime.terminal_ring,
            &runtime.spawn_events,
            child,
            runtime.drain_timeout,
            ModuleState::Failed,
            Some(true),
        )
        .await?;
        process_liveness.untrack_if_current(&spec.module_id, snapshot);
        return Ok(());
    }

    let schedule = schedule.expect("a health restart must have a crash-restart schedule");
    let mut restart_count = 0;
    update_snapshot(snapshot, Some(&spec.module_id), |state| {
        restart_count = state.crash_restarts.len();
        state.state = ModuleState::Unresponsive;
        state.health.status = status;
        state.health.last_action = Some(HealthAction::Restart.to_string());
        state.health.last_action_ms = Some(now_ms);
    })?;
    warn!(
        module_id = %spec.module_id,
        status = ?status,
        detail,
        restart_count,
        restart_in_window = schedule.restart_in_window,
        delay_ms = schedule.delay.as_millis() as u64,
        "health-triggered module restart"
    );

    let stop_notice = begin_forwarding_drain_if_configured(
        spec,
        runtime,
        registry,
        snapshot,
        Some(true),
        RouteCloseReason::Restart,
    )
    .await?;
    drain_optional_child(
        &spec.module_id,
        spec.protocol,
        stop_notice,
        registry,
        runtime.forwarding.as_deref(),
        snapshot,
        &runtime.terminal_ring,
        &runtime.spawn_events,
        child,
        runtime.drain_timeout,
        ModuleState::Restarting,
        Some(true),
    )
    .await?;
    schedule_respawn(
        runtime,
        snapshot,
        &spec.module_id,
        schedule.delay,
        RespawnKind::Spawn,
    )
}

fn cancel_deferred_reload(runtime: &SupervisorRuntimeConfig, module_id: &str, reason: &str) {
    if let Some(reply) = runtime
        .deferred_reload_reply
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .take()
    {
        let _ = reply.send(Err(SuperviseError::ReloadFailed {
            module_id: module_id.to_string(),
            reason: reason.to_string(),
        }));
    }
}

fn schedule_respawn(
    runtime: &SupervisorRuntimeConfig,
    snapshot: &SharedSnapshot,
    module_id: &str,
    delay: Duration,
    kind: RespawnKind,
) -> Result<(), SuperviseError> {
    cancel_deferred_reload(runtime, module_id, "respawn superseded by another restart");
    update_snapshot(snapshot, Some(module_id), |state| {
        state.respawn_pending = true
    })?;
    *runtime
        .scheduled_respawn
        .lock()
        .unwrap_or_else(|p| p.into_inner()) = Some(PendingRespawn {
        deadline: Instant::now() + delay,
        kind,
    });
    Ok(())
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
            protocol: ModuleProtocol::Subc,
            overlap: Default::default(),
        });

        assert!(
            handle.removal_tombstone_age_ms(module_id).is_none(),
            "a re-added module must not retain a stale removal tombstone"
        );
    }

    /// What one module's owner looked like from the control plane at the
    /// instant after its first process was spawned.
    #[derive(Debug, PartialEq, Eq)]
    struct OwnerInSpawnWindow {
        module_id: String,
        configured: bool,
        on_roster: bool,
        admission_refusal: Option<&'static str>,
    }

    /// A supervised module's process can connect, register, sync its scopes
    /// and describe them as soon as it is spawned, which is BEFORE the
    /// supervisor puts the module on the roster. In that window the owner must
    /// already read as configured, so a scoped `route.open` against it is
    /// refused as retryable `scope_not_synced` and not as terminal
    /// `scope_not_live` ("will never sync").
    ///
    /// The hook runs in exactly that window on every path that takes on a new
    /// module, so no race with a real child is needed: `on_roster: false`
    /// proves each observation was taken before the roster insert.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_new_module_is_configured_before_its_first_process_can_register() {
        use crate::scopes::ScopeTable;
        use subc_protocol::{error_codes, scope::ScopeSelector, Principal};

        let dir = subc_test_support::TestTempDir::new("configured-before-spawn");
        let stub = |module_id: &str, program: PathBuf| ModuleSpec {
            module_id: module_id.to_string(),
            program,
            args: Vec::new(),
            env: ["XDG_DATA_HOME", "XDG_RUNTIME_DIR", "XDG_CONFIG_HOME"]
                .into_iter()
                .map(|key| (key.to_string(), dir.path().display().to_string()))
                .chain([("FAKE_AFT_NEVER_CONNECT".to_string(), "1".to_string())])
                .collect(),
            reserved: false,
            reserved_prefixes: Vec::new(),
            protocol: ModuleProtocol::Subc,
            overlap: Default::default(),
        };
        let live = super::terminal_history_tests::fake_aft_stub_path();
        let missing = dir.path().join("definitely-missing-module");

        let handle = SupervisorHandle::new();
        let mut supervisor =
            Supervisor::new_for_test(Arc::new(Registry::default()), RestartPolicy::default())
                .with_handle(handle.clone());
        let observed = Arc::new(Mutex::new(Vec::<OwnerInSpawnWindow>::new()));
        let hook_handle = handle.clone();
        let hook_observed = Arc::clone(&observed);
        supervisor.test_after_first_spawn = AfterFirstSpawnHook(Some(Arc::new(move |module_id| {
            // Exactly what the control plane computes for a scoped route.open
            // naming this module as the owner of a scope it has not synced.
            let configured = hook_handle.is_configured(module_id);
            let selector = ScopeSelector {
                owner: Principal::Reserved {
                    module_id: module_id.to_string(),
                },
                scope_ref: "s".to_string(),
                scope_epoch: Some(1),
            };
            let carrier = Principal::Reserved {
                module_id: "carrier".to_string(),
            };
            let admission_refusal = match ScopeTable::new(Vec::<String>::new())
                .admit(&carrier, module_id, &selector, configured)
            {
                Ok(_) => None,
                Err(refusal) => Some(refusal.code),
            };
            hook_observed.lock().unwrap().push(OwnerInSpawnWindow {
                module_id: module_id.to_string(),
                configured,
                on_roster: hook_handle.get(module_id).is_some(),
                admission_refusal,
            });
        })));

        let plain = supervisor.spawn(stub("plain", live.clone())).unwrap();
        let configured = supervisor
            .supervise_configured(stub("configured", live.clone()), true)
            .unwrap();
        let with_health = supervisor
            .supervise_configured_with_health(
                stub("with-health", live.clone()),
                true,
                HealthConfig::default(),
                None,
                RestartPolicy::default(),
            )
            .unwrap();
        // The failed-spawn path still puts the module on the roster (as
        // failed), so it is configured throughout.
        let failed = supervisor
            .supervise_configured_with_health(
                stub("failed-spawn", missing.clone()),
                true,
                HealthConfig::default(),
                None,
                RestartPolicy::default(),
            )
            .unwrap();
        // A failed plain `spawn` puts nothing on the roster, so its mark is
        // taken back once the spawn has failed.
        assert!(supervisor.spawn(stub("spawn-error", missing)).is_err());

        let expected = [
            "plain",
            "configured",
            "with-health",
            "failed-spawn",
            "spawn-error",
        ]
        .into_iter()
        .map(|module_id| OwnerInSpawnWindow {
            module_id: module_id.to_string(),
            configured: true,
            on_roster: false,
            admission_refusal: Some(error_codes::SCOPE_NOT_SYNCED),
        })
        .collect::<Vec<_>>();
        assert_eq!(*observed.lock().unwrap(), expected);

        for module_id in ["plain", "configured", "with-health", "failed-spawn"] {
            assert!(
                handle.get(module_id).is_some(),
                "{module_id} is on the roster"
            );
            assert!(
                handle.is_configured(module_id),
                "{module_id} stays configured"
            );
        }
        assert!(handle.get("spawn-error").is_none());
        assert!(
            !handle.is_configured("spawn-error"),
            "a plain spawn that failed must not leave its module marked configured"
        );

        // Leaving the roster clears the mark with it.
        handle.retire("failed-spawn");
        assert!(!handle.is_configured("failed-spawn"));

        for module in [plain, configured, with_health] {
            module.stop().await.unwrap();
        }
        drop(failed);
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
        let supervisor =
            Supervisor::new_for_test(Arc::new(Registry::default()), RestartPolicy::default());
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
            protocol: ModuleProtocol::Subc,
            overlap: Default::default(),
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

    #[tokio::test]
    async fn start_revives_stranded_restarting_but_not_pending_backoff() {
        let supervisor =
            Supervisor::new_for_test(Arc::new(Registry::default()), RestartPolicy::default());
        let runtime = supervisor.runtime_config();
        let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::new(
            ModuleState::Restarting,
            true,
        )));
        let spec = ModuleSpec {
            module_id: "start-stranded-restarting".to_string(),
            program: super::terminal_history_tests::fake_aft_stub_path(),
            args: Vec::new(),
            env: vec![("FAKE_AFT_NEVER_CONNECT".to_string(), "1".to_string())],
            reserved: false,
            reserved_prefixes: Vec::new(),
            protocol: ModuleProtocol::None,
            overlap: Default::default(),
        };
        let mut child = None;
        lock_snapshot(&snapshot).unwrap().respawn_pending = true;
        assert!(!super::set_child_enabled(
            &spec,
            &runtime,
            &Registry::default(),
            &supervisor.process_liveness,
            &snapshot,
            &mut child,
            true
        )
        .await
        .unwrap());
        assert!(child.is_none());
        lock_snapshot(&snapshot).unwrap().respawn_pending = false;
        assert!(super::set_child_enabled(
            &spec,
            &runtime,
            &Registry::default(),
            &supervisor.process_liveness,
            &snapshot,
            &mut child,
            true
        )
        .await
        .unwrap());
        assert_eq!(
            lock_snapshot(&snapshot).unwrap().state,
            ModuleState::Running
        );
        let mut child = child.unwrap();
        child.start_kill().unwrap();
        child.wait().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn failed_reload_spawn_clears_current_process_facts() {
        let supervisor =
            Supervisor::new_for_test(Arc::new(Registry::default()), RestartPolicy::default());
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
            protocol: ModuleProtocol::Subc,
            overlap: Default::default(),
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
        let supervisor =
            Supervisor::new_for_test(Arc::new(Registry::default()), RestartPolicy::default());
        let snapshot = stale_process_snapshot(ModuleState::Running, true);
        let module = supervisor.supervised_module(
            ModuleSpec {
                module_id: "drop-clears-facts".to_string(),
                program: PathBuf::from("/unused/drop-module"),
                args: Vec::new(),
                env: Vec::new(),
                reserved: false,
                reserved_prefixes: Vec::new(),
                protocol: ModuleProtocol::Subc,
                overlap: Default::default(),
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

    #[cfg(unix)]
    #[tokio::test]
    async fn rescan_preserves_running_protocol_until_respawn() {
        let dir = subc_test_support::TestTempDir::new("rescan-protocol");
        let initial = ModuleSpec {
            module_id: "rescan-protocol".into(),
            program: PathBuf::from("/bin/sleep"),
            args: vec!["60".into()],
            env: ["XDG_DATA_HOME", "XDG_RUNTIME_DIR", "XDG_CONFIG_HOME"]
                .into_iter()
                .map(|key| (key.into(), dir.path().to_string_lossy().into_owned()))
                .collect(),
            reserved: false,
            reserved_prefixes: vec![],
            protocol: ModuleProtocol::None,
            overlap: Default::default(),
        };
        let supervisor =
            Supervisor::new_for_test(Arc::new(Registry::default()), RestartPolicy::default());
        let module = supervisor.spawn(initial.clone()).unwrap();
        assert!(module.status().unwrap().live);
        let mut next = initial;
        next.protocol = ModuleProtocol::Subc;
        module
            .update_configuration(next.clone(), HealthConfig::default(), None)
            .await
            .unwrap();
        assert!(
            module.status().unwrap().live,
            "rescan must not require HELLO from the old non-wire process"
        );
        let runtime = supervisor.runtime_config();
        let action = on_child_exit(
            &next,
            RestartPolicy::default(),
            &supervisor.registry,
            &module.inner.snapshot,
            &runtime.terminal_ring,
            &runtime.spawn_events,
            &runtime.child_roster,
            ExitReport {
                kind: ExitKind::Clean,
                code: Some(0),
                signal: None,
                at_ms: unix_ms_now(),
            },
        )
        .await;
        assert!(
            matches!(action, NextAction::Restart { .. }),
            "the old non-wire process's clean exit must restart"
        );
        module.drain().await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn orphan_identity_matches_path_names_and_shebang_interpreters() {
        use std::os::unix::fs::PermissionsExt;
        let dir = subc_test_support::TestTempDir::new("spawn-image-identity");
        let script = dir.join("module.sh");
        std::fs::write(&script, "#!/bin/sh\nwhile :; do sleep 0.1; done\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let record_path = dir.join("live-children.json");
        let supervisor =
            Supervisor::new_for_test(Arc::new(Registry::default()), RestartPolicy::default())
                .with_live_children_record(&record_path);
        for (program, args) in [
            (PathBuf::from("sleep"), vec!["60".into()]),
            (script, vec![]),
        ] {
            let spec = ModuleSpec {
                module_id: "image-identity".into(),
                program,
                args,
                env: ["XDG_DATA_HOME", "XDG_RUNTIME_DIR", "XDG_CONFIG_HOME"]
                    .into_iter()
                    .map(|key| (key.into(), dir.to_string_lossy().into_owned()))
                    .collect(),
                reserved: false,
                reserved_prefixes: vec![],
                protocol: ModuleProtocol::None,
                overlap: Default::default(),
            };
            let module = supervisor.spawn(spec).unwrap();
            #[cfg(target_os = "macos")]
            {
                // SETEXEC confirmation is asynchronous; the orphan record must
                // identify the final image, never the intermediate trampoline.
                let deadline = Instant::now() + Duration::from_secs(5);
                while crate::live_children::read_record(&record_path)
                    .unwrap()
                    .iter()
                    .all(|entry| entry.executable.is_none())
                {
                    assert!(Instant::now() < deadline, "module image was not confirmed");
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            }
            let entry = crate::live_children::read_record(&record_path)
                .unwrap()
                .pop()
                .unwrap();
            let observed = subc_os::Process::open(entry.pid)
                .unwrap()
                .unwrap()
                .observe()
                .unwrap();
            let verdict = crate::live_children::identity_verdict(&entry, entry.pid, &observed);
            module.drain().await.unwrap();
            assert_eq!(verdict, crate::live_children::IdentityVerdict::Matches);
        }
    }

    #[cfg(unix)]
    fn http_fixture(
        dir: &std::path::Path,
        url: &str,
        threshold: u32,
    ) -> crate::daemon_config::ConfiguredModule {
        let path = dir.join("subc.jsonc");
        std::fs::write(&path, serde_json::json!({"version": 1, "modules": {"http-process": {
            "program": "/bin/sleep", "args": ["60"], "protocol": "none",
            "env": {"XDG_DATA_HOME":dir,"XDG_RUNTIME_DIR":dir,"XDG_CONFIG_HOME":dir},
            "health": {"http":url,"cadence_ms":20,"deadline_ms":200,"failure_threshold":threshold},
            "restart": {"backoff_ms":1,"max_backoff_ms":1}, "drain_timeout_ms":10
        }}}).to_string()).unwrap();
        crate::daemon_config::load(&path)
            .unwrap()
            .unwrap()
            .modules
            .pop()
            .unwrap()
    }

    #[cfg(unix)]
    async fn wait_http_health(module: &SupervisedModule, status: SupervisorHealthStatus) {
        timeout(Duration::from_secs(5), async {
            loop {
                if module.status().unwrap().health.status == status {
                    break;
                }
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "expected {status:?}, got {:?}",
                module.status().unwrap().health
            )
        });
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn http_health_status_flips_ok_failing_ok() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let dir = subc_test_support::TestTempDir::new("http-status-flips");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let status = Arc::new(std::sync::atomic::AtomicU16::new(200));
        let serving_status = status.clone();
        let url = format!("http://{}/healthz", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0u8; 2048];
                let count = stream.read(&mut request).await.unwrap();
                assert!(count > 0, "a probe must send an HTTP request");
                let code = serving_status.load(std::sync::atomic::Ordering::SeqCst);
                let body = if code == 200 {
                    "ready"
                } else {
                    "{\"status\":\"unavailable\",\"error\":\"scratch failure\"}"
                };
                let response = format!("HTTP/1.1 {code} scratch\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });
        let configured = http_fixture(&dir, &url, 1000);
        let module =
            Supervisor::new_for_test(Arc::new(Registry::default()), RestartPolicy::default())
                .supervise_configured_with_health(
                    configured.module_spec(),
                    true,
                    configured.health,
                    configured.drain_timeout_ms,
                    configured.restart,
                )
                .unwrap();
        wait_http_health(&module, SupervisorHealthStatus::Ok).await;
        status.store(503, std::sync::atomic::Ordering::SeqCst);
        wait_http_health(&module, SupervisorHealthStatus::Failing).await;
        assert!(module
            .status()
            .unwrap()
            .health
            .detail
            .unwrap()
            .contains("scratch failure"));
        status.store(200, std::sync::atomic::Ordering::SeqCst);
        wait_http_health(&module, SupervisorHealthStatus::Ok).await;
        assert_eq!(module.status().unwrap().health.consecutive_failures, 0);
        let before = module.status().unwrap();
        let (spec, mut health) = module.configuration().unwrap();
        health.http = None;
        module
            .update_configuration(spec.clone(), health.clone(), Some(10))
            .await
            .unwrap();
        wait_http_health(&module, SupervisorHealthStatus::Unknown).await;
        health.http = Some(url);
        module
            .update_configuration(spec, health, Some(10))
            .await
            .unwrap();
        wait_http_health(&module, SupervisorHealthStatus::Ok).await;
        assert_eq!(
            module.status().unwrap().pid,
            before.pid,
            "changing a probe must apply live, not restart its process"
        );
        let (spec, mut health) = module.configuration().unwrap();
        health.failure_threshold = 2;
        module
            .update_configuration(spec, health, Some(10))
            .await
            .unwrap();
        status.store(503, std::sync::atomic::Ordering::SeqCst);
        timeout(Duration::from_secs(5), async {
            while module.status().unwrap().spawn_generation == before.spawn_generation {
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("sustained HTTP 503 responses must trigger the health restart policy");
        module.drain().await.unwrap();
        server.abort();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn http_health_no_listener_restarts_after_consecutive_failures() {
        let dir = subc_test_support::TestTempDir::new("http-refused");
        let unused = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/healthz", unused.local_addr().unwrap());
        drop(unused);
        let configured = http_fixture(&dir, &url, 2);
        let module =
            Supervisor::new_for_test(Arc::new(Registry::default()), RestartPolicy::default())
                .supervise_configured_with_health(
                    configured.module_spec(),
                    true,
                    configured.health,
                    configured.drain_timeout_ms,
                    configured.restart,
                )
                .unwrap();
        let before = module.status().unwrap().spawn_generation;
        timeout(Duration::from_secs(5), async {
            loop {
                let status = module.status().unwrap();
                if status.spawn_generation > before {
                    assert!(status.lifetime_restarts > 0);
                    break;
                }
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("sustained HTTP refusal must trigger the health restart policy");
        module.drain().await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn http_health_timeout_honours_deadline() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/healthz", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let _held = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
        });
        let error = timeout(
            Duration::from_secs(1),
            probe_http_health(&url, Duration::from_millis(10)),
        )
        .await
        .expect("the probe must enforce its own deadline")
        .unwrap_err();
        server.abort();
        assert!(error.to_string().contains("timed out"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn http_health_localhost_reaches_an_ipv6_only_listener() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("[::1]:0").await.unwrap();
        let url = format!(
            "http://localhost:{}/healthz",
            listener.local_addr().unwrap().port()
        );
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 2048];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                .await
                .unwrap();
        });
        // The deadline only bounds a hang. A probe that never tried the IPv6
        // address would be refused on 127.0.0.1 and fail at once, so a longer
        // deadline does not weaken the assertion; one second timed out under a
        // loaded parallel test run.
        assert_eq!(
            probe_http_health(&url, Duration::from_secs(10))
                .await
                .unwrap()
                .status,
            HealthStatus::Ok
        );
        server.await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn http_health_timeout_keeps_partial_status_and_body() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/healthz", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 2048];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            stream
                .write_all(
                    b"HTTP/1.1 503 Unavailable\r\nContent-Length: 100\r\n\r\npartial diagnostic",
                )
                .await
                .unwrap();
            std::future::pending::<()>().await;
        });
        let error = probe_http_health(&url, Duration::from_secs(1))
            .await
            .unwrap_err()
            .to_string();
        server.abort();
        assert!(
            error.contains("timed out")
                && error.contains("503 Unavailable")
                && error.contains("partial diagnostic"),
            "{error}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn http_health_diagnostic_body_is_bounded_to_200_bytes() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/healthz", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 2048];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            let body = format!("{}not-in-diagnostic", "x".repeat(400));
            let response = format!(
                "HTTP/1.1 503 Unavailable\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        let error = probe_http_health(&url, Duration::from_secs(1))
            .await
            .unwrap_err()
            .to_string();
        server.await.unwrap();
        assert!(error.ends_with(&"x".repeat(200)), "{error}");
        assert_eq!(error.split(": ").last().unwrap().len(), 200);
        assert!(!error.contains("not-in-diagnostic"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn http_health_real_nats_server_monitoring() {
        if std::process::Command::new("nats-server")
            .arg("--version")
            .env("XDG_DATA_HOME", std::env::temp_dir())
            .env("XDG_RUNTIME_DIR", std::env::temp_dir())
            .env("XDG_CONFIG_HOME", std::env::temp_dir())
            .output()
            .is_err()
        {
            eprintln!(
                "skipped: http_health_real_nats_server_monitoring (nats-server is not on PATH)"
            );
            return;
        }
        let dir = subc_test_support::TestTempDir::new("nats-http-monitoring");
        let monitor = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = monitor.local_addr().unwrap().port();
        let client = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client_port = client.local_addr().unwrap().port();
        let config = dir.join("server.conf");
        std::fs::write(&config, format!("listen: 127.0.0.1:{client_port}\nhttp: 127.0.0.1:{port}\njetstream {{ store_dir: \"{}\" }}\n", dir.join("jetstream").display())).unwrap();
        let mut configured = http_fixture(&dir, &format!("http://127.0.0.1:{port}/healthz"), 1000);
        configured.program = PathBuf::from("nats-server");
        configured.args = vec!["-c".into(), config.to_string_lossy().into_owned()];
        drop(monitor);
        drop(client);
        let module =
            Supervisor::new_for_test(Arc::new(Registry::default()), RestartPolicy::default())
                .supervise_configured_with_health(
                    configured.module_spec(),
                    true,
                    configured.health,
                    configured.drain_timeout_ms,
                    configured.restart,
                )
                .unwrap();
        wait_http_health(&module, SupervisorHealthStatus::Ok).await;
        module.drain().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn configuration_update_does_not_replace_captured_running_process_facts() {
        let supervisor =
            Supervisor::new_for_test(Arc::new(Registry::default()), RestartPolicy::default());
        let snapshot = stale_process_snapshot(ModuleState::Running, true);
        let initial = ModuleSpec {
            module_id: "rescan-preserves-spawn-facts".to_string(),
            program: PathBuf::from("/spawned/module"),
            args: Vec::new(),
            env: Vec::new(),
            reserved: false,
            reserved_prefixes: Vec::new(),
            protocol: ModuleProtocol::Subc,
            overlap: Default::default(),
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
    // All restart backoffs run here, including health and operator requests.
    // While one is pending the loop serves commands, so disable or drain can
    // cancel the replacement without spawning a process just to stop it.
    let mut pending_respawn: Option<PendingRespawn> = None;
    // Commands a swap handed back to run next (see `swap::SwapEnd`). Served
    // before anything else so a stop that interrupted a swap runs at once.
    let mut requeued: VecDeque<SupervisorCommand> = VecDeque::new();
    loop {
        #[cfg(target_os = "macos")]
        if let Some(active) = child.as_mut() {
            active.confirm_privacy_exec().await;
        }
        if let Some(scheduled) = runtime
            .scheduled_respawn
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
        {
            pending_respawn = Some(scheduled);
        }
        if pending_respawn.is_some() && (child.is_some() || !respawn_still_pending(&snapshot)) {
            pending_respawn = None;
            cancel_deferred_reload(
                &runtime,
                &spec.module_id,
                "respawn cancelled by a supervisor command",
            );
        }
        if child.is_none() && pending_respawn.is_none() {
            cancel_deferred_reload(
                &runtime,
                &spec.module_id,
                "respawn cancelled before a replacement was spawned",
            );
            let _ = update_snapshot(&snapshot, Some(&spec.module_id), |state| {
                state.respawn_pending = false;
                state.coalesced_restart_pending = false;
                if matches!(
                    state.state,
                    ModuleState::Restarting
                        | ModuleState::Starting
                        | ModuleState::Draining
                        | ModuleState::Unresponsive
                ) {
                    error!(module_id = %spec.module_id, state = ?state.state,
                        "supervision operation ended without a child or pending respawn; marking failed so start can retry");
                    state.state = ModuleState::Failed;
                    clear_current_process_facts(state);
                }
            });
        }
        if let Some(command) = requeued.pop_front() {
            if !handle_supervisor_command(
                command,
                &mut spec,
                &mut runtime,
                &registry,
                &process_liveness,
                &snapshot,
                &mut child,
                &mut commands,
                &mut requeued,
            )
            .await
            {
                return;
            }
            if child.is_some() || !respawn_still_pending(&snapshot) {
                pending_respawn = None;
                let _ = update_snapshot(&snapshot, Some(&spec.module_id), |state| {
                    state.respawn_pending = false
                });
            }
            continue;
        }
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
                        Ok(status) => classify_reaped_child_exit(&snapshot, active_child, &status),
                        Err(err) => {
                            active_child.drain_stderr(&spec.module_id).await;
                            fail_snapshot(&snapshot, Some(&spec.module_id), None);
                            // Every other exit path (on_child_exit's Clean/Crash arms,
                            // the reload-registration-failure path) records a terminal
                            // before moving on. Without one here, a module whose wait()
                            // itself errored (e.g. already reaped) leaves no terminal
                            // record at all -- an empty ring reads as "nothing died".
                            record_wait_error_terminal(
                                &spec.module_id,
                                &runtime.terminal_ring,
                                &runtime.spawn_events,
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

                    let next = on_child_exit(
                        &spec,
                        runtime.restart_policy,
                        &registry,
                        &snapshot,
                        &runtime.terminal_ring,
                        &runtime.spawn_events,
                        &runtime.child_roster,
                        exit_report,
                    ).await;
                    // The exit is recorded, so a daemon shutdown may stop
                    // waiting for this child (see `SupervisedChild::wait`).
                    active_child.release_roster();
                    match next {
                        NextAction::Stop { registration_released } => {
                            if registration_released {
                                process_liveness.untrack_if_current(&spec.module_id, &snapshot);
                            }
                            child = None;
                        }
                        NextAction::Restart { schedule } => {
                            let delay = schedule.map_or(
                                runtime.restart_policy.delay_for_restart(0),
                                |schedule| schedule.delay,
                            );
                            if let Some(schedule) = schedule {
                                log_crash_respawn(&spec.module_id, schedule);
                            }
                            // The exited child is fully recorded at this point,
                            // so release it and count the backoff down in the
                            // command-serving branch below rather than sleeping
                            // here: commands cannot be received from inside this
                            // select arm, and an operator disable or drain that
                            // arrives during the backoff must cancel the pending
                            // respawn instead of waiting for it to spawn first.
                            child = None;
                            pending_respawn = Some(PendingRespawn { deadline: Instant::now() + delay, kind: RespawnKind::Spawn });
                             let _ = update_snapshot(&snapshot, Some(&spec.module_id), |state| state.respawn_pending = true);
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
                        &mut commands,
                        &mut requeued,
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
        } else if let Some(pending) = pending_respawn {
            tokio::select! {
                _ = sleep_until(pending.deadline) => {
                    pending_respawn = None;
                let _ = update_snapshot(&snapshot, Some(&spec.module_id), |state| state.respawn_pending = false);
                    // A command handled below while the backoff elapsed may
                    // have stopped the module; never respawn past an operator's
                    // disable or drain.
                    if !respawn_still_pending(&snapshot) {
                        continue;
                    }
                    // The daemon began shutting down during the backoff: the
                    // spawn would be refused anyway, and refusing it here
                    // leaves the module stopped instead of reporting a
                    // failed restart.
                    if runtime.child_roster.is_closed() {
                        let _ = update_snapshot(&snapshot, Some(&spec.module_id), |state| {
                            state.state = ModuleState::Stopped;
                        });
                        debug!(module_id = %spec.module_id, "crash respawn cancelled by daemon shutdown");
                        continue;
                    }
                    if let Err(err) = release_dead_registration(
                        &registry,
                        runtime.forwarding.as_deref(),
                        &snapshot,
                        &spec.module_id,
                    ).await {
                        fail_snapshot(&snapshot, Some(&spec.module_id), None);
                        error!(module_id = %spec.module_id, error = %err, "registration did not release before restart");
                        continue;
                    }

                    if matches!(pending.kind, RespawnKind::Reload) {
                        let reply = runtime.deferred_reload_reply.lock().unwrap_or_else(|p| p.into_inner()).take();
                        let result = finish_reload_child(&spec, &runtime, &registry, &process_liveness, &snapshot, &mut child).await;
                        if let Some(reply) = reply { let _ = reply.send(result); }
                        continue;
                    }
                    process_liveness.track(spec.module_id.clone(), Arc::clone(&snapshot));
                    match spawn_and_mark_running(&spec, &runtime, &snapshot) {
                        Ok(next_child) => {
                            child = Some(next_child);
                            debug!(module_id = %spec.module_id, "supervised module restarted after crash");
                        }
                        Err(err) => {
                            fail_snapshot(&snapshot, Some(&spec.module_id), None);
                            process_liveness.untrack_if_current(&spec.module_id, &snapshot);
                            error!(module_id = %spec.module_id, error = %err, "failed to restart supervised module");
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
                        &mut commands,
                        &mut requeued,
                    ).await {
                        return;
                    }
                    // Reconcile the pending respawn with what the command did:
                    // a start may already have spawned a fresh child,
                    // while a disable or drain moved the snapshot out of the
                    // state the respawn was counting down from.
                    if child.is_some() || !respawn_still_pending(&snapshot) {
                        pending_respawn = None;
                let _ = update_snapshot(&snapshot, Some(&spec.module_id), |state| state.respawn_pending = false);
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
                &mut commands,
                &mut requeued,
            )
            .await
            {
                return;
            }
        }
    }
}

fn log_crash_respawn(module_id: &str, schedule: CrashRestartSchedule) {
    info!(
        module_id,
        restart_in_window = schedule.restart_in_window,
        delay_ms = schedule.delay.as_millis() as u64,
        "respawning after crash"
    );
}

/// Whether the respawn a backoff was counting down to is still wanted. A
/// disable or drain handled while the backoff elapsed moves the snapshot out
/// of `Restarting`, and the operator's stop must win over the pending respawn,
/// so every sleep-then-spawn path re-validates against the live snapshot
/// instead of assuming the state it left behind still holds.
fn respawn_still_pending(snapshot: &SharedSnapshot) -> bool {
    matches!(
        lock_snapshot(snapshot),
        Ok(state) if state.enabled && state.state == ModuleState::Restarting
    )
}

enum NextAction {
    Stop {
        registration_released: bool,
    },
    Restart {
        schedule: Option<CrashRestartSchedule>,
    },
}

#[allow(clippy::too_many_arguments)]
async fn handle_supervisor_command(
    command: SupervisorCommand,
    spec: &mut ModuleSpec,
    runtime: &mut SupervisorRuntimeConfig,
    registry: &Arc<Registry>,
    process_liveness: &SupervisorProcessLiveness,
    snapshot: &SharedSnapshot,
    child: &mut Option<SupervisedChild>,
    commands: &mut mpsc::Receiver<SupervisorCommand>,
    requeued: &mut VecDeque<SupervisorCommand>,
) -> bool {
    match command {
        SupervisorCommand::Drain { reply } => {
            // A plain stop runs no forwarding drain, so nothing reaches the
            // module over its connection before the wait: ask by signal.
            let result = drain_optional_child(
                &spec.module_id,
                spec.protocol,
                StopNotice::NotSent,
                registry,
                runtime.forwarding.as_deref(),
                snapshot,
                &runtime.terminal_ring,
                &runtime.spawn_events,
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
                let stop_notice = begin_forwarding_drain_if_configured(
                    spec,
                    runtime,
                    registry,
                    snapshot,
                    None,
                    RouteCloseReason::Disable,
                )
                .await?;
                drain_optional_child(
                    &spec.module_id,
                    spec.protocol,
                    stop_notice,
                    registry,
                    runtime.forwarding.as_deref(),
                    snapshot,
                    &runtime.terminal_ring,
                    &runtime.spawn_events,
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
            received_at_generation,
            queued_at,
            reply,
        } => {
            // Without this line a restart that waited in the queue (behind a
            // health probe cycle or another command) was invisible: the log
            // showed only the drain timing out, minutes after the operator's call.
            info!(
                module_id = %spec.module_id,
                queued_ms = u64::try_from(queued_at.elapsed().as_millis()).unwrap_or(u64::MAX),
                "restart command dequeued"
            );
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
            // A restart asks for a fresh process. Commands run one at a time,
            // so a restart queued behind another restart (two operator calls
            // in quick succession) is dequeued the moment the first one has
            // spawned its replacement -- before that process has sent HELLO.
            // Running it would drain and kill the process the first restart
            // just produced, which is the opposite of what both callers asked
            // for. If a process spawned after this request was received is
            // still supervised, the request is already satisfied. Not when the
            // configuration changed since that spawn: then the newer process
            // predates the spec this restart may exist to apply.
            let satisfied_by_generation = if initiated && child.is_some() {
                lock_snapshot(snapshot).ok().and_then(|state| {
                    (state.spawn_generation > received_at_generation
                        && !state.configuration_updated_since_spawn)
                        .then_some(state.spawn_generation)
                })
            } else {
                None
            };
            let satisfied_by_pending = initiated
                && child.is_none()
                && lock_snapshot(snapshot).ok().is_some_and(|mut state| {
                    let pending = state.respawn_pending && !state.configuration_updated_since_spawn;
                    if pending {
                        state.coalesced_restart_pending = true;
                    }
                    pending
                });
            if satisfied_by_pending {
                debug!(module_id = %spec.module_id, "restart coalesced into the pending replacement");
            } else if let Some(generation) = satisfied_by_generation {
                info!(
                    module_id = %spec.module_id,
                    received_at_generation,
                    "restart already satisfied by generation {generation}; not restarting again"
                );
            } else if initiated {
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
            if result.is_ok()
                && runtime
                    .scheduled_respawn
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .as_ref()
                    .is_some_and(|pending| matches!(pending.kind, RespawnKind::Reload))
            {
                *runtime
                    .deferred_reload_reply
                    .lock()
                    .unwrap_or_else(|p| p.into_inner()) = Some(reply);
            } else {
                let _ = reply.send(result);
            }
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
            let _ = update_snapshot(snapshot, Some(&spec.module_id), |state| {
                state.configuration_updated_since_spawn = true;
            });
            let health_changed = runtime.health != health;
            runtime.health = health;
            // Reset the cadence and old endpoint's failure streak on a live
            // health-policy change rather than waiting for its old deadline.
            if health_changed {
                let _ = update_snapshot(snapshot, Some(&spec.module_id), |state| {
                    state.health = ModuleHealthStatus::default();
                });
            }
            runtime.drain_timeout = drain_timeout_ms
                .map(Duration::from_millis)
                .unwrap_or(runtime.default_drain_timeout);
            *runtime
                .effective_drain_timeout
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = runtime.drain_timeout;
            let _ = reply.send(());
            true
        }
        SupervisorCommand::Swap {
            ready_timeout,
            reply,
        } => {
            let end = swap::run_swap(
                spec,
                runtime,
                registry,
                process_liveness,
                snapshot,
                child,
                commands,
                ready_timeout.unwrap_or(DEFAULT_SWAP_READY_TIMEOUT),
                reply,
            )
            .await;
            requeued.extend(end.requeue);
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
    let stop_notice = begin_forwarding_drain_with_timeout(
        spec,
        runtime,
        registry,
        snapshot,
        None,
        RouteCloseReason::Restart,
        drain_timeout,
    )
    .await?;

    if child.is_some() {
        drain_optional_child(
            &spec.module_id,
            spec.protocol,
            stop_notice,
            registry,
            runtime.forwarding.as_deref(),
            snapshot,
            &runtime.terminal_ring,
            &runtime.spawn_events,
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
        release_dead_registration(
            registry,
            runtime.forwarding.as_deref(),
            snapshot,
            &spec.module_id,
        )
        .await?;
    }

    reset_restart_count(snapshot, &spec.module_id)?;
    process_liveness.track(spec.module_id.clone(), Arc::clone(snapshot));
    schedule_respawn(
        runtime,
        snapshot,
        &spec.module_id,
        runtime.restart_policy.backoff,
        RespawnKind::Spawn,
    )
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
    let stop_notice = begin_forwarding_drain(
        spec,
        runtime,
        registry,
        snapshot,
        Some(true),
        RouteCloseReason::Reload,
    )
    .await?;

    if child.is_some() {
        drain_optional_child(
            &spec.module_id,
            spec.protocol,
            stop_notice,
            registry,
            runtime.forwarding.as_deref(),
            snapshot,
            &runtime.terminal_ring,
            &runtime.spawn_events,
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
        release_dead_registration(
            registry,
            runtime.forwarding.as_deref(),
            snapshot,
            &spec.module_id,
        )
        .await?;
    }

    reset_restart_count(snapshot, &spec.module_id)?;
    process_liveness.track(spec.module_id.clone(), Arc::clone(snapshot));
    schedule_respawn(
        runtime,
        snapshot,
        &spec.module_id,
        runtime.restart_policy.backoff,
        RespawnKind::Reload,
    )
}

async fn finish_reload_child(
    spec: &ModuleSpec,
    runtime: &SupervisorRuntimeConfig,
    registry: &Registry,
    process_liveness: &SupervisorProcessLiveness,
    snapshot: &SharedSnapshot,
    child: &mut Option<SupervisedChild>,
) -> Result<(), SuperviseError> {
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
            snapshot,
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
            // Keep the reaped child's roster guard until its terminal is written.
            // Shutdown waits on that guard, not on the child Option used for respawn.
            let mut exited_child = child.take().expect("exited reload child is still stored");
            #[cfg(test)]
            if let Some(gate) = &runtime.test_reload_exit_record_gate {
                gate.reached.notify_one();
                gate.resume.notified().await;
            }
            let result = handle_reload_child_registration_failure(
                spec,
                runtime,
                registry,
                process_liveness,
                snapshot,
                child,
                ReloadRegistrationFailure {
                    exit_report: registration_failure_exit_report(exit_report),
                    reason: exited_child
                        .spawn_failure
                        .clone()
                        .unwrap_or_else(|| "new child exited before registering".to_string()),
                },
            )
            .await;
            exited_child.release_roster();
            result
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
                    exit_report: registration_failure_exit_report(classify_reaped_child_exit(
                        snapshot,
                        &timed_out_child,
                        &status,
                    )),
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
    let (current_enabled, current_state, respawn_pending) = {
        let state = lock_snapshot(snapshot)?;
        (state.enabled, state.state, state.respawn_pending)
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
        && (matches!(current_state, ModuleState::Failed | ModuleState::Stopped)
            || (current_state == ModuleState::Restarting && !respawn_pending));
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
        release_dead_registration(
            registry,
            runtime.forwarding.as_deref(),
            snapshot,
            &spec.module_id,
        )
        .await?;
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
        let stop_notice = begin_forwarding_drain_if_configured(
            spec,
            runtime,
            registry,
            snapshot,
            Some(false),
            RouteCloseReason::Disable,
        )
        .await?;
        drain_optional_child(
            &spec.module_id,
            spec.protocol,
            stop_notice,
            registry,
            runtime.forwarding.as_deref(),
            snapshot,
            &runtime.terminal_ring,
            &runtime.spawn_events,
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

#[allow(clippy::too_many_arguments)]
async fn on_child_exit(
    spec: &ModuleSpec,
    policy: RestartPolicy,
    registry: &Registry,
    snapshot: &SharedSnapshot,
    terminal_ring: &Arc<Mutex<TerminalRing>>,
    spawn_events: &SpawnEventFeed,
    roster: &ChildRoster,
    exit_report: ExitReport,
) -> NextAction {
    // Once the daemon has begun shutting down, no exit is a crash to recover
    // from: the module is exiting because the daemon is going away (EOF on its
    // connection, or a service manager signalling the whole cgroup). Record it
    // as such and never schedule a respawn, which would only start a process
    // for the shutdown to end again.
    if roster.is_closed() {
        return on_child_exit_during_daemon_shutdown(
            spec,
            registry,
            snapshot,
            terminal_ring,
            spawn_events,
            exit_report,
        )
        .await;
    }
    // Every stop the supervisor itself asks for (operator stop, disable,
    // restart, reload, swap, a health restart, a drain that runs out of budget)
    // takes the child out of the supervise loop and reaps it in
    // `drain_child_to_state`, and daemon shutdown is handled above. So an exit
    // that reaches this point was not requested by the daemon.
    //
    // For a subc-wire module a clean exit is still a stop: those modules are
    // written to re-raise SIGTERM, so a stray outside signal already reads as a
    // crash, and exiting 0 is a deliberate choice the module made. A
    // `protocol: "none"` module is a stock program we cannot change, and many
    // of them (nats-server among them) exit 0 on SIGTERM. Treating that as a
    // stop would leave the module down for good after any stray signal, so it
    // goes through the crash path instead: it spends restart budget, respawns
    // with the crash backoff, and ends `failed` when the budget runs out.
    let unrequested_clean_exit_of_protocol_none = exit_report.kind == ExitKind::Clean
        && running_protocol(spec, snapshot) == ModuleProtocol::None;
    match exit_report.kind {
        ExitKind::Clean if !unrequested_clean_exit_of_protocol_none => {
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
            record_terminal(
                &spec.module_id,
                terminal_ring,
                spawn_events,
                &exit_report,
                TerminalDisposition::Stopped,
            );
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
        ExitKind::Clean | ExitKind::Crash => {
            if unrequested_clean_exit_of_protocol_none {
                warn!(
                    module_id = %spec.module_id,
                    exit_code = ?exit_report.code,
                    exit_signal = ?exit_report.signal,
                    "protocol-none module exited cleanly without a stop request; handling it as a crash"
                );
            } else {
                warn!(
                    module_id = %spec.module_id,
                    exit_code = ?exit_report.code,
                    exit_signal = ?exit_report.signal,
                    "supervised module exited abnormally (crash)"
                );
            }
            let mut restart_schedule = None;
            let mut disposition = TerminalDisposition::Disabled;
            // Set only when the budget is what stopped the module, so the
            // terminal record says which limit was hit rather than leaving
            // `failed` to be read as "crashed once, badly".
            let mut disposition_detail = lock_snapshot(snapshot)
                .ok()
                .and_then(|mut state| state.spawn_failure.take());
            let now = Instant::now();
            if let Err(err) = update_snapshot(snapshot, Some(&spec.module_id), |state| {
                clear_current_process_facts(state);
                state.last_exit = Some(exit_report.clone());
                if state.enabled {
                    if let Some(schedule) = state.next_crash_restart(&policy, now) {
                        state.state = ModuleState::Restarting;
                        restart_schedule = Some(schedule);
                        disposition = TerminalDisposition::Restarting;
                    } else {
                        state.state = ModuleState::Failed;
                        disposition = TerminalDisposition::Failed;
                        let budget = policy.budget_exhausted_detail();
                        disposition_detail =
                            Some(disposition_detail.take().map_or_else(
                                || budget.clone(),
                                |cause| format!("{cause}; {budget}"),
                            ));
                    }
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
            if disposition == TerminalDisposition::Failed {
                // The window is in the message, not only in the fields: this line
                // is read in a scrollback where a bare `max_restarts=3` reads as a
                // lifetime cap and sends the operator looking for three crashes
                // that never happened together.
                error!(
                    module_id = %spec.module_id,
                    max_restarts = policy.max_restarts,
                    window_secs = policy.window.as_secs(),
                    "module stopped: {}",
                    policy.budget_exhausted_detail()
                );
            }
            record_terminal_with_detail(
                &spec.module_id,
                terminal_ring,
                spawn_events,
                &exit_report,
                disposition,
                disposition_detail,
            );

            if let Some(schedule) = restart_schedule {
                NextAction::Restart {
                    schedule: Some(schedule),
                }
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
        ExitKind::DeliberateSeverance => {
            warn!(
                module_id = %spec.module_id,
                exit_code = ?exit_report.code,
                exit_signal = ?exit_report.signal,
                "supervised module exited after deliberate connection severance"
            );
            let mut should_restart = false;
            let mut disposition = TerminalDisposition::Disabled;
            if let Err(err) = update_snapshot(snapshot, Some(&spec.module_id), |state| {
                clear_current_process_facts(state);
                state.last_exit = Some(exit_report.clone());
                state.lifetime_restarts += 1;
                if state.enabled {
                    state.state = ModuleState::Restarting;
                    should_restart = true;
                    disposition = TerminalDisposition::Restarting;
                } else {
                    state.state = ModuleState::Disabled;
                }
            }) {
                error!(module_id = %spec.module_id, error = %err, "failed to record deliberately severed module exit");
                return NextAction::Stop {
                    registration_released: false,
                };
            }
            record_terminal(
                &spec.module_id,
                terminal_ring,
                spawn_events,
                &exit_report,
                disposition,
            );

            if should_restart {
                NextAction::Restart { schedule: None }
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
                        warn!(module_id = %spec.module_id, error = %err, "registration still active after deliberately severed module exit");
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

async fn on_child_exit_during_daemon_shutdown(
    spec: &ModuleSpec,
    registry: &Registry,
    snapshot: &SharedSnapshot,
    terminal_ring: &Arc<Mutex<TerminalRing>>,
    spawn_events: &SpawnEventFeed,
    exit_report: ExitReport,
) -> NextAction {
    info!(
        module_id = %spec.module_id,
        exit_code = ?exit_report.code,
        exit_signal = ?exit_report.signal,
        exit_kind = ?exit_report.kind,
        "supervised module exited during daemon shutdown; not restarting it"
    );
    if let Err(err) = update_snapshot(snapshot, Some(&spec.module_id), |state| {
        state.state = ModuleState::Stopped;
        clear_current_process_facts(state);
        state.last_exit = Some(exit_report.clone());
    }) {
        error!(module_id = %spec.module_id, error = %err, "failed to record module exit during daemon shutdown");
    }
    record_terminal(
        &spec.module_id,
        terminal_ring,
        spawn_events,
        &exit_report,
        TerminalDisposition::DaemonShutdown,
    );
    let registration_released =
        wait_for_registration_release(registry, &spec.module_id, REGISTRY_RELEASE_TIMEOUT)
            .await
            .is_ok();
    NextAction::Stop {
        registration_released,
    }
}

fn record_wait_error_terminal(
    module_id: &str,
    terminal_ring: &Arc<Mutex<TerminalRing>>,
    spawn_events: &SpawnEventFeed,
) {
    record_terminal(
        module_id,
        terminal_ring,
        spawn_events,
        &wait_error_exit_report(),
        TerminalDisposition::Failed,
    );
}

fn record_terminal(
    module_id: &str,
    terminal_ring: &Arc<Mutex<TerminalRing>>,
    spawn_events: &SpawnEventFeed,
    exit_report: &ExitReport,
    disposition: TerminalDisposition,
) {
    record_terminal_with_detail(
        module_id,
        terminal_ring,
        spawn_events,
        exit_report,
        disposition,
        None,
    );
}

/// The ring lock is held only to capture the read (see
/// `TerminalJournal::capture_read`), so this module's exits keep recording
/// while the journal files are read. Blocking: it reads files.
fn durable_terminal_history_of(
    terminal_ring: &Mutex<TerminalRing>,
    module_id: &str,
) -> subc_control::TerminalHistory {
    let read = terminal_ring
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .capture_durable_history();
    read.read(module_id)
}

fn record_terminal_with_detail(
    module_id: &str,
    terminal_ring: &Arc<Mutex<TerminalRing>>,
    spawn_events: &SpawnEventFeed,
    exit_report: &ExitReport,
    disposition: TerminalDisposition,
    disposition_detail: Option<String>,
) {
    spawn_events.emit_exited(module_id, exit_report.code, exit_report.signal);
    let record = TerminalRecord {
        exit_code: exit_report.code,
        exit_signal: exit_report.signal,
        at_ms: exit_report.at_ms,
        disposition,
        exit_kind: exit_report.kind.into(),
        disposition_detail,
    };
    terminal_ring
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .record_exit(module_id, record);
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

/// The child's environment plan: inherit the parent's, drop ambient `CK_LOG`,
/// then apply the module's configured entries minus daemon-private capture keys.
///
/// Separated from `spawn_child` only so it can be asserted without spawning a
/// process — a duplicate of this logic in a test would pass while the real one
/// drifted, which is the defect class this function exists to avoid.
/// The subc-wire half of a spawn: `--subc <connection file>` and the launch
/// nonce. A `protocol: "none"` module gets neither, because it cannot use
/// either and the argument would stop a stock binary from starting at all.
/// `SUBC_MODULE_ID` is set on every path since an unread variable is inert.
///
/// The plain-spawn form, kept for the tests that assert its plan; spawns go
/// through [`apply_wire_spawn_args_for_role`].
#[cfg(test)]
fn apply_wire_spawn_args(
    command: &mut Command,
    spec: &ModuleSpec,
    connection_file_path: Option<&std::path::Path>,
    handle: Option<&SupervisorHandle>,
) -> Result<Option<NonceHandoff>, SuperviseError> {
    apply_wire_spawn_args_for_role(
        command,
        spec,
        connection_file_path,
        handle,
        SpawnRole::Plain,
    )
}

/// The read end of a spawn's launch-nonce pipe, prepared by
/// [`apply_wire_spawn_args_for_role`] and installed as the child's descriptor 3
/// by the last pre-exec step, just before `spawn()`. Windows has no descriptor
/// handoff and keeps only the environment copy.
#[cfg(unix)]
type NonceHandoff = subc_os::LaunchNonceHandoff;
#[cfg(not(unix))]
type NonceHandoff = std::convert::Infallible;

/// Prepare wire identity for a plain spawn or a swap candidate.
///
/// A plain spawn replaces the module's recorded nonce. A swap candidate records
/// a separate candidate token so the still-serving incumbent and its consumers
/// keep their nonce. Both records are installed before the process exists, so
/// the child's initial HELLO registration cannot arrive ahead of its nonce.
///
/// On Unix the nonce is delivered only through a pipe. It is written into
/// a pipe whose read end the child gets as descriptor 3, named by
/// `SUBC_LAUNCH_NONCE_FD=3:<pipe inode>`: unlike the environment, another
/// process of the same user cannot read it with `ps eww`. That handoff is
/// returned rather than installed here, because installing it replaces
/// whatever the child has at descriptor 3 and so must be the last pre-exec
/// step, after the Linux cgroup placement that the caller registers later.
/// Windows retains the environment handoff until restricted handle inheritance
/// can be implemented outside std's process primitives.
fn apply_wire_spawn_args_for_role(
    command: &mut Command,
    spec: &ModuleSpec,
    connection_file_path: Option<&std::path::Path>,
    handle: Option<&SupervisorHandle>,
    role: SpawnRole,
) -> Result<Option<NonceHandoff>, SuperviseError> {
    command.env(SUBC_MODULE_ID_ENV, &spec.module_id);
    // SUBC_LAUNCH_NONCE_FD is removed for every child, `protocol: "none"`
    // included: a daemon started from a module's process tree inherits it,
    // and passing it on would point the child at a descriptor it does not
    // have.
    command.env_remove(subc_os::LAUNCH_NONCE_FD_ENV);
    // Remove inherited or configured copies too: withholding must mean absent.
    command.env_remove(SUBC_LAUNCH_NONCE_ENV);
    if spec.protocol == ModuleProtocol::None {
        return Ok(None);
    }
    if let Some(connection_file_path) = connection_file_path {
        command.arg(SUBC_ARG).arg(connection_file_path);
    }

    // Every subc-wire spawn receives a fresh one-time launch nonce for consumer
    // route.open attestation. Reserved modules additionally use the same nonce
    // for HELLO id-squatting protection. A respawn rotates both records.
    let nonce = generate_launch_nonce()?;
    if let Some(handle) = handle {
        match role {
            SpawnRole::Plain => {
                handle.set_spawn_nonce(&spec.module_id, nonce.clone());
                if spec.reserved {
                    handle.set_reserved_nonce(&spec.module_id, nonce.clone());
                }
            }
            SpawnRole::SwapCandidate => handle.open_swap(&spec.module_id, nonce.clone()),
        }
    }
    #[cfg(unix)]
    let handoff = {
        let handoff =
            subc_os::LaunchNonceHandoff::new(&nonce).map_err(|source| SuperviseError::Spawn {
                program: spec.program.clone(),
                source,
                cgroup_path: None,
            })?;
        command.env(subc_os::LAUNCH_NONCE_FD_ENV, handoff.fd_env_value());
        Some(handoff)
    };
    #[cfg(not(unix))]
    let handoff = None;
    // Windows keeps the environment copy: std cannot restrict an inherited pipe
    // handle to this child without leaking it to concurrently spawned processes.
    #[cfg(not(unix))]
    command.env(SUBC_LAUNCH_NONCE_ENV, nonce);
    Ok(handoff)
}

fn apply_child_env(command: &mut Command, spec: &ModuleSpec) {
    command.env_remove(CK_LOG_ENV);
    // The spawn role is the supervisor's to set, and only on a swap candidate
    // (see `apply_spawn_role`). Removing it here, rather than just not setting
    // it, is what makes it absent on a plain spawn: the daemon's own
    // environment could carry it, and so could a spec built outside daemon
    // config (config refuses it as an `env` key). A module reading it on a
    // plain restart would pick the long swap budget and leave callers waiting.
    command.env_remove(SUBC_SPAWN_ROLE_ENV);
    for (key, value) in &spec.env {
        // cortexkit-log currently exposes retention only as a Rust struct, not
        // environment names. These values are daemon-private sink metadata and
        // must never become a public child-process contract by being inherited.
        if matches!(
            key.as_str(),
            CAPTURE_MAX_FILE_MB_ENV | CAPTURE_KEEP_ENV | CAPTURE_MAX_AGE_DAYS_ENV
        ) || key == SUBC_SPAWN_ROLE_ENV
        {
            continue;
        }
        command.env(key, value);
    }
}

/// Which slot a spawn fills: the module's ordinary one, or the candidate slot
/// of a blue/green swap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SpawnRole {
    Plain,
    SwapCandidate,
}

/// Set the spawn role for a swap candidate. A plain spawn gets nothing here;
/// `apply_child_env` has already removed the variable for every spawn.
fn apply_spawn_role(command: &mut Command, role: SpawnRole) {
    if role == SpawnRole::SwapCandidate {
        command.env(SUBC_SPAWN_ROLE_ENV, SPAWN_ROLE_SWAP_CANDIDATE);
    }
}

fn spawn_child(
    spec: &ModuleSpec,
    connection_file_path: Option<&std::path::Path>,
    handle: Option<&SupervisorHandle>,
    ring: &Arc<Mutex<StderrRing>>,
    capture_logs_dir: Option<&std::path::Path>,
    roster: &ChildRoster,
    #[cfg(target_os = "linux")] cgroup_placement: Option<&subc_cgroup::Placement>,
) -> Result<SupervisedChild, SuperviseError> {
    spawn_child_in_slot(
        spec,
        connection_file_path,
        handle,
        ring,
        capture_logs_dir,
        roster,
        #[cfg(target_os = "linux")]
        cgroup_placement,
        SpawnRole::Plain,
        false,
    )
}

/// Spawn one process of `spec` into a slot.
///
/// `alternate_slot` picks the process's cgroup label on Linux (see `swap::cgroup_name`).
/// A swap candidate needs a different cgroup from the process it is replacing,
/// which is still alive: in the same cgroup the two would be one kill domain,
/// and killing a failed candidate could take the incumbent with it.
///
/// The stderr capture file is `<module_id>.stderr.log` for every process of
/// the module, whichever slot it is in, because that is the one file
/// `ck module logs` reads. During a swap's overlap both processes append to it;
/// the daemon writes whole lines, so the two interleave by line, which is also
/// the merged view an operator wants while a swap runs.
#[allow(clippy::too_many_arguments)]
fn spawn_child_in_slot(
    spec: &ModuleSpec,
    connection_file_path: Option<&std::path::Path>,
    handle: Option<&SupervisorHandle>,
    ring: &Arc<Mutex<StderrRing>>,
    capture_logs_dir: Option<&std::path::Path>,
    roster: &ChildRoster,
    #[cfg(target_os = "linux")] cgroup_placement: Option<&subc_cgroup::Placement>,
    role: SpawnRole,
    alternate_slot: bool,
) -> Result<SupervisedChild, SuperviseError> {
    if roster.is_closed() {
        return Err(SuperviseError::Spawn {
            program: spec.program.clone(),
            source: io::Error::other("the daemon is shutting down; not starting a new process"),
            cgroup_path: None,
        });
    }
    #[cfg(target_os = "linux")]
    let cgroup_name = {
        // Slot names alone are not kill domains: a retired incumbent may still
        // be draining when a later enable/restart spawns into the same slot.
        // Decimal entropy keeps the suffix unambiguous; Placement performs
        // the module-id escaping and constructs the filesystem path.
        if cgroup_placement.is_none() {
            swap::cgroup_name(&spec.module_id, alternate_slot)
        } else {
            let nonce = generate_launch_nonce()?;
            let suffix = u128::from_str_radix(&nonce[..32], 16).expect("hex launch nonce");
            // Leave room for byte escaping and the suffix under NAME_MAX. The
            // label is only for humans; the nonce identifies the kill domain.
            let mut end = spec.module_id.len().min(64);
            while !spec.module_id.is_char_boundary(end) {
                end -= 1;
            }
            format!(
                "{}_{suffix}",
                swap::cgroup_name(&spec.module_id[..end], alternate_slot)
            )
        }
    };
    #[cfg(not(target_os = "linux"))]
    let _ = alternate_slot;
    #[cfg(target_os = "macos")]
    let (mut command, privacy_exec, exec_ack) = privacy_command(spec, roster)?;
    #[cfg(not(target_os = "macos"))]
    let mut command = Command::new(&spec.program);
    command.args(&spec.args);
    // AMBIENT `CK_LOG` MUST NOT LEAK INTO AN OTHERWISE UNCONFIGURED MODULE — but
    // that is the whole of the intent, so remove that one key rather than the
    // environment.
    //
    // This was `env_clear()` from 0.17.41 until 0.18.3, which achieved the goal
    // and took the POSIX environment with it. Modules spawned that way had no
    // HOME, XDG_RUNTIME_DIR, TMPDIR or USER, and the consequences ran past
    // logging:
    //
    //   * `connection_file::discover` reads XDG_RUNTIME_DIR and HOME, so with
    //     both unset it fell back to the temp dir alone and `ck` could not find
    //     a daemon running on the same machine from inside any module's process
    //     tree — reporting a path the file has never lived at, which reads as
    //     "the daemon did not write its file".
    //   * `default_data_home()` with HOME and XDG_DATA_HOME both unset returns
    //     the RELATIVE `.local/share`, so a module deriving its own store path
    //     resolved it against its own CWD. That is the store-fragmentation
    //     defect the daemon already refuses in config (`parse_doc` rejects a
    //     relative `storage.data_home`) arriving by derivation instead.
    //   * anything a module spawns inherited it: git without ~/.gitconfig,
    //     cargo without CARGO_HOME, ssh, python user dirs — all degrading
    //     quietly rather than erroring.
    //
    // Reported by iceteaSA as #104 after deploying 0.18.2, where `ck daemon`
    // offered one candidate under /tmp while the file sat in /run/user/1000.
    //
    // A configured module is unaffected either way: `module_spec()` puts the
    // resolved CK_LOG into `spec.env`, which is applied below and therefore
    // wins over anything ambient.
    apply_child_env(&mut command, spec);
    apply_spawn_role(&mut command, role);
    let nonce_handoff =
        apply_wire_spawn_args_for_role(&mut command, spec, connection_file_path, handle, role)?;

    #[cfg(target_os = "linux")]
    let cgroup_path = cgroup_placement
        .map(|placement| placement.module_path(&cgroup_name))
        .transpose()
        .map_err(|source| SuperviseError::Cgroup {
            module_id: spec.module_id.clone(),
            source,
        })?;
    #[cfg(not(target_os = "linux"))]
    let cgroup_path: Option<PathBuf> = None;
    #[cfg(target_os = "linux")]
    if let Some(path) = &cgroup_path {
        if let Err(error) = apply_cgroup_placement(&mut command, spec, path) {
            if let Some(placement) = cgroup_placement {
                remove_module_cgroup(placement, &cgroup_name);
            }
            return Err(error);
        }
    }

    let output_sink = if let Some(logs_dir) = capture_logs_dir {
        let path = logs_dir.join(format!("{}.stderr.log", spec.module_id));
        match ChildOutputSink::open(&path, capture_retention(spec)) {
            Ok(sink) => sink,
            Err(error) => {
                warn!(
                    module_id = %spec.module_id,
                    path = %path.display(),
                    error = %error,
                    "could not open child output capture file; forwarding to stderr"
                );
                ChildOutputSink::Stderr
            }
        }
    } else {
        ChildOutputSink::Stderr
    };

    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    command.kill_on_drop(true);
    // EACH MODULE LEADS ITS OWN PROCESS GROUP (the child calls setpgid(0, 0)
    // before exec). In the daemon's group, a service manager that kills the
    // job's process group when the daemon exits (launchd's default) killed
    // every module at the same moment its control connection closed, so no
    // module ever ran its EOF teardown on a daemon stop. Outside that group a
    // module is reached only by the daemon: the EOF it sees when its
    // connection closes, and the bounded stop in `child_roster` for anything
    // still running after that. On Linux this composes with the cgroup
    // placement above: that is a pre_exec write to cgroup.procs, std performs
    // setpgid in the child before running pre_exec callbacks, and the two
    // change independent process attributes.
    //
    // stdin is /dev/null because a process outside the terminal's foreground
    // group is stopped (SIGTTIN) if it reads the terminal, which a daemon run
    // by hand would otherwise hand down. Under a service manager stdin is
    // already /dev/null.
    #[cfg(unix)]
    command.process_group(0);
    command.stdin(Stdio::null());
    // The LAST pre-exec step, after the cgroup placement above: installing the
    // nonce at descriptor 3 replaces whatever the child had there, which could
    // be the descriptor an earlier step writes through.
    #[cfg(unix)]
    if let Some(handoff) = nonce_handoff {
        handoff.install_last(command.as_std_mut());
    }
    #[cfg(not(unix))]
    let _ = nonce_handoff;

    // Containment, step 1 of 3 (issue #109): create the child suspended so it
    // cannot run a single instruction -- and therefore cannot spawn a
    // grandchild -- before it is in the job. See `contain_spawned_child` for the
    // other two steps and why the window matters.
    #[cfg(windows)]
    subc_jobobject::suspend_on_create_async(&mut command);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(source) => {
            #[cfg(target_os = "linux")]
            if let Some(placement) = cgroup_placement {
                remove_module_cgroup(placement, &cgroup_name);
            }
            return Err(SuperviseError::Spawn {
                program: spec.program.clone(),
                source,
                cgroup_path,
            });
        }
    };
    // The parent must close its writer now: the acknowledgement pipe reports EOF
    // only when every writer is gone, and the child's copy closes when the
    // trampoline replaces itself with the module. Command holds only an integer
    // in its pre_exec callback, not another writer.
    #[cfg(target_os = "macos")]
    drop(exec_ack);

    // Containment, steps 2 and 3: assign while suspended, then resume.
    #[cfg(windows)]
    let job = contain_spawned_child(&child, spec)?;
    let spawned_at_ms = unix_ms_now();
    let spawned_from = spec.program.clone();
    let spawned_file_identity = spawned_file_identity(&spawned_from);
    let pid = child.id().ok_or_else(|| SuperviseError::Spawn {
        program: spec.program.clone(),
        source: io::Error::other("spawned child exposed no live pid"),
        cgroup_path: cgroup_path.clone(),
    })?;
    let process_start_time = crate::provenance::process_start_time(pid);
    let process_identity = process_start_time.map(|start_time| ProcessIdentity { pid, start_time });
    // Unix spawn returns after exec's error pipe closes. The kernel image is
    // therefore the executable to compare during a future orphan sweep: PATH
    // lookup and shebang interpretation may select a different file from the
    // configured program. Keep the literal program's identity for provenance,
    // but never use it as proof that a recorded pid may be signalled.
    let recorded_image = observe_spawned_image(pid);
    // spawn() confirms only the first exec, into the trampoline. Never persist
    // the trampoline image; the asynchronous acknowledgement publishes the
    // module image once the trampoline has replaced itself with the module.
    #[cfg(target_os = "macos")]
    let recorded_image = if privacy_exec.is_some() {
        None
    } else {
        recorded_image
    };
    #[cfg(target_os = "linux")]
    let recorded_cgroup_name = cgroup_path.as_ref().map(|_| cgroup_name.clone());
    #[cfg(not(target_os = "linux"))]
    let recorded_cgroup_name = None;
    let roster_guard = roster.admit(
        spec.module_id.clone(),
        pid,
        spec.protocol,
        process_start_time,
        crate::child_roster::RecordedIdentity {
            start_time: recorded_image.map(|image| image.start_time),
            executable: recorded_image
                .and_then(|image| image.executable)
                .map(crate::live_children::ExecutableIdentity::from),
            cgroup_name: recorded_cgroup_name,
            #[cfg(target_os = "linux")]
            cgroup_placement: cgroup_placement.cloned(),
        },
    );
    // The check at the top of this function can pass just before daemon
    // shutdown begins, and the process is only in the roster from here on.
    // The shutdown stop returns as soon as it finds the roster empty, so a
    // process admitted after that look would outlive the daemon. The roster
    // is closed before the stop first reads it and admission happens under
    // the roster's lock, so either the stop sees this process or this check
    // sees the roster closed: end the process now rather than start a module
    // the daemon is about to stop.
    if roster.is_closed() {
        // This child was never admitted, so there is no module protocol shutdown to wait for.
        #[cfg(target_os = "linux")]
        kill_module_cgroup(cgroup_placement, &cgroup_name);
        if let Err(error) = child.start_kill() {
            debug!(module_id = %spec.module_id, pid, %error, "kill of a process spawned during daemon shutdown failed; it may already have exited");
        }
        #[cfg(target_os = "linux")]
        if let Some(placement) = cgroup_placement {
            // This spawn was never admitted, so shutdown has no roster entry
            // to await. Do not detach its cleanup: the runtime could exit
            // before that task reaps the rejected child and removes its group.
            while matches!(child.try_wait(), Ok(None)) {
                std::thread::yield_now();
            }
            if matches!(
                subc_cgroup::kill_module(Some(placement), &cgroup_name),
                subc_cgroup::KillOutcome::Killed
            ) {
                if let Ok(path) = placement.module_path(&cgroup_name) {
                    while std::fs::read_to_string(path.join("cgroup.events"))
                        .ok()
                        .is_some_and(|events| events.lines().any(|line| line == "populated 1"))
                    {
                        std::thread::yield_now();
                    }
                }
            }
            remove_module_cgroup(placement, &cgroup_name);
        }
        drop(roster_guard);
        return Err(SuperviseError::Spawn {
            program: spec.program.clone(),
            source: io::Error::other(
                "the daemon began shutting down while this process was starting; ended it",
            ),
            cgroup_path,
        });
    }

    let stdout_pump = match child.stdout.take() {
        Some(stdout) => Some(tokio::spawn(pump_stdout_to(stdout, output_sink.clone()))),
        None => {
            warn!(
                module_id = %spec.module_id,
                "spawned child exposed no stdout pipe; file capture will be incomplete"
            );
            None
        }
    };
    let stderr_pump = match child.stderr.take() {
        Some(stderr) => {
            let generation = ring
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .begin_process();
            Some(StderrPump {
                task: tokio::spawn(pump_stderr_to(
                    stderr,
                    Arc::clone(ring),
                    generation,
                    output_sink,
                )),
                generation,
            })
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
        protocol: spec.protocol,
        #[cfg(target_os = "linux")]
        module_id: cgroup_name,
        #[cfg(target_os = "linux")]
        cgroup_placement: cgroup_placement.cloned(),
        #[cfg(windows)]
        job,
        stdout_pump,
        stderr_pump,
        stderr_ring: Arc::clone(ring),
        spawned_at_ms,
        spawned_from,
        spawned_file_identity,
        process_start_time,
        process_identity,
        pid,
        roster_guard: Some(roster_guard),
        #[cfg(target_os = "macos")]
        privacy_exec,
        #[cfg(target_os = "macos")]
        report_ready: Arc::new(OnceLock::new()),
        spawn_failure: None,
    })
}

#[cfg(target_os = "linux")]
pub(crate) fn kill_module_cgroup(placement: Option<&subc_cgroup::Placement>, module_id: &str) {
    use subc_cgroup::KillOutcome;
    match subc_cgroup::kill_module(placement, module_id) {
        KillOutcome::Killed => {}
        KillOutcome::NotPlaced | KillOutcome::Unsupported => {
            debug!(
                module_id,
                "cgroup tree kill unavailable; using direct-child kill"
            );
        }
        KillOutcome::IoError { path, error } => {
            warn!(module_id, path = %path.display(), %error, "cgroup tree kill failed; using direct-child kill");
        }
    }
}

/// Contain a freshly spawned Windows child and start it.
///
/// Steps 2 and 3 of the suspended-create contract: the job is created and the
/// child assigned **while it is still suspended** (step 1 is
/// `suspend_on_create_async` at the spawn site), then the child is resumed.
///
/// A child that is never resumed hangs forever holding a pid, so a resume
/// failure kills the child and fails the spawn rather than returning a
/// `SupervisedChild` that can never run.
///
/// An assignment failure is NOT fatal: an uncontained module behaves exactly as
/// it did before this existed, whereas refusing to start one would be a new
/// outage. It is logged at warn because it means a helper process could leak.
#[cfg(windows)]
fn contain_spawned_child(
    child: &Child,
    spec: &ModuleSpec,
) -> Result<Option<subc_jobobject::JobObject>, SuperviseError> {
    let module_id = spec.module_id.as_str();
    let Some(pid) = child.id() else {
        // The child exited between spawn and here. Its tree, if it made one,
        // needs no containment: nothing is left to contain.
        warn!(
            module_id,
            "spawned child had already exited before containment; no job object attached"
        );
        return Ok(None);
    };

    let job = match subc_jobobject::JobObject::new() {
        Ok(job) => job,
        Err(source) => {
            warn!(
                module_id,
                error = %source,
                "could not create a job object; this module's helper processes will not be \
                 reaped on teardown"
            );
            // Resume regardless: leaving the child suspended would turn a
            // containment gap into a hung module.
            resume_suspended_child(pid, spec)?;
            return Ok(None);
        }
    };

    if let Err(source) = job.assign(child) {
        warn!(
            module_id,
            error = %source,
            "could not assign the child to its job object; this module's helper processes \
             will not be reaped on teardown"
        );
        resume_suspended_child(pid, spec)?;
        return Ok(None);
    }

    resume_suspended_child(pid, spec)?;
    Ok(Some(job))
}

/// Resume a suspended child, killing it if it cannot be started.
///
/// A suspended process holds a pid and does nothing, so there is no useful
/// state to return: the caller gets an error and the spawn fails.
#[cfg(windows)]
fn resume_suspended_child(pid: u32, spec: &ModuleSpec) -> Result<(), SuperviseError> {
    if let Err(source) = subc_jobobject::resume_main_thread(pid) {
        // Kill it here rather than leaving a suspended process for the caller
        // to notice; `kill_on_drop` would eventually do this, but the module
        // would have been reported as running in between.
        let _ = std::process::Command::new("taskkill.exe")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        return Err(SuperviseError::Spawn {
            program: spec.program.clone(),
            source,
            cgroup_path: None,
        });
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn remove_module_cgroup(placement: &subc_cgroup::Placement, module_id: &str) {
    match placement.remove_module(module_id) {
        Ok(()) => debug!(module_id, "removed module cgroup after process exit"),
        Err(error) => warn!(
            module_id,
            error = %error,
            "could not remove module cgroup after process exit; continuing teardown"
        ),
    }
}

#[cfg(target_os = "linux")]
async fn cleanup_reaped_cgroup(placement: &subc_cgroup::Placement, module_id: &str) {
    // Reaping the direct child is not proof its descendants exited. End the
    // residual tree and wait for the kernel's population fact before rmdir;
    // otherwise a successful parent wait leaks a directory on each restart.
    if matches!(
        subc_cgroup::kill_module(Some(placement), module_id),
        subc_cgroup::KillOutcome::Killed
    ) {
        if let Ok(path) = placement.module_path(module_id) {
            while std::fs::read_to_string(path.join("cgroup.events"))
                .ok()
                .is_some_and(|events| events.lines().any(|line| line == "populated 1"))
            {
                sleep(Duration::from_millis(1)).await;
            }
        }
    }
    remove_module_cgroup(placement, module_id);
}

#[cfg(target_os = "linux")]
fn apply_cgroup_placement(
    command: &mut Command,
    spec: &ModuleSpec,
    path: &std::path::Path,
) -> Result<(), SuperviseError> {
    subc_cgroup::apply(command, path).map_err(|source| SuperviseError::Cgroup {
        module_id: spec.module_id.clone(),
        source,
    })
}

fn capture_retention(spec: &ModuleSpec) -> Retention {
    let defaults = Retention::default();
    let value = |name: &str| {
        spec.env
            .iter()
            .rev()
            .find_map(|(key, value)| (key == name).then_some(value.as_str()))
    };
    Retention {
        max_file_mb: value(CAPTURE_MAX_FILE_MB_ENV)
            .and_then(|value| value.parse().ok())
            .unwrap_or(defaults.max_file_mb),
        keep: value(CAPTURE_KEEP_ENV)
            .and_then(|value| value.parse().ok())
            .unwrap_or(defaults.keep),
        max_age_days: value(CAPTURE_MAX_AGE_DAYS_ENV)
            .and_then(|value| value.parse().ok())
            .unwrap_or(defaults.max_age_days),
    }
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

/// The kernel's image after an acknowledged exec, shared by ordinary launches
/// and privacy trampolines. PATH and shebang interpretation are kernel facts,
/// not identities inferred from a configured pathname.
fn observe_spawned_image(pid: u32) -> Option<subc_os::Observation> {
    subc_os::Process::open(pid)
        .ok()
        .flatten()
        .and_then(|process| process.observe())
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
        runtime.capture_logs_dir.as_deref(),
        &runtime.child_roster,
        #[cfg(target_os = "linux")]
        runtime.cgroup_placement.as_ref(),
    )?;
    set_running(snapshot, &child, &spec.module_id, &runtime.spawn_events)?;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BusyGaugeObservation {
    Quiescent,
    Busy,
    Omitted,
}

fn busy_gauge_observation(metrics: Option<&Value>, gauges: &[String]) -> BusyGaugeObservation {
    let Some(metrics) = metrics.and_then(Value::as_object) else {
        return BusyGaugeObservation::Omitted;
    };
    let mut sum = 0u128;
    for gauge in gauges {
        let Some(value) = metrics.get(gauge) else {
            return BusyGaugeObservation::Omitted;
        };
        let Some(value) = value.as_u64() else {
            return BusyGaugeObservation::Busy;
        };
        sum = sum.saturating_add(u128::from(value));
    }
    if sum == 0 {
        BusyGaugeObservation::Quiescent
    } else {
        BusyGaugeObservation::Busy
    }
}

fn declared_busy_gauges(
    registry: &Registry,
    module_id: &str,
) -> Result<Vec<String>, SuperviseError> {
    busy_gauges_of(
        registry
            .get_module(module_id)
            .map_err(SuperviseError::Registry)?,
    )
}

/// [`declared_busy_gauges`] for the registration a connection holds, in any
/// slot: after cutover the incumbent is no longer the id's active
/// registration, and its own manifest is the one that names its gauges.
fn declared_busy_gauges_for_connection(
    registry: &Registry,
    connection_id: ConnectionId,
) -> Result<Vec<String>, SuperviseError> {
    busy_gauges_of(
        registry
            .get_module_by_connection(connection_id)
            .map_err(SuperviseError::Registry)?,
    )
}

fn busy_gauges_of(
    registration: Option<crate::registry::ModuleRegistration>,
) -> Result<Vec<String>, SuperviseError> {
    let Some(registration) = registration else {
        return Ok(Vec::new());
    };
    let Some(self_signals) = registration.manifest.self_signals else {
        return Ok(Vec::new());
    };

    let mut gauges = Vec::new();
    for declaration in self_signals {
        if declaration.kind != SelfSignalKind::Busy {
            continue;
        }
        match declaration.anchored_to {
            SignalAnchor::HealthGauges { gauges: declared } if !declared.is_empty() => {
                gauges.extend(declared)
            }
            _ => {
                // An invalid Busy anchor is fail-safe: the empty name cannot be
                // present in a conforming health report, so this drain stays busy.
                gauges.push(String::new());
            }
        }
    }
    Ok(gauges)
}

/// Wait for `endpoint` to have nothing in flight and, when the module declares
/// busy gauges, for a health probe to report them quiet. The probe is addressed
/// by `scope`: a swap's superseded incumbent must be asked about its own
/// gauges, and by module id the probe would reach the promoted candidate.
async fn wait_for_forwarding_quiescence(
    forwarding: &ForwardingTable,
    module_id: &str,
    runtime: &SupervisorRuntimeConfig,
    endpoint: crate::ModuleEndpointId,
    deadline: Instant,
    busy_gauges: &[String],
    scope: DrainScope,
) -> Result<bool, SuperviseError> {
    let mut gauges_quiescent = busy_gauges.is_empty();
    let mut next_probe_at = Instant::now();
    let mut omission_counted = false;

    loop {
        let now = Instant::now();
        if !busy_gauges.is_empty() && now >= next_probe_at && now < deadline {
            let report = match scope {
                DrainScope::Active => probe_module_health(module_id, runtime, Some(deadline)).await,
                DrainScope::Endpoint(endpoint) => {
                    probe_endpoint_health(endpoint, runtime, Some(deadline)).await
                }
            };
            gauges_quiescent = match report {
                Ok(report) => match busy_gauge_observation(report.metrics.as_ref(), busy_gauges) {
                    BusyGaugeObservation::Quiescent => true,
                    BusyGaugeObservation::Busy => false,
                    BusyGaugeObservation::Omitted => {
                        if !omission_counted {
                            forwarding
                                .counters()
                                .increment_drains_with_undeclared_gauge();
                            omission_counted = true;
                        }
                        false
                    }
                },
                Err(err) => {
                    warn!(
                        module_id,
                        error = %err,
                        "drain health.check did not produce declared busy gauges; treating module as busy"
                    );
                    false
                }
            };
            next_probe_at = Instant::now() + runtime.health.cadence.max(REGISTRY_RELEASE_POLL);
        }

        let in_flight = forwarding
            .endpoint_in_flight_count(endpoint)
            .map_err(SuperviseError::Forwarding)?;
        if in_flight == 0 && gauges_quiescent {
            return Ok(true);
        }

        let now = Instant::now();
        if now >= deadline {
            return Ok(false);
        }
        let mut wait = deadline
            .saturating_duration_since(now)
            .min(REGISTRY_RELEASE_POLL);
        if !busy_gauges.is_empty() {
            wait = wait.min(next_probe_at.saturating_duration_since(now));
        }
        sleep(wait).await;
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
        if !released.close_on_delivery_failure() {
            crate::forwarding::send_module_route_goodbye(
                &forwarding.counters(),
                &released.sink,
                frame,
                released.module_id.as_deref(),
                "supervisor drain",
            );
            continue;
        }
        if let Err(err) = released.sink.try_send(frame) {
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
                crate::forwarding::UndeliveredFrame {
                    module_id: released.module_id.as_deref(),
                    sink: &released.sink,
                },
            );
        }
    }
}

fn send_module_draining(
    module_id: &str,
    reason: RouteCloseReason,
    deadline_ms: u64,
    target: &ModuleDrainTarget,
) {
    let body = match serde_json::to_vec(&ModuleControlCommand::Draining {
        reason,
        deadline_ms,
    }) {
        Ok(body) => body,
        Err(err) => {
            warn!(
                module_id,
                error = %err,
                "failed to encode module draining command"
            );
            return;
        }
    };
    let frame = match Frame::build_with_version(
        target.negotiated_ver,
        FrameType::Push,
        control_flags(),
        0,
        0,
        0,
        body,
    ) {
        Ok(frame) => frame,
        Err(err) => {
            warn!(
                module_id,
                error = %err,
                "failed to build module draining command frame"
            );
            return;
        }
    };
    if let Err(err) = target.sink.try_send(frame) {
        warn!(
            module_id,
            target_connection_id = target.endpoint.connection_id.get(),
            error = %err,
            "module draining command was not delivered to peer"
        );
    }
}

/// The channel-0 GOODBYE that tells a module its stop is planned.
fn module_goodbye_frame(module_id: &str, negotiated_ver: u8) -> Option<Frame> {
    match Frame::build_with_version(
        negotiated_ver,
        FrameType::Goodbye,
        control_flags(),
        0,
        0,
        0,
        Vec::new(),
    ) {
        Ok(frame) => Some(frame),
        Err(err) => {
            warn!(
                module_id,
                error = %err,
                "failed to build module GOODBYE frame"
            );
            None
        }
    }
}

/// Send every registered module connection its module GOODBYE at daemon
/// shutdown, then request that connection's close.
///
/// A module tells a planned stop from a lost daemon by whether a GOODBYE came
/// before EOF, so the GOODBYE must reach the socket before the close. A close
/// request does not wait for the connection's queued frames: its writer gets a
/// bounded grace after the close, is aborted if it overruns it, and the daemon
/// process may exit before that grace ends. So with `wait_for_flush`, each
/// connection is closed only after its writer has acknowledged writing the
/// GOODBYE, or once a short shared budget runs out, so one module that is not
/// reading cannot hold up the others or the shutdown. Without it the GOODBYEs
/// are only queued, for a shutdown the operator has told to stop waiting.
/// A connection that is already gone is skipped.
#[cfg(unix)]
async fn send_module_goodbyes_for_daemon_shutdown(
    forwarding: &Arc<ForwardingTable>,
    reason: &CloseReason,
    wait_for_flush: bool,
) {
    const GOODBYE_BUDGET: Duration = Duration::from_millis(500);
    let targets = match forwarding.module_connections() {
        Ok(targets) => targets,
        Err(err) => {
            warn!(error = %err, "could not list module connections for shutdown GOODBYE");
            return;
        }
    };
    let deadline = Instant::now() + GOODBYE_BUDGET;
    let mut sends = tokio::task::JoinSet::new();
    for target in targets {
        let Some(frame) = module_goodbye_frame(&target.module_id, target.negotiated_ver) else {
            continue;
        };
        if !wait_for_flush {
            if let Err(err) = target.sink.try_send(frame) {
                debug!(
                    module_id = %target.module_id,
                    error = %err,
                    "shutdown module GOODBYE was not queued"
                );
            }
            continue;
        }
        let forwarding = Arc::clone(forwarding);
        let reason = reason.clone();
        sends.spawn(async move {
            match timeout_at(deadline, target.sink.send_flushed(frame)).await {
                Ok(Ok(())) => {}
                Ok(Err(err)) => debug!(
                    module_id = %target.module_id,
                    error = %err,
                    "module connection closed before its shutdown GOODBYE was written"
                ),
                Err(_) => warn!(
                    module_id = %target.module_id,
                    budget = ?GOODBYE_BUDGET,
                    "shutdown module GOODBYE was not written within its budget; closing anyway"
                ),
            }
            forwarding.request_connection_close(target.endpoint.connection_id, reason);
        });
    }
    // Every task ends by the shared deadline, so this wait is bounded too.
    while sends.join_next().await.is_some() {}
}

fn send_module_goodbye(module_id: &str, forwarding: &ForwardingTable, target: &ModuleDrainTarget) {
    let Some(frame) = module_goodbye_frame(module_id, target.negotiated_ver) else {
        return;
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

#[derive(Clone, Copy)]
struct ForwardingDrainContext<'a> {
    spec: &'a ModuleSpec,
    runtime: &'a SupervisorRuntimeConfig,
    registry: &'a Registry,
    scope: DrainScope,
}

/// Which process a forwarding drain addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DrainScope {
    /// Whatever endpoint is active for the module id: every plain stop,
    /// restart and reload. Also moves the module's state to `Draining`.
    Active,
    /// One specific endpoint: a swap's incumbent after cutover. Draining it by
    /// module id would resolve to the promoted candidate and leave neither
    /// process routable. The module's state is left alone, since the promoted
    /// candidate is what it describes and that process is running.
    Endpoint(crate::ModuleEndpointId),
}

/// Whether a child being drained has already been asked to stop by the time
/// its drain wait starts.
///
/// The drain wait is the same budget whatever this says. What it decides is
/// whether the supervisor must ask by signal before that wait begins: a child
/// that nobody asked will sit out the whole budget and then be SIGKILLed,
/// healthy or not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopNotice {
    /// The module was sent `module.draining` and a module GOODBYE over its own
    /// registered connection, and stops itself.
    SentOverConnection,
    /// The forwarding drain found no registered connection for the module: a
    /// subc child spawned moments ago that has not sent HELLO yet, or a
    /// `protocol: "none"` child, which never registers.
    NoConnection,
    /// This path sends nothing over the module's connection: the supervisor has
    /// no forwarding table, or the caller stops the child without a forwarding
    /// drain.
    NotSent,
}

async fn begin_forwarding_drain(
    spec: &ModuleSpec,
    runtime: &SupervisorRuntimeConfig,
    registry: &Registry,
    snapshot: &SharedSnapshot,
    enabled: Option<bool>,
    reason: RouteCloseReason,
) -> Result<StopNotice, SuperviseError> {
    let Some(forwarding) = runtime.forwarding.as_ref() else {
        return Err(SuperviseError::ReloadUnavailable {
            module_id: spec.module_id.clone(),
            reason: "supervisor was not configured with a forwarding table".to_string(),
        });
    };

    begin_forwarding_drain_with(
        forwarding,
        ForwardingDrainContext {
            spec,
            runtime,
            registry,
            scope: DrainScope::Active,
        },
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
    registry: &Registry,
    snapshot: &SharedSnapshot,
    enabled: Option<bool>,
    reason: RouteCloseReason,
) -> Result<StopNotice, SuperviseError> {
    begin_forwarding_drain_with_timeout(
        spec,
        runtime,
        registry,
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
    registry: &Registry,
    snapshot: &SharedSnapshot,
    enabled: Option<bool>,
    reason: RouteCloseReason,
    drain_timeout: Duration,
) -> Result<StopNotice, SuperviseError> {
    let Some(forwarding) = runtime.forwarding.as_ref() else {
        return Ok(StopNotice::NotSent);
    };

    begin_forwarding_drain_with(
        forwarding,
        ForwardingDrainContext {
            spec,
            runtime,
            registry,
            scope: DrainScope::Active,
        },
        snapshot,
        enabled,
        reason,
        drain_timeout,
    )
    .await
}

async fn begin_forwarding_drain_with(
    forwarding: &ForwardingTable,
    context: ForwardingDrainContext<'_>,
    snapshot: &SharedSnapshot,
    enabled: Option<bool>,
    reason: RouteCloseReason,
    drain_timeout: Duration,
) -> Result<StopNotice, SuperviseError> {
    let ForwardingDrainContext {
        spec,
        runtime,
        registry,
        scope,
    } = context;
    debug_assert_ne!(reason, RouteCloseReason::Crash);
    let terminal = matches!(reason, RouteCloseReason::Disable);
    let drain_started_at = Instant::now();
    let drain_deadline = drain_started_at + drain_timeout;
    let deadline_ms =
        unix_ms_now().saturating_add(u64::try_from(drain_timeout.as_millis()).unwrap_or(u64::MAX));
    let busy_gauges = match scope {
        DrainScope::Active => declared_busy_gauges(registry, &spec.module_id)?,
        DrainScope::Endpoint(endpoint) => {
            declared_busy_gauges_for_connection(registry, endpoint.connection_id)?
        }
    };

    // Admission gate first: route.open/commit and route REQUEST admission are closed
    // before the first quiescence check, so the outstanding count can only fall.
    let gate_started = Instant::now();
    let drain_target = match scope {
        DrainScope::Active => forwarding.begin_module_drain(&spec.module_id, reason),
        DrainScope::Endpoint(endpoint) => forwarding.begin_endpoint_drain(endpoint, reason),
    }
    .map_err(SuperviseError::Forwarding)?;
    // The instant admission closed, and how long taking the forwarding write
    // lock to close it took. The timeout line reports only the quiescence
    // wait, so without this a drain that started late looked like one that
    // started on time.
    info!(
        module_id = %spec.module_id,
        ?reason,
        gate_ms = u64::try_from(gate_started.elapsed().as_millis()).unwrap_or(u64::MAX),
        connected = drain_target.is_some(),
        "module drain began; route admission closed"
    );
    if scope == DrainScope::Active {
        update_snapshot(snapshot, Some(&spec.module_id), |state| {
            state.state = ModuleState::Draining;
            state.draining_to_replace =
                matches!(reason, RouteCloseReason::Restart | RouteCloseReason::Reload);
            if let Some(enabled) = enabled {
                state.enabled = enabled;
            }
        })?;
    }

    let Some(target) = drain_target.as_ref() else {
        // Nothing was sent: the module has no registered connection to carry
        // `module.draining` or a GOODBYE. The caller must not assume the child
        // was asked to stop.
        return Ok(StopNotice::NoConnection);
    };
    {
        send_module_draining(&spec.module_id, reason, deadline_ms, target);
        let routes = forwarding
            .endpoint_routes(target.endpoint)
            .map_err(SuperviseError::Forwarding)?;
        let routes_notified = routes.len();
        crate::control::send_route_control_pushes(
            forwarding,
            routes.clone(),
            ClientControlPush::RouteClosing {
                module_id: spec.module_id.clone(),
                channels: Vec::new(),
                reason,
            },
        );
        send_route_goodbyes(forwarding, target.abandoned_bindings.clone());

        // `route.closing` was just sent above: from here on every return path,
        // including an early one, MUST send `route.closed` before propagating
        // anything else. A client holds `closing` as a promise that a verdict is
        // coming; leaving early without `closed` strands it waiting forever, since
        // `closing` carries no timeout of its own.
        let wait_result = wait_for_forwarding_quiescence(
            forwarding,
            &spec.module_id,
            runtime,
            target.endpoint,
            drain_deadline,
            &busy_gauges,
            scope,
        )
        .await;
        let drained = drained_after_quiescence_wait(&wait_result);
        if let Err(err) = &wait_result {
            error!(
                module_id = %spec.module_id,
                ?reason,
                error = %err,
                "forwarding quiescence wait failed after route.closing; forcing route.closed(drained: false) so the client is not left waiting on an unfulfilled promise"
            );
        } else if !drained {
            // Name what the drain waited on. Without it the line says only that
            // something did not settle, and "one wedged call" and "every
            // session's held stream" read the same; the first is a module bug,
            // the second is a module that should end its streams on
            // module.draining. Read before teardown releases the routes.
            let holdouts = forwarding
                .endpoint_drain_holdouts(target.endpoint)
                .unwrap_or_default();
            warn!(
                module_id = %spec.module_id,
                waited = ?drain_timeout,
                ?reason,
                held_requests = holdouts.requests,
                held_routes = holdouts.routes,
                total_routes = holdouts.total_routes,
                top_connections = ?holdouts.top_connections,
                // `module_channel:corr`, so the module can find each held request
                // in its own log; capped, so `held_requests` is the full count.
                held = %holdouts
                    .held
                    .iter()
                    .map(|(channel, corr)| format!("{channel}:{corr}"))
                    .collect::<Vec<_>>()
                    .join(","),
                "route drain timed out before request quiescence; forcing teardown"
            );
        }
        crate::control::send_route_control_pushes(
            forwarding,
            routes,
            ClientControlPush::RouteClosed {
                module_id: spec.module_id.clone(),
                channels: Vec::new(),
                reason,
                drained,
                abandoned: target.abandoned_bindings.len() as u32,
                excluded_subscriptions: target.excluded_subscriptions,
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
            excluded_subscriptions = target.excluded_subscriptions,
            drained,
            "module drain complete; consumers notified via route.closing/route.closed pushes and per-route GOODBYE frames"
        );
    }

    Ok(StopNotice::SentOverConnection)
}

/// Wait for the freshly spawned child to take the ACTIVE slot for `module_id`,
/// the only slot a plain (non-swap) spawn can register into.
async fn wait_for_registration_after_reload(
    registry: &Registry,
    module_id: &str,
    snapshot: &SharedSnapshot,
    child: &mut SupervisedChild,
    wait: Duration,
) -> Result<RegistrationWaitOutcome, SuperviseError> {
    wait_for_slot_registration(
        registry,
        crate::registry::RegistrationSlot::Active(module_id),
        module_id,
        snapshot,
        child,
        wait,
    )
    .await
}

/// Wait for `child` to register into `slot`, or to exit, or for `wait` to pass.
///
/// Keyed on the slot rather than the bare module id because during a swap the
/// id's active slot is already held by the incumbent: an id-keyed wait would
/// report the incumbent's registration as the candidate's and a candidate that
/// never registers would look registered. A swap candidate waits on
/// `crate::registry::RegistrationSlot::Candidate`.
async fn wait_for_slot_registration(
    registry: &Registry,
    slot: crate::registry::RegistrationSlot<'_>,
    module_id: &str,
    snapshot: &SharedSnapshot,
    child: &mut SupervisedChild,
    wait: Duration,
) -> Result<RegistrationWaitOutcome, SuperviseError> {
    let deadline = Instant::now() + wait;
    loop {
        if registry
            .registration(slot)
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
                return Ok(RegistrationWaitOutcome::Exited(classify_reaped_child_exit(
                    snapshot,
                    child,
                    &status,
                )));
            }
            _ = sleep(poll) => {}
        }
    }
}

fn registration_failure_exit_report(mut exit_report: ExitReport) -> ExitReport {
    // A replacement process that exits before HELLO did not provide service, even
    // if it used status 0. Count it against the restart cap as a new-binary failure.
    if exit_report.kind != ExitKind::DeliberateSeverance {
        exit_report.kind = ExitKind::Crash;
    }
    exit_report
}

async fn handle_reload_child_registration_failure(
    spec: &ModuleSpec,
    runtime: &SupervisorRuntimeConfig,
    registry: &Registry,
    process_liveness: &SupervisorProcessLiveness,
    snapshot: &SharedSnapshot,
    _child: &mut Option<SupervisedChild>,
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
        &runtime.spawn_events,
        &runtime.child_roster,
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
        NextAction::Restart { schedule } => {
            let delay = schedule.map_or(runtime.restart_policy.delay_for_restart(0), |schedule| {
                schedule.delay
            });
            if let Some(schedule) = schedule {
                log_crash_respawn(&spec.module_id, schedule);
            }
            schedule_respawn(
                runtime,
                snapshot,
                &spec.module_id,
                delay,
                RespawnKind::Spawn,
            )?;
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
    _child: &mut Option<SupervisedChild>,
    reason: String,
) -> Result<(), SuperviseError> {
    let now = Instant::now();
    let mut schedule = None;
    update_snapshot(snapshot, Some(&spec.module_id), |state| {
        clear_current_process_facts(state);
        if state.enabled {
            schedule = state.next_crash_restart(&runtime.restart_policy, now);
            state.state = if schedule.is_some() {
                ModuleState::Restarting
            } else {
                ModuleState::Failed
            };
        } else {
            state.state = ModuleState::Disabled;
        }
    })?;
    if let Some(schedule) = schedule {
        schedule_respawn(
            runtime,
            snapshot,
            &spec.module_id,
            schedule.delay,
            RespawnKind::Spawn,
        )?;
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
    protocol: ModuleProtocol,
    stop_notice: StopNotice,
    registry: &Registry,
    forwarding: Option<&ForwardingTable>,
    snapshot: &SharedSnapshot,
    terminal_ring: &Arc<Mutex<TerminalRing>>,
    spawn_events: &SpawnEventFeed,
    child: &mut Option<SupervisedChild>,
    drain_timeout: Duration,
    final_state: ModuleState,
    enabled: Option<bool>,
) -> Result<(), SuperviseError> {
    if let Some(child) = child.take() {
        drain_child_to_state(
            module_id,
            protocol,
            stop_notice,
            registry,
            forwarding,
            snapshot,
            terminal_ring,
            spawn_events,
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
        release_dead_registration(registry, forwarding, snapshot, module_id).await
    }
}

#[allow(clippy::too_many_arguments)]
async fn drain_child_to_state(
    module_id: &str,
    _protocol: ModuleProtocol,
    stop_notice: StopNotice,
    registry: &Registry,
    forwarding: Option<&ForwardingTable>,
    snapshot: &SharedSnapshot,
    terminal_ring: &Arc<Mutex<TerminalRing>>,
    spawn_events: &SpawnEventFeed,
    mut child: SupervisedChild,
    drain_timeout: Duration,
    final_state: ModuleState,
    enabled: Option<bool>,
) -> Result<(), SuperviseError> {
    let protocol = child.protocol;
    update_snapshot(snapshot, Some(module_id), |state| {
        state.state = ModuleState::Draining;
        state.draining_to_replace = final_state == ModuleState::Restarting;
        if let Some(enabled) = enabled {
            state.enabled = enabled;
        }
    })?;

    // The wait below is the same budget in every case; what differs is
    // whether anything has ASKED the child to stop before it starts. Only a
    // forwarding drain that reached the module's registered connection has
    // (`module.draining`, then a module GOODBYE). Every other child was told
    // nothing: a `protocol: "none"` module, which never registers; a subc
    // module spawned moments ago that has not sent HELLO yet; or a stop that
    // runs no forwarding drain. Without a signal the budget is only a delay
    // in front of SIGKILL -- and the not-yet-registered child is the worst
    // case, because it registers into a module that is already draining,
    // is never told, and is killed while healthy.
    if stop_notice != StopNotice::SentOverConnection {
        if protocol == ModuleProtocol::Subc && stop_notice == StopNotice::NoConnection {
            info!(
                module_id,
                pid = child.pid,
                budget_ms = u64::try_from(drain_timeout.as_millis()).unwrap_or(u64::MAX),
                "module has no connection yet; requesting stop by signal"
            );
        }
        request_graceful_stop(module_id, &child);
    }

    let exit_report = match timeout(drain_timeout, child.wait()).await {
        Ok(Ok(status)) => classify_reaped_child_exit(snapshot, &child, &status),
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
            //
            // Logged because the kill is otherwise visible only as signal 9 in
            // the terminal ring, and the budget it follows can be long enough
            // that consumers see a stretch of refusals with no stated cause.
            warn!(
                module_id,
                pid = child.pid,
                budget_ms = u64::try_from(drain_timeout.as_millis()).unwrap_or(u64::MAX),
                reason = ?final_state,
                ?stop_notice,
                "drain budget expired before the module exited; killing it"
            );
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
            classify_reaped_child_exit(snapshot, &child, &status)
        }
    };

    update_snapshot(snapshot, Some(module_id), |state| {
        state.state = final_state;
        if let Some(enabled) = enabled {
            state.enabled = enabled;
        }
        clear_current_process_facts(state);
        state.last_exit = Some(exit_report.clone());
        if exit_report.kind == ExitKind::DeliberateSeverance {
            state.lifetime_restarts += 1;
        }
    })?;
    let detail = lock_snapshot(snapshot)?.drain_disposition_detail.take();
    record_terminal_with_detail(
        module_id,
        terminal_ring,
        spawn_events,
        &exit_report,
        terminal_disposition(final_state),
        detail,
    );
    child.drain_stderr(module_id).await;

    release_dead_registration(registry, forwarding, snapshot, module_id).await
}

/// Ask a child that nothing else has asked to stop, by signal.
///
/// A registered subc module is asked over its own connection: the drain sends
/// `route.closing`/`route.closed` to its consumers, a GOODBYE per route, then a
/// module GOODBYE, and the module stops itself. A module that speaks no subc
/// wire receives none of that, and neither does a subc module that has not
/// registered yet, so for them the drain budget would be pure delay in front of
/// a SIGKILL -- and for a process with a store to flush (JetStream is the
/// reason `protocol: "none"` exists) a SIGKILL turns every ordinary teardown
/// into a recovery on the next start.
///
/// NEVER CALLED FOR A MODULE THAT WAS TOLD OVER ITS CONNECTION, and that is a
/// rule rather than an optimisation: that module's graceful stop is already
/// running by the time its child is drained, and a signal would race it.
///
/// Best-effort by construction. A child that has already exited is the ordinary
/// case rather than an error (the kill lands on a reaped or exiting pid), so a
/// failure is logged at debug and the wait-then-kill below still decides the
/// outcome.
#[cfg(unix)]
fn request_graceful_stop(module_id: &str, child: &SupervisedChild) {
    let Some(pid) = child
        .id()
        .and_then(|pid| i32::try_from(pid).ok())
        .and_then(rustix::process::Pid::from_raw)
    else {
        debug!(
            module_id,
            "no pid to signal for teardown; falling through to the drain wait"
        );
        return;
    };
    match rustix::process::kill_process(pid, rustix::process::Signal::TERM) {
        Ok(()) => debug!(
            module_id,
            "sent SIGTERM to a module nothing else asked to stop"
        ),
        Err(err) => debug!(
            module_id,
            error = %err,
            "SIGTERM to module failed; the drain wait and kill still apply"
        ),
    }
}

/// Windows has no SIGTERM and no portable stand-in for one. The graceful stops
/// Windows does offer need cooperation this supervisor cannot assume: a console
/// control event requires sharing a console with the child, and `WM_CLOSE`
/// requires the child to pump a message loop. A supervised server process does
/// neither, so there is nothing to send and teardown is the wait followed by the
/// kill. Emulating a signal here would mean inventing a stop protocol, which is
/// the thing `protocol: "none"` exists to avoid.
#[cfg(not(unix))]
fn request_graceful_stop(module_id: &str, _child: &SupervisedChild) {
    debug!(
        module_id,
        "no graceful stop signal exists on this platform; teardown of a module nothing asked to stop waits, then kills"
    );
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

/// Release a reaped child's registration before allowing another spawn.
///
/// EOF is not a process-lifetime signal: an inherited socket can stay open
/// indefinitely, and serial frame dispatch can be waiting on egress instead of
/// reading EOF. After the normal release grace, request connection close (which
/// cancels both reads and dispatch), then allow one more release grace for the
/// connection guard's forwarding cleanup. Never evict a different connection.
async fn release_dead_registration(
    registry: &Registry,
    forwarding: Option<&ForwardingTable>,
    snapshot: &SharedSnapshot,
    module_id: &str,
) -> Result<(), SuperviseError> {
    let result = async {
        let registration = registry
            .get_module(module_id)
            .map_err(SuperviseError::Registry)?;
        match wait_for_registration_release(registry, module_id, REGISTRY_RELEASE_TIMEOUT).await {
            Ok(()) => return Ok(()),
            Err(SuperviseError::RegistrationStillActive { .. }) => {}
            Err(err) => return Err(err),
        }
        let pid = lock_snapshot(snapshot)?.reaped_pid;
        if let (Some(registration), Some(forwarding), Some(pid)) = (registration, forwarding, pid) {
            warn!(
                module_id,
                pid,
                connection_id = registration.connection_id.get(),
                "reaped module registration outlived release grace; closing dead connection"
            );
            forwarding.request_connection_close(
                registration.connection_id,
                CloseReason::new(
                    "supervised_process_reaped",
                    format!("module '{module_id}' pid {pid} exited"),
                ),
            );
            wait_for_slot_registration_release(
                registry,
                crate::registry::RegistrationSlot::Connection(registration.connection_id),
                REGISTRY_RELEASE_TIMEOUT,
            )
            .await?;
        }
        wait_for_registration_release(registry, module_id, Duration::ZERO).await
    }
    .await;
    if let Err(err) = &result {
        fail_snapshot(snapshot, Some(module_id), None);
        error!(module_id, error = %err, "registration release failed after child exit; module is failed and start can retry");
    }
    result
}

/// Wait for the ACTIVE registration of `module_id` to go away, which is what a
/// plain stop or restart waits for before it spawns a replacement.
async fn wait_for_registration_release(
    registry: &Registry,
    module_id: &str,
    wait: Duration,
) -> Result<(), SuperviseError> {
    wait_for_slot_registration_release(
        registry,
        crate::registry::RegistrationSlot::Active(module_id),
        wait,
    )
    .await
}

/// Wait for the registration in `slot` to go away.
///
/// Keyed on the slot rather than the bare module id because a successful swap
/// never empties the id's active slot (the promoted candidate is in it), so an
/// id-keyed wait for the incumbent's release would always time out. Draining a
/// swap's incumbent waits on `crate::registry::RegistrationSlot::Connection` with the
/// incumbent's connection instead.
async fn wait_for_slot_registration_release(
    registry: &Registry,
    slot: crate::registry::RegistrationSlot<'_>,
    wait: Duration,
) -> Result<(), SuperviseError> {
    let deadline = Instant::now() + wait;
    let mut release_events = registration_release_events().subscribe();
    let still_active = |registration: &crate::registry::ModuleRegistration| {
        SuperviseError::RegistrationStillActive {
            module_id: registration.manifest.module_id.clone(),
            waited: wait,
        }
    };
    loop {
        let _observed_generation = *release_events.borrow_and_update();
        let Some(registration) = registry
            .registration(slot)
            .map_err(SuperviseError::Registry)?
        else {
            return Ok(());
        };

        let now = Instant::now();
        if now >= deadline {
            return Err(still_active(&registration));
        }

        let remaining = deadline.saturating_duration_since(now);
        match timeout(remaining, release_events.changed()).await {
            Ok(Ok(())) | Ok(Err(_)) => {}
            Err(_) => return Err(still_active(&registration)),
        }
    }
}

#[cfg(test)]
mod slot_registration_wait_tests {
    use super::*;
    use crate::registry::{ConnectionId, RegistrationSlot};
    use subc_protocol::manifest::ModuleManifest;

    #[tokio::test]
    async fn enable_release_failure_is_failed_and_a_second_enable_retries() {
        let registry = Arc::new(Registry::default());
        let supervisor = Supervisor::new_for_test(Arc::clone(&registry), RestartPolicy::default());
        let runtime = supervisor.runtime_config();
        let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::disabled()));
        let spec = ModuleSpec {
            module_id: "enable-stale-registration".to_string(),
            program: PathBuf::from("/missing/enable-retry-test"),
            args: Vec::new(),
            env: Vec::new(),
            reserved: false,
            reserved_prefixes: Vec::new(),
            protocol: ModuleProtocol::Subc,
            overlap: Default::default(),
        };
        let connection = ConnectionId::new(90);
        registry
            .register_with_control_ops(
                ModuleManifest::builder(&spec.module_id, "0.1.0").build(),
                1,
                connection,
                Vec::new(),
            )
            .unwrap();
        let mut child = None;
        let err = set_child_enabled(
            &spec,
            &runtime,
            &registry,
            &supervisor.process_liveness,
            &snapshot,
            &mut child,
            true,
        )
        .await
        .unwrap_err();
        assert!(matches!(
            err,
            SuperviseError::RegistrationStillActive { .. }
        ));
        assert_eq!(lock_snapshot(&snapshot).unwrap().state, ModuleState::Failed);
        assert!(child.is_none());
        registry.deregister_connection(connection).unwrap();
        let err = set_child_enabled(
            &spec,
            &runtime,
            &registry,
            &supervisor.process_liveness,
            &snapshot,
            &mut child,
            true,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, SuperviseError::Spawn { .. }),
            "second enable must attempt a spawn: {err}"
        );
        assert_eq!(lock_snapshot(&snapshot).unwrap().state, ModuleState::Failed);
    }

    const INCUMBENT: u64 = 1;
    const CANDIDATE: u64 = 2;

    fn swapped_registry() -> Arc<Registry> {
        let registry = Arc::new(Registry::default());
        let manifest = ModuleManifest::builder("m", "0.1.0").build();
        registry
            .register_with_control_ops(
                manifest.clone(),
                1,
                ConnectionId::new(INCUMBENT),
                Vec::new(),
            )
            .unwrap();
        registry
            .register_candidate_with_control_ops(
                manifest,
                1,
                ConnectionId::new(CANDIDATE),
                Vec::new(),
            )
            .unwrap();
        registry
    }

    /// After a promotion the id's active slot is held by the new process, so an
    /// id-keyed wait for the incumbent's release can never succeed; the
    /// connection-keyed wait completes as soon as the incumbent deregisters.
    #[tokio::test]
    async fn incumbent_release_is_awaited_by_connection_not_by_module_id() {
        let registry = swapped_registry();
        registry.promote_candidate("m").unwrap().unwrap();

        assert!(matches!(
            wait_for_registration_release(&registry, "m", Duration::from_millis(50)).await,
            Err(SuperviseError::RegistrationStillActive { .. })
        ));

        // Still held while the incumbent's connection has not deregistered.
        assert!(matches!(
            wait_for_slot_registration_release(
                &registry,
                RegistrationSlot::Connection(ConnectionId::new(INCUMBENT)),
                Duration::from_millis(50),
            )
            .await,
            Err(SuperviseError::RegistrationStillActive { .. })
        ));

        let releaser = Arc::clone(&registry);
        let release = tokio::spawn(async move {
            sleep(Duration::from_millis(20)).await;
            releaser
                .deregister_connection(ConnectionId::new(INCUMBENT))
                .unwrap();
            notify_registration_release();
        });
        wait_for_slot_registration_release(
            &registry,
            RegistrationSlot::Connection(ConnectionId::new(INCUMBENT)),
            Duration::from_secs(5),
        )
        .await
        .expect("the incumbent's own registration is released");
        release.await.unwrap();
        assert!(registry.get_module("m").unwrap().is_some());
    }

    /// The candidate slot is waited on separately from the active slot: the
    /// incumbent's registration neither holds up nor stands in for it.
    #[tokio::test]
    async fn candidate_slot_wait_ignores_the_incumbents_registration() {
        let registry = swapped_registry();
        assert!(matches!(
            wait_for_slot_registration_release(
                &registry,
                RegistrationSlot::Candidate("m"),
                Duration::from_millis(50),
            )
            .await,
            Err(SuperviseError::RegistrationStillActive { .. })
        ));
        registry
            .deregister_connection(ConnectionId::new(CANDIDATE))
            .unwrap();
        wait_for_slot_registration_release(
            &registry,
            RegistrationSlot::Candidate("m"),
            Duration::from_millis(50),
        )
        .await
        .expect("a candidate slot with no candidate is released");
        assert!(registry
            .registration(RegistrationSlot::Active("m"))
            .unwrap()
            .is_some());
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

/// Give an operator-touched module its full crash budget back.
///
/// Named for the counter it used to zero; it now empties the in-window ring,
/// which is the same act. `lifetime_restarts` is untouched on purpose -- the
/// ledger of what happened survives every operator action.
fn reset_restart_count(snapshot: &SharedSnapshot, module_id: &str) -> Result<(), SuperviseError> {
    update_snapshot(snapshot, Some(module_id), |state| {
        state.clear_crash_restarts();
    })
}

fn set_running(
    snapshot: &SharedSnapshot,
    child: &SupervisedChild,
    module_id: &str,
    spawn_events: &SpawnEventFeed,
) -> Result<(), SuperviseError> {
    let mut state = snapshot.lock().map_err(|_| SuperviseError::StatePoisoned {
        module_id: Some(module_id.to_string()),
    })?;
    state.spawn_generation = spawn_events.emit_spawned(module_id, child.pid, child.spawned_at_ms);
    if std::mem::take(&mut state.coalesced_restart_pending) {
        let generation = state.spawn_generation;
        info!(module_id, "restart already satisfied by generation {generation}; coalesced pending request completed");
    }
    state.drain_disposition_detail = None;
    state.spawn_failure = None;
    // Every caller of this is a plain spawn, which always uses the primary key;
    // a promoted swap candidate sets the flag itself after this returns.
    state.in_alternate_slot = false;
    state.configuration_updated_since_spawn = false;
    state.spawned_protocol = Some(child.protocol);
    state.state = ModuleState::Running;
    state.enabled = true;
    state.process_alive = true;
    state.pid = child.id();
    #[cfg(target_os = "macos")]
    {
        state.report_ready = Some(Arc::clone(&child.report_ready));
    }
    state.spawned_at_ms = Some(child.spawned_at_ms);
    state.spawned_from = Some(child.spawned_from.clone());
    state.spawned_file_identity = child.spawned_file_identity;
    state.process_start_time = child.process_start_time;
    Ok(())
}

fn clear_current_process_facts(state: &mut SupervisorSnapshot) {
    state.process_alive = false;
    state.spawned_protocol = None;
    state.pid = None;
    #[cfg(target_os = "macos")]
    {
        state.report_ready = None;
    }
    state.spawned_at_ms = None;
    state.spawned_from = None;
    state.spawned_file_identity = None;
    state.process_start_time = None;
    state.deliberate_severance = None;
}

#[cfg(test)]
fn record_deliberate_severance(
    snapshot: &SharedSnapshot,
    identity: ProcessIdentity,
) -> Result<(), SuperviseError> {
    update_snapshot(snapshot, None, |state| {
        state.deliberate_severance = Some(identity);
    })
}

fn apply_deliberate_severance_marker(
    snapshot: &SharedSnapshot,
    exited_identity: Option<ProcessIdentity>,
    mut exit_report: ExitReport,
) -> ExitReport {
    let marker = lock_snapshot(snapshot)
        .ok()
        .and_then(|mut state| state.deliberate_severance.take());
    if marker.is_some() && marker == exited_identity {
        exit_report.kind = ExitKind::DeliberateSeverance;
    }
    exit_report
}

fn classify_reaped_child_exit(
    snapshot: &SharedSnapshot,
    child: &SupervisedChild,
    status: &ExitStatus,
) -> ExitReport {
    let _ = update_snapshot(snapshot, None, |state| {
        state.reaped_pid = Some(child.pid);
        state.spawn_failure = child.spawn_failure.clone();
    });
    apply_deliberate_severance_marker(snapshot, child.process_identity(), classify_exit(status))
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
        apply_deliberate_severance_marker, daemon_will_restart, drain_child_to_state,
        drained_after_quiescence_wait, handle_reload_spawn_failure, health_restart_child,
        lock_snapshot, on_child_exit, record_deliberate_severance, record_wait_error_terminal,
        reset_restart_count, spawn_and_mark_running, update_snapshot, wait_error_exit_report,
        ExitKind, ExitReport, ModuleProtocol, ModuleSpec, ModuleState, NextAction, ProcessIdentity,
        RestartPolicy, SpawnEventKind, StopNotice, SuperviseError, SupervisedModule, Supervisor,
        SupervisorHandle, SupervisorHealthStatus, SupervisorSnapshot,
    };
    // The supervisor's clock, distinct from the `std::time::Instant` these tests
    // use for their own wall-clock deadlines: crash-restart instants must be on
    // the same clock the production code stamps them with, which is tokio's (and
    // is what `start_paused` tests can move).
    use super::Instant as ClockInstant;
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
    pub(super) fn fake_aft_stub_path() -> PathBuf {
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
            protocol: ModuleProtocol::Subc,
            overlap: Default::default(),
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
            protocol: ModuleProtocol::Subc,
            overlap: Default::default(),
        });
        assert!(supervisor
            .reserved_hello_rejection("never-spawned", Some("minted"))
            .is_none());
        assert!(supervisor
            .reserved_hello_rejection("never-spawned", Some("forged"))
            .is_some());
    }

    /// Put `count` crash restarts on a snapshot's ring as if they had all just
    /// happened, which is what "spent budget" looks like to every reader.
    fn seed_crash_restarts(state: &mut SupervisorSnapshot, count: u32) {
        let now = ClockInstant::now();
        for _ in 0..count {
            state.crash_restarts.push_back(now);
        }
    }

    /// Age the oldest recorded restart out of `window`, standing in for the hours
    /// that would otherwise have to pass. Injecting the instant is the point: a
    /// test that slept a real window would take ten minutes and still prove less.
    fn age_oldest_crash_restart_out_of_window(state: &mut SupervisorSnapshot, window: Duration) {
        let aged = state
            .crash_restarts
            .front()
            .expect("a crash restart must be recorded before it can be aged")
            .checked_sub(window + Duration::from_secs(1))
            .expect("the test clock is far enough from its origin to age an instant");
        state.crash_restarts[0] = aged;
    }

    fn snapshot_with_restarts(enabled: bool, count: u32) -> SupervisorSnapshot {
        let mut state = SupervisorSnapshot::new(ModuleState::Running, enabled);
        seed_crash_restarts(&mut state, count);
        state
    }

    #[test]
    fn daemon_owned_recovery_predicate_uses_the_pre_increment_budget() {
        let policy = RestartPolicy::new(3, Duration::ZERO);
        let now = ClockInstant::now();
        assert!(daemon_will_restart(
            &mut snapshot_with_restarts(true, 2),
            &policy,
            now
        ));
        assert!(!daemon_will_restart(
            &mut snapshot_with_restarts(true, 3),
            &policy,
            now
        ));
        assert!(!daemon_will_restart(
            &mut snapshot_with_restarts(false, 0),
            &policy,
            now
        ));
    }

    #[test]
    fn crash_restart_backoff_escalates_with_in_window_count() {
        let policy = RestartPolicy::new(4, Duration::from_millis(100))
            .with_max_backoff(Duration::from_secs(30));
        let now = ClockInstant::now();
        let mut state = SupervisorSnapshot::new(ModuleState::Running, true);
        let schedules = (0..4)
            .map(|_| {
                state
                    .next_crash_restart(&policy, now)
                    .expect("the test policy allows four crash restarts")
            })
            .collect::<Vec<_>>();

        assert_eq!(
            schedules
                .iter()
                .map(|schedule| schedule.restart_in_window)
                .collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
        assert_eq!(
            schedules
                .iter()
                .map(|schedule| schedule.delay)
                .collect::<Vec<_>>(),
            vec![
                Duration::from_millis(100),
                Duration::from_secs(1),
                Duration::from_secs(10),
                Duration::from_secs(30),
            ]
        );
    }

    #[test]
    fn crash_restart_backoff_resets_after_ring_clear() {
        let policy = RestartPolicy::new(3, Duration::from_millis(100));
        let now = ClockInstant::now();
        let mut state = SupervisorSnapshot::new(ModuleState::Running, true);
        assert_eq!(
            state.next_crash_restart(&policy, now).unwrap().delay,
            Duration::from_millis(100)
        );
        assert_eq!(
            state.next_crash_restart(&policy, now).unwrap().delay,
            Duration::from_secs(1)
        );

        state.clear_crash_restarts();
        let schedule = state
            .next_crash_restart(&policy, now)
            .expect("a cleared ring must allow another restart");
        assert_eq!(schedule.restart_in_window, 0);
        assert_eq!(schedule.delay, Duration::from_millis(100));
    }

    #[test]
    fn crash_restart_backoff_ignores_aged_restarts() {
        let policy = RestartPolicy::new(3, Duration::from_millis(100));
        let now = ClockInstant::now();
        let mut state = SupervisorSnapshot::new(ModuleState::Running, true);
        state
            .next_crash_restart(&policy, now)
            .expect("the first restart is allowed");
        state
            .next_crash_restart(&policy, now)
            .expect("the second restart is allowed");
        state.crash_restarts[0] = now
            .checked_sub(policy.window + Duration::from_secs(1))
            .expect("the fake clock can age a restart past the window");

        let schedule = state
            .next_crash_restart(&policy, now)
            .expect("an aged restart must release its slot");
        assert_eq!(schedule.restart_in_window, 1);
        assert_eq!(schedule.delay, Duration::from_secs(1));
        assert_eq!(state.crash_restarts.len(), 2);
    }

    /// The budget is a rate: the same three spent restarts refuse a respawn
    /// while they are recent and allow one once they have aged past the window.
    /// Nothing about the module changed in between, which is the whole point.
    #[test]
    fn a_budget_spent_before_the_window_no_longer_refuses() {
        let policy = RestartPolicy::new(3, Duration::ZERO);
        let mut state = snapshot_with_restarts(true, 3);
        let now = ClockInstant::now();
        assert!(!daemon_will_restart(&mut state, &policy, now));

        assert!(daemon_will_restart(
            &mut state,
            &policy,
            now + policy.window + Duration::from_secs(1)
        ));
        assert!(
            state.crash_restarts.is_empty(),
            "reading the budget must drop the instants that left the window"
        );
    }

    fn module_with_recovery_snapshot(
        state: ModuleState,
        enabled: bool,
        restart_count: u32,
    ) -> SupervisedModule {
        let registry = Arc::new(Registry::default());
        let supervisor =
            Supervisor::new_for_test(Arc::clone(&registry), RestartPolicy::new(3, Duration::ZERO));
        let module = supervisor
            .spawn(ModuleSpec {
                module_id: "recovery-snapshot".to_string(),
                program: fake_aft_stub_path(),
                args: Vec::new(),
                env: Vec::new(),
                reserved: false,
                reserved_prefixes: Vec::new(),
                protocol: ModuleProtocol::Subc,
                overlap: Default::default(),
            })
            .unwrap();
        update_snapshot(
            &module.inner.snapshot,
            Some("recovery-snapshot"),
            |snapshot| {
                snapshot.state = state;
                snapshot.enabled = enabled;
                seed_crash_restarts(snapshot, restart_count);
            },
        )
        .unwrap();
        module
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn no_cgroup_placement_does_not_block_fake_aft_stub_spawn() {
        let supervisor =
            Supervisor::new_for_test(Arc::new(Registry::default()), RestartPolicy::default())
                .with_cgroup_placement(None);
        let result = supervisor.spawn(ModuleSpec {
            module_id: "no-cgroup-placement".to_string(),
            program: fake_aft_stub_path(),
            args: Vec::new(),
            env: Vec::new(),
            reserved: false,
            reserved_prefixes: Vec::new(),
            protocol: ModuleProtocol::Subc,
            overlap: Default::default(),
        });

        assert!(
            result.is_ok(),
            "no delegation must not turn an otherwise valid spawn into a failure: {result:?}"
        );
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
            Supervisor::new_for_test(Arc::clone(&registry), RestartPolicy::new(1, Duration::ZERO));
        let module = supervisor
            .spawn(ModuleSpec {
                module_id: "terminal-history".to_string(),
                program: fake_aft_stub_path(),
                args: Vec::new(),
                env: vec![("FAKE_AFT_EXIT_CODE".to_string(), "23".to_string())],
                reserved: false,
                reserved_prefixes: Vec::new(),
                protocol: ModuleProtocol::Subc,
                overlap: Default::default(),
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

    /// A disable issued while a crash respawn is still backing off must preempt
    /// that respawn: the operator's stop wins, the disable must not queue behind
    /// the backoff, and the module must never come back up afterwards.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn disable_during_crash_backoff_cancels_pending_respawn() {
        let backoff = Duration::from_secs(2);
        let supervisor = Supervisor::new_for_test(
            Arc::new(Registry::default()),
            RestartPolicy::new(10, backoff),
        );
        let module = supervisor
            .spawn(ModuleSpec {
                module_id: "disable-during-backoff".to_string(),
                program: fake_aft_stub_path(),
                args: Vec::new(),
                env: vec![("FAKE_AFT_EXIT_CODE".to_string(), "23".to_string())],
                reserved: false,
                reserved_prefixes: Vec::new(),
                protocol: ModuleProtocol::Subc,
                overlap: Default::default(),
            })
            .unwrap();

        // Wait for the first crash to put the module into its backoff window.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if module.status().unwrap().state == ModuleState::Restarting {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "module never entered the crash backoff"
            );
            sleep(Duration::from_millis(10)).await;
        }

        let started = Instant::now();
        module.set_enabled(false).await.unwrap();
        let waited = started.elapsed();

        assert!(
            waited < backoff / 2,
            "disable waited {waited:?} behind the {backoff:?} crash backoff; the operator command must preempt the pending respawn"
        );
        assert_eq!(module.status().unwrap().state, ModuleState::Disabled);

        // Outlast the backoff: the respawn it was counting down to must never run.
        sleep(backoff + Duration::from_millis(500)).await;
        let status = module.status().unwrap();
        assert_eq!(status.state, ModuleState::Disabled);
        assert_eq!(
            status.spawn_generation, 1,
            "module respawned after the operator disabled it"
        );
    }

    /// A `protocol: "none"` child that stays parked and exits 0 on SIGTERM:
    /// the shape of nats-server, the program this rule exists for.
    #[cfg(unix)]
    fn protocol_none_sigterm_exits_clean_spec(
        module_id: &str,
        dir: &std::path::Path,
    ) -> (ModuleSpec, PathBuf, PathBuf) {
        let ready = dir.join("ready");
        let marker = dir.join("sigterm");
        let spec = ModuleSpec {
            module_id: module_id.to_string(),
            program: fake_aft_stub_path(),
            args: Vec::new(),
            env: vec![
                ("FAKE_AFT_NEVER_CONNECT".to_string(), "1".to_string()),
                (
                    "FAKE_AFT_SIGTERM_MARKER_PATH".to_string(),
                    marker.display().to_string(),
                ),
                (
                    "FAKE_AFT_NEVER_CONNECT_READY_PATH".to_string(),
                    ready.display().to_string(),
                ),
            ],
            reserved: false,
            reserved_prefixes: Vec::new(),
            protocol: ModuleProtocol::None,
            overlap: Default::default(),
        };
        (spec, ready, marker)
    }

    /// Wait for a file the child writes, so a signal is never sent before the
    /// child's SIGTERM handler is installed (the default disposition would
    /// kill it by signal and the exit would not be clean).
    #[cfg(unix)]
    async fn wait_for_file(path: &std::path::Path) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !path.exists() {
            assert!(
                Instant::now() < deadline,
                "{} never appeared",
                path.display()
            );
            sleep(Duration::from_millis(10)).await;
        }
    }

    /// A protocol-none module that exits 0 because something OUTSIDE the
    /// supervisor sent it SIGTERM is respawned, and the terminal record carries
    /// the crash-path disposition rather than `stopped`.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn protocol_none_unrequested_clean_exit_restarts_as_a_crash() {
        let dir = subc_test_support::TestTempDir::new("none-unrequested-clean-exit");
        let (spec, ready, marker) =
            protocol_none_sigterm_exits_clean_spec("none-unrequested-clean-exit", dir.path());
        let supervisor = Supervisor::new_for_test(
            Arc::new(Registry::default()),
            RestartPolicy::new(3, Duration::ZERO),
        );
        let module = supervisor.spawn(spec).unwrap();
        wait_for_file(&ready).await;
        let first_pid = module
            .status()
            .unwrap()
            .pid
            .expect("a running module reports its pid");

        rustix::process::kill_process(
            rustix::process::Pid::from_raw(i32::try_from(first_pid).unwrap()).unwrap(),
            rustix::process::Signal::TERM,
        )
        .unwrap();

        let deadline = Instant::now() + Duration::from_secs(10);
        let respawned = loop {
            let status = module.status().unwrap();
            if status.state == ModuleState::Running
                && status.pid.is_some_and(|pid| pid != first_pid)
            {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "protocol-none module was not respawned after an unrequested clean exit: {status:?}"
            );
            sleep(Duration::from_millis(10)).await;
        };
        assert_eq!(respawned.spawn_generation, 2);
        assert!(
            marker.exists(),
            "the child must have exited through its SIGTERM handler (exit 0), or this proves nothing about clean exits"
        );

        let history = module.terminal_history();
        assert_eq!(history.entries.len(), 1, "{history:?}");
        let entry = &history.entries[0];
        assert_eq!(entry.exit_code, Some(0));
        assert_eq!(entry.exit_kind, subc_control::TerminalExitKind::Clean);
        assert_eq!(entry.disposition, TerminalDisposition::Restarting);

        module.stop().await.unwrap();
    }

    /// Repeated unrequested clean exits of a protocol-none module spend the
    /// restart budget exactly as crashes do, and the module ends `failed` with
    /// the budget named.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn protocol_none_repeated_clean_exits_exhaust_the_restart_budget() {
        let supervisor = Supervisor::new_for_test(
            Arc::new(Registry::default()),
            RestartPolicy::new(1, Duration::ZERO),
        );
        let module = supervisor
            .spawn(ModuleSpec {
                module_id: "none-clean-exit-budget".to_string(),
                program: fake_aft_stub_path(),
                args: Vec::new(),
                env: vec![("FAKE_AFT_EXIT_CODE".to_string(), "0".to_string())],
                reserved: false,
                reserved_prefixes: Vec::new(),
                protocol: ModuleProtocol::None,
                overlap: Default::default(),
            })
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let status = module.status().unwrap();
            if status.state == ModuleState::Failed {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "module never exhausted its budget: {status:?} {:?}",
                module.terminal_history()
            );
            sleep(Duration::from_millis(10)).await;
        }
        let history = module.terminal_history();
        assert_eq!(
            history
                .entries
                .iter()
                .map(|entry| (entry.exit_code, entry.disposition.clone()))
                .collect::<Vec<_>>(),
            vec![
                (Some(0), TerminalDisposition::Restarting),
                (Some(0), TerminalDisposition::Failed),
            ]
        );
        let detail = history.entries[1]
            .disposition_detail
            .as_deref()
            .expect("a budget failure names the budget");
        assert!(detail.contains("max_restarts=1"), "{detail}");
        assert_eq!(module.status().unwrap().spawn_generation, 2);
    }

    /// A stop the supervisor itself requests still stops a protocol-none
    /// module, even though the child answers the SIGTERM with exit 0.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn protocol_none_requested_stop_and_disable_do_not_respawn() {
        for disable in [false, true] {
            let label = if disable {
                "none-requested-disable"
            } else {
                "none-requested-stop"
            };
            let dir = subc_test_support::TestTempDir::new(label);
            let (spec, ready, marker) = protocol_none_sigterm_exits_clean_spec(label, dir.path());
            let supervisor = Supervisor::new_for_test(
                Arc::new(Registry::default()),
                RestartPolicy::new(3, Duration::ZERO),
            );
            let module = supervisor.spawn(spec).unwrap();
            wait_for_file(&ready).await;

            if disable {
                module.set_enabled(false).await.unwrap();
            } else {
                module.stop().await.unwrap();
            }
            assert!(
                marker.exists(),
                "{label}: the child must have left through its SIGTERM handler with exit 0"
            );

            // Long enough for a zero-backoff respawn to have happened if the
            // exit had been treated as a crash.
            sleep(Duration::from_millis(500)).await;
            let status = module.status().unwrap();
            let expected = if disable {
                ModuleState::Disabled
            } else {
                ModuleState::Stopped
            };
            assert_eq!(status.state, expected, "{label}");
            assert_eq!(
                status.spawn_generation, 1,
                "{label}: respawned after a requested stop"
            );
            let history = module.terminal_history();
            assert_eq!(history.entries.len(), 1, "{label}: {history:?}");
            assert_eq!(history.entries[0].exit_code, Some(0), "{label}");
            assert_ne!(
                history.entries[0].disposition,
                TerminalDisposition::Restarting,
                "{label}"
            );
        }
    }

    /// A subc-wire module that exits 0 on its own is still a stop: the
    /// protocol-none rule must not reach it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn subc_wire_clean_exit_is_still_a_stop() {
        let supervisor = Supervisor::new_for_test(
            Arc::new(Registry::default()),
            RestartPolicy::new(3, Duration::ZERO),
        );
        let module = supervisor
            .spawn(ModuleSpec {
                module_id: "wire-clean-exit".to_string(),
                program: fake_aft_stub_path(),
                args: Vec::new(),
                env: vec![("FAKE_AFT_EXIT_CODE".to_string(), "0".to_string())],
                reserved: false,
                reserved_prefixes: Vec::new(),
                protocol: ModuleProtocol::Subc,
                overlap: Default::default(),
            })
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(10);
        while module.terminal_history().entries.is_empty() {
            assert!(Instant::now() < deadline, "module never exited");
            sleep(Duration::from_millis(10)).await;
        }
        // Long enough for a zero-backoff respawn to have happened.
        sleep(Duration::from_millis(500)).await;
        let status = module.status().unwrap();
        assert_eq!(status.state, ModuleState::Stopped);
        assert_eq!(status.spawn_generation, 1);
        let history = module.terminal_history();
        assert_eq!(history.entries.len(), 1, "{history:?}");
        assert_eq!(history.entries[0].exit_code, Some(0));
        assert_eq!(history.entries[0].disposition, TerminalDisposition::Stopped);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reload_exit_keeps_roster_until_terminal_is_recorded_during_shutdown() {
        let dir = subc_test_support::TestTempDir::new("reload-roster-terminal-order");
        let record = dir.join("live-children.json");
        let supervisor = Supervisor::new_for_test(
            Arc::new(Registry::default()),
            RestartPolicy::new(0, Duration::ZERO),
        );
        let mut runtime = supervisor.runtime_config();
        runtime.child_roster.record_to(record.clone());
        let gate = Arc::new(super::ReloadExitRecordGate::default());
        runtime.test_reload_exit_record_gate = Some(Arc::clone(&gate));
        let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::starting()));
        let spec = ModuleSpec {
            module_id: "reload-exit-roster".into(),
            program: fake_aft_stub_path(),
            args: Vec::new(),
            env: vec![("FAKE_AFT_EXIT_CODE".into(), "23".into())],
            reserved: false,
            reserved_prefixes: Vec::new(),
            protocol: ModuleProtocol::Subc,
            overlap: Default::default(),
        };
        let mut child = None;
        let reload = super::finish_reload_child(
            &spec,
            &runtime,
            &supervisor.registry,
            &supervisor.process_liveness,
            &snapshot,
            &mut child,
        );
        tokio::pin!(reload);
        tokio::select! {
            result = &mut reload => panic!("reload missed the before-record gate: {result:?}"),
            _ = gate.reached.notified() => {}
        }
        assert!(runtime
            .terminal_ring
            .lock()
            .unwrap()
            .snapshot()
            .entries
            .is_empty());
        assert_eq!(
            crate::live_children::read_record(&record).unwrap().len(),
            1,
            "shutdown must still wait for the reaped child until its terminal record exists"
        );
        runtime.child_roster.close();
        gate.resume.notify_one();
        assert!(reload.await.is_err());
        assert!(crate::live_children::read_record(&record)
            .unwrap()
            .is_empty());
        let history = runtime.terminal_ring.lock().unwrap().snapshot();
        assert_eq!(history.entries.len(), 1);
        assert_eq!(
            history.entries[0].disposition,
            TerminalDisposition::DaemonShutdown
        );
    }

    /// Each restart-producing arm has its own state transition. Keeping their
    /// lifetime count assertions adjacent prevents a later new arm from silently
    /// spending budget without recording the historical restart.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn every_restart_increment_path_advances_lifetime_count() {
        let supervisor = Supervisor::new_for_test(
            Arc::new(Registry::default()),
            RestartPolicy::new(1, Duration::ZERO),
        );
        let runtime = supervisor.runtime_config();
        let spec = ModuleSpec {
            module_id: "lifetime-increment-path".to_string(),
            program: PathBuf::from("/unused/lifetime-increment-path"),
            args: Vec::new(),
            env: Vec::new(),
            reserved: false,
            reserved_prefixes: Vec::new(),
            protocol: ModuleProtocol::Subc,
            overlap: Default::default(),
        };

        let crash_snapshot = Arc::new(Mutex::new(SupervisorSnapshot::starting()));
        assert!(matches!(
            on_child_exit(
                &spec,
                runtime.restart_policy,
                &supervisor.registry,
                &crash_snapshot,
                &runtime.terminal_ring,
                &runtime.spawn_events,
                &runtime.child_roster,
                ExitReport {
                    kind: ExitKind::Crash,
                    code: Some(1),
                    signal: None,
                    at_ms: 1,
                },
            )
            .await,
            NextAction::Restart { schedule: _ }
        ));
        let (crash_restarts, crash_lifetime) = {
            let state = lock_snapshot(&crash_snapshot).unwrap();
            (state.crash_restarts.len(), state.lifetime_restarts)
        };
        assert_eq!(crash_restarts, 1);
        assert_eq!(crash_lifetime, 1);

        let health_snapshot = Arc::new(Mutex::new(SupervisorSnapshot::starting()));
        let mut health_child = None;
        assert!(matches!(
            health_restart_child(
                &spec,
                &runtime,
                &supervisor.registry,
                &supervisor.process_liveness,
                &health_snapshot,
                &mut health_child,
                SupervisorHealthStatus::Failing,
                None,
                2,
            )
            .await,
            Ok(())
        ));
        assert!(health_child.is_none());
        assert!(lock_snapshot(&health_snapshot).unwrap().respawn_pending);
        assert!(runtime.scheduled_respawn.lock().unwrap().take().is_some());
        let (health_restarts, health_lifetime) = {
            let state = lock_snapshot(&health_snapshot).unwrap();
            (state.crash_restarts.len(), state.lifetime_restarts)
        };
        assert_eq!(health_restarts, 1);
        assert_eq!(health_lifetime, 1);

        let reload_snapshot = Arc::new(Mutex::new(SupervisorSnapshot::starting()));
        let mut reload_child = None;
        assert!(matches!(
            handle_reload_spawn_failure(
                &spec,
                &runtime,
                &supervisor.process_liveness,
                &reload_snapshot,
                &mut reload_child,
                "forced reload spawn failure".to_string(),
            )
            .await,
            Err(SuperviseError::ReloadFailed { .. })
        ));
        let (reload_restarts, reload_lifetime) = {
            let state = lock_snapshot(&reload_snapshot).unwrap();
            (state.crash_restarts.len(), state.lifetime_restarts)
        };
        assert_eq!(reload_restarts, 1);
        assert_eq!(reload_lifetime, 1);
    }

    #[tokio::test]
    async fn deliberately_severed_live_child_records_lifetime_without_spending_restart_budget() {
        let supervisor = Supervisor::new_for_test(
            Arc::new(Registry::default()),
            RestartPolicy::new(3, Duration::ZERO),
        );
        let runtime = supervisor.runtime_config();
        let spec = ModuleSpec {
            module_id: "deliberately-severed".to_string(),
            program: PathBuf::from("/unused/deliberately-severed"),
            args: Vec::new(),
            env: Vec::new(),
            reserved: false,
            reserved_prefixes: Vec::new(),
            protocol: ModuleProtocol::Subc,
            overlap: Default::default(),
        };
        let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::starting()));
        let process = ProcessIdentity {
            pid: 41,
            start_time: 101,
        };
        record_deliberate_severance(&snapshot, process).unwrap();
        let exit_report = apply_deliberate_severance_marker(
            &snapshot,
            Some(process),
            ExitReport {
                kind: ExitKind::Crash,
                code: Some(1),
                signal: None,
                at_ms: 1,
            },
        );
        assert_eq!(exit_report.kind, ExitKind::DeliberateSeverance);

        assert!(matches!(
            on_child_exit(
                &spec,
                runtime.restart_policy,
                &supervisor.registry,
                &snapshot,
                &runtime.terminal_ring,
                &runtime.spawn_events,
                &runtime.child_roster,
                exit_report,
            )
            .await,
            NextAction::Restart { schedule: _ }
        ));
        let state = lock_snapshot(&snapshot).unwrap();
        assert_eq!(state.lifetime_restarts, 1);
        assert_eq!(state.crash_restarts.len(), 0);
    }

    #[tokio::test]
    async fn genuine_crash_spends_restart_budget_and_records_lifetime() {
        let supervisor = Supervisor::new_for_test(
            Arc::new(Registry::default()),
            RestartPolicy::new(3, Duration::ZERO),
        );
        let runtime = supervisor.runtime_config();
        let spec = ModuleSpec {
            module_id: "genuine-crash".to_string(),
            program: PathBuf::from("/unused/genuine-crash"),
            args: Vec::new(),
            env: Vec::new(),
            reserved: false,
            reserved_prefixes: Vec::new(),
            protocol: ModuleProtocol::Subc,
            overlap: Default::default(),
        };
        let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::starting()));

        assert!(matches!(
            on_child_exit(
                &spec,
                runtime.restart_policy,
                &supervisor.registry,
                &snapshot,
                &runtime.terminal_ring,
                &runtime.spawn_events,
                &runtime.child_roster,
                ExitReport {
                    kind: ExitKind::Crash,
                    code: Some(1),
                    signal: None,
                    at_ms: 1,
                },
            )
            .await,
            NextAction::Restart { schedule: _ }
        ));
        let state = lock_snapshot(&snapshot).unwrap();
        assert_eq!(state.lifetime_restarts, 1);
        assert_eq!(state.crash_restarts.len(), 1);
    }

    fn crash_exit_report(at_ms: u64) -> ExitReport {
        ExitReport {
            kind: ExitKind::Crash,
            code: Some(1),
            signal: None,
            at_ms,
        }
    }

    fn windowed_crash_spec(module_id: &str) -> ModuleSpec {
        ModuleSpec {
            module_id: module_id.to_string(),
            program: PathBuf::from("/unused").join(module_id),
            args: Vec::new(),
            env: Vec::new(),
            reserved: false,
            reserved_prefixes: Vec::new(),
            protocol: ModuleProtocol::Subc,
            overlap: Default::default(),
        }
    }

    /// A real crash loop still stops. Three crashes with nothing aging out spend
    /// a budget of two and the third respawn is refused, and both surfaces an
    /// operator has -- the log line and the retained terminal record -- name the
    /// window rather than only the cap, because `max_restarts=2` alone is what
    /// this budget used to mean.
    #[tokio::test]
    async fn three_crashes_inside_the_window_stop_the_module_and_name_the_window() {
        let (logs, _guard) = crate::router::test_log::log_capture(tracing::Level::ERROR);
        let supervisor = Supervisor::new_for_test(
            Arc::new(Registry::default()),
            RestartPolicy::new(2, Duration::ZERO),
        );
        let runtime = supervisor.runtime_config();
        let spec = windowed_crash_spec("crash-loop-in-window");
        let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::starting()));

        for attempt in 1..=2 {
            assert!(
                matches!(
                    on_child_exit(
                        &spec,
                        runtime.restart_policy,
                        &supervisor.registry,
                        &snapshot,
                        &runtime.terminal_ring,
                        &runtime.spawn_events,
                        &runtime.child_roster,
                        crash_exit_report(attempt),
                    )
                    .await,
                    NextAction::Restart { schedule: _ }
                ),
                "crash {attempt} is inside the budget and must respawn"
            );
        }

        assert!(matches!(
            on_child_exit(
                &spec,
                runtime.restart_policy,
                &supervisor.registry,
                &snapshot,
                &runtime.terminal_ring,
                &runtime.spawn_events,
                &runtime.child_roster,
                crash_exit_report(3),
            )
            .await,
            NextAction::Stop { .. }
        ));

        {
            let state = lock_snapshot(&snapshot).unwrap();
            assert_eq!(state.state, ModuleState::Failed);
            assert_eq!(state.crash_restarts.len(), 2);
            assert_eq!(state.lifetime_restarts, 2);
        }

        let history = runtime
            .terminal_ring
            .lock()
            .expect("terminal ring is not poisoned")
            .snapshot();
        let last = history
            .entries
            .last()
            .expect("the refused crash is retained");
        assert_eq!(last.disposition, TerminalDisposition::Failed);
        assert_eq!(
            last.disposition_detail.as_deref(),
            Some("crash budget exhausted: max_restarts=2 within window_secs=600")
        );

        let captured = crate::router::test_log::captured_logs(&logs);
        assert!(
            captured.contains("crash budget exhausted: max_restarts=2 within window_secs=600"),
            "the stop must be logged with its window: {captured}"
        );
    }

    /// The rate, stated as a test: three crashes where the first has aged past
    /// the window are two crashes as far as the budget is concerned, so the
    /// third respawn is allowed and the ring holds only the two recent ones.
    ///
    /// This is the case a lifetime counter got wrong -- and the case the daemon
    /// now hits routinely, since a module exits non-zero every time its
    /// connection to the daemon drops.
    #[tokio::test]
    async fn a_crash_older_than_the_window_frees_its_slot_for_a_later_crash() {
        let supervisor = Supervisor::new_for_test(
            Arc::new(Registry::default()),
            RestartPolicy::new(2, Duration::ZERO),
        );
        let runtime = supervisor.runtime_config();
        let spec = windowed_crash_spec("crash-across-windows");
        let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::starting()));

        for attempt in 1..=2 {
            assert!(matches!(
                on_child_exit(
                    &spec,
                    runtime.restart_policy,
                    &supervisor.registry,
                    &snapshot,
                    &runtime.terminal_ring,
                    &runtime.spawn_events,
                    &runtime.child_roster,
                    crash_exit_report(attempt),
                )
                .await,
                NextAction::Restart { schedule: _ }
            ));
        }

        // The oldest crash moves out of the window; nothing else about the
        // module changes.
        update_snapshot(&snapshot, Some(&spec.module_id), |state| {
            age_oldest_crash_restart_out_of_window(state, runtime.restart_policy.window);
        })
        .unwrap();

        assert!(
            matches!(
                on_child_exit(
                    &spec,
                    runtime.restart_policy,
                    &supervisor.registry,
                    &snapshot,
                    &runtime.terminal_ring,
                    &runtime.spawn_events,
                    &runtime.child_roster,
                    crash_exit_report(3),
                )
                .await,
                NextAction::Restart { schedule: _ }
            ),
            "a crash older than the window must not hold a budget slot"
        );

        let state = lock_snapshot(&snapshot).unwrap();
        assert_eq!(state.state, ModuleState::Restarting);
        assert_eq!(
            state.crash_restarts.len(),
            2,
            "the aged instant is dropped and the new one takes its place"
        );
        assert_eq!(
            state.lifetime_restarts, 3,
            "the ledger counts every restart, including the ones the window forgot"
        );
    }

    /// An operator restart hands the budget back whole, and the ledger keeps
    /// counting. Those are different questions -- "how close is this module to
    /// being stopped" and "how many times has it been replaced" -- and the
    /// operator action answers only the first.
    #[tokio::test]
    async fn an_operator_restart_clears_the_ring_and_leaves_the_ledger_alone() {
        let supervisor = Supervisor::new_for_test(
            Arc::new(Registry::default()),
            RestartPolicy::new(2, Duration::ZERO),
        );
        let runtime = supervisor.runtime_config();
        let spec = windowed_crash_spec("operator-cleared-budget");
        let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::starting()));

        for attempt in 1..=2 {
            assert!(matches!(
                on_child_exit(
                    &spec,
                    runtime.restart_policy,
                    &supervisor.registry,
                    &snapshot,
                    &runtime.terminal_ring,
                    &runtime.spawn_events,
                    &runtime.child_roster,
                    crash_exit_report(attempt),
                )
                .await,
                NextAction::Restart { schedule: _ }
            ));
        }

        reset_restart_count(&snapshot, &spec.module_id).unwrap();
        {
            let state = lock_snapshot(&snapshot).unwrap();
            assert!(
                state.crash_restarts.is_empty(),
                "an operator restart returns the full budget"
            );
            assert_eq!(
                state.lifetime_restarts, 2,
                "clearing the budget must not unmake the crashes"
            );
        }

        assert!(
            matches!(
                on_child_exit(
                    &spec,
                    runtime.restart_policy,
                    &supervisor.registry,
                    &snapshot,
                    &runtime.terminal_ring,
                    &runtime.spawn_events,
                    &runtime.child_roster,
                    crash_exit_report(3),
                )
                .await,
                NextAction::Restart { schedule: _ }
            ),
            "the cleared budget must be spendable again"
        );
        let state = lock_snapshot(&snapshot).unwrap();
        assert_eq!(state.crash_restarts.len(), 1);
        assert_eq!(state.lifetime_restarts, 3);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn severance_marker_for_a_dead_child_does_not_label_its_successor() {
        let severed = ProcessIdentity {
            pid: 41,
            start_time: 101,
        };
        let successor = ProcessIdentity {
            pid: 41,
            start_time: 202,
        };
        let module = module_with_recovery_snapshot(ModuleState::Running, true, 0);
        update_snapshot(&module.inner.snapshot, Some("recovery-snapshot"), |state| {
            state.pid = Some(successor.pid);
            state.process_start_time = Some(successor.start_time);
        })
        .unwrap();
        assert!(!module.record_deliberate_severance(severed).unwrap());

        let exit_report = apply_deliberate_severance_marker(
            &module.inner.snapshot,
            Some(successor),
            ExitReport {
                kind: ExitKind::Crash,
                code: Some(1),
                signal: None,
                at_ms: 1,
            },
        );

        assert_eq!(exit_report.kind, ExitKind::Crash);
    }

    #[tokio::test]
    async fn drain_reap_marks_deliberate_severance_and_records_lifetime_without_budget() {
        let registry = Registry::default();
        let supervisor = Supervisor::new_for_test(
            Arc::new(Registry::default()),
            RestartPolicy::new(3, Duration::ZERO),
        );
        let runtime = supervisor.runtime_config();
        let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::starting()));
        let spec = ModuleSpec {
            module_id: "drain-deliberate-severance".to_string(),
            program: fake_aft_stub_path(),
            args: Vec::new(),
            env: vec![("FAKE_AFT_EXIT_CODE".to_string(), "23".to_string())],
            reserved: false,
            reserved_prefixes: Vec::new(),
            protocol: ModuleProtocol::Subc,
            overlap: Default::default(),
        };
        let mut child = spawn_and_mark_running(&spec, &runtime, &snapshot).unwrap();
        let process = ProcessIdentity {
            pid: 41,
            start_time: 101,
        };
        child.process_identity = Some(process);
        update_snapshot(&snapshot, Some(&spec.module_id), |state| {
            state.pid = Some(process.pid);
            state.process_start_time = Some(process.start_time);
        })
        .unwrap();
        record_deliberate_severance(&snapshot, process).unwrap();

        drain_child_to_state(
            &spec.module_id,
            spec.protocol,
            // The child exits on its own; no signal may change the exit this
            // test classifies.
            StopNotice::SentOverConnection,
            &registry,
            None,
            &snapshot,
            &runtime.terminal_ring,
            &runtime.spawn_events,
            child,
            Duration::from_secs(1),
            ModuleState::Stopped,
            Some(false),
        )
        .await
        .unwrap();

        let state = lock_snapshot(&snapshot).unwrap();
        assert_eq!(
            state.last_exit.as_ref().map(|exit| exit.kind),
            Some(ExitKind::DeliberateSeverance)
        );
        assert_eq!(state.lifetime_restarts, 1);
        assert_eq!(state.crash_restarts.len(), 0);
        drop(state);
        let history = runtime.terminal_ring.lock().unwrap().snapshot();
        assert_eq!(
            history.entries[0].exit_kind,
            subc_control::TerminalExitKind::DeliberateSeverance
        );
    }

    #[tokio::test]
    async fn ordinary_drain_reap_does_not_record_a_lifetime_restart() {
        let registry = Registry::default();
        let supervisor = Supervisor::new_for_test(
            Arc::new(Registry::default()),
            RestartPolicy::new(3, Duration::ZERO),
        );
        let runtime = supervisor.runtime_config();
        let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::starting()));
        let spec = ModuleSpec {
            module_id: "ordinary-drain".to_string(),
            program: fake_aft_stub_path(),
            args: Vec::new(),
            env: vec![("FAKE_AFT_EXIT_CODE".to_string(), "23".to_string())],
            reserved: false,
            reserved_prefixes: Vec::new(),
            protocol: ModuleProtocol::Subc,
            overlap: Default::default(),
        };
        let child = spawn_and_mark_running(&spec, &runtime, &snapshot).unwrap();

        drain_child_to_state(
            &spec.module_id,
            spec.protocol,
            // The child exits on its own; no signal may change the exit this
            // test classifies.
            StopNotice::SentOverConnection,
            &registry,
            None,
            &snapshot,
            &runtime.terminal_ring,
            &runtime.spawn_events,
            child,
            Duration::from_secs(1),
            ModuleState::Stopped,
            Some(false),
        )
        .await
        .unwrap();

        let state = lock_snapshot(&snapshot).unwrap();
        assert_eq!(
            state.last_exit.as_ref().map(|exit| exit.kind),
            Some(ExitKind::Crash)
        );
        assert_eq!(state.lifetime_restarts, 0);
        assert_eq!(state.crash_restarts.len(), 0);
    }

    #[test]
    fn fatal_connection_teardown_cannot_arm_a_marker_for_a_surviving_process() {
        // The server's generic fatal-routing branch only knows that the
        // connection failed; it does not know that the daemon deliberately
        // initiated a process-killing severance. Keep this seam explicit so a
        // future connection error path cannot silently reintroduce the stale
        // exemption that mislabels a later genuine crash.
        assert!(!include_str!("server.rs")
            .contains("router.record_deliberate_connection_severance(ctx.connection_id)"));
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
        record_wait_error_terminal("wait-error", &ring, &super::SpawnEventFeed::default());

        let snapshot = ring.lock().unwrap().snapshot();
        assert_eq!(snapshot.entries.len(), 1);
        let entry = &snapshot.entries[0];
        assert_eq!(entry.exit_code, None);
        assert_eq!(entry.exit_signal, None);
        assert_eq!(entry.disposition, TerminalDisposition::Failed);
    }

    #[test]
    fn wait_error_exit_path_preserves_spawn_event_density() {
        let feed = super::SpawnEventFeed::default();
        feed.configure_incarnation("wait-error-density".to_string());
        feed.emit_spawned("wait-error", 41, 1);
        let ring = Arc::new(Mutex::new(TerminalRing::new(
            TerminalRingConfig::default(),
            0,
        )));

        record_wait_error_terminal("wait-error", &ring, &feed);
        feed.emit_spawned("after-wait-error", 42, 2);

        let state = feed.0.lock().unwrap();
        let sequences = state
            .events
            .iter()
            .map(|event| event.cursor.seq)
            .collect::<Vec<_>>();
        assert_eq!(sequences, vec![1, 2, 3]);
        assert_eq!(state.events[1].kind, SpawnEventKind::Exited);
        assert_eq!(state.events[1].exit_code, None);
        assert_eq!(state.events[1].exit_signal, None);
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
        probe_module_health, HealthAction, HealthConfig, HealthProbeEvidence, ModuleProtocol,
        ModuleSpec, RestartPolicy, Supervisor, SupervisorRuntimeConfig,
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
            http: None,
            cadence: Duration::from_secs(30),
            deadline: Duration::from_secs(5),
            failure_threshold: 3,
            on_degraded: HealthAction::Report,
            on_failing: HealthAction::Report,
            critical: false,
        };
        let supervisor = Supervisor::new_for_test(Arc::clone(&registry), RestartPolicy::default())
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
            protocol: ModuleProtocol::Subc,
            overlap: Default::default(),
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
        let probe = probe_module_health(&harness.spec.module_id, &harness.runtime, None);
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
        let probe = probe_module_health(&harness.spec.module_id, &harness.runtime, None);
        let exhaust_deadline = async {
            let _frame = harness.module_rx.recv().await.expect("health.check frame");
            tokio::time::advance(deadline).await;
            tokio::task::yield_now().await;
        };
        let (probe_result, ()) = tokio::join!(probe, exhaust_deadline);
        let err = probe_result.expect_err("probe must miss its deadline");
        assert!(matches!(err.evidence, HealthProbeEvidence::NoAnswer));
    }

    async fn run_probe_cycle(harness: &mut ProbeHarness, answer: bool) {
        let registry = Arc::clone(&harness.module.inner.registry);
        let snapshot = Arc::clone(&harness.module.inner.snapshot);
        let process_liveness = super::SupervisorProcessLiveness::default();
        let mut child = None;
        let cycle = super::run_health_probe_cycle(
            &harness.spec,
            &harness.runtime,
            &registry,
            &process_liveness,
            &snapshot,
            &mut child,
        );
        let peer = async {
            let frame = harness.module_rx.recv().await.expect("health.check frame");
            if answer {
                harness
                    .forwarding
                    .complete_module_control_rpc(
                        harness.module_connection,
                        frame.header.corr,
                        Some("health.check"),
                        ModuleControlRpcOutcome::Response(ModuleControlResponse::HealthCheck {
                            status: HealthStatus::Ok,
                            detail: None,
                            metrics: Some(serde_json::json!({"ready": true})),
                        }),
                    )
                    .unwrap();
            } else {
                tokio::time::advance(harness.runtime.health.deadline).await;
                tokio::task::yield_now().await;
            }
        };
        tokio::join!(cycle, peer);
    }

    #[tokio::test(start_paused = true)]
    async fn unanswered_probe_is_unknown_until_threshold_and_ok_report_recovers() {
        let mut harness = probe_harness();
        // Drive the probe cycle directly with an in-memory wire peer. Stop the
        // disabled module's monitor so only this test owns lifecycle transitions;
        // no OS process is launched, and a restart is observed at scheduling.
        let monitor = harness.module.inner.monitor.lock().unwrap().take().unwrap();
        monitor.abort();
        let _ = monitor.await;
        super::update_snapshot(&harness.module.inner.snapshot, None, |state| {
            state.enabled = true;
            state.state = super::ModuleState::Running;
            state.process_alive = true;
        })
        .unwrap();

        run_probe_cycle(&mut harness, true).await;
        assert_eq!(
            harness.module.status().unwrap().health.status,
            super::SupervisorHealthStatus::Ok
        );

        for failures in 1..harness.runtime.health.failure_threshold {
            run_probe_cycle(&mut harness, false).await;
            let status = harness.module.status().unwrap();
            assert_eq!(status.health.status, super::SupervisorHealthStatus::Unknown);
            assert_eq!(status.health.consecutive_failures, failures);
            assert!(status.health.last_probe_ms.is_some());
            assert!(status.health.detail.unwrap().starts_with("[no-answer]"));
            assert!(status.health.metrics.is_none());
            assert_eq!(status.state, super::ModuleState::Running);
            assert!(status.process_alive);
            assert_eq!(status.restart_count, 0);
            assert_eq!(status.lifetime_restarts, 0);
            assert!(status.health.last_action.is_none());
            assert!(harness.runtime.scheduled_respawn.lock().unwrap().is_none());
        }

        run_probe_cycle(&mut harness, true).await;
        let recovered = harness.module.status().unwrap();
        assert_eq!(recovered.health.status, super::SupervisorHealthStatus::Ok);
        assert_eq!(recovered.health.consecutive_failures, 0);
        assert!(recovered.health.detail.is_none());
        assert_eq!(
            recovered.health.metrics,
            Some(serde_json::json!({"ready": true}))
        );
        assert_eq!(recovered.lifetime_restarts, 0);

        for failures in 1..=harness.runtime.health.failure_threshold {
            run_probe_cycle(&mut harness, false).await;
            let status = harness.module.status().unwrap();
            assert_eq!(status.health.consecutive_failures, failures);
            if failures < harness.runtime.health.failure_threshold {
                assert_eq!(status.health.status, super::SupervisorHealthStatus::Unknown);
                assert_eq!(status.state, super::ModuleState::Running);
                assert_eq!(status.lifetime_restarts, 0);
                assert!(harness.runtime.scheduled_respawn.lock().unwrap().is_none());
            } else {
                assert_eq!(
                    status.health.status,
                    super::SupervisorHealthStatus::Unresponsive
                );
                assert_eq!(status.state, super::ModuleState::Restarting);
                assert_eq!(status.restart_count, 1);
                assert_eq!(status.lifetime_restarts, 1);
                assert_eq!(status.health.last_action.as_deref(), Some("restart"));
                assert!(harness.runtime.scheduled_respawn.lock().unwrap().is_some());
            }
        }
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
mod child_env_tests {
    use super::{
        apply_child_env, apply_spawn_role, apply_wire_spawn_args, ModuleProtocol, ModuleSpec,
        SpawnRole, SupervisorHandle, SPAWN_ROLE_SWAP_CANDIDATE, SUBC_ARG, SUBC_LAUNCH_NONCE_ENV,
        SUBC_MODULE_ID_ENV, SUBC_SPAWN_ROLE_ENV,
    };
    use std::{ffi::OsStr, path::PathBuf};
    use tokio::process::Command;

    fn spec(env: Vec<(String, String)>) -> ModuleSpec {
        ModuleSpec {
            module_id: "env-plan".to_string(),
            program: PathBuf::from("/nonexistent"),
            args: Vec::new(),
            env,
            reserved: false,
            reserved_prefixes: Vec::new(),
            protocol: ModuleProtocol::Subc,
            overlap: Default::default(),
        }
    }

    /// Ambient `CK_LOG` is REMOVED for an unconfigured module, and a configured
    /// one still gets its own.
    ///
    /// This is the narrow goal `env_clear()` was reached for, and the reason the
    /// fix is `env_remove` rather than deleting the line: an operator's ambient
    /// filter silently becoming an unconfigured module's log level is a real
    /// defect, just a much smaller one than clearing the environment.
    ///
    /// Asserted on the command plan rather than a spawned child because proving
    /// the ABSENCE of an inherited variable needs the parent's environment
    /// mutated, and `forbid(unsafe_code)` refuses that. `get_envs()` reports a
    /// removal as `(key, None)`, which is exactly the distinction wanted: not
    /// "absent because nobody set it" but "explicitly unset for the child".
    #[test]
    fn ambient_ck_log_is_removed_and_a_configured_one_survives() {
        let mut command = Command::new("/nonexistent");
        apply_child_env(&mut command, &spec(Vec::new()));
        let removed = command
            .as_std()
            .get_envs()
            .any(|(key, value)| key == OsStr::new("CK_LOG") && value.is_none());
        assert!(
            removed,
            "ambient CK_LOG must be explicitly removed for an unconfigured module"
        );

        let mut configured = Command::new("/nonexistent");
        apply_child_env(
            &mut configured,
            &spec(vec![("CK_LOG".to_string(), "debug".to_string())]),
        );
        let effective = configured
            .as_std()
            .get_envs()
            .filter(|(key, _)| *key == OsStr::new("CK_LOG"))
            .last()
            .map(|(_, value)| value.map(|v| v.to_string_lossy().into_owned()));
        assert_eq!(
            effective,
            Some(Some("debug".to_string())),
            "a module's configured CK_LOG must survive the ambient removal"
        );
    }

    /// A `protocol: "none"` spawn carries NO `--subc` argument and NO launch
    /// nonce; a subc-wire spawn carries both. Asserted on the command plan for
    /// the same reason as the CK_LOG test above.
    ///
    /// The argument is the load-bearing half: a stock binary exits on an
    /// unknown flag before it listens, so with `--subc` appended the mode
    /// could not supervise the one process it exists for. Found by the first
    /// conformance run (nats-server: `flag provided but not defined: -subc`).
    #[test]
    fn protocol_none_spawn_carries_no_subc_argument_and_no_nonce() {
        let connection_file = std::path::Path::new("/run/subc-connection.json");
        let handle = SupervisorHandle::new();

        let mut none_spec = spec(Vec::new());
        none_spec.protocol = ModuleProtocol::None;
        let mut none = Command::new("/nonexistent");
        let none_handoff =
            apply_wire_spawn_args(&mut none, &none_spec, Some(connection_file), Some(&handle))
                .expect("protocol-none spawn args apply");
        assert!(
            none_handoff.is_none(),
            "protocol:none spawn must not receive a nonce descriptor"
        );
        assert!(
            !none.as_std().get_envs().any(|(key, value)| key
                == OsStr::new(subc_os::LAUNCH_NONCE_FD_ENV)
                && value.is_some()),
            "protocol:none spawn must not name a nonce descriptor"
        );
        let none_args: Vec<String> = none
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(
            !none_args.iter().any(|a| a == SUBC_ARG),
            "protocol:none argv must not carry --subc; got {none_args:?}"
        );
        let none_has_nonce = none
            .as_std()
            .get_envs()
            .any(|(key, value)| key == OsStr::new(SUBC_LAUNCH_NONCE_ENV) && value.is_some());
        assert!(
            !none_has_nonce,
            "protocol:none spawn must not receive a launch nonce"
        );
        let none_has_module_id = none
            .as_std()
            .get_envs()
            .any(|(key, value)| key == OsStr::new(SUBC_MODULE_ID_ENV) && value.is_some());
        assert!(
            none_has_module_id,
            "SUBC_MODULE_ID is inert and stays on every path"
        );
        assert!(
            handle.spawn_nonce(&none_spec.module_id).is_none(),
            "no nonce record for a process that will never present one"
        );

        // Control: the subc-wire path is unchanged by the branch above.
        let wire_spec = spec(Vec::new());
        let mut wire = Command::new("/nonexistent");
        let wire_handoff =
            apply_wire_spawn_args(&mut wire, &wire_spec, Some(connection_file), Some(&handle))
                .expect("subc-wire spawn args apply");
        let wire_fd_env = wire
            .as_std()
            .get_envs()
            .find(|(key, _)| *key == OsStr::new(subc_os::LAUNCH_NONCE_FD_ENV))
            .and_then(|(_, value)| value.map(|v| v.to_string_lossy().into_owned()));
        #[cfg(unix)]
        assert_eq!(
            wire_fd_env,
            Some(wire_handoff.expect("a descriptor handoff").fd_env_value()),
            "a subc-wire spawn names the pipe it will receive at descriptor 3"
        );
        #[cfg(not(unix))]
        assert!(wire_handoff.is_none() && wire_fd_env.is_none());
        let wire_args: Vec<String> = wire
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            wire_args,
            vec![
                SUBC_ARG.to_string(),
                connection_file.to_string_lossy().into_owned()
            ],
            "a subc-wire spawn still carries --subc <path>"
        );
        assert_eq!(
            wire.as_std()
                .get_envs()
                .any(|(key, value)| key == OsStr::new(SUBC_LAUNCH_NONCE_ENV) && value.is_some()),
            !cfg!(unix),
            "only Windows supplies the environment nonce"
        );
        assert!(handle.spawn_nonce(&wire_spec.module_id).is_some());
    }

    /// A plain spawn EXPLICITLY REMOVES the spawn role, even when the module's
    /// spec tries to set it; only a swap candidate carries it.
    ///
    /// "Set it only on candidates" is not enough, because spawn applies the
    /// spec's env verbatim and the daemon's own environment is inherited: either
    /// could hand a plain restart the swap role, and a module reading it would
    /// warm on its long swap budget while callers wait. Asserted as an explicit
    /// removal (`(key, None)`), not mere absence, for the reason the `CK_LOG`
    /// test above gives.
    #[test]
    fn plain_spawn_removes_the_spawn_role_even_when_the_spec_sets_it() {
        let role = |command: &Command| {
            command
                .as_std()
                .get_envs()
                .filter(|(key, _)| *key == OsStr::new(SUBC_SPAWN_ROLE_ENV))
                .last()
                .map(|(_, value)| value.map(|v| v.to_string_lossy().into_owned()))
        };
        let forged = spec(vec![(
            SUBC_SPAWN_ROLE_ENV.to_string(),
            SPAWN_ROLE_SWAP_CANDIDATE.to_string(),
        )]);

        let mut plain = Command::new("/nonexistent");
        apply_child_env(&mut plain, &forged);
        apply_spawn_role(&mut plain, SpawnRole::Plain);
        assert_eq!(
            role(&plain),
            Some(None),
            "a plain spawn must remove SUBC_SPAWN_ROLE, whatever the spec says"
        );

        let mut candidate = Command::new("/nonexistent");
        apply_child_env(&mut candidate, &spec(Vec::new()));
        apply_spawn_role(&mut candidate, SpawnRole::SwapCandidate);
        assert_eq!(
            role(&candidate),
            Some(Some(SPAWN_ROLE_SWAP_CANDIDATE.to_string()))
        );
    }

    /// Daemon-private capture retention keys never reach the child.
    ///
    /// cortexkit-log exposes retention as a Rust struct with no environment
    /// names, so these entries are supervisor metadata. Passing them through
    /// would invent a public child-process contract by accident.
    #[test]
    fn daemon_private_capture_keys_are_not_passed_to_the_child() {
        let mut command = Command::new("/nonexistent");
        apply_child_env(
            &mut command,
            &spec(vec![
                (super::CAPTURE_KEEP_ENV.to_string(), "5".to_string()),
                ("KEPT".to_string(), "yes".to_string()),
            ]),
        );
        let keys: Vec<String> = command
            .as_std()
            .get_envs()
            .filter(|(_, value)| value.is_some())
            .map(|(key, _)| key.to_string_lossy().into_owned())
            .collect();
        assert!(keys.contains(&"KEPT".to_string()), "got {keys:?}");
        assert!(
            !keys.contains(&super::CAPTURE_KEEP_ENV.to_string()),
            "daemon-private capture key leaked to the child: {keys:?}"
        );
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

#[cfg(all(test, target_os = "linux"))]
mod cgroup_placement_tests {
    use super::{
        apply_cgroup_placement, remove_module_cgroup, ModuleProtocol, ModuleSpec, SuperviseError,
        SupervisedChild,
    };
    use crate::stderr_tail::{StderrRing, StderrTailConfig};
    use std::{
        fs, io,
        path::{Path, PathBuf},
        sync::{Arc, Mutex},
    };
    use subc_test_support::TestTempDir;
    use tokio::process::Command;

    #[tokio::test]
    async fn unique_spawn_cgroups_clean_up_across_restarts_and_shutdown() {
        use super::*;
        let dir = TestTempDir::new("unique-spawn-cgroups");
        let root = PathBuf::from(format!(
            "/sys/fs/cgroup/subc-unique-test-{}-{}",
            std::process::id(),
            unix_ms_now()
        ));
        if let Err(error) = fs::create_dir(&root) {
            assert!(
                std::env::var_os("SUBC_REQUIRE_CGROUP_TEST").is_none(),
                "required cgroup test cannot execute: {error}"
            );
            eprintln!("SKIP unique_spawn_cgroups_clean_up_across_restarts_and_shutdown: {error}");
            return;
        }
        let placement = subc_cgroup::prepare_at(&root).unwrap().unwrap();
        let group_count = || {
            fs::read_dir(root.join("subc-modules"))
                .unwrap()
                .map(|entry| entry.unwrap().file_type().unwrap())
                .filter(|kind| kind.is_dir())
                .count()
        };
        let supervisor =
            Supervisor::new_for_test(Arc::new(Registry::default()), RestartPolicy::default())
                .with_cgroup_placement(Some(placement.clone()));
        let runtime = supervisor.runtime_config();
        let mut spec = ModuleSpec {
            module_id: "unique-spawn".into(),
            program: PathBuf::from("/bin/sleep"),
            args: vec!["60".into()],
            env: ["XDG_DATA_HOME", "XDG_RUNTIME_DIR", "XDG_CONFIG_HOME"]
                .into_iter()
                .map(|key| (key.into(), dir.to_string_lossy().into_owned()))
                .collect(),
            reserved: false,
            reserved_prefixes: vec![],
            protocol: ModuleProtocol::None,
            overlap: Default::default(),
        };
        let spawn = |spec: &ModuleSpec| {
            spawn_child(
                spec,
                None,
                None,
                &runtime.stderr_ring,
                None,
                &runtime.child_roster,
                Some(&placement),
            )
            .unwrap()
        };
        let mut live = spawn(&spec);
        for _ in 0..3 {
            // A new process can enter the old slot while retirement is pending.
            let next = spawn(&spec);
            assert_ne!(live.module_id, next.module_id);
            live.start_kill().unwrap();
            live.wait().await.unwrap();
            live = next;
            assert!(
                live.child.try_wait().unwrap().is_none(),
                "retiring the old slot must not kill the replacement"
            );
            assert_eq!(
                group_count(),
                1,
                "only the live spawn's cgroup should remain"
            );
        }
        supervisor.begin_daemon_shutdown();
        let reap = tokio::spawn(async move {
            live.wait().await.unwrap();
        });
        supervisor
            .end_children_for_daemon_shutdown(false, std::future::pending())
            .await;
        reap.await.unwrap();
        assert_eq!(group_count(), 0);
        // A normal exit uses the same tree-cleanup path as a killed spawn.
        spec.program = PathBuf::from("/bin/true");
        spec.args.clear();
        let fresh_roster = ChildRoster::default();
        let mut short = spawn_child(
            &spec,
            None,
            None,
            &runtime.stderr_ring,
            None,
            &fresh_roster,
            Some(&placement),
        )
        .unwrap();
        short.wait().await.unwrap();
        assert_eq!(group_count(), 0);
        spec.module_id = "_".repeat(255);
        let mut long_id = spawn_child(
            &spec,
            None,
            None,
            &runtime.stderr_ring,
            None,
            &fresh_roster,
            Some(&placement),
        )
        .unwrap();
        long_id.wait().await.unwrap();
        assert_eq!(
            group_count(),
            0,
            "valid long module IDs must not exceed cgroup NAME_MAX"
        );
        fs::remove_dir(root.join("subc-modules")).unwrap();
        fs::remove_dir(root).unwrap();
        eprintln!("EXECUTED unique_spawn_cgroups_clean_up_across_restarts_and_shutdown");
    }

    #[test]
    fn failed_parent_cgroup_open_is_a_cgroup_supervision_error() {
        let path = Path::new("/definitely-missing-subc-cgroup");
        let mut command = Command::new("true");
        let error = apply_cgroup_placement(
            &mut command,
            &ModuleSpec {
                module_id: "broken-cgroup".to_string(),
                program: PathBuf::from("true"),
                args: Vec::new(),
                env: Vec::new(),
                reserved: false,
                reserved_prefixes: Vec::new(),
                protocol: ModuleProtocol::Subc,
                overlap: Default::default(),
            },
            path,
        )
        .expect_err("a parent cgroup open failure must reject the supervised spawn");
        let reason = error.to_string();

        assert!(
            matches!(error, SuperviseError::Cgroup { .. }),
            "parent cgroup open must be reported as a cgroup supervision error: {reason}"
        );
        assert!(
            reason.contains("/definitely-missing-subc-cgroup/cgroup.procs"),
            "parent cgroup open failure must name cgroup.procs: {reason}"
        );
    }

    #[tokio::test]
    async fn reaping_a_child_removes_its_empty_module_cgroup() {
        let root = TestTempDir::new("supervisor-reap-cgroup");
        fs::write(root.join("cgroup.procs"), b"").expect("write scratch cgroup marker");
        let placement = subc_cgroup::prepare_at(&root)
            .expect("prepare scratch cgroup root")
            .expect("scratch root has a cgroup.procs marker");
        let module_id = "reaped-module";
        let module = placement
            .module_path(module_id)
            .expect("create scratch module cgroup");
        let child = Command::new("true")
            .env("XDG_DATA_HOME", root.path())
            .env("XDG_RUNTIME_DIR", root.path())
            .env("XDG_CONFIG_HOME", root.path())
            .spawn()
            .expect("spawn short-lived child");
        let pid = child.id().expect("spawned child has pid");
        let mut child = SupervisedChild {
            child,
            protocol: ModuleProtocol::Subc,
            module_id: module_id.to_string(),
            cgroup_placement: Some(placement),
            stdout_pump: None,
            stderr_pump: None,
            stderr_ring: Arc::new(Mutex::new(StderrRing::new(StderrTailConfig::default()))),
            spawned_at_ms: 0,
            spawned_from: PathBuf::from("true"),
            spawned_file_identity: None,
            process_start_time: None,
            process_identity: None,
            pid,
            roster_guard: None,
            #[cfg(target_os = "macos")]
            privacy_exec: None,
            spawn_failure: None,
        };

        child.wait().await.expect("reap short-lived child");

        assert!(
            !module.exists(),
            "reaping the supervised child must remove its empty cgroup"
        );
    }

    #[test]
    fn non_empty_cgroup_removal_is_reported_without_blocking_teardown() {
        let root = TestTempDir::new("supervisor-non-empty-cgroup");
        fs::write(root.join("cgroup.procs"), b"").expect("write scratch cgroup marker");
        let placement = subc_cgroup::prepare_at(&root)
            .expect("prepare scratch cgroup root")
            .expect("scratch root has a cgroup.procs marker");
        let module = placement
            .module_path("surviving-module")
            .expect("create scratch module cgroup");
        fs::write(module.join("surviving-process"), b"still present")
            .expect("make scratch cgroup non-empty");
        let (logs, _guard) = crate::router::test_log::log_capture(tracing::Level::WARN);

        remove_module_cgroup(&placement, "surviving-module");

        let logs = crate::router::test_log::captured_logs(&logs);
        assert!(
            module.exists(),
            "failed removal must leave the cgroup intact"
        );
        assert!(
            logs.contains("could not remove module cgroup after process exit; continuing teardown")
                && logs.contains("surviving-module"),
            "best-effort removal must report the failure without returning it: {logs}"
        );
    }

    #[test]
    fn cgroup_pre_exec_spawn_failure_names_the_cgroup_path() {
        let cgroup_path = PathBuf::from("/sys/fs/cgroup/subc-modules/broken-module");
        let reason = SuperviseError::Spawn {
            program: PathBuf::from("/bin/true"),
            source: io::Error::from_raw_os_error(13),
            cgroup_path: Some(cgroup_path.clone()),
        }
        .to_string();

        assert!(
            reason.contains(&cgroup_path.display().to_string()),
            "a pre_exec spawn failure must name the cgroup path: {reason}"
        );
    }
}

#[cfg(test)]
mod spawn_subscriber_lag_tests {
    use super::*;

    /// A subscriber whose connection stops draining is dropped once its frame
    /// channel fills. The client must learn that from a terminal Error frame
    /// after the frames already queued for it, not from a stream that simply
    /// goes quiet.
    #[tokio::test]
    async fn lagged_spawn_subscriber_receives_a_terminal_lagged_error_after_its_queued_frames() {
        let feed = SpawnEventFeed::default();
        feed.configure_incarnation("lag-incarnation".to_string());
        // A one-slot connection queue that nobody reads until the emits are
        // done: the forwarder parks on it and the subscriber channel fills.
        let (tx, mut rx) = mpsc::channel(1);
        feed.subscribe(ConnectionId::new(1), 7, 1, None, FrameSink::new(tx))
            .expect("subscribe");
        let emitted = SPAWN_SUBSCRIBER_BUFFER + 16;
        for index in 0..emitted {
            feed.emit_spawned(&format!("lag-module-{index}"), 1000, 0);
            // Let the forwarder take what it can so the fill point is the
            // subscriber channel, not a scheduling accident.
            tokio::task::yield_now().await;
        }
        assert_eq!(
            feed.subscriber_count(),
            0,
            "the lagged subscriber must be removed"
        );

        let mut data = Vec::new();
        let mut last = None;
        loop {
            let next = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("the forwarder must finish once the subscriber is dropped");
            let Some(outbound) = next else { break };
            let frame = outbound.frame;
            if frame.header.ty == FrameType::StreamData {
                assert!(last.is_none(), "no data may follow the terminal frame");
                let event: SpawnEvent = serde_json::from_slice(&frame.body).unwrap();
                data.push(event.cursor.seq);
            } else {
                assert!(last.is_none(), "exactly one terminal frame");
                last = Some(frame);
            }
        }
        assert!(!data.is_empty(), "queued frames drain before the terminal");
        for pair in data.windows(2) {
            assert_eq!(
                pair[1],
                pair[0] + 1,
                "queued frames arrive dense and in order"
            );
        }
        let terminal = last.expect("a lagged subscriber must receive a terminal frame");
        assert_eq!(terminal.header.ty, FrameType::Error);
        assert_eq!(terminal.header.corr, 7);
        let body: subc_protocol::ErrorBody = serde_json::from_slice(&terminal.body).unwrap();
        assert_eq!(body.code, SPAWN_SUBSCRIBER_LAGGED_CODE);
        let detail = body.detail.expect("lagged error carries detail");
        assert_eq!(
            detail["first_undelivered_cursor"]["seq"],
            data.last().unwrap() + 1,
            "the named cursor is the first event the subscriber did not receive"
        );
        assert_eq!(
            detail["first_undelivered_cursor"]["daemon_incarnation"],
            "lag-incarnation"
        );
    }
}

#[cfg(test)]
mod terminal_history_read_concurrency_tests {
    use super::*;
    use crate::terminal_journal::read_pause;
    use std::sync::mpsc as std_mpsc;
    use subc_test_support::TestTempDir;

    fn journaled_ring(
        journal: &Arc<crate::terminal_journal::TerminalJournal>,
    ) -> Arc<Mutex<TerminalRing>> {
        Arc::new(Mutex::new(
            TerminalRing::new(TerminalRingConfig::default(), 1)
                .with_journal(Some(Arc::clone(journal))),
        ))
    }

    fn crash(at_ms: u64) -> ExitReport {
        ExitReport {
            kind: ExitKind::Crash,
            code: Some(1),
            signal: None,
            at_ms,
        }
    }

    /// Record an exit on another thread and report whether it finished within
    /// `bound`. The recorder thread is left running if it did not.
    fn record_within(
        module_id: &'static str,
        ring: &Arc<Mutex<TerminalRing>>,
        at_ms: u64,
        bound: Duration,
    ) -> bool {
        let ring = Arc::clone(ring);
        let (done, done_rx) = std_mpsc::channel();
        std::thread::spawn(move || {
            record_terminal(
                module_id,
                &ring,
                &SpawnEventFeed::default(),
                &crash(at_ms),
                TerminalDisposition::Restarting,
            );
            let _ = done.send(());
        });
        done_rx.recv_timeout(bound).is_ok()
    }

    /// A history read in progress must not hold the journal writer (which every
    /// module's exit recording needs) or the module's own ring. Exits recorded
    /// while the read is paused complete promptly; the paused read answers as of
    /// the moment it started, and the next read has each exit exactly once.
    #[test]
    fn exits_recorded_during_a_paused_history_read_are_not_blocked_or_half_merged() {
        let dir = TestTempDir::new("terminal-history-concurrent-read");
        let path = dir.join("terminals.jsonl");
        let journal = Arc::new(crate::terminal_journal::TerminalJournal::open(
            path.clone(),
            "daemon".into(),
        ));
        let reader_ring = journaled_ring(&journal);
        let other_ring = journaled_ring(&journal);
        assert!(record_within(
            "reader-module",
            &reader_ring,
            10,
            Duration::from_secs(5)
        ));

        let (started, release) = read_pause::install(&path);
        let reading = {
            let ring = Arc::clone(&reader_ring);
            std::thread::spawn(move || durable_terminal_history_of(&ring, "reader-module"))
        };
        started
            .recv_timeout(Duration::from_secs(5))
            .expect("the history read reached its pause");

        let bound = Duration::from_secs(1);
        assert!(
            record_within("other-module", &other_ring, 20, bound),
            "another module's exit waited on a history read (journal writer held)"
        );
        assert!(
            record_within("reader-module", &reader_ring, 30, bound),
            "the read module's own exit waited on its history read (ring held)"
        );

        drop(release);
        let paused = reading.join().unwrap();
        assert_eq!(
            paused.entries.iter().map(|e| e.at_ms).collect::<Vec<_>>(),
            vec![10],
            "an exit recorded after the read began lands in neither half of it"
        );
        assert_eq!(paused.journal_skipped_lines, 0);
        assert_eq!(paused.journal_read_errors, 0);

        let after = durable_terminal_history_of(&reader_ring, "reader-module");
        assert_eq!(
            after.entries.iter().map(|e| e.at_ms).collect::<Vec<_>>(),
            vec![10, 30],
            "the next read merges ring and journal with no duplicate"
        );
        assert_eq!(after.journal_skipped_lines, 0);
    }
}

/// What a restart does with the exited process's stderr reader. These drive
/// the same `settle_stderr_pump` the supervisor calls, with a reader the test
/// holds, so a reader that has not been scheduled by the bound is a controlled
/// input rather than something only a loaded machine produces.
#[cfg(test)]
mod stderr_settle_tests {
    use std::{
        future::Future,
        io,
        pin::Pin,
        sync::{Arc, Mutex},
        task::{Context, Poll},
        time::Duration,
    };

    use tokio::{
        io::{AsyncRead, ReadBuf},
        sync::oneshot,
        time::Instant,
    };

    use super::{settle_stderr_pump, StderrPump};
    use crate::stderr_tail::{
        pump_stderr_to, untimed, CaptureState, OutputSink, StderrRing, StderrTailConfig, TailEntry,
    };

    const BOUND: Duration = Duration::from_millis(250);

    /// Yields `before`, then stays pending until the gate is released, then
    /// yields `after` and reaches EOF. The bytes after the gate were written
    /// by a process that has already exited; only the reader is behind.
    struct HeldReader {
        before: Option<Vec<u8>>,
        gate: Option<oneshot::Receiver<()>>,
        after: io::Cursor<Vec<u8>>,
    }

    impl AsyncRead for HeldReader {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            if let Some(bytes) = self.before.take() {
                buf.put_slice(&bytes);
                return Poll::Ready(Ok(()));
            }
            if let Some(gate) = self.gate.as_mut() {
                match Pin::new(gate).poll(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(_) => self.gate = None,
                }
            }
            Pin::new(&mut self.after).poll_read(cx, buf)
        }
    }

    struct DiscardSink;

    impl OutputSink for DiscardSink {
        fn write_line(&mut self, _line: &[u8]) {}
    }

    fn line(text: &str) -> TailEntry {
        TailEntry::Line {
            text: text.to_string(),
            truncated: false,
            at_ms: None,
        }
    }

    fn lock(ring: &Arc<Mutex<StderrRing>>) -> std::sync::MutexGuard<'_, StderrRing> {
        ring.lock().unwrap()
    }

    /// Start a reader for a new process generation that delivers `before`
    /// immediately and `after` only once the returned sender fires (or is
    /// dropped).
    fn held_pump(
        ring: &Arc<Mutex<StderrRing>>,
        before: &str,
        after: &str,
    ) -> (StderrPump, oneshot::Sender<()>) {
        let generation = lock(ring).begin_process();
        let (release, gate) = oneshot::channel();
        let reader = HeldReader {
            before: Some(before.as_bytes().to_vec()),
            gate: Some(gate),
            after: io::Cursor::new(after.as_bytes().to_vec()),
        };
        let task = tokio::spawn(pump_stderr_to(
            reader,
            Arc::clone(ring),
            generation,
            DiscardSink,
        ));
        (StderrPump { task, generation }, release)
    }

    async fn wait_until(ring: &Arc<Mutex<StderrRing>>, done: impl Fn(&StderrRing) -> bool) {
        for _ in 0..1000 {
            if done(&lock(ring)) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        panic!(
            "ring never reached the expected state: {:?}",
            lock(ring).snapshot(None, None)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_crash_line_the_reader_had_not_reached_by_the_bound_is_kept_before_the_restart() {
        let ring = Arc::new(Mutex::new(StderrRing::new(StderrTailConfig::default())));
        let (pump, release) = held_pump(&ring, "booting\n", "config error: missing storage\n");

        settle_stderr_pump("crasher", &ring, pump, BOUND).await;
        let before_release = lock(&ring).snapshot(None, None);
        assert!(
            matches!(before_release.capture, CaptureState::Incomplete { .. }),
            "a reader that has not reached EOF cannot claim a whole tail: {before_release:?}"
        );

        // The restart: the next process starts and writes before the old
        // reader catches up.
        let next = lock(&ring).begin_process();
        lock(&ring).push_line_from(next, "next process booting");
        release.send(()).unwrap();
        wait_until(&ring, |ring| {
            ring.snapshot(None, None).capture == CaptureState::Captured
        })
        .await;

        assert_eq!(
            untimed(lock(&ring).snapshot(None, None).entries),
            vec![
                line("booting"),
                line("config error: missing storage"),
                TailEntry::ProcessStart,
                line("next process booting"),
            ],
            "the crash's last line must survive a slow reader and stay in the crashed process's section"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_pipe_held_open_by_a_descendant_reads_incomplete_without_delaying_the_restart_past_the_bound(
    ) {
        let ring = Arc::new(Mutex::new(StderrRing::new(StderrTailConfig::default())));
        // `_held` is never fired: a descendant keeps the pipe open for the
        // whole test.
        let (pump, _held) = held_pump(&ring, "parent exiting\n", "");

        let started = Instant::now();
        settle_stderr_pump("orphaning", &ring, pump, BOUND).await;
        assert_eq!(
            started.elapsed(),
            BOUND,
            "the restart must wait exactly the bound for a pipe that stays open, no longer"
        );

        let next = lock(&ring).begin_process();
        lock(&ring).push_line_from(next, "next process booting");
        tokio::time::sleep(Duration::from_secs(60)).await;

        let snapshot = lock(&ring).snapshot(None, None);
        match &snapshot.capture {
            CaptureState::Incomplete { reason } => assert!(
                reason.contains("had not reached EOF") && reason.contains("250ms"),
                "the reason must say what is missing and after how long: {reason}"
            ),
            other => panic!("expected Incomplete while the pipe is held open, got {other:?}"),
        }
        assert_eq!(
            untimed(snapshot.entries),
            vec![
                line("parent exiting"),
                TailEntry::ProcessStart,
                line("next process booting"),
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_reader_that_reaches_eof_within_the_bound_leaves_the_tail_captured() {
        let ring = Arc::new(Mutex::new(StderrRing::new(StderrTailConfig::default())));
        let (pump, release) = held_pump(&ring, "one\n", "two\n");
        release.send(()).unwrap();

        settle_stderr_pump("clean", &ring, pump, BOUND).await;

        let snapshot = lock(&ring).snapshot(None, None);
        assert_eq!(snapshot.capture, CaptureState::Captured);
        assert_eq!(untimed(snapshot.entries), vec![line("one"), line("two")]);
    }
}

/// Containment of a module's process tree (issue #109).
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
mod job_containment_tests {
    use super::*;
    use std::{
        path::{Path, PathBuf},
        sync::{Arc, Mutex},
        time::{Duration, Instant},
    };
    use subc_test_support::TestTempDir;

    /// The stub, expected beside this test executable.
    ///
    /// The existence check is here for the reason its twin at `fake_aft_stub_path`
    /// documents: `--lib` does not build `[[bin]]` targets, and a bare spawn
    /// failure then reads as a broken test rather than an unbuilt dependency.
    fn stub_path() -> PathBuf {
        let mut path = std::env::current_exe().expect("current_exe available in tests");
        path.pop();
        path.pop();
        path.push("fake-aft-stub.exe");
        assert!(
            path.exists(),
            "fake-aft-stub not built at {}: run `cargo test -p subc-core` (which builds \
             [[bin]] targets) rather than `cargo test -p subc-core --lib` (which does not)",
            path.display()
        );
        path
    }

    /// Poll for the grandchild pid the stub records, and parse it.
    fn read_grandchild_pid(path: &Path) -> u32 {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(contents) = std::fs::read_to_string(path) {
                if let Ok(pid) = contents.trim().parse() {
                    return pid;
                }
            }
            assert!(
                Instant::now() < deadline,
                "the stub never recorded a grandchild pid at {}",
                path.display()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Everything one fixture run needs, so the two tests below differ in exactly
    /// one place: whether the child is contained.
    struct Fixture {
        _dir: TestTempDir,
        module_id: String,
        grandchild: u32,
        child: Option<SupervisedChild>,
        registry: Arc<Registry>,
        snapshot: Arc<Mutex<SupervisorSnapshot>>,
        terminal_ring: Arc<Mutex<TerminalRing>>,
        spawn_events: SpawnEventFeed,
    }

    fn fixture(label: &str, module_id: &str) -> Fixture {
        let dir = TestTempDir::new(label);
        let pid_file = dir.join("grandchild.pid");
        let supervisor = Supervisor::new_for_test(
            Arc::new(Registry::default()),
            RestartPolicy::new(3, Duration::ZERO),
        );
        let runtime = supervisor.runtime_config();
        let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::starting()));
        let spec = ModuleSpec {
            module_id: module_id.to_string(),
            program: stub_path(),
            // Zero args deliberately: a `--subc` argument would make the stub dial
            // a daemon that is not there, and the failure would land in the same
            // stderr ring this fixture exists to keep quiet.
            args: Vec::new(),
            env: vec![
                ("FAKE_AFT_NEVER_CONNECT".to_string(), "1".to_string()),
                (
                    "FAKE_AFT_GRANDCHILD_PID_FILE".to_string(),
                    pid_file.display().to_string(),
                ),
            ],
            reserved: false,
            reserved_prefixes: Vec::new(),
            protocol: ModuleProtocol::Subc,
            overlap: Default::default(),
        };
        let child = spawn_and_mark_running(&spec, &runtime, &snapshot)
            .expect("spawn the supervised fixture");
        let grandchild = read_grandchild_pid(&pid_file);
        Fixture {
            _dir: dir,
            module_id: module_id.to_string(),
            grandchild,
            child: Some(child),
            registry: Arc::new(Registry::default()),
            snapshot,
            terminal_ring: Arc::clone(&runtime.terminal_ring),
            spawn_events: SpawnEventFeed::default(),
        }
    }

    impl Fixture {
        /// Drain through the supervisor's own teardown path.
        async fn drain(&mut self) {
            let child = self
                .child
                .take()
                .expect("the fixture child is still present");
            drain_child_to_state(
                &self.module_id,
                ModuleProtocol::Subc,
                // No forwarding table in this fixture, so nothing reaches the
                // child over a connection.
                StopNotice::NotSent,
                &self.registry,
                None,
                &self.snapshot,
                &self.terminal_ring,
                &self.spawn_events,
                child,
                Duration::from_millis(500),
                ModuleState::Stopped,
                Some(false),
            )
            .await
            .expect("drain the supervised fixture");
        }
    }

    /// Teardown reaps the grandchild, not merely the direct child.
    ///
    /// This is the assertion the change exists for. Before containment the
    /// grandchild survived: it is a separate process, and `start_kill` is
    /// `TerminateProcess` scoped to one pid.
    #[tokio::test]
    async fn teardown_reaps_the_grandchild() {
        let mut fixture = fixture("teardown-grandchild", "tree-teardown");
        let grandchild = fixture.grandchild;

        assert!(
            subc_jobobject::process_exists(grandchild),
            "grandchild {grandchild} must be alive before teardown, or this proves nothing"
        );

        fixture.drain().await;

        assert!(
            subc_jobobject::wait_for_process_exit(grandchild, Duration::from_secs(10)),
            "grandchild {grandchild} outlived module teardown: the tree was not contained"
        );
    }

    /// The mutation control: with containment withheld, the grandchild survives
    /// the same kill.
    ///
    /// This is the defect reproduction from #109 — a direct-child kill reaches
    /// one pid, and the grandchild is a different process. It spawns OUTSIDE the
    /// supervisor because `spawn_and_mark_running` now always contains on
    /// Windows, which is the point: there is no longer a path that spawns
    /// uncontained, so the control has to construct one.
    ///
    /// Its job is to keep `teardown_reaps_the_grandchild` honest. If the
    /// grandchild ever dies here, that test is passing for a reason unrelated to
    /// the job object and the containment claim is unproven.
    #[test]
    fn an_uncontained_grandchild_survives_a_direct_child_kill() {
        let dir = TestTempDir::new("teardown-uncontained");
        let pid_file = dir.join("grandchild.pid");
        let mut child = std::process::Command::new(stub_path())
            .env("FAKE_AFT_NEVER_CONNECT", "1")
            .env(
                "FAKE_AFT_GRANDCHILD_PID_FILE",
                pid_file.display().to_string(),
            )
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn the uncontained fixture");
        let grandchild = read_grandchild_pid(&pid_file);

        // Exactly what the pre-fix teardown did: kill the direct child.
        child.kill().expect("kill the direct child");
        let _ = child.wait();

        assert!(
            subc_jobobject::process_exists(grandchild),
            "grandchild {grandchild} died with the direct child, so this control no longer \
             distinguishes contained from uncontained teardown and the regression test is \
             passing vacuously"
        );

        // The orphan this control demonstrates is the leak the fix prevents, so
        // the control must not leave one behind.
        kill_tree(grandchild);
    }

    /// Crash durability: closing the containment handle reaps the tree with no
    /// teardown code running at all.
    ///
    /// This is the case `taskkill /T` cannot cover — a daemon that dies cannot
    /// call anything — and it is why containment is a kernel property of the
    /// handle rather than a step in the drain. Discovered by getting the
    /// mutation control wrong: clearing `job` to "disable" containment instead
    /// killed the tree, which is the guarantee, not a mistake.
    #[tokio::test]
    async fn dropping_containment_reaps_the_grandchild() {
        let mut fixture = fixture("drop-containment", "tree-drop");
        let grandchild = fixture.grandchild;

        assert!(subc_jobobject::process_exists(grandchild));

        // No `drain` call, no kill: dropping the handle is the entire mechanism.
        fixture.child.as_mut().expect("child present").job = None;

        assert!(
            subc_jobobject::wait_for_process_exit(grandchild, Duration::from_secs(10)),
            "grandchild {grandchild} survived the containment handle closing, so a daemon \
             crash would leave the tree behind"
        );
    }

    /// Kill a pid and its tree, then confirm it is gone.
    fn kill_tree(pid: u32) {
        let _ = std::process::Command::new("taskkill.exe")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        assert!(
            subc_jobobject::wait_for_process_exit(pid, Duration::from_secs(10)),
            "could not clean up grandchild {pid}"
        );
    }
}

#[cfg(test)]
mod privacy_trampoline_configuration_tests {
    #[tokio::test]
    #[cfg_attr(not(target_os = "macos"), ignore = "requires macOS privacy trampoline")]
    async fn macos_spawn_without_a_configured_trampoline_refuses_by_name() {
        #[cfg(target_os = "macos")]
        {
            let supervisor = super::Supervisor::new(
                std::sync::Arc::new(crate::Registry::default()),
                super::RestartPolicy::default(),
            );
            let error = supervisor.spawn(spec()).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("no privacy trampoline configured"),
                "{error}"
            );
        }
    }

    #[tokio::test]
    #[cfg_attr(not(target_os = "macos"), ignore = "requires macOS privacy trampoline")]
    async fn macos_wrong_trampoline_is_rejected_by_the_startup_probe() {
        #[cfg(target_os = "macos")]
        {
            let supervisor = super::Supervisor::new(
                std::sync::Arc::new(crate::Registry::default()),
                super::RestartPolicy::default(),
            )
            .with_privacy_trampoline(std::env::current_exe().unwrap());
            let error = supervisor.spawn(spec()).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("binary does not implement the privacy trampoline protocol"),
                "{error}"
            );
        }
    }

    #[cfg(target_os = "macos")]
    fn spec() -> super::ModuleSpec {
        super::ModuleSpec {
            module_id: "privacy-configuration".into(),
            program: "/bin/sleep".into(),
            args: vec!["30".into()],
            env: vec![],
            reserved: false,
            reserved_prefixes: vec![],
            protocol: subc_control::ModuleProtocol::None,
            overlap: super::ModuleOverlap::Exclusive,
        }
    }
}

/// The daemon's real spawn path hands a subc-wire child its launch nonce on
/// descriptor 3, without an environment copy. The shell records the nonce
/// and its environment after exec so these tests observe the real handover.
#[cfg(all(test, unix))]
mod launch_nonce_descriptor_tests {
    use super::{ChildRoster, ModuleProtocol, ModuleSpec, SupervisorHandle};
    use crate::stderr_tail::{StderrRing, StderrTailConfig};
    use std::{
        path::PathBuf,
        sync::{Arc, Mutex},
        time::{Duration, Instant},
    };
    use subc_test_support::TestTempDir;

    async fn probe(role: super::SpawnRole) {
        let scratch = TestTempDir::new("launch-nonce-descriptor");
        let fd_copy = scratch.join("from-descriptor");
        let env_copy = scratch.join("environment");
        let script = format!(
            "env > '{env}.tmp' && mv '{env}.tmp' '{env}'; cat <&3 > '{fd}.tmp' && mv '{fd}.tmp' '{fd}'; sleep 30",
            fd = fd_copy.display(), env = env_copy.display(),
        );
        let xdg = |name: &str| (name.to_string(), scratch.join(name).display().to_string());
        let spec = ModuleSpec {
            module_id: "nonce-descriptor-probe".to_string(),
            program: PathBuf::from("/bin/sh"),
            args: vec!["-c".to_string(), script],
            env: vec![
                xdg("XDG_DATA_HOME"),
                xdg("XDG_RUNTIME_DIR"),
                xdg("XDG_CONFIG_HOME"),
                ("SUBC_LAUNCH_NONCE".to_string(), "must-not-leak".to_string()),
            ],
            reserved: true,
            reserved_prefixes: Vec::new(),
            protocol: ModuleProtocol::Subc,
            overlap: Default::default(),
        };
        let handle = SupervisorHandle::new();
        let ring = Arc::new(Mutex::new(StderrRing::new(StderrTailConfig::default())));
        let roster = ChildRoster::default();
        #[cfg(target_os = "macos")]
        {
            let path = super::test_privacy_trampoline();
            roster.set_privacy_trampoline(super::probe_privacy_trampoline(&path).map(|()| path));
        }
        let child = super::spawn_child_in_slot(
            &spec,
            None,
            Some(&handle),
            &ring,
            None,
            &roster,
            #[cfg(target_os = "linux")]
            None,
            role,
            matches!(role, super::SpawnRole::SwapCandidate),
        )
        .expect("spawn probe");
        let deadline = Instant::now() + Duration::from_secs(10);
        while !(fd_copy.exists() && env_copy.exists()) {
            assert!(Instant::now() < deadline, "probe never wrote its copies");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let nonce = std::fs::read_to_string(fd_copy).unwrap();
        assert!(!nonce.is_empty());
        let environment = std::fs::read_to_string(env_copy).unwrap();
        assert!(environment
            .lines()
            .any(|line| line.starts_with("SUBC_LAUNCH_NONCE_FD=3:")));
        let copy = environment
            .lines()
            .find_map(|line| line.strip_prefix("SUBC_LAUNCH_NONCE="));
        assert_eq!(
            copy, None,
            "Unix children must never receive the environment nonce"
        );
        if matches!(role, super::SpawnRole::Plain) {
            assert_eq!(
                handle.spawn_nonce(&spec.module_id).as_deref(),
                Some(nonce.as_str())
            );
        }
        drop(child);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_spawned_module_receives_its_nonce_only_on_descriptor_3() {
        probe(super::SpawnRole::Plain).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn swap_candidate_receives_its_nonce_only_on_descriptor_3() {
        probe(super::SpawnRole::SwapCandidate).await;
    }
}

#[cfg(all(test, target_os = "linux"))]
mod cgroup_containment_tests {
    use super::*;
    use subc_test_support::TestTempDir;

    fn running(pid: u32) -> bool {
        // An orphan can remain a zombie until the container init reaps it.
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|stat| {
                stat.rsplit_once(") ")
                    .map(|(_, rest)| rest.starts_with('Z'))
            })
            .is_some_and(|zombie| !zombie)
    }

    #[tokio::test]
    async fn linux_teardown_reaps_the_grandchild() {
        teardown_tree("linux_teardown_reaps_the_grandchild", false).await;
    }

    #[tokio::test]
    async fn linux_shutdown_straggler_reaps_the_grandchild() {
        teardown_tree("linux_shutdown_straggler_reaps_the_grandchild", true).await;
    }

    async fn teardown_tree(test_name: &str, shutdown: bool) {
        let dir = TestTempDir::new(test_name);
        let root = PathBuf::from(format!(
            "/sys/fs/cgroup/subc-tree-test-{test_name}-{}-{}",
            std::process::id(),
            unix_ms_now()
        ));
        if let Err(error) = std::fs::create_dir(&root) {
            assert!(
                std::env::var_os("SUBC_REQUIRE_CGROUP_TEST").is_none(),
                "required cgroup test cannot execute: {error}"
            );
            eprintln!(
                "SKIP {test_name}: no writable delegated cgroup at {}: {error}",
                root.display()
            );
            return;
        }
        let placement = subc_cgroup::prepare_at(&root)
            .expect("prepare isolated kernel cgroup")
            .expect("isolated cgroup is delegated");
        let module_id = "tree-teardown";
        let module = placement
            .module_path(module_id)
            .expect("create isolated module cgroup");
        if !module.join("cgroup.kill").exists() {
            std::fs::remove_dir(&module).unwrap();
            std::fs::remove_dir(root.join("subc-modules")).unwrap();
            std::fs::remove_dir(&root).unwrap();
            assert!(
                std::env::var_os("SUBC_REQUIRE_CGROUP_TEST").is_none(),
                "required cgroup.kill interface unavailable"
            );
            eprintln!("SKIP {test_name}: cgroup.kill unavailable (kernel < 5.14)");
            return;
        }
        let supervisor = Supervisor::new_for_test(
            Arc::new(Registry::default()),
            RestartPolicy::new(3, Duration::ZERO),
        )
        .with_cgroup_placement(Some(placement));
        let mut runtime = supervisor.runtime_config();
        runtime.child_roster = runtime
            .child_roster
            .for_module(Arc::new(Mutex::new(Duration::from_millis(100))));
        let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::starting()));
        let pid_file = dir.join("grandchild.pid");
        let spec = ModuleSpec {
            module_id: module_id.to_string(),
            program: PathBuf::from("/bin/sh"),
            args: vec![
                "-c".into(),
                "trap '' TERM; sleep 600 & echo $! > \"$1\"; wait".into(),
                "fixture".into(),
                pid_file.display().to_string(),
            ],
            env: ["XDG_DATA_HOME", "XDG_RUNTIME_DIR", "XDG_CONFIG_HOME"]
                .into_iter()
                .map(|key| (key.to_string(), dir.display().to_string()))
                .collect(),
            reserved: false,
            reserved_prefixes: Vec::new(),
            protocol: ModuleProtocol::None,
            overlap: Default::default(),
        };
        let child =
            spawn_and_mark_running(&spec, &runtime, &snapshot).expect("spawn supervised tree");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let grandchild: u32 = loop {
            if let Ok(contents) = std::fs::read_to_string(&pid_file) {
                if let Ok(pid) = contents.trim().parse() {
                    break pid;
                }
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "grandchild pid was not recorded"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        assert!(
            running(grandchild),
            "grandchild must be alive before teardown"
        );
        if shutdown {
            let mut child = child;
            crate::child_roster::end_children_for_daemon_shutdown(
                &runtime.child_roster,
                false,
                std::future::pending(),
            )
            .await;
            child.wait().await.expect("reap shutdown straggler");
        } else {
            drain_child_to_state(
                module_id,
                ModuleProtocol::None,
                StopNotice::NotSent,
                &Registry::default(),
                None,
                &snapshot,
                &runtime.terminal_ring,
                &SpawnEventFeed::default(),
                child,
                Duration::from_millis(100),
                ModuleState::Stopped,
                Some(false),
            )
            .await
            .expect("real supervisor teardown");
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while running(grandchild) && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let survived = running(grandchild);
        // Kill a surviving grandchild so a failed test does not leave it behind.
        if survived {
            let pid = rustix::process::Pid::from_raw(grandchild as i32).unwrap();
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        if module.exists() {
            std::fs::remove_dir(&module).expect("remove empty module cgroup");
        }
        std::fs::remove_dir(root.join("subc-modules")).unwrap();
        std::fs::remove_dir(&root).unwrap();
        assert!(
            !survived,
            "grandchild {grandchild} outlived module teardown"
        );
        eprintln!("EXECUTED {test_name}: grandchild {grandchild} killed in isolated cgroup");
    }
}
