//! Handle-pinned process observations and spawn-time executable identity on Windows.
#![allow(unsafe_code)]

use std::{
    fs::File,
    io,
    mem::size_of,
    os::windows::{
        fs::OpenOptionsExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::{Path, PathBuf},
    time::Duration,
};
use windows_sys::Win32::{
    Foundation::{ERROR_INVALID_PARAMETER, FILETIME, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT},
    Storage::FileSystem::{
        FileAttributeTagInfo, FileIdInfo, GetFileInformationByHandleEx,
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_ID_INFO, FILE_SHARE_READ, FILE_SHARE_WRITE,
    },
    System::{
        ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS},
        Threading::{
            GetProcessTimes, OpenProcess, TerminateProcess, WaitForSingleObject,
            PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
        },
    },
};

use crate::{MemoryKind, Observation, Process, ResourceUsage};

/// A Windows file object identity, not a content hash. All sixteen ID bytes matter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WindowsFileIdentity {
    pub volume_serial_number: u64,
    pub file_id: [u8; 16],
}

/// A resolved executable held against rename or deletion until the suspended spawn binds it.
#[derive(Debug)]
pub struct ExecutableCapture {
    file: File,
    path: PathBuf,
    identity: WindowsFileIdentity,
}

/// Identity of a suspended child, retaining the exact process handle used at spawn.
#[derive(Debug)]
pub struct SpawnedImage {
    process: Process,
    path: PathBuf,
    start_time: u64,
    identity: WindowsFileIdentity,
}

impl PartialEq for SpawnedImage {
    fn eq(&self, other: &Self) -> bool {
        self.process.pid() == other.process.pid()
            && self.start_time == other.start_time
            && self.identity == other.identity
            && self.path == other.path
    }
}
impl Eq for SpawnedImage {}

/// Why the running image cannot be confirmed. Unknown identity is never agreement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageUnavailable {
    ProcessExited,
    ProcessIdentityUnconfirmed,
    SpawnPathUnreadable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageAgreement {
    Match(WindowsFileIdentity),
    Mismatch {
        running: WindowsFileIdentity,
        disk: WindowsFileIdentity,
    },
    Unavailable(ImageUnavailable),
}

fn open_image(path: &Path) -> io::Result<File> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        // Omitting FILE_SHARE_DELETE pins the name across CreateProcessW.
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let mut attributes: FILE_ATTRIBUTE_TAG_INFO = Default::default();
    // SAFETY: file owns a live handle; the output buffer has the requested layout and size.
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileAttributeTagInfo,
            (&mut attributes as *mut FILE_ATTRIBUTE_TAG_INFO).cast(),
            size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if attributes.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "executable is a reparse point",
        ));
    }
    Ok(file)
}

fn identity(file: &File) -> io::Result<WindowsFileIdentity> {
    let mut info: FILE_ID_INFO = Default::default();
    // SAFETY: file owns a live handle; the output buffer has the FileIdInfo layout and size.
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileIdInfo,
            (&mut info as *mut FILE_ID_INFO).cast(),
            size_of::<FILE_ID_INFO>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(WindowsFileIdentity {
        volume_serial_number: info.VolumeSerialNumber,
        file_id: info.FileId.Identifier,
    })
}

impl ExecutableCapture {
    /// Open the resolved spawn path before CreateProcessW. The caller must use
    /// `path()` for the suspended spawn, then bind before resuming the child.
    pub fn open(path: &Path) -> io::Result<Self> {
        // Reject the supplied leaf before canonicalizing: canonicalize would hide
        // a symlink or other leaf reparse point by resolving its target.
        let supplied = open_image(path)?;
        let path = std::fs::canonicalize(path)?;
        let file = open_image(&path)?;
        let identity = identity(&file)?;
        if identity != self::identity(&supplied)? {
            return Err(io::Error::other(
                "executable changed while resolving its path",
            ));
        }
        Ok(Self {
            file,
            path,
            identity,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Bind while the child is suspended. `child` must be the handle returned by
    /// the spawn, not a process reopened by PID. The file lock is released only
    /// after the child's handle and creation time have been retained.
    pub fn bind(self, child: &std::process::Child) -> io::Result<SpawnedImage> {
        let handle = child.as_raw_handle();
        // SAFETY: AsRawHandle borrows a live child process handle for this call.
        let borrowed = unsafe { std::os::windows::io::BorrowedHandle::borrow_raw(handle) };
        self.bind_owned(child.id(), borrowed.try_clone_to_owned()?)
    }

    /// Bind a Tokio child while it is suspended, retaining its spawn handle.
    #[cfg(feature = "tokio")]
    pub fn bind_tokio(self, child: &tokio::process::Child) -> io::Result<SpawnedImage> {
        let pid = child
            .id()
            .ok_or_else(|| io::Error::other("child already reaped"))?;
        let handle = child
            .raw_handle()
            .ok_or_else(|| io::Error::other("child handle unavailable"))?;
        // SAFETY: raw_handle borrows the unreaped Tokio child's live process handle.
        let borrowed = unsafe { std::os::windows::io::BorrowedHandle::borrow_raw(handle) };
        self.bind_owned(pid, borrowed.try_clone_to_owned()?)
    }

    fn bind_owned(self, pid: u32, handle: OwnedHandle) -> io::Result<SpawnedImage> {
        let process = Process { pid, handle };
        let start_time = observe(&process)
            .ok_or_else(|| io::Error::other("spawned process identity unavailable"))?
            .start_time;
        // Keep the executable locked until the identity is bound to the process.
        drop(self.file);
        Ok(SpawnedImage {
            process,
            path: self.path,
            start_time,
            identity: self.identity,
        })
    }
}

impl SpawnedImage {
    pub fn process(&self) -> &Process {
        &self.process
    }
    pub fn start_time(&self) -> u64 {
        self.start_time
    }
    pub fn identity(&self) -> WindowsFileIdentity {
        self.identity
    }
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn agreement(&self, expected_start_time: u64) -> ImageAgreement {
        if expected_start_time != self.start_time {
            return ImageAgreement::Unavailable(ImageUnavailable::ProcessIdentityUnconfirmed);
        }
        if !matches!(observe(&self.process), Some(observed) if observed.start_time == self.start_time)
        {
            return self.unavailable_process();
        }
        let disk = match open_image(&self.path).and_then(|file| identity(&file)) {
            Ok(identity) => identity,
            Err(_) => return ImageAgreement::Unavailable(ImageUnavailable::SpawnPathUnreadable),
        };
        // Check again on the pinned process handle: IDs may be reused after exit,
        // so a disk observation made as the child exited must not be compared.
        if !matches!(observe(&self.process), Some(observed) if observed.start_time == self.start_time)
        {
            return self.unavailable_process();
        }
        if disk == self.identity {
            ImageAgreement::Match(self.identity)
        } else {
            ImageAgreement::Mismatch {
                running: self.identity,
                disk,
            }
        }
    }

    fn unavailable_process(&self) -> ImageAgreement {
        ImageAgreement::Unavailable(if matches!(wait(&self.process, Duration::ZERO), Ok(true)) {
            ImageUnavailable::ProcessExited
        } else {
            ImageUnavailable::ProcessIdentityUnconfirmed
        })
    }
}

pub(crate) fn open(pid: u32) -> io::Result<Option<OwnedHandle>> {
    // SAFETY: OpenProcess takes a numeric pid and returns an owned handle or null.
    let handle = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE | PROCESS_TERMINATE,
            0,
            pid,
        )
    };
    if handle.is_null() {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) {
            return Ok(None);
        }
        return Err(error);
    }
    // SAFETY: successful OpenProcess transfers ownership of this handle.
    Ok(Some(unsafe { OwnedHandle::from_raw_handle(handle) }))
}

fn times(handle: HANDLE) -> Option<(u64, u64, u64)> {
    let (mut creation, mut exit, mut kernel, mut user): (FILETIME, FILETIME, FILETIME, FILETIME) =
        Default::default();
    // SAFETY: the caller holds a live process handle; all output buffers are valid FILETIMEs.
    if unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) } == 0 {
        return None;
    }
    let ticks =
        |time: FILETIME| (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime);
    Some((ticks(creation), ticks(kernel), ticks(user)))
}

pub(crate) fn wait(process: &Process, timeout: Duration) -> io::Result<bool> {
    let millis = u32::try_from(timeout.as_millis())
        .unwrap_or(u32::MAX - 1)
        .min(u32::MAX - 1);
    // SAFETY: the owned process handle stays live throughout the wait.
    let result = match unsafe { WaitForSingleObject(process.handle.as_raw_handle(), millis) } {
        WAIT_OBJECT_0 => Ok(true),
        WAIT_TIMEOUT => Ok(false),
        _ => Err(io::Error::last_os_error()),
    };
    #[cfg(test)]
    if !timeout.is_zero() && matches!(result, Ok(true)) {
        // TerminateProcess can finish before a test's next observation. Record
        // the actual bounded wait too, so removing it cannot pass by scheduling luck.
        COMPLETED_BOUNDED_WAITS.with(|count| count.set(count.get() + 1));
    }
    result
}

#[cfg(test)]
thread_local! {
    static COMPLETED_BOUNDED_WAITS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn completed_bounded_waits() -> u64 {
    COMPLETED_BOUNDED_WAITS.with(|count| count.get())
}

pub(crate) fn observe(process: &Process) -> Option<Observation> {
    if wait(process, Duration::ZERO).ok()? {
        return None;
    }
    let start_time = times(process.handle.as_raw_handle())?.0;
    if wait(process, Duration::ZERO).ok()? {
        return None;
    }
    Some(Observation {
        start_time,
        executable: None,
    })
}

pub(crate) fn force_stop(process: &Process, expected: u64, timeout: Duration) -> io::Result<bool> {
    let Some(observed) = observe(process) else {
        return if wait(process, Duration::ZERO)? {
            Ok(true)
        } else {
            Err(io::Error::other("process identity unavailable"))
        };
    };
    if observed.start_time != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "process creation time does not match",
        ));
    }
    // SAFETY: the checked process is the same owned handle, never reopened by PID.
    if unsafe { TerminateProcess(process.handle.as_raw_handle(), 1) } == 0 {
        let error = io::Error::last_os_error();
        if !wait(process, Duration::ZERO)? {
            return Err(error);
        }
    }
    wait(process, timeout)
}

pub(crate) fn resource_usage(process: &Process) -> Option<ResourceUsage> {
    observe(process)?;
    let mut counters: PROCESS_MEMORY_COUNTERS = Default::default();
    // SAFETY: the process handle is live and the output buffer has the requested layout and size.
    if unsafe {
        GetProcessMemoryInfo(
            process.handle.as_raw_handle(),
            &mut counters,
            size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        )
    } == 0
    {
        return None;
    }
    let (_, kernel, user) = times(process.handle.as_raw_handle())?;
    observe(process)?;
    let duration = |ticks: u64| {
        Duration::from_secs(ticks / 10_000_000) + Duration::from_nanos((ticks % 10_000_000) * 100)
    };
    Some(ResourceUsage {
        memory_bytes: counters.WorkingSetSize as u64,
        memory_kind: MemoryKind::WindowsWorkingSet,
        swap_bytes: None,
        cpu_user: duration(user),
        cpu_system: duration(kernel),
    })
}
