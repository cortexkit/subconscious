//! Who is at the other end of an accepted local-endpoint connection: a
//! Unix-domain socket on macOS and Linux, a named-pipe client on Windows.
//!
//! Unlike [`crate::socket_peer`], which searches process tables for the holder
//! of a loopback TCP socket, every value here comes from a kernel primitive
//! tied to the connection itself and read when it is accepted:
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

/// [`observe_pipe_client`] with read failures forced, for tests.
#[cfg(all(windows, feature = "test-support"))]
pub fn observe_pipe_client_forcing(
    pipe: &impl std::os::windows::io::AsHandle,
    forced: ForcedFailures,
) -> ObservedPeer {
    windows::observe(pipe.as_handle(), forced)
}
