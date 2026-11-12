//! Keep a forked child before exec until its parent has recorded its identity.
//!
//! `Command::spawn` waits for exec, so the caller must run spawn on a blocking
//! worker while it waits for the PID and decides whether to release the child.
//! Serialize gate lifetimes before calling `install`: overlapping pre-exec
//! children could otherwise inherit each other's writers and prevent EOF when
//! their parent dies. Other children must exec without waiting on these gates.

use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Parent side of an admission barrier. Retain it until the blocking spawn call
/// returns, even after cancellation, so its writer number cannot be reused
/// before the child closes its inherited copy. Daemon death still produces EOF.
pub struct AdmissionGate {
    ready: std::io::PipeReader,
    go: Option<std::io::PipeWriter>,
    ready_seen: bool,
    cancelled: bool,
    released: bool,
    #[cfg(test)]
    descriptors: [RawFd; 4],
}

/// Installs the final readiness/permission callback after cgroup placement.
/// Keeping this separate lets the child report its PID before placement can
/// block, without permitting exec before placement has actually completed.
pub struct PlacementBarrier {
    report: Arc<OwnedFd>,
    permission: OwnedFd,
}

impl AdmissionGate {
    /// Register PID reporting before any callback that can block in child
    /// placement. Install the returned `PlacementBarrier` after those callbacks
    /// and before `LaunchNonceHandoff::install_last` replaces descriptor 3.
    /// Every descriptor remains CLOEXEC and is outside stdio and nonce fd 3.
    pub fn install(command: &mut Command) -> io::Result<(Self, PlacementBarrier)> {
        let (ready, report) = io::pipe()?;
        let (permission, go) = io::pipe()?;
        let ready = outside_stdio(ready.into())?;
        let report = outside_stdio(report.into())?;
        let permission = outside_stdio(permission.into())?;
        let go = outside_stdio(go.into())?;
        // Cancellation is a single byte and must never block a Drop backstop.
        // Only this parent writes, at most once for go and once for cancel.
        #[allow(unsafe_code)]
        unsafe {
            let flags = libc::fcntl(go.as_raw_fd(), libc::F_GETFL);
            if flags == -1
                || libc::fcntl(go.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) == -1
            {
                return Err(io::Error::last_os_error());
            }
        }
        #[cfg(test)]
        let descriptors = [
            ready.as_raw_fd(),
            report.as_raw_fd(),
            permission.as_raw_fd(),
            go.as_raw_fd(),
        ];
        let report = Arc::new(report);
        let child = ChildReport {
            parent_writer: go.as_raw_fd(),
            permission: permission.as_raw_fd(),
            report: Arc::clone(&report),
        };
        // SAFETY: the owned child ends live in the callback. The gate is run
        // with only close/poll/write/getpid/read/_exit, fixed stack buffers, no
        // allocation, and no Rust or OS locks. Its writer is owned only by the
        // caller, so cancelling really closes the parent's last go writer.
        #[allow(unsafe_code)]
        unsafe {
            command.pre_exec(move || child.report_pid());
        }
        Ok((
            Self {
                ready: ready.into(),
                go: Some(go.into()),
                ready_seen: false,
                cancelled: false,
                released: false,
                #[cfg(test)]
                descriptors,
            },
            PlacementBarrier { report, permission },
        ))
    }

    /// Read the PID reported by this command's forked child, without waiting for
    /// exec. No module instruction has run yet. A deadline failure does not
    /// authorize forgetting the child: cancel, collect its handle, and reap it.
    pub fn pid_within(&mut self, timeout: Duration) -> io::Result<u32> {
        let bytes = self.read_within::<{ size_of::<i32>() }>(timeout)?;
        u32::try_from(i32::from_ne_bytes(bytes))
            .ok()
            .filter(|pid| *pid > 1)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "admission child reported an invalid PID",
                )
            })
    }

    /// Confirm the child completed every placement callback. Durable recording
    /// may run concurrently, but neither success alone permits `release`.
    pub fn ready_within(&mut self, timeout: Duration) -> io::Result<()> {
        if self.read_within::<1>(timeout)? == *b"R" {
            self.ready_seen = true;
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "admission child reported invalid placement readiness",
            ))
        }
    }

    fn read_within<const N: usize>(&mut self, timeout: Duration) -> io::Result<[u8; N]> {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "admission child readiness timed out",
                ));
            }
            let mut pollfd = libc::pollfd {
                fd: self.ready.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let millis = remaining.as_millis().max(1).min(i32::MAX as u128) as i32;
            // SAFETY: poll receives one live, writable pollfd and a bounded wait.
            #[allow(unsafe_code)]
            let result = unsafe { libc::poll(&mut pollfd, 1, millis) };
            if result == 0 {
                continue;
            }
            if result < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            let mut bytes = [0; N];
            // One atomic pipe write reports the complete fixed-size PID, or
            // closes without reporting it. There are no other report writers.
            self.ready.read_exact(&mut bytes)?;
            return Ok(bytes);
        }
    }

    /// Permit exec exactly once. The caller must first finish the child's
    /// process-group/cgroup setup and persist its complete recovery identity to
    /// disk. It must reject an admission result that arrives after its deadline
    /// rather than release the child. Calling `cancel` makes release impossible.
    pub fn release(&mut self) -> io::Result<()> {
        if self.cancelled || self.released {
            return Err(io::Error::other("admission gate is already decided"));
        }
        if !self.ready_seen {
            return Err(io::Error::other(
                "admission child placement is not confirmed",
            ));
        }
        let writer = self
            .go
            .as_mut()
            .ok_or_else(|| io::Error::other("admission gate is already closed"))?;
        writer.write_all(b"G")?;
        self.released = true;
        Ok(())
    }

    /// Deny exec with a nonblocking cancel byte. Retain this gate until spawn
    /// returns, so the inherited writer cannot be mistaken for a reused fd.
    pub fn cancel(&mut self) {
        if !self.cancelled {
            self.cancelled = true;
            if let Some(writer) = &mut self.go {
                let _ = writer.write_all(b"C");
            }
        }
    }
}

fn outside_stdio(fd: OwnedFd) -> io::Result<OwnedFd> {
    if fd.as_raw_fd() > 3 {
        return Ok(fd);
    }
    // SAFETY: duplicate a live descriptor above the launch-reserved range.
    #[allow(unsafe_code)]
    let copy = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 4) };
    if copy < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fcntl returned a new descriptor, owned only by this value.
    #[allow(unsafe_code)]
    Ok(unsafe { OwnedFd::from_raw_fd(copy) })
}

struct ChildReport {
    report: Arc<OwnedFd>,
    parent_writer: RawFd,
    permission: RawFd,
}

impl ChildReport {
    fn report_pid(&self) -> io::Result<()> {
        // SAFETY: the callback owns report/permission. Close the inherited
        // parent writer FIRST so daemon death produces EOF. Every operation
        // below uses only calls safe in a signal handler and a constant refusal
        // error: no errno inspection, formatting, allocation or locking can touch
        // state inherited from other threads across fork.
        #[allow(unsafe_code)]
        unsafe {
            libc::close(self.parent_writer);
            let mut permission = libc::pollfd {
                fd: self.permission,
                events: libc::POLLIN,
                revents: 0,
            };
            if libc::poll(&mut permission, 1, 0) != 0 {
                // Any early byte (including cancellation), EOF or poll error
                // refuses execution before placement or even the PID report.
                return Err(io::Error::from_raw_os_error(libc::ECANCELED));
            }
            let bytes = libc::getpid().to_ne_bytes();
            if libc::write(self.report.as_raw_fd(), bytes.as_ptr().cast(), bytes.len())
                != bytes.len() as isize
            {
                libc::_exit(125);
            }
        }
        Ok(())
    }
}

impl PlacementBarrier {
    /// Register after all placement callbacks, before descriptor 3 is replaced.
    pub fn install(self, command: &mut Command) {
        // SAFETY: the callback owns both descriptors and makes only the fixed
        // write/read/_exit operations in `await_permission`, without allocation.
        #[allow(unsafe_code)]
        unsafe {
            command.pre_exec(move || self.await_permission());
        }
    }

    fn await_permission(&self) -> io::Result<()> {
        // SAFETY: only async-signal-safe calls on the owned pipe descriptors;
        // both messages fit in PIPE_BUF and all buffers are fixed-size stack data.
        #[allow(unsafe_code)]
        unsafe {
            let ready = *b"R";
            if libc::write(self.report.as_raw_fd(), ready.as_ptr().cast(), 1) != 1 {
                libc::_exit(125);
            }
            let mut go = [0u8; 1];
            if libc::read(self.permission.as_raw_fd(), go.as_mut_ptr().cast(), 1) != 1
                || go[0] != b'G'
            {
                libc::_exit(125);
            }
        }
        Ok(())
    }
}

/// Test-only stand-in for a placement syscall that has not completed yet.
#[cfg(feature = "test-support")]
#[derive(Debug)]
pub struct PlacementPause(OwnedFd);

#[cfg(feature = "test-support")]
impl PlacementPause {
    pub fn pipe() -> io::Result<(Self, std::io::PipeWriter)> {
        let (reader, writer) = io::pipe()?;
        Ok((Self(outside_stdio(reader.into())?), writer))
    }

    pub fn install(self, command: &mut Command) {
        // SAFETY: fixed-buffer read on an owned pipe, exactly like a placement
        // operation blocked in kernel I/O. No allocation or locks after fork.
        #[allow(unsafe_code)]
        unsafe {
            command.pre_exec(move || {
                let mut byte = [0u8];
                if libc::read(self.0.as_raw_fd(), byte.as_mut_ptr().cast(), 1) != 1 {
                    libc::_exit(126);
                }
                Ok(())
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cortexkit_test_support::ScratchDir;
    static GATES: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct OwnedReportedChild(u32, u64);
    impl OwnedReportedChild {
        fn capture(pid: u32) -> Self {
            assert_eq!(crate::parent_pid(pid), Some(std::process::id()));
            assert!(crate::child_exit::observe(pid).is_ok());
            Self(
                pid,
                crate::process_group::inspect(pid)
                    .unwrap()
                    .unwrap()
                    .start_time,
            )
        }
    }
    impl Drop for OwnedReportedChild {
        fn drop(&mut self) {
            if crate::child_exit::observe(self.0).is_ok()
                && crate::process_group::inspect(self.0)
                    .ok()
                    .flatten()
                    .is_some_and(|facts| facts.start_time == self.1 && !facts.exited)
            {
                if let Ok(Some(process)) = crate::Process::open(self.0) {
                    let _ = process.signal(crate::Signal::Kill);
                }
            }
        }
    }

    fn fixture(marker: &std::path::Path) -> Command {
        let fixture = cortexkit_test_support::cargo_profile_bin("subc-os-admission-fixture");
        assert!(
            fixture.is_file(),
            "build subc-os-admission-fixture with test-support before this test"
        );
        let mut command = Command::new(fixture);
        command.arg(marker);
        command
    }

    fn descriptor_identities(gate: &AdmissionGate) -> String {
        gate.descriptors
            .iter()
            .map(|fd| {
                // SAFETY: all four live descriptors are still owned by the gate or
                // the command. Recording their identity avoids mistaking a reused
                // numeric fd in the exec target for an inherited barrier pipe.
                #[allow(unsafe_code)]
                let facts = unsafe {
                    let mut facts: libc::stat = std::mem::zeroed();
                    assert_eq!(libc::fstat(*fd, &mut facts), 0);
                    facts
                };
                format!("{fd}:{}:{}", facts.st_dev, facts.st_ino)
            })
            .collect::<Vec<_>>()
            .join(",")
    }

    #[test]
    fn gate_blocks_exec_until_release_and_leaks_no_descriptors() {
        let _serialized = GATES.lock().unwrap_or_else(|p| p.into_inner());
        let dir = ScratchDir::new("admission-go");
        let marker = dir.join("exec");
        let mut command = fixture(&marker);
        let (mut gate, placement) = AdmissionGate::install(&mut command).unwrap();
        placement.install(&mut command);
        command.arg(descriptor_identities(&gate));
        let spawning = std::thread::spawn(move || command.spawn());
        let reported = gate.pid_within(Duration::from_secs(5)).unwrap();
        let _owned = OwnedReportedChild::capture(reported);
        gate.ready_within(Duration::from_secs(5)).unwrap();
        assert!(
            !marker.exists(),
            "no configured instruction may run before admission"
        );
        assert!(
            !spawning.is_finished(),
            "spawn must still await exec behind the gate"
        );
        gate.release().unwrap();
        let mut child = spawning.join().unwrap().unwrap();
        assert_eq!(
            child.id(),
            reported,
            "readiness PID must be the owned child"
        );
        assert!(
            child.wait().unwrap().success(),
            "exec target checks descriptor absence"
        );
        assert_eq!(std::fs::read_to_string(marker).unwrap(), "exec\n");
        assert!(gate.release().is_err(), "go can be sent only once");
    }

    #[test]
    fn gate_cancellation_exits_without_running_the_image() {
        let _serialized = GATES.lock().unwrap_or_else(|p| p.into_inner());
        let dir = ScratchDir::new("admission-cancel");
        let marker = dir.join("exec");
        let mut command = fixture(&marker);
        let (mut gate, placement) = AdmissionGate::install(&mut command).unwrap();
        placement.install(&mut command);
        command.arg(descriptor_identities(&gate));
        let spawning = std::thread::spawn(move || command.spawn());
        let reported = gate.pid_within(Duration::from_secs(5)).unwrap();
        let _owned = OwnedReportedChild::capture(reported);
        gate.cancel();
        assert!(
            gate.release().is_err(),
            "cancellation must never be reversible"
        );
        let mut child = spawning.join().unwrap().unwrap();
        assert_eq!(child.id(), reported);
        assert_eq!(child.wait().unwrap().code(), Some(125));
        assert!(
            !marker.exists(),
            "EOF must exit before any image instruction"
        );
    }

    #[test]
    fn pid_is_known_during_stalled_placement_and_go_requires_placement_ready() {
        let _serialized = GATES.lock().unwrap_or_else(|p| p.into_inner());
        let dir = ScratchDir::new("admission-placement");
        let marker = dir.join("exec");
        let mut command = fixture(&marker);
        let (mut gate, placement) = AdmissionGate::install(&mut command).unwrap();
        let (blocked, mut release) = io::pipe().unwrap();
        // SAFETY: this hook stands in for a blocked cgroup write. It owns its
        // pipe and performs only a fixed-buffer read between fork and exec.
        #[allow(unsafe_code)]
        unsafe {
            command.pre_exec(move || {
                let mut byte = [0u8];
                if libc::read(blocked.as_raw_fd(), byte.as_mut_ptr().cast(), 1) != 1 {
                    libc::_exit(126);
                }
                Ok(())
            });
        }
        placement.install(&mut command);
        command.arg(descriptor_identities(&gate));
        let spawning = std::thread::spawn(move || command.spawn());
        let reported = gate.pid_within(Duration::from_secs(5)).unwrap();
        let _owned = OwnedReportedChild::capture(reported);
        assert_eq!(
            gate.ready_within(Duration::from_millis(20))
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
        assert!(
            gate.release().is_err(),
            "durable identity alone cannot release unconfirmed placement"
        );
        assert!(!marker.exists());
        release.write_all(&[1]).unwrap();
        gate.ready_within(Duration::from_secs(5)).unwrap();
        gate.cancel();
        let mut child = spawning.join().unwrap().unwrap();
        assert_eq!(
            child.id(),
            reported,
            "identify the child even while placement stalls"
        );
        assert_eq!(child.wait().unwrap().code(), Some(125));
        assert!(!marker.exists());
    }

    #[test]
    fn cancel_before_fork_refuses_pid_report_and_placement_and_spawn_reaps_it() {
        let _serialized = GATES.lock().unwrap_or_else(|p| p.into_inner());
        let dir = ScratchDir::new("admission-prefork-cancel");
        let marker = dir.join("exec");
        let placed = dir.join("placement");
        let mut command = fixture(&marker);
        let (mut gate, placement) = AdmissionGate::install(&mut command).unwrap();
        let placement_file = std::fs::File::create(&placed).unwrap();
        // SAFETY: the placement probe writes a fixed byte to its owned file.
        #[allow(unsafe_code)]
        unsafe {
            command.pre_exec(move || {
                let byte = [1u8];
                libc::write(placement_file.as_raw_fd(), byte.as_ptr().cast(), 1);
                Ok(())
            });
        }
        placement.install(&mut command);
        command.arg(descriptor_identities(&gate));
        gate.cancel();
        let result = command.spawn();
        let refused = result.is_err();
        if let Ok(mut child) = result {
            let _ = child.wait();
        } else {
            assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::ECANCELED));
        }
        assert!(
            refused,
            "std must see the pre-exec refusal and reap the child itself"
        );
        assert_eq!(
            std::fs::metadata(placed).unwrap().len(),
            0,
            "early cancellation must refuse even placement, not just exec"
        );
        drop(command);
        assert!(
            gate.pid_within(Duration::from_millis(20)).is_err(),
            "cancelled child must not report a PID"
        );
        assert!(!marker.exists());
    }

    #[test]
    fn unknown_permission_byte_never_executes_the_configured_image() {
        let _serialized = GATES.lock().unwrap_or_else(|p| p.into_inner());
        let dir = ScratchDir::new("admission-unknown-permission");
        let marker = dir.join("exec");
        let mut command = fixture(&marker);
        let (mut gate, placement) = AdmissionGate::install(&mut command).unwrap();
        placement.install(&mut command);
        command.arg(descriptor_identities(&gate));
        let spawning = std::thread::spawn(move || command.spawn());
        let pid = gate.pid_within(Duration::from_secs(5)).unwrap();
        let _owned = OwnedReportedChild::capture(pid);
        gate.ready_within(Duration::from_secs(5)).unwrap();
        gate.go.as_mut().unwrap().write_all(b"X").unwrap();
        let mut child = spawning.join().unwrap().unwrap();
        assert_eq!(child.id(), pid);
        assert_eq!(child.wait().unwrap().code(), Some(125));
        assert!(!marker.exists(), "only G may authorize exec");
    }
}
