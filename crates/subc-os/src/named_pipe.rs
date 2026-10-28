//! Owner-only, local-only Windows named pipes, behind a safe API.
//!
//! The daemon's local listener is a named pipe, created by tokio's overlapped
//! named-pipe server so reads and writes can run at once on one end. Creating
//! that end with a chosen DACL is an `unsafe` call tokio cannot make safe, so
//! this crate builds the security attributes:
//!
//! - [`OwnerOnlySecurity`] holds a protected DACL that grants full access to
//!   the current user's SID, plus LocalSystem and Administrators, which Win32
//!   lets into a kernel object whatever its DACL says; naming them keeps the
//!   access check simple. No other user is admitted.
//! - [`reject_remote_client`] refuses a client that reached the machine over
//!   SMB. The pipe is also created with `PIPE_REJECT_REMOTE_CLIENTS`, so the
//!   kernel refuses such a client before any byte is exchanged; this check
//!   catches one that gets further than that.
//!
//! Other platforms have no named pipes, so every function here reports
//! [`io::ErrorKind::Unsupported`].

#![allow(unsafe_code)]

use std::io;

#[cfg(windows)]
mod windows;

#[cfg(windows)]
pub use windows::OwnerOnlySecurity;

/// Create the security attributes for a pipe whose DACL admits only the current
/// user, LocalSystem and Administrators.
///
/// Hold the returned value for as long as the pipe end is created, and pass
/// [`OwnerOnlySecurity::attributes_ptr`] as tokio's
/// `create_with_security_attributes_raw` argument. The pointer stays valid for
/// the value's whole life; it is not an owning handle.
#[cfg(windows)]
pub fn owner_only_security() -> io::Result<OwnerOnlySecurity> {
    OwnerOnlySecurity::new()
}

/// Named pipes exist only on Windows.
#[cfg(not(windows))]
pub fn owner_only_security() -> io::Result<()> {
    Err(unsupported())
}

/// Refuse a client that reached the pipe from another machine. Call it before
/// reading anything from the connection.
#[cfg(windows)]
pub fn reject_remote_client(pipe: &impl std::os::windows::io::AsRawHandle) -> io::Result<()> {
    windows::reject_remote_client(pipe.as_raw_handle())
}

/// [`reject_remote_client`] for a caller that already holds the raw handle,
/// such as tokio's `NamedPipeServer`, whose `AsRawHandle` is not public.
#[cfg(windows)]
pub fn reject_remote_client_raw(handle: std::os::windows::raw::HANDLE) -> io::Result<()> {
    windows::reject_remote_client(handle)
}

/// Create one end of `pipe_name`, the daemon's local listener end on Windows:
/// overlapped, byte mode, remote clients refused, and with an owner-only DACL.
/// `first` marks the process's first end for that name. Needs the `tokio`
/// feature, because the end is tokio's server type.
#[cfg(all(windows, feature = "tokio"))]
pub fn create_end(
    pipe_name: &std::ffi::OsStr,
    first: bool,
) -> io::Result<tokio::net::windows::named_pipe::NamedPipeServer> {
    owner_only_security()?.create_end(pipe_name, first)
}

/// Named pipes exist only on Windows.
#[cfg(not(windows))]
pub fn reject_remote_client(_pipe: &std::fs::File) -> io::Result<()> {
    Err(unsupported())
}

/// The SID of the current user, as `S-1-5-21-…`, so a test or check can compare
/// it with a client's SID without opening a token itself.
#[cfg(windows)]
pub fn current_user_sid_string() -> io::Result<String> {
    windows::current_user_sid_string()
}

/// The current user's SID as a string exists only on Windows.
#[cfg(not(windows))]
pub fn current_user_sid_string() -> io::Result<String> {
    Err(unsupported())
}

#[cfg(not(windows))]
fn unsupported() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "named pipes exist only on Windows",
    )
}
