use std::{
    fs,
    io::{self, Write as _},
    path::{Path, PathBuf},
    time::Duration,
};

use tokio::time::{sleep, Instant};

use crate::{module_directory_name, Placement};

/// Result of requesting a freeze and waiting for the entire subtree to freeze.
#[derive(Debug)]
pub enum FreezeOutcome {
    Frozen(FrozenModule),
    NotPlaced,
    Unsupported,
    TimedOut { path: PathBuf },
    IoError { path: PathBuf, error: io::Error },
}

/// A module subtree whose `cgroup.events` has reported `frozen 1`.
///
/// Keep this guard alive until all listed PIDs have been signalled: frozen tasks
/// cannot exit and release their PIDs for reuse. Dropping the guard attempts to
/// thaw the subtree, including when a caller cancels the cleanup future; a drop
/// cannot report a failed thaw, so callers that need the result use `thaw`.
#[derive(Debug)]
pub struct FrozenModule {
    path: PathBuf,
    thaw_on_drop: bool,
}

/// Result of listing the frozen module and all its descendant cgroups.
#[derive(Debug)]
pub enum ProcsOutcome {
    Listed(Vec<i32>),
    IoError { path: PathBuf, error: io::Error },
}

/// Result of releasing a module subtree's freeze request.
#[derive(Debug)]
pub enum ThawOutcome {
    Thawed,
    IoError { path: PathBuf, error: io::Error },
}

/// Freeze a placed module, waiting at most `timeout` for kernel confirmation.
///
/// Like `cgroup.kill`, the freezer interface is opened without creating it, so
/// a kernel without freezer support produces `Unsupported` instead of a file.
/// A failed or cancelled wait attempts to release the freeze request on drop.
pub async fn freeze_module(
    placement: Option<&Placement>,
    module_id: &str,
    timeout: Duration,
) -> FreezeOutcome {
    let Some(placement) = placement else {
        return FreezeOutcome::NotPlaced;
    };
    let module = placement.modules.join(module_directory_name(module_id));
    let path = module.join("cgroup.freeze");
    if matches!(module_id, "" | "." | "..") {
        return FreezeOutcome::IoError {
            path,
            error: io::Error::new(
                io::ErrorKind::InvalidInput,
                "module id must name a child cgroup",
            ),
        };
    }
    match fs::metadata(&module) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return FreezeOutcome::NotPlaced,
        Err(error) => return FreezeOutcome::IoError { path, error },
        Ok(_) => {}
    }
    let mut file = match fs::OpenOptions::new().write(true).open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return FreezeOutcome::Unsupported,
        Err(error) => return FreezeOutcome::IoError { path, error },
    };
    let frozen = FrozenModule {
        path: module,
        thaw_on_drop: true,
    };
    if let Err(error) = file.write_all(b"1") {
        return FreezeOutcome::IoError { path, error };
    }
    let path = frozen.path.join("cgroup.events");
    let deadline = Instant::now() + timeout;
    loop {
        match fs::read_to_string(&path) {
            Ok(events) if events.lines().any(|line| line == "frozen 1") => {
                return FreezeOutcome::Frozen(frozen);
            }
            Ok(_) => {}
            Err(error) => return FreezeOutcome::IoError { path, error },
        }
        if Instant::now() >= deadline {
            return FreezeOutcome::TimedOut { path };
        }
        sleep(Duration::from_millis(1)).await;
    }
}

impl FrozenModule {
    /// Read this cgroup's and its descendants' `cgroup.procs`, descending only
    /// into entries reported as directories rather than symlinks.
    ///
    /// No partial list is returned on error, so callers can fall back to an
    /// atomic kill instead of signalling an incompletely enumerated subtree.
    pub fn processes(&self) -> ProcsOutcome {
        let mut pending = vec![self.path.clone()];
        let mut pids = Vec::new();
        while let Some(dir) = pending.pop() {
            let path = dir.join("cgroup.procs");
            let contents = match fs::read_to_string(&path) {
                Ok(contents) => contents,
                Err(error) => return ProcsOutcome::IoError { path, error },
            };
            for line in contents.lines() {
                match line.parse::<i32>() {
                    Ok(pid) if pid > 1 => pids.push(pid),
                    // PID 0 can represent a task outside the reader's PID
                    // namespace; neither it nor init is a signal target.
                    Ok(_) => {}
                    Err(error) => {
                        return ProcsOutcome::IoError {
                            path,
                            error: io::Error::new(io::ErrorKind::InvalidData, error),
                        };
                    }
                }
            }
            let entries = match fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(error) => return ProcsOutcome::IoError { path: dir, error },
            };
            for entry in entries {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(error) => return ProcsOutcome::IoError { path: dir, error },
                };
                match entry.file_type() {
                    Ok(kind) if kind.is_dir() => pending.push(entry.path()),
                    Ok(_) => {}
                    Err(error) => {
                        return ProcsOutcome::IoError {
                            path: entry.path(),
                            error,
                        };
                    }
                }
            }
        }
        pids.sort_unstable();
        pids.dedup();
        ProcsOutcome::Listed(pids)
    }

    /// Release the freeze request, reporting errors rather than hiding them in Drop.
    pub fn thaw(mut self) -> ThawOutcome {
        match write_thaw(&self.path) {
            Ok(()) => {
                self.thaw_on_drop = false;
                ThawOutcome::Thawed
            }
            Err(error) => ThawOutcome::IoError {
                path: self.path.join("cgroup.freeze"),
                error,
            },
        }
    }
}

impl Drop for FrozenModule {
    fn drop(&mut self) {
        if self.thaw_on_drop {
            let _ = write_thaw(&self.path);
        }
    }
}

fn write_thaw(module: &Path) -> io::Result<()> {
    fs::OpenOptions::new()
        .write(true)
        .open(module.join("cgroup.freeze"))?
        .write_all(b"0")
}
