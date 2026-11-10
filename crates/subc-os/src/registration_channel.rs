//! The process's single registration connection. [`supervised`] caches the
//! validation decision once, independently of whether the connection has been
//! taken. [`take_registration_channel`] transfers ownership exactly once; a
//! second take is an error, never permission to connect as `direct`. Refused
//! descriptors are left untouched and reported by [`registration_refusal`].
//! Errors after validation are fatal adoption errors, not refusals.
//!
//! Call an accessor before spawning anything: adoption sets close-on-exec on
//! Unix. An inherited variable without the descriptor does not confer identity.
//! The SDK owns framing, claims, and process exit on registration-channel EOF.
//!
//! [`HelperClaimHandoff`] passes only an explicit `helper` claim, never a launch
//! nonce. Unix uses an inherited one-read pipe. Windows reserves a single-use
//! named pipe and checks both process IDs; it does not commandeer stdin, which
//! helpers may need for stdio protocols. Only pipe metadata enters the child's
//! environment. The daemon's nonce handoff to ungated spawns is unchanged.
//! [`PreparedHelperClaimHandoff`] separates preparation from delivery for runtimes
//! that own their child process: put its child descriptor in the chosen stdio
//! slot, spawn, then complete with the live child's PID and a freshly minted claim.
//! Drop it on a spawn error. [`HelperClaimHandoff`] is the convenience wrapper
//! for Rust callers using `std::process::Command` with a claim already minted.

use std::{
    ffi::OsString,
    fmt,
    sync::{Mutex, OnceLock},
};

#[cfg(all(windows, feature = "tokio"))]
mod tokio_pipe;

pub const REGISTRATION_FD_ENV: &str = "SUBC_REGISTRATION_FD";
pub const REGISTRATION_PIPE_ENV: &str = "SUBC_REGISTRATION_PIPE";
pub const DAEMON_PID_ENV: &str = "SUBC_DAEMON_PID";
pub const HELPER_CLAIM_FD_ENV: &str = "SUBC_HELPER_CLAIM_FD";
pub const HELPER_CLAIM_PIPE_ENV: &str = "SUBC_HELPER_CLAIM_PIPE";
pub const HELPER_PARENT_PID_ENV: &str = "SUBC_HELPER_PARENT_PID";

/// Parent-owned sources for the final, allocation-free descriptor placement.
/// Sources stay above all three reserved slots so stdio setup and placement
/// callbacks cannot overwrite one before it is copied.
#[cfg(unix)]
#[derive(Default)]
pub struct ChildDescriptorSlots {
    pub(crate) sources: [Option<std::os::fd::OwnedFd>; 3],
}

#[cfg(unix)]
#[allow(unsafe_code)]
impl ChildDescriptorSlots {
    pub fn nonce(&mut self, nonce: crate::LaunchNonceHandoff) {
        self.sources[0] = Some(nonce.into_descriptor());
    }

    pub fn channel(&mut self, channel: std::os::fd::OwnedFd) -> std::io::Result<()> {
        self.sources[1] = Some(outside_child_slots(channel)?);
        Ok(())
    }

    pub fn acknowledgement(&mut self, ack: std::os::fd::OwnedFd) -> std::io::Result<()> {
        self.sources[2] = Some(outside_child_slots(ack)?);
        Ok(())
    }

    /// Install after the placement barrier. Drop the command immediately after
    /// spawn to close the parent's copies of the child-only descriptors.
    pub fn install(self, command: &mut std::process::Command) {
        use std::os::unix::process::CommandExt;
        // SAFETY: sources are owned by the callback. Only dup2 and fcntl run in
        // the forked child; no allocation, destructors or locks run there.
        unsafe { command.pre_exec(move || self.place()) };
    }

    pub(crate) fn place(&self) -> std::io::Result<()> {
        use std::os::fd::AsRawFd;
        for (index, source) in self.sources.iter().enumerate() {
            if let Some(source) = source {
                let slot = index as i32 + 3;
                // SAFETY: parent preparation owns all sources above slot 5.
                place_descriptor(source.as_raw_fd(), slot)?;
            }
        }
        Ok(())
    }
}

#[cfg(unix)]
#[allow(unsafe_code)]
pub(crate) fn place_descriptor(source: i32, slot: i32) -> std::io::Result<()> {
    // SAFETY: callers retain the source across fork. Both calls are
    // async-signal-safe, and an errno-only error needs no allocation.
    if unsafe { libc::dup2(source, slot) } == -1
        || unsafe { libc::fcntl(slot, libc::F_SETFD, 0) } == -1
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(unix)]
#[allow(unsafe_code)]
pub(crate) fn outside_child_slots(
    descriptor: std::os::fd::OwnedFd,
) -> std::io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::{AsRawFd, FromRawFd};
    // SAFETY: duplicate a live descriptor, retaining CLOEXEC in the parent.
    let copy = unsafe { libc::fcntl(descriptor.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 6) };
    if copy == -1 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: fcntl returned a new descriptor owned only here.
    Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(copy) })
}

/// A socketpair ready for a supervised Unix spawn. Creation and Darwin's
/// CLOEXEC setup hold the same lock as every daemon fork.
#[cfg(unix)]
#[allow(unsafe_code)]
pub fn registration_socketpair(
) -> std::io::Result<(std::os::unix::net::UnixStream, std::os::fd::OwnedFd, String)> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    let _guard = crate::child_spawn::creation_lock();
    let mut pair = [-1; 2];
    #[cfg(target_os = "linux")]
    let kind = libc::SOCK_STREAM | libc::SOCK_CLOEXEC;
    #[cfg(not(target_os = "linux"))]
    let kind = libc::SOCK_STREAM;
    // SAFETY: socketpair writes exactly two descriptors into the local array.
    if unsafe { libc::socketpair(libc::AF_UNIX, kind, 0, pair.as_mut_ptr()) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: both newly created descriptors are uniquely owned here.
    let (parent, child) = unsafe { (OwnedFd::from_raw_fd(pair[0]), OwnedFd::from_raw_fd(pair[1])) };
    #[cfg(not(target_os = "linux"))]
    for fd in [&parent, &child] {
        // SAFETY: creation and flag setup are serialized against every fork.
        if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
            return Err(std::io::Error::last_os_error());
        }
    }
    let parent = outside_child_slots(parent)?;
    let child = outside_child_slots(child)?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: fstat initializes the local buffer on success.
    if unsafe { libc::fstat(child.as_raw_fd(), stat.as_mut_ptr()) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    let inode = unsafe { stat.assume_init() }.st_ino;
    Ok((parent.into(), child, format!("4:{inode}")))
}

/// Why a named channel was not adopted. No variant contains a path or a secret.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RegistrationRefusal {
    Malformed,
    NotOpen,
    NotSocket,
    WrongInode,
    NamedSocket,
    WrongPeer,
    Unsupported,
}

impl fmt::Display for RegistrationRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Malformed => "invalid registration metadata",
            Self::NotOpen => "registration channel is not open",
            Self::NotSocket => "registration descriptor is not a socket",
            Self::WrongInode => "registration socket identity does not match",
            Self::NamedSocket => "registration socket or its peer is named",
            Self::WrongPeer => "registration peer is not the expected process",
            Self::Unsupported => "registration channel is unsupported on this platform",
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RegistrationChannelError {
    AlreadyTaken,
    Adoption { errno: Option<i32> },
}

impl fmt::Display for RegistrationChannelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyTaken => f.write_str("registration channel already taken"),
            Self::Adoption { .. } => f.write_str("could not adopt validated registration channel"),
        }
    }
}
impl std::error::Error for RegistrationChannelError {}

/// Uniquely owned transport with no cloning API.
pub struct RegistrationChannel {
    #[cfg(unix)]
    stream: std::os::unix::net::UnixStream,
    #[cfg(windows)]
    file: std::fs::File,
}

impl fmt::Debug for RegistrationChannel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RegistrationChannel(<redacted>)")
    }
}

impl RegistrationChannel {
    #[cfg(unix)]
    pub fn into_unix_stream(self) -> std::os::unix::net::UnixStream {
        self.stream
    }

    /// The file owns an overlapped, non-inheritable duplex pipe handle. Transfer
    /// it to an overlapped transport (for example Tokio), not synchronous I/O.
    #[cfg(windows)]
    pub fn into_file(self) -> std::fs::File {
        self.file
    }
}

struct Decision {
    supervised: bool,
    refusal: Option<RegistrationRefusal>,
    channel: Mutex<Result<Option<RegistrationChannel>, RegistrationChannelError>>,
}

impl Decision {
    fn refused(refusal: Option<RegistrationRefusal>) -> Self {
        Self {
            supervised: false,
            refusal,
            channel: Mutex::new(Ok(None)),
        }
    }

    fn validated(channel: Result<RegistrationChannel, RegistrationChannelError>) -> Self {
        Self {
            supervised: true,
            refusal: None,
            channel: Mutex::new(channel.map(Some)),
        }
    }

    fn take(&self) -> Result<Option<RegistrationChannel>, RegistrationChannelError> {
        if !self.supervised {
            return Ok(None);
        }
        let mut channel = self.channel.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::replace(&mut *channel, Err(RegistrationChannelError::AlreadyTaken))
    }
}

struct ChannelCell(OnceLock<Decision>);
impl ChannelCell {
    const fn new() -> Self {
        Self(OnceLock::new())
    }
    fn get(&self, lookup: impl FnMut(&str) -> Option<OsString>) -> &Decision {
        self.0.get_or_init(|| decide(lookup))
    }
}
static PROCESS_CHANNEL: ChannelCell = ChannelCell::new();

fn process_decision() -> &'static Decision {
    PROCESS_CHANNEL.get(|key| std::env::var_os(key))
}

/// Whether this process validated its registration channel, even after a take.
pub fn supervised() -> bool {
    process_decision().supervised
}

/// Take the one registered connection. Every error is fatal to an SDK caller;
/// only `Ok(None)` permits an unsupervised connection.
pub fn take_registration_channel() -> Result<Option<RegistrationChannel>, RegistrationChannelError>
{
    process_decision().take()
}

/// A safe-to-log explanation for a refused handoff, cached with the decision.
pub fn registration_refusal() -> Option<RegistrationRefusal> {
    process_decision().refusal
}

fn pid(value: Option<OsString>) -> Result<u32, RegistrationRefusal> {
    value
        .and_then(|v| v.into_string().ok())
        .and_then(|v| v.parse().ok())
        .filter(|pid| *pid > 0)
        .ok_or(RegistrationRefusal::Malformed)
}

fn decide(mut lookup: impl FnMut(&str) -> Option<OsString>) -> Decision {
    #[cfg(unix)]
    let value = lookup(REGISTRATION_FD_ENV);
    #[cfg(windows)]
    let value = lookup(REGISTRATION_PIPE_ENV);
    #[cfg(not(any(unix, windows)))]
    let value = lookup(REGISTRATION_FD_ENV).or_else(|| lookup(REGISTRATION_PIPE_ENV));
    let Some(value) = value else {
        return Decision::refused(None);
    };
    let daemon = match pid(lookup(DAEMON_PID_ENV)) {
        Ok(pid) => pid,
        Err(reason) => return Decision::refused(Some(reason)),
    };
    #[cfg(unix)]
    return unix::adopt(&value, daemon);
    #[cfg(windows)]
    return windows::adopt(&value, daemon);
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (value, daemon);
        Decision::refused(Some(RegistrationRefusal::Unsupported))
    }
}

#[cfg(unix)]
#[allow(unsafe_code)]
mod unix {
    use super::*;
    use std::{
        ffi::OsStr,
        io,
        mem::{size_of, zeroed},
        os::fd::{FromRawFd, RawFd},
    };

    fn stat(fd: RawFd) -> io::Result<libc::stat> {
        // SAFETY: fstat initializes the local output on success.
        let mut stat = unsafe { zeroed() };
        if unsafe { libc::fstat(fd, &mut stat) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(stat)
    }

    fn unnamed(fd: RawFd, peer: bool) -> bool {
        // SAFETY: both socket-name calls receive a sockaddr_un-sized buffer.
        let mut addr: libc::sockaddr_un = unsafe { zeroed() };
        let mut len = size_of::<libc::sockaddr_un>() as libc::socklen_t;
        let result = unsafe {
            if peer {
                libc::getpeername(fd, (&mut addr as *mut libc::sockaddr_un).cast(), &mut len)
            } else {
                libc::getsockname(fd, (&mut addr as *mut libc::sockaddr_un).cast(), &mut len)
            }
        };
        // Abstract Unix addresses start with NUL too. Only the absence of a
        // path, not its first byte, distinguishes an unnamed socket on Linux.
        #[cfg(target_os = "macos")]
        let no_path = {
            // Darwin returns a padded sockaddr for unnamed socketpairs. It
            // has no abstract namespace, so an all-zero path is still unnamed.
            #[cfg(test)]
            eprintln!(
                "Darwin socket name: peer={peer} len={len} family={} zero_path={}",
                addr.sun_family,
                addr.sun_path.iter().all(|byte| *byte == 0)
            );
            addr.sun_path.iter().all(|byte| *byte == 0)
        };
        #[cfg(not(target_os = "macos"))]
        let no_path = len as usize <= std::mem::offset_of!(libc::sockaddr_un, sun_path);
        result == 0 && addr.sun_family as i32 == libc::AF_UNIX && no_path
    }

    pub(super) fn peer_pid(fd: RawFd) -> Option<u32> {
        #[cfg(target_os = "linux")]
        {
            // SAFETY: SO_PEERCRED fills a ucred of the declared size.
            let mut cred: libc::ucred = unsafe { zeroed() };
            let mut len = size_of::<libc::ucred>() as libc::socklen_t;
            let ok = unsafe {
                libc::getsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_PEERCRED,
                    (&mut cred as *mut libc::ucred).cast(),
                    &mut len,
                )
            } == 0;
            (ok && len as usize == size_of::<libc::ucred>() && cred.pid > 0)
                .then_some(cred.pid as u32)
        }
        #[cfg(target_os = "macos")]
        {
            let mut pid: libc::pid_t = 0;
            let mut len = size_of::<libc::pid_t>() as libc::socklen_t;
            // SAFETY: LOCAL_PEERPID fills a pid_t of the declared size.
            let ok = unsafe {
                libc::getsockopt(
                    fd,
                    libc::SOL_LOCAL,
                    libc::LOCAL_PEERPID,
                    (&mut pid as *mut libc::pid_t).cast(),
                    &mut len,
                )
            } == 0;
            (ok && len as usize == size_of::<libc::pid_t>() && pid > 0).then_some(pid as u32)
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = fd;
            None
        }
    }

    pub(super) fn adopt(value: &OsStr, daemon: u32) -> Decision {
        adopt_with_stat(value, daemon, stat)
    }

    pub(super) fn adopt_with_stat(
        value: &OsStr,
        daemon: u32,
        stat: impl Fn(RawFd) -> io::Result<libc::stat>,
    ) -> Decision {
        let validate = || {
            let (fd, inode) = value
                .to_str()
                .and_then(|v| v.split_once(':'))
                .and_then(|(fd, inode)| {
                    Some((fd.parse::<RawFd>().ok()?, inode.parse::<u64>().ok()?))
                })
                .filter(|(fd, _)| *fd >= 0)
                .ok_or(RegistrationRefusal::Malformed)?;
            let stat = stat(fd).map_err(|_| RegistrationRefusal::NotOpen)?;
            if stat.st_mode & libc::S_IFMT != libc::S_IFSOCK {
                return Err(RegistrationRefusal::NotSocket);
            }
            // Darwin commonly reports zero for socket inodes. A nonzero inode
            // remains binding; zero falls back to socket names and peer PID.
            // ino_t is narrower than u64 on some Unix targets.
            #[allow(clippy::unnecessary_cast)]
            let actual_inode = stat.st_ino as u64;
            if !inode_matches(actual_inode, inode) {
                return Err(RegistrationRefusal::WrongInode);
            }
            if !unnamed(fd, false) || !unnamed(fd, true) {
                return Err(RegistrationRefusal::NamedSocket);
            }
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            return Err(RegistrationRefusal::Unsupported);
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            if peer_pid(fd) != Some(daemon) {
                return Err(RegistrationRefusal::WrongPeer);
            }
            Ok(fd)
        };
        let fd = match validate() {
            Ok(fd) => fd,
            Err(reason) => return Decision::refused(Some(reason)),
        };
        // SAFETY: all checks passed. This process owns the inherited descriptor;
        // the OnceLock prevents any second adoption or independent reader.
        let stream = unsafe { std::os::unix::net::UnixStream::from_raw_fd(fd) };
        // Preserve existing flags, but ensure no subsequent exec inherits it.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        let channel = if flags == -1
            || unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } == -1
        {
            Err(RegistrationChannelError::Adoption {
                errno: io::Error::last_os_error().raw_os_error(),
            })
        } else {
            Ok(RegistrationChannel { stream })
        };
        Decision::validated(channel)
    }

    pub(super) fn inode_matches(actual: u64, expected: u64) -> bool {
        (cfg!(target_os = "macos") && actual == 0) || actual == expected
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod windows {
    use super::*;
    use std::{
        ffi::OsStr,
        os::windows::{
            ffi::OsStrExt,
            io::{AsRawHandle, FromRawHandle, OwnedHandle},
        },
        ptr::{null, null_mut},
        time::{Duration, Instant},
    };
    use windows_sys::Win32::{
        Foundation::{ERROR_PIPE_BUSY, GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE},
        Storage::FileSystem::{
            CreateFileW, FILE_FLAG_OVERLAPPED, OPEN_EXISTING, SECURITY_IDENTIFICATION,
            SECURITY_SQOS_PRESENT,
        },
        System::Pipes::GetNamedPipeServerProcessId,
    };

    // A busy instance may still be finishing its previous connect attempt. Do
    // not turn a refused or unavailable channel into an unbounded startup wait.
    const OPEN_WINDOW: Duration = Duration::from_millis(500);

    pub(super) fn adopt(name: &OsStr, daemon: u32) -> Decision {
        let name: Vec<u16> = name.encode_wide().chain(Some(0)).collect();
        if name.len() <= 1 || name[..name.len() - 1].contains(&0) {
            return Decision::refused(Some(RegistrationRefusal::Malformed));
        }
        let deadline = Instant::now() + OPEN_WINDOW;
        let pipe = loop {
            // SAFETY: valid NUL-terminated name; null security attributes make
            // the handle non-inheritable. Identification prevents impersonation.
            let handle = unsafe {
                CreateFileW(
                    name.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    0,
                    null(),
                    OPEN_EXISTING,
                    FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                    null_mut(),
                )
            };
            if handle != INVALID_HANDLE_VALUE && !handle.is_null() {
                // SAFETY: CreateFileW returned a fresh owned handle.
                break unsafe { OwnedHandle::from_raw_handle(handle) };
            }
            if std::io::Error::last_os_error().raw_os_error() != Some(ERROR_PIPE_BUSY as i32)
                || Instant::now() >= deadline
            {
                return Decision::refused(Some(RegistrationRefusal::NotOpen));
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        let mut server = 0;
        // SAFETY: live pipe and writable scalar output. No bytes have been sent.
        if unsafe { GetNamedPipeServerProcessId(pipe.as_raw_handle(), &mut server) } == 0
            || server != daemon
        {
            return Decision::refused(Some(RegistrationRefusal::WrongPeer));
        }
        Decision::validated(Ok(RegistrationChannel {
            file: std::fs::File::from(pipe),
        }))
    }
}

/// One single-use claim. Debug and errors never expose its bytes.
pub struct HelperClaim(zeroize::Zeroizing<String>);
impl HelperClaim {
    pub fn value(&self) -> &str {
        &self.0
    }
}
impl fmt::Debug for HelperClaim {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("HelperClaim(<redacted>)")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum HelperClaimError {
    InvalidHandoff,
    AlreadyTaken,
}
impl fmt::Display for HelperClaimError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidHandoff => "invalid helper claim handoff",
            Self::AlreadyTaken => "helper claim already taken",
        })
    }
}
impl std::error::Error for HelperClaimError {}

type HelperResult = Result<Option<HelperClaim>, HelperClaimError>;
static HELPER_CLAIM: OnceLock<Mutex<HelperResult>> = OnceLock::new();

/// Take an explicit helper claim once. Reconnecting requires a fresh claim
/// from the owner, not another read of the same pipe. No nonce fallback exists.
pub fn helper_claim() -> HelperResult {
    let mut result = HELPER_CLAIM
        .get_or_init(|| Mutex::new(read_helper_claim(|key| std::env::var_os(key))))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if matches!(&*result, Ok(None)) {
        return Ok(None);
    }
    std::mem::replace(&mut *result, Err(HelperClaimError::AlreadyTaken))
}

fn read_helper_claim(mut lookup: impl FnMut(&str) -> Option<OsString>) -> HelperResult {
    #[cfg(unix)]
    let read = {
        let Some(value) = lookup(HELPER_CLAIM_FD_ENV) else {
            return Ok(None);
        };
        helper_unix::read(&value)
    };
    #[cfg(windows)]
    let read = {
        let Some(value) = lookup(HELPER_CLAIM_PIPE_ENV) else {
            return Ok(None);
        };
        let parent =
            pid(lookup(HELPER_PARENT_PID_ENV)).map_err(|_| HelperClaimError::InvalidHandoff)?;
        crate::launch_nonce::read_helper_pipe(&value, parent)
    };
    #[cfg(any(unix, windows))]
    {
        let value = read
            .map_err(|_| HelperClaimError::InvalidHandoff)?
            .ok_or(HelperClaimError::InvalidHandoff)?;
        Ok(Some(HelperClaim(zeroize::Zeroizing::new(value))))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = &mut lookup;
        Err(HelperClaimError::InvalidHandoff)
    }
}

pub(crate) fn check_claim(claim: &str) -> std::io::Result<()> {
    // POSIX guarantees at least 512 bytes in an empty pipe. Bound the claim
    // before writing, since the helper has not been spawned to drain it yet.
    if claim.is_empty() || claim.len() > 512 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid helper claim length",
        ));
    }
    Ok(())
}

#[cfg(unix)]
#[derive(Debug)]
pub struct HelperClaimHandoff(PreparedHelperClaimHandoff);
#[cfg(unix)]
impl HelperClaimHandoff {
    pub fn new(claim: &str) -> std::io::Result<Self> {
        check_claim(claim)?;
        let mut prepared = PreparedHelperClaimHandoff::new(3)?;
        prepared.write_claim(claim)?;
        Ok(Self(prepared))
    }

    /// Install last, after all other pre-exec hooks. Only descriptor metadata
    /// is placed in the environment; stdin and the other stdio are untouched.
    pub fn install_last(self, command: &mut std::process::Command) {
        self.0.install_last(command);
    }
}

#[cfg(windows)]
pub use crate::launch_nonce::{HelperClaimHandoff, HELPER_CLAIM_CONNECT_WINDOW};

#[cfg(unix)]
mod helper_unix;
#[cfg(unix)]
pub use helper_unix::PreparedHelperClaimHandoff;

/// Reserved single-instance helper pipe. Preparation contains no claim bytes;
/// completion checks the live child's PID before delivering exactly one claim.
#[cfg(windows)]
pub struct PreparedHelperClaimHandoff(crate::launch_nonce::PreparedHelperPipe);
#[cfg(windows)]
impl fmt::Debug for PreparedHelperClaimHandoff {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PreparedHelperClaimHandoff(<redacted>)")
    }
}
#[cfg(windows)]
impl PreparedHelperClaimHandoff {
    #[cfg(test)]
    pub(crate) fn name(&self) -> &str {
        self.0.name()
    }
    pub fn new(_child_fd: i32) -> std::io::Result<Self> {
        Ok(Self(crate::launch_nonce::PreparedHelperPipe::reserve()?))
    }
    pub fn environment(&self) -> Vec<(OsString, OsString)> {
        vec![
            (HELPER_CLAIM_PIPE_ENV.into(), self.0.name().into()),
            (
                HELPER_PARENT_PID_ENV.into(),
                std::process::id().to_string().into(),
            ),
        ]
    }
    pub fn child_fd(&self) -> Option<i32> {
        None
    }
    /// Retain the child until completion returns to prevent PID reuse.
    pub fn complete(self, child_pid: u32, claim: &str) -> std::io::Result<()> {
        check_claim(claim)?;
        self.0.deliver_helper(child_pid, claim)
    }
    pub fn abort(self) {}
}

#[cfg(all(test, unix))]
#[allow(unsafe_code)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        os::fd::{AsRawFd, IntoRawFd},
        os::unix::net::{UnixListener, UnixStream},
        process::{Command, Stdio},
    };

    const CHANNEL_TEST: &str = "registration_channel::tests::registration_channel_contract";
    const HELPER_TEST: &str = "registration_channel::tests::helper_claim_handoff_contract";
    const MODE: &str = "SUBC_OS_REGISTRATION_TEST_MODE";
    const CLAIM: &str = "test-helper-claim-not-for-logs";

    #[cfg(target_os = "macos")]
    #[test]
    fn darwin_zero_inode_retains_socket_names_and_peer_pid_checks() {
        fn no_inode(fd: i32) -> std::io::Result<libc::stat> {
            let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
            // SAFETY: the writable output is read only after successful fstat.
            if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } == -1 {
                return Err(std::io::Error::last_os_error());
            }
            let mut stat = unsafe { stat.assume_init() };
            stat.st_ino = 0;
            Ok(stat)
        }
        let (stream, _peer) = UnixStream::pair().unwrap();
        let fd = stream.into_raw_fd();
        let decision =
            unix::adopt_with_stat(format!("{fd}:999").as_ref(), std::process::id(), no_inode);
        assert!(
            decision.supervised,
            "zero inode socketpair refused: {:?}",
            decision.refusal
        );
        drop(decision.take().unwrap());
        let (stream, _peer) = UnixStream::pair().unwrap();
        let value = format!("{}:999", stream.as_raw_fd());
        assert_eq!(
            unix::adopt_with_stat(value.as_ref(), std::process::id() + 1, no_inode).refusal,
            Some(RegistrationRefusal::WrongPeer)
        );
        let file = std::fs::File::open("/dev/null").unwrap();
        let value = format!("{}:999", file.as_raw_fd());
        assert_eq!(
            unix::adopt_with_stat(value.as_ref(), std::process::id(), no_inode).refusal,
            Some(RegistrationRefusal::NotSocket)
        );
        // A relative address stays below Darwin's sockaddr_un path limit even
        // when the checkout or the test runner's scratch root has a long name.
        let path =
            std::path::PathBuf::from(format!("target/scratch/zero-inode-{}", std::process::id()));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let listener = UnixListener::bind(&path).unwrap();
        let stream = UnixStream::connect(&path).unwrap();
        let (_peer, _) = listener.accept().unwrap();
        let value = format!("{}:999", stream.as_raw_fd());
        assert_eq!(
            unix::adopt_with_stat(value.as_ref(), std::process::id(), no_inode).refusal,
            Some(RegistrationRefusal::NamedSocket)
        );
        drop(listener);
        std::fs::remove_file(path).unwrap();
        eprintln!("Darwin zero-inode fallback: socketpair adopted; non-socket, named peer and wrong peer PID refused");
    }

    #[test]
    fn unix_channel_parent_prepares_cloexec_sources() {
        let (parent, child, metadata) = registration_socketpair().unwrap();
        for fd in [parent.as_raw_fd(), child.as_raw_fd()] {
            assert!(fd >= 6, "socket source collided with a reserved slot");
            // SAFETY: inspect flags on these owned descriptors.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
            assert_ne!(flags & libc::FD_CLOEXEC, 0);
        }
        let actual = inode(child.as_raw_fd());
        assert_eq!(metadata, format!("4:{actual}"));
        let peer = unix::peer_pid(child.as_raw_fd());
        eprintln!(
            "socketpair: peer_pid={peer:?} inode={actual} usable_inode={} both_cloexec=true",
            actual != 0
        );
        assert_eq!(peer, Some(std::process::id()));
        assert!(unix::inode_matches(actual, actual));
        assert!(!unix::inode_matches(12, 13));
        #[cfg(target_os = "macos")]
        assert!(
            unix::inode_matches(0, 13),
            "zero inode must retain the other three validators"
        );
    }

    fn child(test: &str, mode: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([test, "--exact", "--nocapture"])
            .env(MODE, mode)
            .env_remove(HELPER_CLAIM_FD_ENV)
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

    // ino_t's width depends on the Unix target.
    #[allow(clippy::unnecessary_cast)]
    fn inode(fd: i32) -> u64 {
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: a writable stat buffer, read only after successful fstat.
        assert_eq!(unsafe { libc::fstat(fd, stat.as_mut_ptr()) }, 0);
        unsafe { stat.assume_init() }.st_ino as u64
    }

    fn lookup(value: String, daemon: u32) -> impl FnMut(&str) -> Option<OsString> {
        move |key| match key {
            REGISTRATION_FD_ENV => Some(value.clone().into()),
            DAEMON_PID_ENV => Some(daemon.to_string().into()),
            _ => None,
        }
    }

    fn refuse(value: String, daemon: u32, reason: RegistrationRefusal) {
        let cell = ChannelCell::new();
        let decision = cell.get(lookup(value, daemon));
        assert!(!decision.supervised);
        assert_eq!(decision.refusal, Some(reason));
        assert!(decision.take().unwrap().is_none());
        assert!(cell
            .get(|_| panic!("cached refusal reread environment"))
            .take()
            .unwrap()
            .is_none());
    }

    #[test]
    fn registration_channel_contract() {
        if std::env::var(MODE).as_deref() == Ok("inherited-channel") {
            assert!(supervised(), "inherited socketpair failed validation");
            let mut stream = take_registration_channel()
                .unwrap()
                .unwrap()
                .into_unix_stream();
            assert!(matches!(
                take_registration_channel(),
                Err(RegistrationChannelError::AlreadyTaken)
            ));
            let mut byte = [0];
            stream.read_exact(&mut byte).unwrap();
            assert_eq!(&byte, b"x");
            stream.write_all(b"x").unwrap();
            success(child(CHANNEL_TEST, "missing-channel").output().unwrap());
            return;
        }
        if std::env::var(MODE).as_deref() == Ok("missing-channel") {
            assert!(std::env::var_os(REGISTRATION_FD_ENV).is_some());
            assert!(
                !supervised(),
                "a variable without its descriptor is not supervision"
            );
            assert!(take_registration_channel().unwrap().is_none());
            assert!(registration_refusal().is_some());
            return;
        }

        let (stream, mut peer) = UnixStream::pair().unwrap();
        let fd = stream.into_raw_fd();
        let identity = inode(fd);
        // SAFETY: fcntl on the test's owned socket. Simulate the inheritable
        // descriptor delivered by the daemon before exercising adoption.
        let original_flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert_ne!(original_flags & libc::FD_CLOEXEC, 0);
        assert_eq!(unsafe { libc::fcntl(fd, libc::F_SETFD, 0) }, 0);
        let cell = ChannelCell::new();
        let metadata = format!("{fd}:{identity}");
        let decision = cell.get(lookup(metadata.clone(), std::process::id()));
        assert!(
            decision.supervised,
            "socketpair validation: {:?}",
            decision.refusal
        );
        assert_eq!(decision.refusal, None);
        assert_ne!(
            unsafe { libc::fcntl(fd, libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
        eprintln!(
            "socketpair: pid={} inode={identity} initial_cloexec={} adopted_cloexec=true",
            std::process::id(),
            original_flags & libc::FD_CLOEXEC != 0
        );
        let mut stream = decision.take().unwrap().unwrap().into_unix_stream();
        assert!(matches!(
            decision.take(),
            Err(RegistrationChannelError::AlreadyTaken)
        ));
        assert!(
            cell.get(|_| panic!("cached adoption reread environment"))
                .supervised
        );
        peer.write_all(b"x").unwrap();
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        assert_eq!(&byte, b"x");
        success(
            child(CHANNEL_TEST, "missing-channel")
                .env(REGISTRATION_FD_ENV, metadata)
                .env(DAEMON_PID_ENV, std::process::id().to_string())
                .output()
                .unwrap(),
        );

        // Exercise the actual exec boundary, including peer credentials on a
        // socketpair made by the parent and the child's process-wide accessors.
        let (inherited, mut peer) = UnixStream::pair().unwrap();
        let source = inherited.as_raw_fd();
        let mut command = child(CHANNEL_TEST, "inherited-channel");
        command
            .env(REGISTRATION_FD_ENV, format!("4:{}", inode(source)))
            .env(DAEMON_PID_ENV, std::process::id().to_string());
        use std::os::unix::process::CommandExt;
        // SAFETY: only async-signal-safe, allocation-free descriptor placement
        // runs after fork; the source stays owned until spawn has returned.
        unsafe {
            command.pre_exec(move || {
                if libc::dup2(source, 4) == -1 || libc::fcntl(4, libc::F_SETFD, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let process = command.spawn().unwrap();
        drop(inherited);
        peer.set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        peer.write_all(b"x").unwrap();
        peer.read_exact(&mut byte).unwrap();
        assert_eq!(&byte, b"x");
        success(process.wait_with_output().unwrap());

        // OnceLock serializes first callers and the transport has only one owner.
        let (other, _peer) = UnixStream::pair().unwrap();
        let fd = other.into_raw_fd();
        let metadata = format!("{fd}:{}", inode(fd));
        let cell = ChannelCell::new();
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    let cell = &cell;
                    let metadata = metadata.clone();
                    scope.spawn(move || {
                        cell.get(lookup(metadata, std::process::id()))
                            .take()
                            .is_ok()
                    })
                })
                .collect();
            assert_eq!(
                handles
                    .into_iter()
                    .map(|h| h.join().unwrap() as usize)
                    .sum::<usize>(),
                1
            );
        });

        let file = std::fs::File::open("/dev/null").unwrap();
        refuse(
            format!("{}:{}", file.as_raw_fd(), inode(file.as_raw_fd())),
            std::process::id(),
            RegistrationRefusal::NotSocket,
        );
        assert!(
            file.metadata().is_ok(),
            "refusal must not close an unrelated file"
        );
        for malformed in ["", "3", "-1:0", "x:1", "3:x"] {
            refuse(
                malformed.into(),
                std::process::id(),
                RegistrationRefusal::Malformed,
            );
        }
        let (socket, _peer) = UnixStream::pair().unwrap();
        let fd = socket.as_raw_fd();
        let identity = inode(fd);
        if !cfg!(target_os = "macos") || identity != 0 {
            refuse(
                format!("{fd}:{}", identity.wrapping_add(1)),
                std::process::id(),
                RegistrationRefusal::WrongInode,
            );
        } else {
            eprintln!(
                "Darwin socket inode unavailable; exercising PID and unnamed-socket fallback"
            );
            let fd = socket.try_clone().unwrap().into_raw_fd();
            assert!(ChannelCell::new()
                .get(lookup(format!("{fd}:999"), std::process::id()))
                .take()
                .unwrap()
                .is_some());
        }
        refuse(
            format!("{fd}:{identity}"),
            std::process::id().wrapping_add(1),
            RegistrationRefusal::WrongPeer,
        );
        assert!(
            socket.peer_addr().is_ok(),
            "wrong-peer refusal leaves descriptor open"
        );

        let path = std::path::PathBuf::from(format!(
            "target/scratch/registration-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let listener = UnixListener::bind(&path).unwrap();
        let connected = UnixStream::connect(&path).unwrap();
        let (_accepted, _) = listener.accept().unwrap();
        let fd = connected.as_raw_fd();
        refuse(
            format!("{fd}:{}", inode(fd)),
            std::process::id(),
            RegistrationRefusal::NamedSocket,
        );
        drop(listener);
        std::fs::remove_file(path).unwrap();

        let cell = ChannelCell::new();
        assert!(!cell.get(|_| None).supervised);
        assert!(
            !cell
                .get(|_| panic!("absent decision reread environment"))
                .supervised
        );
    }

    #[test]
    fn helper_claim_handoff_contract() {
        match std::env::var(MODE).as_deref() {
            Ok("helper") => {
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
                let metadata = std::env::var_os(HELPER_CLAIM_FD_ENV).unwrap();
                success(
                    child(HELPER_TEST, "helper-descendant")
                        .env(HELPER_CLAIM_FD_ENV, metadata)
                        .output()
                        .unwrap(),
                );
                return;
            }
            Ok("helper-descendant") => {
                assert!(
                    helper_claim().is_err(),
                    "a consumed pipe cannot hand the claim to descendants"
                );
                return;
            }
            _ => {}
        }
        let handoff = HelperClaimHandoff::new(CLAIM).unwrap();
        assert!(!format!("{handoff:?}").contains(CLAIM));
        let mut command = child(HELPER_TEST, "helper");
        command.stdin(Stdio::piped());
        handoff.install_last(&mut command);
        assert!(!format!("{command:?}").contains(CLAIM));
        let mut process = command.spawn().unwrap();
        process
            .stdin
            .take()
            .unwrap()
            .write_all(b"stdio-still-available")
            .unwrap();
        success(process.wait_with_output().unwrap());
        assert!(HelperClaimHandoff::new("").is_err());
        assert!(HelperClaimHandoff::new(&"x".repeat(513)).is_err());
    }
}

#[cfg(test)]
#[allow(unsafe_code)]
mod prepared_helper_tests {
    use super::*;
    use std::process::{Command, Stdio};
    const MODE: &str = "SUBC_PREPARED_HELPER_TEST";
    #[test]
    fn prepared_helper_split_delivery_and_abort() {
        if let Ok(mode) = std::env::var(MODE) {
            let value = helper_claim();
            if mode == "read" {
                assert!(value.unwrap().unwrap().value() == "test-single-use-claim");
            } else {
                assert!(value.is_err(), "aborted handoff must deliver no bytes");
            }
            return;
        }
        #[cfg(unix)]
        let modes = ["read", "abort"].as_slice();
        #[cfg(windows)]
        let modes = ["read", "abort", "refuse"].as_slice();
        for mode in modes.iter().copied() {
            let prepared = PreparedHelperClaimHandoff::new(3).unwrap();
            let mut command = Command::new(std::env::current_exe().unwrap());
            command.args(["--exact", "registration_channel::prepared_helper_tests::prepared_helper_split_delivery_and_abort", "--nocapture"])
                .env(MODE, mode).envs(prepared.environment()).stdout(Stdio::piped()).stderr(Stdio::piped());
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt;
                let fd = prepared.child_fd().unwrap();
                assert_ne!(
                    unsafe { libc::fcntl(fd, libc::F_GETFD) } & libc::FD_CLOEXEC,
                    0
                );
                unsafe {
                    command.pre_exec(move || {
                        if libc::dup2(fd, 3) == -1 || libc::fcntl(3, libc::F_SETFD, 0) == -1 {
                            return Err(std::io::Error::last_os_error());
                        }
                        Ok(())
                    });
                }
            }
            let child = command.spawn().unwrap();
            if mode == "read" {
                prepared
                    .complete(child.id(), "test-single-use-claim")
                    .unwrap();
            } else if mode == "refuse" {
                assert!(
                    prepared
                        .complete(std::process::id(), "test-single-use-claim")
                        .is_err(),
                    "foreign helper must receive no bytes"
                );
            } else {
                prepared.abort();
            }
            let output = child.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}
