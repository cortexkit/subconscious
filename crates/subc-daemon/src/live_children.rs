//! The live-children record and the boot-time orphan sweep.
//!
//! Supervised modules lead their own process groups (see `spawn_child_in_slot`),
//! so when the daemon dies without running its shutdown stop (a crash, an OOM
//! kill, SIGKILL) nothing ends the children that do not exit on EOF: every
//! `protocol: "none"` child, which has no connection to see EOF on, and any
//! module that ignores it. The next daemon would then start fresh copies
//! beside them, fighting over ports and stores.
//!
//! So the daemon keeps a record, in its run directory, of every child in its
//! roster: rewritten whole (temp file, then rename) on each admit and release,
//! so a reader only ever sees a complete record. At boot, before any module is
//! spawned, [`sweep_orphans`] reads the previous daemon's record and ends every
//! listed process that is still the process that was recorded.
//!
//! "Still the process that was recorded" is three checks, and a process that
//! fails any of them is never signalled: the pid, the kernel's start time for
//! it (a reaped pid can be reused by an unrelated process, which then has a
//! later start time), and the device and inode of the image it is executing
//! (which catches a reuse inside one start-time tick). The start time and
//! image come from the `subc-os` crate, the only place those reads need unsafe
//! code.
//!
//! On Windows the sweep signals nothing: the daemon's job object already ends
//! a crashed daemon's children, and the record is read only to log it.

use std::{
    collections::BTreeSet,
    fs, io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use subc_control::ModuleProtocol;
use tracing::{info, warn};

/// The record's file name inside the daemon run directory.
pub(crate) const LIVE_CHILDREN_FILE_NAME: &str = "live-children.json";

/// Bumped on any change a previous daemon's reader would misread. A record
/// with another version is not swept: nothing in it is trusted enough to
/// signal on.
const RECORD_VERSION: u32 = 1;

/// Device and inode of the executable a child was spawned from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ExecutableIdentity {
    pub(crate) device: u64,
    pub(crate) inode: u64,
}

impl From<subc_os::FileIdentity> for ExecutableIdentity {
    fn from(identity: subc_os::FileIdentity) -> Self {
        Self {
            device: identity.device,
            inode: identity.inode,
        }
    }
}

/// One roster entry as recorded on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LiveChild {
    pub(crate) module_id: String,
    pub(crate) pid: u32,
    pub(crate) protocol: ModuleProtocol,
    /// `subc_os::start_time` read right after spawn. `None` where the platform
    /// has no source, and then the entry is never signalled.
    pub(crate) start_time: Option<u64>,
    /// The spawned path's device and inode, read at spawn. `None` if the path
    /// could not be read, and then the entry is never signalled.
    pub(crate) executable: Option<ExecutableIdentity>,
    /// The child's cgroup directory name, when it was placed in one (Linux).
    pub(crate) cgroup_name: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct RecordFile {
    version: u32,
    children: Vec<LiveChild>,
}

/// Replace the record at `path` with `children`.
pub(crate) fn write_record(path: &Path, children: &[LiveChild]) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(&RecordFile {
        version: RECORD_VERSION,
        children: children.to_vec(),
    })
    .map_err(io::Error::other)?;
    write_atomically(path, &bytes, |_| Ok(()))
}

/// Why a record could not be used.
#[derive(Debug)]
pub(crate) enum RecordReadError {
    Io(io::Error),
    Malformed(serde_json::Error),
    UnknownVersion(u32),
}

impl std::fmt::Display for RecordReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "could not read the record: {error}"),
            Self::Malformed(error) => write!(f, "the record is not valid: {error}"),
            Self::UnknownVersion(version) => {
                write!(
                    f,
                    "the record has version {version}, expected {RECORD_VERSION}"
                )
            }
        }
    }
}

/// The children listed at `path`. A missing file is an empty record: the
/// previous daemon never spawned anything, or this is the first boot.
pub(crate) fn read_record(path: &Path) -> Result<Vec<LiveChild>, RecordReadError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(RecordReadError::Io(error)),
    };
    let record: RecordFile = serde_json::from_slice(&bytes).map_err(RecordReadError::Malformed)?;
    if record.version != RECORD_VERSION {
        return Err(RecordReadError::UnknownVersion(record.version));
    }
    Ok(record.children)
}

/// Write `bytes` to a new file beside `path`, then rename it over `path`.
///
/// Rename within one directory is atomic, so `path` always holds either the
/// previous complete record or the new one, never a prefix, whenever the
/// daemon dies. `before_rename` runs between the two steps; it exists so a
/// test can fail the write at that point. The file is not fsynced: the record
/// only has to survive the daemon process dying, which leaves the page cache
/// intact, and a whole-machine crash ends every child with it.
fn write_atomically(
    path: &Path,
    bytes: &[u8],
    before_rename: impl FnOnce(&Path) -> io::Result<()>,
) -> io::Result<()> {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(directory)?;
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| LIVE_CHILDREN_FILE_NAME.to_owned());
    let temp = directory.join(format!(
        ".{file_name}.{}.{}.tmp",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let result = write_new_owner_only(&temp, bytes)
        .and_then(|()| before_rename(&temp))
        .and_then(|()| fs::rename(&temp, path));
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn write_new_owner_only(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;

    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Pids and module ids of a user's processes: nobody else's business.
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)
}

/// Pids a handover from a previous daemon image says are being adopted, which
/// the sweep must leave alone.
///
/// An in-place upgrade execs the new daemon binary in the same process, so the
/// record then lists exactly the children the new image is taking over, still
/// running and with matching start times; sweeping them would kill the fleet
/// the upgrade exists to keep. Nothing adopts children yet, so every caller
/// passes [`AdoptedPids::none`] today; the handover work fills this in.
#[derive(Debug, Clone, Default)]
pub(crate) struct AdoptedPids(BTreeSet<u32>);

impl AdoptedPids {
    pub(crate) fn none() -> Self {
        Self::default()
    }

    #[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
    pub(crate) fn of(pids: impl IntoIterator<Item = u32>) -> Self {
        Self(pids.into_iter().collect())
    }

    fn contains(&self, pid: u32) -> bool {
        self.0.contains(&pid)
    }
}

/// Whether a live process is the one a record entry describes.
// Only the Linux and macOS sweep compares identities and signals; elsewhere
// the record is logged and these go unused.
#[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IdentityVerdict {
    Matches,
    PidDiffers,
    StartTimeUnrecorded,
    StartTimeDiffers,
    ExecutableUnrecorded,
    ExecutableUnreadable,
    ExecutableDiffers,
}

impl IdentityVerdict {
    fn describe(self) -> &'static str {
        match self {
            Self::Matches => "matches",
            Self::PidDiffers => "the observed process has another pid",
            Self::StartTimeUnrecorded => "no start time was recorded at spawn",
            Self::StartTimeDiffers => {
                "the pid now belongs to a process with another start time (pid reused)"
            }
            Self::ExecutableUnrecorded => "no executable identity was recorded at spawn",
            Self::ExecutableUnreadable => "the process's executable could not be read",
            Self::ExecutableDiffers => "the process is running another executable",
        }
    }
}

/// Compare a record entry with what is running at `observed_pid` now. Only
/// [`IdentityVerdict::Matches`] permits a signal; a field the record lacks or
/// the platform cannot read refuses, like a field that differs.
#[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
pub(crate) fn identity_verdict(
    recorded: &LiveChild,
    observed_pid: u32,
    observed: &subc_os::Observation,
) -> IdentityVerdict {
    if observed_pid != recorded.pid {
        return IdentityVerdict::PidDiffers;
    }
    let Some(start_time) = recorded.start_time else {
        return IdentityVerdict::StartTimeUnrecorded;
    };
    if observed.start_time != start_time {
        return IdentityVerdict::StartTimeDiffers;
    }
    let Some(executable) = recorded.executable else {
        return IdentityVerdict::ExecutableUnrecorded;
    };
    let Some(running) = observed.executable else {
        return IdentityVerdict::ExecutableUnreadable;
    };
    if ExecutableIdentity::from(running) != executable {
        return IdentityVerdict::ExecutableDiffers;
    }
    IdentityVerdict::Matches
}

/// What the sweep did with one record entry.
#[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SweepDecision {
    /// Claimed by a handover; left running.
    Adopted,
    /// No process holds the pid any more, or it has exited.
    Gone,
    /// Something holds the pid, but not the recorded process; not signalled.
    Mismatched(IdentityVerdict),
    /// The process could not be inspected; not signalled.
    Unverifiable(String),
    /// Matched; exited after SIGTERM.
    Terminated,
    /// Matched; still running at the SIGTERM grace, exited after SIGKILL.
    Killed,
    /// Matched and signalled, but still observed at the end of the bound.
    Survived,
    /// No process identity source on this platform; not signalled.
    #[cfg_attr(any(target_os = "linux", target_os = "macos"), allow(dead_code))]
    NotSignalledOnThisPlatform,
}

/// How long the sweep waits after each signal.
#[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
#[derive(Debug, Clone, Copy)]
pub(crate) struct SweepBounds {
    /// Between SIGTERM and SIGKILL. An orphan's daemon is already gone, so
    /// this only has to let a module finish a clean exit (flush, remove its
    /// socket) that a SIGKILL would skip; it also delays boot, but only when
    /// there is an orphan to end.
    pub(crate) term_grace: Duration,
    /// After SIGKILL. SIGKILL cannot be ignored, so this covers scheduling
    /// and the orphan's new parent reaping it.
    pub(crate) kill_bound: Duration,
}

impl Default for SweepBounds {
    fn default() -> Self {
        Self {
            term_grace: Duration::from_secs(2),
            kill_bound: Duration::from_secs(1),
        }
    }
}

#[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
const POLL: Duration = Duration::from_millis(20);

/// End every process the previous daemon's record at `path` lists that is
/// still exactly the process recorded, except those in `adopted`, then
/// replace the record with an empty one. Runs before any module is spawned.
///
/// Every entry is logged with its decision. Returns the decisions for tests.
pub(crate) async fn sweep_orphans(
    path: &Path,
    adopted: &AdoptedPids,
    bounds: SweepBounds,
) -> Vec<(LiveChild, SweepDecision)> {
    let entries = match read_record(path) {
        Ok(entries) => entries,
        Err(error) => {
            warn!(
                path = %path.display(),
                %error,
                "live-children record unusable; no orphan sweep, nothing signalled"
            );
            Vec::new()
        }
    };
    let decisions = sweep_entries(entries, adopted, bounds).await;
    for (entry, decision) in &decisions {
        log_decision(entry, decision);
    }
    // Everything listed has now been dealt with or deliberately left alone;
    // a stale record would only have the next boot look at it again.
    if let Err(error) = write_record(path, &[]) {
        warn!(path = %path.display(), %error, "could not reset the live-children record after the orphan sweep");
    }
    decisions
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn sweep_entries(
    entries: Vec<LiveChild>,
    adopted: &AdoptedPids,
    bounds: SweepBounds,
) -> Vec<(LiveChild, SweepDecision)> {
    use subc_os::{Process, Signal};
    use tokio::time::{sleep, Instant};

    let mut decisions: Vec<(LiveChild, SweepDecision)> = Vec::new();
    let mut signalled: Vec<(LiveChild, Process)> = Vec::new();
    for entry in entries {
        if adopted.contains(entry.pid) {
            decisions.push((entry, SweepDecision::Adopted));
            continue;
        }
        // Opened before any check, so on Linux the checks and the signal
        // below all refer to one process through its pidfd.
        let process = match Process::open(entry.pid) {
            Ok(Some(process)) => process,
            Ok(None) => {
                decisions.push((entry, SweepDecision::Gone));
                continue;
            }
            Err(error) => {
                decisions.push((entry, SweepDecision::Unverifiable(error.to_string())));
                continue;
            }
        };
        match current_verdict(&entry, &process) {
            None => decisions.push((entry, SweepDecision::Gone)),
            Some(IdentityVerdict::Matches) => {
                info!(
                    module_id = %entry.module_id,
                    pid = entry.pid,
                    pidfd = process.signals_through_pidfd(),
                    "orphan sweep: previous daemon's child still running and matches pid, start time and executable; sending SIGTERM"
                );
                match process.signal(Signal::Terminate) {
                    Ok(true) => signalled.push((entry, process)),
                    // It exited between the check and the signal.
                    Ok(false) => decisions.push((entry, SweepDecision::Gone)),
                    Err(error) => {
                        decisions.push((entry, SweepDecision::Unverifiable(error.to_string())))
                    }
                }
            }
            Some(verdict) => decisions.push((entry, SweepDecision::Mismatched(verdict))),
        }
    }

    let exited = |entry: &LiveChild, process: &Process| {
        current_verdict(entry, process) != Some(IdentityVerdict::Matches)
    };
    let term_deadline = Instant::now() + bounds.term_grace;
    loop {
        signalled.retain(|(entry, process)| {
            if exited(entry, process) {
                decisions.push((entry.clone(), SweepDecision::Terminated));
                false
            } else {
                true
            }
        });
        if signalled.is_empty() || Instant::now() >= term_deadline {
            break;
        }
        sleep(POLL).await;
    }

    for (entry, process) in &signalled {
        // Checked again right before the signal: without a pidfd this is what
        // keeps a SIGKILL from reaching a process that took the pid after the
        // orphan exited.
        if exited(entry, process) {
            continue;
        }
        warn!(
            module_id = %entry.module_id,
            pid = entry.pid,
            grace_ms = bounds.term_grace.as_millis() as u64,
            "orphan sweep: previous daemon's child still running after SIGTERM grace; sending SIGKILL"
        );
        let _ = process.signal(Signal::Kill);
    }
    let kill_deadline = Instant::now() + bounds.kill_bound;
    loop {
        signalled.retain(|(entry, process)| {
            if exited(entry, process) {
                decisions.push((entry.clone(), SweepDecision::Killed));
                false
            } else {
                true
            }
        });
        if signalled.is_empty() || Instant::now() >= kill_deadline {
            break;
        }
        sleep(POLL).await;
    }
    decisions.extend(
        signalled
            .into_iter()
            .map(|(entry, _)| (entry, SweepDecision::Survived)),
    );
    decisions
}

/// `None` once the process has exited; otherwise whether it is still the
/// recorded one.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn current_verdict(entry: &LiveChild, process: &subc_os::Process) -> Option<IdentityVerdict> {
    process
        .observe()
        .map(|observed| identity_verdict(entry, process.pid(), &observed))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
async fn sweep_entries(
    entries: Vec<LiveChild>,
    adopted: &AdoptedPids,
    _bounds: SweepBounds,
) -> Vec<(LiveChild, SweepDecision)> {
    entries
        .into_iter()
        .map(|entry| {
            let decision = if adopted.contains(entry.pid) {
                SweepDecision::Adopted
            } else {
                SweepDecision::NotSignalledOnThisPlatform
            };
            (entry, decision)
        })
        .collect()
}

fn log_decision(entry: &LiveChild, decision: &SweepDecision) {
    let module_id = entry.module_id.as_str();
    let pid = entry.pid;
    match decision {
        SweepDecision::Adopted => {
            info!(module_id, pid, "orphan sweep: child adopted by the handover; left running")
        }
        SweepDecision::Gone => {
            info!(module_id, pid, "orphan sweep: previous daemon's child is gone; dropped from the record")
        }
        SweepDecision::Mismatched(verdict) => info!(
            module_id,
            pid,
            reason = verdict.describe(),
            "orphan sweep: pid does not match the recorded child; not signalled"
        ),
        SweepDecision::Unverifiable(error) => warn!(
            module_id,
            pid,
            %error,
            "orphan sweep: could not inspect the recorded pid; not signalled"
        ),
        SweepDecision::Terminated => {
            info!(module_id, pid, "orphan sweep: previous daemon's child exited after SIGTERM")
        }
        SweepDecision::Killed => {
            info!(module_id, pid, "orphan sweep: previous daemon's child exited after SIGKILL")
        }
        SweepDecision::Survived => warn!(
            module_id,
            pid,
            "orphan sweep: previous daemon's child still observed after SIGKILL; continuing boot"
        ),
        SweepDecision::NotSignalledOnThisPlatform => info!(
            module_id,
            pid,
            "orphan sweep: previous daemon's child recorded; not signalled, the job object ends a crashed daemon's children on this platform"
        ),
    }
}

/// The record's path inside a daemon run directory.
pub(crate) fn record_path(run_dir: &Path) -> PathBuf {
    run_dir.join(LIVE_CHILDREN_FILE_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestTempDir;

    fn child(module_id: &str, pid: u32) -> LiveChild {
        LiveChild {
            module_id: module_id.to_owned(),
            pid,
            protocol: ModuleProtocol::None,
            start_time: Some(1_000),
            executable: Some(ExecutableIdentity {
                device: 7,
                inode: 11,
            }),
            cgroup_name: Some(format!("{module_id}-a")),
        }
    }

    fn observed(start_time: u64, device: u64, inode: u64) -> subc_os::Observation {
        subc_os::Observation {
            start_time,
            executable: Some(subc_os::FileIdentity { device, inode }),
        }
    }

    #[test]
    fn record_round_trips() {
        let dir = TestTempDir::new("live-children-round-trip");
        let path = record_path(&dir);
        let mut subc = child("aft", 41);
        subc.protocol = ModuleProtocol::Subc;
        subc.start_time = None;
        subc.executable = None;
        subc.cgroup_name = None;
        let children = vec![child("nats", 40), subc];
        write_record(&path, &children).unwrap();
        assert_eq!(read_record(&path).unwrap(), children);
        write_record(&path, &[]).unwrap();
        assert_eq!(read_record(&path).unwrap(), Vec::new());
    }

    #[test]
    fn a_missing_record_is_empty_and_an_unknown_version_is_refused() {
        let dir = TestTempDir::new("live-children-missing");
        let path = record_path(&dir);
        assert_eq!(read_record(&path).unwrap(), Vec::new());
        fs::write(&path, br#"{"version":99,"children":[]}"#).unwrap();
        assert!(matches!(
            read_record(&path),
            Err(RecordReadError::UnknownVersion(99))
        ));
    }

    /// A write that fails after the new bytes are on disk but before the
    /// rename must leave the previous record whole and no temp file behind.
    #[test]
    fn a_failed_write_leaves_the_previous_record_whole() {
        let dir = TestTempDir::new("live-children-failed-write");
        let path = record_path(&dir);
        let previous = vec![child("nats", 40)];
        write_record(&path, &previous).unwrap();
        let before = fs::read(&path).unwrap();

        let error = write_atomically(&path, b"{\"version\":1,\"chil", |temp| {
            assert!(temp.exists(), "the new bytes are written before the rename");
            Err(io::Error::other("simulated crash before rename"))
        })
        .unwrap_err();
        assert_eq!(error.to_string(), "simulated crash before rename");
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(read_record(&path).unwrap(), previous);
        let leftovers: Vec<_> = fs::read_dir(&*dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name != LIVE_CHILDREN_FILE_NAME)
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
    }

    /// A reader racing a writer only ever sees a complete record. Records of
    /// different lengths alternate, so a reader of a file written in place
    /// would sooner or later see a torn one.
    #[test]
    fn a_concurrent_reader_never_sees_a_partial_record() {
        let dir = TestTempDir::new("live-children-concurrent");
        let path = record_path(&dir);
        let short = vec![child("nats", 40)];
        let long: Vec<LiveChild> = (0..200).map(|pid| child("module", pid)).collect();
        write_record(&path, &short).unwrap();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let writer = {
            let (path, stop, short, long) =
                (path.clone(), stop.clone(), short.clone(), long.clone());
            std::thread::spawn(move || {
                let mut round = 0usize;
                while !stop.load(Ordering::Relaxed) {
                    let children = if round.is_multiple_of(2) {
                        &long
                    } else {
                        &short
                    };
                    write_record(&path, children).unwrap();
                    round += 1;
                }
                round
            })
        };
        for _ in 0..2_000 {
            let read = read_record(&path).expect("every read sees a complete record");
            assert!(read == short || read == long);
        }
        stop.store(true, Ordering::Relaxed);
        assert!(writer.join().unwrap() > 1, "the writer ran concurrently");
    }

    #[test]
    fn matcher_accepts_only_when_pid_start_time_and_executable_all_match() {
        let recorded = child("nats", 40);
        assert_eq!(
            identity_verdict(&recorded, 40, &observed(1_000, 7, 11)),
            IdentityVerdict::Matches
        );
    }

    #[test]
    fn matcher_refuses_another_pid() {
        let recorded = child("nats", 40);
        assert_eq!(
            identity_verdict(&recorded, 41, &observed(1_000, 7, 11)),
            IdentityVerdict::PidDiffers
        );
    }

    #[test]
    fn matcher_refuses_a_reused_pid_with_another_start_time() {
        let recorded = child("nats", 40);
        assert_eq!(
            identity_verdict(&recorded, 40, &observed(1_001, 7, 11)),
            IdentityVerdict::StartTimeDiffers
        );
    }

    #[test]
    fn matcher_refuses_same_pid_and_start_time_running_another_executable() {
        let recorded = child("nats", 40);
        assert_eq!(
            identity_verdict(&recorded, 40, &observed(1_000, 7, 12)),
            IdentityVerdict::ExecutableDiffers
        );
        assert_eq!(
            identity_verdict(&recorded, 40, &observed(1_000, 8, 11)),
            IdentityVerdict::ExecutableDiffers
        );
    }

    #[test]
    fn matcher_refuses_whatever_it_cannot_compare() {
        let mut no_start = child("nats", 40);
        no_start.start_time = None;
        assert_eq!(
            identity_verdict(&no_start, 40, &observed(1_000, 7, 11)),
            IdentityVerdict::StartTimeUnrecorded
        );
        let mut no_exe = child("nats", 40);
        no_exe.executable = None;
        assert_eq!(
            identity_verdict(&no_exe, 40, &observed(1_000, 7, 11)),
            IdentityVerdict::ExecutableUnrecorded
        );
        let unreadable = subc_os::Observation {
            start_time: 1_000,
            executable: None,
        };
        assert_eq!(
            identity_verdict(&child("nats", 40), 40, &unreadable),
            IdentityVerdict::ExecutableUnreadable
        );
    }

    /// The roster rewrites the record on every admit and release, so it
    /// always lists exactly the processes the daemon has running.
    #[test]
    fn the_roster_keeps_the_record_in_step_with_admits_and_releases() {
        let dir = TestTempDir::new("roster-record");
        let path = record_path(&dir);
        let roster = crate::child_roster::ChildRoster::default();
        roster.record_to(path.clone());
        let recorded = crate::child_roster::RecordedIdentity {
            start_time: Some(5),
            executable: Some(ExecutableIdentity {
                device: 1,
                inode: 2,
            }),
            cgroup_name: Some("nats-a".to_owned()),
        };
        let first = roster.admit(
            "nats".to_owned(),
            40,
            ModuleProtocol::None,
            None,
            recorded.clone(),
        );
        let second = roster.admit(
            "aft".to_owned(),
            41,
            ModuleProtocol::Subc,
            None,
            crate::child_roster::RecordedIdentity::default(),
        );
        let pids = |path: &Path| -> Vec<u32> {
            read_record(path)
                .unwrap()
                .iter()
                .map(|child| child.pid)
                .collect()
        };
        assert_eq!(pids(&path), vec![40, 41]);
        assert_eq!(
            read_record(&path).unwrap()[0],
            LiveChild {
                module_id: "nats".to_owned(),
                pid: 40,
                protocol: ModuleProtocol::None,
                start_time: Some(5),
                executable: recorded.executable,
                cgroup_name: Some("nats-a".to_owned()),
            }
        );
        drop(first);
        assert_eq!(pids(&path), vec![41]);
        drop(second);
        assert_eq!(pids(&path), Vec::<u32>::new());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    mod real_processes {
        use std::{
            os::unix::process::ExitStatusExt,
            path::Path,
            process::{Child, Command, Stdio},
            time::Instant,
        };

        use super::*;

        fn executable(name: &str) -> PathBuf {
            ["/bin", "/usr/bin"]
                .into_iter()
                .map(|dir| Path::new(dir).join(name))
                .find(|path| path.exists())
                .unwrap_or_else(|| panic!("{name} is installed"))
        }

        /// A child of this test standing in for a previous daemon's child,
        /// recorded the way the daemon records one at spawn: the start time
        /// read right after spawn and the spawned path's device and inode.
        fn spawn_recorded(program: &Path, args: &[&str]) -> (Child, LiveChild) {
            let child = Command::new(program)
                .args(args)
                .stdin(Stdio::piped())
                .spawn()
                .unwrap();
            let entry = LiveChild {
                module_id: "previous".to_owned(),
                pid: child.id(),
                protocol: ModuleProtocol::None,
                start_time: subc_os::start_time(child.id()),
                executable: subc_os::file_identity(program).map(ExecutableIdentity::from),
                cgroup_name: None,
            };
            assert!(entry.start_time.is_some() && entry.executable.is_some());
            // Until its exec completes the child still runs this test's image.
            let deadline = Instant::now() + Duration::from_secs(5);
            while subc_os::Process::open(child.id())
                .unwrap()
                .and_then(|process| process.observe())
                .and_then(|observation| observation.executable)
                .map(ExecutableIdentity::from)
                != entry.executable
            {
                assert!(Instant::now() < deadline, "child never ran {program:?}");
                std::thread::sleep(Duration::from_millis(5));
            }
            (child, entry)
        }

        fn quick() -> SweepBounds {
            SweepBounds {
                term_grace: Duration::from_millis(300),
                kill_bound: Duration::from_secs(2),
            }
        }

        fn still_running(child: &mut Child) -> bool {
            child.try_wait().unwrap().is_none()
        }

        #[tokio::test]
        async fn a_recorded_child_still_running_is_terminated_at_boot() {
            let dir = TestTempDir::new("sweep-terminates");
            let path = record_path(&dir);
            let (mut child, entry) = spawn_recorded(&executable("sleep"), &["60"]);
            write_record(&path, std::slice::from_ref(&entry)).unwrap();

            let decisions = sweep_orphans(&path, &AdoptedPids::none(), quick()).await;

            assert_eq!(decisions, vec![(entry, SweepDecision::Terminated)]);
            let status = child.wait().unwrap();
            assert_eq!(status.signal(), Some(15), "ended by SIGTERM: {status}");
            assert_eq!(read_record(&path).unwrap(), Vec::new());
        }

        #[tokio::test]
        async fn a_recorded_child_ignoring_sigterm_is_killed_after_the_grace() {
            let dir = TestTempDir::new("sweep-kills");
            let path = record_path(&dir);
            // `read` is a shell builtin, so the shell itself waits on the
            // piped stdin with SIGTERM ignored; nothing else is exec'd. The
            // marker (the script's `$0`) appears once the trap is in place.
            let ready = dir.join("ready");
            let (mut child, entry) = spawn_recorded(
                &executable("bash"),
                &[
                    "-c",
                    "trap '' TERM; : > \"$0\"; read -r _",
                    ready.to_str().unwrap(),
                ],
            );
            let deadline = Instant::now() + Duration::from_secs(5);
            while !ready.exists() {
                assert!(Instant::now() < deadline, "shell never installed its trap");
                std::thread::sleep(Duration::from_millis(5));
            }
            write_record(&path, std::slice::from_ref(&entry)).unwrap();

            let started = Instant::now();
            let decisions = sweep_orphans(&path, &AdoptedPids::none(), quick()).await;

            assert_eq!(decisions, vec![(entry, SweepDecision::Killed)]);
            assert!(started.elapsed() >= quick().term_grace);
            let status = child.wait().unwrap();
            assert_eq!(status.signal(), Some(9), "ended by SIGKILL: {status}");
        }

        #[tokio::test]
        async fn a_pid_now_held_by_an_unrelated_process_is_not_signalled() {
            let dir = TestTempDir::new("sweep-reused-pid");
            let path = record_path(&dir);
            let (mut child, entry) = spawn_recorded(&executable("sleep"), &["60"]);
            // The record says the pid belonged to a process started at
            // another time: what a reaped pid handed to a new process reads as.
            let mut reused = entry.clone();
            reused.start_time = reused.start_time.map(|start| start + 1);
            write_record(&path, std::slice::from_ref(&reused)).unwrap();

            let decisions = sweep_orphans(&path, &AdoptedPids::none(), quick()).await;

            assert_eq!(
                decisions,
                vec![(
                    reused,
                    SweepDecision::Mismatched(IdentityVerdict::StartTimeDiffers)
                )]
            );
            assert!(
                still_running(&mut child),
                "an unrelated process was signalled"
            );
            child.kill().unwrap();
            child.wait().unwrap();
        }

        #[tokio::test]
        async fn same_pid_and_start_time_running_another_executable_is_not_signalled() {
            let dir = TestTempDir::new("sweep-other-executable");
            let path = record_path(&dir);
            let (mut child, entry) = spawn_recorded(&executable("sleep"), &["60"]);
            let mut other = entry.clone();
            other.executable = subc_os::file_identity(&std::env::current_exe().unwrap())
                .map(ExecutableIdentity::from);
            write_record(&path, std::slice::from_ref(&other)).unwrap();

            let decisions = sweep_orphans(&path, &AdoptedPids::none(), quick()).await;

            assert_eq!(
                decisions,
                vec![(
                    other,
                    SweepDecision::Mismatched(IdentityVerdict::ExecutableDiffers)
                )]
            );
            assert!(
                still_running(&mut child),
                "an unrelated process was signalled"
            );
            child.kill().unwrap();
            child.wait().unwrap();
        }

        #[tokio::test]
        async fn an_entry_for_a_gone_process_is_dropped() {
            let dir = TestTempDir::new("sweep-gone");
            let path = record_path(&dir);
            let (mut child, entry) = spawn_recorded(&executable("sleep"), &["60"]);
            child.kill().unwrap();
            child.wait().unwrap();
            write_record(&path, std::slice::from_ref(&entry)).unwrap();

            let decisions = sweep_orphans(&path, &AdoptedPids::none(), quick()).await;

            assert_eq!(decisions, vec![(entry, SweepDecision::Gone)]);
            assert_eq!(read_record(&path).unwrap(), Vec::new());
        }

        #[tokio::test]
        async fn an_adopted_pid_is_left_running() {
            let dir = TestTempDir::new("sweep-adopted");
            let path = record_path(&dir);
            let (mut child, entry) = spawn_recorded(&executable("sleep"), &["60"]);
            write_record(&path, std::slice::from_ref(&entry)).unwrap();

            let decisions = sweep_orphans(&path, &AdoptedPids::of([entry.pid]), quick()).await;

            assert_eq!(decisions, vec![(entry, SweepDecision::Adopted)]);
            assert!(still_running(&mut child), "an adopted child was signalled");
            child.kill().unwrap();
            child.wait().unwrap();
        }
    }
}
