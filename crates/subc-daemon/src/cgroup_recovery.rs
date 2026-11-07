//! Recorded cgroup cleanup uses only the daemon's current trusted placement.
//! Directory removal is housekeeping: kernel-confirmed emptiness ends the block.

use crate::live_children::{RetainedReason, SweepBounds, SweepDecision};
use std::{
    fs, io,
    path::{Path, PathBuf},
    time::Duration,
};
use subc_cgroup::{FreezeOutcome, FrozenModule, KillOutcome, Placement, ProcsOutcome};
use subc_os::{Process, Signal};

const FREEZE_BOUND: Duration = Duration::from_millis(100);
const POLL: Duration = Duration::from_millis(10);

fn trusted_path(
    placement: Option<&Placement>,
    path: &Path,
) -> Result<Option<PathBuf>, RetainedReason> {
    let placement = placement.ok_or_else(root_unavailable)?;
    placement.existing_module_path(path).map_err(|error| {
        if error.kind() == io::ErrorKind::InvalidInput {
            root_unavailable()
        } else {
            RetainedReason::ContainerUnverifiable(error.to_string())
        }
    })
}

fn root_unavailable() -> RetainedReason {
    RetainedReason::ContainerUnverifiable("container root unavailable".into())
}

fn populated(path: &Path) -> io::Result<bool> {
    let events = fs::read_to_string(path.join("cgroup.events"))?;
    match events
        .lines()
        .find_map(|line| line.strip_prefix("populated "))
    {
        Some("0") => Ok(false),
        Some("1") => Ok(true),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "missing or malformed cgroup populated state",
        )),
    }
}

fn directories(path: &Path) -> io::Result<Vec<PathBuf>> {
    let mut pending = vec![path.to_owned()];
    let mut all = Vec::new();
    while let Some(path) = pending.pop() {
        for entry in fs::read_dir(&path)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                pending.push(entry.path());
            }
        }
        all.push(path);
    }
    Ok(all)
}

fn survivors(path: &Path) -> io::Result<Vec<u32>> {
    let mut pids = Vec::new();
    for path in directories(path)? {
        for line in fs::read_to_string(path.join("cgroup.procs"))?.lines() {
            let pid = line.parse::<u32>().map_err(io::Error::other)?;
            if pid > 1 {
                pids.push(pid);
            }
        }
    }
    pids.sort_unstable();
    pids.dedup();
    Ok(pids)
}

fn remove_empty(placement: &Placement, path: &Path) -> io::Result<()> {
    // Remove child directories before their parent, never interface files or
    // symlinks. Every removal repeats the trusted-root path check.
    for path in directories(path)?.into_iter().rev() {
        placement.remove_path(&path)?;
    }
    Ok(())
}

fn state(placement: Option<&Placement>, path: &Path) -> Option<RetainedReason> {
    let path = match trusted_path(placement, path) {
        Ok(Some(path)) => path,
        Ok(None) => return None,
        Err(reason) => return Some(reason),
    };
    match populated(&path) {
        Ok(false) => {
            if let Err(error) = remove_empty(placement.expect("validated placement"), &path) {
                tracing::warn!(path = %path.display(), %error, "stopped container directory removal deferred");
                return Some(RetainedReason::EmptyContainerDirectory);
            }
            None
        }
        Ok(true) => match survivors(&path) {
            Ok(pids) if !pids.is_empty() => Some(RetainedReason::ContainerSurvivors(pids)),
            Ok(_) => Some(RetainedReason::ContainerUnverifiable(
                "container is populated but no member PID is observable".into(),
            )),
            Err(error) => Some(RetainedReason::ContainerUnverifiable(error.to_string())),
        },
        Err(error) => match trusted_path(placement, &path) {
            Ok(None) => None,
            _ => Some(RetainedReason::ContainerUnverifiable(error.to_string())),
        },
    }
}

fn signal_frozen(frozen: &FrozenModule, signal: Signal) -> io::Result<()> {
    let ProcsOutcome::Listed(pids) = frozen.processes() else {
        return Err(io::Error::other("cannot list frozen cgroup members"));
    };
    let mut failure = None;
    for pid in pids {
        let result: io::Result<()> = (|| {
            let Some(process) = Process::open(pid as u32)? else {
                return Ok(());
            };
            // Open the process handle first, then confirm it is still a member.
            // Linux signals through the opened process file descriptor (a
            // pidfd) where the kernel has them; on older kernels the frozen
            // guard keeps members stable instead.
            match frozen.processes() {
                ProcsOutcome::Listed(current) if current.contains(&pid) => {
                    process.signal(signal)?;
                }
                ProcsOutcome::Listed(_) => {}
                ProcsOutcome::IoError { error, .. } => return Err(error),
            }
            Ok(())
        })();
        if let Err(error) = result {
            failure.get_or_insert(error);
        }
    }
    failure.map_or(Ok(()), Err)
}

#[cfg(test)]
#[path = "cgroup_recovery_tests.rs"]
mod tests;

fn kill_checked(placement: &Placement, path: &Path) {
    match subc_cgroup::kill_path(placement, path) {
        KillOutcome::Killed | KillOutcome::NotPlaced => return,
        outcome => {
            tracing::warn!(path = %path.display(), ?outcome, "atomic carried-cgroup kill unavailable; trying frozen members")
        }
    }
    match subc_cgroup::freeze_path_blocking(placement, path, FREEZE_BOUND) {
        FreezeOutcome::Frozen(frozen) => {
            if let Err(error) = signal_frozen(&frozen, Signal::Kill) {
                tracing::warn!(path = %path.display(), %error, "frozen carried-cgroup kill failed");
            }
        }
        outcome => {
            tracing::warn!(path = %path.display(), ?outcome, "carried-cgroup freezer fallback failed")
        }
    }
}

/// Retry without an async runtime. Never clear a populated or uninspectable
/// subtree just because its leader is gone or its kill request was accepted.
pub(crate) fn recheck(placement: Option<&Placement>, path: &Path) -> Option<RetainedReason> {
    let path = match trusted_path(placement, path) {
        Ok(Some(path)) => path,
        Ok(None) => return None,
        Err(reason) => return Some(reason),
    };
    if !matches!(populated(&path), Ok(false)) {
        kill_checked(placement.expect("validated placement"), &path);
    }
    state(placement, &path)
}

fn decision(reason: Option<RetainedReason>, cleared: SweepDecision) -> SweepDecision {
    match reason {
        None => cleared,
        Some(RetainedReason::EmptyContainerDirectory) => SweepDecision::EmptyContainerDirectory,
        Some(RetainedReason::ContainerSurvivors(pids)) => SweepDecision::ContainerSurvivors(pids),
        Some(RetainedReason::ContainerUnverifiable(error)) => SweepDecision::Unverifiable(error),
        Some(other) => SweepDecision::Unverifiable(other.waiting_message(0)),
    }
}

/// Freeze and TERM first, then hard-kill and confirm the whole subtree empty.
pub(crate) async fn sweep(
    placement: Option<&Placement>,
    path: &Path,
    bounds: SweepBounds,
) -> SweepDecision {
    let path = match trusted_path(placement, path) {
        Ok(Some(path)) => path,
        Ok(None) => return SweepDecision::Gone,
        Err(reason) => return decision(Some(reason), SweepDecision::Gone),
    };
    let placement = placement.expect("validated placement");
    if matches!(populated(&path), Ok(false)) {
        return decision(state(Some(placement), &path), SweepDecision::Gone);
    }
    if let FreezeOutcome::Frozen(frozen) =
        subc_cgroup::freeze_path(placement, &path, FREEZE_BOUND).await
    {
        if let Err(error) = signal_frozen(&frozen, Signal::Terminate) {
            tracing::warn!(path = %path.display(), %error, "frozen carried-cgroup TERM failed");
        }
        drop(frozen);
        let deadline = tokio::time::Instant::now() + bounds.term_grace;
        loop {
            if matches!(populated(&path), Ok(false)) {
                return decision(state(Some(placement), &path), SweepDecision::Terminated);
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(POLL).await;
        }
    }
    kill_checked(placement, &path);
    let deadline = tokio::time::Instant::now() + bounds.kill_bound;
    loop {
        let reason = state(Some(placement), &path);
        if reason
            .as_ref()
            .is_none_or(|reason| !reason.blocks_fresh_copy())
            || tokio::time::Instant::now() >= deadline
        {
            return decision(reason, SweepDecision::Killed);
        }
        tokio::time::sleep(POLL).await;
    }
}
