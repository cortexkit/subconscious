//! Ownership of the daemon run directory, held for the daemon's whole life.
//!
//! The singleton check and the start lock are both keyed on the connection
//! file, which lives in the runtime directory. The run directory (the
//! live-children record, logs, the terminal journal) is derived from the data
//! home instead. When a daemon is started with a different runtime directory
//! but the same data home as a running one, it finds no live daemon beside
//! its own connection file and binds, and without this lock it would then
//! treat the running daemon's live-children record as a crashed daemon's and
//! signal every module that daemon is supervising.
//!
//! So the daemon takes an exclusive advisory lock on a file in its run
//! directory before it touches anything there, and keeps it until it exits.
//! The kernel releases the lock when the process dies, however it dies, so a
//! crashed daemon never blocks the next one. The orphan sweep takes a
//! [`RunDirLock`] as an argument, which makes sweeping a record without
//! owning its directory impossible to write.

use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process,
};

use fs4::{FileExt, TryLockError};
use tracing::warn;

use crate::bootstrap::{open_owner_only_lock, BootstrapError};

/// The lock file's name inside the daemon run directory.
pub(crate) const RUN_DIR_LOCK_FILE_NAME: &str = "daemon.lock";

/// Exclusive ownership of the run directory that holds a live-children
/// record. Dropping it (or the process ending) releases the directory.
#[derive(Debug)]
pub(crate) struct RunDirLock {
    // Closing this handle releases the advisory lock; the file itself stays.
    _file: fs::File,
    live_children_record: PathBuf,
}

impl RunDirLock {
    /// Lock the directory containing `live_children_record`, creating the
    /// directory and the lock file when absent. Never waits: a directory
    /// another process holds belongs to a running daemon, and waiting for it
    /// would only start a second daemon over the same state later.
    pub(crate) fn acquire(live_children_record: &Path) -> Result<Self, BootstrapError> {
        let run_dir = live_children_record
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let path = run_dir.join(RUN_DIR_LOCK_FILE_NAME);
        let file = fs::create_dir_all(run_dir)
            .and_then(|()| open_owner_only_lock(&path))
            .map_err(|source| BootstrapError::RunDirLockCreate {
                path: path.clone(),
                source,
            })?;
        match FileExt::try_lock(&file) {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(BootstrapError::RunDirBusy {
                    holder_pid: read_holder_pid(&path),
                    path,
                });
            }
            Err(TryLockError::Error(source)) => {
                return Err(BootstrapError::RunDirLockCreate { path, source });
            }
        }
        // The pid is only for the refusal message a later daemon prints, so a
        // failure to record it is logged and startup continues.
        if let Err(error) = record_owner_pid(&file) {
            warn!(path = %path.display(), %error, "could not record the run directory owner's pid");
        }
        Ok(Self {
            _file: file,
            live_children_record: live_children_record.to_path_buf(),
        })
    }

    /// The live-children record inside the owned run directory.
    pub(crate) fn live_children_record(&self) -> &Path {
        &self.live_children_record
    }
}

/// Replace the file's contents with this process's pid, through the locked
/// handle so no other process writes it concurrently.
fn record_owner_pid(file: &fs::File) -> io::Result<()> {
    file.set_len(0)?;
    let mut writer = file;
    writer.write_all(format!("{}\n", process::id()).as_bytes())?;
    writer.flush()
}

/// Best effort: the holder may not have written its pid yet, and on Windows
/// a locked file cannot be read by another process at all.
fn read_holder_pid(path: &Path) -> Option<u32> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use subc_test_support::TestTempDir;

    #[test]
    fn a_held_run_dir_refuses_a_second_owner_and_names_the_holder() {
        let dir = TestTempDir::new("run-dir-lock-held");
        let record = crate::live_children::record_path(&dir);
        let first = RunDirLock::acquire(&record).unwrap();
        match RunDirLock::acquire(&record) {
            Err(BootstrapError::RunDirBusy { path, holder_pid }) => {
                assert_eq!(path, dir.join(RUN_DIR_LOCK_FILE_NAME));
                #[cfg(unix)]
                assert_eq!(holder_pid, Some(process::id()));
                #[cfg(not(unix))]
                let _ = holder_pid;
            }
            other => panic!("expected the run directory to be busy, got {other:?}"),
        }
        drop(first);
        RunDirLock::acquire(&record).expect("a released run directory can be taken again");
    }

    #[test]
    fn a_missing_run_dir_is_created() {
        let dir = TestTempDir::new("run-dir-lock-missing");
        let record = crate::live_children::record_path(&dir.join("run"));
        let lock = RunDirLock::acquire(&record).unwrap();
        assert_eq!(lock.live_children_record(), record);
        assert!(dir.join("run").join(RUN_DIR_LOCK_FILE_NAME).exists());
    }
}
