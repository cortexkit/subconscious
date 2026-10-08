//! PID-reuse-safe liveness checks using versioned kernel start-time identities.
//!
//! Callers must treat [`Liveness::Unknown`] as alive. A lease or scratch directory
//! may be reclaimed only on [`Liveness::Dead`]: an unreadable start time or a
//! different encoding family is never evidence that a PID was reused.
//! Identities are comparable only on the same host and within the same boot.

use std::fmt;

/// An opaque, versioned kernel start-time identity, suitable for persistence.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ProcessStart(String);

impl ProcessStart {
    /// The stored encoding, without normalization.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Accept a `linux-v1-<u64>` or `macos-v1-<i64>-<i64>` identity.
    /// Numbers must be decimal digits, with an optional minus sign for macOS.
    pub fn parse(s: &str) -> Option<Self> {
        if let Some(ticks) = s.strip_prefix("linux-v1-") {
            if !decimal_digits(ticks) || ticks.parse::<u64>().is_err() {
                return None;
            }
        } else {
            let numbers = s.strip_prefix("macos-v1-")?;
            // Skip the seconds' optional sign when locating the field separator.
            let unsigned = numbers.strip_prefix('-').unwrap_or(numbers);
            let (seconds, micros) = unsigned.split_once('-')?;
            let seconds_len = numbers.len() - unsigned.len() + seconds.len();
            if !decimal_digits(seconds)
                || !decimal_digits(micros.strip_prefix('-').unwrap_or(micros))
                || numbers[..seconds_len].parse::<i64>().is_err()
                || micros.parse::<i64>().is_err()
            {
                return None;
            }
        }
        Some(Self(s.to_owned()))
    }

    #[cfg(unix)]
    fn same_family(&self, other: &Self) -> bool {
        self.0.starts_with("linux-v1-") == other.0.starts_with("linux-v1-")
    }
}

fn decimal_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|byte| byte.is_ascii_digit())
}

impl fmt::Display for ProcessStart {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The result of a process existence and identity probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    /// The process exists and, if recorded, its start identity matches.
    Alive,
    /// The process is absent, a Linux zombie, or has a different compatible start.
    Dead,
    /// Invalid PID, unsupported platform, or an unreadable/incompatible identity.
    /// Callers must treat this as alive.
    Unknown,
}

/// Read a kernel start-time identity without spawning a process.
/// `None` means unknown, never dead. PID zero and PIDs outside `i32` are invalid.
pub fn process_start(pid: u32) -> Option<ProcessStart> {
    let pid = valid_pid(pid)?;
    platform_process_start(pid)
}

/// Check whether `pid` still names the recorded process.
///
/// Only `ESRCH` from signal zero proves absence; other errors (including `EPERM`)
/// continue to the identity check. With no recorded identity, an existing process
/// is alive. Linux zombies are dead even without a recorded identity. Incompatible
/// encodings or unreadable current starts are unknown, not evidence of PID reuse.
/// On non-Unix platforms this always returns [`Liveness::Unknown`].
pub fn liveness(pid: u32, recorded: Option<&ProcessStart>) -> Liveness {
    let Some(pid) = valid_pid(pid) else {
        return Liveness::Unknown;
    };
    #[cfg(unix)]
    {
        // SAFETY: pid is positive and representable as pid_t; signal zero probes
        // existence and permissions without sending a signal.
        #[allow(unsafe_code)]
        let result = unsafe { libc::kill(pid, 0) };
        if result != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
            return Liveness::Dead;
        }
        #[cfg(target_os = "linux")]
        if std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .is_some_and(|stat| state_from_stat(&stat) == Some("Z"))
        {
            return Liveness::Dead;
        }
        let Some(recorded) = recorded else {
            return Liveness::Alive;
        };
        let Some(current) = platform_process_start(pid) else {
            return Liveness::Unknown;
        };
        if current == *recorded {
            Liveness::Alive
        } else if current.same_family(recorded) {
            Liveness::Dead
        } else {
            // Older or platform-specific encodings cannot prove PID reuse.
            Liveness::Unknown
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (pid, recorded);
        Liveness::Unknown
    }
}

fn valid_pid(pid: u32) -> Option<i32> {
    i32::try_from(pid).ok().filter(|pid| *pid > 0)
}

#[cfg(target_os = "linux")]
fn platform_process_start(pid: i32) -> Option<ProcessStart> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let ticks = start_time_from_stat(&stat)?;
    Some(ProcessStart(format!("linux-v1-{ticks}")))
}

#[cfg(any(target_os = "linux", test))]
fn stat_fields(stat: &str) -> Option<std::str::SplitWhitespace<'_>> {
    // The comm field can contain spaces and parentheses. Field 3 begins after
    // the last closing parenthesis, not the first one.
    Some(stat.rsplit_once(')')?.1.split_whitespace())
}

#[cfg(any(target_os = "linux", test))]
fn start_time_from_stat(stat: &str) -> Option<u64> {
    // Field 22 is the twentieth field after comm. Do not reject zombie starts:
    // the kernel identity remains readable until the process is reaped.
    stat_fields(stat)?.nth(19)?.parse().ok()
}

#[cfg(any(target_os = "linux", test))]
fn state_from_stat(stat: &str) -> Option<&str> {
    stat_fields(stat)?.next()
}

#[cfg(target_os = "macos")]
fn platform_process_start(pid: i32) -> Option<ProcessStart> {
    // Initialize the entire buffer in safe Rust, so reading the completed reply
    // needs no unsafe initialization or pointer dereference.
    let mut info = libc::proc_bsdinfo {
        pbi_flags: 0,
        pbi_status: 0,
        pbi_xstatus: 0,
        pbi_pid: 0,
        pbi_ppid: 0,
        pbi_uid: 0,
        pbi_gid: 0,
        pbi_ruid: 0,
        pbi_rgid: 0,
        pbi_svuid: 0,
        pbi_svgid: 0,
        rfu_1: 0,
        pbi_comm: [0; 16],
        pbi_name: [0; 32],
        pbi_nfiles: 0,
        pbi_pgid: 0,
        pbi_pjobc: 0,
        e_tdev: 0,
        e_tpgid: 0,
        pbi_nice: 0,
        pbi_start_tvsec: 0,
        pbi_start_tvusec: 0,
    };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
    // SAFETY: info is an initialized, aligned, writable buffer of exactly size
    // bytes. proc_pidinfo writes at most size bytes; the return count is checked.
    #[allow(unsafe_code)]
    let read = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            std::ptr::from_mut(&mut info).cast(),
            size,
        )
    };
    if read != size {
        return None;
    }
    Some(ProcessStart(format!(
        "macos-v1-{}-{}",
        info.pbi_start_tvsec, info.pbi_start_tvusec
    )))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn platform_process_start(_pid: i32) -> Option<ProcessStart> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_both_versioned_encodings() {
        for encoding in [
            "linux-v1-0",
            "linux-v1-18446744073709551615",
            "macos-v1-123456789-987654",
            "macos-v1--9223372036854775808--9223372036854775808",
            "macos-v1-9223372036854775807-9223372036854775807",
        ] {
            let start = ProcessStart::parse(encoding).expect("valid identity");
            assert_eq!(start.as_str(), encoding);
            assert_eq!(start.to_string(), encoding);
        }
    }

    #[test]
    fn parse_rejects_malformed_encodings() {
        for encoding in [
            "",
            "garbage",
            "linux-v2-1",
            "other-v1-1",
            // A wrong prefix followed by well-formed numbers must still fail, so
            // the check can't pass merely because the remainder is malformed.
            "macos-v2-1-2",
            "Macos-v1-1-2",
            "xxxxxxxxx1-2",
            "linux-v2-5",
            "linux-v1-",
            "linux-v1--1",
            "linux-v1-+1",
            "linux-v1-1-2",
            "linux-v1-18446744073709551616",
            "macos-v1-",
            "macos-v1-1",
            "macos-v1-1-",
            "macos-v1--1-",
            "macos-v1-1-2-3",
            "macos-v1-+1-2",
            "macos-v1-1-+2",
            "macos-v1-9223372036854775808-0",
            "macos-v1-0--9223372036854775809",
            "linux-v1- 1",
            "macos-v1-1-2\n",
        ] {
            assert_eq!(ProcessStart::parse(encoding), None, "{encoding:?}");
        }
    }

    #[test]
    fn invalid_pids_are_unknown() {
        let recorded = ProcessStart::parse("linux-v1-1").unwrap();
        for pid in [0, u32::MAX] {
            assert_eq!(process_start(pid), None);
            assert_eq!(liveness(pid, None), Liveness::Unknown);
            assert_eq!(liveness(pid, Some(&recorded)), Liveness::Unknown);
        }
    }

    #[test]
    fn stat_parser_uses_the_last_parenthesis_and_field_22() {
        let stat = "123 ((a b) (c)) R 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 4242";
        assert_eq!(start_time_from_stat(stat), Some(4242));
        assert_eq!(state_from_stat(stat), Some("R"));
    }

    #[test]
    fn stat_parser_rejects_truncated_or_invalid_start_times() {
        for stat in [
            "",
            "123 (comm)",
            "123 (comm) R 4 5",
            "123 (comm) R 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21",
            "123 (comm) R 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 invalid",
        ] {
            assert_eq!(start_time_from_stat(stat), None, "{stat:?}");
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn own_pid_with_matching_start_is_alive() {
        let pid = std::process::id();
        let start = process_start(pid).expect("own kernel start is readable");
        assert_eq!(ProcessStart::parse(start.as_str()), Some(start.clone()));
        assert_eq!(liveness(pid, Some(&start)), Liveness::Alive);
    }

    #[cfg(unix)]
    #[test]
    fn own_pid_without_recorded_start_is_alive() {
        assert_eq!(liveness(std::process::id(), None), Liveness::Alive);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn own_pid_with_different_same_family_start_is_dead() {
        let pid = std::process::id();
        let current = process_start(pid).unwrap();
        let encoding = if cfg!(target_os = "linux") {
            "linux-v1-0"
        } else {
            "macos-v1-0-0"
        };
        let mut recorded = ProcessStart::parse(encoding).unwrap();
        if recorded == current {
            recorded = ProcessStart::parse(&encoding.replace("-0", "-1")).unwrap();
        }
        assert_ne!(current, recorded);
        assert_eq!(liveness(pid, Some(&recorded)), Liveness::Dead);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn own_pid_with_cross_family_start_is_unknown() {
        let encoding = if cfg!(target_os = "linux") {
            "macos-v1-0-0"
        } else {
            "linux-v1-0"
        };
        let recorded = ProcessStart::parse(encoding).unwrap();
        assert_eq!(
            liveness(std::process::id(), Some(&recorded)),
            Liveness::Unknown
        );
    }

    #[cfg(unix)]
    #[test]
    fn exited_and_reaped_child_is_dead() {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        let recorded = process_start(pid);
        child.wait().unwrap();
        assert_eq!(liveness(pid, None), Liveness::Dead);
        assert_eq!(liveness(pid, recorded.as_ref()), Liveness::Dead);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn exited_unreaped_zombie_child_is_dead() {
        use std::time::{Duration, Instant};

        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        let deadline = Instant::now() + Duration::from_secs(5);
        let zombie = loop {
            // Observe procfs directly, independently of the liveness probe. Do
            // not call try_wait: that would reap the child and miss the case.
            let zombie = std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .is_some_and(|stat| {
                    stat.rsplit_once(')')
                        .and_then(|(_, fields)| fields.split_whitespace().next())
                        == Some("Z")
                });
            if zombie {
                break true;
            }
            if Instant::now() >= deadline {
                break false;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let recorded = process_start(pid);
        let without_start = liveness(pid, None);
        let with_start = liveness(pid, recorded.as_ref());
        // Reap before assertions so a failing liveness test leaves no zombie.
        child.wait().unwrap();
        assert!(zombie, "child did not reach state Z within five seconds");
        assert!(
            recorded.is_some(),
            "unreaped zombie still has a kernel start"
        );
        assert_eq!(without_start, Liveness::Dead);
        assert_eq!(with_start, Liveness::Dead);
    }

    #[cfg(not(unix))]
    #[test]
    fn unsupported_platform_is_unknown() {
        let pid = std::process::id();
        let recorded = ProcessStart::parse("linux-v1-1").unwrap();
        assert_eq!(process_start(pid), None);
        assert_eq!(liveness(pid, None), Liveness::Unknown);
        assert_eq!(liveness(pid, Some(&recorded)), Liveness::Unknown);
    }
}
