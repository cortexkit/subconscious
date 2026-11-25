//! One fork boundary for daemon and OS helpers. Spawns share the lock, while
//! Unix descriptor creation and daemon image replacement hold it exclusively.
//! A guard never survives into an await or a wait on a running process.

#[cfg(unix)]
use std::sync::RwLockWriteGuard;
use std::{io, process, sync::RwLock};

static FORK_LOCK: RwLock<()> = RwLock::new(());

/// Exclude child creation while changing which descriptors a fork can inherit.
/// Keep the guard on one thread, and restore the descriptor flags before dropping it.
#[cfg(unix)]
pub fn creation_lock() -> RwLockWriteGuard<'static, ()> {
    FORK_LOCK.write().unwrap_or_else(|error| error.into_inner())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use image::{
    exec_with_descriptors, monotonic_ns, ExecImage, ExecImageError, ExecTarget,
    InheritedDescriptors,
};

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[allow(unsafe_code)]
mod image {
    use super::creation_lock;
    #[cfg(target_os = "linux")]
    use std::os::fd::{AsRawFd, BorrowedFd};
    use std::{
        collections::{BTreeMap, BTreeSet},
        ffi::{c_char, CStr, CString, OsString},
        fs,
        io::{self, PipeWriter},
        os::fd::{FromRawFd, OwnedFd, RawFd},
        path::Path,
        ptr,
        sync::OnceLock,
    };

    /// Owns the strings and the null-terminated tables passed to execve.
    pub struct ExecImage {
        _args: Vec<CString>,
        _environment: Vec<CString>,
        argv: Vec<*const c_char>,
        envp: Vec<*const c_char>,
    }

    // SAFETY: All pointers address immutable, privately owned CString heaps.
    // Moving the owner between threads neither moves nor mutates those buffers.
    unsafe impl Send for ExecImage {}

    impl ExecImage {
        pub fn new(args: Vec<CString>, environment: Vec<CString>) -> Self {
            let pointers = |strings: &[CString]| {
                strings
                    .iter()
                    .map(|s| s.as_ptr())
                    .chain([ptr::null()])
                    .collect()
            };
            Self {
                argv: pointers(&args),
                envp: pointers(&environment),
                _args: args,
                _environment: environment,
            }
        }
    }

    #[derive(Debug)]
    pub enum ExecImageError {
        DescriptorFlags(io::Error),
        Recheck(io::Error),
        Exec(io::Error),
    }

    pub enum ExecTarget<'a> {
        Path(&'a CStr),
        #[cfg(target_os = "linux")]
        Descriptor(BorrowedFd<'a>),
    }

    #[derive(Default)]
    struct Changed(Vec<(RawFd, libc::c_int)>);

    impl Drop for Changed {
        fn drop(&mut self) {
            for &(fd, flags) in self.0.iter().rev() {
                // SAFETY: fcntl reads no user memory. It only changes flags on fd.
                unsafe { libc::fcntl(fd, libc::F_SETFD, flags) };
            }
        }
    }

    /// Replace this process, passing exactly the given descriptors above stdio.
    /// The caller keeps the passed descriptors open; all other descriptors opened
    /// concurrently by the process must use CLOEXEC. If exec returns, the flags
    /// are restored while forks are still excluded.
    pub fn exec_with_descriptors(
        target: ExecTarget<'_>,
        image: &ExecImage,
        fds: &BTreeSet<RawFd>,
        key_writer: PipeWriter,
        recheck: impl FnOnce() -> io::Result<()>,
    ) -> ExecImageError {
        let _fork = creation_lock();
        let mut changed = Changed::default();
        if let Err(error) = configure(fds, &mut changed) {
            return ExecImageError::DescriptorFlags(error);
        }
        drop(key_writer);
        if let Err(error) = recheck() {
            return ExecImageError::Recheck(error);
        }
        // SAFETY: The path and pointer tables are null-terminated and live.
        // The descriptor variant borrows the file for the entire syscall.
        unsafe {
            match target {
                ExecTarget::Path(path) => {
                    libc::execve(path.as_ptr(), image.argv.as_ptr(), image.envp.as_ptr());
                }
                #[cfg(target_os = "linux")]
                ExecTarget::Descriptor(fd) => {
                    libc::syscall(
                        libc::SYS_execveat,
                        fd.as_raw_fd(),
                        c"".as_ptr(),
                        image.argv.as_ptr(),
                        image.envp.as_ptr(),
                        libc::AT_EMPTY_PATH,
                    );
                }
            }
        }
        ExecImageError::Exec(io::Error::last_os_error())
    }

    fn descriptor_dir() -> &'static str {
        if cfg!(target_os = "linux") {
            "/proc/self/fd"
        } else {
            "/dev/fd"
        }
    }

    /// A snapshot of descriptor numbers left open by an image replacement.
    /// A number in the snapshot is not owned until explicitly claimed.
    pub struct InheritedDescriptors {
        open: BTreeSet<RawFd>,
        key: RawFd,
    }

    impl InheritedDescriptors {
        /// Snapshot descriptors on the daemon's argv-gated resume path.
        ///
        /// # Safety
        /// Call first in `main`, after parsing the actual resume argv but
        /// before creating any descriptor or starting any thread. Until the
        /// snapshot is consumed, no other code may take ownership of, close,
        /// or replace a descriptor recorded in it.
        pub unsafe fn capture_daemon_resume(path: &Path, key_fd: RawFd) -> io::Result<Self> {
            static CLAIMED: OnceLock<()> = OnceLock::new();
            let args = std::env::args_os().skip(1).collect::<Vec<_>>();
            reserve_daemon_resume(&CLAIMED, &args, path, key_fd)?;
            Ok(Self {
                open: inherited_at_startup()?,
                key: key_fd,
            })
        }

        /// Take only the key named on the resume command line.
        pub fn take_key(&mut self, fd: RawFd) -> io::Result<OwnedFd> {
            if fd != self.key || !self.open.remove(&fd) {
                return Err(io::Error::other(
                    "handover key descriptor was not inherited",
                ));
            }
            // SAFETY: the startup preconditions and removal give this handle
            // exclusive ownership of the still-open key fd.
            Ok(unsafe { OwnedFd::from_raw_fd(fd) })
        }

        /// Claim the descriptors named by the authenticated handover. Other
        /// descriptors are left untouched, including when validation fails.
        pub fn into_named(
            self,
            expected: &BTreeSet<RawFd>,
        ) -> io::Result<BTreeMap<RawFd, OwnedFd>> {
            if !expected.is_subset(&self.open) {
                return Err(io::Error::other(
                    "handover names a descriptor that was not inherited",
                ));
            }
            Ok(expected
                .iter()
                .map(|&fd| {
                    // SAFETY: consumption of the one-shot snapshot and its startup
                    // preconditions make each explicitly requested fd uniquely owned.
                    (fd, unsafe { OwnedFd::from_raw_fd(fd) })
                })
                .collect())
        }
    }

    fn inherited_at_startup() -> io::Result<BTreeSet<RawFd>> {
        let mut candidates = BTreeSet::new();
        for entry in fs::read_dir(descriptor_dir())? {
            if let Ok(fd) = entry?.file_name().to_string_lossy().parse::<RawFd>() {
                if fd > 2 {
                    candidates.insert(fd);
                }
            }
        }
        let mut inherited = BTreeSet::new();
        // The directory is closed before checking flags, so its own fd
        // can never be mistaken for an inherited one.
        for fd in candidates {
            // SAFETY: F_GETFD queries the fd table, not user memory.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
            if flags == -1 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::EBADF) {
                    return Err(error);
                }
            } else if flags & libc::FD_CLOEXEC == 0 {
                inherited.insert(fd);
            }
        }
        Ok(inherited)
    }

    fn reserve_daemon_resume(
        claimed: &OnceLock<()>,
        args: &[OsString],
        path: &Path,
        key_fd: RawFd,
    ) -> io::Result<()> {
        let named_path = args
            .windows(2)
            .any(|pair| pair[0] == "--resume-handover" && pair[1] == path.as_os_str());
        let named_key = key_fd > 2
            && args.windows(2).any(|pair| {
                pair[0] == "--handover-key-fd"
                    && pair[1].to_str().and_then(|s| s.parse().ok()) == Some(key_fd)
            });
        if !named_path || !named_key {
            return Err(io::Error::other(
                "descriptor capture requires daemon resume arguments",
            ));
        }
        claimed
            .set(())
            .map_err(|()| io::Error::other("daemon resume descriptors already captured"))
    }

    #[cfg(test)]
    #[test]
    fn inherited_pass_set_rejects_closed_descriptors() {
        use std::os::fd::AsRawFd;

        let (first, second) = io::pipe().unwrap();
        let missing = second.as_raw_fd();
        drop(second);
        let mut snapshot = InheritedDescriptors {
            open: BTreeSet::from([first.as_raw_fd()]),
            key: missing,
        };
        assert!(snapshot.take_key(missing).is_err());
        assert!(snapshot.into_named(&BTreeSet::from([missing])).is_err());
    }

    #[cfg(test)]
    #[test]
    fn unnamed_inherited_descriptor_stays_open_and_unchanged() {
        use std::os::fd::{AsRawFd, IntoRawFd};

        let (first, unlisted) = io::pipe().unwrap();
        let wanted = first.into_raw_fd();
        let raw = unlisted.as_raw_fd();
        // SAFETY: F_GETFD observes the descriptor still owned by unlisted.
        let flags = unsafe { libc::fcntl(raw, libc::F_GETFD) };
        assert_ne!(flags, -1);
        let snapshot = InheritedDescriptors {
            open: BTreeSet::from([wanted, raw]),
            key: -1,
        };
        let claimed = snapshot.into_named(&BTreeSet::from([wanted])).unwrap();
        assert_eq!(claimed.len(), 1);
        drop(claimed);
        // SAFETY: query the untouched descriptor without reading user memory.
        assert_eq!(unsafe { libc::fcntl(raw, libc::F_GETFD) }, flags);
    }

    #[cfg(test)]
    #[test]
    fn inherited_descriptor_capture_requires_resume_args_and_refuses_a_second_call() {
        let path = Path::new("/daemon/run/daemon-upgrade.handover");
        let claimed = OnceLock::new();
        assert!(reserve_daemon_resume(&claimed, &[], path, 3).is_err());
        let args = [
            "--resume-handover",
            path.to_str().unwrap(),
            "--handover-key-fd",
            "3",
        ]
        .map(OsString::from);
        assert!(reserve_daemon_resume(&claimed, &args, path, 4).is_err());
        reserve_daemon_resume(&claimed, &args, path, 3).unwrap();
        assert!(reserve_daemon_resume(&claimed, &args, path, 3)
            .unwrap_err()
            .to_string()
            .contains("already captured"));
    }

    fn configure(pass: &BTreeSet<RawFd>, changed: &mut Changed) -> io::Result<()> {
        let mut seen = BTreeSet::new();
        if pass.iter().any(|&fd| fd <= 2) {
            return Err(io::Error::other("handover descriptor aliases stdio"));
        }
        for entry in fs::read_dir(descriptor_dir())? {
            let entry = entry?;
            let Ok(fd) = entry.file_name().to_string_lossy().parse::<RawFd>() else {
                continue;
            };
            if fd <= 2 {
                continue;
            }
            // SAFETY: These fcntl operations access only kernel descriptor flags.
            let old = unsafe { libc::fcntl(fd, libc::F_GETFD) };
            if old == -1 {
                let error = io::Error::last_os_error();
                if !pass.contains(&fd) && error.raw_os_error() == Some(libc::EBADF) {
                    continue;
                }
                return Err(error);
            }
            let new = if pass.contains(&fd) {
                seen.insert(fd);
                old & !libc::FD_CLOEXEC
            } else {
                old | libc::FD_CLOEXEC
            };
            if old != new {
                // SAFETY: F_SETFD takes the integer flag word, not a pointer.
                if unsafe { libc::fcntl(fd, libc::F_SETFD, new) } == -1 {
                    return Err(io::Error::last_os_error());
                }
                changed.0.push((fd, old));
            }
        }
        if &seen != pass {
            return Err(io::Error::other("handover descriptor is not open"));
        }
        Ok(())
    }

    pub fn monotonic_ns() -> io::Result<u64> {
        let mut time = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: clock_gettime writes a timespec into a live, writable object.
        if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((time.tv_sec as u64).saturating_mul(1_000_000_000) + time.tv_nsec as u64)
    }
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

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    #[allow(unsafe_code)]
    fn failed_exec_restores_the_pass_descriptor_before_spawning() {
        use std::{collections::BTreeSet, ffi::CString, os::fd::AsRawFd};
        let (read, write) = io::pipe().unwrap();
        let fd = read.as_raw_fd();
        // SAFETY: F_GETFD only samples the live pipe descriptor's flags.
        let flags = || unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert_eq!(flags(), libc::FD_CLOEXEC);
        let image = ExecImage::new(vec![CString::new("unavailable-fixture").unwrap()], vec![]);
        let result = exec_with_descriptors(
            ExecTarget::Path(c"/nonexistent/subc/fixture"),
            &image,
            &BTreeSet::from([fd]),
            write,
            || {
                assert_eq!(flags(), 0);
                Ok(())
            },
        );
        assert!(
            matches!(result, ExecImageError::Exec(error) if error.kind() == io::ErrorKind::NotFound)
        );
        assert_eq!(flags(), libc::FD_CLOEXEC);
        assert!(
            status(process::Command::new("/bin/sh").args(["-c", "exit 0"]))
                .unwrap()
                .success()
        );
    }
}
