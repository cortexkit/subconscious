//! Non-reaping exit observation for supervisors that must decide the lifetime
//! of a process container before releasing the leader's pid.

use std::{io, os::unix::process::ExitStatusExt, process::ExitStatus};

/// Observe an owned child's exit without consuming its wait status.
///
/// The owner must retain the child handle and prevent other waiters (including
/// a dropped asynchronous child handle) from reaping it until the supervisor
/// decides whether to keep the process container for a replacement or destroy
/// it. `None` means the child is still running. `ECHILD` is an error: it does not
/// prove that the first group leader remains unreaped, reserving its PID so a
/// group signal cannot accidentally reach an unrelated, reused process group.
pub fn observe(pid: u32) -> io::Result<Option<ExitStatus>> {
    let pid = i32::try_from(pid)
        .ok()
        .filter(|pid| *pid > 0)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid child pid"))?;
    // SAFETY: waitid writes to a local initialized siginfo_t. P_PID restricts
    // observation to this pid; WNOWAIT preserves the owner's eventual reap.
    #[allow(unsafe_code)]
    unsafe {
        loop {
            let mut info: libc::siginfo_t = std::mem::zeroed();
            if libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOWAIT | libc::WNOHANG,
            ) != 0
            {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            #[cfg(target_os = "linux")]
            let (exited_pid, status) = (info.si_pid(), info.si_status());
            #[cfg(not(target_os = "linux"))]
            let (exited_pid, status) = (info.si_pid, info.si_status);
            if exited_pid == 0 {
                return Ok(None);
            }
            let raw = match info.si_code {
                libc::CLD_EXITED => status << 8,
                libc::CLD_KILLED => status,
                libc::CLD_DUMPED => status | 0x80,
                // The macOS kernel can report START_SUSPENDED stops despite WEXITED. Keep
                // those non-exit events unconsumed so the owned pin stays live.
                #[cfg(target_os = "macos")]
                libc::CLD_STOPPED | libc::CLD_CONTINUED => return Ok(None),
                _ => return Err(io::Error::other("unexpected child exit observation")),
            };
            return Ok(Some(ExitStatus::from_raw(raw)));
        }
    }
}

/// Arm a close-on-exec kqueue for the exit of a child the caller owns and
/// keeps unreaped. The returned descriptor owns the watcher, not the child;
/// the caller still observes and reaps that child separately.
#[cfg(target_os = "macos")]
pub fn watch_owned_child(pid: u32) -> io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    let pid = i32::try_from(pid)
        .ok()
        .filter(|pid| *pid > 0)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid child pid"))?;
    let _fork = crate::child_spawn::creation_lock();
    // SAFETY: kqueue returns a new descriptor on success. The fork guard
    // keeps other spawns from inheriting it before FD_CLOEXEC is set.
    #[allow(unsafe_code)]
    unsafe {
        let raw = libc::kqueue();
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = OwnedFd::from_raw_fd(raw);
        if libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) < 0 {
            return Err(io::Error::last_os_error());
        }
        let change = libc::kevent {
            ident: pid as libc::uintptr_t,
            filter: libc::EVFILT_PROC,
            flags: libc::EV_ADD | libc::EV_ONESHOT,
            fflags: libc::NOTE_EXIT,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        // Register one initialized change without reading events. A failed
        // registration (including ESRCH) closes the descriptor on return.
        if libc::kevent(
            fd.as_raw_fd(),
            &change,
            1,
            std::ptr::null_mut(),
            0,
            std::ptr::null(),
        ) < 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(fd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        process::{Child, Command, Stdio},
        time::{Duration, Instant},
    };

    struct OwnedChild(Child);

    impl Drop for OwnedChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn observed_exit(child: &OwnedChild) -> ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = observe(child.0.id()).unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "owned child did not exit");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn non_reaping_observation_keeps_the_owned_wait_status() {
        let mut child = OwnedChild(
            Command::new("/bin/sh")
                .args(["-c", "read line; exit 23"])
                .stdin(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        assert_eq!(observe(child.0.id()).unwrap(), None);
        drop(child.0.stdin.take());
        let status = observed_exit(&child);
        assert_eq!(status.code(), Some(23));
        assert_eq!(observe(child.0.id()).unwrap(), Some(status));
        assert_eq!(child.0.wait().unwrap(), status);
        assert!(
            observe(child.0.id()).is_err(),
            "an already-reaped child is not a reserved anchor"
        );

        let mut killed = OwnedChild(
            Command::new("/bin/sh")
                .args(["-c", "read line"])
                .stdin(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        killed.0.kill().unwrap();
        let status = observed_exit(&killed);
        assert_eq!(status.signal(), Some(libc::SIGKILL));
        assert_eq!(killed.0.wait().unwrap(), status);
        assert!(observe(0).is_err());
        assert!(observe(u32::MAX).is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn owned_child_kqueue_notifies_without_reaping() {
        use std::os::fd::AsRawFd;

        let mut child = OwnedChild(
            Command::new("/bin/sh")
                .args(["-c", "read line; exit 17"])
                .stdin(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let pid = child.0.id();
        let fd = watch_owned_child(pid).unwrap();
        assert_eq!(observe(pid).unwrap(), None);
        drop(child.0.stdin.take());
        // SAFETY: event is writable storage for one result; timeout is a
        // valid relative interval and the descriptor remains owned here.
        #[allow(unsafe_code)]
        unsafe {
            let mut event: libc::kevent = std::mem::zeroed();
            let timeout = libc::timespec {
                tv_sec: 5,
                tv_nsec: 0,
            };
            assert_eq!(
                libc::kevent(fd.as_raw_fd(), std::ptr::null(), 0, &mut event, 1, &timeout),
                1
            );
            let filter = event.filter;
            assert_eq!(filter, libc::EVFILT_PROC);
        }
        assert_eq!(observe(pid).unwrap().unwrap().code(), Some(17));
        assert_eq!(child.0.wait().unwrap().code(), Some(17));
        assert!(observe(pid).is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn suspended_child_is_not_an_exit_and_keeps_its_owned_wait_status() {
        let mut child = OwnedChild(
            Command::new("/bin/sh")
                .args(["-c", "kill -STOP $$; exit 23"])
                .spawn()
                .unwrap(),
        );
        let pid = child.0.id();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !crate::privacy_identity::SuspendedSignature::is_suspended(pid).unwrap() {
            assert!(Instant::now() < deadline, "child did not stop");
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(observe(pid).unwrap(), None);
        assert_eq!(observe(pid).unwrap(), None);
        // SAFETY: OwnedChild retains this stopped, unreaped child until wait.
        #[allow(unsafe_code)]
        let resumed = unsafe { libc::kill(pid as i32, libc::SIGCONT) };
        assert_eq!(resumed, 0);
        let status = observed_exit(&child);
        assert_eq!(status.code(), Some(23));
        assert_eq!(observe(pid).unwrap(), Some(status));
        assert_eq!(child.0.wait().unwrap(), status);
    }
}
