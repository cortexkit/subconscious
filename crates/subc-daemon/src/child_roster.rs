//! Every supervised process this daemon has spawned and not yet reaped, and the
//! daemon's own end-of-life stop for them.
//!
//! Each supervised module runs in its own process group (see
//! `spawn_child_in_slot`). That keeps a service manager's process-group kill
//! from reaching modules before they see EOF on their control connection, but
//! it also means nothing outside this daemon will end a child that outlives it.
//! A `protocol: "none"` child (the NATS server) has no control connection and
//! never sees EOF at all; left alone it would survive the daemon as an orphan
//! and the next daemon would start a second one that fights it for its port.
//! So the daemon ends its own children on the announced-shutdown path, and this
//! roster is how it finds them: the child handles themselves are owned by the
//! per-module supervisor tasks, which keep reaping them while shutdown runs.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex, MutexGuard, OnceLock,
    },
    time::Duration,
};

use subc_control::ModuleProtocol;
use tracing::warn;

use crate::live_children::{ExecutableIdentity, LiveChild};

/// One live supervised process.
///
/// Signalled only by the Unix shutdown stop; Windows child lifetime is a
/// job-object concern, so there the entry only feeds the live-children record.
#[cfg_attr(not(unix), allow(dead_code))]
#[derive(Debug, Clone)]
pub(crate) struct RosterEntry {
    pub(crate) module_id: String,
    pub(crate) pid: u32,
    pub(crate) protocol: ModuleProtocol,
    /// Kernel start time where the platform exposes one, used to refuse a
    /// signal to a different process that has reused a reaped child's pid.
    pub(crate) start_time: Option<u64>,
    /// What the live-children record says about this process, for the next
    /// daemon's orphan sweep if this one dies without stopping it.
    recorded: RecordedIdentity,
    /// The module's resolved drain budget, shared with its supervisor so a
    /// configuration rescan that changes it is seen at shutdown.
    drain_budget: Arc<Mutex<Duration>>,
}

/// The identity facts the live-children record keeps for one process beyond
/// its module id, pid and protocol. See `live_children::LiveChild`.
#[derive(Debug, Clone, Default)]
pub(crate) struct RecordedIdentity {
    pub(crate) start_time: Option<u64>,
    pub(crate) executable: Option<ExecutableIdentity>,
    pub(crate) cgroup_name: Option<String>,
    /// The owning subtree for shutdown tree kills; not persisted in the record.
    #[cfg(target_os = "linux")]
    pub(crate) cgroup_placement: Option<subc_cgroup::Placement>,
}

/// Set once, when the daemon begins its announced shutdown, and never cleared.
///
/// Shared by the roster (which refuses spawns once it is set), every module's
/// terminal ring (which records any exit after it as `daemon_shutdown`) and the
/// control handler's capability-requirement log (which reports requirement
/// changes after it at INFO with `cause=daemon_shutdown`), so the reap path,
/// the spawn path, the record and the log all read the same flag.
#[derive(Debug, Clone, Default)]
pub(crate) struct DaemonShutdownFlag(Arc<AtomicBool>);

impl DaemonShutdownFlag {
    pub(crate) fn is_set(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    #[cfg(unix)]
    fn set(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// Set the flag without a roster, so a test can stand in for the start of
    /// daemon shutdown on any platform.
    #[cfg(test)]
    pub(crate) fn set_for_test(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[derive(Debug, Default)]
struct RosterInner {
    next_key: AtomicU64,
    closed: DaemonShutdownFlag,
    live: Mutex<HashMap<u64, RosterEntry>>,
    record_write: Mutex<()>,
    /// Where the live-children record is written; unset means no record, the
    /// default for an in-process daemon (see `BootstrapConfig`).
    record_path: OnceLock<PathBuf>,
    retained: Mutex<Vec<(LiveChild, crate::live_children::RetainedReason)>>,
    #[cfg(target_os = "macos")]
    privacy_trampoline: OnceLock<Result<PathBuf, String>>,
    /// Test-only: called before the record is serialized and written, so a
    /// test can block there to stand in for a slow write.
    #[cfg(test)]
    record_write_hook: Mutex<Option<RecordWriteHook>>,
}

#[cfg(test)]
type RecordWriteFn = dyn Fn(&HashMap<u64, RosterEntry>) + Send + Sync;

#[cfg(test)]
#[derive(Clone)]
struct RecordWriteHook(Arc<RecordWriteFn>);

#[cfg(test)]
impl std::fmt::Debug for RecordWriteHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RecordWriteHook")
    }
}

impl RosterInner {
    /// Run roster work that may wait on the record write without holding up
    /// the rest of the async runtime.
    ///
    /// Rewriting the live-children record syncs the new file and then its
    /// directory to disk, and on a busy disk each sync can take seconds. The
    /// roster is called synchronously from async tasks: a module's supervisor
    /// admits each child it spawns and releases each child it reaps. Blocking
    /// a tokio worker thread for that long stalls more than the calling task.
    /// A task woken by I/O or a timer normally runs on the worker that was
    /// polling the runtime's I/O and timer driver, and while that worker is
    /// blocked no other worker takes the driver over, so every timer and
    /// socket in the daemon waits. That includes the authentication handshake
    /// of the module just spawned, and the Rust SDK gives up on that handshake
    /// after 2 seconds, so a restart on a busy machine crashed its own
    /// replacement.
    ///
    /// The wait covers the record-write gate as well as the write. Readers of
    /// the live roster do not take that gate, so shutdown can still poll them.
    ///
    /// On a multi-threaded runtime `block_in_place` hands this worker's queued
    /// tasks to another thread for the duration. A current-thread runtime
    /// (some in-process test daemons) cannot do that, so the work runs inline
    /// there. Without a record path nothing slow happens under the lock, and
    /// the work also runs inline.
    fn without_stalling_runtime<T>(&self, work: impl FnOnce() -> T) -> T {
        let multi_threaded = tokio::runtime::Handle::try_current().is_ok_and(|handle| {
            handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread
        });
        if multi_threaded && self.record_path.get().is_some() {
            tokio::task::block_in_place(work)
        } else {
            work()
        }
    }

    /// Rewrite a snapshot taken after acquiring the record-write gate. Taking
    /// each snapshot inside that gate prevents an older snapshot from replacing
    /// a newer record, without locking the live roster across disk syncs.
    fn write_record(&self, live: &HashMap<u64, RosterEntry>) -> std::io::Result<()> {
        let Some(path) = self.record_path.get() else {
            return Ok(());
        };
        #[cfg(test)]
        {
            let hook = lock(&self.record_write_hook).clone();
            if let Some(hook) = hook {
                (hook.0)(live);
            }
        }
        let mut entries: Vec<(&u64, &RosterEntry)> = live.iter().collect();
        entries.sort_by_key(|(key, _)| **key);
        let children: Vec<LiveChild> = entries
            .into_iter()
            .map(|(_, entry)| LiveChild {
                module_id: entry.module_id.clone(),
                pid: entry.pid,
                protocol: entry.protocol,
                start_time: entry.recorded.start_time,
                executable: entry.recorded.executable,
                cgroup_name: entry.recorded.cgroup_name.clone(),
            })
            .collect();
        let children: Vec<_> = children
            .into_iter()
            .chain(lock(&self.retained).iter().map(|(entry, _)| entry.clone()))
            .collect();
        let reasons = lock(&self.retained)
            .iter()
            .map(|(entry, reason)| (entry.pid, *reason))
            .collect();
        if let Err(error) =
            crate::live_children::write_record_with_reasons(path, &children, reasons)
        {
            warn!(
                path = %path.display(),
                %error,
                "could not rewrite the live-children record; a crash now could leave orphans the next boot cannot find"
            );
            return Err(error);
        }
        Ok(())
    }

    fn recheck_retained(&self) {
        self.without_stalling_runtime(|| self.recheck_retained_now());
    }

    fn recheck_retained_now(&self) {
        // Serialize pruning with admits and releases, but leave the live roster
        // readable while probing retained processes and syncing the record.
        let _write = lock(&self.record_write);
        let changed = {
            let mut retained = lock(&self.retained);
            let before = retained.len();
            retained.retain(|(entry, _)| {
                #[cfg(unix)]
                {
                    let Some(pid) = i32::try_from(entry.pid)
                        .ok()
                        .and_then(rustix::process::Pid::from_raw)
                    else {
                        return true;
                    };
                    // Only signal-zero ESRCH permits dropping an unidentifiable
                    // previous instance. Permission and other errors keep it blocked.
                    rustix::process::test_kill_process(pid) != Err(rustix::io::Errno::SRCH)
                }
                #[cfg(windows)]
                {
                    match subc_os::Process::open(entry.pid) {
                        Ok(None) => false,
                        Ok(Some(process)) => {
                            !matches!(process.wait_for_exit(Duration::ZERO), Ok(true))
                        }
                        Err(_) => true,
                    }
                }
                #[cfg(not(any(unix, windows)))]
                {
                    let _ = entry;
                    true
                }
            });
            retained.len() != before
        };
        if changed {
            let live = lock(&self.live).clone();
            let _ = self.write_record(&live);
        }
    }
}

/// Shared by every clone of one `Supervisor` and every module task it starts.
/// A module task's copy also carries that module's drain budget, which every
/// process it spawns is admitted with.
#[derive(Debug, Clone)]
pub(crate) struct ChildRoster {
    inner: Arc<RosterInner>,
    drain_budget: Arc<Mutex<Duration>>,
}

impl Default for ChildRoster {
    fn default() -> Self {
        Self {
            inner: Arc::default(),
            drain_budget: Arc::new(Mutex::new(crate::supervise::DEFAULT_DRAIN_TIMEOUT)),
        }
    }
}

/// Holds a child's roster entry. Dropped when the child is reaped, or when its
/// handle is dropped (which kills it), so the roster never outlives the pid.
#[derive(Debug)]
pub(crate) struct RosterGuard {
    inner: Arc<RosterInner>,
    key: u64,
}

#[derive(Debug)]
pub(crate) struct AdmissionError {
    source: std::io::Error,
    guard: RosterGuard,
}

impl AdmissionError {
    /// Keep shutdown ownership until the caller has killed and reaped the
    /// rejected child. A failed record write is not proof that the child exited.
    pub(crate) fn with_cleanup<T>(self, cleanup: impl FnOnce(std::io::Error) -> T) -> T {
        let result = cleanup(self.source);
        drop(self.guard);
        result
    }
}

impl Drop for RosterGuard {
    fn drop(&mut self) {
        self.inner.without_stalling_runtime(|| {
            let _write = lock(&self.inner.record_write);
            let snapshot = {
                let mut live = lock(&self.inner.live);
                live.remove(&self.key).map(|_| live.clone())
            };
            if let Some(snapshot) = snapshot {
                let _ = self.inner.write_record(&snapshot);
            }
        });
    }
}

#[cfg(target_os = "macos")]
impl RosterGuard {
    /// Publish an executable only after the privacy trampoline has exec'd and
    /// the kernel image agrees with the configured module. Before then shutdown
    /// owns the pid, but orphan cleanup has no image it could mistakenly trust.
    pub(crate) fn confirm_executable(
        &self,
        observation: subc_os::Observation,
    ) -> std::io::Result<()> {
        self.inner.without_stalling_runtime(|| {
            let _write = lock(&self.inner.record_write);
            let snapshot = {
                let mut live = lock(&self.inner.live);
                if let Some(entry) = live.get_mut(&self.key) {
                    entry.recorded.start_time = Some(observation.start_time);
                    entry.recorded.executable =
                        observation.executable.map(ExecutableIdentity::from);
                    Some(live.clone())
                } else {
                    None
                }
            };
            if let Some(snapshot) = snapshot {
                self.inner.write_record(&snapshot)?;
            }
            Ok(())
        })
    }
}

impl ChildRoster {
    #[cfg(target_os = "macos")]
    pub(crate) fn set_privacy_trampoline(&self, result: Result<PathBuf, String>) {
        let _ = self.inner.privacy_trampoline.set(result);
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn privacy_trampoline(&self) -> Result<PathBuf, String> {
        self.inner
            .privacy_trampoline
            .get()
            .cloned()
            .unwrap_or_else(|| Err("no privacy trampoline configured".to_string()))
    }
    /// The same roster, admitting children under one module's drain budget.
    pub(crate) fn for_module(&self, drain_budget: Arc<Mutex<Duration>>) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            drain_budget,
        }
    }

    /// True once daemon shutdown has begun. A spawn after this point would
    /// create a child the shutdown stop may already have finished looking for,
    /// so spawning refuses instead (the supervisor would otherwise restart each
    /// module as it exits on EOF).
    pub(crate) fn is_closed(&self) -> bool {
        self.inner.closed.is_set()
    }

    /// The flag [`Self::close`] sets, for the terminal rings to read.
    pub(crate) fn shutdown_flag(&self) -> DaemonShutdownFlag {
        self.inner.closed.clone()
    }

    /// Keep the live-children record at `path` from now on. Set once, before
    /// anything is admitted; a second call is ignored.
    pub(crate) fn record_to(&self, path: PathBuf) {
        let (retained, reasons) =
            crate::live_children::read_record_with_reasons(&path).unwrap_or_default();
        let first_record = self.inner.record_path.set(path).is_ok();
        if first_record {
            *lock(&self.inner.retained) = retained
                .into_iter()
                .map(|entry| {
                    let reason = reasons
                        .get(&entry.pid)
                        .copied()
                        .unwrap_or(crate::live_children::RetainedReason::Hidden);
                    (entry, reason)
                })
                .collect();
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                let weak = Arc::downgrade(&self.inner);
                runtime.spawn(async move {
                    loop {
                        tokio::time::sleep(Duration::from_secs(5)).await;
                        let Some(inner) = weak.upgrade() else {
                            return;
                        };
                        inner.recheck_retained();
                    }
                });
            }
        }
    }

    pub(crate) fn previous_instance_pid(&self, module_id: &str) -> Option<u32> {
        self.previous_instance(module_id).map(|(pid, _)| pid)
    }

    pub(crate) fn previous_instance(
        &self,
        module_id: &str,
    ) -> Option<(u32, crate::live_children::RetainedReason)> {
        self.inner.recheck_retained();
        lock(&self.inner.retained)
            .iter()
            .find(|(entry, _)| entry.module_id == module_id)
            .map(|(entry, reason)| (entry.pid, *reason))
    }

    pub(crate) fn admit(
        &self,
        module_id: String,
        pid: u32,
        protocol: ModuleProtocol,
        start_time: Option<u64>,
        recorded: RecordedIdentity,
    ) -> Result<RosterGuard, AdmissionError> {
        let key = self.inner.next_key.fetch_add(1, Ordering::Relaxed);
        // The process is already spawned. Publish ownership before waiting for
        // another writer, so shutdown can stop it even while that writer syncs.
        lock(&self.inner.live).insert(
            key,
            RosterEntry {
                module_id,
                pid,
                protocol,
                start_time,
                recorded,
                drain_budget: Arc::clone(&self.drain_budget),
            },
        );
        let guard = RosterGuard {
            inner: Arc::clone(&self.inner),
            key,
        };
        let recorded = self.inner.without_stalling_runtime(|| {
            let _write = lock(&self.inner.record_write);
            let snapshot = lock(&self.inner.live).clone();
            self.inner.write_record(&snapshot)
        });
        match recorded {
            Ok(()) => Ok(guard),
            Err(source) => Err(AdmissionError { source, guard }),
        }
    }

    #[cfg(unix)]
    fn live(&self) -> Vec<(u64, RosterEntry)> {
        lock(&self.inner.live)
            .iter()
            .map(|(key, entry)| (*key, entry.clone()))
            .collect()
    }

    /// Run `hook` at every record write from now on (when a record path is
    /// set), before the roster is serialized and written, with the roster
    /// being written. Only the Unix restart test uses it.
    #[cfg(all(test, unix))]
    pub(crate) fn set_record_write_hook_for_test(
        &self,
        hook: impl Fn(&HashMap<u64, RosterEntry>) + Send + Sync + 'static,
    ) {
        *lock(&self.inner.record_write_hook) = Some(RecordWriteHook(Arc::new(hook)));
    }

    /// Mark daemon shutdown as begun. Idempotent.
    ///
    /// Called at the very start of the announced shutdown, before the notice
    /// and before any connection is closed: from then on no module is
    /// respawned, and every exit is recorded as `daemon_shutdown`.
    #[cfg(unix)]
    pub(crate) fn close(&self) {
        self.inner.closed.set();
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(unix)]
pub(crate) use unix_shutdown::end_children_for_daemon_shutdown;

#[cfg(unix)]
mod unix_shutdown {
    use std::{
        collections::{HashMap, HashSet},
        future::Future,
        time::Duration,
    };

    use rustix::process::{kill_process, Pid, Signal};
    use tokio::time::{sleep, Instant};
    use tracing::{debug, info, warn};

    use super::{lock, ChildRoster, RosterEntry};
    use subc_control::ModuleProtocol;

    /// The longest any one child is waited on before escalation, whatever its
    /// drain budget says.
    ///
    /// Modules are outside the daemon's process group, so if the service
    /// manager SIGKILLs the daemon before this stop finishes, whatever the
    /// daemon has not ended survives it. A subc module has already had its EOF
    /// and finishes its own teardown regardless, but a `protocol: "none"` child
    /// would be orphaned. So the whole shutdown must fit inside the service
    /// manager's stop timeout: the 0.5 s notice and 2 s drain before this, this
    /// cap, [`TERM_TO_KILL`] and [`CHILD_REAP_BOUND`] after it come to 28.25 s,
    /// under the 35 s `ExitTimeOut` / `TimeoutStopSec` that `ck setup` writes
    /// (see `desired_definition`). 25 s covers BROCA's teardown (10 s run grace,
    /// then a seal, inside a 20 s budget; 12 s measured) with room to spare.
    const CHILD_SHUTDOWN_CAP: Duration = Duration::from_secs(25);
    /// For a subc module still running at its deadline: the time between the
    /// SIGTERM sent then and the SIGKILL. Long enough for a handler to write a
    /// last line and exit.
    const TERM_TO_KILL: Duration = Duration::from_millis(500);
    /// After a SIGKILL, how long to wait for the supervisor tasks to reap.
    /// SIGKILL cannot be ignored, so this only covers scheduling; it bounds the
    /// wait even if a reap never lands.
    const CHILD_REAP_BOUND: Duration = Duration::from_millis(250);
    const POLL: Duration = Duration::from_millis(10);

    /// End every supervised child before the daemon exits.
    ///
    /// The caller has already closed every connection, so each subc module has
    /// had its EOF: that is its one stop request, and it is left alone to run
    /// its own teardown. A `protocol: "none"` child has no connection, so its
    /// one stop request is SIGTERM, sent here immediately (the same stop the
    /// supervisor sends it on restart).
    ///
    /// Escalation happens per child, at that child's own deadline: its
    /// resolved drain budget (per-module `drain_timeout_ms`, else the daemon
    /// default), capped at [`CHILD_SHUTDOWN_CAP`]. At the deadline a subc
    /// module still running gets SIGTERM and, [`TERM_TO_KILL`] later, SIGKILL;
    /// a `protocol: "none"` child, already asked, gets SIGKILL. All children are
    /// waited on concurrently, so the whole stop takes as long as the longest
    /// single deadline, and returns as soon as every child has been reaped.
    /// A fixed short bound for everyone would SIGKILL a module whose teardown
    /// legitimately takes seconds (BROCA seals in-flight runs) on exactly the
    /// stops where it has work in flight.
    ///
    /// Spawns are refused first, so a module exiting on EOF is not restarted.
    /// `already_escalated` or `escalate` resolving (a second SIGTERM to the
    /// daemon) skips every remaining wait and kills what is left: the
    /// operator has said stop waiting, and leaving children behind would be
    /// the orphan this exists to prevent.
    pub(crate) async fn end_children_for_daemon_shutdown(
        roster: &ChildRoster,
        already_escalated: bool,
        escalate: impl Future<Output = ()>,
    ) {
        roster.close();
        tokio::pin!(escalate);
        let started = Instant::now();
        let mut termed = HashMap::new();
        let mut killed = HashSet::new();
        let mut last_kill: Option<Instant> = None;
        let mut escalated = already_escalated;

        if !escalated {
            for (key, entry) in roster.live() {
                if entry.protocol == ModuleProtocol::None {
                    signal(&entry, Signal::TERM);
                    termed.insert(key, Instant::now());
                }
            }
        }

        loop {
            let live = roster.live();
            if live.is_empty() {
                break;
            }
            let now = Instant::now();
            for (key, entry) in &live {
                if killed.contains(key) {
                    continue;
                }
                let deadline = started + budget(entry);
                let due = shutdown_signal(
                    entry.protocol,
                    now,
                    deadline,
                    termed.get(key).copied(),
                    escalated,
                );
                if due == Some(Signal::KILL) {
                    warn!(
                        module_id = %entry.module_id,
                        pid = entry.pid,
                        escalated,
                        child_stop_elapsed_ms = started.elapsed().as_millis() as u64,
                        term_to_kill_ms = termed.get(key).map(|at| at.elapsed().as_millis() as u64),
                        "supervised child did not exit during daemon shutdown; sending SIGKILL"
                    );
                    #[cfg(target_os = "linux")]
                    if entry.start_time.is_none_or(|expected| {
                        crate::provenance::process_start_time(entry.pid) == Some(expected)
                    }) {
                        if let Some(name) = &entry.recorded.cgroup_name {
                            crate::supervise::kill_module_cgroup(
                                entry.recorded.cgroup_placement.as_ref(),
                                name,
                            );
                        }
                    }
                    signal(entry, Signal::KILL);
                    killed.insert(*key);
                    last_kill = Some(now);
                } else if due == Some(Signal::TERM) {
                    warn!(
                        module_id = %entry.module_id,
                        pid = entry.pid,
                        child_stop_elapsed_ms = started.elapsed().as_millis() as u64,
                        "supervised module still running at its shutdown deadline after EOF; sending SIGTERM"
                    );
                    signal(entry, Signal::TERM);
                    termed.insert(*key, Instant::now());
                }
            }
            // Everything left has been SIGKILLed: wait only for the reaps.
            if live.iter().all(|(key, _)| killed.contains(key))
                && last_kill.is_some_and(|at| now >= at + CHILD_REAP_BOUND)
            {
                break;
            }
            if escalated {
                sleep(POLL).await;
                continue;
            }
            tokio::select! {
                biased;
                _ = escalate.as_mut() => {
                    info!("second SIGTERM: killing remaining supervised children without further grace");
                    escalated = true;
                }
                _ = sleep(POLL) => {}
            }
        }
        info!(
            child_stop_elapsed_ms = started.elapsed().as_millis() as u64,
            "supervised child shutdown wait complete"
        );
    }

    fn budget(entry: &RosterEntry) -> Duration {
        (*lock(&entry.drain_budget)).min(CHILD_SHUTDOWN_CAP)
    }

    fn shutdown_signal(
        protocol: ModuleProtocol,
        now: Instant,
        deadline: Instant,
        term_sent: Option<Instant>,
        escalated: bool,
    ) -> Option<Signal> {
        // A delayed poll must not skip SIGTERM or consume its grace before the
        // signal was sent. The second SIGTERM deliberately bypasses that grace.
        let kill_due = match protocol {
            ModuleProtocol::None => now >= deadline,
            ModuleProtocol::Subc => term_sent.is_some_and(|at| now >= at + TERM_TO_KILL),
        };
        if escalated || kill_due {
            Some(Signal::KILL)
        } else if now >= deadline && term_sent.is_none() {
            Some(Signal::TERM)
        } else {
            None
        }
    }

    #[test]
    fn release_record_sync_does_not_lock_the_shutdown_roster() {
        let dir = cortexkit_test_support::ScratchDir::new("roster-release-sync");
        let path = dir.join("children.json");
        let roster = ChildRoster::default();
        roster.record_to(path.clone());
        let guard = roster
            .admit(
                "record-only".into(),
                std::process::id(),
                ModuleProtocol::Subc,
                None,
                super::RecordedIdentity::default(),
            )
            .unwrap();
        let entered = std::sync::Arc::new(std::sync::Barrier::new(2));
        let release = std::sync::Arc::new(std::sync::Barrier::new(2));
        roster.set_record_write_hook_for_test({
            let entered = entered.clone();
            let release = release.clone();
            move |live| {
                if live.is_empty() {
                    entered.wait();
                    release.wait();
                }
            }
        });
        let writer = std::thread::spawn(move || drop(guard));
        entered.wait();
        let readable = roster.inner.live.try_lock().is_ok();
        release.wait();
        writer.join().unwrap();
        assert!(
            readable,
            "a record sync must not block shutdown's live-child snapshot"
        );
        assert!(roster.live().is_empty());
        assert!(crate::live_children::read_record_with_reasons(&path)
            .unwrap()
            .0
            .is_empty());
    }

    #[test]
    fn failed_admission_keeps_ownership_until_child_is_killed_and_reaped() {
        struct OwnedChild(std::process::Child);
        impl Drop for OwnedChild {
            fn drop(&mut self) {
                if self.0.try_wait().is_ok_and(|status| status.is_none()) {
                    let _ = self.0.kill();
                    let _ = self.0.wait();
                }
            }
        }
        let dir = cortexkit_test_support::ScratchDir::new("failed-admission-owner");
        let record = dir.join("children.json");
        std::fs::create_dir(&record).unwrap();
        let roster = ChildRoster::default();
        roster.record_to(record);
        let mut child = OwnedChild(
            std::process::Command::new("sleep")
                .arg("60")
                .spawn()
                .unwrap(),
        );
        let pid = child.0.id();
        let failure = roster
            .admit(
                "rejected".into(),
                pid,
                ModuleProtocol::Subc,
                None,
                super::RecordedIdentity::default(),
            )
            .unwrap_err();
        failure.with_cleanup(|_source| {
            assert!(
                roster.live().iter().any(|(_, entry)| entry.pid == pid),
                "failed admission released ownership before rejection cleanup"
            );
            child.0.kill().unwrap();
            assert!(!child.0.wait().unwrap().success());
            assert!(
                roster.live().iter().any(|(_, entry)| entry.pid == pid),
                "ownership must survive through the reap"
            );
        });
        assert!(
            child.0.try_wait().unwrap().is_some(),
            "the rejected child must be reaped"
        );
        assert!(
            roster.live().is_empty(),
            "cleanup must release the rejected child's roster entry"
        );
    }

    #[test]
    fn queued_admission_remains_visible_to_shutdown() {
        let roster = ChildRoster::default();
        let write = lock(&roster.inner.record_write);
        let worker_roster = roster.clone();
        let worker = std::thread::spawn(move || {
            worker_roster
                .admit(
                    "queued".into(),
                    std::process::id(),
                    ModuleProtocol::Subc,
                    None,
                    super::RecordedIdentity::default(),
                )
                .unwrap()
        });
        // This is a fixture scheduling guard, not a shutdown-policy deadline.
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        let visible = loop {
            if !roster.live().is_empty() {
                break true;
            }
            if std::time::Instant::now() >= deadline {
                break false;
            }
            std::thread::sleep(Duration::from_millis(1));
        };
        drop(write);
        let guard = worker.join().unwrap();
        assert!(
            visible,
            "shutdown must see a spawned child queued behind a record write"
        );
        drop(guard);
    }

    #[test]
    fn late_shutdown_poll_still_gives_sigterm_its_grace() {
        let deadline = Instant::now();
        let late_poll = deadline + Duration::from_secs(1);
        assert_eq!(
            shutdown_signal(ModuleProtocol::Subc, late_poll, deadline, None, false),
            Some(Signal::TERM)
        );
        assert_eq!(
            shutdown_signal(
                ModuleProtocol::Subc,
                late_poll,
                deadline,
                Some(late_poll),
                false
            ),
            None
        );
        assert_eq!(
            shutdown_signal(
                ModuleProtocol::Subc,
                late_poll + TERM_TO_KILL,
                deadline,
                Some(late_poll),
                false
            ),
            Some(Signal::KILL)
        );
        assert_eq!(
            shutdown_signal(
                ModuleProtocol::Subc,
                late_poll,
                deadline,
                Some(late_poll),
                true
            ),
            Some(Signal::KILL)
        );
    }

    fn signal(entry: &RosterEntry, signal: Signal) {
        // A reaped child's pid can be reused. Where the kernel start time is
        // known, refuse to signal a process that is not the one spawned.
        if let Some(expected) = entry.start_time {
            if crate::provenance::process_start_time(entry.pid) != Some(expected) {
                debug!(
                    module_id = %entry.module_id,
                    pid = entry.pid,
                    "supervised child already gone; not signalling its pid"
                );
                return;
            }
        }
        let Some(pid) = i32::try_from(entry.pid).ok().and_then(Pid::from_raw) else {
            return;
        };
        if let Err(error) = kill_process(pid, signal) {
            debug!(
                module_id = %entry.module_id,
                pid = entry.pid,
                ?signal,
                %error,
                "signal to supervised child failed; it has most likely already exited"
            );
        }
    }
}
