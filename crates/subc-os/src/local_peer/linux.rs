#![allow(unsafe_code)]

use super::{ForcedFailures, ObservedPeer, PeerObservation, PeerUser};
use std::{
    mem::size_of,
    os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd},
    path::PathBuf,
};

use rustix::event::{poll, PollFd, PollFlags, Timespec};

pub(super) fn observe(socket: BorrowedFd<'_>, forced: ForcedFailures) -> ObservedPeer {
    observe_checking(socket, forced, pidfd_alive)
}

/// [`observe`] with the after-read liveness check supplied, so a test can
/// report the peer as exited between the exe read and the check.
pub(super) fn observe_checking(
    socket: BorrowedFd<'_>,
    forced: ForcedFailures,
    alive: impl FnOnce(&OwnedFd) -> bool,
) -> ObservedPeer {
    observe_with_pid(socket, forced, alive).0
}

pub(super) fn observe_with_pid(
    socket: BorrowedFd<'_>,
    forced: ForcedFailures,
    alive: impl FnOnce(&OwnedFd) -> bool,
) -> (ObservedPeer, Option<u32>) {
    // Both values were recorded by the kernel when the peer connected. The
    // pidfd pins that process, so a later exe read can be checked against it.
    let Some(pidfd) = peer_pidfd(socket).filter(|_| !forced.peer_pidfd) else {
        return (ObservedPeer::Unobserved, peer_pid(socket.as_raw_fd()));
    };
    let Some(credentials) = peer_credentials(socket.as_raw_fd()).filter(|_| !forced.user) else {
        return (ObservedPeer::Unobserved, None);
    };
    // A negative pid never names a process.
    let Ok(pid) = u32::try_from(credentials.pid) else {
        return (ObservedPeer::Unobserved, None);
    };
    let exe = (!forced.exe)
        .then(|| executable(pid))
        .flatten()
        // Read first, then check: if the peer has exited, its pid may already
        // name another process, whose executable the read may have returned.
        .filter(|_| alive(&pidfd));
    (
        ObservedPeer::Observed(PeerObservation {
            user: PeerUser::Uid(credentials.uid),
            pid,
            exe,
            signing_id: None,
        }),
        (pid != 0).then_some(pid),
    )
}

pub(super) fn peer_pid(socket: RawFd) -> Option<u32> {
    u32::try_from(peer_credentials(socket)?.pid)
        .ok()
        .filter(|pid| *pid != 0)
}

/// The peer's pidfd, or `None` when the kernel lacks `SO_PEERPIDFD` (before
/// 6.5) or the read fails, for example because the peer has already gone.
fn peer_pidfd(socket: BorrowedFd<'_>) -> Option<OwnedFd> {
    let mut fd: libc::c_int = -1;
    let mut length = size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: fd and length are live, writable and sized for the int this
    // option returns; the socket descriptor is borrowed for the call.
    let status = unsafe {
        libc::getsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERPIDFD,
            (&mut fd as *mut libc::c_int).cast(),
            &mut length,
        )
    };
    if status != 0 || length as usize != size_of::<libc::c_int>() || fd < 0 {
        return None;
    }
    // SAFETY: a successful SO_PEERPIDFD read returns a new descriptor that
    // this process owns and nothing else closes.
    Some(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn peer_credentials(socket: RawFd) -> Option<libc::ucred> {
    let mut credentials = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut length = size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: credentials and length are live, writable and sized for the
    // ucred this option returns. An invalid descriptor returns an error.
    let status = unsafe {
        libc::getsockopt(
            socket,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        )
    };
    (status == 0 && length as usize == size_of::<libc::ucred>()).then_some(credentials)
}

fn executable(pid: u32) -> Option<PathBuf> {
    // pid 0 means the peer is outside this pid namespace: nothing to read.
    if pid == 0 {
        return None;
    }
    // This procfs kernel lookup does not open the executable's file.
    std::fs::read_link(format!("/proc/{pid}/exe")).ok()
}

/// A pidfd polls readable once its process has exited, reaped or not. Only a
/// poll that answers "not readable" counts as alive; an error does not.
pub(super) fn pidfd_alive(pidfd: &OwnedFd) -> bool {
    let mut fds = [PollFd::new(pidfd, PollFlags::IN)];
    matches!(poll(&mut fds, Some(&Timespec::default())), Ok(0))
}
