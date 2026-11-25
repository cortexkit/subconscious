use rustix::process::{kill_process_group, Pid, Signal};
use std::{io, process::ExitStatus, sync::Mutex, time::Duration};
use tokio::time::{sleep_until, Instant};

pub(crate) const TERM_GRACE: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(50);
const EMPTY_GRACE: Duration = Duration::from_secs(2);

trait GroupOperations {
    fn observe(&self, leader: u32) -> io::Result<Option<ExitStatus>>;
    fn signal(&self, group: Pid, signal: Signal) -> io::Result<()>;
    fn has_live_members(&self, group: u32) -> io::Result<bool>;
}

struct Unix;

impl GroupOperations for Unix {
    fn observe(&self, leader: u32) -> io::Result<Option<ExitStatus>> {
        subc_os::child_exit::observe(leader)
    }

    fn signal(&self, group: Pid, signal: Signal) -> io::Result<()> {
        match kill_process_group(group, signal) {
            Ok(()) | Err(rustix::io::Errno::SRCH) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    fn has_live_members(&self, group: u32) -> io::Result<bool> {
        use subc_os::process_group;
        // Signal zero also sees the unreaped leader. Inspect membership so a
        // group containing only zombies does not consume the full grace.
        for pid in process_group::pids()? {
            if process_group::pgid(pid)? == Some(group)
                && process_group::inspect(pid)?
                    .is_some_and(|member| member.pgid == group && !member.exited)
            {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// Finish cleanup while the daemon still owns the exited, unreaped leader.
/// A group-wide signal is permitted only in this state: after the leader is
/// reaped, its numeric group id can be reused by an unrelated process.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(super) fn settle_pinned_exited_group(leader: u32, may_signal: bool) -> io::Result<()> {
    let work = || settle_pinned_exited_group_inline(leader, may_signal);
    let multi_threaded = tokio::runtime::Handle::try_current()
        .is_ok_and(|handle| handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread);
    if multi_threaded {
        // The complete scan can wait on another process's parent reaping a
        // zombie. Hand this worker's async tasks to another worker while that
        // wait holds the child's reap lock. A current-thread runtime has no
        // such handoff, so the bounded scan runs inline there.
        tokio::task::block_in_place(work)
    } else {
        work()
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn settle_pinned_exited_group_inline(leader: u32, may_signal: bool) -> io::Result<()> {
    let group = i32::try_from(leader)
        .ok()
        .and_then(Pid::from_raw)
        .ok_or_else(|| io::Error::other("invalid adopted process group"))?;
    leader_is_held(leader)?;
    if may_signal {
        match Unix.signal(group, Signal::KILL) {
            #[cfg(target_os = "macos")]
            Err(error)
                if error.raw_os_error() == Some(rustix::io::Errno::PERM.raw_os_error())
                    && matches!(Unix.has_live_members(leader), Ok(false)) =>
            {
                Ok(())
            }
            result => result,
        }?;
    }
    // A replacement must not launch beside a surviving descendant. The full
    // scan runs only while the unreaped leader reserves the group id, and only
    // the leader's own zombie is ignored. A zombie cannot fork, so no new
    // descendant of the old module can appear after an empty scan. An unrelated
    // process in the daemon's session that joined before the group stop shares
    // that stop; one that joins after the scan is not the old module's tree and
    // is never signalled after the leader is reaped. Any unreadable entry makes
    // the scan inconclusive, never proof that the group is empty.
    let deadline = std::time::Instant::now() + EMPTY_GRACE;
    let reason = loop {
        leader_is_held(leader)?;
        let reason = match subc_os::process_group::members(leader) {
            Ok(members) if members.iter().all(|member| *member == leader) => {
                leader_is_held(leader)?;
                return Ok(());
            }
            Ok(members) => {
                let survivors = members.iter().filter(|member| **member != leader).count();
                io::Error::new(
                    io::ErrorKind::WouldBlock,
                    format!("process group {leader} still contains {survivors} other process(es)"),
                )
            }
            Err(error) => error,
        };
        let now = std::time::Instant::now();
        if now >= deadline {
            break reason;
        }
        std::thread::sleep(POLL.min(deadline.saturating_duration_since(now)));
    };
    Err(io::Error::new(
        reason.kind(),
        format!("process group {leader} was not proven empty: {reason}"),
    ))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn leader_is_held(leader: u32) -> io::Result<()> {
    // macOS does not expose a zombie's process-group ID, but waitid with
    // WNOWAIT observes exit without reaping, proving that this daemon still
    // owns the leader pinning the group.
    if matches!(subc_os::child_exit::observe(leader), Ok(Some(_))) {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "process-group leader {leader} is not held as an exited, unreaped child"
        )))
    }
}

#[derive(Debug, Default)]
struct Ownership {
    term_sent: Option<Instant>,
    killed: bool,
    closed: bool,
}

/// The child handle keeps the spawn's leader unreaped. The shutdown roster may
/// concurrently request a signal, but cannot use the identity during or after
/// reaping: closure and every check-and-signal share this lock.
#[derive(Debug)]
pub(crate) struct PlainGroup {
    leader: u32,
    group: Pid,
    ownership: Mutex<Ownership>,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    adopted: Option<std::sync::Weak<super::live_upgrade::VerifiedChild>>,
}

impl PlainGroup {
    /// Only called with the pid returned by spawning a child with setpgid(0, 0).
    pub(super) fn from_spawn(leader: u32) -> Option<Self> {
        let group = i32::try_from(leader)
            .ok()
            .filter(|pid| *pid > 1)
            .and_then(Pid::from_raw)?;
        Some(Self {
            leader,
            group,
            ownership: Mutex::default(),
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            adopted: None,
        })
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(super) fn from_adopted(
        leader: u32,
        verified: &std::sync::Arc<super::live_upgrade::VerifiedChild>,
    ) -> Option<Self> {
        let mut group = Self::from_spawn(leader)?;
        group.adopted = Some(std::sync::Arc::downgrade(verified));
        Some(group)
    }

    pub(super) fn observe(&self) -> io::Result<Option<ExitStatus>> {
        Unix.observe(self.leader)
    }

    pub(super) async fn wait_for_exit(&self) -> io::Result<ExitStatus> {
        use tokio::signal::unix::{signal, SignalKind};

        // Subscribe before checking for exit. SIGCHLD can coalesce or belong
        // to a different child, so only observing this leader ends the wait.
        // Keep it unreaped so it can anchor group teardown after observation.
        let mut child_events = signal(SignalKind::child())?;
        loop {
            if let Some(status) = self.observe()? {
                return Ok(status);
            }
            child_events
                .recv()
                .await
                .ok_or_else(|| io::Error::other("child exit notifications closed"))?;
        }
    }

    pub(crate) fn signal(&self, signal: Signal) -> io::Result<()> {
        self.signal_with(signal, &Unix)
    }

    fn signal_with(&self, signal: Signal, ops: &impl GroupOperations) -> io::Result<()> {
        let mut owned = self.ownership.lock().unwrap_or_else(|p| p.into_inner());
        if owned.closed || owned.killed || (signal == Signal::TERM && owned.term_sent.is_some()) {
            return Ok(());
        }
        // Check that the child still belongs to us without reaping it. A
        // failed check never authorizes a signal to the numeric group.
        let ordinary_signal = || {
            ops.observe(self.leader)?;
            ops.signal(self.group, signal)
        };
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let sent = if let Some(verified) = &self.adopted {
            verified
                .upgrade()
                .ok_or_else(|| io::Error::other("adopted group lost its owner"))?
                .signal(signal)
        } else {
            ordinary_signal()
        };
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        let sent = ordinary_signal();
        #[cfg(target_os = "macos")]
        let sent = match sent {
            // The macOS kernel skips zombies when signalling a group and
            // returns EPERM if none can be signalled. Recheck after the signal:
            // the leader may have just exited, but it still pins the group.
            Err(error)
                if error.raw_os_error() == Some(rustix::io::Errno::PERM.raw_os_error())
                    && matches!(ops.observe(self.leader), Ok(Some(_)))
                    && matches!(ops.has_live_members(self.leader), Ok(false)) =>
            {
                Ok(())
            }
            sent => sent,
        };
        if signal == Signal::TERM {
            owned.term_sent = Some(Instant::now());
        } else if signal == Signal::KILL {
            owned.killed = sent.is_ok();
        }
        sent
    }

    pub(super) async fn settle(&self) {
        self.settle_with(&Unix).await;
    }

    #[cfg(test)]
    pub(super) fn remaining_term_grace_for_test(&self) -> Option<Duration> {
        let owned = self.ownership.lock().unwrap_or_else(|p| p.into_inner());
        let sent = owned.term_sent?;
        Some(if owned.killed || owned.closed {
            Duration::ZERO
        } else {
            (sent + TERM_GRACE).saturating_duration_since(Instant::now())
        })
    }

    async fn settle_with(&self, ops: &impl GroupOperations) {
        let fallback = Instant::now();
        if let Err(error) = self.signal_with(Signal::TERM, ops) {
            tracing::debug!(pid = self.leader, %error, "plain group TERM was not delivered");
        }
        loop {
            let deadline = {
                let owned = self.ownership.lock().unwrap_or_else(|p| p.into_inner());
                if owned.closed || owned.killed {
                    return;
                }
                owned.term_sent.unwrap_or(fallback) + TERM_GRACE
            };
            let now = Instant::now();
            if now >= deadline || matches!(ops.has_live_members(self.leader), Ok(false)) {
                break;
            }
            sleep_until((now + POLL).min(deadline)).await;
        }
        // Even an empty sample can race a member forking while we enumerate.
        // This is the last chance to reach it while the leader is still pinned.
        if let Err(error) = self.signal_with(Signal::KILL, ops) {
            tracing::debug!(pid = self.leader, %error, "plain group KILL was not delivered");
        }
    }

    pub(super) fn close(&self) {
        self.ownership
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .closed = true;
    }
}

impl super::SupervisedChild {
    pub(super) async fn drain_plain_capture(&mut self, module_id: &str) {
        use crate::child_output::PLAIN_PIPE_DRAIN;
        if let Err(error) = self.child.reap().await {
            tracing::debug!(module_id, %error, "could not reap plain group leader before capture drain");
        }
        let stdout = self.stdout_pump.take();
        let stderr = self.stderr_pump.take();
        if let Some(pump) = &stderr {
            self.stderr_ring
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .mark_pump_late(
                    pump.generation,
                    "stderr pipe had not reached EOF after group exit",
                );
        }
        tokio::join!(
            async {
                if let Some(stdout) = stdout {
                    super::settle_stdout_pump(module_id, stdout, PLAIN_PIPE_DRAIN).await;
                }
            },
            async {
                if let Some(stderr) = stderr {
                    super::settle_stderr_pump(
                        module_id,
                        &self.stderr_ring,
                        stderr,
                        PLAIN_PIPE_DRAIN,
                    )
                    .await;
                }
            }
        );
        if let Some(stop) = self.capture_stop.take() {
            self.late_output_lost |= stop.had_open_readers();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, os::unix::process::ExitStatusExt};

    #[derive(Debug, PartialEq, Eq)]
    enum Call {
        Observe(u32),
        Signal(Pid, Signal),
    }

    struct Kernel {
        observed: io::Result<Option<ExitStatus>>,
        signal_error: Option<rustix::io::Errno>,
        calls: RefCell<Vec<Call>>,
    }

    impl GroupOperations for Kernel {
        fn observe(&self, leader: u32) -> io::Result<Option<ExitStatus>> {
            self.calls.borrow_mut().push(Call::Observe(leader));
            self.observed
                .as_ref()
                .copied()
                .map_err(|error| io::Error::new(error.kind(), "waitid refused the child"))
        }

        fn signal(&self, group: Pid, signal: Signal) -> io::Result<()> {
            self.calls.borrow_mut().push(Call::Signal(group, signal));
            self.signal_error.map_or(Ok(()), |error| Err(error.into()))
        }

        fn has_live_members(&self, _: u32) -> io::Result<bool> {
            Ok(true)
        }
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn macos_stop_kill_accepts_an_unreaped_zombie_only_group() {
        use super::super::containment::ProcessChild;
        use std::sync::Arc;
        use tokio::{process::Command, time::timeout};

        let process = Command::new("/bin/sh")
            .args(["-c", "exit 23"])
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let pid = process.id().unwrap();
        let mut child = ProcessChild::plain(process, pid);
        child.plain_group = PlainGroup::from_spawn(pid).map(Arc::new);

        // The supervisor's group wait leaves the exited leader unreaped. Escalate
        // while it still pins the group, just as when exit wins the race with KILL.
        let status = timeout(Duration::from_secs(5), child.wait())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(status.code(), Some(23));
        assert_eq!(subc_os::child_exit::observe(pid).unwrap(), Some(status));
        child.start_kill().unwrap();
        child.reap().await.unwrap();
        assert_eq!(
            subc_os::child_exit::observe(pid)
                .unwrap_err()
                .raw_os_error(),
            Some(rustix::io::Errno::CHILD.raw_os_error())
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_stop_kill_preserves_eperm_for_a_live_leader_or_member() {
        for observed in [None, Some(ExitStatus::from_raw(0))] {
            let group = PlainGroup::from_spawn(42).unwrap();
            let kernel = Kernel {
                observed: Ok(observed),
                signal_error: Some(rustix::io::Errno::PERM),
                calls: RefCell::default(),
            };
            let error = group.signal_with(Signal::KILL, &kernel).unwrap_err();
            assert_eq!(
                error.raw_os_error(),
                Some(rustix::io::Errno::PERM.raw_os_error())
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn macos_policy_waits_for_helpers_and_closes_signal_authority_before_reap() {
        let group = PlainGroup::from_spawn(42).unwrap();
        let kernel = Kernel {
            observed: Ok(Some(ExitStatus::from_raw(0))),
            signal_error: None,
            calls: RefCell::default(),
        };
        let start = Instant::now();
        group.settle_with(&kernel).await;
        assert_eq!(start.elapsed(), TERM_GRACE);
        let disposed = PlainGroup::from_spawn(43).unwrap();
        disposed.close();
        disposed.signal_with(Signal::KILL, &kernel).unwrap();
        let id = Pid::from_raw(42).unwrap();
        assert_eq!(
            kernel.calls.into_inner(),
            [
                Call::Observe(42),
                Call::Signal(id, Signal::TERM),
                Call::Observe(42),
                Call::Signal(id, Signal::KILL)
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn macos_policy_never_signals_without_an_owned_leader() {
        assert!(PlainGroup::from_spawn(0).is_none());
        assert!(PlainGroup::from_spawn(1).is_none());
        let group = PlainGroup::from_spawn(42).unwrap();
        let kernel = Kernel {
            observed: Err(io::Error::from(rustix::io::Errno::CHILD)),
            signal_error: None,
            calls: RefCell::default(),
        };
        let start = Instant::now();
        group.settle_with(&kernel).await;
        assert_eq!(start.elapsed(), TERM_GRACE);
        assert_eq!(
            kernel.calls.into_inner(),
            [Call::Observe(42), Call::Observe(42)]
        );
    }
}
