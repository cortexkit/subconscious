#![allow(unsafe_code)]

use super::{ForcedFailures, ObservedPeer, PeerObservation, PeerUser};
use std::{
    ffi::OsString,
    mem::size_of_val,
    os::windows::{
        ffi::OsStringExt,
        io::{AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle},
    },
    path::PathBuf,
    ptr::null_mut,
};
use windows_sys::Win32::{
    Foundation::{GetLastError, LocalFree, ERROR_PIPE_LOCAL},
    Security::{
        Authorization::ConvertSidToStringSidW, GetTokenInformation, TokenUser, TOKEN_QUERY,
        TOKEN_USER,
    },
    System::{
        Pipes::{GetNamedPipeClientComputerNameW, GetNamedPipeClientProcessId},
        Threading::{
            OpenProcess, OpenProcessToken, QueryFullProcessImageNameW,
            PROCESS_QUERY_LIMITED_INFORMATION,
        },
    },
};

pub(super) fn observe(pipe: BorrowedHandle<'_>, forced: ForcedFailures) -> ObservedPeer {
    // A pid reported for a client on another machine names no local process.
    if !local_client(pipe) {
        return ObservedPeer::Unobserved;
    }
    let Some(pid) = client_pid(pipe) else {
        return ObservedPeer::Unobserved;
    };
    let Some(process) = open_process(pid) else {
        return ObservedPeer::Unobserved;
    };
    let Some(sid) = user_sid(&process).filter(|_| !forced.user) else {
        return ObservedPeer::Unobserved;
    };
    let exe = (!forced.exe).then(|| executable(&process)).flatten();
    ObservedPeer::Observed(PeerObservation {
        user: PeerUser::Sid(sid),
        pid,
        exe,
        signing_id: None,
    })
}

/// Windows answers a client computer-name query on a pipe whose client is on
/// this machine with `ERROR_PIPE_LOCAL`. Anything else, success included, is
/// treated as a remote or unknown client.
fn local_client(pipe: BorrowedHandle<'_>) -> bool {
    let mut name = [0u16; 256];
    // SAFETY: name is writable for the byte length passed; the pipe handle is
    // borrowed for the call.
    let named = unsafe {
        GetNamedPipeClientComputerNameW(
            pipe.as_raw_handle(),
            name.as_mut_ptr(),
            size_of_val(&name) as u32,
        )
    };
    // SAFETY: reads this thread's last-error value, set by the call above.
    named == 0 && unsafe { GetLastError() } == ERROR_PIPE_LOCAL
}

fn client_pid(pipe: BorrowedHandle<'_>) -> Option<u32> {
    let mut pid = 0;
    // SAFETY: pid is a writable u32; the pipe handle is borrowed for the call.
    let ok = unsafe { GetNamedPipeClientProcessId(pipe.as_raw_handle(), &mut pid) } != 0;
    (ok && pid != 0).then_some(pid)
}

fn open_process(pid: u32) -> Option<OwnedHandle> {
    // SAFETY: scalar arguments; the result is a new handle or null. Only the
    // rights needed to read the token user and image name are requested.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    // SAFETY: a non-null OpenProcess result is a handle owned here alone.
    (!handle.is_null()).then(|| unsafe { OwnedHandle::from_raw_handle(handle) })
}

/// The user SID of the process's token, in `S-1-…` form.
fn user_sid(process: &OwnedHandle) -> Option<String> {
    let mut token = null_mut();
    // SAFETY: process is a live handle with limited query rights, which
    // OpenProcessToken accepts; token is a writable out-pointer.
    if unsafe { OpenProcessToken(process.as_raw_handle(), TOKEN_QUERY, &mut token) } == 0
        || token.is_null()
    {
        return None;
    }
    // SAFETY: a successful OpenProcessToken returns a handle owned here alone.
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let mut needed = 0;
    // SAFETY: a null buffer asks for the size TOKEN_USER needs.
    unsafe { GetTokenInformation(token.as_raw_handle(), TokenUser, null_mut(), 0, &mut needed) };
    if needed == 0 {
        return None;
    }
    // usize storage gives TOKEN_USER and the SID that follows it alignment.
    let mut buffer = vec![0usize; (needed as usize).div_ceil(size_of::<usize>())];
    // SAFETY: buffer is aligned and at least the size Windows asked for.
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
        return None;
    }
    // SAFETY: the successful query initialized a TOKEN_USER at the start.
    let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    let mut text = null_mut();
    // SAFETY: the SID points into buffer, still live; text is an out-pointer.
    if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut text) } == 0 || text.is_null() {
        return None;
    }
    let mut length = 0;
    // SAFETY: ConvertSidToStringSidW returns a NUL-terminated UTF-16 string.
    while unsafe { *text.add(length) } != 0 {
        length += 1;
    }
    // SAFETY: length covers the initialized characters before the NUL.
    let sid = String::from_utf16(unsafe { std::slice::from_raw_parts(text, length) }).ok();
    // SAFETY: the string was LocalAlloc'd by ConvertSidToStringSidW and is
    // freed once, after its last use.
    unsafe { LocalFree(text.cast()) };
    sid.filter(|sid| sid.starts_with("S-"))
}

fn executable(process: &OwnedHandle) -> Option<PathBuf> {
    let mut path = vec![0u16; 32768];
    let mut length = path.len() as u32;
    // SAFETY: the handle is live; path and length are writable for the stated
    // UTF-16 capacity. The returned length excludes the NUL.
    let ok = unsafe {
        QueryFullProcessImageNameW(process.as_raw_handle(), 0, path.as_mut_ptr(), &mut length)
    } != 0;
    if !ok || length == 0 || length as usize > path.len() {
        return None;
    }
    Some(PathBuf::from(OsString::from_wide(&path[..length as usize])))
}
