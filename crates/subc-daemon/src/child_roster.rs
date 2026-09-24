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
}

/// Set once, when the daemon begins its announced shutdown, and never cleared.
///
/// Shared by the roster (which refuses spawns once it is set) and every
/// module's terminal ring (which records any exit after it as
/// `daemon_shutdown`), so the reap path, the spawn path and the record all
/// read the same flag.
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
}

#[derive(Debug, Default)]
struct RosterInner {
    next_key: AtomicU64,
    closed: DaemonShutdownFlag,
    live: Mutex<HashMap<u64, RosterEntry>>,
    /// Where the live-children record is written; unset means no record, the
    /// default for an in-process daemon (see `BootstrapConfig`).
    record_path: OnceLock<PathBuf>,
}

impl RosterInner {
    /// Rewrite the record from `live`. Called with the `live` lock held, so
    /// concurrent admits and releases write their snapshots in the order they
    /// changed the roster and the last write on disk is the current roster.
    fn write_record(&self, live: &HashMap<u64, RosterEntry>) {
        let Some(path) = self.record_path.get() else {
            return;
        };
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
        if let Err(error) = crate::live_children::write_record(path, &children) {
            warn!(
                path = %path.display(),
                %error,
                "could not rewrite the live-children record; a crash now could leave orphans the next boot cannot find"
            );
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

impl Drop for RosterGuard {
    fn drop(&mut self) {
        let mut live = lock(&self.inner.live);
        if live.remove(&self.key).is_some() {
            self.inner.write_record(&live);
        }
    }
}

impl ChildRoster {
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
        let _ = self.inner.record_path.set(path);
    }

    pub(crate) fn admit(
        &self,
        module_id: String,
        pid: u32,
        protocol: ModuleProtocol,
        start_time: Option<u64>,
        recorded: RecordedIdentity,
    ) -> RosterGuard {
        let key = self.inner.next_key.fetch_add(1, Ordering::Relaxed);
        let mut live = lock(&self.inner.live);
        live.insert(
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
        self.inner.write_record(&live);
        drop(live);
        RosterGuard {
            inner: Arc::clone(&self.inner),
            key,
        }
    }

    #[cfg(unix)]
    fn live(&self) -> Vec<(u64, RosterEntry)> {
        lock(&self.inner.live)
            .iter()
            .map(|(key, entry)| (*key, entry.clone()))
            .collect()
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
    use std::{collections::HashSet, future::Future, time::Duration};

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
        let mut termed = HashSet::new();
        let mut killed = HashSet::new();
        let mut last_kill: Option<Instant> = None;
        let mut escalated = already_escalated;

        if !escalated {
            for (key, entry) in roster.live() {
                if entry.protocol == ModuleProtocol::None {
                    signal(&entry, Signal::TERM);
                    termed.insert(key);
                }
            }
        }

        loop {
            let live = roster.live();
            if live.is_empty() {
                return;
            }
            let now = Instant::now();
            for (key, entry) in &live {
                if killed.contains(key) {
                    continue;
                }
                let deadline = started + budget(entry);
                let kill_at = match entry.protocol {
                    ModuleProtocol::None => deadline,
                    ModuleProtocol::Subc => deadline + TERM_TO_KILL,
                };
                if escalated || now >= kill_at {
                    warn!(
                        module_id = %entry.module_id,
                        pid = entry.pid,
                        escalated,
                        "supervised child did not exit during daemon shutdown; sending SIGKILL"
                    );
                    signal(entry, Signal::KILL);
                    killed.insert(*key);
                    last_kill = Some(now);
                } else if now >= deadline && termed.insert(*key) {
                    warn!(
                        module_id = %entry.module_id,
                        pid = entry.pid,
                        "supervised module still running at its shutdown deadline after EOF; sending SIGTERM"
                    );
                    signal(entry, Signal::TERM);
                }
            }
            // Everything left has been SIGKILLed: wait only for the reaps.
            if live.iter().all(|(key, _)| killed.contains(key))
                && last_kill.is_some_and(|at| now >= at + CHILD_REAP_BOUND)
            {
                return;
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
    }

    fn budget(entry: &RosterEntry) -> Duration {
        (*lock(&entry.drain_budget)).min(CHILD_SHUTDOWN_CAP)
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
