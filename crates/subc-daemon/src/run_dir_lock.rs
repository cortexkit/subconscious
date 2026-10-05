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

    /// Run the case in a fresh, single-test process. Other libtest workers may
    /// fork while a case holds a flock; their children retain the shared open
    /// file description until exec even when FD_CLOEXEC is set. A parent drop
    /// therefore cannot promise immediate release during concurrent spawns.
    fn isolated_case(name: &str, case: fn()) {
        const CASE_ENV: &str = "SUBC_RUN_DIR_LOCK_TEST_CASE";
        const PARENT_ENV: &str = "SUBC_RUN_DIR_LOCK_TEST_PARENT";
        if std::env::var(CASE_ENV).as_deref() == Ok(name) {
            let parent: u32 = std::env::var(PARENT_ENV).unwrap().parse().unwrap();
            assert_ne!(process::id(), parent, "the lock case must run after exec");
            case();
            println!(
                "isolated run-dir lock verified: {name} pid={}",
                process::id()
            );
            return;
        }
        let root = TestTempDir::new("run-dir-lock-isolated");
        let mut command = process::Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", name, "--nocapture", "--test-threads=1"])
            .env(CASE_ENV, name)
            .env(PARENT_ENV, process::id().to_string())
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_RUNTIME_DIR", root.join("runtime"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .stdin(process::Stdio::null())
            .stdout(process::Stdio::piped())
            .stderr(process::Stdio::piped());
        let child = command.spawn().expect("start isolated lock case");
        let pid = child.id();
        let output = child.wait_with_output().expect("reap isolated lock case");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "isolated lock case failed:\n{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            stdout.contains("1 passed; 0 failed"),
            "the child must execute exactly one test: {stdout}"
        );
        assert!(
            stdout.contains(&format!("isolated run-dir lock verified: {name} pid={pid}")),
            "the selected child must actually execute the lock assertions: {stdout}"
        );
        print!("{stdout}");
    }

    #[test]
    #[cfg_attr(
        not(target_os = "macos"),
        ignore = "requires the macOS trampoline and lsof"
    )]
    fn a_cloexec_run_dir_lock_is_retained_only_until_the_childs_first_exec() {
        #[cfg(target_os = "macos")]
        isolated_case("run_dir_lock::tests::a_cloexec_run_dir_lock_is_retained_only_until_the_childs_first_exec", fork_window_case);
    }

    #[cfg(target_os = "macos")]
    fn fork_window_case() {
        use std::{
            io::{Read, Write},
            os::fd::AsRawFd,
            thread,
        };

        // Releasing and reaping even after an assertion failure prevents a
        // diagnostic test from leaving its deliberately paused fork behind.
        struct PausedSpawn {
            release: Option<std::io::PipeWriter>,
            spawn: Option<thread::JoinHandle<io::Result<process::Child>>>,
            child: Option<process::Child>,
        }
        impl PausedSpawn {
            fn resume(&mut self) -> &mut process::Child {
                self.release.take().unwrap().write_all(&[1]).unwrap();
                self.child = Some(self.spawn.take().unwrap().join().unwrap().unwrap());
                self.child.as_mut().unwrap()
            }
        }
        impl Drop for PausedSpawn {
            fn drop(&mut self) {
                if let Some(mut release) = self.release.take() {
                    let _ = release.write_all(&[1]);
                }
                if let Some(spawn) = self.spawn.take() {
                    if let Ok(Ok(child)) = spawn.join() {
                        self.child = Some(child);
                    }
                }
                if let Some(child) = &mut self.child {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
        }

        let dir = TestTempDir::new("run-dir-lock-fork-window");
        let record = crate::live_children::record_path(&dir);
        let lock_path = dir.join(RUN_DIR_LOCK_FILE_NAME);
        let first = RunDirLock::acquire(&record).unwrap();
        let (ack_reader, ack) = subc_os::privacy_identity::ExecAcknowledgement::pipe().unwrap();
        let mut command = process::Command::new(crate::supervise::test_privacy_trampoline());
        command
            .args(["__disclaim-exec", &ack.fd().to_string(), "/bin/sleep", "30"])
            .env("SUBC_TEST_PRIVACY_EXEC_DELAY_MS", "30000")
            .env("XDG_DATA_HOME", dir.join("data"))
            .env("XDG_RUNTIME_DIR", dir.join("runtime"))
            .env("XDG_CONFIG_HOME", dir.join("config"))
            .stdin(process::Stdio::null())
            .stdout(process::Stdio::null())
            .stderr(process::Stdio::null());
        ack.install(&mut command);
        let (mut ready, release) =
            subc_os::fork_exec_test::pause_before_exec(&mut command, first._file.as_raw_fd())
                .unwrap();
        let spawn = thread::spawn(move || command.spawn());
        let mut paused = PausedSpawn {
            release: Some(release),
            spawn: Some(spawn),
            child: None,
        };
        let mut observation = [0u8; 8];
        ready.read_exact(&mut observation).unwrap();
        let child_pid = i32::from_ne_bytes(observation[..4].try_into().unwrap());
        let flags = i32::from_ne_bytes(observation[4..].try_into().unwrap());
        assert!(
            subc_os::fork_exec_test::flags_are_close_on_exec(flags),
            "the inherited lock descriptor must still be CLOEXEC"
        );
        drop(first);
        assert!(
            matches!(RunDirLock::acquire(&record), Err(BootstrapError::RunDirBusy { holder_pid: Some(holder), .. }) if holder == process::id()),
            "a paused fork alone must retain the parent's flock"
        );

        let inspect = || {
            process::Command::new("/usr/sbin/lsof")
                .args(["-nP", "-a", "-p", &child_pid.to_string()])
                .arg(&lock_path)
                .output()
                .unwrap()
        };
        let before = inspect();
        let before_text = String::from_utf8_lossy(&before.stdout);
        assert!(
            before.status.success() && before_text.contains(RUN_DIR_LOCK_FILE_NAME),
            "lsof must identify the paused child as the remaining descriptor holder: {before_text}"
        );
        println!("paused child {child_pid}: FD_CLOEXEC=true; parent owner dropped; acquire=RunDirBusy; lsof before first exec:\n{before_text}");

        // spawn returns only after the child's first exec. The fixture then
        // deliberately remains in the trampoline, before its second SETEXEC.
        let child = paused.resume();
        assert_eq!(child.id(), child_pid as u32);
        assert!(
            child.try_wait().unwrap().is_none(),
            "the trampoline must still be alive"
        );
        drop(ack);
        let second = RunDirLock::acquire(&record)
            .expect("first exec closes the inherited lock even while the trampoline stays alive");
        let after = inspect();
        assert!(
            !after.status.success()
                && !String::from_utf8_lossy(&after.stdout).contains(RUN_DIR_LOCK_FILE_NAME),
            "the live trampoline must not retain daemon.lock: {}",
            String::from_utf8_lossy(&after.stdout)
        );
        println!("live trampoline {child_pid}: acquire succeeded after first exec; lsof finds no daemon.lock descriptor");
        drop(second);
        drop(ack_reader);
    }

    #[test]
    fn a_held_run_dir_refuses_a_second_owner_and_names_the_holder() {
        isolated_case(
            "run_dir_lock::tests::a_held_run_dir_refuses_a_second_owner_and_names_the_holder",
            held_owner_and_release_case,
        );
    }

    fn held_owner_and_release_case() {
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
