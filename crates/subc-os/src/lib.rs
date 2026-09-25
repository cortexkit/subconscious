//! Operating-system primitives the subc daemon needs and cannot reach without
//! unsafe code, each behind a small safe API.
//!
//! The daemon crates forbid unsafe code. This crate is the one deliberate
//! exception (like `subc-uptime` and `subc-cgroup`): every `unsafe` block here
//! is a single foreign call with its preconditions stated beside it, and nothing
//! unsafe is exported.
//!
//! Today it answers one question: is the process now holding pid N the same
//! process the daemon spawned earlier? A pid alone cannot say, because the
//! kernel reuses pids once a process has been reaped. [`Process`] reads the two
//! facts that tell processes apart, the kernel's start time for the pid and the
//! file identity (device and inode) of the executable image it runs, and sends
//! signals to it.
//!
//! Sources, per platform:
//!
//! - Linux: the start time is field 22 of `/proc/<pid>/stat` (clock ticks since
//!   boot), and the executable is `stat` through `/proc/<pid>/exe`, which
//!   resolves to the running image even if its file has since been replaced or
//!   deleted. A pidfd is opened before either is read and signals go through it
//!   (`pidfd_send_signal`), so the process that was checked is the process that
//!   is signalled. No unsafe code is needed: rustix wraps both calls.
//! - macOS: the start time is `kp_proc.p_starttime` from `sysctl`
//!   `KERN_PROC_PID` (microseconds since the epoch), and the executable is the
//!   path `proc_pidpath` reports, then `stat` on that path. These two calls are
//!   the crate's only unsafe code. macOS has no pidfd, so a signal is a plain
//!   `kill` sent right after the checks; see [`Process::signal`].
//! - Anywhere else: [`Process::open`] reports [`std::io::ErrorKind::Unsupported`].
//!
//! It also reads how much memory and CPU time one process is using, for
//! reporting only; see [`resource_usage`]. On Linux that is procfs again; on
//! macOS it is `proc_pid_rusage`, plus `mach_timebase_info` to convert its CPU
//! times to nanoseconds, the other two unsafe calls in the crate.

#![deny(unsafe_code)]

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "linux")]
use linux as platform;
#[cfg(target_os = "macos")]
use macos as platform;

use std::{io, path::Path};

/// True where [`Process`] can identify and signal a process by pid.
pub const PROCESS_IDENTITY_SUPPORTED: bool = cfg!(any(target_os = "linux", target_os = "macos"));

/// Device and inode of a file: which file, independent of the name used to
/// reach it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileIdentity {
    pub device: u64,
    pub inode: u64,
}

/// The device and inode of the file at `path`, following symlinks. `None` if it
/// cannot be read or the platform has no inode numbers.
pub fn file_identity(path: &Path) -> Option<FileIdentity> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        std::fs::metadata(path).ok().map(|metadata| FileIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// What a live process looks like right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Observation {
    /// The kernel's start time for the process. Opaque: compare it only with a
    /// value read on the same host by this crate. Linux counts clock ticks since
    /// boot; macOS counts microseconds since the epoch.
    pub start_time: u64,
    /// The file the process is executing, or `None` if it could not be read
    /// (for example, a process owned by another user).
    pub executable: Option<FileIdentity>,
}

/// A signal [`Process::signal`] can send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// SIGTERM: a request to exit, which the process may handle or ignore.
    Terminate,
    /// SIGKILL: ends the process; it cannot be handled or ignored.
    Kill,
}

/// The kernel start time of the process holding `pid`, or `None` if there is
/// none, it has already exited (a zombie waiting to be reaped counts as exited),
/// or the platform has no source.
pub fn start_time(pid: u32) -> Option<u64> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        platform::start_time(pid)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        None
    }
}

/// True where [`resource_usage`] can read a live process. Elsewhere it always
/// answers `None`, and a caller can use this to say "not supported here"
/// rather than "could not read".
pub const RESOURCE_USAGE_SUPPORTED: bool = cfg!(any(target_os = "linux", target_os = "macos"));

/// What [`ResourceUsage::memory_bytes`] measures. The platforms offer
/// different figures, and they are not interchangeable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryKind {
    /// macOS `phys_footprint`: the memory the kernel charges to the process
    /// (dirty and compressed pages, among others), which is also what jetsam
    /// acts on. Pages an allocator has released with `MADV_FREE` do not count.
    PhysFootprint,
    /// Linux `VmRSS`: pages of the process resident in RAM, including shared
    /// file-backed pages. Swapped-out pages are not included; see
    /// [`ResourceUsage::swap_bytes`].
    ResidentSet,
}

/// One reading of a process's memory and cumulative CPU time.
///
/// It covers the process named by the pid alone: its threads are included,
/// processes it has started are not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceUsage {
    /// Memory in bytes, measured as [`Self::memory_kind`] says.
    pub memory_bytes: u64,
    pub memory_kind: MemoryKind,
    /// Bytes swapped out (Linux `VmSwap`). `None` where the platform does not
    /// report it for a single process, which is not the same as zero.
    pub swap_bytes: Option<u64>,
    /// CPU time spent in user mode since the process started.
    pub cpu_user: std::time::Duration,
    /// CPU time spent in the kernel on the process's behalf since it started.
    pub cpu_system: std::time::Duration,
}

/// Memory and cumulative CPU time of the process holding `pid`, read now.
///
/// `None` when there is no such process, it has exited (a zombie awaiting its
/// reap counts as exited), it cannot be read (for example, another user's
/// process on macOS), or the platform has no source
/// (see [`RESOURCE_USAGE_SUPPORTED`]). Never a reading of zeros in place of
/// one of those.
///
/// Like any pid-based read, this describes whatever process holds `pid` now;
/// a caller that needs it to be a particular process should confirm that
/// process's [`start_time`] around the call.
pub fn resource_usage(pid: u32) -> Option<ResourceUsage> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        platform::resource_usage(pid)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        None
    }
}

/// A handle on the process holding one pid at the moment it was opened.
#[derive(Debug)]
pub struct Process {
    pid: u32,
    #[cfg(target_os = "linux")]
    pidfd: Option<std::os::fd::OwnedFd>,
}

impl Process {
    /// Open a handle on the process now holding `pid`.
    ///
    /// `Ok(None)` means no process holds that pid (on macOS, also a zombie
    /// awaiting its reap; on Linux a zombie opens, and [`Self::observe`] then
    /// reports it as exited). On Linux this opens a pidfd,
    /// which from then on refers to this exact process even if it exits and the
    /// pid is reused; when the kernel cannot open one (older than 5.3, or a
    /// seccomp policy refusing the call) the handle falls back to the pid, as
    /// on macOS.
    pub fn open(pid: u32) -> io::Result<Option<Self>> {
        #[cfg(target_os = "linux")]
        {
            linux::open(pid).map(|opened| opened.map(|pidfd| Self { pid, pidfd }))
        }
        #[cfg(target_os = "macos")]
        {
            Ok(platform::exists(pid).then_some(Self { pid }))
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = pid;
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "process identity is not available on this platform",
            ))
        }
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// True when signals go through a pidfd, so they cannot reach a different
    /// process that has since reused this pid.
    pub fn signals_through_pidfd(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            self.pidfd.is_some()
        }
        #[cfg(not(target_os = "linux"))]
        {
            false
        }
    }

    /// The process's start time and executable, or `None` once it has exited
    /// (including as a zombie not yet reaped by its parent).
    pub fn observe(&self) -> Option<Observation> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            #[cfg(target_os = "linux")]
            if !linux::pidfd_alive(self.pidfd.as_ref()) {
                return None;
            }
            let start_time = platform::start_time(self.pid)?;
            Some(Observation {
                start_time,
                executable: platform::executable_identity(self.pid),
            })
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            None
        }
    }

    /// Send `signal` to the process.
    ///
    /// With a pidfd the signal can only reach the process this handle was
    /// opened on: if that process has exited, the call fails with `ESRCH` even
    /// if the pid has been reused. Without one (macOS, or a Linux kernel with no
    /// pidfd) the signal goes to whatever holds the pid now, so callers should
    /// [`Self::observe`] immediately before signalling. What remains is the
    /// time between that check and this call; for a different process to be
    /// hit, the checked one must exit, be reaped, and have its pid handed to a
    /// new process inside that window, and both kernels hand out pids in
    /// increasing order, so a reuse needs the whole pid space to wrap first.
    ///
    /// `Ok(false)` means the process had already exited (`ESRCH`).
    pub fn signal(&self, signal: Signal) -> io::Result<bool> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            #[cfg(target_os = "linux")]
            let result = linux::signal(self.pid, self.pidfd.as_ref(), signal);
            #[cfg(target_os = "macos")]
            let result = macos::signal(self.pid, signal);
            match result {
                Ok(()) => Ok(true),
                Err(rustix::io::Errno::SRCH) => Ok(false),
                Err(error) => Err(error.into()),
            }
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = signal;
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "process signalling is not available on this platform",
            ))
        }
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use std::{
        process::{Child, Command},
        time::{Duration, Instant},
    };

    use super::*;

    fn spawn_sleep() -> Child {
        Command::new("sleep")
            .arg("60")
            .spawn()
            .expect("spawn sleep")
    }

    /// The executable a spawned `sleep` runs, resolved the way `Command` found it.
    fn sleep_identity() -> FileIdentity {
        let path = ["/bin/sleep", "/usr/bin/sleep"]
            .into_iter()
            .find(|path| Path::new(path).exists())
            .expect("sleep is installed");
        file_identity(Path::new(path)).expect("stat sleep")
    }

    /// Right after `spawn` returns the child may not have finished exec yet,
    /// and until then it still runs the test binary's image.
    fn wait_for_executable(process: &Process, expected: FileIdentity) -> Observation {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let observation = process.observe().expect("child is alive");
            if observation.executable == Some(expected) || Instant::now() > deadline {
                return observation;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn own_process_is_observable_with_its_own_image() {
        let process = Process::open(std::process::id())
            .expect("open own process")
            .expect("own process exists");
        let observation = process.observe().expect("own process is alive");
        let own_image = file_identity(&std::env::current_exe().unwrap()).unwrap();
        assert_eq!(observation.executable, Some(own_image));
        assert_eq!(start_time(std::process::id()), Some(observation.start_time));
    }

    #[test]
    fn child_start_time_is_stable_and_differs_from_ours() {
        let mut child = spawn_sleep();
        let pid = child.id();
        let process = Process::open(pid).unwrap().unwrap();
        let observation = wait_for_executable(&process, sleep_identity());
        assert_eq!(observation.executable, Some(sleep_identity()));
        assert_eq!(start_time(pid), Some(observation.start_time));
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn a_signalled_and_unreaped_child_reads_as_exited() {
        let mut child = spawn_sleep();
        let process = Process::open(child.id()).unwrap().unwrap();
        assert!(process.signal(Signal::Terminate).unwrap());
        let deadline = Instant::now() + Duration::from_secs(5);
        while process.observe().is_some() {
            assert!(Instant::now() < deadline, "child still observed as alive");
            std::thread::sleep(Duration::from_millis(10));
        }
        // Not yet reaped: the pid is still a zombie here, and still reads as exited.
        assert_eq!(start_time(child.id()), None);
        child.wait().unwrap();
    }

    #[test]
    fn a_reaped_child_cannot_be_opened_or_observed() {
        let mut child = spawn_sleep();
        let pid = child.id();
        child.kill().unwrap();
        child.wait().unwrap();
        // The pid could in principle be reused by now; either way it is not the child.
        if let Some(process) = Process::open(pid).unwrap() {
            if let Some(observation) = process.observe() {
                assert_ne!(observation.executable, Some(sleep_identity()));
            }
        }
    }

    /// The macOS fields are read at fixed offsets, so check the value is a
    /// plausible start time and not some other field: our own process started
    /// in the past, and not long ago.
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_start_time_is_microseconds_since_the_epoch() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_micros() as u64;
        let started = start_time(std::process::id()).unwrap();
        assert!(started <= now, "start time {started} is after now {now}");
        assert!(
            now - started < 3_600 * 1_000_000,
            "start time {started} is more than an hour before now {now}"
        );
    }

    /// Keeps one core busy for at least `wall` of wall-clock time.
    fn burn_cpu(wall: Duration) {
        let deadline = Instant::now() + wall;
        let mut value = 0u64;
        while Instant::now() < deadline {
            for step in 0..10_000u64 {
                value = std::hint::black_box(value.wrapping_mul(31).wrapping_add(step));
            }
        }
        std::hint::black_box(value);
    }

    #[test]
    fn own_resource_usage_is_present_and_plausible() {
        let usage = resource_usage(std::process::id()).expect("own process is readable");
        // A running test binary maps well over a megabyte; zero or a handful
        // of bytes would mean the wrong field or unit was read.
        assert!(
            usage.memory_bytes > 1024 * 1024,
            "memory {} bytes is implausibly small",
            usage.memory_bytes
        );
        assert!(
            usage.memory_bytes < 64 * 1024 * 1024 * 1024,
            "memory {} bytes is implausibly large",
            usage.memory_bytes
        );
        #[cfg(target_os = "macos")]
        assert_eq!(usage.memory_kind, MemoryKind::PhysFootprint);
        #[cfg(target_os = "linux")]
        {
            assert_eq!(usage.memory_kind, MemoryKind::ResidentSet);
            assert!(usage.swap_bytes.is_some(), "Linux reports VmSwap");
        }
    }

    /// CPU time must grow with busy work, and by roughly the amount of work
    /// done: a reading in the wrong unit (for example Mach ticks taken as
    /// nanoseconds on Apple silicon, about 24 times too small) grows too, but
    /// not by enough.
    #[test]
    fn own_cpu_time_grows_by_about_the_busy_work_done() {
        let pid = std::process::id();
        let total = |usage: ResourceUsage| usage.cpu_user + usage.cpu_system;
        let before = total(resource_usage(pid).unwrap());
        let busy = Duration::from_millis(400);
        burn_cpu(busy);
        let after = total(resource_usage(pid).unwrap());
        let grown = after.saturating_sub(before);
        // Other tests run in parallel threads of this process, so the growth
        // may exceed the busy time; it cannot fall far below it unless this
        // thread was starved, which a 50% floor tolerates.
        assert!(
            grown >= busy / 2,
            "cpu time grew by {grown:?} over {busy:?} of busy work"
        );
    }

    #[test]
    fn a_child_reads_its_own_usage_not_ours() {
        let mut child = spawn_sleep();
        let process = Process::open(child.id()).unwrap().unwrap();
        wait_for_executable(&process, sleep_identity());
        let ours = resource_usage(std::process::id()).unwrap();
        let usage = resource_usage(child.id()).expect("live child is readable");
        assert!(usage.memory_bytes > 0);
        assert!(
            usage.memory_bytes < ours.memory_bytes,
            "a sleeping child ({} bytes) should be smaller than the test binary ({} bytes)",
            usage.memory_bytes,
            ours.memory_bytes
        );
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn an_exited_child_reads_as_unavailable_not_zero() {
        let mut child = spawn_sleep();
        let pid = child.id();
        child.kill().unwrap();
        // Killed but not reaped: a zombie, which still has a pid.
        let deadline = Instant::now() + Duration::from_secs(5);
        while start_time(pid).is_some() {
            assert!(Instant::now() < deadline, "child still observed as alive");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(resource_usage(pid), None, "a zombie reads as unavailable");
        child.wait().unwrap();
        // Reaped: the pid names nothing (barring reuse, which would be some
        // other live process and so still not a reading of zeros).
        if let Some(usage) = resource_usage(pid) {
            assert!(usage.memory_bytes > 0, "a reused pid is some live process");
        }
    }

    #[test]
    fn a_pid_with_no_process_reads_as_unavailable() {
        // Above both kernels' pid limits (Linux caps pid_max at 2^22, macOS at
        // 99998), so nothing can hold it.
        assert_eq!(resource_usage(i32::MAX as u32), None);
        // Not a representable pid at all.
        assert_eq!(resource_usage(u32::MAX), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_signals_go_through_a_pidfd() {
        let mut child = spawn_sleep();
        let process = Process::open(child.id()).unwrap().unwrap();
        assert!(process.signals_through_pidfd());
        child.kill().unwrap();
        child.wait().unwrap();
        // The pidfd still names the reaped child, so a signal cannot reach anything else.
        assert!(!process.signal(Signal::Kill).unwrap());
    }
}
