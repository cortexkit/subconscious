//! Every Win32 call in this crate, plus the handle-owning type, and the safety
//! argument for each.
//!
//! `unsafe_code` is denied at the crate root and allowed here by exception, which
//! keeps the FFI boundary auditable in one file rather than spread across the
//! public API. [`JobObject`] lives here rather than in `lib.rs` because it owns a
//! raw handle and therefore needs a manual `Send`: an `unsafe impl` belongs on
//! the side of the boundary that is allowed to write one.

#![allow(unsafe_code)]

use std::{io, mem::size_of, os::windows::process::CommandExt, process::Command};

use windows_sys::Win32::{
    Foundation::{CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE, WAIT_TIMEOUT},
    System::{
        Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
        },
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicProcessIdList,
            JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
            TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        },
        Threading::{
            OpenProcess, OpenThread, ResumeThread, WaitForSingleObject, CREATE_SUSPENDED,
            PROCESS_SYNCHRONIZE, THREAD_SUSPEND_RESUME,
        },
    },
};

/// Preceded by `CREATE_SUSPENDED` so a child cannot run before it is contained.
pub const CREATE_SUSPENDED_FLAG: u32 = CREATE_SUSPENDED;

/// `ERROR_NO_MORE_FILES`. The toolhelp thread walk reports ordinary exhaustion
/// through this code, so it is the one value that separates "finished
/// enumerating" from a real failure.
const ERROR_NO_MORE_FILES: u32 = 18;

/// `ERROR_MORE_DATA`. A job holding more processes than the query's buffer can
/// enumerate reports this while still filling in the assigned count.
const ERROR_MORE_DATA: u32 = 234;

/// How many pids one [`job_process_count`] query can enumerate.
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
        // Close-on-drop is the crash-durability guarantee, not merely cleanup: if
        // the daemon is killed, this handle closes with it and the kernel reaps
        // the tree that no supervisor code is left alive to kill.
        unsafe { CloseHandle(self.handle) };
    }
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

/// Mark `command` to create its child suspended.
pub fn set_suspended_creation_flags(command: &mut Command) {
    command.creation_flags(CREATE_SUSPENDED_FLAG);
}

/// The same, for the async command the supervisor's spawn path uses.
pub fn set_suspended_creation_flags_async(command: &mut tokio::process::Command) {
    command.creation_flags(CREATE_SUSPENDED_FLAG);
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
