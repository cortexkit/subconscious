//! Process-group facts for recovery after a daemon no longer keeps its original
//! group leader as an unreaped child to reserve the group's numeric identity.
//!
//! These observations do not authorize a signal by themselves. Callers must
//! compare the group, owner and start time with their recorded container, and
//! repeat the observation immediately before signalling a process.

use std::io;

/// Kernel facts for one process. Linux exposes unreaped zombies; macOS may
/// instead report an exited process as gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Identity {
    pub pid: u32,
    pub pgid: u32,
    /// The kernel session ID. A shared group number alone is not membership
    /// evidence for a recovered container.
    pub session_id: u32,
    /// Effective user ID, matching `ps -o uid=`.
    pub uid: u32,
    /// Linux clock ticks since boot; macOS microseconds since the Unix epoch.
    pub start_time: u64,
    pub exited: bool,
}

/// Inspect one PID. A process that disappears during inspection is `Ok(None)`.
pub fn inspect(pid: u32) -> io::Result<Option<Identity>> {
    if pid <= 1 || i32::try_from(pid).is_err() {
        return Ok(None);
    }
    #[cfg(target_os = "linux")]
    return inspect_linux(pid, |path| std::fs::read_to_string(path));
    #[cfg(target_os = "macos")]
    return inspect_macos(pid);
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "process-group inspection is unavailable",
    ))
}

/// Read a process's current session without treating permission errors as exit.
/// A successful zero is preserved: the session leader can be outside this PID
/// namespace. Absence and failed reads are distinct from that kernel value.
pub fn session_id(pid: u32) -> io::Result<Option<u32>> {
    let pid = i32::try_from(pid)
        .ok()
        .filter(|pid| *pid > 1)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid session-query PID"))?;
    // SAFETY: getsid only reads a kernel process identifier and takes no pointer.
    #[allow(unsafe_code)]
    let session = unsafe { libc::getsid(pid) };
    if session == -1 {
        let error = io::Error::last_os_error();
        return if gone(&error) { Ok(None) } else { Err(error) };
    }
    u32::try_from(session)
        .ok()
        .map(Some)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid kernel session ID"))
}

/// Read just the kernel process-group ID, without requiring image inspection.
/// A missing process is `Ok(None)`; this does not establish ownership or identity.
pub fn pgid(pid: u32) -> io::Result<Option<u32>> {
    let Some(pid) = i32::try_from(pid).ok().filter(|pid| *pid > 1) else {
        return Ok(None);
    };
    // SAFETY: getpgid takes a validated numeric PID and has no pointer arguments.
    #[allow(unsafe_code)]
    let group = unsafe { libc::getpgid(pid) };
    if group >= 0 {
        return Ok(Some(group as u32));
    }
    let error = io::Error::last_os_error();
    if gone(&error) {
        Ok(None)
    } else {
        Err(error)
    }
}

/// List candidate PIDs, not verified group members. Every PID needs inspection.
pub fn pids() -> io::Result<Vec<u32>> {
    #[cfg(target_os = "linux")]
    {
        let mut pids = Vec::new();
        for entry in std::fs::read_dir("/proc")? {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) if gone(&error) => continue,
                Err(error) => return Err(error),
            };
            if let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u32>().ok())
                .filter(|pid| *pid > 1)
            {
                pids.push(pid);
            }
        }
        Ok(pids)
    }
    #[cfg(target_os = "macos")]
    return pids_macos();
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "process listing is unavailable",
    ))
}

fn gone(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound || error.raw_os_error() == Some(libc::ESRCH)
}

#[cfg(target_os = "linux")]
fn inspect_linux(
    pid: u32,
    mut read: impl FnMut(&std::path::Path) -> io::Result<String>,
) -> io::Result<Option<Identity>> {
    let root = std::path::PathBuf::from(format!("/proc/{pid}"));
    let sample = || {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid procfs process identity",
        )
    };
    let mut read_existing = |name: &str| match read(&root.join(name)) {
        Ok(text) => Ok(Some(text)),
        Err(error) if gone(&error) => Ok(None),
        Err(error) => Err(error),
    };
    let Some(stat) = read_existing("stat")? else {
        return Ok(None);
    };
    let parse_stat = |stat: &str| -> Option<(u32, u32, u64, bool)> {
        let (_, fields) = stat.rsplit_once(") ")?;
        let fields: Vec<_> = fields.split_whitespace().collect();
        Some((
            fields.get(2)?.parse().ok()?,
            fields.get(3)?.parse().ok()?,
            fields.get(19)?.parse().ok()?,
            matches!(*fields.first()?, "Z" | "X"),
        ))
    };
    let Some((pgid, session_id, start_time, exited)) = parse_stat(&stat) else {
        return Err(sample());
    };
    let Some(status) = read_existing("status")? else {
        return Ok(None);
    };
    let uid = status
        .lines()
        .find_map(|line| {
            line.strip_prefix("Uid:")
                .and_then(|value| value.split_whitespace().nth(1))
        })
        .and_then(|value| value.parse::<u32>().ok())
        .ok_or_else(sample)?;
    let Some(after) = read_existing("stat")? else {
        return Ok(None);
    };
    let Some((after_pgid, after_session, after_start, after_exited)) = parse_stat(&after) else {
        return Err(sample());
    };
    if (pgid, session_id, start_time) != (after_pgid, after_session, after_start) {
        // Do not combine one process's ownership with another process's group.
        return Ok(None);
    }
    Ok(Some(Identity {
        pid,
        pgid,
        session_id,
        uid,
        start_time,
        exited: exited || after_exited,
    }))
}

#[cfg(target_os = "macos")]
fn inspect_macos(pid: u32) -> io::Result<Option<Identity>> {
    // Darwin's SZOMB process state: exited but not yet reaped.
    const SZOMB: u32 = 5;
    // SAFETY: libc's C output structure is correctly sized and aligned; its
    // integer fields admit zero, and proc_pidinfo writes at most that size.
    #[allow(unsafe_code)]
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = i32::try_from(std::mem::size_of_val(&info)).expect("proc_bsdinfo fits c_int");
    // SAFETY: the writable buffer has exactly `size` bytes. No pointer escapes.
    #[allow(unsafe_code)]
    let read = unsafe {
        libc::proc_pidinfo(
            pid as i32,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    if read <= 0 {
        let error = io::Error::last_os_error();
        return if gone(&error) { Ok(None) } else { Err(error) };
    }
    if read != size || info.pbi_pid != pid {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "incomplete proc_bsdinfo identity",
        ));
    }
    let start_time = info
        .pbi_start_tvsec
        .checked_mul(1_000_000)
        .and_then(|seconds| seconds.checked_add(info.pbi_start_tvusec))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid process birth time"))?;
    Ok(Some(Identity {
        pid,
        pgid: info.pbi_pgid,
        session_id: match session_id(pid)? {
            Some(session) => session,
            None => return Ok(None),
        },
        uid: info.pbi_uid,
        start_time,
        exited: info.pbi_status == SZOMB,
    }))
}

#[cfg(target_os = "macos")]
fn pids_macos() -> io::Result<Vec<u32>> {
    // libproc.h's all-processes selector is not exported by the libc crate.
    const PROC_ALL_PIDS: u32 = 1;
    // SAFETY: a null, zero-sized output buffer asks libproc for its size only.
    #[allow(unsafe_code)]
    let needed = unsafe { libc::proc_listpids(PROC_ALL_PIDS, 0, std::ptr::null_mut(), 0) };
    if needed <= 0 {
        return Err(io::Error::last_os_error());
    }
    let mut count = needed as usize / std::mem::size_of::<libc::pid_t>() + 64;
    for _ in 0..4 {
        let mut pids = vec![0 as libc::pid_t; count];
        let size = i32::try_from(std::mem::size_of_val(pids.as_slice()))
            .map_err(|_| io::Error::other("process list exceeds libproc's buffer limit"))?;
        // SAFETY: the allocation is aligned for pid_t and has `size` bytes.
        #[allow(unsafe_code)]
        let read = unsafe { libc::proc_listpids(PROC_ALL_PIDS, 0, pids.as_mut_ptr().cast(), size) };
        if read <= 0 {
            return Err(io::Error::last_os_error());
        }
        if read < size {
            pids.truncate(read as usize / std::mem::size_of::<libc::pid_t>());
            return Ok(pids
                .into_iter()
                .filter_map(|pid| u32::try_from(pid).ok())
                .filter(|pid| *pid > 1)
                .collect());
        }
        count *= 2;
    }
    Err(io::Error::new(
        io::ErrorKind::WouldBlock,
        "process list kept growing during inspection",
    ))
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    fn ps_birth_seconds(parts: &[&str]) -> u64 {
        let months = [
            "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
        ];
        let clock: Vec<i32> = parts[3].split(':').map(|n| n.parse().unwrap()).collect();
        // SAFETY: tm is an integer/pointer C structure with a valid zero value.
        #[allow(unsafe_code)]
        let mut birth: libc::tm = unsafe { std::mem::zeroed() };
        birth.tm_mon = months.iter().position(|month| *month == parts[1]).unwrap() as i32;
        birth.tm_mday = parts[2].parse().unwrap();
        birth.tm_year = parts[4].parse::<i32>().unwrap() - 1900;
        birth.tm_hour = clock[0];
        birth.tm_min = clock[1];
        birth.tm_sec = clock[2];
        // SAFETY: timegm receives one fully initialized, writable tm and does
        // not retain its address. UTC avoids changing the test process's TZ.
        #[allow(unsafe_code)]
        u64::try_from(unsafe { libc::timegm(&mut birth) }).unwrap()
    }

    fn assert_matches_ps(pid: u32) {
        let facts = inspect(pid).unwrap().expect("our process is still alive");
        // SAFETY: the test reads the session of its own process or owned child.
        #[allow(unsafe_code)]
        let expected_session = u32::try_from(unsafe { libc::getsid(pid as i32) }).unwrap();
        assert_eq!(facts.session_id, expected_session, "inspection session");
        assert_eq!(
            session_id(pid).unwrap(),
            Some(expected_session),
            "fresh kernel session query"
        );
        let output = Command::new("ps")
            .env("LC_ALL", "C")
            .env("TZ", "UTC0")
            .args(["-p", &pid.to_string(), "-o", "uid=,pgid=,lstart="])
            .output()
            .unwrap();
        assert!(output.status.success(), "ps: {:?}", output);
        let text = String::from_utf8(output.stdout).unwrap();
        let fields: Vec<_> = text.split_whitespace().collect();
        assert_eq!(fields.len(), 7, "ps identity: {text}");
        assert_eq!(facts.pid, pid);
        assert_eq!(facts.uid, fields[0].parse::<u32>().unwrap(), "owner");
        assert_eq!(facts.pgid, fields[1].parse::<u32>().unwrap(), "group");
        assert_eq!(
            pgid(pid).unwrap(),
            Some(fields[1].parse::<u32>().unwrap()),
            "group-only query"
        );
        #[cfg(target_os = "macos")]
        let birth = facts.start_time / 1_000_000;
        #[cfg(target_os = "linux")]
        let birth = {
            let boot = std::fs::read_to_string("/proc/stat")
                .unwrap()
                .lines()
                .find_map(|line| {
                    line.strip_prefix("btime ")
                        .and_then(|n| n.parse::<u64>().ok())
                })
                .unwrap();
            // SAFETY: sysconf takes a constant selector, with no pointers.
            #[allow(unsafe_code)]
            let ticks = u64::try_from(unsafe { libc::sysconf(libc::_SC_CLK_TCK) }).unwrap();
            boot + facts.start_time / ticks
        };
        assert_eq!(
            birth,
            ps_birth_seconds(&fields[2..]),
            "birth time: {facts:?} vs {text}"
        );
        assert!(!facts.exited);
        assert!(
            pids().unwrap().contains(&pid),
            "PID enumeration includes our process"
        );
    }

    #[test]
    fn process_group_facts_match_ps_for_self_and_owned_child() {
        let mut child = Command::new("sleep")
            .arg("2")
            .process_group(0)
            .spawn()
            .unwrap();
        assert_matches_ps(std::process::id());
        assert_matches_ps(child.id());
        assert!(child.wait().unwrap().success());
        assert_eq!(inspect(child.id()).unwrap(), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn process_group_exit_between_procfs_reads_is_gone() {
        let mut child = Command::new("sleep").arg("1").spawn().unwrap();
        let pid = child.id();
        let result = inspect_linux(pid, |path| {
            if path.file_name().unwrap() == "status" {
                assert!(child.wait().unwrap().success());
            }
            std::fs::read_to_string(path)
        });
        assert!(child.wait().unwrap().success());
        assert_eq!(result.unwrap(), None);
    }
}
