//! Linux process identity through procfs and pidfds. Entirely safe code:
//! rustix wraps `pidfd_open`, `pidfd_send_signal`, `poll` and `kill`.

use std::{io, os::fd::OwnedFd};

use rustix::{
    event::{poll, PollFd, PollFlags, Timespec},
    io::Errno,
    process::{kill_process, pidfd_open, pidfd_send_signal, Pid, PidfdFlags},
};

use crate::{FileIdentity, MemoryKind, ResourceUsage, Signal};

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

/// Memory and CPU time of `pid` from procfs, or `None` if there is no such
/// process, it has exited (a zombie has no memory lines in `status`), or a
/// file cannot be read or parsed.
///
/// CPU time is `utime` and `stime` (fields 14 and 15) of `/proc/<pid>/stat`,
/// in clock ticks; memory is `VmRSS` of `/proc/<pid>/status`, with `VmSwap`
/// when the kernel reports it. Both files cover the process itself: threads
/// are included, children are not.
pub(crate) fn resource_usage(pid: u32) -> Option<ResourceUsage> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (user_ticks, system_ticks) = cpu_ticks_from_stat(&stat)?;
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let (memory_bytes, swap_bytes) = memory_from_status(&status)?;
    let ticks_per_second = rustix::param::clock_ticks_per_second().max(1);
    let ticks = |count: u64| {
        let nanos = u128::from(count) * 1_000_000_000 / u128::from(ticks_per_second);
        std::time::Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
    };
    Some(ResourceUsage {
        memory_bytes,
        memory_kind: MemoryKind::ResidentSet,
        swap_bytes,
        cpu_user: ticks(user_ticks),
        cpu_system: ticks(system_ticks),
    })
}

/// `utime` and `stime` of `/proc/<pid>/stat`, or `None` for a zombie or a
/// process being torn down. Fields are counted from the last `)` for the
/// reason given on [`start_time_from_stat`]: after it, the state is the first
/// field, `utime` the twelfth and `stime` the thirteenth.
pub(crate) fn cpu_ticks_from_stat(stat: &str) -> Option<(u64, u64)> {
    let mut fields = stat.rsplit_once(')')?.1.split_whitespace();
    let state = fields.next()?;
    if matches!(state, "Z" | "X" | "x") {
        return None;
    }
    let user = fields.nth(10)?.parse().ok()?;
    let system = fields.next()?.parse().ok()?;
    Some((user, system))
}

/// `VmRSS` and `VmSwap` of `/proc/<pid>/status`, in bytes (the file reports
/// kB). `None` without a `VmRSS` line, which is how a zombie reads.
pub(crate) fn memory_from_status(status: &str) -> Option<(u64, Option<u64>)> {
    let kilobytes = |key: &str| {
        status.lines().find_map(|line| {
            let value = line.strip_prefix(key)?.strip_prefix(':')?;
            let number = value.trim().strip_suffix("kB")?.trim();
            number.parse::<u64>().ok()?.checked_mul(1024)
        })
    };
    Some((kilobytes("VmRSS")?, kilobytes("VmSwap")))
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
    use super::{cpu_ticks_from_stat, memory_from_status, start_time_from_stat};

    #[test]
    fn stat_parser_reads_user_and_system_ticks() {
        // Fields 4 to 13 are 1..=10, then utime 250 and stime 40.
        let stat = "123 (foo) bar) S 1 2 3 4 5 6 7 8 9 10 250 40 16 17 18 19 20 21 424242 23";
        assert_eq!(cpu_ticks_from_stat(stat), Some((250, 40)));
        assert_eq!(start_time_from_stat(stat), Some(424242));
        let zombie = "123 (foo) Z 1 2 3 4 5 6 7 8 9 10 250 40 16 17 18 19 20 21 424242 23";
        assert_eq!(cpu_ticks_from_stat(zombie), None);
    }

    #[test]
    fn status_parser_reads_rss_and_swap_in_bytes() {
        let status = "Name:\tfoo\nVmRSS:\t    2048 kB\nVmSwap:\t       0 kB\n";
        assert_eq!(memory_from_status(status), Some((2048 * 1024, Some(0))));
        // A zombie's status has no memory lines at all.
        assert_eq!(memory_from_status("Name:\tfoo\nState:\tZ (zombie)\n"), None);
    }

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
