//! Every Win32 call in this crate, plus the handle-owning type, and the safety
//! argument for each.
//!
//! `unsafe_code` is denied at the crate root and allowed here by exception, which
//! keeps the FFI boundary auditable in one file rather than spread across the
//! public API. [`JobObject`] lives here rather than in `lib.rs` because it owns a
//! raw handle and therefore needs a manual `Send`: an `unsafe impl` belongs on
//! the side of the boundary that is allowed to write one.

#![allow(unsafe_code)]

use std::{
    io,
    mem::size_of,
    os::windows::{
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
        process::CommandExt,
    },
    process::Command,
    time::Instant,
};

use windows_sys::Win32::{
    Foundation::{
        CloseHandle, DuplicateHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE, WAIT_TIMEOUT,
    },
    System::{
        Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
        },
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob,
            JobObjectBasicProcessIdList, JobObjectExtendedLimitInformation,
            QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
            JOBOBJECT_BASIC_PROCESS_ID_LIST, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        },
        ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS},
        Threading::{
            GetCurrentProcess, GetExitCodeProcess, OpenProcess, OpenThread, ResumeThread,
            WaitForSingleObject, CREATE_NO_WINDOW, CREATE_SUSPENDED,
            PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, THREAD_SUSPEND_RESUME,
        },
    },
};

/// Preceded by `CREATE_SUSPENDED` so a child cannot run before it is contained.
pub const CREATE_SUSPENDED_FLAG: u32 = CREATE_SUSPENDED;

/// Every creation flag a contained child is spawned with, as one mask.
///
/// `CommandExt::creation_flags` replaces the whole mask rather than adding to
/// it, so the flags must be set together in one call: a second call for either
/// flag would silently drop the other.
///
/// - `CREATE_SUSPENDED`: the child must not run before it is in its job, or it
///   could start a grandchild that escapes containment.
/// - `CREATE_NO_WINDOW`: a supervised module is a background process. Without
///   this flag a console-subsystem child gets its own console window, and
///   closing that window ends the module with `STATUS_CONTROL_C_EXIT`
///   (0xC000013A), which the supervisor then restarts.
pub const CONTAINMENT_CREATION_FLAGS: u32 = CREATE_SUSPENDED | CREATE_NO_WINDOW;

/// `ERROR_NO_MORE_FILES`. The toolhelp thread walk reports ordinary exhaustion
/// through this code, so it is the one value that separates "finished
/// enumerating" from a real failure.
const ERROR_NO_MORE_FILES: u32 = 18;

/// `ERROR_MORE_DATA`. A job holding more processes than the query's buffer can
/// enumerate reports this while still filling in the assigned count.
const ERROR_MORE_DATA: u32 = 234;

// JOB_OBJECT_QUERY from winnt.h; windows-sys does not expose this access mask.
const JOB_OBJECT_QUERY: u32 = 0x0004;

/// How many pids one [`JobObject::process_count`] query can enumerate.
///
/// The count of *assigned* processes is reported by the kernel regardless of what
/// the buffer can hold; only the enumerated subset is bounded. Far past any
/// legitimate module tree, and small enough to keep the query a stack
/// allocation.
const PROCESS_ID_LIST_WIDTH: usize = 256;

/// A job object that owns one supervised module's whole process tree.
///
/// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` is set at creation, so dropping the last
/// handle kills every member. That is what makes containment survive a daemon
/// crash, where no code runs to call [`JobObject::terminate`] — the handle closes
/// with the process and the kernel reaps the tree.
///
/// Breakaway is deliberately NOT permitted: leaving `JOB_OBJECT_LIMIT_BREAKAWAY_OK`
/// unset is what stops a child from calling `CreateProcess` with
/// `CREATE_BREAKAWAY_FROM_JOB` and escaping. A module that could leave the job
/// could leave its grandchildren behind, which is the whole defect.
#[derive(Debug)]
pub struct JobObject {
    handle: HANDLE,
}

// SAFETY: a `HANDLE` is a process-wide kernel handle value, not a pointer into
// this process's address space. Every operation this crate performs on it
// (`AssignProcessToJobObject`, `TerminateJobObject`, `QueryInformationJobObject`,
// `CloseHandle`) is thread-agnostic, and the daemon holds the job across an await
// in a spawned task, which is why `Send` is required at all. `Sync` is NOT
// implemented: concurrent `TerminateJobObject` and `Drop` on the same handle
// would be a double-close, and nothing needs it.
unsafe impl Send for JobObject {}

impl JobObject {
    /// Duplicate only query access, for report-only reads. The handle stays
    /// open until the returned `JobQuery` is dropped.
    pub fn query(&self) -> io::Result<JobQuery> {
        let mut handle = std::ptr::null_mut();
        // SAFETY: the source handle is owned and live, both process arguments
        // are this process, and `handle` is a valid output slot. No mutation
        // rights or inheritance are granted to the duplicate.
        if unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                self.handle,
                GetCurrentProcess(),
                &mut handle,
                JOB_OBJECT_QUERY,
                0,
                0,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: DuplicateHandle returned a new owned handle.
        Ok(JobQuery {
            handle: unsafe { OwnedHandle::from_raw_handle(handle) },
        })
    }
    /// Contain this process and all subsequently created children, including
    /// suspended children not yet assigned to a narrower per-module job.
    pub(crate) fn assign_current_process(&self) -> io::Result<()> {
        // GetCurrentProcess returns a pseudo-handle, which must not be closed.
        if unsafe { AssignProcessToJobObject(self.handle, GetCurrentProcess()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Create a job object whose members die with it.
    pub fn new() -> io::Result<Self> {
        // A null `SECURITY_ATTRIBUTES` gives the default descriptor and a null
        // name means unnamed, so no other process can open it to interfere.
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }

        // Breakaway is left unset deliberately, per the type documentation.
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let applied = unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                std::ptr::from_ref(&limits).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if applied == 0 {
            let error = io::Error::last_os_error();
            // The handle is already owned by us, so a failure here must close it
            // or the job outlives the error that rejected it.
            unsafe { CloseHandle(handle) };
            return Err(error);
        }

        Ok(Self { handle })
    }

    /// Put a process in the job, and with it every process it goes on to create.
    ///
    /// Generic over [`crate::ProcessHandle`] rather than taking a raw `HANDLE`:
    /// a raw handle cannot be validated by a safe caller, and the trait keeps the
    /// `None` (already reaped) case expressed in the type. Must be called while
    /// the process is still suspended: see the crate root.
    pub fn assign<C: crate::ProcessHandle>(&self, child: &C) -> io::Result<()> {
        let Some(process) = child.handle() else {
            return Err(io::Error::other(
                "child was reaped before it could be assigned to the job",
            ));
        };
        let assigned = unsafe { AssignProcessToJobObject(self.handle, process) };
        if assigned == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Kill every member of the job.
    ///
    /// The explicit counterpart to the close-on-drop guarantee, for a teardown
    /// that wants the tree gone now rather than when the handle is released.
    /// Membership is what lets this reach grandchildren whose parent has already
    /// exited and been reparented away from the tree.
    pub fn terminate(&self) -> io::Result<()> {
        let terminated = unsafe { TerminateJobObject(self.handle, 1) };
        if terminated == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// How many processes the job currently contains.
    ///
    /// Exists so a test can witness the grandchild rather than infer it: a
    /// teardown that reaped the direct child and nothing else reports `1` here,
    /// which is a different observation from the child being gone.
    pub fn process_count(&self) -> io::Result<u32> {
        let mut list = PidList::default();
        let queried = unsafe {
            QueryInformationJobObject(
                self.handle,
                JobObjectBasicProcessIdList,
                std::ptr::from_mut(&mut list).cast(),
                size_of::<PidList>() as u32,
                std::ptr::null_mut(),
            )
        };
        if queried == 0 {
            // A job with more members than the buffer enumerates returns this
            // while still filling in the true assigned count, so the count is
            // readable rather than unavailable.
            let code = unsafe { GetLastError() };
            if code != ERROR_MORE_DATA {
                return Err(io::Error::from_raw_os_error(code as i32));
            }
        }
        Ok(list.number_of_assigned_processes)
    }
}

impl Drop for JobObject {
    fn drop(&mut self) {
        // These owners are created with KILL_ON_JOB_CLOSE. A query duplicate
        // must not defer the owner's teardown until the last reader closes.
        // Zero preserves the kernel's kill-on-close process exit status.
        unsafe { TerminateJobObject(self.handle, 0) };
        // Close-on-drop is the crash-durability guarantee, not merely cleanup: if
        // the daemon is killed, this handle closes with it and the kernel reaps
        // the tree that no supervisor code is left alive to kill.
        unsafe { CloseHandle(self.handle) };
    }
}

/// A query-only duplicate of a job handle, closed when this value is dropped.
#[derive(Debug)]
pub struct JobQuery {
    handle: OwnedHandle,
}

/// Working sets of confirmed live job members; Windows swap is not reported.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct JobMemory {
    pub memory_bytes: u64,
    pub process_count: u64,
}

/// A failed job reading retains the number of live processes already counted.
#[derive(Debug)]
#[non_exhaustive]
pub struct JobMemoryError {
    pub error: io::Error,
    pub process_count: Option<u64>,
}

impl std::fmt::Display for JobMemoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}

impl std::error::Error for JobMemoryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

impl JobQuery {
    fn check(deadline: Instant) -> io::Result<()> {
        if Instant::now() >= deadline {
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "job memory deadline",
            ))
        } else {
            Ok(())
        }
    }

    fn members(&self, deadline: Instant) -> io::Result<Vec<usize>> {
        let header = 8 / size_of::<usize>();
        let mut capacity = PROCESS_ID_LIST_WIDTH;
        for _ in 0..3 {
            Self::check(deadline)?;
            if capacity > 1_000_000 {
                break;
            }
            let mut buffer = vec![0_usize; header + capacity];
            // SAFETY: the usize allocation has the alignment of the variable
            // PID array and space for both DWORD counts and every PID.
            let queried = unsafe {
                QueryInformationJobObject(
                    self.handle.as_raw_handle(),
                    JobObjectBasicProcessIdList,
                    buffer.as_mut_ptr().cast(),
                    (buffer.len() * size_of::<usize>()) as u32,
                    std::ptr::null_mut(),
                )
            };
            let list = buffer.as_ptr().cast::<JOBOBJECT_BASIC_PROCESS_ID_LIST>();
            // SAFETY: both fixed header fields fit even in an empty PID list.
            let (assigned, listed) = unsafe {
                (
                    (*list).NumberOfAssignedProcesses as usize,
                    (*list).NumberOfProcessIdsInList as usize,
                )
            };
            if queried != 0 && assigned == listed && listed <= capacity {
                return Ok(buffer[header..header + listed].to_vec());
            }
            if queried == 0 && unsafe { GetLastError() } != ERROR_MORE_DATA {
                return Err(io::Error::last_os_error());
            }
            capacity = assigned.max(capacity.saturating_mul(2));
        }
        Err(io::Error::other(
            "job membership changed during enumeration",
        ))
    }

    /// Sum live members with PID-reuse protection, never a truncated PID list.
    pub fn memory(&self, deadline: Instant) -> Result<JobMemory, JobMemoryError> {
        self.memory_counted(deadline, |_| {})
    }

    /// Publish confirmed process counts so a bounded caller can retain them
    /// even if a later process query stalls.
    pub fn memory_counted(
        &self,
        deadline: Instant,
        mut counted: impl FnMut(u64),
    ) -> Result<JobMemory, JobMemoryError> {
        let mut process_count = None;
        let result = (|| -> io::Result<JobMemory> {
            let mut reading = JobMemory {
                memory_bytes: 0,
                process_count: 0,
            };
            let members = self.members(deadline)?;
            process_count = Some(0);
            counted(0);
            for pid in members {
                Self::check(deadline)?;
                // SAFETY: no inheritance; the PID is only a candidate until its
                // membership is confirmed through the returned handle.
                let process =
                    unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid as u32) };
                if process.is_null() {
                    let error = io::Error::last_os_error();
                    if matches!(error.raw_os_error(), Some(87 | 1168)) {
                        continue;
                    }
                    return Err(error);
                }
                // SAFETY: OpenProcess returned a new owned handle.
                let process = unsafe { OwnedHandle::from_raw_handle(process) };
                let mut member = 0;
                // SAFETY: both handles remain owned throughout this read and the
                // membership output slot is valid.
                if unsafe {
                    IsProcessInJob(
                        process.as_raw_handle(),
                        self.handle.as_raw_handle(),
                        &mut member,
                    )
                } == 0
                {
                    return Err(io::Error::last_os_error());
                }
                if member == 0 || exited(&process)? {
                    continue;
                }
                let mut counters = PROCESS_MEMORY_COUNTERS::default();
                // SAFETY: the same confirmed process handle and a correctly sized
                // output buffer are retained; no second PID lookup is performed.
                if unsafe {
                    GetProcessMemoryInfo(
                        process.as_raw_handle(),
                        &mut counters,
                        size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
                    )
                } == 0
                {
                    let error = io::Error::last_os_error();
                    if exited(&process)? {
                        continue;
                    }
                    return Err(error);
                }
                if exited(&process)? {
                    continue;
                }
                reading.memory_bytes = reading
                    .memory_bytes
                    .checked_add(counters.WorkingSetSize as u64)
                    .ok_or_else(|| io::Error::other("working set sum overflow"))?;
                reading.process_count += 1;
                process_count = Some(reading.process_count);
                counted(reading.process_count);
            }
            Self::check(deadline)?;
            Ok(reading)
        })();
        result.map_err(|error| JobMemoryError {
            error,
            process_count,
        })
    }
}

fn exited(process: &OwnedHandle) -> io::Result<bool> {
    let mut code = 0;
    // SAFETY: the owned process handle has query access and the output is valid.
    if unsafe { GetExitCodeProcess(process.as_raw_handle(), &mut code) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(code != 259) // STILL_ACTIVE
}

/// The kernel's variable-length pid list, sized for a single query.
///
/// Mirrors `JOBOBJECT_BASIC_PROCESS_ID_LIST`, whose trailing array the SDK
/// declares as one element. Field order and `#[repr(C)]` are the SDK's, because
/// the kernel writes this layout directly.
#[repr(C)]
struct PidList {
    number_of_assigned_processes: u32,
    number_of_process_ids_in_list: u32,
    process_ids: [usize; PROCESS_ID_LIST_WIDTH],
}

impl Default for PidList {
    fn default() -> Self {
        Self {
            number_of_assigned_processes: 0,
            number_of_process_ids_in_list: 0,
            process_ids: [0; PROCESS_ID_LIST_WIDTH],
        }
    }
}

/// Mark `command` to create its child suspended and without a console window.
pub fn set_suspended_creation_flags(command: &mut Command) {
    command.creation_flags(CONTAINMENT_CREATION_FLAGS);
}

/// The same, for the async command the supervisor's spawn path uses.
pub fn set_suspended_creation_flags_async(command: &mut tokio::process::Command) {
    command.creation_flags(CONTAINMENT_CREATION_FLAGS);
}

/// Whether this process has a console window.
///
/// Safety: `GetConsoleWindow` takes no arguments, only reads this process's
/// console attachment, and returns null when there is none.
pub fn has_console_window() -> bool {
    !unsafe { windows_sys::Win32::System::Console::GetConsoleWindow() }.is_null()
}

/// Find the primary thread of `pid` and open it with resume rights.
///
/// A suspended process has exactly one thread, so the first match is the one to
/// resume.
///
/// Safety: the snapshot handle is checked against `INVALID_HANDLE_VALUE` before
/// use and closed on every exit path. `entry` is a correctly-sized stack value
/// whose `dwSize` is set as the API requires.
pub fn open_first_thread(pid: u32) -> io::Result<Option<HANDLE>> {
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }

    let mut entry = THREADENTRY32 {
        dwSize: size_of::<THREADENTRY32>() as u32,
        ..Default::default()
    };
    let mut found = None;
    let mut ok = unsafe { Thread32First(snapshot, &mut entry) };
    while ok != 0 {
        if entry.th32OwnerProcessID == pid {
            let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
            if thread.is_null() {
                let error = io::Error::last_os_error();
                unsafe { CloseHandle(snapshot) };
                return Err(error);
            }
            found = Some(thread);
            break;
        }
        ok = unsafe { Thread32Next(snapshot, &mut entry) };
        if ok == 0 {
            if let Err(error) = exhaustion_or_error() {
                unsafe { CloseHandle(snapshot) };
                return Err(error);
            }
        }
    }

    // A first call that reported exhaustion still needs classifying: treating a
    // broken snapshot as "this process has no threads" would make the caller kill
    // a child it could have started.
    if found.is_none() {
        if let Err(error) = exhaustion_or_error() {
            unsafe { CloseHandle(snapshot) };
            return Err(error);
        }
    }

    unsafe { CloseHandle(snapshot) };
    Ok(found)
}

/// Classify a `0` from the toolhelp thread walk: `Ok(())` for ordinary
/// exhaustion, `Err` for a real failure.
fn exhaustion_or_error() -> Result<(), io::Error> {
    let code = unsafe { GetLastError() };
    if code == ERROR_NO_MORE_FILES {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(code as i32))
    }
}

/// Resume a suspended thread and close the handle opened for it.
///
/// Safety: `thread` was opened by `open_first_thread` with resume rights, is
/// resumed at most once, and is closed exactly once here.
pub fn resume_and_close(thread: HANDLE) -> io::Result<()> {
    let previous = unsafe { ResumeThread(thread) };
    let result = if previous == u32::MAX {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    };
    unsafe { CloseHandle(thread) };
    result
}

/// Whether `pid` names a process that is still running.
///
/// `OpenProcess` succeeding is NOT the answer: a terminated process stays
/// openable while any handle to it survives, and a killed module's handles
/// outlive the kill. That version reported a dead parent as alive and made a
/// passing teardown look like a leak — so liveness is read from the process's
/// signal state, which is what `WaitForSingleObject` with a zero timeout
/// answers.
///
/// Safety: the handle is opened with synchronize rights (the minimum
/// `WaitForSingleObject` accepts), checked for null (the ordinary "no such
/// process" answer, reported as absence rather than an error), used for exactly
/// one zero-timeout wait, and closed.
pub fn process_exists(pid: u32) -> bool {
    let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if handle.is_null() {
        return false;
    }
    // Signalled means terminated; a timeout means it is still running.
    let alive = unsafe { WaitForSingleObject(handle, 0) } == WAIT_TIMEOUT;
    unsafe { CloseHandle(handle) };
    alive
}

#[cfg(test)]
mod memory_tests {
    use super::*;

    #[test]
    fn explicit_owner_drop_preserves_kernel_close_exit_status() {
        fn spawn() -> crate::ContainedChild<std::process::Child> {
            let mut command = Command::new("powershell.exe");
            command
                .args([
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    "Start-Sleep -Seconds 600",
                ])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            crate::spawn_contained(&mut command).unwrap()
        }
        let control = spawn();
        let pid = control.child.id();
        let job = std::mem::ManuallyDrop::new(control.job);
        // SAFETY: close the uniquely owned control handle exactly once to
        // measure the kernel path, without the owner's explicit termination.
        assert_ne!(unsafe { CloseHandle(job.handle) }, 0);
        assert!(crate::wait_for_process_exit(
            pid,
            std::time::Duration::from_secs(10)
        ));
        let mut child = control.child;
        let kernel_code = child.wait().unwrap().code();
        eprintln!("WINDOWS_KERNEL_JOB_CLOSE_EXIT_CODE={kernel_code:?}");
        assert_eq!(kernel_code, Some(0));

        let owner = spawn();
        let pid = owner.child.id();
        let query = owner.job.query().unwrap();
        drop(owner.job);
        assert!(crate::wait_for_process_exit(
            pid,
            std::time::Duration::from_secs(10)
        ));
        assert_eq!(
            owner.child.wait_with_output().unwrap().status.code(),
            kernel_code
        );
        drop(query);
    }
}
