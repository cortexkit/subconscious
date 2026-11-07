//! Recovery when no unreaped child reserves a container's process-group ID.
//! Never signal the group: authorize and recheck each individual member instead.

use crate::live_children::{RetainedReason, SweepBounds, SweepDecision};
use std::io;
use subc_os::{process_group::Identity, Process, Signal};
use tokio::time::{sleep, Duration, Instant};

#[derive(Debug, Clone, Copy)]
pub(crate) struct RecordedGroup<'a> {
    pub(crate) pgid: u32,
    pub(crate) created_start_time: u64,
    pub(crate) boot_id: &'a str,
    pub(crate) session_id: u32,
}

impl RecordedGroup<'_> {
    fn validate(self) -> io::Result<()> {
        if self.pgid <= 1 || i32::try_from(self.pgid).is_err() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid recorded process group",
            ));
        }
        Ok(())
    }

    fn owns(self, identity: Identity) -> bool {
        identity.pgid == self.pgid
            && identity.session_id == self.session_id
            && identity.uid == subc_os::effective_uid()
            && identity.start_time >= self.created_start_time
    }

    fn anchor_matches(self) -> io::Result<bool> {
        self.validate()?;
        if !subc_os::boot_id::is_current(self.boot_id)? {
            return Ok(false);
        }
        Ok(
            subc_os::process_group::inspect(self.pgid)?.is_none_or(|anchor| {
                anchor.start_time == self.created_start_time
                    && anchor.uid == subc_os::effective_uid()
            }),
        )
    }
}

struct Members {
    live: Vec<u32>,
    unknown: Option<String>,
}

fn members(
    record: RecordedGroup<'_>,
    pids: impl Fn() -> io::Result<Vec<u32>>,
) -> io::Result<Members> {
    members_using(
        record,
        pids,
        subc_os::process_group::pgid,
        subc_os::process_group::inspect,
    )
}

fn members_using(
    record: RecordedGroup<'_>,
    pids: impl Fn() -> io::Result<Vec<u32>>,
    pgid: impl Fn(u32) -> io::Result<Option<u32>>,
    inspect: impl Fn(u32) -> io::Result<Option<Identity>>,
) -> io::Result<Members> {
    let mut members = Members {
        live: Vec::new(),
        unknown: None,
    };
    for pid in pids()? {
        // A kernel-proven other group can be excluded without inspecting its
        // owner. Unknown groups cannot be treated as absent container members.
        match pgid(pid) {
            Ok(Some(group)) if group != record.pgid => continue,
            Ok(None) => continue,
            Err(error) if gone(&error) => continue,
            Err(error) => {
                members.unknown.get_or_insert_with(|| {
                    format!("could not establish process {pid}'s group: {error}")
                });
                continue;
            }
            Ok(Some(_)) => {}
        }
        match inspect(pid) {
            Ok(Some(identity)) if record.owns(identity) && !identity.exited => {
                members.live.push(pid)
            }
            Ok(Some(identity)) if identity.pgid == record.pgid && !identity.exited => {
                members.unknown.get_or_insert_with(|| {
                    format!("process {pid} is in the group but its container identity cannot be verified")
                });
            }
            Ok(_) => {}
            Err(error) if gone(&error) => {}
            Err(error) => {
                members
                    .unknown
                    .get_or_insert_with(|| format!("could not inspect process {pid}: {error}"));
            }
        }
    }
    members.live.sort_unstable();
    Ok(members)
}

fn gone(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound
        || error.raw_os_error() == Some(rustix::io::Errno::SRCH.raw_os_error())
}

fn confirmed_members(
    record: RecordedGroup<'_>,
    pids: impl Fn() -> io::Result<Vec<u32>>,
) -> io::Result<Members> {
    let first = members(record, &pids)?;
    if first.live.is_empty() && first.unknown.is_none() {
        // A member can fork and exit while a full enumeration is inspected.
        // Begin the confirming pass only after that first pass has completed.
        return members(record, &pids);
    }
    Ok(first)
}

/// Reuse the roster's existing cadence without waiting: keep the block for
/// members observed alive in this round, even if their SIGKILL is already pending.
pub(crate) fn recheck(record: RecordedGroup<'_>) -> Option<RetainedReason> {
    recheck_candidates(record, subc_os::process_group::pids)
        .unwrap_or_else(|error| Some(RetainedReason::ContainerUnverifiable(error.to_string())))
}

fn recheck_candidates(
    record: RecordedGroup<'_>,
    pids: impl Fn() -> io::Result<Vec<u32>>,
) -> io::Result<Option<RetainedReason>> {
    if !subc_os::boot_id::is_current(record.boot_id)? {
        return Ok(None);
    }
    record.validate()?;
    let found = confirmed_members(record, pids)?;
    if found.live.is_empty() && found.unknown.is_none() {
        return Ok(None);
    }
    if !record.anchor_matches()? {
        return Ok(Some(RetainedReason::ContainerUnverifiable(
            "recorded container anchor PID was reused".into(),
        )));
    }
    let mut unknown = found.unknown;
    for pid in &found.live {
        if let Err(error) = signal_member(record, *pid, Signal::Kill) {
            unknown.get_or_insert_with(|| {
                format!("could not signal verified container member {pid}: {error}")
            });
        }
    }
    Ok(Some(match unknown {
        Some(error) => RetainedReason::ContainerUnverifiable(error),
        None => RetainedReason::ContainerSurvivors(found.live),
    }))
}

fn signal_member(record: RecordedGroup<'_>, pid: u32, signal: Signal) -> io::Result<()> {
    signal_member_using(
        record,
        pid,
        signal,
        subc_os::process_group::session_id,
        subc_os::process_group::inspect,
        |process, signal| process.signal(signal).map(|_| ()),
    )
}

fn signal_member_using(
    record: RecordedGroup<'_>,
    pid: u32,
    signal: Signal,
    session: impl Fn(u32) -> io::Result<Option<u32>>,
    inspect: impl Fn(u32) -> io::Result<Option<Identity>>,
    deliver: impl Fn(&Process, Signal) -> io::Result<()>,
) -> io::Result<()> {
    // Open first: Linux uses this process's pidfd even if the numeric PID is
    // reused between inspection and signalling. macOS rechecks immediately.
    let Some(process) = Process::open(pid)? else {
        return Ok(());
    };
    if !record.anchor_matches()? {
        return Ok(());
    }
    if inspect(pid)?.is_some_and(|facts| record.owns(facts) && !facts.exited)
        && session(pid)? == Some(record.session_id)
    {
        deliver(&process, signal)?;
    }
    Ok(())
}

pub(crate) async fn sweep(record: RecordedGroup<'_>, bounds: SweepBounds) -> SweepDecision {
    sweep_candidates(record, bounds, subc_os::process_group::pids).await
}

async fn sweep_candidates(
    record: RecordedGroup<'_>,
    bounds: SweepBounds,
    pids: impl Fn() -> io::Result<Vec<u32>> + Send + Sync,
) -> SweepDecision {
    let result = sweep_checked(record, bounds, pids).await;
    match result {
        Ok(decision) => decision,
        Err(error) => SweepDecision::Unverifiable(error.to_string()),
    }
}

async fn sweep_checked(
    record: RecordedGroup<'_>,
    bounds: SweepBounds,
    pids: impl Fn() -> io::Result<Vec<u32>> + Send + Sync,
) -> io::Result<SweepDecision> {
    if !subc_os::boot_id::is_current(record.boot_id)? {
        return Ok(SweepDecision::Gone);
    }
    record.validate()?;
    let found = confirmed_members(record, &pids)?;
    let unknown = found.unknown;
    if found.live.is_empty() {
        return Ok(unknown
            .map(SweepDecision::Unverifiable)
            .unwrap_or(SweepDecision::Gone));
    }
    if !record.anchor_matches()? {
        return Ok(SweepDecision::Unverifiable(
            "recorded container anchor PID was reused".into(),
        ));
    }
    for pid in found.live {
        if let Err(error) = signal_member(record, pid, Signal::Terminate) {
            tracing::debug!(pgid = record.pgid, pid, %error, "verified member SIGTERM failed");
        }
    }
    let term_deadline = Instant::now() + bounds.term_grace;
    loop {
        let found = confirmed_members(record, &pids)?;
        let unknown = found.unknown;
        if found.live.is_empty() {
            return Ok(unknown
                .map(SweepDecision::Unverifiable)
                .unwrap_or(SweepDecision::Terminated));
        }
        if !record.anchor_matches()? {
            return Ok(SweepDecision::Unverifiable(
                "recorded container anchor PID was reused".into(),
            ));
        }
        if Instant::now() >= term_deadline {
            break;
        }
        sleep(Duration::from_millis(10)).await;
    }
    let kill_deadline = Instant::now() + bounds.kill_bound;
    loop {
        let found = confirmed_members(record, &pids)?;
        let unknown = found.unknown;
        if found.live.is_empty() {
            return Ok(unknown
                .map(SweepDecision::Unverifiable)
                .unwrap_or(SweepDecision::Killed));
        }
        if !record.anchor_matches()? {
            return Ok(SweepDecision::Unverifiable(
                "recorded container anchor PID was reused".into(),
            ));
        }
        if Instant::now() >= kill_deadline {
            return Ok(unknown
                .map(SweepDecision::Unverifiable)
                .unwrap_or(SweepDecision::ContainerSurvivors(found.live)));
        }
        // Rescan after every round: an exiting member may have forked another
        // helper before receiving its signal. Never reuse a prior identity check.
        for pid in found.live {
            if let Err(error) = signal_member(record, pid, Signal::Kill) {
                tracing::debug!(pgid = record.pgid, pid, %error, "verified member SIGKILL failed");
            }
        }
        sleep(Duration::from_millis(10)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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

    fn boot() -> &'static str {
        static BOOT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        BOOT.get_or_init(|| subc_os::boot_id::current().unwrap())
    }

    fn record(child: &OwnedChild) -> RecordedGroup<'static> {
        let facts = subc_os::process_group::inspect(child.id())
            .unwrap()
            .unwrap();
        RecordedGroup {
            pgid: child.id(),
            created_start_time: facts.start_time,
            boot_id: boot(),
            session_id: facts.session_id,
        }
    }

    fn bounds() -> SweepBounds {
        SweepBounds {
            term_grace: Duration::from_millis(100),
            kill_bound: Duration::from_secs(2),
        }
    }

    #[tokio::test]
    async fn sweep_requires_two_empty_passes_before_declaring_a_group_gone() {
        let mut first = OwnedChild::spawn(0);
        let record = record(&first);
        let mut holder = OwnedChild::spawn(first.id());
        first.0.kill().unwrap();
        first.0.wait().unwrap();
        let passes = std::sync::atomic::AtomicUsize::new(0);
        let outcome = sweep_candidates(record, bounds(), || {
            // Reproduce a fork/exit race in the enumerator, without allowing
            // even a broken sweep to see any PID not owned by this fixture.
            if passes.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                Ok(Vec::new())
            } else {
                Ok(vec![holder.id()])
            }
        })
        .await;
        assert_eq!(
            outcome,
            SweepDecision::Terminated,
            "the second pass must find and terminate the survivor, not return Gone"
        );
        assert!(
            passes.load(std::sync::atomic::Ordering::SeqCst) >= 4,
            "termination also needs two subsequent complete empty passes"
        );
        assert!(holder.0.wait().is_ok());
    }

    #[test]
    fn retained_recheck_requires_two_empty_passes_before_releasing_identity() {
        let mut first = OwnedChild::spawn(0);
        let record = record(&first);
        let mut holder = OwnedChild::spawn(first.id());
        first.0.kill().unwrap();
        first.0.wait().unwrap();
        let passes = std::sync::atomic::AtomicUsize::new(0);
        let retained = recheck_candidates(record, || {
            if passes.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                Ok(Vec::new())
            } else {
                Ok(vec![holder.id()])
            }
        })
        .unwrap();
        assert_eq!(
            retained,
            Some(RetainedReason::ContainerSurvivors(vec![holder.id()])),
            "an empty first pass must not erase the surviving container identity"
        );
        assert!(holder.0.wait().is_ok());
    }

    #[test]
    fn possible_members_with_unknown_groups_or_denied_identity_are_unverifiable() {
        let record = RecordedGroup {
            pgid: 42,
            created_start_time: 100,
            boot_id: boot(),
            session_id: 1,
        };
        let own = Identity {
            pid: 43,
            pgid: 42,
            session_id: 1,
            uid: subc_os::effective_uid(),
            start_time: 100,
            exited: false,
        };
        for unknown_group in [true, false] {
            let found = members_using(
                record,
                || Ok(vec![own.pid]),
                |_| {
                    if unknown_group {
                        Err(io::ErrorKind::PermissionDenied.into())
                    } else {
                        Ok(Some(own.pgid))
                    }
                },
                |_| {
                    if unknown_group {
                        Ok(Some(own))
                    } else {
                        Err(io::ErrorKind::PermissionDenied.into())
                    }
                },
            )
            .unwrap();
            assert!(
                found.live.is_empty() && found.unknown.is_some(),
                "a possible member is neither gone nor verified while group or identity is unknown"
            );
        }
    }

    #[test]
    fn matching_group_with_unverifiable_creation_identity_is_not_empty() {
        let record = RecordedGroup {
            pgid: 42,
            created_start_time: 100,
            boot_id: boot(),
            session_id: 1,
        };
        let found = members_using(
            record,
            || Ok(vec![43]),
            |_| Ok(Some(42)),
            |_| {
                Ok(Some(Identity {
                    pid: 43,
                    pgid: 42,
                    session_id: 1,
                    uid: subc_os::effective_uid(),
                    start_time: 99,
                    exited: false,
                }))
            },
        )
        .unwrap();
        assert!(found.live.is_empty() && found.unknown.is_some(),
            "a live process in this group cannot disappear merely because its birth cannot be verified");
    }

    #[test]
    fn recovered_member_requires_group_owner_and_creation_time() {
        let record = RecordedGroup {
            pgid: 42,
            created_start_time: 100,
            boot_id: boot(),
            session_id: 1,
        };
        let own = Identity {
            pid: 43,
            pgid: 42,
            session_id: 1,
            uid: subc_os::effective_uid(),
            start_time: 100,
            exited: false,
        };
        assert!(record.owns(own));
        assert!(!record.owns(Identity {
            session_id: 2,
            ..own
        }));
        assert!(!record.owns(Identity { pgid: 44, ..own }));
        assert!(!record.owns(Identity {
            uid: own.uid.wrapping_add(1),
            ..own
        }));
        assert!(!record.owns(Identity {
            start_time: 99,
            ..own
        }));
    }

    #[tokio::test]
    async fn previous_boot_is_gone_before_any_container_recovery() {
        let record = RecordedGroup {
            pgid: 42,
            created_start_time: 100,
            boot_id: "00000000-0000-0000-0000-000000000000",
            session_id: 1,
        };
        assert_ne!(record.boot_id, boot());
        let no_enumeration = || -> io::Result<Vec<u32>> {
            panic!("another boot must never authorize enumeration or signalling")
        };
        assert_eq!(
            sweep_candidates(record, bounds(), no_enumeration).await,
            SweepDecision::Gone
        );
        assert_eq!(recheck_candidates(record, no_enumeration).unwrap(), None);
        let malformed = RecordedGroup {
            boot_id: "unknown",
            ..record
        };
        assert!(matches!(
            sweep_candidates(malformed, bounds(), no_enumeration).await,
            SweepDecision::Unverifiable(_)
        ));
        assert!(recheck_candidates(malformed, no_enumeration).is_err());
        #[cfg(target_os = "linux")]
        {
            use crate::live_children::{self, ContainerIdentity, LiveChild};
            let dir = cortexkit_test_support::ScratchDir::new("previous-boot-cgroup");
            let path = live_children::record_path(&dir);
            let entry = LiveChild {
                module_id: "old-cgroup".into(),
                pid: 0,
                protocol: subc_control::ModuleProtocol::None,
                start_time: None,
                executable: None,
                cgroup_name: None,
                container: Some(ContainerIdentity {
                    boot_id: Some(record.boot_id.into()),
                    cgroup_path: Some(dir.join("no-longer-delegated/old")),
                    ..ContainerIdentity::test_metadata()
                }),
            };
            assert_eq!(
                live_children::recheck_container(&entry, None),
                None,
                "a boot mismatch clears even when the old delegation is unavailable"
            );
            live_children::write_record(&path, std::slice::from_ref(&entry)).unwrap();
            let owner = crate::run_dir_lock::RunDirLock::acquire(&path).unwrap();
            assert_eq!(
                live_children::sweep_orphans_with_placement(
                    &owner,
                    &live_children::AdoptedPids::none(),
                    bounds(),
                    None,
                )
                .await,
                vec![(entry, SweepDecision::Gone)]
            );
        }
    }

    #[test]
    fn changed_session_at_delivery_refuses_a_verified_group_member() {
        let child = OwnedChild::spawn(0);
        let record = record(&child);
        let queried = std::cell::Cell::new(false);
        signal_member_using(
            record,
            child.id(),
            Signal::Kill,
            |_| {
                queried.set(true);
                Ok(Some(record.session_id.wrapping_add(1)))
            },
            subc_os::process_group::inspect,
            |_, _| panic!("a session mismatch must never authorize a signal"),
        )
        .unwrap();
        assert!(
            queried.get(),
            "delivery must query the current kernel session"
        );
    }

    #[test]
    fn equal_zero_session_never_authorizes_a_member_older_than_its_container() {
        let first = OwnedChild::spawn(0);
        let holder = OwnedChild::spawn(first.id());
        let record = RecordedGroup {
            session_id: 0,
            ..record(&first)
        };
        let mut facts = subc_os::process_group::inspect(holder.id())
            .unwrap()
            .unwrap();
        facts.session_id = 0;
        facts.start_time = record.created_start_time.checked_sub(1).unwrap();
        let inspected = std::cell::Cell::new(false);
        signal_member_using(
            record,
            holder.id(),
            Signal::Kill,
            |_| Ok(Some(0)),
            |_| {
                inspected.set(true);
                Ok(Some(facts))
            },
            |_, _| panic!("equal zero sessions must not authorize an older member"),
        )
        .unwrap();
        assert!(
            inspected.get(),
            "exercise the member check behind a real owned anchor"
        );
    }

    #[tokio::test]
    async fn recovered_group_stops_owned_holder_and_spares_separate_group() {
        let mut first = OwnedChild::spawn(0);
        let record = record(&first);
        let mut holder = OwnedChild::spawn(first.id());
        let mut sentinel = OwnedChild::spawn(0);
        first.0.kill().unwrap();
        first.0.wait().unwrap();
        // Restrict even broken membership checks to these two owned children;
        // no ambient PID is ever a candidate.
        let candidates = || Ok(vec![holder.id(), sentinel.id()]);
        let outcome = sweep_candidates(record, bounds(), candidates).await;
        assert_eq!(outcome, SweepDecision::Terminated);
        assert!(
            holder.0.try_wait().unwrap().is_some(),
            "the owned holder must have exited"
        );
        assert!(
            sentinel.0.try_wait().unwrap().is_none(),
            "a separate-group sentinel must survive"
        );
    }

    #[tokio::test]
    async fn recovered_group_refuses_a_reused_anchor_identity() {
        let mut first = OwnedChild::spawn(0);
        let mut record = record(&first);
        record.created_start_time = record.created_start_time.checked_sub(1).unwrap();
        let outcome = sweep_candidates(record, bounds(), || Ok(vec![first.id()])).await;
        assert_eq!(
            outcome,
            SweepDecision::Unverifiable("recorded container anchor PID was reused".into())
        );
        assert!(
            first.0.try_wait().unwrap().is_none(),
            "an unmatched anchor must never be signalled"
        );
    }

    #[tokio::test]
    async fn recovered_group_zombie_member_does_not_block() {
        let mut first = OwnedChild::spawn(0);
        let record = record(&first);
        let mut holder = OwnedChild::spawn(first.id());
        first.0.kill().unwrap();
        first.0.wait().unwrap();
        holder.0.kill().unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while subc_os::child_exit::observe(holder.id()).unwrap().is_none() {
                sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        // macOS process inspection may hide an exited, unreaped child. The
        // non-reaping observation above proves its PID is still reserved.
        assert!(subc_os::process_group::inspect(holder.id())
            .unwrap()
            .is_none_or(|facts| facts.exited));
        let outcome = sweep_candidates(record, bounds(), || Ok(vec![holder.id()])).await;
        assert_eq!(
            outcome,
            SweepDecision::Gone,
            "a verified zombie holds no live container resources"
        );
    }

    #[test]
    fn unrelated_uninspectable_process_does_not_block_recovery() {
        let first = OwnedChild::spawn(0);
        let record = record(&first);
        let unrelated = OwnedChild::spawn(0);
        let found = members_using(
            record,
            || Ok(vec![unrelated.id()]),
            subc_os::process_group::pgid,
            |_| Err(io::Error::from(io::ErrorKind::PermissionDenied)),
        )
        .unwrap();
        assert!(found.live.is_empty());
        assert!(
            found.unknown.is_none(),
            "a proven different group cannot be a denied container member"
        );
    }

    #[tokio::test]
    async fn wnowait_proven_exited_holder_is_gone_when_inspection_returns_esrch() {
        let first = OwnedChild::spawn(0);
        let record = record(&first);
        let mut holder = OwnedChild::spawn(first.id());
        holder.0.kill().unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while subc_os::child_exit::observe(holder.id()).unwrap().is_none() {
                sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        // Simulate process inspection returning "no such process" for an owned,
        // positively exited child. It still has wait status, so its numeric PID
        // cannot denote a stranger.
        let found = members_using(
            record,
            || Ok(vec![holder.id()]),
            |_| Ok(Some(record.pgid)),
            |_| {
                Err(io::Error::from_raw_os_error(
                    rustix::io::Errno::SRCH.raw_os_error(),
                ))
            },
        )
        .unwrap();
        assert!(found.live.is_empty());
        assert!(
            found.unknown.is_none(),
            "ESRCH for the exited holder is gone, not an inspection block"
        );
        assert!(
            subc_os::child_exit::observe(holder.id()).unwrap().is_some(),
            "the holder remains unreaped"
        );
    }
}
