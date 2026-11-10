//! One fork boundary for daemon and OS helpers. Spawns share the lock, while
//! Unix socketpair creation holds it exclusively until both ends are CLOEXEC.
//! A guard never survives into an await or a wait on a running process.

#[cfg(unix)]
use std::sync::RwLockWriteGuard;
use std::{io, process, sync::RwLock};

static FORK_LOCK: RwLock<()> = RwLock::new(());

#[cfg(unix)]
pub(crate) fn creation_lock() -> RwLockWriteGuard<'static, ()> {
    FORK_LOCK.write().unwrap_or_else(|error| error.into_inner())
}

pub fn spawn(command: &mut process::Command) -> io::Result<process::Child> {
    let _guard = FORK_LOCK.read().unwrap_or_else(|error| error.into_inner());
    command.spawn()
}

/// Capture both output streams with null stdin. For custom stdio, call
/// [`spawn`] and then `Child::wait_with_output` instead.
pub fn output(command: &mut process::Command) -> io::Result<process::Output> {
    command
        .stdin(process::Stdio::null())
        .stdout(process::Stdio::piped())
        .stderr(process::Stdio::piped());
    spawn(command)?.wait_with_output()
}

pub fn status(command: &mut process::Command) -> io::Result<process::ExitStatus> {
    spawn(command)?.wait()
}

#[cfg(feature = "tokio")]
pub fn tokio_spawn(command: &mut tokio::process::Command) -> io::Result<tokio::process::Child> {
    let _guard = FORK_LOCK.read().unwrap_or_else(|error| error.into_inner());
    command.spawn()
}

#[cfg(feature = "tokio")]
/// Capture both output streams with null stdin. Custom stdio callers use
/// [`tokio_spawn`] followed by `Child::wait_with_output`.
pub async fn tokio_output(command: &mut tokio::process::Command) -> io::Result<process::Output> {
    command
        .stdin(process::Stdio::null())
        .stdout(process::Stdio::piped())
        .stderr(process::Stdio::piped());
    tokio_spawn(command)?.wait_with_output().await
}

#[cfg(feature = "tokio")]
pub async fn tokio_status(
    command: &mut tokio::process::Command,
) -> io::Result<process::ExitStatus> {
    tokio_spawn(command)?.wait().await
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn socket_creation_excludes_child_forks() {
        let guard = creation_lock();
        let (started, ready) = std::sync::mpsc::channel();
        let (finished, result) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            started.send(()).unwrap();
            let child = spawn(process::Command::new("/bin/sh").args(["-c", "exit 0"])).unwrap();
            finished.send(child).unwrap();
        });
        ready.recv().unwrap();
        assert!(
            result
                .recv_timeout(std::time::Duration::from_millis(100))
                .is_err(),
            "spawn bypassed exclusive socket creation"
        );
        drop(guard);
        assert!(result
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap()
            .wait()
            .unwrap()
            .success());
        thread.join().unwrap();
    }
}
