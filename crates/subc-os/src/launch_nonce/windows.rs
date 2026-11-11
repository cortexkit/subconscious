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
        ERROR_PIPE_CONNECTED, ERROR_PIPE_NOT_CONNECTED, ERROR_SEM_TIMEOUT, GENERIC_READ, HANDLE,
        INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT,
    },
    Security::Cryptography::{BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG},
    Security::{
        Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        },
        GetTokenInformation, TokenUser, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
    },
    Storage::FileSystem::{
        CreateFileW, FlushFileBuffers, ReadFile, WriteFile, FILE_FLAG_FIRST_PIPE_INSTANCE,
        FILE_FLAG_OVERLAPPED, FILE_WRITE_DATA, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
        PIPE_ACCESS_OUTBOUND, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
    },
    System::{
        Pipes::{
            ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeClientProcessId,
            GetNamedPipeServerProcessId, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
        },
        Threading::{
            CreateEventW, GetCurrentProcess, OpenProcess, OpenProcessToken, WaitForSingleObject,
            PROCESS_SYNCHRONIZE,
        },
        IO::{CancelIoEx, CancelSynchronousIo, GetOverlappedResult, OVERLAPPED},
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

/// Owns the named-pipe server created before the daemon starts the module.
/// While its handle stays open, another process cannot create an instance with
/// the same name. Dropping it closes that handle and releases the pipe name.
pub struct LaunchNoncePipeHandoff {
    pipe: OwnedHandle,
    name: String,
    nonce: zeroize::Zeroizing<Vec<u8>>,
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
        let mut pipe = Self::reserve_with_names(&mut next)?;
        pipe.nonce = zeroize::Zeroizing::new(nonce.as_bytes().to_vec());
        Ok(pipe)
    }

    pub(crate) fn reserve() -> io::Result<Self> {
        Self::reserve_with_names(random_name)
    }

    fn reserve_with_names(mut next: impl FnMut() -> io::Result<String>) -> io::Result<Self> {
        Self::reserve_access(&mut next, PIPE_ACCESS_OUTBOUND)
    }

    fn reserve_access(
        mut next: impl FnMut() -> io::Result<String>,
        access: u32,
    ) -> io::Result<Self> {
        let duplex = access == PIPE_ACCESS_DUPLEX;
        let security = user_security_descriptor(if duplex {
            CLIENT_ACCESS | FILE_WRITE_DATA
        } else {
            CLIENT_ACCESS
        })?;
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
                    access | FILE_FLAG_FIRST_PIPE_INSTANCE | FILE_FLAG_OVERLAPPED,
                    PIPE_TYPE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                    1,
                    MAX_NONCE_BYTES as u32,
                    if duplex { MAX_NONCE_BYTES as u32 } else { 0 },
                    0,
                    &attrs,
                )
            });
            match result {
                Ok(pipe) => {
                    return Ok(Self {
                        pipe,
                        name,
                        nonce: zeroize::Zeroizing::new(Vec::new()),
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
        consumed: &Arc<OnceLock<()>>,
    ) -> io::Result<()> {
        self.deliver_with_policy(pid, process, deadline, cancelled, consumed, false)
    }

    fn deliver_with_policy(
        self,
        pid: u32,
        process: &OwnedHandle,
        deadline: Instant,
        cancelled: &AtomicBool,
        consumed: &Arc<OnceLock<()>>,
        reject_foreign: bool,
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
                        operation.pending = true;
                        operation.wait(deadline, Some(process), Some(cancelled))?;
                    }
                    _ => return Err(error),
                }
            }
            drop(operation);
            let mut client_pid = 0;
            // SAFETY: connected server handle and valid writable output pointer.
            let matches = unsafe { GetNamedPipeClientProcessId(handle, &mut client_pid) } != 0
                && client_pid == pid;
            if !matches {
                // Disconnect a client whose PID is not the direct child, but
                // keep the server handle so nobody can re-create this name.
                // SAFETY: handle is our server instance.
                unsafe { DisconnectNamedPipe(handle) };
                if reject_foreign {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "unexpected helper pipe client",
                    ));
                }
                if Instant::now() >= deadline || cancelled.load(Ordering::Acquire) {
                    return Err(timeout());
                }
                continue;
            }
            let mut operation = Operation::new(handle)?;
            // SAFETY: nonce and OVERLAPPED remain alive through wait and cancellation.
            let result = unsafe {
                WriteFile(
                    handle,
                    self.nonce.as_ptr(),
                    self.nonce.len() as u32,
                    null_mut(),
                    &mut operation.overlapped,
                )
            };
            if result == 0 {
                if io::Error::last_os_error().raw_os_error() != Some(ERROR_IO_PENDING as i32) {
                    return Err(io::Error::last_os_error());
                }
                operation.pending = true;
            }
            // Overlapped byte counts come from GetOverlappedResult, including
            // operations that completed before WriteFile returned.
            let written = operation.wait(deadline, Some(process), Some(cancelled))?;
            if written as usize != self.nonce.len() {
                return Err(io::Error::other("short launch nonce pipe write"));
            }
            drop(operation);
            return flush_delivery(self.pipe, process, deadline, cancelled, consumed.clone());
        }
    }
}

impl LaunchNoncePipeHandoff {
    pub(crate) fn registration() -> io::Result<Self> {
        Self::reserve_access(random_name, PIPE_ACCESS_DUPLEX)
    }

    pub(crate) fn accept_registration(
        self,
        pid: u32,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> io::Result<std::fs::File> {
        // SAFETY: a fresh, non-inheritable process handle pins the child's PID.
        let process = owned(unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) })?;
        let handle = self.pipe.as_raw_handle();
        loop {
            // SAFETY: the pinned process handle is open for synchronization.
            if cancelled.load(Ordering::Acquire)
                || Instant::now() >= deadline
                || unsafe { WaitForSingleObject(process.as_raw_handle(), 0) } == WAIT_OBJECT_0
            {
                return Err(timeout());
            }
            // Explicit retry cleanup must not move a pending OVERLAPPED: the
            // kernel and CancelIoEx identify it by its original address.
            let mut operation = Box::new(Operation::new(handle)?);
            // SAFETY: the OVERLAPPED and event stay live until cancellation or completion.
            if unsafe { ConnectNamedPipe(handle, &mut operation.overlapped) } == 0 {
                let error = io::Error::last_os_error();
                match error.raw_os_error() {
                    Some(code) if code == ERROR_PIPE_CONNECTED as i32 => {}
                    Some(code) if code == ERROR_IO_PENDING as i32 => {
                        operation.pending = true;
                        if operation
                            .wait(deadline, Some(&process), Some(cancelled))
                            .is_err()
                        {
                            // A peer may disappear before the overlapped connect
                            // completes. Keep the first instance and its name;
                            // only the child exit or deadline ends acceptance.
                            drop(operation);
                            unsafe { DisconnectNamedPipe(handle) };
                            std::thread::sleep(POLL);
                            continue;
                        }
                    }
                    _ => {
                        // ERROR_NO_DATA means a peer connected and closed before
                        // ConnectNamedPipe. Reset this instance, never its name.
                        drop(operation);
                        unsafe { DisconnectNamedPipe(handle) };
                        std::thread::sleep(POLL);
                        continue;
                    }
                }
            }
            drop(operation);
            let mut client_pid = 0;
            // SAFETY: connected server handle and writable PID output. No reads precede this check.
            if unsafe { GetNamedPipeClientProcessId(handle, &mut client_pid) } != 0
                && client_pid == pid
            {
                return Ok(std::fs::File::from(self.pipe));
            }
            // SAFETY: disconnect only our client; keep the original first-instance handle.
            // An already-disconnected foreign peer can make this fail. Neither
            // the failure nor that peer may release the reserved server name.
            unsafe { DisconnectNamedPipe(handle) };
            std::thread::sleep(POLL);
        }
    }

    pub(crate) fn registration_shutdown_handle(&self) -> io::Result<std::fs::File> {
        self.pipe.try_clone().map(std::fs::File::from)
    }
}

/// Bound helper startup and flushing so a helper that never reads cannot keep
/// its owner's handoff alive indefinitely. Expiry closes the reserved instance.
pub const HELPER_CLAIM_CONNECT_WINDOW: Duration = Duration::from_secs(5);

/// A one-use helper pipe. Neither its Debug output nor its environment metadata
/// includes the claim. Unlike the daemon's legacy nonce server, a foreign client
/// ends this handoff instead of retrying with the real helper.
pub struct HelperClaimHandoff(
    crate::registration_channel::PreparedHelperClaimHandoff,
    zeroize::Zeroizing<String>,
);
impl std::fmt::Debug for HelperClaimHandoff {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HelperClaimHandoff(<redacted>)")
    }
}

impl HelperClaimHandoff {
    pub fn new(claim: &str) -> io::Result<Self> {
        crate::registration_channel::check_claim(claim)?;
        Ok(Self(
            crate::registration_channel::PreparedHelperClaimHandoff::new(3)?,
            zeroize::Zeroizing::new(claim.to_owned()),
        ))
    }

    /// Set only public metadata before spawn. The helper's stdin is untouched.
    pub fn configure(&self, command: &mut std::process::Command) {
        command.envs(self.0.environment());
    }

    /// Call after spawn with the live helper's PID. The caller retains its
    /// child until this bounded delivery returns, preventing PID reuse.
    pub fn serve(self, pid: u32) -> io::Result<()> {
        self.0.complete(pid, &self.1)
    }
}

impl LaunchNoncePipeHandoff {
    pub(crate) fn deliver_helper(mut self, pid: u32, claim: &str) -> io::Result<()> {
        self.nonce = zeroize::Zeroizing::new(claim.as_bytes().to_vec());
        // SAFETY: a fresh, non-inheritable process handle pins the live child.
        let process = owned(unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) })?;
        self.deliver_with_policy(
            pid,
            &process,
            Instant::now() + HELPER_CLAIM_CONNECT_WINDOW,
            &AtomicBool::new(false),
            &Arc::new(OnceLock::new()),
            true,
        )
    }
}

/// Records whether the process started for this spawn read every nonce byte.
/// The daemon sets this only after the pipe flush completes; it does not trust
/// the source field sent by the module in its registration message.
/// Dropping this guard asks the serving thread to stop waiting and close the
/// pipe, including when a replacement process is discarded before promotion.
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

fn flush_delivery(
    pipe: OwnedHandle,
    process: &OwnedHandle,
    deadline: Instant,
    cancelled: &AtomicBool,
    consumed: Arc<OnceLock<()>>,
) -> io::Result<()> {
    // Write completion only means Windows buffered the nonce. Flush waits for
    // the client to drain it before disconnect/close can discard unread bytes:
    // https://learn.microsoft.com/en-us/windows/win32/ipc/named-pipe-operations.
    // FlushFileBuffers is synchronous even on an overlapped handle. Give it a
    // dedicated thread so cancellation cannot target unrelated daemon I/O.
    let flush = std::thread::Builder::new()
        .name("launch-nonce-flush".into())
        .spawn(move || {
            // SAFETY: this thread uniquely owns the live server pipe handle.
            let result = if unsafe { FlushFileBuffers(pipe.as_raw_handle()) } == 0 {
                Err(io::Error::last_os_error())
            } else {
                // Record the completed read before disconnecting sends EOF.
                // The module can register as soon as it receives EOF, so the
                // daemon's list/status and supervisor.provenance responses must
                // already report pipe rather than the offered environment copy.
                let _ = consumed.set(());
                Ok(())
            };
            // SAFETY: the flush has finished; no asynchronous operation still
            // uses the handle. A failed flush deliberately abandons delivery.
            unsafe { DisconnectNamedPipe(pipe.as_raw_handle()) };
            result
        })?;

    let mut wait_error = None;
    while !flush.is_finished() {
        // SAFETY: JoinHandle owns a live thread handle until join below.
        match unsafe { WaitForSingleObject(flush.as_raw_handle(), POLL.as_millis() as u32) } {
            WAIT_OBJECT_0 => break,
            WAIT_TIMEOUT => {}
            _ => {
                wait_error = Some(io::Error::last_os_error());
                std::thread::sleep(POLL);
            }
        }
        // SAFETY: process stays live through this borrowed synchronize handle.
        if wait_error.is_some()
            || Instant::now() >= deadline
            || cancelled.load(Ordering::Acquire)
            || unsafe { WaitForSingleObject(process.as_raw_handle(), 0) } == WAIT_OBJECT_0
        {
            // Repeat cancellation until the worker exits: it might not have
            // entered FlushFileBuffers when the first request was made.
            // SAFETY: this is the dedicated flush thread, not a reusable pool
            // thread; its only blocking I/O is on the launch pipe.
            unsafe { CancelSynchronousIo(flush.as_raw_handle()) };
        }
    }
    let result = flush
        .join()
        .map_err(|_| io::Error::other("launch nonce flush thread panicked"))?;
    match wait_error {
        Some(error) => Err(error),
        None => result,
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

fn user_security_descriptor(client_access: u32) -> io::Result<LocalAllocation> {
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
    // Grant that bit, GENERIC_READ and SYNCHRONIZE. Duplex registration also
    // grants FILE_WRITE_DATA, but never GENERIC_WRITE: it includes the right
    // to create another server instance under the same name.
    let sddl = wide(OsStr::new(&format!(
        "D:P(A;;0x{client_access:08x};;;{sid_text})"
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
    pending: bool,
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
            pending: false,
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
                    self.pending = false;
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
        // ERROR_PIPE_CONNECTED and immediate EOF do not submit an operation.
        // Their events need not be signaled, so joining them could wait forever.
        if !self.pending {
            return;
        }
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
    read_open_pipe(open_pipe(name)?)
}

pub(crate) fn read_helper_pipe(
    name: &OsStr,
    parent: u32,
) -> Result<Option<String>, LaunchNonceError> {
    let pipe = open_pipe(name)?;
    let mut server = 0;
    // SAFETY: live pipe and valid output pointer; no bytes have been read.
    if unsafe { GetNamedPipeServerProcessId(pipe.as_raw_handle(), &mut server) } == 0
        || server != parent
    {
        return Err(LaunchNonceError::PipeUnreadable { errno: None });
    }
    read_open_pipe(pipe).map(|nonce| nonce.map(|nonce| nonce.value))
}

fn open_pipe(name: &OsStr) -> Result<OwnedHandle, LaunchNonceError> {
    let path = wide(name);
    if path.len() <= 1 || path[..path.len() - 1].contains(&0) {
        return Err(LaunchNonceError::PipeNotOpen { errno: 123 });
    }
    let open_deadline = Instant::now() + OPEN_WINDOW;
    loop {
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
            Ok(pipe) => return Ok(pipe),
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
    }
}

fn read_open_pipe(pipe: OwnedHandle) -> Cached {
    let deadline = Instant::now() + READ_WINDOW;
    let mut bytes = Vec::new();
    loop {
        let mut chunk = [0u8; 256];
        let mut operation = Operation::new(pipe.as_raw_handle()).map_err(pipe_unreadable)?;
        // SAFETY: chunk/OVERLAPPED stay live through completion and cancellation.
        let result = unsafe {
            ReadFile(
                pipe.as_raw_handle(),
                chunk.as_mut_ptr(),
                chunk.len() as u32,
                null_mut(),
                &mut operation.overlapped,
            )
        };
        if result == 0 {
            let error = io::Error::last_os_error();
            if pipe_eof(&error) {
                break;
            }
            if error.raw_os_error() != Some(ERROR_IO_PENDING as i32) {
                return Err(pipe_unreadable(error));
            }
            operation.pending = true;
        }
        let read = match operation.wait(deadline, None, None) {
            Ok(count) => count,
            Err(error) if pipe_eof(&error) => break,
            Err(error) => return Err(pipe_unreadable(error)),
        };
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

fn pipe_eof(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(code) if code == ERROR_BROKEN_PIPE as i32 || code == ERROR_PIPE_NOT_CONNECTED as i32)
}

fn pipe_unreadable(error: io::Error) -> LaunchNonceError {
    LaunchNonceError::PipeUnreadable {
        errno: error.raw_os_error(),
    }
}

#[cfg(test)]
mod registration_channel_tests {
    use super::*;
    use crate::registration_channel::*;
    use std::{
        io::{Read, Write},
        process::{Command, Stdio},
    };
    use windows_sys::Win32::{
        Foundation::{GetHandleInformation, HANDLE_FLAG_INHERIT},
        Storage::FileSystem::PIPE_ACCESS_DUPLEX,
    };

    const CHANNEL_TEST: &str =
        "launch_nonce::windows::registration_channel_tests::registration_channel_contract";
    const HELPER_TEST: &str =
        "launch_nonce::windows::registration_channel_tests::helper_claim_handoff_contract";
    const SERVER_TEST: &str = "launch_nonce::windows::registration_channel_tests::registration_server_rejects_foreign_and_retains_instance";
    const MODE: &str = "SUBC_OS_REGISTRATION_TEST_MODE";
    const CLAIM: &str = "test-helper-claim-not-for-logs";

    fn child(test: &str, mode: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([test, "--exact", "--nocapture"])
            .env(MODE, mode)
            .env_remove(HELPER_CLAIM_PIPE_ENV)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }
    fn success(output: std::process::Output) {
        assert!(
            output.status.success(),
            "child failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!String::from_utf8_lossy(&output.stdout).contains(CLAIM));
        assert!(!String::from_utf8_lossy(&output.stderr).contains(CLAIM));
    }
    fn begin_connect(pipe: &OwnedHandle) -> Box<Operation> {
        // The pending OVERLAPPED must not move when returned to the caller.
        let mut operation = Box::new(Operation::new(pipe.as_raw_handle()).unwrap());
        // SAFETY: the operation pins the OVERLAPPED until completion.
        if unsafe { ConnectNamedPipe(pipe.as_raw_handle(), &mut operation.overlapped) } == 0 {
            match io::Error::last_os_error().raw_os_error() {
                Some(code) if code == ERROR_PIPE_CONNECTED as i32 => {}
                Some(code) if code == ERROR_IO_PENDING as i32 => {
                    operation.pending = true;
                }
                other => panic!("connect failed: {other:?}"),
            }
        }
        operation
    }
    fn finish_connect(operation: &mut Operation) {
        if operation.pending {
            operation
                .wait(Instant::now() + READ_WINDOW, None, None)
                .unwrap();
        }
    }
    fn transfer(pipe: &OwnedHandle, writing: bool) -> io::Result<u32> {
        let mut byte = b'x';
        let mut operation = Operation::new(pipe.as_raw_handle())?;
        // SAFETY: byte and OVERLAPPED stay live until completion or cancellation.
        let result = unsafe {
            if writing {
                WriteFile(
                    pipe.as_raw_handle(),
                    &byte,
                    1,
                    null_mut(),
                    &mut operation.overlapped,
                )
            } else {
                ReadFile(
                    pipe.as_raw_handle(),
                    &mut byte,
                    1,
                    null_mut(),
                    &mut operation.overlapped,
                )
            }
        };
        if result == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(ERROR_IO_PENDING as i32) {
                return Err(error);
            }
            operation.pending = true;
        }
        let count = operation.wait(Instant::now() + READ_WINDOW, None, None)?;
        if count > 0 {
            assert_eq!(byte, b'x');
        }
        Ok(count)
    }

    #[test]
    fn registration_server_rejects_foreign_and_retains_instance() {
        if std::env::var_os(MODE).is_some() {
            let mut release = [0];
            std::io::stdin().read_exact(&mut release).unwrap();
            let pipe: OwnedHandle = take_registration_channel()
                .unwrap()
                .unwrap()
                .into_file()
                .into();
            assert_eq!(transfer(&pipe, true).unwrap(), 1);
            assert_eq!(transfer(&pipe, false).unwrap(), 1);
            return;
        }
        let pipe = RegistrationPipe::new().unwrap();
        let name = pipe.name().to_owned();
        let mut process = child(SERVER_TEST, "direct-child")
            .env(REGISTRATION_PIPE_ENV, &name)
            .env(DAEMON_PID_ENV, std::process::id().to_string())
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        let path = wide(OsStr::new(&name));
        // SAFETY: a live pipe name and non-inheritable overlapped client handle.
        let foreign = owned(unsafe {
            CreateFileW(
                path.as_ptr(),
                GENERIC_READ | FILE_WRITE_DATA,
                0,
                null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                null_mut(),
            )
        })
        .unwrap();
        assert_eq!(transfer(&foreign, true).unwrap(), 1);
        let pid = process.id();
        let accepting =
            std::thread::spawn(move || pipe.accept(pid, Instant::now() + Duration::from_secs(5)));
        let read = transfer(&foreign, false);
        assert!(matches!(&read, Ok(0)) || read.as_ref().err().is_some_and(pipe_eof));
        // Neither a second instance nor a replacement first instance is allowed
        // after rejection. The original handle must reserve the name throughout.
        for flags in [0, FILE_FLAG_FIRST_PIPE_INSTANCE] {
            // SAFETY: valid name; an accidentally successful handle is closed below.
            assert!(owned(unsafe {
                CreateNamedPipeW(
                    path.as_ptr(),
                    PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | flags,
                    PIPE_TYPE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                    1,
                    512,
                    512,
                    0,
                    null(),
                )
            })
            .is_err());
        }
        process.stdin.take().unwrap().write_all(b"x").unwrap();
        let server: OwnedHandle = accepting.join().unwrap().unwrap().into();
        let mut accepted_pid = 0;
        // SAFETY: accepted pipe and scalar output.
        assert_ne!(
            unsafe { GetNamedPipeClientProcessId(server.as_raw_handle(), &mut accepted_pid) },
            0
        );
        assert_eq!(accepted_pid, pid);
        assert_eq!(transfer(&server, false).unwrap(), 1);
        assert_eq!(transfer(&server, true).unwrap(), 1);
        success(process.wait_with_output().unwrap());
    }

    #[test]
    fn registration_server_uses_registration_deadline() {
        let pipe = RegistrationPipe::new().unwrap();
        let deadline = Instant::now() + Duration::from_millis(1100);
        // The legacy nonce server expires at one second. Registration retains
        // its instance until the caller's deadline, even without a client.
        let result = pipe.accept(std::process::id(), deadline);
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
        assert!(Instant::now() >= deadline);
    }

    #[test]
    fn registration_channel_contract() {
        if let Ok(mode) = std::env::var(MODE) {
            let good = mode == "channel-good";
            assert_eq!(supervised(), good);
            if good {
                let file = take_registration_channel().unwrap().unwrap().into_file();
                let pipe: OwnedHandle = file.into();
                let mut flags = 0;
                // SAFETY: retained handle and scalar output.
                assert_ne!(
                    unsafe { GetHandleInformation(pipe.as_raw_handle(), &mut flags) },
                    0
                );
                assert_eq!(flags & HANDLE_FLAG_INHERIT, 0);
                assert!(supervised());
                assert!(matches!(
                    take_registration_channel(),
                    Err(RegistrationChannelError::AlreadyTaken)
                ));
                assert_eq!(transfer(&pipe, true).unwrap(), 1, "duplex client write");
                assert_eq!(transfer(&pipe, false).unwrap(), 1, "duplex client read");
            } else {
                assert!(take_registration_channel().unwrap().is_none());
                assert_eq!(
                    registration_refusal(),
                    Some(if mode == "channel-foreign-server" {
                        RegistrationRefusal::WrongPeer
                    } else {
                        RegistrationRefusal::NotOpen
                    })
                );
            }
            return;
        }
        for good in [true, false] {
            let name = random_name().unwrap();
            let path = wide(OsStr::new(&name));
            // SAFETY: unique name, non-inheritable single-instance duplex pipe.
            let pipe = owned(unsafe {
                CreateNamedPipeW(
                    path.as_ptr(),
                    PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE | FILE_FLAG_OVERLAPPED,
                    PIPE_TYPE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                    1,
                    512,
                    512,
                    0,
                    null(),
                )
            })
            .unwrap();
            // Arm connect before the child can open, validate and immediately
            // close a refused pipe, so the test cannot miss that connection.
            let mut connecting = begin_connect(&pipe);
            let process = child(
                CHANNEL_TEST,
                if good {
                    "channel-good"
                } else {
                    "channel-foreign-server"
                },
            )
            .env(REGISTRATION_PIPE_ENV, name)
            .env(
                DAEMON_PID_ENV,
                (std::process::id() + u32::from(!good)).to_string(),
            )
            .spawn()
            .unwrap();
            finish_connect(&mut connecting);
            if good {
                assert_eq!(transfer(&pipe, false).unwrap(), 1);
                assert_eq!(transfer(&pipe, true).unwrap(), 1);
            } else {
                let read = transfer(&pipe, false);
                assert!(
                    matches!(&read, Ok(0)) || read.as_ref().err().is_some_and(pipe_eof),
                    "foreign-server check must precede every write"
                );
            }
            success(process.wait_with_output().unwrap());
        }
        success(
            child(CHANNEL_TEST, "channel-missing")
                .env(REGISTRATION_PIPE_ENV, random_name().unwrap())
                .env(DAEMON_PID_ENV, std::process::id().to_string())
                .output()
                .unwrap(),
        );
    }

    #[test]
    fn helper_claim_handoff_contract() {
        if let Ok(mode) = std::env::var(MODE) {
            if mode == "helper-good" {
                let claim = helper_claim().unwrap().unwrap();
                assert!(claim.value() == CLAIM, "claim mismatch");
                assert!(!format!("{claim:?}").contains(CLAIM));
                assert!(matches!(
                    helper_claim(),
                    Err(HelperClaimError::AlreadyTaken)
                ));
                let mut stdin = String::new();
                std::io::stdin().read_to_string(&mut stdin).unwrap();
                assert_eq!(stdin, "stdio-still-available");
            } else {
                // The parent releases stdin only after the foreign client's
                // one-use handoff has closed, without relying on scheduling.
                let mut release = [0];
                std::io::stdin().read_exact(&mut release).unwrap();
                assert!(
                    helper_claim().is_err(),
                    "foreign client must poison this handoff"
                );
            }
            return;
        }
        let handoff = HelperClaimHandoff::new(CLAIM).unwrap();
        assert!(!format!("{handoff:?}").contains(CLAIM));
        let mut command = child(HELPER_TEST, "helper-good");
        command.stdin(Stdio::piped());
        handoff.configure(&mut command);
        assert!(!format!("{command:?}").contains(CLAIM));
        let mut process = command.spawn().unwrap();
        process
            .stdin
            .take()
            .unwrap()
            .write_all(b"stdio-still-available")
            .unwrap();
        handoff.serve(process.id()).unwrap();
        success(process.wait_with_output().unwrap());

        let handoff = HelperClaimHandoff::new(CLAIM).unwrap();
        let name = handoff.0.name().to_owned();
        let mut command = child(HELPER_TEST, "helper-poisoned");
        command.stdin(Stdio::piped());
        handoff.configure(&mut command);
        let mut process = command.spawn().unwrap();
        // Connect as this process, not the spawned helper, before serving.
        let foreign = open_pipe(OsStr::new(&name)).unwrap();
        assert!(handoff.serve(process.id()).is_err());
        assert!(matches!(
            read_open_pipe(foreign),
            Err(LaunchNonceError::PipeEmpty)
        ));
        process.stdin.take().unwrap().write_all(b"x").unwrap();
        success(process.wait_with_output().unwrap());

        // A counterfeit server is refused before the helper reads any bytes.
        let pipe = LaunchNoncePipeHandoff::new(CLAIM).unwrap();
        let name = pipe.name().to_owned();
        let mut connecting = begin_connect(&pipe.pipe);
        let reader =
            std::thread::spawn(move || read_helper_pipe(OsStr::new(&name), std::process::id() + 1));
        finish_connect(&mut connecting);
        assert!(reader.join().unwrap().is_err());
        assert!(HelperClaimHandoff::new("").is_err());
        assert!(HelperClaimHandoff::new(&"x".repeat(513)).is_err());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::launch_nonce::{
        launch_nonce, LaunchNonceCell, LAUNCH_NONCE_ENV, LAUNCH_NONCE_PIPE_ENV,
        LAUNCH_NONCE_PIPE_FALLBACK_ENV,
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
            .env_remove(LAUNCH_NONCE_PIPE_FALLBACK_ENV)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }

    fn assert_child_reads(mut child: std::process::Child, evidence: &OnceLock<()>) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("child did not finish its bounded pipe handoff");
            }
            std::thread::sleep(POLL);
        }
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

    #[test]
    fn windows_phase_one_gone_pipe_uses_and_caches_the_environment_source() {
        let name = LaunchNoncePipeHandoff::new("secret")
            .unwrap()
            .name()
            .to_owned();
        let cell = LaunchNonceCell::new();
        let result = cell
            .get(|key| match key {
                LAUNCH_NONCE_PIPE_ENV => Some(name.clone().into()),
                LAUNCH_NONCE_PIPE_FALLBACK_ENV => Some("env".into()),
                LAUNCH_NONCE_ENV => Some("environment-secret".into()),
                _ => None,
            })
            .unwrap()
            .unwrap();
        assert_eq!(result.value(), "environment-secret");
        assert_eq!(result.source(), LaunchNonceSource::Env);
        assert_eq!(
            cell.get(|_| panic!("must use cached fallback")),
            Ok(Some(result))
        );
    }
}

#[cfg(test)]
mod buffered_delivery_tests {
    use super::*;
    use crate::launch_nonce::{
        launch_nonce, LAUNCH_NONCE_ENV, LAUNCH_NONCE_PIPE_ENV, LAUNCH_NONCE_PIPE_FALLBACK_ENV,
    };
    use cortexkit_test_support::ScratchDir;
    use std::{
        path::{Path, PathBuf},
        process::{Child, Command, Stdio},
    };
    use windows_sys::Win32::System::Pipes::PeekNamedPipe;

    struct Gates(ScratchDir);
    impl Gates {
        fn new() -> Self {
            Self(ScratchDir::new("subc-pipe-gates"))
        }
        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    fn wait_for(path: &Path) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !path.exists() {
            assert!(
                Instant::now() < deadline,
                "pipe child did not reach {}",
                path.display()
            );
            std::thread::sleep(POLL);
        }
    }

    #[test]
    fn windows_buffered_pipe_child_fixture() {
        let Some(root) = std::env::var_os("SUBC_PIPE_GATES") else {
            return;
        };
        let root = PathBuf::from(root);
        let name = std::env::var_os(LAUNCH_NONCE_PIPE_ENV).unwrap();
        let pipe = open_pipe(&name).unwrap();
        std::fs::write(root.join("connected"), b"ready").unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let mut available = 0;
            // SAFETY: this is the live read-only client handle, and only the
            // byte-count output is requested; no credential bytes leave it.
            assert_ne!(
                unsafe {
                    PeekNamedPipe(
                        pipe.as_raw_handle(),
                        null_mut(),
                        0,
                        null_mut(),
                        &mut available,
                        null_mut(),
                    )
                },
                0
            );
            if available != 0 {
                break;
            }
            assert!(Instant::now() < deadline, "server never buffered the nonce");
            std::thread::sleep(POLL);
        }
        std::fs::write(root.join("buffered"), b"ready").unwrap();
        while !root.join("read").exists() {
            let mut available = 0;
            // SAFETY: same live client handle and byte-count-only query as above.
            if unsafe {
                PeekNamedPipe(
                    pipe.as_raw_handle(),
                    null_mut(),
                    0,
                    null_mut(),
                    &mut available,
                    null_mut(),
                )
            } == 0
            {
                assert!(
                    pipe_eof(&io::Error::last_os_error()),
                    "unexpected pipe failure"
                );
                drop(pipe);
                let nonce = launch_nonce().unwrap().unwrap();
                assert_eq!(nonce.source(), LaunchNonceSource::Env);
                println!("child-env:{}", nonce.value());
                std::fs::write(root.join("closed"), b"ready").unwrap();
                return;
            }
            assert!(Instant::now() < deadline, "reader gate was not opened");
            std::thread::sleep(POLL);
        }
        let nonce = read_open_pipe(pipe).unwrap().unwrap();
        assert_eq!(nonce.source(), LaunchNonceSource::Pipe);
        println!("child-pipe:{}", nonce.value());
    }

    fn spawn_buffered_reader(
        pipe: &LaunchNoncePipeHandoff,
        gates: &Gates,
        fallback: bool,
    ) -> Child {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["launch_nonce::windows::buffered_delivery_tests::windows_buffered_pipe_child_fixture", "--exact", "--nocapture"])
            .env("SUBC_PIPE_GATES", gates.0.path())
            .env(LAUNCH_NONCE_PIPE_ENV, pipe.name())
            .env_remove(LAUNCH_NONCE_PIPE_FALLBACK_ENV)
            .env_remove(LAUNCH_NONCE_ENV)
            .stdout(Stdio::piped()).stderr(Stdio::piped());
        if fallback {
            command
                .env(LAUNCH_NONCE_PIPE_FALLBACK_ENV, "env")
                .env(LAUNCH_NONCE_ENV, "test-secret");
        }
        command.spawn().unwrap()
    }

    #[test]
    fn windows_client_connected_first_is_drained_before_consumption_and_eof() {
        let gates = Gates::new();
        let pipe = LaunchNoncePipeHandoff::new("test-secret").unwrap();
        let mut child = spawn_buffered_reader(&pipe, &gates, false);
        wait_for(&gates.path("connected"));
        let delivery = pipe.serve(child.id(), Duration::from_secs(5)).unwrap();
        wait_for(&gates.path("buffered"));
        assert!(
            delivery.consumed().get().is_none(),
            "buffering bytes is not client consumption"
        );
        std::fs::write(gates.path("read"), b"ready").unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                let _ = child.kill();
                panic!("connected-first reader did not reach EOF");
            }
            std::thread::sleep(POLL);
        }
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("child-pipe:test-secret"));
        assert!(delivery.consumed().get().is_some());
    }

    #[test]
    fn windows_flush_deadline_closes_pipe_without_reporting_consumption() {
        let gates = Gates::new();
        let pipe = LaunchNoncePipeHandoff::new("test-secret").unwrap();
        let child = spawn_buffered_reader(&pipe, &gates, true);
        wait_for(&gates.path("connected"));
        let delivery = pipe.serve(child.id(), Duration::from_secs(1)).unwrap();
        wait_for(&gates.path("buffered"));
        wait_for(&gates.path("closed"));
        assert!(delivery.consumed().get().is_none());
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("child-env:test-secret"));
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
