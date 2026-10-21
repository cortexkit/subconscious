//! Facts about the machine as a whole: its memory, its load and the space on a
//! filesystem, read now, for reporting only.
//!
//! Each reader returns `None` when the platform has no source for the value or
//! the read failed, never a zero in its place: a dashboard that shows "0 GB
//! free" for a failed read is making a claim the machine never made.
//!
//! Sources, per platform:
//!
//! - Linux: memory from `/proc/meminfo` (`MemTotal`, `MemAvailable`), load
//!   from `getloadavg`, filesystem space from `statvfs`.
//! - macOS: total memory from `sysctl` `hw.memsize`; available memory is the
//!   free plus inactive pages `host_statistics64` reports (the pages the kernel
//!   can hand out without paging anything to disk), times the page size. Load
//!   from `getloadavg`, filesystem space from `statvfs`.
//! - Windows: memory from `GlobalMemoryStatusEx`, filesystem space from
//!   `GetDiskFreeSpaceExW`. Windows keeps no load average, so
//!   [`load_average`] is always `None` there.

use std::path::Path;

/// The machine's physical memory, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryInfo {
    pub total_bytes: u64,
    /// Memory that can be given to programs without swapping: free memory
    /// plus caches the kernel can drop. Not the same as "unused".
    pub available_bytes: u64,
}

/// Space on one filesystem, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskSpace {
    pub total_bytes: u64,
    /// Bytes an unprivileged program may still write. On Unix this leaves out
    /// the blocks reserved for the superuser, so `total - free` can exceed
    /// what `df` lists as used.
    pub free_bytes: u64,
}

/// The machine's physical memory, read now.
pub fn memory() -> Option<MemoryInfo> {
    platform_memory()
}

/// The 1, 5 and 15 minute load averages: the average number of runnable (and,
/// on Linux, uninterruptible) threads over each span. `None` on Windows, which
/// keeps no such figure.
pub fn load_average() -> Option<[f64; 3]> {
    #[cfg(unix)]
    {
        let mut loads = [0f64; 3];
        // SAFETY: `getloadavg` writes at most `nelem` doubles into the buffer
        // it is given; the buffer holds exactly three and we pass three.
        #[allow(unsafe_code)]
        let n = unsafe { libc::getloadavg(loads.as_mut_ptr(), 3) };
        (n == 3).then_some(loads)
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// Total and free space of the filesystem holding `path`, read now.
pub fn disk_space(path: &Path) -> Option<DiskSpace> {
    platform_disk_space(path)
}

#[cfg(target_os = "linux")]
fn platform_memory() -> Option<MemoryInfo> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    parse_meminfo(&text)
}

/// Reads `MemTotal` and `MemAvailable` (kibibytes) from `/proc/meminfo` text.
#[cfg(any(target_os = "linux", test))]
fn parse_meminfo(text: &str) -> Option<MemoryInfo> {
    let field = |name: &str| -> Option<u64> {
        let line = text.lines().find(|l| l.starts_with(name))?;
        let rest = line[name.len()..].trim_start_matches(':').trim();
        let kib: u64 = rest.split_whitespace().next()?.parse().ok()?;
        kib.checked_mul(1024)
    };
    Some(MemoryInfo {
        total_bytes: field("MemTotal")?,
        available_bytes: field("MemAvailable")?,
    })
}

#[cfg(target_os = "macos")]
fn platform_memory() -> Option<MemoryInfo> {
    let total_bytes = macos_memsize()?;
    let page_size = macos_page_size()?;
    let (free, inactive) = macos_free_and_inactive_pages()?;
    Some(MemoryInfo {
        total_bytes,
        available_bytes: (free + inactive).saturating_mul(page_size).min(total_bytes),
    })
}

#[cfg(target_os = "macos")]
fn macos_memsize() -> Option<u64> {
    let mut value: u64 = 0;
    let mut len: libc::size_t = std::mem::size_of::<u64>();
    // SAFETY: the name is a NUL-terminated C string; `value` is a writable
    // u64 and `len` says exactly its size, which `hw.memsize` (a 64-bit
    // integer) fills completely.
    #[allow(unsafe_code)]
    let result = unsafe {
        libc::sysctlbyname(
            c"hw.memsize".as_ptr(),
            (&mut value as *mut u64).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    (result == 0 && len == std::mem::size_of::<u64>() && value > 0).then_some(value)
}

#[cfg(target_os = "macos")]
fn macos_page_size() -> Option<u64> {
    // SAFETY: `sysconf` only reads a configuration value.
    #[allow(unsafe_code)]
    let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    u64::try_from(size).ok().filter(|s| *s > 0)
}

#[cfg(target_os = "macos")]
#[allow(deprecated)] // libc points Mach users at the mach2 crate; these calls are stable.
fn macos_free_and_inactive_pages() -> Option<(u64, u64)> {
    // SAFETY: `vm_statistics64` is a plain C struct of integers, so all-zero
    // bytes are a valid value.
    #[allow(unsafe_code)]
    let mut stats: libc::vm_statistics64 = unsafe { std::mem::zeroed() };
    let mut count = libc::HOST_VM_INFO64_COUNT;
    // `mach_host_self` adds a reference to the task's send right for the host
    // port on every call. Asking once and keeping the right for the life of
    // the process bounds that to one reference instead of one per reading.
    static HOST_PORT: std::sync::OnceLock<libc::mach_port_t> = std::sync::OnceLock::new();
    // SAFETY: `mach_host_self` takes no arguments and only returns a port name.
    #[allow(unsafe_code)]
    let host = *HOST_PORT.get_or_init(|| unsafe { libc::mach_host_self() });
    // SAFETY: `stats` is a writable vm_statistics64 and `count` is its size in
    // natural-width integers, as HOST_VM_INFO64 requires; the kernel writes at
    // most `count` of them. `host` is a send right this process holds.
    #[allow(unsafe_code)]
    let result = unsafe {
        libc::host_statistics64(
            host,
            libc::HOST_VM_INFO64,
            (&mut stats as *mut libc::vm_statistics64).cast(),
            &mut count,
        )
    };
    (result == libc::KERN_SUCCESS)
        .then(|| (u64::from(stats.free_count), u64::from(stats.inactive_count)))
}

#[cfg(windows)]
fn platform_memory() -> Option<MemoryInfo> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    // SAFETY: MEMORYSTATUSEX is a plain struct of integers; zero is valid.
    #[allow(unsafe_code)]
    let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    // SAFETY: `status` is writable and its `dwLength` names its size, as the
    // call requires.
    #[allow(unsafe_code)]
    let ok = unsafe { GlobalMemoryStatusEx(&mut status) };
    (ok != 0 && status.ullTotalPhys > 0).then_some(MemoryInfo {
        total_bytes: status.ullTotalPhys,
        available_bytes: status.ullAvailPhys,
    })
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn platform_memory() -> Option<MemoryInfo> {
    None
}

#[cfg(unix)]
fn platform_disk_space(path: &Path) -> Option<DiskSpace> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `statvfs` is a plain C struct of integers; zero is valid.
    #[allow(unsafe_code)]
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c_path` is NUL-terminated and outlives the call; `st` is a
    // writable statvfs.
    #[allow(unsafe_code)]
    let result = unsafe { libc::statvfs(c_path.as_ptr(), &mut st) };
    if result != 0 {
        return None;
    }
    // Block counts are in units of the fragment size, `f_frsize`.
    #[allow(clippy::unnecessary_cast)] // the field widths differ by platform
    let unit = st.f_frsize as u64;
    #[allow(clippy::unnecessary_cast)]
    let (blocks, avail) = (st.f_blocks as u64, st.f_bavail as u64);
    let total_bytes = blocks.checked_mul(unit)?;
    (total_bytes > 0).then(|| DiskSpace {
        total_bytes,
        free_bytes: avail.saturating_mul(unit),
    })
}

#[cfg(windows)]
fn platform_disk_space(path: &Path) -> Option<DiskSpace> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let (mut free_to_caller, mut total, mut _free_total) = (0u64, 0u64, 0u64);
    // SAFETY: `wide` is NUL-terminated and outlives the call; the three
    // out-pointers are writable u64s.
    #[allow(unsafe_code)]
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut free_to_caller,
            &mut total,
            &mut _free_total,
        )
    };
    (ok != 0 && total > 0).then_some(DiskSpace {
        total_bytes: total,
        free_bytes: free_to_caller,
    })
}

#[cfg(not(any(unix, windows)))]
fn platform_disk_space(_path: &Path) -> Option<DiskSpace> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meminfo_text_parses_to_bytes() {
        let text = "MemTotal:       16314952 kB\nMemFree:  1000 kB\nMemAvailable:   12002000 kB\n";
        assert_eq!(
            parse_meminfo(text),
            Some(MemoryInfo {
                total_bytes: 16_314_952 * 1024,
                available_bytes: 12_002_000 * 1024,
            })
        );
        assert_eq!(parse_meminfo("MemTotal: 5 kB\n"), None);
    }

    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[test]
    fn memory_is_plausible() {
        let m = memory().expect("this platform reports memory");
        // At least 64 MiB and at most 64 TiB of RAM, and available within it.
        assert!(m.total_bytes >= 64 << 20, "{m:?}");
        assert!(m.total_bytes <= 64 << 40, "{m:?}");
        assert!(m.available_bytes > 0, "{m:?}");
        assert!(m.available_bytes <= m.total_bytes, "{m:?}");
    }

    #[cfg(unix)]
    #[test]
    fn load_average_is_plausible() {
        let [one, five, fifteen] = load_average().expect("unix reports load");
        for l in [one, five, fifteen] {
            assert!(l.is_finite() && l >= 0.0, "{one} {five} {fifteen}");
            // No test machine runs a hundred thousand runnable threads.
            assert!(l < 100_000.0, "{one} {five} {fifteen}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_has_no_load_average() {
        assert_eq!(load_average(), None);
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn disk_space_is_plausible() {
        let dir = std::env::temp_dir();
        let d = disk_space(&dir).expect("the temp directory's filesystem reports space");
        assert!(d.total_bytes >= 1 << 20, "{d:?}");
        assert!(d.free_bytes <= d.total_bytes, "{d:?}");
    }

    #[test]
    fn a_missing_path_has_no_disk_space() {
        assert_eq!(
            disk_space(Path::new("/definitely/not/a/real/path/for/subc-os")),
            None
        );
    }
}
