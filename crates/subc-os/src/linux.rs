//! Linux process identity through procfs and pidfds. Entirely safe code:
//! rustix wraps `pidfd_open`, `pidfd_send_signal`, `poll` and `kill`.

use std::{io, os::fd::OwnedFd};

use rustix::{
    event::{poll, PollFd, PollFlags, Timespec},
    io::Errno,
    process::{kill_process, pidfd_open, pidfd_send_signal, Pid, PidfdFlags},
};

use crate::{FileIdentity, Signal};

fn raw_pid(pid: u32) -> Option<Pid> {
    i32::try_from(pid).ok().and_then(Pid::from_raw)
}

/// `Ok(None)`: no such process. `Ok(Some(None))`: the process exists but no
/// pidfd could be opened, so the handle signals by pid.
pub(crate) fn open(pid: u32) -> io::Result<Option<Option<OwnedFd>>> {
    let Some(raw) = raw_pid(pid) else {
        return Ok(None);
    };
    match pidfd_open(raw, PidfdFlags::empty()) {
        Ok(pidfd) => Ok(Some(Some(pidfd))),
        Err(Errno::SRCH) => Ok(None),
        // ENOSYS: a kernel older than 5.3. EPERM or EACCES: a seccomp or LSM
        // policy refusing the call. Either way the pid-based path still works.
        Err(_) => Ok(start_time(pid).map(|_| None)),
    }
}

/// A pidfd becomes readable once its process has exited, whether or not it
/// has been reaped yet. Without a pidfd there is nothing to ask, so the pid
/// checks alone decide.
pub(crate) fn pidfd_alive(pidfd: Option<&OwnedFd>) -> bool {
    let Some(pidfd) = pidfd else {
        return true;
    };
    let mut fds = [PollFd::new(pidfd, PollFlags::IN)];
    match poll(&mut fds, Some(&Timespec::default())) {
        Ok(0) => true,
        Ok(_) => false,
        // Unknown: let the procfs checks decide rather than guess.
        Err(_) => true,
    }
}

pub(crate) fn start_time(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    start_time_from_stat(&stat)
}

/// Field 22 (`starttime`) of `/proc/<pid>/stat`, or `None` for a process that
/// has exited and awaits reaping (state `Z`) or is being torn down (`X`).
///
/// The second field is the command name in parentheses and may itself contain
/// spaces and `)`, so fields are counted from the last `)`: after it, the
/// state is the first field and the start time the twentieth.
pub(crate) fn start_time_from_stat(stat: &str) -> Option<u64> {
    let mut fields = stat.rsplit_once(')')?.1.split_whitespace();
    let state = fields.next()?;
    if matches!(state, "Z" | "X" | "x") {
        return None;
    }
    fields.nth(18)?.parse().ok()
}

/// `stat` through the `/proc/<pid>/exe` link, which names the image the
/// process is running even after its file was replaced or unlinked.
pub(crate) fn executable_identity(pid: u32) -> Option<FileIdentity> {
    crate::file_identity(std::path::Path::new(&format!("/proc/{pid}/exe")))
}

pub(crate) fn signal(pid: u32, pidfd: Option<&OwnedFd>, signal: Signal) -> Result<(), Errno> {
    let signal = match signal {
        Signal::Terminate => rustix::process::Signal::TERM,
        Signal::Kill => rustix::process::Signal::KILL,
    };
    match pidfd {
        Some(pidfd) => pidfd_send_signal(pidfd, signal),
        None => kill_process(raw_pid(pid).ok_or(Errno::SRCH)?, signal),
    }
}

#[cfg(test)]
mod tests {
    use super::start_time_from_stat;

    #[test]
    fn stat_parser_counts_fields_from_the_last_parenthesis() {
        let stat = "123 (foo) bar) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 424242 20";
        assert_eq!(start_time_from_stat(stat), Some(424242));
    }

    #[test]
    fn stat_parser_reads_a_zombie_as_exited() {
        let stat = "123 (foo) Z 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 424242 20";
        assert_eq!(start_time_from_stat(stat), None);
    }
}
