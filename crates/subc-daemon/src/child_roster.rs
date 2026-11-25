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

use crate::live_children::{
    AdoptedPids, ExecutableIdentity, LiveChild, ProtectedChild, RecordedChild, RetainedReason,
};

/// One live supervised process.
///
/// Signalled only by the Unix shutdown stop; Windows child lifetime is a
/// job-object concern, so there the entry only feeds the live-children record.
#[cfg_attr(not(unix), allow(dead_code))]
#[derive(Debug, Clone)]
pub(crate) struct RosterEntry {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    adopted_signal: Option<std::sync::Weak<crate::supervise::live_upgrade::VerifiedChild>>,
    pub(crate) module_id: String,
    pub(crate) pid: u32,
    pub(crate) protocol: ModuleProtocol,
    /// Kernel start time where the platform exposes one, used to refuse a
    /// signal to a different process that has reused a reaped child's pid.
    pub(crate) start_time: Option<u64>,
    /// What the live-children record says about this process, for the next
    /// daemon's orphan sweep if this one dies without stopping it.
    recorded: RecordedIdentity,
    resume_token_sha256: Option<String>,
    /// The module's resolved drain budget, shared with its supervisor so a
    /// configuration rescan that changes it is seen at shutdown.
    drain_budget: Arc<Mutex<Duration>>,
}

impl RosterEntry {
    fn to_recorded_child(&self) -> RecordedChild {
        RecordedChild {
            child: LiveChild {
                module_id: self.module_id.clone(),
                pid: self.pid,
                protocol: self.protocol,
                start_time: self.recorded.start_time,
                executable: self.recorded.executable,
                cgroup_name: self.recorded.cgroup_name.clone(),
                container: self.recorded.container.clone(),
            },
            resume_token_sha256: self.resume_token_sha256.clone(),
        }
    }
}

/// The identity facts the live-children record keeps for one process beyond
/// its module id, pid and protocol. See `live_children::LiveChild`.
#[derive(Debug, Clone, Default)]
pub(crate) struct RecordedIdentity {
    pub(crate) start_time: Option<u64>,
    pub(crate) executable: Option<ExecutableIdentity>,
    pub(crate) cgroup_name: Option<String>,
    pub(crate) container: Option<crate::live_children::ContainerIdentity>,
    #[cfg(unix)]
    pub(crate) carried: Option<std::sync::Weak<crate::supervise::containment::CarriedContainment>>,
    #[cfg(unix)]
    pub(crate) plain_group: Option<std::sync::Weak<crate::supervise::plain_group::PlainGroup>>,
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
    #[cfg(target_os = "linux")]
    cgroup_placement: Mutex<Option<subc_cgroup::Placement>>,
    next_key: AtomicU64,
    closed: DaemonShutdownFlag,
    closed_notify: tokio::sync::Notify,
    live: Mutex<HashMap<u64, RosterEntry>>,
    record_write: Mutex<()>,
    /// Where the live-children record is written; unset means no record, the
    /// default for an in-process daemon (see `BootstrapConfig`).
    record_path: OnceLock<PathBuf>,
    recovery_block: OnceLock<String>,
    pending_admissions: Mutex<HashMap<String, (u32, String)>>,
    unquarantined_record: std::sync::atomic::AtomicBool,
    retained: Mutex<Vec<RetainedEntry>>,
    rechecking: AtomicBool,
    #[cfg(target_os = "macos")]
    privacy_trampoline: OnceLock<Result<PathBuf, String>>,
    /// Test-only: called before the record is serialized and written, so a
    /// test can block there to stand in for a slow write.
    #[cfg(test)]
    record_write_hook: Mutex<Option<RecordWriteHook>>,
    #[cfg(all(test, unix))]
    admission_timeout: Mutex<Option<Duration>>,
    #[cfg(all(test, unix))]
    admission_hook: Mutex<Option<AdmissionHook>>,
    #[cfg(test)]
    fail_adoption: AtomicBool,
    #[cfg(all(test, unix))]
    placement_pause: Mutex<HashMap<String, subc_os::admission_gate::PlacementPause>>,
}

#[derive(Debug)]
struct RetainedEntry {
    recorded: RecordedChild,
    reason: Option<RetainedReason>,
    protection: Option<ProtectedChild>,
}

impl RetainedEntry {
    fn waiting_reason(&self) -> &RetainedReason {
        if self.protection.is_some() {
            // Handover children block starts even if the old on-disk reason
            // was non-blocking. Don't replace that reason in the record.
            &RetainedReason::Hidden
        } else {
            self.reason.as_ref().unwrap_or(&RetainedReason::Hidden)
        }
    }
}

#[cfg(test)]
type RecordWriteFn = dyn Fn(&HashMap<u64, RosterEntry>) + Send + Sync;

#[cfg(test)]
#[derive(Clone)]
struct RecordWriteHook(Arc<RecordWriteFn>);

#[cfg(all(test, unix))]
type AdmissionHookFn = dyn Fn(&str, &'static str, Option<u32>) -> bool + Send + Sync;
#[cfg(all(test, unix))]
#[derive(Clone)]
struct AdmissionHook(Arc<AdmissionHookFn>);
#[cfg(all(test, unix))]
impl std::fmt::Debug for AdmissionHook {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("AdmissionHook")
    }
}

#[cfg(test)]
impl std::fmt::Debug for RecordWriteHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RecordWriteHook")
    }
}

impl RosterInner {
    fn ensure_rechecks(self: &Arc<Self>) {
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            if self.rechecking.swap(true, Ordering::Relaxed) {
                return;
            }
            let weak = Arc::downgrade(self);
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
        if self.unquarantined_record.load(Ordering::Acquire) {
            return Err(std::io::Error::other(
                "unusable recovery record could not be quarantined; refusing to overwrite it",
            ));
        }
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
        let retained = lock(&self.retained);
        let children: Vec<_> = entries
            .into_iter()
            .map(|(_, entry)| entry.to_recorded_child())
            .chain(retained.iter().map(|entry| entry.recorded.clone()))
            .collect();
        let reasons = retained
            .iter()
            .filter_map(|entry| Some((entry.recorded.child.pid, entry.reason.clone()?)))
            .collect();
        drop(retained);
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
            #[cfg(unix)]
            let mut updated = false;
            #[cfg(not(unix))]
            let updated = false;
            retained.retain_mut(|retained| {
                // Only the handover's exit handling may remove a Held child.
                // The same rule protects an adopted child awaiting import.
                if retained.protection.is_some() {
                    return true;
                }
                let entry = &retained.recorded.child;
                #[cfg(unix)]
                {
                    let reason = retained.reason.get_or_insert(RetainedReason::Hidden);
                    if entry.container.is_some() {
                        return match crate::live_children::recheck_container(entry,
                            #[cfg(target_os = "linux")]
                            lock(&self.cgroup_placement).as_ref(),
                        ) {
                            Some(next) => {
                                if reason.blocks_fresh_copy() && !next.blocks_fresh_copy() {
                                    tracing::info!(module_id = %entry.module_id, container = ?entry.container,
                                        "container survivors are gone; fresh starts are no longer blocked");
                                }
                                updated |= *reason != next;
                                *reason = next;
                                true
                            }
                            None => {
                                if reason.blocks_fresh_copy() {
                                    tracing::info!(module_id = %entry.module_id, container = ?entry.container,
                                        "container survivors are gone; fresh starts are no longer blocked");
                                }
                                false
                            }
                        };
                    }
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
            retained.len() != before || updated
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
    pub(crate) containment: crate::supervise::containment::Slot,
    pub(crate) counters: crate::observability::DaemonCounters,
}

impl Default for ChildRoster {
    fn default() -> Self {
        Self {
            inner: Arc::default(),
            drain_budget: Arc::new(Mutex::new(crate::supervise::DEFAULT_DRAIN_TIMEOUT)),
            containment: Arc::default(),
            counters: crate::observability::DaemonCounters::default(),
        }
    }
}

/// A cached diagnostic view of containment; it does not own the resource.
#[derive(Debug, Clone)]
pub(crate) struct RetainedView(std::sync::Weak<RosterInner>);

impl RetainedView {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn blocks_upgrade(&self) -> bool {
        self.0.upgrade().is_none_or(|inner| {
            let retained = !lock(&inner.retained).is_empty();
            let awaiting_admission = !lock(&inner.pending_admissions).is_empty();
            inner.recovery_block.get().is_some() || retained || awaiting_admission
        })
    }
    pub(crate) fn has_carried_container(&self, module_id: &str) -> bool {
        let Some(inner) = self.0.upgrade() else {
            return false;
        };
        let live = lock(&inner.live);
        live.values().any(|entry| {
            #[cfg(unix)]
            let carried = entry.recorded.carried.is_some();
            #[cfg(not(unix))]
            let carried = entry.recorded.container.is_some();
            entry.module_id == module_id && carried
        })
    }

    pub(crate) fn recovery_block(&self) -> Option<String> {
        self.0.upgrade()?.recovery_block.get().cloned()
    }

    pub(crate) fn waiting_detail(&self, module_id: &str) -> Option<(u32, String)> {
        let inner = self.0.upgrade()?;
        if let Some((pid, reason)) = lock(&inner.pending_admissions).get(module_id) {
            return Some((*pid, reason.clone()));
        }
        let retained = lock(&inner.retained);
        retained
            .iter()
            .find(|entry| {
                entry.recorded.child.module_id == module_id
                    && (entry.recorded.child.container.is_some() || entry.protection.is_some())
                    && entry.waiting_reason().blocks_fresh_copy()
            })
            .map(|entry| {
                let pid = entry.recorded.child.pid;
                (pid, entry.waiting_reason().waiting_message(pid))
            })
    }
}

/// Holds a child's roster entry. Dropped when the child is reaped, or when its
/// handle is dropped (which kills it), so the roster never outlives the pid.
#[derive(Debug)]
pub(crate) struct RosterGuard {
    inner: Arc<RosterInner>,
    key: u64,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    adoption: Option<Box<crate::supervise::live_upgrade::AdoptionRecovery>>,
}

impl RosterGuard {
    /// Return the durable identity for the exact launch owned by this guard.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn recorded_identity(&self) -> Option<RecordedChild> {
        lock(&self.inner.live)
            .get(&self.key)
            .map(RosterEntry::to_recorded_child)
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn bind_adopted_signal(
        &mut self,
        signal: &Arc<crate::supervise::live_upgrade::VerifiedChild>,
        recovery: crate::supervise::live_upgrade::AdoptionRecovery,
    ) {
        if let Some(entry) = lock(&self.inner.live).get_mut(&self.key) {
            entry.adopted_signal = Some(Arc::downgrade(signal));
        }
        self.adoption = Some(Box::new(recovery));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn retain_held_adoption(&self) {
        self.inner.without_stalling_runtime(|| {
            let _write = lock(&self.inner.record_write);
            let Some(entry) = lock(&self.inner.live).remove(&self.key) else {
                return;
            };
            lock(&self.inner.retained).push(RetainedEntry {
                recorded: entry.to_recorded_child(),
                reason: None,
                protection: Some(ProtectedChild::Held),
            });
        });
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn stop_adoption_without_replacement(&mut self) {
        if let Some(recovery) = &mut self.adoption {
            recovery.no_restart();
        }
    }

    pub(crate) fn resume_token_record(&self) -> Option<ResumeTokenRecord> {
        self.inner.record_path.get()?;
        Some(ResumeTokenRecord {
            inner: Arc::downgrade(&self.inner),
            key: self.key,
        })
    }

    #[cfg(test)]
    fn set_resume_token_sha256(&self, hash: &str) -> std::io::Result<()> {
        self.resume_token_record()
            .ok_or_else(|| std::io::Error::other("no live-children record configured"))?
            .set_sha256(hash)
    }
}

/// Permission to update one record, without prolonging its lifetime past reap.
#[derive(Debug)]
pub(crate) struct ResumeTokenRecord {
    inner: std::sync::Weak<RosterInner>,
    key: u64,
}

impl ResumeTokenRecord {
    /// Persist the hash for this exact child before registration acknowledges
    /// the token. Leave the in-memory hash unchanged if writing fails.
    pub(crate) fn set_sha256(&self, hash: &str) -> std::io::Result<()> {
        if !crate::live_children::valid_resume_token_sha256(hash) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "resume token hash is not a lowercase hex SHA-256 digest",
            ));
        }
        let inner = self.inner.upgrade().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "child roster no longer live")
        })?;
        inner.without_stalling_runtime(|| {
            let _write = lock(&inner.record_write);
            let mut snapshot = lock(&inner.live).clone();
            let entry = snapshot.get_mut(&self.key).ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::NotFound, "child no longer rostered")
            })?;
            entry.resume_token_sha256 = Some(hash.to_owned());
            inner.write_record(&snapshot)?;
            // Removing the entry also takes the write gate.
            lock(&inner.live)
                .get_mut(&self.key)
                .expect("child stays rostered until the write gate is released")
                .resume_token_sha256 = Some(hash.to_owned());
            Ok(())
        })
    }
}

impl RosterGuard {
    /// Remove a cancelled, proven-never-executed attempt only after the new
    /// record is durable. An error requires quarantining the recovery store.
    #[cfg(unix)]
    pub(crate) fn release_durably(self) -> std::io::Result<()> {
        self.inner.without_stalling_runtime(|| {
            let _write = lock(&self.inner.record_write);
            let snapshot = {
                let mut live = lock(&self.inner.live);
                live.remove(&self.key);
                live.clone()
            };
            self.inner.write_record(&snapshot)
        })
    }
    #[cfg(unix)]
    pub(crate) fn retain(self, reason: crate::live_children::RetainedReason) {
        self.retain_with_identity(reason, None);
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn retain_cgroup(
        self,
        identity: crate::live_children::ContainerIdentity,
        reason: crate::live_children::RetainedReason,
    ) {
        self.retain_with_identity(reason, Some(identity));
    }

    #[cfg(unix)]
    fn retain_with_identity(
        self,
        reason: crate::live_children::RetainedReason,
        identity: Option<crate::live_children::ContainerIdentity>,
    ) {
        self.inner.without_stalling_runtime(|| {
            let _write = lock(&self.inner.record_write);
            let Some(entry) = lock(&self.inner.live).remove(&self.key) else {
                return;
            };
            let mut recorded = entry.to_recorded_child();
            recorded.child.container = recorded.child.container.or(identity);
            if reason.blocks_fresh_copy() {
                tracing::error!(module_id = %recorded.child.module_id, container = ?recorded.child.container,
                    reason = %reason.waiting_message(recorded.child.pid),
                    "container survivors block a fresh instance; retaining the container record");
            }
            lock(&self.inner.retained).push(RetainedEntry {
                recorded,
                reason: Some(reason),
                protection: None,
            });
            let live = lock(&self.inner.live).clone();
            let _ = self.inner.write_record(&live);
        });
        self.inner.ensure_rechecks();
    }
}

#[derive(Debug)]
pub(crate) struct AdmissionError {
    source: std::io::Error,
    guard: RosterGuard,
}

impl AdmissionError {
    /// Transfer ownership to cleanup that may have to retain an unresolved
    /// container. Dropping the error itself would discard that recovery guard.
    pub(crate) fn into_parts(self) -> (std::io::Error, RosterGuard) {
        (self.source, self.guard)
    }

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
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        if let Some(recovery) = self.adoption.take().filter(|recovery| !recovery.settled()) {
            recovery.block();
            self.retain_held_adoption();
            recovery.observe();
            return;
        }
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

#[cfg(unix)]
impl RosterGuard {
    /// Publish the committed executable after exec. A carried child is recorded
    /// before its exec (it waits in the admission barrier), so that early record
    /// must not persist the daemon's pre-exec image, nor the macOS privacy
    /// trampoline before it has replaced itself with the configured module.
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
    #[cfg(target_os = "linux")]
    pub(crate) fn set_cgroup_placement(&self, placement: Option<subc_cgroup::Placement>) {
        *lock(&self.inner.cgroup_placement) = placement;
    }

    pub(crate) fn retained_view(&self) -> RetainedView {
        RetainedView(Arc::downgrade(&self.inner))
    }
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
            containment: Arc::default(),
            counters: self.counters.clone(),
        }
    }

    /// True once daemon shutdown has begun. A spawn after this point would
    /// create a child the shutdown stop may already have finished looking for,
    /// so spawning refuses instead (the supervisor would otherwise restart each
    /// module as it exits on EOF).
    pub(crate) fn is_closed(&self) -> bool {
        self.inner.closed.is_set()
    }

    pub(crate) async fn closed(&self) {
        loop {
            let changed = self.inner.closed_notify.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.is_closed() {
                return;
            }
            changed.await;
        }
    }

    /// The flag [`Self::close`] sets, for the terminal rings to read.
    pub(crate) fn shutdown_flag(&self) -> DaemonShutdownFlag {
        self.inner.closed.clone()
    }

    /// Keep the live-children record at `path` from now on. Set once, before
    /// anything is admitted; a second call is ignored.
    pub(crate) fn record_to(&self, path: PathBuf) {
        self.record_to_with_adopted(path, &AdoptedPids::none());
    }

    /// Seed the record after startup's sweep. Protected entries stay in the
    /// record until the supervisor takes ownership or confirms a Held exit.
    pub(crate) fn record_to_with_adopted(&self, path: PathBuf, protected: &AdoptedPids) {
        if self.inner.record_path.set(path.clone()).is_err() {
            return;
        }
        let (retained, reasons) = match crate::live_children::read_record_entries(&path) {
            Ok(record) => record,
            Err(error) => {
                if let Err(quarantine_error) =
                    crate::live_children::quarantine_record(&path, &error)
                {
                    self.inner
                        .unquarantined_record
                        .store(true, Ordering::Release);
                    tracing::error!(path = %path.display(), %error, %quarantine_error,
                        "cannot quarantine recovery identity; refusing to overwrite the evidence");
                }
                let _ = self.inner.recovery_block.set(format!(
                    "unknown previous container identity: unusable recovery record {}: {error}; offline clearance is required",
                    path.display()
                ));
                Default::default()
            }
        };
        if let Some(reason) = crate::live_children::quarantine_block(&path) {
            tracing::error!(%reason, "carried modules blocked by quarantined recovery identity");
            let _ = self.inner.recovery_block.set(reason);
        }
        {
            *lock(&self.inner.retained) = retained
                .into_iter()
                .map(|recorded| {
                    let protection = protected.protection(recorded.child.pid);
                    let reason = reasons
                        .get(&recorded.child.pid)
                        .cloned()
                        .or_else(|| protection.is_none().then_some(RetainedReason::Hidden));
                    RetainedEntry {
                        recorded,
                        reason,
                        protection,
                    }
                })
                .collect();
        }
        self.inner.ensure_rechecks();
    }

    pub(crate) fn recovery_block(&self) -> Option<&str> {
        self.inner.recovery_block.get().map(String::as_str)
    }

    pub(crate) fn previous_instance_pid(&self, module_id: &str) -> Option<u32> {
        self.previous_instance(module_id)
            .map(|(pid, _)| pid)
            .filter(|pid| *pid > 0)
    }

    pub(crate) fn previous_instance(
        &self,
        module_id: &str,
    ) -> Option<(u32, crate::live_children::RetainedReason)> {
        if let Some((pid, reason)) = lock(&self.inner.pending_admissions).get(module_id) {
            return Some((
                *pid,
                crate::live_children::RetainedReason::ContainerUnverifiable(reason.clone()),
            ));
        }
        // A different module's stalled write cannot change an absent retained
        // identity into this module's previous instance. Avoid the write gate
        // until there is actually retained state for this module to recheck.
        if !lock(&self.inner.retained)
            .iter()
            .any(|entry| entry.recorded.child.module_id == module_id)
        {
            return None;
        }
        self.inner.recheck_retained();
        lock(&self.inner.retained)
            .iter()
            .find(|entry| {
                entry.recorded.child.module_id == module_id
                    && entry.waiting_reason().blocks_fresh_copy()
            })
            .map(|entry| (entry.recorded.child.pid, entry.waiting_reason().clone()))
    }

    pub(crate) fn admission_pending(&self, module_id: &str) -> bool {
        lock(&self.inner.pending_admissions).contains_key(module_id)
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn has_retained_cgroup(&self, module_id: &str) -> bool {
        lock(&self.inner.retained).iter().any(|entry| {
            entry.recorded.child.module_id == module_id
                && entry.waiting_reason().blocks_fresh_copy()
                && entry
                    .recorded
                    .child
                    .container
                    .as_ref()
                    .is_some_and(|identity| identity.cgroup_path.is_some())
        })
    }

    #[cfg(all(test, unix))]
    pub(crate) fn set_admission_timeout_for_test(&self, timeout: Duration) {
        *lock(&self.inner.admission_timeout) = Some(timeout);
    }

    #[cfg(all(test, unix))]
    pub(crate) fn admission_timeout_for_test(&self) -> Option<Duration> {
        *lock(&self.inner.admission_timeout)
    }

    #[cfg(all(test, unix))]
    pub(crate) fn set_admission_hook_for_test(
        &self,
        hook: impl Fn(&str, &'static str, Option<u32>) -> bool + Send + Sync + 'static,
    ) {
        *lock(&self.inner.admission_hook) = Some(AdmissionHook(Arc::new(hook)));
    }

    #[cfg(all(test, unix))]
    pub(crate) fn admission_event_for_test(
        &self,
        module: &str,
        stage: &'static str,
        pid: Option<u32>,
    ) -> bool {
        let hook = lock(&self.inner.admission_hook).clone();
        hook.is_none_or(|hook| (hook.0)(module, stage, pid))
    }

    #[cfg(all(test, unix))]
    pub(crate) fn fail_adoption_for_test(&self) {
        self.inner.fail_adoption.store(true, Ordering::Release);
    }

    #[cfg(all(test, unix))]
    pub(crate) fn set_placement_pause_for_test(
        &self,
        module: &str,
        pause: subc_os::admission_gate::PlacementPause,
    ) {
        lock(&self.inner.placement_pause).insert(module.into(), pause);
    }

    #[cfg(all(test, unix))]
    pub(crate) fn take_placement_pause_for_test(
        &self,
        module: &str,
    ) -> Option<subc_os::admission_gate::PlacementPause> {
        lock(&self.inner.placement_pause).remove(module)
    }

    #[cfg(test)]
    pub(crate) fn take_adoption_failure_for_test(&self) -> bool {
        self.inner.fail_adoption.swap(false, Ordering::AcqRel)
    }

    #[cfg(unix)]
    pub(crate) fn flush_admissions(&self) -> std::io::Result<()> {
        self.inner.without_stalling_runtime(|| {
            let _write = lock(&self.inner.record_write);
            let snapshot = lock(&self.inner.live).clone();
            self.inner.write_record(&snapshot)
        })
    }

    #[cfg(unix)]
    pub(crate) fn begin_pending_admission(&self, module_id: &str, pid: u32) {
        lock(&self.inner.pending_admissions).insert(module_id.into(), (pid,
            "carried admission cleanup pending; the child and durable record must be resolved before another start".into()));
    }

    #[cfg(unix)]
    pub(crate) fn finish_pending_admission(&self, module_id: &str) {
        lock(&self.inner.pending_admissions).remove(module_id);
    }

    #[cfg(unix)]
    pub(crate) fn quarantine_failed_admission(&self, module_id: &str, source: &std::io::Error) {
        let reason = format!(
            "failed late admission write for {module_id}: {source}; offline clearance is required"
        );
        let _ = self.inner.recovery_block.set(reason.clone());
        tracing::error!(module_id, %source, "failed late admission write; quarantining the recovery store");
        self.inner.without_stalling_runtime(|| {
            let _write = lock(&self.inner.record_write);
            if let Some(path) = self.inner.record_path.get() {
                if let Err(error) = crate::live_children::quarantine_failed_admission(path, &reason, source) {
                    self.inner.unquarantined_record.store(true, Ordering::Release);
                    tracing::error!(%error, %reason, "cannot persist admission failure quarantine; preserving recovery evidence");
                }
            }
        });
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
                #[cfg(any(target_os = "linux", target_os = "macos"))]
                adopted_signal: None,
                module_id,
                pid,
                protocol,
                start_time,
                recorded,
                resume_token_sha256: None,
                drain_budget: Arc::clone(&self.drain_budget),
            },
        );
        let guard = RosterGuard {
            inner: Arc::clone(&self.inner),
            key,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            adoption: None,
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

    /// Transfer a verified child staged by `record_to_with_adopted` into the
    /// live roster. The disk record already includes the child and its hash;
    /// only its in-memory owner changes here.
    // The startup adoption path calls this after verifying the child's identity.
    #[allow(dead_code)]
    pub(crate) fn admit_adopted(
        &self,
        verified: &RecordedChild,
        mut identity: RecordedIdentity,
    ) -> std::io::Result<RosterGuard> {
        self.inner.without_stalling_runtime(|| {
            let _write = lock(&self.inner.record_write);
            let recorded = {
                let mut retained = lock(&self.inner.retained);
                let mut matching = retained
                    .iter()
                    .enumerate()
                    .filter(|(_, entry)| entry.recorded.child.pid == verified.child.pid);
                match (matching.next(), matching.next()) {
                    (Some((index, entry)), None)
                        if entry.protection == Some(ProtectedChild::Adopted)
                            && verified.resume_token_sha256.is_some()
                            && &entry.recorded == verified =>
                    {
                        retained.remove(index).recorded
                    }
                    _ => {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::NotFound,
                            "verified child is not staged for adoption",
                        ));
                    }
                }
            };
            let child = recorded.child;
            identity.start_time = child.start_time;
            identity.executable = child.executable;
            identity.cgroup_name = child.cgroup_name;
            identity.container = child.container;
            let key = self.inner.next_key.fetch_add(1, Ordering::Relaxed);
            lock(&self.inner.live).insert(
                key,
                RosterEntry {
                    #[cfg(any(target_os = "linux", target_os = "macos"))]
                    adopted_signal: None,
                    module_id: child.module_id,
                    pid: child.pid,
                    protocol: child.protocol,
                    start_time: child.start_time,
                    recorded: identity,
                    resume_token_sha256: recorded.resume_token_sha256,
                    drain_budget: Arc::clone(&self.drain_budget),
                },
            );
            Ok(RosterGuard {
                inner: Arc::clone(&self.inner),
                key,
                #[cfg(any(target_os = "linux", target_os = "macos"))]
                adoption: None,
            })
        })
    }

    /// Remove only the protected row whose exact pid and start time the
    /// supervisor confirmed and reaped as its own child.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn remove_reaped_adoption(
        &self,
        module: &str,
        pid: u32,
        start_time: u64,
    ) -> std::io::Result<bool> {
        self.inner.without_stalling_runtime(|| {
            let _write = lock(&self.inner.record_write);
            let removed = {
                let mut retained = lock(&self.inner.retained);
                let before = retained.len();
                retained.retain(|entry| {
                    let child = &entry.recorded.child;
                    !(entry.protection.is_some()
                        && child.pid == pid
                        && child.module_id == module
                        && child.start_time == Some(start_time))
                });
                retained.len() != before
            };
            if removed {
                let live = lock(&self.inner.live).clone();
                self.inner.write_record(&live)?;
            }
            Ok(removed)
        })
    }

    #[cfg(all(test, unix))]
    pub(crate) fn live_count_for_test(&self) -> usize {
        self.live().len()
    }

    #[cfg(all(test, unix))]
    pub(crate) fn plain_group_for_test(
        &self,
        pid: u32,
    ) -> Option<Arc<crate::supervise::plain_group::PlainGroup>> {
        lock(&self.inner.live)
            .values()
            .find(|entry| entry.pid == pid)?
            .recorded
            .plain_group
            .as_ref()?
            .upgrade()
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
        self.inner.closed_notify.notify_waiters();
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod resume_hash_tests {
    use super::*;
    use crate::live_children::{
        fail_next_file_sync, read_record_entries, write_record_with_reasons,
    };
    use cortexkit_test_support::ScratchDir;
    use std::{collections::BTreeMap, fs, path::Path};

    fn admit(roster: &ChildRoster, module: &str, pid: u32) -> RosterGuard {
        roster
            .admit(
                module.into(),
                pid,
                ModuleProtocol::Subc,
                Some(1000),
                RecordedIdentity {
                    start_time: Some(1000),
                    ..RecordedIdentity::default()
                },
            )
            .unwrap()
    }

    fn record(module: &str, pid: u32, hash: Option<&str>) -> RecordedChild {
        RecordedChild {
            child: LiveChild {
                module_id: module.into(),
                pid,
                protocol: ModuleProtocol::Subc,
                start_time: Some(1000),
                executable: None,
                cgroup_name: None,
                container: None,
            },
            resume_token_sha256: hash.map(str::to_owned),
        }
    }

    fn entries(path: &Path) -> Vec<RecordedChild> {
        read_record_entries(path).unwrap().0
    }

    #[test]
    fn module_a_hash_survives_module_b_admit_and_reap() {
        let dir = ScratchDir::new("roster-hash-admit-reap");
        let path = dir.join("live-children.json");
        let roster = ChildRoster::default();
        roster.record_to(path.clone());
        let hash = "a".repeat(64);
        let a = admit(&roster, "a", 41);
        assert_eq!(entries(&path), [record("a", 41, None)]);
        let bytes = fs::read_to_string(&path).unwrap();
        assert!(!bytes.contains("resume_token_sha256"));
        a.set_resume_token_sha256(&hash).unwrap();
        let expected = record("a", 41, Some(&hash));
        assert_eq!(entries(&path), std::slice::from_ref(&expected));

        let b = admit(&roster, "b", 42);
        assert_eq!(entries(&path), [expected.clone(), record("b", 42, None)]);
        drop(b);
        assert_eq!(entries(&path), [expected]);
        drop(a);
        assert!(entries(&path).is_empty());
    }

    #[test]
    fn failed_hash_write_leaves_previous_hash_for_later_rewrites() {
        let dir = ScratchDir::new("roster-hash-write-failure");
        let path = dir.join("live-children.json");
        let roster = ChildRoster::default();
        roster.record_to(path.clone());
        let a = admit(&roster, "a", 41);
        let hash = "a".repeat(64);
        a.set_resume_token_sha256(&hash).unwrap();
        fail_next_file_sync();
        assert!(a.set_resume_token_sha256(&"b".repeat(64)).is_err());
        let expected = record("a", 41, Some(&hash));
        assert_eq!(entries(&path), std::slice::from_ref(&expected));
        drop(admit(&roster, "b", 42));
        assert_eq!(entries(&path), [expected]);
    }

    #[cfg(unix)]
    #[test]
    fn retained_guard_keeps_child_hash() {
        let dir = ScratchDir::new("roster-retained-hash");
        let path = dir.join("live-children.json");
        let roster = ChildRoster::default();
        roster.record_to(path.clone());
        let a = admit(&roster, "a", 41);
        let hash = "a".repeat(64);
        a.set_resume_token_sha256(&hash).unwrap();
        a.retain(RetainedReason::Unverifiable);
        drop(admit(&roster, "b", 42));
        assert_eq!(entries(&path), [record("a", 41, Some(&hash))]);
    }

    #[cfg(unix)]
    #[test]
    fn held_is_exempt_from_cold_rechecks_and_survives_admit_and_reap() {
        let dir = ScratchDir::new("roster-held-hash");
        let path = dir.join("live-children.json");
        let held_pid = i32::MAX as u32;
        let cold_pid = held_pid - 1;
        let held = record("held", held_pid, Some(&"b".repeat(64)));
        let cold = record("cold", cold_pid, None);
        let reasons = BTreeMap::from([(held_pid, RetainedReason::EmptyContainerDirectory)]);
        write_record_with_reasons(&path, &[held.clone(), cold], reasons.clone()).unwrap();
        let roster = ChildRoster::default();
        roster.record_to_with_adopted(path.clone(), &AdoptedPids::from_classes([], [held_pid]));

        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            let start = held.child.start_time.expect("test child has a start time");
            assert!(!roster
                .remove_reaped_adoption("held", cold_pid, start)
                .unwrap());
            assert!(!roster
                .remove_reaped_adoption("held", held_pid, start + 1)
                .unwrap());
        }

        // Both PIDs are outside the OS's allocation range. Cold recovery can
        // drop its entry when the PID doesn't exist, but must not touch Held.
        roster.inner.recheck_retained_now();
        assert_eq!(
            read_record_entries(&path).unwrap(),
            (vec![held.clone()], reasons.clone())
        );
        assert_eq!(roster.previous_instance_pid("held"), Some(held_pid));
        let b = admit(&roster, "b", 42);
        assert_eq!(entries(&path), [record("b", 42, None), held.clone()]);
        drop(b);
        assert_eq!(read_record_entries(&path).unwrap(), (vec![held], reasons));
    }

    #[test]
    fn adopt_moves_the_verified_entry_and_hash_into_the_live_roster_once() {
        let dir = ScratchDir::new("roster-adopted-hash");
        let path = dir.join("live-children.json");
        let adopted = record("adopted", 41, Some(&"a".repeat(64)));
        let unhashed = record("unhashed", 42, None);
        let held = record("held", 43, Some(&"c".repeat(64)));
        let initial = [adopted.clone(), unhashed.clone(), held.clone()];
        write_record_with_reasons(&path, &initial, BTreeMap::new()).unwrap();
        let roster = ChildRoster::default();
        roster.record_to_with_adopted(path.clone(), &AdoptedPids::from_classes([41, 42], [43]));
        let bytes = fs::read(&path).unwrap();

        let mut mismatched = adopted.clone();
        mismatched.child.start_time = Some(2000);
        for rejected in [&mismatched, &unhashed, &held] {
            assert!(roster
                .admit_adopted(rejected, RecordedIdentity::default())
                .is_err());
        }
        let guard = roster
            .admit_adopted(&adopted, RecordedIdentity::default())
            .unwrap();
        assert!(roster
            .admit_adopted(&adopted, RecordedIdentity::default())
            .is_err());
        assert_eq!(
            fs::read(&path).unwrap(),
            bytes,
            "adoption needs no record rewrite"
        );
        drop(admit(&roster, "other", 44));
        assert_eq!(entries(&path), initial);
        drop(guard);
        assert_eq!(entries(&path), [unhashed, held]);
    }
}

#[cfg(unix)]
pub(crate) use unix_shutdown::{end_children_for_daemon_shutdown, TERM_TO_KILL};

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
    use crate::supervise::plain_group::TERM_GRACE;
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
    /// cap, group TERM grace and pipe drain, and [`CHILD_REAP_BOUND`] after it
    /// come to less than 32 s,
    /// under the 35 s `ExitTimeOut` / `TimeoutStopSec` that `ck setup` writes
    /// (see `desired_definition`). 25 s covers BROCA's teardown (10 s run grace,
    /// then a seal, inside a 20 s budget; 12 s measured) with room to spare.
    const CHILD_SHUTDOWN_CAP: Duration = Duration::from_secs(25);
    /// For a subc module still running at its deadline: the time between the
    /// SIGTERM sent then and the SIGKILL. Long enough for a handler to write a
    /// last line and exit.
    pub(crate) const TERM_TO_KILL: Duration = Duration::from_millis(500);
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
    /// module still running gets SIGTERM and then SIGKILL after [`TERM_TO_KILL`],
    /// whether the target is an ordinary group or an individual child.
    /// A `protocol: "none"` child has already been asked. All children are
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
                let term_sent = termed.get(key).copied();
                let due =
                    if !escalated && entry.recorded.plain_group.is_some() && term_sent.is_some() {
                        let grace = match entry.protocol {
                            ModuleProtocol::None => budget(entry).min(TERM_GRACE),
                            ModuleProtocol::Subc => TERM_TO_KILL,
                        };
                        term_sent
                            .filter(|at| now >= *at + grace)
                            .map(|_| Signal::KILL)
                    } else {
                        shutdown_signal(entry.protocol, now, deadline, term_sent, escalated)
                    };
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
                    if entry.adopted_signal.is_none()
                        && entry.start_time.is_none_or(|expected| {
                            crate::provenance::process_start_time(entry.pid) == Some(expected)
                        })
                    {
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
            let reap_bound = CHILD_REAP_BOUND
                + if live
                    .iter()
                    .any(|(_, entry)| entry.recorded.plain_group.is_some())
                {
                    crate::child_output::PLAIN_PIPE_DRAIN
                } else {
                    Duration::ZERO
                };
            if live.iter().all(|(key, _)| killed.contains(key))
                && last_kill.is_some_and(|at| now >= at + reap_bound)
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
        let roster = ChildRoster::default();
        roster.record_to(record.clone());
        // Startup quarantine can move an unusable path aside. Make the
        // destination a directory afterwards so admission's file write fails,
        // preserving the ordinary child's ownership through kill and reap.
        std::fs::create_dir(&record).unwrap();
        assert!(roster.recovery_block().is_none());
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
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        if let Some(adopted) = &entry.adopted_signal {
            if let Some(adopted) = adopted.upgrade() {
                if let Err(error) = adopted.signal(signal) {
                    debug!(module_id = %entry.module_id, %error, "verified shutdown signal was not delivered");
                }
            }
            return;
        }
        if let Some(group) = &entry.recorded.plain_group {
            if let Some(group) = group.upgrade() {
                if let Err(error) = group.signal(signal) {
                    debug!(module_id = %entry.module_id, %error, "shutdown group signal was not delivered");
                }
            }
            return;
        }
        if let Some(carried) = &entry.recorded.carried {
            if let Some(container) = carried.upgrade() {
                if signal == Signal::KILL {
                    container.hard_kill();
                }
            }
            return;
        }
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
        let Some(pid) = i32::try_from(entry.pid)
            .ok()
            .filter(|pid| *pid > 1)
            .and_then(Pid::from_raw)
        else {
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

#[cfg(all(test, unix))]
mod retained_container_tests {
    use super::*;
    use crate::live_children::{ContainerIdentity, RetainedReason};
    use cortexkit_test_support::ScratchDir;
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command};

    struct OwnedChild(Child);
    impl OwnedChild {
        fn spawn(group: u32) -> Self {
            Self(
                Command::new("/bin/sleep")
                    .arg("600")
                    .process_group(group as i32)
                    .spawn()
                    .unwrap(),
            )
        }
        fn id(&self) -> u32 {
            self.0.id()
        }
    }
    impl Drop for OwnedChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn fixture(label: &str) -> (ScratchDir, ChildRoster, OwnedChild, u32) {
        let dir = ScratchDir::new(label);
        let roster = ChildRoster::default();
        roster.record_to(dir.join("children.json"));
        let mut first = OwnedChild::spawn(0);
        let identity = subc_os::process_group::inspect(first.id())
            .unwrap()
            .unwrap();
        let holder = OwnedChild::spawn(first.id());
        let guard = roster
            .admit(
                label.into(),
                first.id(),
                ModuleProtocol::None,
                Some(identity.start_time),
                RecordedIdentity {
                    container: Some(ContainerIdentity {
                        pgid: Some(first.id()),
                        created_start_time: Some(identity.start_time),
                        cgroup_path: None,
                        ..ContainerIdentity::test_metadata()
                    }),
                    ..RecordedIdentity::default()
                },
            )
            .unwrap();
        first.0.kill().unwrap();
        first.0.wait().unwrap();
        guard.retain(RetainedReason::ContainerSurvivors(vec![holder.id()]));
        (dir, roster, holder, first.id())
    }

    #[test]
    fn retained_container_survivor_blocks_until_its_death() {
        let label = "retained-container-live";
        let (dir, roster, mut holder, pgid) = fixture(label);
        let recorded = crate::live_children::read_record(&dir.join("children.json")).unwrap();
        assert_eq!(
            recorded.len(),
            1,
            "the dead leader's container record must survive guard release"
        );
        assert_eq!(recorded[0].container.as_ref().unwrap().pgid, Some(pgid));
        let (pid, reason) = roster
            .previous_instance(label)
            .expect("a live container member blocks a fresh copy");
        assert_eq!(pid, pgid);
        assert_eq!(
            reason,
            RetainedReason::ContainerSurvivors(vec![holder.id()])
        );
        let detail = reason.waiting_message(pid);
        assert!(
            detail.contains("container survivors") && detail.contains(&holder.id().to_string()),
            "{detail}"
        );
        // The recheck queued SIGKILL after verifying this owned child. Confirm
        // its death before checking that the roster no longer blocks a fresh copy.
        holder.0.wait().unwrap();
        assert_eq!(roster.previous_instance(label), None);
        assert!(
            crate::live_children::read_record(&dir.join("children.json"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn retained_container_zombie_does_not_block_a_fresh_copy() {
        let label = "retained-container-zombie";
        let (_dir, roster, mut holder, _) = fixture(label);
        holder.0.kill().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while subc_os::child_exit::observe(holder.id()).unwrap().is_none() {
            assert!(
                std::time::Instant::now() < deadline,
                "owned child must reach zombie state"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(subc_os::process_group::inspect(holder.id())
            .unwrap()
            .is_none_or(|facts| facts.exited));
        assert_eq!(
            roster.previous_instance(label),
            None,
            "an exited unreaped helper has no live resources"
        );
    }
}
