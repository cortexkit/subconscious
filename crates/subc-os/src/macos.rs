//! macOS process identity through `sysctl(KERN_PROC_PID)` and `proc_pidpath`.
//! These two foreign calls are this crate's only unsafe code.

use std::{
    ffi::{c_int, c_void, OsStr},
    os::unix::ffi::OsStrExt,
    path::Path,
};

use rustix::{
    io::Errno,
    process::{kill_process, Pid},
};

use crate::{FileIdentity, Signal};

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
/// `SZOMB`: exited, waiting for its parent to reap it.
const SZOMB: u8 = 5;

/// A buffer with the alignment the kernel's `kinfo_proc` has.
#[repr(C, align(8))]
struct KinfoProc([u8; KINFO_PROC_SIZE]);

fn kinfo_proc(pid: u32) -> Option<KinfoProc> {
    let pid = c_int::try_from(pid).ok()?;
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
    if result != 0 || len != KINFO_PROC_SIZE {
        return None;
    }
    let reported_pid = i32::from_ne_bytes(info.0[P_PID_OFFSET..P_PID_OFFSET + 4].try_into().ok()?);
    (reported_pid == pid).then_some(info)
}

pub(crate) fn exists(pid: u32) -> bool {
    start_time(pid).is_some()
}

/// Microseconds since the epoch at which the process started, or `None` if it
/// does not exist or is a zombie.
pub(crate) fn start_time(pid: u32) -> Option<u64> {
    let info = kinfo_proc(pid)?;
    if info.0[P_STAT_OFFSET] == SZOMB {
        return None;
    }
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
