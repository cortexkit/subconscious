//! Windows job-object containment for supervised subc module children.
//!
//! A supervised module may spawn helpers of its own — the Synapse embedding
//! module spawns a CUDA worker that holds the GPU allocation — and until this
//! crate existed nothing in teardown took them. `Child::kill` is `TerminateProcess`
//! scoped to one pid, and Windows has no process group to signal, so a module
//! that was terminated rather than asked could not close its own pipes and its
//! grandchildren outlived it. A day of restarts accumulated orphans.
//!
//! A job object contains by **membership** rather than ancestry, which covers the
//! two cases a `taskkill /T` walk cannot:
//!
//!   * a grandchild spawned between the walk and the kill;
//!   * a grandchild whose parent has already exited and been reparented, so it is
//!     no longer in the tree to be found. Those are precisely the orphans that
//!     accumulate across restarts.
//!
//! # The suspended-create contract
//!
//! [`suspend_on_create`] and [`resume_main_thread`] exist because assignment must
//! happen before the child can run a single instruction. `CreateProcess` gives the
//! caller no way to place a process in a job atomically short of building the
//! process by hand with `PROC_THREAD_ATTRIBUTE_JOB_LIST`, so the child is created
//! suspended, assigned, and then resumed. A window between spawn and assignment
//! is a grandchild that escapes — the defect this crate fixes, in smaller form.
//!
//! The order is load-bearing and there are exactly three steps:
//!
//! ```no_run
//! # #[cfg(windows)]
//! # fn main() -> std::io::Result<()> {
//! let job = subc_jobobject::JobObject::new()?;
//! let mut command = std::process::Command::new("module.exe");
//! subc_jobobject::suspend_on_create(&mut command);
//! let child = command.spawn()?;
//! job.assign(&child)?;
//! subc_jobobject::resume_main_thread(child.id())?;
//! # Ok(())
//! # }
//! # #[cfg(not(windows))] fn main() {}
//! ```
//!
//! [`spawn_contained`] performs all three for the synchronous case. A caller that
//! spawns asynchronously sets the flag, spawns, calls [`JobObject::assign`], and
//! resumes — which is what the daemon's `contain_spawned_child` does.
//!
//! If the caller never resumes, the child stays suspended forever, so
//! [`resume_main_thread`] reports the failure rather than logging it: the caller
//! must kill the child and fail the spawn.

#![cfg(windows)]
#![deny(unsafe_code)]

mod sys;

use std::{
    io,
    os::windows::io::AsRawHandle,
    process::Child,
    thread,
    time::{Duration, Instant},
};

pub use sys::{JobObject, CONTAINMENT_CREATION_FLAGS, CREATE_SUSPENDED_FLAG};

/// A process handle suitable for [`JobObject::assign`].
///
/// `std` and `tokio` children expose their handle differently — `AsRawHandle`
/// for the former, an inherent `raw_handle()` that returns `Option` for the
/// latter — so this normalizes both rather than making the caller reach for the
/// right accessor and get the `None` case wrong.
pub trait ProcessHandle {
    /// The process handle, or `None` if the process has already been reaped.
    fn handle(&self) -> Option<*mut std::ffi::c_void>;
}

/// A raw handle wrapper, so `JobObject::assign` can take an already-resolved
/// handle without dereferencing one that a safe caller cannot validate.
#[derive(Clone, Copy)]
pub struct RawProcessHandle(pub *mut std::ffi::c_void);

impl ProcessHandle for RawProcessHandle {
    fn handle(&self) -> Option<*mut std::ffi::c_void> {
        Some(self.0)
    }
}

impl ProcessHandle for Child {
    fn handle(&self) -> Option<*mut std::ffi::c_void> {
        Some(self.as_raw_handle().cast())
    }
}

impl ProcessHandle for tokio::process::Child {
    fn handle(&self) -> Option<*mut std::ffi::c_void> {
        self.raw_handle().map(|handle| handle.cast())
    }
}

/// [`ProcessHandle::handle`] by reference, for the call site's readability.
pub fn process_handle<C: ProcessHandle>(child: &C) -> Option<*mut std::ffi::c_void> {
    child.handle()
}

/// How long [`resume_main_thread`] keeps looking for the new process's thread.
///
/// A snapshot taken immediately after `CreateProcess` can miss the thread even
/// though the process exists, so this is a retry budget rather than an expected
/// wait: the ordinary case finds the thread on the first attempt.
const RESUME_DISCOVERY_TIMEOUT: Duration = Duration::from_millis(500);

/// Mark `command` so the child is created suspended and without a console window.
///
/// The child must not run a single instruction before [`JobObject::assign`], or
/// it could spawn a grandchild that escapes containment. Call
/// [`resume_main_thread`] once the child is assigned. This sets the command's
/// whole creation-flag mask ([`CONTAINMENT_CREATION_FLAGS`]); setting creation
/// flags on the command again afterwards would replace it.
pub fn suspend_on_create(command: &mut std::process::Command) {
    sys::set_suspended_creation_flags(command);
}

/// [`suspend_on_create`] for the async command the supervisor spawns.
pub fn suspend_on_create_async(command: &mut tokio::process::Command) {
    sys::set_suspended_creation_flags_async(command);
}

/// Start the child created by [`suspend_on_create`], now that it is contained.
///
/// `std` keeps no handle to the primary thread that `CreateProcess` returns, so
/// it is found through a toolhelp snapshot. A failure here means the child cannot
/// be started and the caller must kill it rather than leave a suspended process
/// holding a pid.
pub fn resume_main_thread(pid: u32) -> io::Result<()> {
    let started = Instant::now();
    let mut backoff = Duration::from_millis(1);
    loop {
        match sys::open_first_thread(pid)? {
            Some(thread) => return sys::resume_and_close(thread),
            None if started.elapsed() < RESUME_DISCOVERY_TIMEOUT => {
                thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_millis(20));
            }
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("no thread appeared for pid {pid} within {RESUME_DISCOVERY_TIMEOUT:?}"),
                ));
            }
        }
    }
}

/// Wait for `pid` to leave the process table.
///
/// Teardown is asynchronous: `TerminateJobObject` returns once the members are
/// signalled, not once they have been reaped, so a caller that asserted
/// immediately would be racing the kernel. `true` means gone, `false` means still
/// present at the deadline.
pub fn wait_for_process_exit(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if !process_exists(pid) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// Whether a pid currently names a running process.
pub fn process_exists(pid: u32) -> bool {
    sys::process_exists(pid)
}

/// Whether this process has a console window. A contained child should not:
/// see [`CONTAINMENT_CREATION_FLAGS`].
pub fn has_console_window() -> bool {
    sys::has_console_window()
}

/// A child and the job that contains it, held together.
///
/// The daemon spawns, assigns, and resumes in sequence; keeping the job beside
/// the child is what makes the association survive a refactor that reorders those
/// steps.
#[derive(Debug)]
pub struct ContainedChild<C> {
    /// The supervised process.
    pub child: C,
    /// The job that contains it and everything it spawns.
    pub job: JobObject,
}

impl<C: ProcessHandle> ContainedChild<C> {
    /// Contain `child` and start it.
    ///
    /// The one call that gets the order right: assign while suspended, then
    /// resume. A failure to resume kills the child, because a suspended process
    /// holding a pid with no way to start is a leak rather than a failed spawn.
    pub fn contain(child: C, job: JobObject, pid: u32) -> io::Result<Self> {
        let handle = child
            .handle()
            .ok_or_else(|| io::Error::other("child was reaped before it could be contained"))?;
        job.assign(&RawProcessHandle(handle))?;
        if let Err(error) = resume_main_thread(pid) {
            let _ = job.terminate();
            return Err(error);
        }
        Ok(Self { child, job })
    }
}

/// Spawn a command already contained, with no window for an escape.
///
/// Applies [`suspend_on_create`], spawns, assigns, and resumes in one place so
/// the ordering cannot be got wrong at a call site.
pub fn spawn_contained(command: &mut std::process::Command) -> io::Result<ContainedChild<Child>> {
    suspend_on_create(command);
    let child = command.spawn()?;
    let pid = child.id();
    let job = JobObject::new()?;
    ContainedChild::contain(child, job, pid)
}
