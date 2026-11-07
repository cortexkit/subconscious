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
                _ => return Err(io::Error::other("unexpected child exit observation")),
            };
            return Ok(Some(ExitStatus::from_raw(raw)));
        }
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
}
