//! One-time Windows handoff. No pipe handle is inherited by the child.

use super::{Cached, LaunchNonce, LaunchNonceError, LaunchNonceSource};
use std::{
    ffi::OsStr,
    io,
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    ptr::{null, null_mut},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, OnceLock,
    },
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{
        LocalFree, ERROR_ACCESS_DENIED, ERROR_BROKEN_PIPE, ERROR_IO_PENDING, ERROR_PIPE_BUSY,
        ERROR_PIPE_CONNECTED, ERROR_SEM_TIMEOUT, GENERIC_READ, HANDLE, INVALID_HANDLE_VALUE,
        WAIT_OBJECT_0, WAIT_TIMEOUT,
    },
    Security::Cryptography::{BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG},
    Security::{
        Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        },
        GetTokenInformation, TokenUser, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
    },
    Storage::FileSystem::{
        CreateFileW, ReadFile, WriteFile, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED,
        OPEN_EXISTING, PIPE_ACCESS_OUTBOUND, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
    },
    System::{
        Pipes::{
            ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeClientProcessId,
            PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
        },
        Threading::{
            CreateEventW, GetCurrentProcess, OpenProcess, OpenProcessToken, WaitForSingleObject,
            PROCESS_SYNCHRONIZE,
        },
        IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED},
    },
};

const OPEN_WINDOW: Duration = Duration::from_millis(500);
const READ_WINDOW: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(5);
const MAX_NONCE_BYTES: usize = 4096;
const CLIENT_ACCESS: u32 = 0x80100100;

fn wide(text: &OsStr) -> Vec<u16> {
    text.encode_wide().chain(Some(0)).collect()
}

fn owned(handle: HANDLE) -> io::Result<OwnedHandle> {
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: each caller supplies a newly created, uniquely owned handle.
        Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
    }
}

/// A server created before spawn. Dropping it closes the reserved name.
pub struct LaunchNoncePipeHandoff {
    pipe: OwnedHandle,
    name: String,
    nonce: Vec<u8>,
}

impl LaunchNoncePipeHandoff {
    pub fn new(nonce: &str) -> io::Result<Self> {
        Self::with_names(nonce, random_name)
    }

    fn with_names(nonce: &str, mut next: impl FnMut() -> io::Result<String>) -> io::Result<Self> {
        if nonce.is_empty() || nonce.len() > MAX_NONCE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid launch nonce length",
            ));
        }
        let security = user_security_descriptor()?;
        let attrs = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: security.0,
            bInheritHandle: 0,
        };
        // A pre-existing pipe makes first-instance creation fail; retry with a
        // fresh random name rather than connecting to another process's server.
        for _ in 0..16 {
            let name = next()?;
            let path = wide(OsStr::new(&name));
            // SAFETY: path and the protected security descriptor remain live for the call.
            let result = owned(unsafe {
                CreateNamedPipeW(
                    path.as_ptr(),
                    PIPE_ACCESS_OUTBOUND | FILE_FLAG_FIRST_PIPE_INSTANCE | FILE_FLAG_OVERLAPPED,
                    PIPE_TYPE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                    1,
                    MAX_NONCE_BYTES as u32,
                    0,
                    0,
                    &attrs,
                )
            });
            match result {
                Ok(pipe) => {
                    return Ok(Self {
                        pipe,
                        name,
                        nonce: nonce.as_bytes().to_vec(),
                    })
                }
                Err(error) if matches!(error.raw_os_error(), Some(code) if code == ERROR_ACCESS_DENIED as i32 || code == ERROR_PIPE_BUSY as i32) => {
                    continue
                }
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not reserve a fresh launch nonce pipe",
        ))
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Pin the child's kernel object before starting the serving thread. The
    /// caller still owns its child, so its PID cannot be recycled during this call.
    pub fn serve(self, pid: u32, deadline: Duration) -> io::Result<LaunchNoncePipeDelivery> {
        // SAFETY: OpenProcess returns a fresh handle; no inheritable rights are requested.
        let process = owned(unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) })?;
        let consumed = Arc::new(OnceLock::new());
        let cancelled = Arc::new(AtomicBool::new(false));
        let delivery = LaunchNoncePipeDelivery {
            consumed: consumed.clone(),
            cancelled: cancelled.clone(),
        };
        let deadline = Instant::now() + deadline;
        std::thread::Builder::new()
            .name("launch-nonce-pipe".into())
            .spawn(move || {
                let _ = self.deliver(pid, &process, deadline, &cancelled, &consumed);
            })?;
        Ok(delivery)
    }

    fn deliver(
        self,
        pid: u32,
        process: &OwnedHandle,
        deadline: Instant,
        cancelled: &AtomicBool,
        consumed: &OnceLock<()>,
    ) -> io::Result<()> {
        let handle = self.pipe.as_raw_handle();
        loop {
            // SAFETY: the process handle is pinned and was opened for synchronization.
            if Instant::now() >= deadline
                || cancelled.load(Ordering::Acquire)
                || unsafe { WaitForSingleObject(process.as_raw_handle(), 0) } == WAIT_OBJECT_0
            {
                return Err(timeout());
            }
            let mut operation = Operation::new(handle)?;
            // SAFETY: operation pins its event/OVERLAPPED until completion or cancellation.
            let connected = unsafe { ConnectNamedPipe(handle, &mut operation.overlapped) };
            if connected == 0 {
                let error = io::Error::last_os_error();
                match error.raw_os_error() {
                    Some(code) if code == ERROR_PIPE_CONNECTED as i32 => {}
                    Some(code) if code == ERROR_IO_PENDING as i32 => {
                        operation.wait(deadline, Some(process), Some(cancelled))?;
                    }
                    _ => return Err(error),
                }
            }
            let mut client_pid = 0;
            // SAFETY: connected server handle and valid writable output pointer.
            let matches = unsafe { GetNamedPipeClientProcessId(handle, &mut client_pid) } != 0
                && client_pid == pid;
            if !matches {
                // Disconnect a client whose PID is not the direct child, but
                // keep the server handle so nobody can re-create this name.
                // SAFETY: handle is our server instance.
                unsafe { DisconnectNamedPipe(handle) };
                if Instant::now() >= deadline || cancelled.load(Ordering::Acquire) {
                    return Err(timeout());
                }
                continue;
            }
            let mut operation = Operation::new(handle)?;
            let mut written = 0;
            // SAFETY: nonce and OVERLAPPED remain alive through wait and cancellation.
            let result = unsafe {
                WriteFile(
                    handle,
                    self.nonce.as_ptr(),
                    self.nonce.len() as u32,
                    &mut written,
                    &mut operation.overlapped,
                )
            };
            if result == 0 {
                if io::Error::last_os_error().raw_os_error() != Some(ERROR_IO_PENDING as i32) {
                    return Err(io::Error::last_os_error());
                }
                written = operation.wait(deadline, Some(process), Some(cancelled))?;
            }
            if written as usize != self.nonce.len() {
                return Err(io::Error::other("short launch nonce pipe write"));
            }
            let _ = consumed.set(());
            return Ok(());
        }
    }
}

/// Per-spawn consumption evidence, independent of anything the module declares.
/// Dropping the guard cancels a pending handoff (including a discarded candidate).
pub struct LaunchNoncePipeDelivery {
    consumed: Arc<OnceLock<()>>,
    cancelled: Arc<AtomicBool>,
}

impl LaunchNoncePipeDelivery {
    pub fn consumed(&self) -> Arc<OnceLock<()>> {
        self.consumed.clone()
    }
}

impl Drop for LaunchNoncePipeDelivery {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
    }
}

fn random_name() -> io::Result<String> {
    let mut bytes = [0u8; 16];
    // SAFETY: valid output buffer; the system RNG needs no provider handle.
    let status = unsafe {
        BCryptGenRandom(
            null_mut(),
            bytes.as_mut_ptr(),
            bytes.len() as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status != 0 {
        return Err(io::Error::other(
            "Windows RNG could not generate a launch pipe name",
        ));
    }
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!(r"\\.\pipe\subc-launch-{hex}"))
}

struct LocalAllocation(*mut std::ffi::c_void);
impl Drop for LocalAllocation {
    fn drop(&mut self) {
        // SAFETY: these allocations were returned by LocalAlloc-backed Windows APIs.
        unsafe { LocalFree(self.0) };
    }
}

fn user_security_descriptor() -> io::Result<LocalAllocation> {
    let mut token = null_mut();
    // SAFETY: current process pseudo-handle is valid; output points to initialized storage.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = owned(token)?;
    let mut needed = 0;
    // SAFETY: a null buffer requests the required TOKEN_USER size.
    unsafe { GetTokenInformation(token.as_raw_handle(), TokenUser, null_mut(), 0, &mut needed) };
    // usize storage provides alignment for TOKEN_USER and its SID.
    let mut buffer = vec![0usize; (needed as usize).div_ceil(size_of::<usize>())];
    // SAFETY: buffer is aligned and covers the size Windows requested.
    if unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buffer.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful TokenUser query initialized the TOKEN_USER header.
    let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    let mut sid = null_mut();
    // SAFETY: SID points inside the still-live token buffer.
    if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut sid) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let sid_allocation = LocalAllocation(sid.cast());
    let mut length = 0;
    // SAFETY: ConvertSidToStringSidW returns a null-terminated UTF-16 string.
    while unsafe { *sid.add(length) } != 0 {
        length += 1;
    }
    // SAFETY: length covers only the initialized SID string.
    let sid_text = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(sid, length) });
    drop(sid_allocation);
    // libuv's read-only fallback also requests FILE_WRITE_ATTRIBUTES to set
    // its own handle's read mode: https://github.com/nodejs/node/blob/main/deps/uv/src/win/pipe.c#L139-L194.
    // Grant that bit, GENERIC_READ and SYNCHRONIZE only. GENERIC_WRITE includes
    // FILE_CREATE_PIPE_INSTANCE and must never be granted to clients.
    let sddl = wide(OsStr::new(&format!(
        "D:P(A;;0x{CLIENT_ACCESS:08x};;;{sid_text})"
    )));
    let mut descriptor = null_mut();
    // SAFETY: null-terminated SDDL and valid output pointer; revision 1 is SDDL_REVISION_1.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &mut descriptor,
            null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(LocalAllocation(descriptor))
}

// OVERLAPPED operations must be cancelled and joined before their buffers or
// event are freed, even when the child exits or the deadline expires.
struct Operation {
    handle: HANDLE,
    overlapped: OVERLAPPED,
    _event: OwnedHandle,
}

impl Operation {
    fn new(handle: HANDLE) -> io::Result<Self> {
        // SAFETY: unnamed, manual-reset event; no handle inheritance.
        let event = owned(unsafe { CreateEventW(null(), 1, 0, null()) })?;
        let overlapped = OVERLAPPED {
            hEvent: event.as_raw_handle(),
            ..Default::default()
        };
        Ok(Self {
            handle,
            overlapped,
            _event: event,
        })
    }

    fn wait(
        &mut self,
        deadline: Instant,
        process: Option<&OwnedHandle>,
        cancelled: Option<&AtomicBool>,
    ) -> io::Result<u32> {
        loop {
            if Instant::now() >= deadline
                || cancelled.is_some_and(|flag| flag.load(Ordering::Acquire))
            {
                return Err(timeout());
            }
            // SAFETY: the borrowed OwnedHandle stays live through this wait and
            // was opened with the synchronization right required by the kernel.
            if process.is_some_and(|process| unsafe { WaitForSingleObject(process.as_raw_handle(), 0) } == WAIT_OBJECT_0) {
                return Err(io::Error::new(io::ErrorKind::BrokenPipe, "launch nonce child exited"));
            }
            // SAFETY: event belongs to this operation and remains live.
            match unsafe { WaitForSingleObject(self.overlapped.hEvent, POLL.as_millis() as u32) } {
                WAIT_OBJECT_0 => {
                    let mut transferred = 0;
                    // SAFETY: the signaled event means the OVERLAPPED has completed.
                    if unsafe {
                        GetOverlappedResult(self.handle, &self.overlapped, &mut transferred, 0)
                    } == 0
                    {
                        return Err(io::Error::last_os_error());
                    }
                    return Ok(transferred);
                }
                WAIT_TIMEOUT => {}
                _ => return Err(io::Error::last_os_error()),
            }
        }
    }
}

impl Drop for Operation {
    fn drop(&mut self) {
        // SAFETY: cancellation names only this operation; joining it keeps all
        // caller buffers alive until Windows has stopped using their pointers.
        unsafe {
            CancelIoEx(self.handle, &self.overlapped);
            let mut transferred = 0;
            GetOverlappedResult(self.handle, &self.overlapped, &mut transferred, 1);
        }
    }
}

fn timeout() -> io::Error {
    io::Error::from_raw_os_error(ERROR_SEM_TIMEOUT as i32)
}

pub(super) fn read_pipe(name: &OsStr) -> Cached {
    let path = wide(name);
    if path.len() <= 1 || path[..path.len() - 1].contains(&0) {
        return Err(LaunchNonceError::PipeNotOpen { errno: 123 });
    }
    let open_deadline = Instant::now() + OPEN_WINDOW;
    let pipe = loop {
        // SAFETY: valid name, no inheritance, read-only access. Identification
        // security quality of service lets the server inspect identity but not
        // impersonate this process's token, even if the server is counterfeit.
        let result = owned(unsafe {
            CreateFileW(
                path.as_ptr(),
                GENERIC_READ,
                0,
                null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                null_mut(),
            )
        });
        match result {
            Ok(pipe) => break pipe,
            Err(error)
                if error.raw_os_error() == Some(ERROR_PIPE_BUSY as i32)
                    && Instant::now() < open_deadline =>
            {
                std::thread::sleep(POLL)
            }
            Err(error) => {
                return Err(LaunchNonceError::PipeNotOpen {
                    errno: error.raw_os_error().unwrap_or(0),
                })
            }
        }
    };
    let deadline = Instant::now() + READ_WINDOW;
    let mut bytes = Vec::new();
    loop {
        let mut chunk = [0u8; 256];
        let mut operation = Operation::new(pipe.as_raw_handle()).map_err(pipe_unreadable)?;
        let mut read = 0;
        // SAFETY: chunk/OVERLAPPED stay live through completion and cancellation.
        let result = unsafe {
            ReadFile(
                pipe.as_raw_handle(),
                chunk.as_mut_ptr(),
                chunk.len() as u32,
                &mut read,
                &mut operation.overlapped,
            )
        };
        if result == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_BROKEN_PIPE as i32) {
                break;
            }
            if error.raw_os_error() != Some(ERROR_IO_PENDING as i32) {
                return Err(pipe_unreadable(error));
            }
            match operation.wait(deadline, None, None) {
                Ok(count) => read = count,
                Err(error) if error.raw_os_error() == Some(ERROR_BROKEN_PIPE as i32) => break,
                Err(error) => return Err(pipe_unreadable(error)),
            }
        }
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..read as usize]);
        if bytes.len() > MAX_NONCE_BYTES {
            return Err(LaunchNonceError::PipeUnreadable { errno: None });
        }
    }
    if bytes.is_empty() {
        return Err(LaunchNonceError::PipeEmpty);
    }
    let value = String::from_utf8(bytes).map_err(|_| LaunchNonceError::PipeNotUtf8)?;
    Ok(Some(LaunchNonce {
        value,
        source: LaunchNonceSource::Pipe,
    }))
}

fn pipe_unreadable(error: io::Error) -> LaunchNonceError {
    LaunchNonceError::PipeUnreadable {
        errno: error.raw_os_error(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::launch_nonce::{
        launch_nonce, LaunchNonceCell, LAUNCH_NONCE_ENV, LAUNCH_NONCE_PIPE_ENV,
    };
    use std::process::{Command, Stdio};

    #[test]
    fn windows_launch_pipe_dacl_grants_exact_client_access() {
        use windows_sys::Win32::Foundation::{GetHandleInformation, HANDLE_FLAG_INHERIT};
        use windows_sys::Win32::Security::{
            Authorization::{GetSecurityInfo, SE_KERNEL_OBJECT},
            GetAce, GetSecurityDescriptorControl, ACCESS_ALLOWED_ACE, DACL_SECURITY_INFORMATION,
            SE_DACL_PROTECTED,
        };
        let pipe = LaunchNoncePipeHandoff::new("secret").unwrap();
        let mut flags = 0;
        // SAFETY: the server handle is live and flags is a valid output pointer.
        assert_ne!(
            unsafe { GetHandleInformation(pipe.pipe.as_raw_handle(), &mut flags) },
            0
        );
        assert_eq!(flags & HANDLE_FLAG_INHERIT, 0);
        let mut dacl = null_mut();
        let mut descriptor = null_mut();
        // SAFETY: the server handle is live, and all outputs name initialized storage.
        assert_eq!(
            unsafe {
                GetSecurityInfo(
                    pipe.pipe.as_raw_handle(),
                    SE_KERNEL_OBJECT,
                    DACL_SECURITY_INFORMATION,
                    null_mut(),
                    null_mut(),
                    &mut dacl,
                    null_mut(),
                    &mut descriptor,
                )
            },
            0
        );
        let _descriptor = LocalAllocation(descriptor);
        assert!(!dacl.is_null());
        // SAFETY: GetSecurityInfo returned a valid DACL.
        assert_eq!(unsafe { (*dacl).AceCount }, 1);
        let mut ace = null_mut();
        // SAFETY: DACL has exactly one ACE and ace is a valid output pointer.
        assert_ne!(unsafe { GetAce(dacl, 0, &mut ace) }, 0);
        // SAFETY: the only ACE was created as an ACCESS_ALLOWED_ACE.
        let mask = unsafe { (*ace.cast::<ACCESS_ALLOWED_ACE>()).Mask };
        // Windows may store GENERIC_READ verbatim or expand its pipe rights.
        assert!(
            mask == 0x80100100 || mask == 0x00120189,
            "unexpected client access mask: {mask:#x}"
        );
        assert_eq!(mask & (0x40000000 | 0x4), 0);
        let mut control = 0;
        let mut revision = 0;
        // SAFETY: descriptor is live and output pointers are valid.
        assert_ne!(
            unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) },
            0
        );
        assert_ne!(control & SE_DACL_PROTECTED, 0);
    }

    #[test]
    fn windows_pipe_child_fixture() {
        if std::env::var_os("SUBC_PIPE_TEST_CHILD").is_none() {
            return;
        }
        std::thread::sleep(Duration::from_millis(250));
        let nonce = launch_nonce()
            .expect("child pipe read")
            .expect("child nonce");
        assert_eq!(nonce.source(), LaunchNonceSource::Pipe);
        assert_eq!(launch_nonce().unwrap(), Some(nonce.clone()));
        assert_eq!(std::env::var(LAUNCH_NONCE_ENV).unwrap(), "environment-copy");
        println!("child-pipe:{}", nonce.value());
    }

    fn spawn_reader(pipe: &LaunchNoncePipeHandoff) -> std::process::Child {
        Command::new(std::env::current_exe().unwrap())
            .args([
                "launch_nonce::windows::tests::windows_pipe_child_fixture",
                "--exact",
                "--nocapture",
            ])
            .env("SUBC_PIPE_TEST_CHILD", "1")
            .env(LAUNCH_NONCE_PIPE_ENV, pipe.name())
            .env(LAUNCH_NONCE_ENV, "environment-copy")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }

    fn assert_child_reads(child: std::process::Child, evidence: &OnceLock<()>) {
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("child-pipe:test-secret"),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(evidence.get().is_some(), "server records pipe consumption");
    }

    #[test]
    fn windows_supervisor_child_reads_secret_through_pipe() {
        let pipe = LaunchNoncePipeHandoff::new("test-secret").unwrap();
        let child = spawn_reader(&pipe);
        let delivery = pipe.serve(child.id(), Duration::from_secs(5)).unwrap();
        assert_child_reads(child, &delivery.consumed());
    }

    #[test]
    fn windows_wrong_pid_gets_nothing_and_real_child_still_reads() {
        let pipe = LaunchNoncePipeHandoff::new("test-secret").unwrap();
        let name = pipe.name().to_owned();
        let child = spawn_reader(&pipe);
        let delivery = pipe.serve(child.id(), Duration::from_secs(5)).unwrap();
        assert_eq!(
            read_pipe(OsStr::new(&name)),
            Err(LaunchNonceError::PipeEmpty)
        );
        assert_child_reads(child, &delivery.consumed());
    }

    #[test]
    fn windows_name_collision_picks_fresh_pipe() {
        let occupied = LaunchNoncePipeHandoff::new("occupied").unwrap();
        let mut calls = 0;
        let pipe = LaunchNoncePipeHandoff::with_names("test-secret", || {
            calls += 1;
            if calls == 1 {
                Ok(occupied.name().to_owned())
            } else {
                random_name()
            }
        })
        .unwrap();
        assert_eq!(calls, 2);
        assert_ne!(pipe.name(), occupied.name());
        let child = spawn_reader(&pipe);
        let delivery = pipe.serve(child.id(), Duration::from_secs(5)).unwrap();
        assert_child_reads(child, &delivery.consumed());
    }

    #[test]
    fn windows_gone_pipe_never_falls_back_and_caches_failure() {
        let name = LaunchNoncePipeHandoff::new("secret")
            .unwrap()
            .name()
            .to_owned();
        let cell = LaunchNonceCell::new();
        let result = cell.get(|key| match key {
            LAUNCH_NONCE_PIPE_ENV => Some(name.clone().into()),
            LAUNCH_NONCE_ENV => panic!("must not look up environment fallback"),
            _ => None,
        });
        assert!(matches!(result, Err(LaunchNonceError::PipeNotOpen { .. })));
        assert_eq!(cell.get(|_| panic!("must use cached failure")), result);
    }
}

#[cfg(all(test, windows))]
mod lifecycle_tests {
    use super::*;
    use std::process::{Command, Stdio};

    #[test]
    fn windows_pipe_idle_child_fixture() {
        if std::env::var_os("SUBC_PIPE_IDLE_CHILD").is_some() {
            std::thread::sleep(Duration::from_secs(1));
        }
    }

    fn idle_child() -> std::process::Child {
        Command::new(std::env::current_exe().unwrap())
            .args([
                "launch_nonce::windows::lifecycle_tests::windows_pipe_idle_child_fixture",
                "--exact",
            ])
            .env("SUBC_PIPE_IDLE_CHILD", "1")
            .stdout(Stdio::null())
            .spawn()
            .unwrap()
    }

    #[test]
    fn windows_registration_deadline_closes_unconsumed_pipe() {
        let pipe = LaunchNoncePipeHandoff::new("secret").unwrap();
        let name = pipe.name().to_owned();
        let mut child = idle_child();
        let delivery = pipe.serve(child.id(), Duration::from_millis(30)).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert!(matches!(
            read_pipe(OsStr::new(&name)),
            Err(LaunchNonceError::PipeNotOpen { .. })
        ));
        assert!(delivery.consumed().get().is_none());
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn windows_child_exit_closes_unconsumed_pipe() {
        let pipe = LaunchNoncePipeHandoff::new("secret").unwrap();
        let name = pipe.name().to_owned();
        let mut child = idle_child();
        let delivery = pipe.serve(child.id(), Duration::from_secs(5)).unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert!(matches!(
            read_pipe(OsStr::new(&name)),
            Err(LaunchNonceError::PipeNotOpen { .. })
        ));
        assert!(delivery.consumed().get().is_none());
    }
}
