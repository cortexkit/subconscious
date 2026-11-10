//! macOS process identity through `sysctl(KERN_PROC_PID)` and `proc_pidpath`,
//! and resource usage through `proc_pid_rusage` and `mach_timebase_info`.
//! Parent pids come from `proc_pidinfo(PROC_PIDTBSDINFO)`, falling back to
//! the current parent in `sysctl(KERN_PROC_PID)` for protected processes.

use std::{
    ffi::{c_int, c_void, OsStr},
    os::unix::ffi::OsStrExt,
    path::Path,
    sync::OnceLock,
    time::Duration,
};

use rustix::{
    io::Errno,
    process::{kill_process, Pid},
};

use crate::{FileIdentity, MemoryKind, ResourceUsage, Signal};

/// `sizeof(struct kinfo_proc)` on 64-bit macOS (arm64 and x86_64). The libc
/// crate does not define the struct, so it is read as bytes at the offsets
/// below, and a reply of any other length is refused rather than misread.
const KINFO_PROC_SIZE: usize = 648;
/// `kp_proc.p_un.__p_starttime.tv_sec` (a `long`), then `tv_usec` (an `int`).
const STARTTIME_SEC_OFFSET: usize = 0;
const STARTTIME_USEC_OFFSET: usize = 8;
/// `kp_proc.p_stat` (a `char`), after the 16-byte union, two pointers and `p_flag`.
const P_STAT_OFFSET: usize = 36;
/// `kp_proc.p_pid` (a `pid_t`), checked against the requested pid so a layout
/// change is caught instead of read as some other process's start time.
const P_PID_OFFSET: usize = 40;
/// `kp_eproc.e_ppid` (a `pid_t`): the current parent, not `p_oppid`'s saved
/// original parent. The 64-bit Apple SDK places it at 560 in the 648-byte record.
const PARENT_PID_OFFSET: usize = 560;
/// `SZOMB`: exited, waiting for its parent to reap it.
const SZOMB: u8 = 5;

/// A buffer with the alignment the kernel's `kinfo_proc` has.
#[repr(C, align(8))]
struct KinfoProc([u8; KINFO_PROC_SIZE]);

fn kinfo_proc(pid: u32) -> Option<KinfoProc> {
    read_kinfo_proc(pid).ok().flatten()
}

fn read_kinfo_proc(pid: u32) -> Result<Option<KinfoProc>, ()> {
    let pid = c_int::try_from(pid).ok().filter(|pid| *pid > 0).ok_or(())?;
    let mut mib = [libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_PID, pid];
    let mut info = KinfoProc([0; KINFO_PROC_SIZE]);
    let mut len: libc::size_t = KINFO_PROC_SIZE;
    // SAFETY: `mib` is a valid array of `mib.len()` ints; `info` is a writable
    // buffer of `len` bytes, aligned like `struct kinfo_proc`, and the kernel
    // writes at most `len` bytes and stores the count it wrote back in `len`.
    // No new value is set (`newp` null, `newlen` 0).
    #[allow(unsafe_code)]
    let result = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            info.0.as_mut_ptr().cast::<c_void>(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    // A pid with no process succeeds with nothing written.
    if result != 0 {
        return Err(());
    }
    if len == 0 {
        return Ok(None);
    }
    if len != KINFO_PROC_SIZE {
        return Err(());
    }
    let reported_pid = i32::from_ne_bytes(
        info.0[P_PID_OFFSET..P_PID_OFFSET + 4]
            .try_into()
            .map_err(|_| ())?,
    );
    if reported_pid != pid {
        return Err(());
    }
    Ok(Some(info))
}

pub(crate) fn read_identity(pid: u32) -> crate::IdentityRead {
    use crate::IdentityRead;
    match read_kinfo_proc(pid) {
        Ok(None) => IdentityRead::Gone,
        Ok(Some(info)) if info.0[P_STAT_OFFSET] == SZOMB => IdentityRead::Gone,
        Ok(Some(info)) => birth_time(&info).map_or(IdentityRead::Unknown, IdentityRead::Alive),
        Err(()) => IdentityRead::Unknown,
    }
}

pub(crate) fn exists(pid: u32) -> bool {
    start_time(pid).is_some()
}

pub(crate) fn parent_pid(pid: u32) -> Option<u32> {
    parent_pid_using(pid, bsd_parent_pid)
}

fn parent_pid_using(pid: u32, bsd_parent: impl FnOnce(u32) -> Option<u32>) -> Option<u32> {
    bsd_parent(pid).or_else(|| {
        // Protected system processes can refuse proc_pidinfo while exposing
        // their ordinary sysctl identity. Resolve their parent rather than
        // making every unrelated process tree structurally unknown.
        let info = kinfo_proc(pid)?;
        if info.0[P_STAT_OFFSET] == SZOMB {
            return None;
        }
        let parent = i32::from_ne_bytes(
            info.0[PARENT_PID_OFFSET..PARENT_PID_OFFSET + 4]
                .try_into()
                .ok()?,
        );
        u32::try_from(parent).ok()
    })
}

fn bsd_parent_pid(pid: u32) -> Option<u32> {
    let pid = c_int::try_from(pid).ok()?;
    // SAFETY: proc_bsdinfo is a C output structure; all-zero bytes are valid.
    #[allow(unsafe_code)]
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of_val(&info) as c_int;
    // SAFETY: info points to a writable, correctly aligned buffer of size bytes.
    #[allow(unsafe_code)]
    let read = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    (read == size && info.pbi_pid == pid as u32 && info.pbi_status != u32::from(SZOMB))
        .then_some(info.pbi_ppid)
}

/// Microseconds since the epoch at which the process started, or `None` if it
/// does not exist or is a zombie.
pub(crate) fn start_time(pid: u32) -> Option<u64> {
    let info = kinfo_proc(pid)?;
    if info.0[P_STAT_OFFSET] == SZOMB {
        return None;
    }
    birth_time(&info)
}

/// Exact birth time of an owned, unreaped child, including a zombie.
///
/// This verifies the birth of a first group leader retained unreaped to reserve
/// the group's numeric identity, not ordinary process liveness. Non-reaping
/// wait observation first proves that the caller owns the child. Missing or
/// malformed kernel identity is an error, never evidence that the pin is gone.
pub fn owned_child_start_time(pid: u32) -> std::io::Result<u64> {
    crate::child_exit::observe(pid)?;
    let invalid = || {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "could not verify the owned child's exact kernel birth time",
        )
    };
    let info = kinfo_proc(pid).ok_or_else(invalid)?;
    birth_time(&info).ok_or_else(invalid)
}

fn birth_time(info: &KinfoProc) -> Option<u64> {
    let seconds = i64::from_ne_bytes(
        info.0[STARTTIME_SEC_OFFSET..STARTTIME_SEC_OFFSET + 8]
            .try_into()
            .ok()?,
    );
    let micros = i32::from_ne_bytes(
        info.0[STARTTIME_USEC_OFFSET..STARTTIME_USEC_OFFSET + 4]
            .try_into()
            .ok()?,
    );
    let seconds = u64::try_from(seconds).ok()?;
    let micros = u64::try_from(micros).ok()?;
    seconds.checked_mul(1_000_000)?.checked_add(micros)
}

/// The device and inode of the file at the path `proc_pidpath` reports.
///
/// Unlike Linux's `/proc/<pid>/exe`, this is a path, so if the file has been
/// replaced since the process started, the identity read here is the
/// replacement's and will not match what was recorded at spawn. That errs on
/// the side of not signalling.
pub(crate) fn executable_identity(pid: u32) -> Option<FileIdentity> {
    let pid = c_int::try_from(pid).ok()?;
    let mut buffer = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: `buffer` is a writable allocation of exactly the length passed;
    // `proc_pidpath` writes at most that many bytes and returns how many it
    // wrote, or a value <= 0 on failure.
    #[allow(unsafe_code)]
    let written = unsafe {
        libc::proc_pidpath(
            pid,
            buffer.as_mut_ptr().cast::<c_void>(),
            buffer.len() as u32,
        )
    };
    let written = usize::try_from(written).ok().filter(|&n| n > 0)?;
    buffer.truncate(written.min(buffer.len()));
    crate::file_identity(Path::new(OsStr::from_bytes(&buffer)))
}

/// Memory and CPU time of `pid` from `proc_pid_rusage(RUSAGE_INFO_V2)`, or
/// `None` if there is no such process, it is a zombie, or the call fails.
///
/// Memory is `ri_phys_footprint`, the figure Activity Monitor and jetsam use,
/// rather than `ri_resident_size`: resident size keeps counting pages an
/// allocator has released with `MADV_FREE` until the kernel reclaims them, so
/// it overstates a long-lived process that allocates and frees heavily.
pub(crate) fn resource_usage(pid: u32) -> Option<ResourceUsage> {
    resource_usage_with_children(pid, false)
}

pub(crate) fn resource_usage_with_children(pid: u32, children: bool) -> Option<ResourceUsage> {
    let (mut usage, child_user, child_system) = resource_usage_parts(pid)?;
    if children {
        usage.cpu_user += child_user;
        usage.cpu_system += child_system;
    }
    Some(usage)
}

pub(crate) fn resource_usage_parts(
    pid: u32,
) -> Option<(ResourceUsage, std::time::Duration, std::time::Duration)> {
    let raw_pid = c_int::try_from(pid).ok()?;
    let mut info = libc::rusage_info_v2 {
        ri_uuid: [0; 16],
        ri_user_time: 0,
        ri_system_time: 0,
        ri_pkg_idle_wkups: 0,
        ri_interrupt_wkups: 0,
        ri_pageins: 0,
        ri_wired_size: 0,
        ri_resident_size: 0,
        ri_phys_footprint: 0,
        ri_proc_start_abstime: 0,
        ri_proc_exit_abstime: 0,
        ri_child_user_time: 0,
        ri_child_system_time: 0,
        ri_child_pkg_idle_wkups: 0,
        ri_child_interrupt_wkups: 0,
        ri_child_pageins: 0,
        ri_child_elapsed_abstime: 0,
        ri_diskio_bytesread: 0,
        ri_diskio_byteswritten: 0,
    };
    // SAFETY: `info` is a live, writable `rusage_info_v2`, which is exactly
    // what the `RUSAGE_INFO_V2` flavor tells the kernel to write. The
    // parameter is declared as `rusage_info_t *` (a pointer to a void
    // pointer) for historical reasons, but the kernel treats it as the
    // address of the struct, so the cast is the documented usage. The call
    // returns 0 on success and -1 (with errno) otherwise.
    #[allow(unsafe_code)]
    let result = unsafe {
        libc::proc_pid_rusage(
            raw_pid,
            libc::RUSAGE_INFO_V2,
            (&mut info as *mut libc::rusage_info_v2).cast::<libc::rusage_info_t>(),
        )
    };
    if result != 0 {
        return None;
    }
    // A zombie still answers with the usage it had when it exited. Checked
    // after the read so a process that exits in between is not reported as
    // running.
    start_time(pid)?;
    Some((
        ResourceUsage {
            memory_bytes: info.ri_phys_footprint,
            memory_kind: MemoryKind::PhysFootprint,
            swap_bytes: None,
            cpu_user: mach_ticks_to_duration(info.ri_user_time)?,
            cpu_system: mach_ticks_to_duration(info.ri_system_time)?,
        },
        mach_ticks_to_duration(info.ri_child_user_time)?,
        mach_ticks_to_duration(info.ri_child_system_time)?,
    ))
}

/// `ri_user_time` and `ri_system_time` count Mach absolute-time ticks, not
/// nanoseconds. The two are the same on Intel Macs, but on Apple silicon a
/// tick is 125/3 ns, so an unconverted value would read about 24 times low.
fn mach_ticks_to_duration(ticks: u64) -> Option<Duration> {
    let (numer, denom) = mach_timebase()?;
    let nanos = u128::from(ticks) * u128::from(numer) / u128::from(denom);
    Some(Duration::from_nanos(
        u64::try_from(nanos).unwrap_or(u64::MAX),
    ))
}

/// The tick-to-nanosecond ratio, fixed for the life of the system, so it is
/// cached only after a successful nonzero ratio. A failed read stays unknown
/// and can be retried; it must not silently use a 1:1 tick-to-nanosecond
/// ratio on machines whose clocks use different units.
///
/// libc marks its Mach bindings deprecated in favour of the `mach2` crate;
/// this one struct and function do not justify a new dependency.
#[allow(deprecated)]
fn mach_timebase() -> Option<(u32, u32)> {
    static TIMEBASE: OnceLock<(u32, u32)> = OnceLock::new();
    timebase_using(&TIMEBASE, || {
        let mut info = libc::mach_timebase_info { numer: 0, denom: 0 };
        // SAFETY: `info` is a live, writable `mach_timebase_info` struct, the
        // only thing the call writes to. It returns KERN_SUCCESS (0) on
        // success.
        #[allow(unsafe_code)]
        let result = unsafe { libc::mach_timebase_info(&mut info) };
        if result == 0 && info.numer != 0 && info.denom != 0 {
            Some((info.numer, info.denom))
        } else {
            None
        }
    })
}

fn timebase_using(
    cache: &OnceLock<(u32, u32)>,
    read: impl FnOnce() -> Option<(u32, u32)>,
) -> Option<(u32, u32)> {
    if let Some(ratio) = cache.get() {
        return Some(*ratio);
    }
    let ratio = read().filter(|(numer, denom)| *numer != 0 && *denom != 0)?;
    let _ = cache.set(ratio);
    cache.get().copied()
}

#[cfg(test)]
mod timebase_tests {
    #[test]
    fn a_failed_timebase_is_unknown_and_not_cached_as_intel_units() {
        let cache = std::sync::OnceLock::new();
        assert_eq!(super::timebase_using(&cache, || None), None);
        assert!(cache.get().is_none());
        assert_eq!(super::timebase_using(&cache, || Some((125, 0))), None);
        assert_eq!(
            super::timebase_using(&cache, || Some((125, 3))),
            Some((125, 3))
        );
        assert_eq!(
            super::timebase_using(&cache, || panic!("a successful ratio is reused")),
            Some((125, 3))
        );
    }
}

pub(crate) fn signal(pid: u32, signal: Signal) -> Result<(), Errno> {
    let raw = i32::try_from(pid)
        .ok()
        .and_then(Pid::from_raw)
        .ok_or(Errno::SRCH)?;
    let signal = match signal {
        Signal::Terminate => rustix::process::Signal::TERM,
        Signal::Kill => rustix::process::Signal::KILL,
    };
    kill_process(raw, signal)
}

#[cfg(test)]
mod owned_child_birth_tests {
    use super::*;

    #[test]
    fn owned_child_birth_survives_exit_without_reaping_and_refuses_non_children() {
        struct OwnedChild(std::process::Child);
        impl Drop for OwnedChild {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let mut child = OwnedChild(
            std::process::Command::new("/bin/sleep")
                .arg("0.05")
                .spawn()
                .unwrap(),
        );
        let pid = child.0.id();
        let birth = owned_child_start_time(pid).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while crate::child_exit::observe(pid).unwrap().is_none() {
            assert!(
                std::time::Instant::now() < deadline,
                "owned child did not exit"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(owned_child_start_time(pid).unwrap(), birth);
        assert!(
            start_time(pid).is_none(),
            "ordinary liveness still filters zombies"
        );
        assert!(
            owned_child_start_time(std::process::id()).is_err(),
            "a process that is not our child cannot authorize pin ownership"
        );
        assert!(child.0.wait().unwrap().success());
        assert!(
            owned_child_start_time(pid).is_err(),
            "a reaped child is not owned"
        );
    }
}

#[cfg(test)]
mod parent_tests {
    #[test]
    fn current_parent_is_readable_when_bsd_process_info_is_refused() {
        // SAFETY: getppid reads the caller's parent and takes no pointers.
        #[allow(unsafe_code)]
        let expected = unsafe { libc::getppid() } as u32;
        assert_eq!(
            super::parent_pid_using(std::process::id(), |_| None),
            Some(expected),
            "a denied BSD query must not hide the sysctl record's current parent"
        );
        assert_eq!(
            super::parent_pid_using(u32::MAX, |_| None),
            None,
            "the fallback must still refuse an invalid process identity"
        );
    }
}
