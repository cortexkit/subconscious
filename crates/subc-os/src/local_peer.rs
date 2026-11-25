//! Who is at the other end of an accepted local-endpoint connection: a
//! Unix-domain socket on macOS and Linux, a named-pipe client on Windows.
//!
//! Unlike [`crate::socket_peer`], which searches process tables for the holder
//! of a loopback TCP socket, peer identity comes from kernel primitives tied
//! to the accepted connection. Optional code metadata is derived from that
//! captured identity:
//!
//! - macOS: the `LOCAL_PEERTOKEN` audit token. `uid` is the token's effective
//!   uid. The guest code for the token (`SecCodeCopyGuestWithAttributes`)
//!   gives `exe`, its path, and `signing_id`, its signing identifier, which is
//!   reported only when `SecCodeCheckValidity` with no requirement passes.
//! - Linux: `SO_PEERPIDFD` (kernel 6.5 and later) and the `SO_PEERCRED` uid
//!   and pid. `exe` is read from `/proc/<pid>/exe` while the pidfd is held, and
//!   kept only if the pidfd still reports the process alive after the read, so
//!   a pid reused after the peer exited cannot lend its executable. Without
//!   `SO_PEERPIDFD` the peer is [`ObservedPeer::Unobserved`]: the credentials
//!   alone cannot pin the process the pid names. There is no `signing_id`.
//! - Windows: `GetNamedPipeClientProcessId` on a local pipe client, then a
//!   handle on that pid opened with `PROCESS_QUERY_LIMITED_INFORMATION`: `sid`
//!   is the user SID of its token and `exe` comes from
//!   `QueryFullProcessImageNameW`. A client on another machine is never
//!   observed. There is no `signing_id`.
//!
//! If the user (uid or SID) cannot be read, the peer is unobserved. If only
//! `exe` or `signing_id` cannot be read, that value alone is absent.
//!
//! An observation describes the peer; it is not an authorization decision.

use std::path::PathBuf;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;

#[cfg(test)]
mod tests;

#[cfg(all(target_os = "macos", feature = "test-support"))]
pub mod fixture;

/// The result of observing the peer of an accepted local connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ObservedPeer {
    /// The platform lacks the primitive, or the peer's user could not be read.
    Unobserved,
    /// The kernel named the peer process and its user.
    Observed(PeerObservation),
}

/// What the kernel reported about an observed peer process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerObservation {
    /// The peer's user: a uid on macOS and Linux, a SID on Windows.
    pub user: PeerUser,
    /// The peer's process id as the kernel recorded it for this connection.
    pub pid: u32,
    /// The peer's executable path, absent when it could not be read.
    pub exe: Option<PathBuf>,
    /// The peer's code-signing identifier (macOS only), present only when its
    /// signature passed `SecCodeCheckValidity`.
    pub signing_id: Option<String>,
}

/// The user a peer runs as.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PeerUser {
    /// Effective uid (macOS, from the audit token) or uid (Linux, from
    /// `SO_PEERCRED`).
    Uid(u32),
    /// The user SID of the peer process's token, in `S-1-…` string form.
    Sid(String),
}

/// An accept-time observation that may carry a private audit token for macOS
/// code verification. Advisory fields alone cannot create that token.
#[derive(Clone)]
#[non_exhaustive]
pub struct CapturedPeer {
    observation: ObservedPeer,
    #[cfg(target_os = "macos")]
    token: Option<macos::AuditToken>,
}

impl std::fmt::Debug for CapturedPeer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CapturedPeer")
            .field("observation", &self.observation)
            .finish_non_exhaustive()
    }
}

impl From<ObservedPeer> for CapturedPeer {
    fn from(observation: ObservedPeer) -> Self {
        Self {
            observation,
            #[cfg(target_os = "macos")]
            token: None,
        }
    }
}

/// A signature check's closed result. Only `Satisfied` is affirmative;
/// missing primitives, invalid requirements and read errors fail closed.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CodeRequirementOutcome {
    /// The running code satisfies the requirement and uses hardened runtime.
    Satisfied(VerifiedCode),
    /// The code or its signature does not meet the policy.
    NotSatisfied,
    /// The platform could not establish the claim.
    Unverifiable(CodeVerificationError),
}

/// Metadata copied from the signature-validated code, never a reopened path.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct VerifiedCode {
    /// `CFBundleShortVersionString` from the validated signing information.
    pub short_version: Option<String>,
}

/// Why verification could not establish a claim.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CodeVerificationError {
    UnsupportedPlatform,
    MissingAuditToken,
    FrameworkUnavailable,
    InvalidRequirement,
    GuestUnavailable,
    SigningInformationUnavailable,
    ValidityCheckFailed(i32),
}

impl CapturedPeer {
    pub fn observation(&self) -> &ObservedPeer {
        &self.observation
    }

    /// Whether this capture can be submitted for macOS code verification.
    pub fn has_audit_token(&self) -> bool {
        #[cfg(target_os = "macos")]
        return self.token.is_some();
        #[cfg(not(target_os = "macos"))]
        false
    }

    /// Resolve the advisory path and signing identifier from the captured
    /// audit token. This may access disk or block in Security.framework; do
    /// not call it inline in a listener or on an async runtime worker.
    #[cfg(target_os = "macos")]
    pub fn observe_code(&self) -> ObservedPeer {
        macos::observe_code(self, ForcedFailures::default())
    }

    /// Evaluate a code-signing requirement against the captured peer token.
    /// Hardened runtime is also required. Repeating this check revalidates the
    /// same process incarnation; neither a pid nor a path is accepted as proof.
    pub fn check_code_requirement(&self, requirement: &str) -> CodeRequirementOutcome {
        #[cfg(target_os = "macos")]
        return macos::check_requirement(self.token, requirement);
        #[cfg(not(target_os = "macos"))]
        {
            let _ = requirement;
            CodeRequirementOutcome::Unverifiable(CodeVerificationError::UnsupportedPlatform)
        }
    }
}

/// Capture peer facts at accept and, on macOS, retain the private audit token
/// for later code verification.
#[cfg(unix)]
pub fn capture_unix_peer(socket: &impl std::os::fd::AsFd) -> CapturedPeer {
    #[cfg(target_os = "macos")]
    return macos::capture(socket.as_fd(), ForcedFailures::default());
    #[cfg(not(target_os = "macos"))]
    observe_unix_peer(socket).into()
}

/// Capture kernel peer facts without consulting macOS Security.framework.
/// On macOS only the audit-token uid and pid are observed at this point.
#[cfg(unix)]
pub fn capture_unix_peer_credentials(socket: &impl std::os::fd::AsFd) -> CapturedPeer {
    #[cfg(target_os = "macos")]
    return macos::capture_credentials(socket.as_fd(), ForcedFailures::default());
    #[cfg(not(target_os = "macos"))]
    capture_unix_peer(socket)
}

/// Capture the authorization peer and, separately, the kernel PID for route
/// diagnostics. The diagnostic PID alone must not be used to recognize an
/// operator app or make any trust decision.
#[cfg(unix)]
pub fn capture_unix_peer_with_diagnostic_pid(
    socket: &impl std::os::fd::AsFd,
) -> (CapturedPeer, Option<u32>) {
    #[cfg(target_os = "linux")]
    {
        let (peer, diagnostic_pid) = linux::observe_with_pid(
            socket.as_fd(),
            ForcedFailures::default(),
            linux::pidfd_alive,
        );
        (peer.into(), diagnostic_pid)
    }
    #[cfg(not(target_os = "linux"))]
    {
        use std::os::fd::AsRawFd;
        (
            capture_unix_peer_credentials(socket),
            unix_peer_pid(socket.as_fd().as_raw_fd()),
        )
    }
}

#[cfg(unix)]
pub(crate) fn unix_peer_pid(socket: std::os::fd::RawFd) -> Option<u32> {
    #[cfg(target_os = "linux")]
    return linux::peer_pid(socket);
    #[cfg(target_os = "macos")]
    return macos::peer_pid(socket);
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = socket;
        None
    }
}

/// Diagnostic identity for the process that opened a local endpoint. The
/// PID and name are advisory and must never be used for authorization.
#[derive(Debug, Clone)]
pub struct DiagnosticProcess {
    pid: u32,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    start: Option<u64>,
    #[cfg(windows)]
    process: Option<std::sync::Arc<std::os::windows::io::OwnedHandle>>,
}

impl DiagnosticProcess {
    /// Capture process identity at accept so a later name lookup can reject
    /// a process that reused the opener's PID.
    pub fn capture(pid: u32) -> Self {
        Self {
            pid,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            start: crate::start_time(pid),
            #[cfg(windows)]
            process: windows::open_diagnostic_process(pid).map(std::sync::Arc::new),
        }
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Read the current short name, if the opener is still alive and readable.
    pub fn short_name(&self) -> Option<String> {
        #[cfg(target_os = "linux")]
        let name = {
            let bytes = std::fs::read(format!("/proc/{}/comm", self.pid)).ok()?;
            String::from_utf8_lossy(bytes.strip_suffix(b"\n").unwrap_or(&bytes)).into_owned()
        };
        #[cfg(target_os = "macos")]
        let name = crate::process_tree::program_name(self.pid);

        #[cfg(any(target_os = "linux", target_os = "macos"))]
        return (!name.is_empty() && self.start? == crate::start_time(self.pid)?).then_some(name);
        #[cfg(windows)]
        return windows::process_name(self.process.as_deref()?);
        #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
        None
    }
}

/// Read failures a test can force, to show how each one shapes the result
/// without needing a platform that actually fails that read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ForcedFailures {
    /// The uid or SID read fails: the peer is unobserved.
    pub user: bool,
    /// The executable read fails: only `exe` is absent.
    pub exe: bool,
    /// macOS: `SecCodeCheckValidity` fails, as for a broken signature: only
    /// `signing_id` is absent.
    pub signing_id: bool,
    /// Linux: the kernel does not support `SO_PEERPIDFD`: the peer is
    /// unobserved.
    pub peer_pidfd: bool,
}

/// Observe the peer of an accepted Unix-domain stream socket. Call it when the
/// connection is accepted. On a Unix other than macOS and Linux the peer is
/// unobserved.
#[cfg(unix)]
pub fn observe_unix_peer(socket: &impl std::os::fd::AsFd) -> ObservedPeer {
    unix_peer(socket.as_fd(), ForcedFailures::default())
}

/// [`observe_unix_peer`] with read failures forced, for tests.
#[cfg(all(unix, feature = "test-support"))]
pub fn observe_unix_peer_forcing(
    socket: &impl std::os::fd::AsFd,
    forced: ForcedFailures,
) -> ObservedPeer {
    unix_peer(socket.as_fd(), forced)
}

#[cfg(unix)]
fn unix_peer(socket: std::os::fd::BorrowedFd<'_>, forced: ForcedFailures) -> ObservedPeer {
    #[cfg(target_os = "macos")]
    return macos::observe(socket, forced);
    #[cfg(target_os = "linux")]
    return linux::observe(socket, forced);
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (socket, forced);
        ObservedPeer::Unobserved
    }
}

/// Observe the client of a connected named-pipe server instance. Call it when
/// the client has connected. A client on another machine is unobserved.
#[cfg(windows)]
pub fn observe_pipe_client(pipe: &impl std::os::windows::io::AsHandle) -> ObservedPeer {
    windows::observe(pipe.as_handle(), ForcedFailures::default())
}

/// Refuse non-local clients before observing the peer of a connected pipe.
/// The returned diagnostic PID must not be used to recognize an operator app
/// or make any trust decision.
#[cfg(windows)]
pub fn capture_pipe_client_with_diagnostic_pid(
    pipe: &impl std::os::windows::io::AsRawHandle,
) -> std::io::Result<(CapturedPeer, u32)> {
    let pid = crate::named_pipe::local_client_pid(pipe.as_raw_handle())?;
    Ok((
        windows::observe_local_process(pid, ForcedFailures::default()).into(),
        pid,
    ))
}

/// [`observe_pipe_client`] with read failures forced, for tests.
#[cfg(all(windows, feature = "test-support"))]
pub fn observe_pipe_client_forcing(
    pipe: &impl std::os::windows::io::AsHandle,
    forced: ForcedFailures,
) -> ObservedPeer {
    windows::observe(pipe.as_handle(), forced)
}
