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
//! code. On macOS, `/bin/sh` is a launcher that re-execs its selected shell;
//! when recording that image, also retain the selected interpreter's identity.
//! Matching never resolves the shell selection again after a crash or upgrade.
//!
//! On Windows the job object is the primary crash cleanup. The sweep is a
//! fallback: only a matching creation time on a retained process handle permits
//! a forced stop. An inaccessible process or reused PID is left alone.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use subc_control::ModuleProtocol;
use tracing::{info, warn};

use crate::run_dir_lock::RunDirLock;

/// The record's file name inside the daemon run directory.
pub(crate) const LIVE_CHILDREN_FILE_NAME: &str = "live-children.json";

/// Bumped on any change a previous daemon's reader would misread. A record
/// with another version is not swept: nothing in it is trusted enough to
/// signal on.
const RECORD_VERSION: u32 = 1;

/// The device and inode of the executable a child was recorded running.
///
/// On macOS, `/bin/sh` re-execs the shell `/private/var/select/sh` points to
/// (see `man sh`), so a `#!/bin/sh` module's image changes once after launch.
/// For that image only, the identity also records the selected shell, read at
/// launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ExecutableIdentity {
    pub(crate) device: u64,
    pub(crate) inode: u64,
    /// The selected shell for a `/bin/sh` image, read when the child was
    /// recorded; the orphan check never looks it up again, so a selection
    /// changed after a crash cannot make an unrelated process match. Absent on
    /// every other image and in records written before this field existed,
    /// which then match only the executable itself. Daemons that predate the
    /// field ignore it when reading.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    macos_sh_interpreter: Option<InterpreterIdentity>,
    /// Older daemons reading version-1 `live-children.json` ignore unknown
    /// executable fields, so adding this does not lose the roster on rollback.
    /// Unix omits it, preserving its record bytes. Windows IDs stay here in full;
    /// the legacy device/inode slots are zero, not a truncated Windows file ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    windows_file_identity: Option<WindowsFileIdentity>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct WindowsFileIdentity {
    volume_serial_number: u64,
    file_id: [u8; 16],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct InterpreterIdentity {
    device: u64,
    inode: u64,
}

impl From<subc_os::FileIdentity> for ExecutableIdentity {
    fn from(identity: subc_os::FileIdentity) -> Self {
        // This runs when the supervisor records a confirmed module image, so the
        // shell selection is read at launch time, never at sweep time.
        #[cfg(target_os = "macos")]
        let macos_sh_interpreter = recorded_sh_interpreter(identity);
        #[cfg(not(target_os = "macos"))]
        let macos_sh_interpreter = None;
        Self {
            device: identity.device,
            inode: identity.inode,
            macos_sh_interpreter,
            windows_file_identity: None,
        }
    }
}

#[cfg(windows)]
impl From<subc_os::WindowsFileIdentity> for ExecutableIdentity {
    fn from(identity: subc_os::WindowsFileIdentity) -> Self {
        Self {
            device: 0,
            inode: 0,
            macos_sh_interpreter: None,
            windows_file_identity: Some(WindowsFileIdentity {
                volume_serial_number: identity.volume_serial_number,
                file_id: identity.file_id,
            }),
        }
    }
}

#[cfg(target_os = "macos")]
fn recorded_sh_interpreter(image: subc_os::FileIdentity) -> Option<InterpreterIdentity> {
    if subc_os::file_identity(Path::new("/bin/sh")) != Some(image) {
        return None;
    }
    // `man sh` names bash, dash and zsh as the only shells `/bin/sh` re-execs.
    // Any other or missing selection records nothing extra, so the orphan check
    // matches only `/bin/sh` itself.
    let selected = fs::canonicalize("/private/var/select/sh").ok()?;
    if !["/bin/bash", "/bin/dash", "/bin/zsh"]
        .iter()
        .any(|shell| selected == Path::new(shell))
    {
        return None;
    }
    let interpreter = subc_os::file_identity(&selected)?;
    Some(InterpreterIdentity {
        device: interpreter.device,
        inode: interpreter.inode,
    })
}

impl ExecutableIdentity {
    fn matches(self, running: subc_os::FileIdentity) -> bool {
        if self.device == running.device && self.inode == running.inode {
            return true;
        }
        #[cfg(target_os = "macos")]
        if self.macos_sh_interpreter.is_some_and(|interpreter| {
            interpreter.device == running.device && interpreter.inode == running.inode
        }) {
            return true;
        }
        false
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
    /// The running image's device and inode, read after exec at spawn. This
    /// includes PATH resolution and a script's interpreter. `None` if the image
    /// could not be read, and then the entry is never signalled.
    pub(crate) executable: Option<ExecutableIdentity>,
    /// The child's cgroup directory name, when it was placed in one (Linux).
    pub(crate) cgroup_name: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct RecordFile {
    version: u32,
    children: Vec<LiveChild>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    retained_reasons: BTreeMap<u32, RetainedReason>,
}

/// Replace the record at `path` with `children`.
#[cfg(test)]
pub(crate) fn write_record(path: &Path, children: &[LiveChild]) -> io::Result<()> {
    write_record_with_reasons(path, children, BTreeMap::new())
}

pub(crate) fn write_record_with_reasons(
    path: &Path,
    children: &[LiveChild],
    retained_reasons: BTreeMap<u32, RetainedReason>,
) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(&RecordFile {
        version: RECORD_VERSION,
        children: children.to_vec(),
        retained_reasons,
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
    read_record_with_reasons(path).map(|(children, _)| children)
}

pub(crate) fn read_record_with_reasons(
    path: &Path,
) -> Result<(Vec<LiveChild>, BTreeMap<u32, RetainedReason>), RecordReadError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok((Vec::new(), BTreeMap::new()))
        }
        Err(error) => return Err(RecordReadError::Io(error)),
    };
    let record: RecordFile = serde_json::from_slice(&bytes).map_err(RecordReadError::Malformed)?;
    if record.version != RECORD_VERSION {
        return Err(RecordReadError::UnknownVersion(record.version));
    }
    Ok((record.children, record.retained_reasons))
}

/// Write `bytes` to a new file beside `path`, then rename it over `path`.
///
/// Rename within one directory is atomic, so `path` always holds either the
/// previous complete record or the new one, never a prefix, whenever the
/// daemon dies. `before_rename` runs between the two steps; it exists so a
/// test can fail the write at that point. Sync the file before replacement and
/// the containing directory afterwards so successful admission is durable.
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
        .and_then(|()| replace_record(&temp, path, directory));
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn replace_record(temp: &Path, path: &Path, directory: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        let _ = directory;
        // The temp file's bytes were synced before this point. Plain rename
        // (MoveFileExW with replace) is used rather than a write-through
        // replace helper because it succeeds while another process holds the
        // destination open with the default share mode, which a concurrent
        // reader of this record does. Windows has no directory fsync.
        fs::rename(temp, path)
    }
    #[cfg(not(windows))]
    {
        fs::rename(temp, path)?;
        #[cfg(unix)]
        fs::File::open(directory)?.sync_all()?;
        Ok(())
    }
}

#[cfg(test)]
thread_local! {
    static FAIL_NEXT_FILE_SYNC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
pub(crate) fn fail_next_file_sync() {
    FAIL_NEXT_FILE_SYNC.set(true);
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
    file.write_all(bytes)?;
    #[cfg(test)]
    if FAIL_NEXT_FILE_SYNC.replace(false) {
        return Err(io::Error::other("injected live-children file sync failure"));
    }
    file.sync_all()
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
    #[cfg(target_os = "linux")]
    MatchesWithExecutablePermissionDenied,
    #[cfg(target_os = "linux")]
    ProcessHidden,
    PidDiffers,
    StartTimeUnrecorded,
    StartTimeDiffers,
    ExecutableUnrecorded,
    ExecutableUnreadable,
    ExecutableDiffers,
}

impl IdentityVerdict {
    #[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
    fn permits_signal(self) -> bool {
        match self {
            Self::Matches => true,
            #[cfg(target_os = "linux")]
            Self::MatchesWithExecutablePermissionDenied => true,
            _ => false,
        }
    }

    fn describe(self) -> &'static str {
        match self {
            Self::Matches => "matches",
            #[cfg(target_os = "linux")]
            Self::MatchesWithExecutablePermissionDenied => {
                "pid and start time match; executable not readable (non-dumpable)"
            }
            #[cfg(target_os = "linux")]
            Self::ProcessHidden => "process still exists but its procfs entry is hidden",
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

    fn refusal(self) -> SweepDecision {
        match self {
            Self::PidDiffers | Self::StartTimeDiffers | Self::ExecutableDiffers => {
                SweepDecision::Mismatched(self)
            }
            _ => SweepDecision::Unverifiable(self.describe().to_owned()),
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
    if !executable.matches(running) {
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
    #[cfg(target_os = "linux")]
    AliveButUnobservable,
    /// Matched; exited after SIGTERM.
    Terminated,
    /// Matched; still running at the SIGTERM grace, exited after SIGKILL.
    Killed,
    /// Windows forced stop, confirmed by waiting on the checked process handle.
    #[cfg(windows)]
    ForcedStopped,
    /// Matched and signalled, but still observed at the end of the bound.
    Survived,
    /// No process identity source on this platform; not signalled.
    #[cfg_attr(any(target_os = "linux", target_os = "macos"), allow(dead_code))]
    NotSignalledOnThisPlatform,
}

/// Why a previous instance still prevents starting its configured module.
/// Kept separately from identity evidence so it can never authorize a signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RetainedReason {
    Hidden,
    Unverifiable,
    Survived,
}

impl RetainedReason {
    pub(crate) fn waiting_message(self, pid: u32) -> String {
        let reason = match self {
            Self::Hidden => "is still running but hidden; it cannot be identified",
            Self::Unverifiable => "is still running but could not be verified",
            Self::Survived => "did not stop",
        };
        format!("waiting: previous instance (pid {pid}) {reason}")
    }
}

impl SweepDecision {
    fn retained_reason(&self) -> Option<RetainedReason> {
        match self {
            #[cfg(target_os = "linux")]
            Self::AliveButUnobservable => Some(RetainedReason::Hidden),
            Self::Unverifiable(_) => Some(RetainedReason::Unverifiable),
            Self::Survived => Some(RetainedReason::Survived),
            _ => None,
        }
    }
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

/// End every process the previous daemon's record lists that is still
/// exactly the process recorded, except those in `adopted`, then replace the
/// record with instances that could not be verified or stopped. Runs before any module is spawned.
///
/// The record is the one in the run directory `owner` locks. Requiring the
/// lock means the record cannot belong to a daemon that is still running:
/// such a daemon would hold the lock itself.
///
/// Every entry is logged with its decision. Returns the decisions for tests.
pub(crate) async fn sweep_orphans(
    owner: &RunDirLock,
    adopted: &AdoptedPids,
    bounds: SweepBounds,
) -> Vec<(LiveChild, SweepDecision)> {
    sweep_orphans_with(owner, adopted, bounds, sweep_entries).await
}

async fn sweep_orphans_with<'a, F, Fut>(
    owner: &RunDirLock,
    adopted: &'a AdoptedPids,
    bounds: SweepBounds,
    sweep: F,
) -> Vec<(LiveChild, SweepDecision)>
where
    F: FnOnce(Vec<LiveChild>, &'a AdoptedPids, SweepBounds) -> Fut,
    Fut: std::future::Future<Output = Vec<(LiveChild, SweepDecision)>>,
{
    let path = owner.live_children_record();
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
    let decisions = sweep(entries, adopted, bounds).await;
    for (entry, decision) in &decisions {
        log_decision(entry, decision);
    }
    let retained: Vec<_> = decisions
        .iter()
        .filter(|(_, decision)| decision.retained_reason().is_some())
        .map(|(entry, _)| entry.clone())
        .collect();
    let reasons = decisions
        .iter()
        .filter_map(|(entry, decision)| {
            decision.retained_reason().map(|reason| (entry.pid, reason))
        })
        .collect();
    if let Err(error) = write_record_with_reasons(path, &retained, reasons) {
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
    sweep_entries_with_verdict(entries, adopted, bounds, current_verdict).await
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn sweep_entries_with_verdict(
    entries: Vec<LiveChild>,
    adopted: &AdoptedPids,
    bounds: SweepBounds,
    verdict_for: impl Fn(&LiveChild, &subc_os::Process) -> Option<IdentityVerdict>,
) -> Vec<(LiveChild, SweepDecision)> {
    sweep_entries_with_hooks(entries, adopted, bounds, verdict_for, |process, signal| {
        process.signal(signal)
    })
    .await
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn sweep_entries_with_hooks(
    entries: Vec<LiveChild>,
    adopted: &AdoptedPids,
    bounds: SweepBounds,
    verdict_for: impl Fn(&LiveChild, &subc_os::Process) -> Option<IdentityVerdict>,
    signal: impl Fn(&subc_os::Process, subc_os::Signal) -> io::Result<bool>,
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
        match verdict_for(&entry, &process) {
            None => decisions.push((entry, SweepDecision::Gone)),
            #[cfg(target_os = "linux")]
            Some(IdentityVerdict::ProcessHidden) => {
                decisions.push((entry, SweepDecision::AliveButUnobservable))
            }
            Some(verdict) if verdict.permits_signal() => {
                #[cfg(target_os = "linux")]
                if verdict == IdentityVerdict::MatchesWithExecutablePermissionDenied {
                    info!(
                        module_id = %entry.module_id,
                        pid = entry.pid,
                        pidfd = process.signals_through_pidfd(),
                        "orphan sweep: pid and start time match; executable not readable (non-dumpable); sending SIGTERM"
                    );
                } else {
                    info!(
                        module_id = %entry.module_id,
                        pid = entry.pid,
                        pidfd = process.signals_through_pidfd(),
                        "orphan sweep: previous daemon's child still running and matches pid, start time and executable; sending SIGTERM"
                    );
                }
                #[cfg(not(target_os = "linux"))]
                info!(
                    module_id = %entry.module_id,
                    pid = entry.pid,
                    pidfd = process.signals_through_pidfd(),
                    "orphan sweep: previous daemon's child still running and matches pid, start time and executable; sending SIGTERM"
                );
                match signal(&process, Signal::Terminate) {
                    Ok(true) => signalled.push((entry, process)),
                    // It exited between the check and the signal.
                    Ok(false) => decisions.push((entry, SweepDecision::Gone)),
                    Err(error) => {
                        decisions.push((entry, SweepDecision::Unverifiable(error.to_string())))
                    }
                }
            }
            Some(verdict) => decisions.push((entry, verdict.refusal())),
        }
    }

    let departed = |entry: &LiveChild, process: &Process, exited| {
        let verdict = verdict_for(entry, process);
        #[cfg(target_os = "linux")]
        if verdict == Some(IdentityVerdict::ProcessHidden) {
            return Some(SweepDecision::AliveButUnobservable);
        }
        // This process was verified before it was signalled, so a positive
        // mismatch now means the pid has gone (and may already be reused). An
        // unreadable executable is usually the process mid-exit; keep polling
        // rather than recording a verified, signalled child as unverifiable.
        // Whatever is still present at the kill deadline is Survived.
        match verdict {
            None => Some(exited),
            Some(verdict) if verdict.permits_signal() => None,
            Some(verdict) => match verdict.refusal() {
                SweepDecision::Mismatched(_) => Some(exited),
                _ => None,
            },
        }
    };
    let term_deadline = Instant::now() + bounds.term_grace;
    loop {
        signalled.retain(|(entry, process)| {
            if let Some(decision) = departed(entry, process, SweepDecision::Terminated) {
                decisions.push((entry.clone(), decision));
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
        if !verdict_for(entry, process).is_some_and(IdentityVerdict::permits_signal) {
            continue;
        }
        warn!(
            module_id = %entry.module_id,
            pid = entry.pid,
            grace_ms = bounds.term_grace.as_millis() as u64,
            "orphan sweep: previous daemon's child still running after SIGTERM grace; sending SIGKILL"
        );
        let _ = signal(process, Signal::Kill);
    }
    let kill_deadline = Instant::now() + bounds.kill_bound;
    loop {
        signalled.retain(|(entry, process)| {
            if let Some(decision) = departed(entry, process, SweepDecision::Killed) {
                decisions.push((entry.clone(), decision));
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
    let Some((observed, access)) = process.observe_with_executable_access() else {
        #[cfg(target_os = "linux")]
        if process.is_alive_but_unobservable() {
            return Some(IdentityVerdict::ProcessHidden);
        }
        return None;
    };
    Some(verdict_with_executable_access(
        entry,
        process.pid(),
        &observed,
        access,
    ))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn verdict_with_executable_access(
    entry: &LiveChild,
    pid: u32,
    observed: &subc_os::Observation,
    _access: subc_os::ExecutableAccess,
) -> IdentityVerdict {
    let verdict = identity_verdict(entry, pid, observed);
    // An ExecutableUnreadable verdict already confirms the recorded PID and
    // Linux start time, ruling out PID reuse. Only permission denial permits
    // signalling without the image; other read failures remain unverifiable.
    #[cfg(target_os = "linux")]
    if verdict == IdentityVerdict::ExecutableUnreadable
        && _access == subc_os::ExecutableAccess::PermissionDenied
    {
        return IdentityVerdict::MatchesWithExecutablePermissionDenied;
    }
    verdict
}

#[cfg(windows)]
async fn sweep_entries(
    entries: Vec<LiveChild>,
    adopted: &AdoptedPids,
    bounds: SweepBounds,
) -> Vec<(LiveChild, SweepDecision)> {
    let mut decisions = Vec::new();
    for entry in entries {
        let decision = if adopted.contains(entry.pid) {
            SweepDecision::Adopted
        } else {
            let recorded = entry.clone();
            tokio::task::spawn_blocking(move || sweep_windows_entry(&recorded, bounds.kill_bound))
                .await
                .unwrap_or_else(|error| SweepDecision::Unverifiable(error.to_string()))
        };
        decisions.push((entry, decision));
    }
    decisions
}

#[cfg(windows)]
fn sweep_windows_entry(entry: &LiveChild, wait_bound: Duration) -> SweepDecision {
    let Some(expected) = entry.start_time else {
        return IdentityVerdict::StartTimeUnrecorded.refusal();
    };
    // Open once: creation-time validation, the forced stop and its wait all use
    // this handle even if the PID is subsequently reused.
    let process = match subc_os::Process::open(entry.pid) {
        Ok(Some(process)) => process,
        Ok(None) => return SweepDecision::Gone,
        Err(error) => return SweepDecision::Unverifiable(error.to_string()),
    };
    let Some(observed) = process.observe() else {
        return match process.wait_for_exit(Duration::ZERO) {
            Ok(true) => SweepDecision::Gone,
            _ => SweepDecision::Unverifiable("process creation time unavailable".to_owned()),
        };
    };
    if observed.start_time != expected {
        return SweepDecision::Mismatched(IdentityVerdict::StartTimeDiffers);
    }
    match process.force_stop(expected, wait_bound) {
        Ok(true) => SweepDecision::ForcedStopped,
        Ok(false) => SweepDecision::Survived,
        Err(error) => SweepDecision::Unverifiable(error.to_string()),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
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
            "orphan sweep: could not verify or stop the recorded pid; keeping its record and blocking its module"
        ),
        #[cfg(target_os = "linux")]
        SweepDecision::AliveButUnobservable => info!(
            module_id, pid,
            "orphan sweep: previous child is alive but unobservable (process hidden); not signalled; keeping its record"
        ),
        SweepDecision::Terminated => {
            info!(module_id, pid, "orphan sweep: previous daemon's child exited after SIGTERM")
        }
        SweepDecision::Killed => {
            info!(module_id, pid, "orphan sweep: previous daemon's child exited after SIGKILL")
        }
        #[cfg(windows)]
        SweepDecision::ForcedStopped => {
            info!(module_id, pid, "orphan sweep: previous daemon's child exited after a forced stop")
        }
        SweepDecision::Survived => warn!(
            module_id,
            pid,
            "orphan sweep: previous daemon's child still observed after SIGKILL; keeping its record and blocking its module"
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
pub(crate) mod tests {
    use super::*;
    use cortexkit_test_support::ScratchDir;

    #[cfg(target_os = "linux")]
    pub(crate) struct NonDumpableChild {
        pub(crate) child: std::process::Child,
        pub(crate) entry: LiveChild,
        pub(crate) program: PathBuf,
    }

    #[cfg(target_os = "linux")]
    impl Drop for NonDumpableChild {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    #[cfg(target_os = "linux")]
    pub(crate) async fn spawn_non_dumpable_child(dir: &Path) -> NonDumpableChild {
        let ready = dir.join("ready");
        let program = std::env::current_exe().unwrap();
        let child = std::process::Command::new(&program)
            .args([
                "--exact",
                "live_children::tests::linux_non_dumpable_child_is_swept_and_image_is_not_readable",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("SUBC_NON_DUMPABLE_CHILD_READY", &ready)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let mut fixture = NonDumpableChild {
            entry: LiveChild {
                module_id: "previous".to_owned(),
                pid: child.id(),
                protocol: ModuleProtocol::None,
                start_time: None,
                executable: subc_os::file_identity(&program).map(ExecutableIdentity::from),
                cgroup_name: None,
            },
            child,
            program,
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let observed = subc_os::Process::open(fixture.child.id())
                .unwrap()
                .and_then(|process| process.observe());
            if let Some(observed) = observed.filter(|observed| {
                observed.executable.map(ExecutableIdentity::from) == fixture.entry.executable
            }) {
                fixture.entry.start_time = Some(observed.start_time);
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "helper never completed exec"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        // Capture the executable inode and process start time before the child
        // calls PR_SET_DUMPABLE, which can hide both from this parent.
        std::io::Write::write_all(fixture.child.stdin.as_mut().unwrap(), b"x").unwrap();
        while !ready.exists() {
            assert!(
                fixture.child.try_wait().unwrap().is_none(),
                "helper exited before readiness"
            );
            assert!(
                std::time::Instant::now() < deadline,
                "helper never became non-dumpable"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        fixture
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "requires an unprivileged Linux process with procfs hidepid=0"]
    async fn linux_non_dumpable_child_is_swept_and_image_is_not_readable() {
        if let Some(ready) = std::env::var_os("SUBC_NON_DUMPABLE_CHILD_READY") {
            let mut release = [0u8; 1];
            std::io::Read::read_exact(&mut std::io::stdin(), &mut release).unwrap();
            rustix::process::set_dumpable_behavior(rustix::process::DumpableBehavior::NotDumpable)
                .unwrap();
            std::fs::write(ready, b"ready").unwrap();
            std::io::Read::read_exact(&mut std::io::stdin(), &mut release).unwrap();
            return;
        }
        let dir = ScratchDir::new("non-dumpable-sweep");
        let mut fixture = spawn_non_dumpable_child(&dir).await;
        let pid = fixture.child.id();
        let program = &fixture.program;
        let entry = fixture.entry.clone();
        let process = subc_os::Process::open(pid).unwrap().unwrap();
        let (observed, access) = process.observe_with_executable_access().unwrap_or_else(|| {
            panic!(
                "non-dumpable child must remain observable: stat={:?}, status={:?}, wait={:?}",
                std::fs::read_to_string(format!("/proc/{pid}/stat")),
                std::fs::read_to_string(format!("/proc/{pid}/status")),
                fixture.child.try_wait()
            );
        });
        assert_eq!(access, subc_os::ExecutableAccess::PermissionDenied);
        assert_eq!(observed.executable, None);
        assert_eq!(process.observe(), Some(observed));
        let error = std::fs::File::open(format!("/proc/{pid}/exe")).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(std::fs::read_to_string(format!("/proc/{pid}/status")).is_ok());

        assert!(entry.executable.is_some());
        assert_eq!(
            current_verdict(&entry, &process),
            Some(IdentityVerdict::MatchesWithExecutablePermissionDenied)
        );
        let mut reused = entry.clone();
        reused.start_time = Some(observed.start_time + 1);
        assert_eq!(
            current_verdict(&reused, &process),
            Some(IdentityVerdict::StartTimeDiffers)
        );
        reused.start_time = None;
        assert_eq!(
            current_verdict(&reused, &process),
            Some(IdentityVerdict::StartTimeUnrecorded)
        );
        reused = entry.clone();
        reused.pid += 1;
        assert_eq!(
            current_verdict(&reused, &process),
            Some(IdentityVerdict::PidDiffers)
        );
        reused = entry.clone();
        reused.executable = None;
        assert_eq!(
            current_verdict(&reused, &process),
            Some(IdentityVerdict::ExecutableUnrecorded)
        );

        let probe = crate::provenance::ExecutableIdentityProbe::default();
        assert_eq!(
            probe
                .observe(Some(pid), Some(program), None, Some(observed.start_time))
                .await,
            subc_control::RunningImageAgreement::Unavailable {
                reason:
                    subc_control::RunningImageUnavailableReason::RunningExecutablePermissionDenied,
            }
        );
        assert_eq!(
            probe
                .observe(
                    Some(pid),
                    Some(program),
                    None,
                    Some(observed.start_time + 1)
                )
                .await,
            subc_control::RunningImageAgreement::Unavailable {
                reason: subc_control::RunningImageUnavailableReason::ProcessIdentityUnconfirmed,
            }
        );

        let path = record_path(&dir);
        write_record(&path, std::slice::from_ref(&entry)).unwrap();
        let decisions = sweep_orphans(
            &RunDirLock::acquire(&path).unwrap(),
            &AdoptedPids::none(),
            SweepBounds {
                term_grace: Duration::from_millis(300),
                kill_bound: Duration::from_secs(2),
            },
        )
        .await;
        assert_eq!(decisions, vec![(entry, SweepDecision::Terminated)]);
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(fixture.child.wait().unwrap().signal(), Some(15));
        assert!(read_record(&path).unwrap().is_empty());
    }

    #[cfg(target_os = "linux")]
    mod hidden_processes {
        use super::*;
        use crate::registry::Registry;
        use crate::supervise::{ModuleOverlap, ModuleSpec, RestartPolicy, Supervisor};
        use std::sync::Arc;

        fn spec(module_id: &str) -> ModuleSpec {
            ModuleSpec {
                module_id: module_id.to_owned(),
                program: PathBuf::from("/bin/sleep"),
                args: vec!["60".to_owned()],
                env: vec![],
                reserved: false,
                reserved_prefixes: vec![],
                protocol: ModuleProtocol::None,
                overlap: ModuleOverlap::Exclusive,
            }
        }

        async fn retained_fixture(dir: &Path) -> NonDumpableChild {
            let fixture = spawn_non_dumpable_child(dir).await;
            assert!(
                subc_os::Process::open(fixture.entry.pid)
                    .unwrap()
                    .unwrap()
                    .is_alive_but_unobservable(),
                "this gate requires procfs hidepid=invisible"
            );
            let path = record_path(dir);
            write_record(&path, std::slice::from_ref(&fixture.entry)).unwrap();
            sweep_orphans(
                &RunDirLock::acquire(&path).unwrap(),
                &AdoptedPids::none(),
                SweepBounds::default(),
            )
            .await;
            fixture
        }

        #[tokio::test]
        #[ignore = "requires Linux procfs hidepid=invisible"]
        async fn linux_hidden_non_dumpable_child_is_retained_and_image_is_hidden() {
            let dir = ScratchDir::new("hidden-sweep");
            let mut fixture = spawn_non_dumpable_child(&dir).await;
            let process = subc_os::Process::open(fixture.entry.pid).unwrap().unwrap();
            assert!(process.is_alive_but_unobservable());
            rustix::process::test_kill_process(
                rustix::process::Pid::from_raw(fixture.entry.pid as i32).unwrap(),
            )
            .unwrap();
            assert_eq!(
                std::fs::metadata(format!("/proc/{}/stat", fixture.entry.pid))
                    .unwrap_err()
                    .kind(),
                std::io::ErrorKind::NotFound
            );
            assert!(process.observe().is_none());
            let identity = subc_os::process_identity::ProcessStart::parse(&format!(
                "linux-v1-{}",
                fixture.entry.start_time.unwrap()
            ))
            .unwrap();
            assert_eq!(
                subc_os::process_identity::liveness(fixture.entry.pid, Some(&identity)),
                subc_os::process_identity::Liveness::Unknown
            );
            let image = crate::provenance::ExecutableIdentityProbe::default()
                .observe(
                    Some(fixture.entry.pid),
                    Some(&fixture.program),
                    None,
                    fixture.entry.start_time,
                )
                .await;
            assert_eq!(
                image,
                subc_control::RunningImageAgreement::Unavailable {
                    reason: subc_control::RunningImageUnavailableReason::ProcessHidden,
                }
            );
            let path = record_path(&dir);
            write_record(&path, std::slice::from_ref(&fixture.entry)).unwrap();
            let decisions = sweep_orphans(
                &RunDirLock::acquire(&path).unwrap(),
                &AdoptedPids::none(),
                SweepBounds::default(),
            )
            .await;
            assert_eq!(
                decisions,
                vec![(fixture.entry.clone(), SweepDecision::AliveButUnobservable)]
            );
            assert_eq!(read_record(&path).unwrap(), vec![fixture.entry.clone()]);
            assert!(
                fixture.child.try_wait().unwrap().is_none(),
                "hidden child must never be signalled"
            );
        }

        #[tokio::test]
        #[ignore = "requires Linux procfs hidepid=invisible"]
        async fn linux_hidden_orphan_blocks_only_its_module_and_refuses_start_requests() {
            let dir = ScratchDir::new("hidden-blocks-one");
            let mut fixture = retained_fixture(&dir).await;
            let supervisor =
                Supervisor::new(Arc::new(Registry::default()), RestartPolicy::default())
                    .with_live_children_record(record_path(&dir));
            let waiting = supervisor
                .supervise_configured_with_health(
                    spec("previous"),
                    true,
                    crate::supervise::HealthConfig::default(),
                    None,
                    RestartPolicy::default(),
                )
                .unwrap();
            let other = supervisor
                .supervise_configured(spec("unrelated"), true)
                .unwrap();
            let other_live = other.status().unwrap().live;
            let refused = waiting.set_enabled(true).await.unwrap_err().to_string();
            assert!(refused.contains(&format!("waiting: previous instance (pid {}) is still running but hidden; it cannot be identified", fixture.entry.pid)));
            assert!(waiting.restart(None).await.is_err());
            assert!(waiting.reload().await.is_err());
            assert!(waiting.swap(None).await.is_err());
            assert!(supervisor.spawn(spec("previous")).is_err());
            tokio::time::sleep(Duration::from_millis(5200)).await;
            let status = waiting.status().unwrap();
            let orphan_alive = fixture.child.try_wait().unwrap().is_none();
            waiting.stop().await.unwrap();
            other.stop().await.unwrap();
            assert!(
                other_live,
                "an unrelated configured module must start normally"
            );
            assert!(!status.process_alive && status.pid.is_none());
            assert_eq!(status.waiting_for_previous_pid, Some(fixture.entry.pid));
            assert_eq!(status.spawn_generation, 0);
            assert!(
                orphan_alive,
                "blocking must never signal the previous instance"
            );
        }

        #[tokio::test]
        #[ignore = "requires Linux procfs hidepid=invisible"]
        async fn linux_hidden_orphan_module_starts_after_previous_pid_exits() {
            let dir = ScratchDir::new("hidden-starts-after-exit");
            let mut fixture = retained_fixture(&dir).await;
            let supervisor =
                Supervisor::new(Arc::new(Registry::default()), RestartPolicy::default())
                    .with_live_children_record(record_path(&dir));
            let waiting = supervisor
                .supervise_configured(spec("previous"), true)
                .unwrap();
            assert_eq!(
                waiting.status().unwrap().waiting_for_previous_pid,
                Some(fixture.entry.pid)
            );
            fixture.child.kill().unwrap();
            fixture.child.wait().unwrap();
            let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
            let status = loop {
                let status = waiting.status().unwrap();
                if status.process_alive {
                    break status;
                }
                if tokio::time::Instant::now() >= deadline {
                    waiting.stop().await.unwrap();
                    panic!("module never started after the orphan exited");
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            };
            let record = read_record(&record_path(&dir)).unwrap();
            waiting.stop().await.unwrap();
            assert_eq!(status.waiting_for_previous_pid, None);
            assert_eq!(status.restart_count, 0);
            assert_eq!(status.lifetime_restarts, 0);
            assert_eq!(status.spawn_generation, 1);
            assert_eq!(record.len(), 1);
            assert_eq!(record[0].pid, status.pid.unwrap());
            assert_ne!(record[0].pid, fixture.entry.pid);
        }

        #[tokio::test]
        #[ignore = "requires Linux procfs hidepid=invisible"]
        async fn linux_hidden_orphan_record_survives_roster_rewrites() {
            let dir = ScratchDir::new("hidden-record-rewrite");
            let mut fixture = retained_fixture(&dir).await;
            let path = record_path(&dir);
            let roster = crate::child_roster::ChildRoster::default();
            roster.record_to(path.clone());
            let guard = roster.admit(
                "unrelated".to_owned(),
                std::process::id(),
                ModuleProtocol::None,
                None,
                crate::child_roster::RecordedIdentity::default(),
            );
            assert!(read_record(&path).unwrap().contains(&fixture.entry));
            drop(guard);
            assert_eq!(read_record(&path).unwrap(), vec![fixture.entry.clone()]);
            fixture.child.kill().unwrap();
            fixture.child.wait().unwrap();
            assert_eq!(roster.previous_instance_pid("previous"), None);
            assert!(read_record(&path).unwrap().is_empty());
        }
    }

    fn child(module_id: &str, pid: u32) -> LiveChild {
        LiveChild {
            module_id: module_id.to_owned(),
            pid,
            protocol: ModuleProtocol::None,
            start_time: Some(1_000),
            executable: Some(ExecutableIdentity {
                device: 7,
                inode: 11,
                macos_sh_interpreter: None,
                windows_file_identity: None,
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

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_sh_reexec_matches_the_interpreter_recorded_at_launch() {
        // A `#!/bin/sh` child is first seen as `/bin/sh` and then, after the
        // re-exec, as the selected shell. Build both observations from the
        // real files instead of trying to catch the brief `/bin/sh` phase,
        // which some macOS versions don't expose to a sampler.
        let launcher = subc_os::file_identity(Path::new("/bin/sh")).unwrap();
        let interpreter = subc_os::file_identity(Path::new("/private/var/select/sh")).unwrap();
        assert_ne!(
            launcher, interpreter,
            "the fixture must model a real image change"
        );
        let mut entry = child("sh-script", 40);
        entry.executable = Some(ExecutableIdentity::from(launcher));
        let dir = ScratchDir::new("sh-interpreter-record");
        let path = record_path(&dir);
        write_record(&path, &[entry]).unwrap();
        let recorded = read_record(&path).unwrap().pop().unwrap();
        let after_reexec = observed(1_000, interpreter.device, interpreter.inode);
        assert_eq!(
            identity_verdict(&recorded, 40, &after_reexec),
            IdentityVerdict::Matches
        );
        assert_eq!(
            identity_verdict(&recorded, 41, &after_reexec),
            IdentityVerdict::PidDiffers
        );
        assert_eq!(
            identity_verdict(
                &recorded,
                40,
                &observed(1_001, interpreter.device, interpreter.inode)
            ),
            IdentityVerdict::StartTimeDiffers
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_sh_reexec_refuses_an_unrelated_system_binary() {
        let launcher = subc_os::file_identity(Path::new("/bin/sh")).unwrap();
        let unrelated = subc_os::file_identity(Path::new("/usr/bin/true")).unwrap();
        let interpreter = subc_os::file_identity(Path::new("/private/var/select/sh")).unwrap();
        let mut entry = child("sh-script", 40);
        entry.executable = Some(ExecutableIdentity::from(launcher));
        assert_eq!(
            identity_verdict(
                &entry,
                40,
                &observed(1_000, unrelated.device, unrelated.inode)
            ),
            IdentityVerdict::ExecutableDiffers
        );
        let ordinary = ExecutableIdentity::from(unrelated);
        assert!(ordinary.macos_sh_interpreter.is_none());
        entry.executable = Some(ordinary);
        assert_eq!(
            identity_verdict(
                &entry,
                40,
                &observed(1_000, interpreter.device, interpreter.inode)
            ),
            IdentityVerdict::ExecutableDiffers
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_sh_legacy_record_does_not_infer_an_unrecorded_interpreter() {
        let launcher = subc_os::file_identity(Path::new("/bin/sh")).unwrap();
        let interpreter = subc_os::file_identity(Path::new("/private/var/select/sh")).unwrap();
        let mut entry = child("legacy-sh-script", 40);
        // The old shape lacks the interpreter field, even on a host where the
        // current shell selection would match. Matching must use recorded facts.
        entry.executable = Some(
            serde_json::from_value(serde_json::json!({
                "device": launcher.device, "inode": launcher.inode,
            }))
            .unwrap(),
        );
        assert_eq!(
            identity_verdict(
                &entry,
                40,
                &observed(1_000, interpreter.device, interpreter.inode)
            ),
            IdentityVerdict::ExecutableDiffers
        );
        assert_eq!(
            identity_verdict(
                &entry,
                40,
                &observed(1_000, launcher.device, launcher.inode)
            ),
            IdentityVerdict::Matches
        );
    }

    #[test]
    fn a_roster_with_a_recorded_sh_interpreter_is_readable_by_the_previous_shape() {
        // These are the complete version-1 shapes used by the previous daemon.
        // Its serde deserializer did not deny unknown fields at either level.
        #[derive(Debug, Deserialize, PartialEq, Eq)]
        struct PreviousExecutableIdentity {
            device: u64,
            inode: u64,
        }
        #[derive(Debug, Deserialize, PartialEq, Eq)]
        struct PreviousLiveChild {
            module_id: String,
            pid: u32,
            protocol: ModuleProtocol,
            start_time: Option<u64>,
            executable: Option<PreviousExecutableIdentity>,
            cgroup_name: Option<String>,
        }
        #[derive(Debug, Deserialize, PartialEq, Eq)]
        struct PreviousRecordFile {
            version: u32,
            children: Vec<PreviousLiveChild>,
        }

        let mut entry = child("sh-script", 40);
        entry.executable.as_mut().unwrap().macos_sh_interpreter = Some(InterpreterIdentity {
            device: 7,
            inode: 12,
        });
        let dir = ScratchDir::new("previous-roster-reader");
        let path = record_path(&dir);
        write_record(&path, &[entry]).unwrap();
        let bytes = fs::read(&path).unwrap();
        let new: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            new["children"][0]["executable"]["macos_sh_interpreter"]["inode"],
            12
        );
        let previous: PreviousRecordFile = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            previous,
            PreviousRecordFile {
                version: 1,
                children: vec![PreviousLiveChild {
                    module_id: "sh-script".to_owned(),
                    pid: 40,
                    protocol: ModuleProtocol::None,
                    start_time: Some(1_000),
                    executable: Some(PreviousExecutableIdentity {
                        device: 7,
                        inode: 11
                    }),
                    cgroup_name: Some("sh-script-a".to_owned()),
                }],
            }
        );
    }

    #[test]
    fn record_round_trips() {
        let dir = ScratchDir::new("live-children-round-trip");
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
    fn windows_file_id_record_is_full_width_and_readable_by_rollback_reader() {
        #[derive(Deserialize)]
        struct PreviousImage {
            device: u64,
            inode: u64,
        }
        #[derive(Deserialize)]
        struct PreviousChild {
            module_id: String,
            pid: u32,
            protocol: ModuleProtocol,
            start_time: Option<u64>,
            executable: Option<PreviousImage>,
            cgroup_name: Option<String>,
        }
        #[derive(Deserialize)]
        struct PreviousRecord {
            version: u32,
            children: Vec<PreviousChild>,
        }
        let dir = ScratchDir::new("windows-file-id-rollback");
        let path = record_path(&dir);
        let mut entry = child("windows-child", 40);
        let image = entry.executable.as_mut().unwrap();
        image.device = 0;
        image.inode = 0;
        image.windows_file_identity = Some(WindowsFileIdentity {
            volume_serial_number: u64::MAX,
            file_id: [
                0, 1, 2, 3, 4, 5, 6, 7, 128, 129, 130, 131, 252, 253, 254, 255,
            ],
        });
        write_record(&path, std::slice::from_ref(&entry)).unwrap();
        assert_eq!(read_record(&path).unwrap(), vec![entry]);
        let previous: PreviousRecord = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(previous.version, 1);
        let previous = &previous.children[0];
        assert_eq!(previous.module_id, "windows-child");
        assert_eq!(previous.pid, 40);
        assert_eq!(previous.protocol, ModuleProtocol::None);
        assert_eq!(previous.start_time, Some(1_000));
        assert_eq!(previous.cgroup_name.as_deref(), Some("windows-child-a"));
        assert_eq!(previous.executable.as_ref().unwrap().device, 0);
        assert_eq!(previous.executable.as_ref().unwrap().inode, 0);
    }

    #[test]
    fn unix_record_without_windows_identity_keeps_original_bytes() {
        let dir = ScratchDir::new("unix-record-bytes");
        let path = record_path(&dir);
        write_record(&path, &[child("nats", 40)]).unwrap();
        assert_eq!(fs::read_to_string(path).unwrap(), "{\n  \"version\": 1,\n  \"children\": [\n    {\n      \"module_id\": \"nats\",\n      \"pid\": 40,\n      \"protocol\": \"none\",\n      \"start_time\": 1000,\n      \"executable\": {\n        \"device\": 7,\n        \"inode\": 11\n      },\n      \"cgroup_name\": \"nats-a\"\n    }\n  ]\n}");
    }

    #[cfg(windows)]
    mod windows_processes {
        use super::*;
        use std::os::windows::process::CommandExt;

        struct ChildGuard(std::process::Child);
        impl Drop for ChildGuard {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }

        fn spawn_recorded() -> (ChildGuard, LiveChild) {
            // Suspended test executable: a real process identity without running
            // another test suite or depending on an installed helper program.
            let child = std::process::Command::new(std::env::current_exe().unwrap())
                .creation_flags(0x4)
                .spawn()
                .unwrap();
            let entry = LiveChild {
                module_id: "previous".to_owned(),
                pid: child.id(),
                protocol: ModuleProtocol::None,
                start_time: subc_os::start_time(child.id()),
                executable: None,
                cgroup_name: None,
            };
            assert!(entry.start_time.is_some());
            (ChildGuard(child), entry)
        }

        #[tokio::test]
        async fn windows_orphan_sweep_leaves_different_creation_time_alone() {
            let (mut child, mut entry) = spawn_recorded();
            entry.start_time = entry.start_time.map(|time| time + 1);
            let dir = ScratchDir::new("windows-sweep-reused-pid");
            let path = record_path(&dir);
            write_record(&path, std::slice::from_ref(&entry)).unwrap();
            let owner = RunDirLock::acquire(&path).unwrap();
            let decisions =
                sweep_orphans(&owner, &AdoptedPids::none(), SweepBounds::default()).await;
            assert_eq!(
                decisions,
                vec![(
                    entry,
                    SweepDecision::Mismatched(IdentityVerdict::StartTimeDiffers)
                )]
            );
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "unrelated process was terminated"
            );
        }

        #[tokio::test]
        async fn windows_orphan_sweep_force_stops_matching_creation_time() {
            let (mut child, entry) = spawn_recorded();
            let dir = ScratchDir::new("windows-sweep-matched");
            let path = record_path(&dir);
            write_record(&path, std::slice::from_ref(&entry)).unwrap();
            let owner = RunDirLock::acquire(&path).unwrap();
            let decisions =
                sweep_orphans(&owner, &AdoptedPids::none(), SweepBounds::default()).await;
            assert_eq!(decisions, vec![(entry, SweepDecision::ForcedStopped)]);
            assert!(
                child.0.try_wait().unwrap().is_some(),
                "sweep did not await forced exit"
            );
        }
    }

    #[test]
    fn a_missing_record_is_empty_and_an_unknown_version_is_refused() {
        let dir = ScratchDir::new("live-children-missing");
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
        let dir = ScratchDir::new("live-children-failed-write");
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
        let dir = ScratchDir::new("live-children-concurrent");
        let path = record_path(&dir);
        let short = vec![child("nats", 40)];
        let long: Vec<LiveChild> = (0..200).map(|pid| child("module", pid)).collect();
        write_record(&path, &short).unwrap();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let rounds = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let writer = {
            let (path, stop, rounds, short, long) = (
                path.clone(),
                stop.clone(),
                rounds.clone(),
                short.clone(),
                long.clone(),
            );
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
                    rounds.store(round, Ordering::Relaxed);
                }
            })
        };
        // Keep reading until the writer has really overlapped the reads. A
        // fixed number of reads can finish before a busy machine schedules the
        // writer at all, which proves nothing about torn records and used to
        // fail the "writer ran" check instead.
        const MIN_READS: usize = 2_000;
        const MIN_WRITER_ROUNDS: usize = 50;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        let mut reads = 0usize;
        while reads < MIN_READS || rounds.load(Ordering::Relaxed) < MIN_WRITER_ROUNDS {
            assert!(
                std::time::Instant::now() < deadline,
                "writer completed only {} rounds in 60 s",
                rounds.load(Ordering::Relaxed)
            );
            let read = read_record(&path).expect("every read sees a complete record");
            assert!(
                read == short || read == long,
                "read a torn record: {read:?}"
            );
            reads += 1;
        }
        stop.store(true, Ordering::Relaxed);
        writer.join().unwrap();
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
        let dir = ScratchDir::new("roster-record");
        let path = record_path(&dir);
        let roster = crate::child_roster::ChildRoster::default();
        roster.record_to(path.clone());
        let recorded = crate::child_roster::RecordedIdentity {
            start_time: Some(5),
            executable: Some(ExecutableIdentity {
                device: 1,
                inode: 2,
                macos_sh_interpreter: None,
                windows_file_identity: None,
            }),
            cgroup_name: Some("nats-a".to_owned()),
            #[cfg(target_os = "linux")]
            cgroup_placement: None,
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
            let dir = ScratchDir::new("sweep-terminates");
            let path = record_path(&dir);
            let (mut child, entry) = spawn_recorded(&executable("sleep"), &["60"]);
            write_record(&path, std::slice::from_ref(&entry)).unwrap();

            let decisions = sweep_orphans(
                &RunDirLock::acquire(&path).unwrap(),
                &AdoptedPids::none(),
                quick(),
            )
            .await;

            assert_eq!(decisions, vec![(entry, SweepDecision::Terminated)]);
            let status = child.wait().unwrap();
            assert_eq!(status.signal(), Some(15), "ended by SIGTERM: {status}");
            assert_eq!(read_record(&path).unwrap(), Vec::new());
        }

        #[cfg(target_os = "linux")]
        #[tokio::test]
        async fn linux_child_with_other_executable_read_failure_is_not_signalled() {
            let (mut child, entry) = spawn_recorded(&executable("sleep"), &["60"]);
            // Inject an unavailable image into a live child's observation. Unlike
            // EACCES/EPERM, an ENOENT or I/O failure must still prevent signalling
            // even when the recorded PID and start time match.
            let decisions = sweep_entries_with_verdict(
                vec![entry.clone()],
                &AdoptedPids::none(),
                quick(),
                |entry, process| {
                    let mut observed = process.observe().unwrap();
                    observed.executable = None;
                    Some(verdict_with_executable_access(
                        entry,
                        process.pid(),
                        &observed,
                        subc_os::ExecutableAccess::Unavailable,
                    ))
                },
            )
            .await;
            let alive = still_running(&mut child);
            child.kill().unwrap();
            child.wait().unwrap();
            assert_eq!(
                decisions,
                vec![(
                    entry,
                    SweepDecision::Unverifiable(
                        IdentityVerdict::ExecutableUnreadable.describe().to_owned()
                    )
                )]
            );
            assert!(alive, "the sweep must leave the unreadable child running");
        }

        #[cfg(target_os = "linux")]
        async fn retained_orphan_blocks_only_its_module(survived: bool) {
            use crate::supervise::{ModuleOverlap, ModuleSpec, RestartPolicy, Supervisor};
            use std::sync::Arc;

            let dir = ScratchDir::new("unverified-orphan-blocks-one");
            let path = record_path(&dir);
            let program = executable("sleep");
            let (child, entry) = spawn_recorded(&program, &["60"]);
            let mut fixture = NonDumpableChild {
                child,
                entry,
                program: program.clone(),
            };
            write_record(&path, std::slice::from_ref(&fixture.entry)).unwrap();
            let owner = RunDirLock::acquire(&path).unwrap();
            let decisions = sweep_orphans_with(
                &owner,
                &AdoptedPids::none(),
                SweepBounds {
                    term_grace: Duration::ZERO,
                    kill_bound: Duration::ZERO,
                },
                |entries, adopted, bounds| async move {
                    sweep_entries_with_hooks(
                        entries,
                        adopted,
                        bounds,
                        |entry, process| {
                            if survived {
                                current_verdict(entry, process)
                            } else {
                                let mut observed = process.observe().unwrap();
                                observed.executable = None;
                                Some(verdict_with_executable_access(
                                    entry,
                                    process.pid(),
                                    &observed,
                                    subc_os::ExecutableAccess::Unavailable,
                                ))
                            }
                        },
                        |_process, signal| {
                            // Simulate successful TERM delivery without exiting, then
                            // a failed KILL. The real child stays killable for cleanup.
                            if signal == subc_os::Signal::Terminate {
                                Ok(true)
                            } else {
                                Err(io::Error::other("injected signal failure"))
                            }
                        },
                    )
                    .await
                },
            )
            .await;
            let reason = if survived {
                RetainedReason::Survived
            } else {
                RetainedReason::Unverifiable
            };
            assert_eq!(decisions.len(), 1);
            assert_eq!(decisions[0].1.retained_reason(), Some(reason));
            assert_eq!(
                read_record(&path).unwrap(),
                vec![fixture.entry.clone()],
                "the top-level sweep must retain the blocker"
            );
            assert!(still_running(&mut fixture.child));

            let roster = crate::child_roster::ChildRoster::default();
            roster.record_to(path.clone());
            assert_eq!(
                roster.previous_instance("previous"),
                Some((fixture.entry.pid, reason))
            );
            assert_eq!(roster.previous_instance_pid("unrelated"), None);
            let guard = roster.admit(
                "unrelated".to_owned(),
                std::process::id(),
                ModuleProtocol::None,
                None,
                crate::child_roster::RecordedIdentity::default(),
            );
            assert!(read_record(&path).unwrap().contains(&fixture.entry));
            drop(guard);
            let supervisor = Supervisor::new(
                Arc::new(crate::registry::Registry::default()),
                RestartPolicy::default(),
            )
            .with_live_children_record(path.clone());
            let spec = |module_id: &str| ModuleSpec {
                module_id: module_id.to_owned(),
                program: program.clone(),
                args: vec!["60".to_owned()],
                env: vec![],
                reserved: false,
                reserved_prefixes: vec![],
                protocol: ModuleProtocol::None,
                overlap: ModuleOverlap::Exclusive,
            };
            let waiting = supervisor
                .supervise_configured(spec("previous"), true)
                .unwrap();
            let other = supervisor
                .supervise_configured(spec("unrelated"), true)
                .unwrap();
            let status = waiting.status().unwrap();
            assert_eq!(status.waiting_for_previous_pid, Some(fixture.entry.pid));
            assert_eq!(
                status.waiting_for_previous_detail,
                Some(reason.waiting_message(fixture.entry.pid))
            );
            assert!(!status.process_alive && status.pid.is_none());
            assert_eq!(status.spawn_generation, 0);
            assert!(
                other.status().unwrap().live,
                "an unrelated module must start"
            );
            let refusal = waiting.set_enabled(true).await.unwrap_err().to_string();
            assert!(refusal.contains(&reason.waiting_message(fixture.entry.pid)));
            assert!(waiting.restart(None).await.is_err());
            assert!(waiting.reload().await.is_err());
            assert!(waiting.swap(None).await.is_err());
            assert!(supervisor.spawn(spec("previous")).is_err());
            other.stop().await.unwrap();
            waiting.stop().await.unwrap();
            assert_eq!(read_record(&path).unwrap(), vec![fixture.entry.clone()]);
            assert!(still_running(&mut fixture.child));
            fixture.child.kill().unwrap();
            fixture.child.wait().unwrap();
            assert_eq!(roster.previous_instance_pid("previous"), None);
            assert!(read_record(&path).unwrap().is_empty());
        }

        #[cfg(target_os = "linux")]
        #[tokio::test]
        async fn live_unverifiable_orphan_is_retained_and_blocks_only_its_module() {
            retained_orphan_blocks_only_its_module(false).await;
        }

        #[cfg(target_os = "linux")]
        #[tokio::test]
        async fn surviving_orphan_is_retained_and_blocks_only_its_module() {
            retained_orphan_blocks_only_its_module(true).await;
        }

        #[tokio::test]
        async fn a_recorded_child_ignoring_sigterm_is_killed_after_the_grace() {
            let dir = ScratchDir::new("sweep-kills");
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
            let decisions = sweep_orphans(
                &RunDirLock::acquire(&path).unwrap(),
                &AdoptedPids::none(),
                quick(),
            )
            .await;

            assert_eq!(decisions, vec![(entry, SweepDecision::Killed)]);
            assert!(started.elapsed() >= quick().term_grace);
            let status = child.wait().unwrap();
            assert_eq!(status.signal(), Some(9), "ended by SIGKILL: {status}");
        }

        #[tokio::test]
        async fn a_pid_now_held_by_an_unrelated_process_is_not_signalled() {
            let dir = ScratchDir::new("sweep-reused-pid");
            let path = record_path(&dir);
            let (mut child, entry) = spawn_recorded(&executable("sleep"), &["60"]);
            // The record says the pid belonged to a process started at
            // another time: what a reaped pid handed to a new process reads as.
            let mut reused = entry.clone();
            reused.start_time = reused.start_time.map(|start| start + 1);
            write_record(&path, std::slice::from_ref(&reused)).unwrap();

            let decisions = sweep_orphans(
                &RunDirLock::acquire(&path).unwrap(),
                &AdoptedPids::none(),
                quick(),
            )
            .await;

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
            let dir = ScratchDir::new("sweep-other-executable");
            let path = record_path(&dir);
            let (mut child, entry) = spawn_recorded(&executable("sleep"), &["60"]);
            let mut other = entry.clone();
            other.executable = subc_os::file_identity(&std::env::current_exe().unwrap())
                .map(ExecutableIdentity::from);
            write_record(&path, std::slice::from_ref(&other)).unwrap();

            let decisions = sweep_orphans(
                &RunDirLock::acquire(&path).unwrap(),
                &AdoptedPids::none(),
                quick(),
            )
            .await;

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
            let dir = ScratchDir::new("sweep-gone");
            let path = record_path(&dir);
            let (mut child, entry) = spawn_recorded(&executable("sleep"), &["60"]);
            child.kill().unwrap();
            child.wait().unwrap();
            write_record(&path, std::slice::from_ref(&entry)).unwrap();

            let decisions = sweep_orphans(
                &RunDirLock::acquire(&path).unwrap(),
                &AdoptedPids::none(),
                quick(),
            )
            .await;

            assert_eq!(decisions, vec![(entry, SweepDecision::Gone)]);
            assert_eq!(read_record(&path).unwrap(), Vec::new());
        }

        #[tokio::test]
        async fn an_adopted_pid_is_left_running() {
            let dir = ScratchDir::new("sweep-adopted");
            let path = record_path(&dir);
            let (mut child, entry) = spawn_recorded(&executable("sleep"), &["60"]);
            write_record(&path, std::slice::from_ref(&entry)).unwrap();

            let decisions = sweep_orphans(
                &RunDirLock::acquire(&path).unwrap(),
                &AdoptedPids::of([entry.pid]),
                quick(),
            )
            .await;

            assert_eq!(decisions, vec![(entry, SweepDecision::Adopted)]);
            assert!(still_running(&mut child), "an adopted child was signalled");
            child.kill().unwrap();
            child.wait().unwrap();
        }

        #[tokio::test]
        async fn an_unconfirmed_trampoline_image_is_never_signalled_by_orphan_cleanup() {
            let dir = ScratchDir::new("sweep-unconfirmed-trampoline");
            let path = record_path(&dir);
            let (mut child, mut entry) = spawn_recorded(&executable("sleep"), &["60"]);
            entry.executable = None;
            write_record(&path, std::slice::from_ref(&entry)).unwrap();
            let decisions = sweep_orphans(
                &RunDirLock::acquire(&path).unwrap(),
                &AdoptedPids::none(),
                quick(),
            )
            .await;
            assert_eq!(
                decisions,
                vec![(
                    entry,
                    SweepDecision::Unverifiable(
                        IdentityVerdict::ExecutableUnrecorded.describe().to_owned()
                    )
                )]
            );
            assert!(
                still_running(&mut child),
                "a pid without a confirmed module image was signalled"
            );
            child.kill().unwrap();
            child.wait().unwrap();
        }
    }
}
