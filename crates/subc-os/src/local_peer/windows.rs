#![allow(unsafe_code)]

use super::{ForcedFailures, ObservedPeer, PeerObservation, PeerUser};
use std::{
    ffi::OsString,
    os::windows::{
        ffi::OsStringExt,
        io::{AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle},
    },
    path::PathBuf,
    ptr::null_mut,
};
use windows_sys::Win32::{
    Foundation::{LocalFree, WAIT_TIMEOUT},
    Security::{
        Authorization::ConvertSidToStringSidW, GetTokenInformation, TokenUser, TOKEN_QUERY,
        TOKEN_USER,
    },
    System::Threading::{
        OpenProcess, OpenProcessToken, QueryFullProcessImageNameW, WaitForSingleObject,
        PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    },
};

pub(super) fn observe(pipe: BorrowedHandle<'_>, forced: ForcedFailures) -> ObservedPeer {
    let Ok(pid) = crate::named_pipe::local_client_pid(pipe.as_raw_handle()) else {
        return ObservedPeer::Unobserved;
    };
    observe_local_process(pid, forced)
}

pub(super) fn observe_local_process(pid: u32, forced: ForcedFailures) -> ObservedPeer {
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

fn open_process(pid: u32) -> Option<OwnedHandle> {
    open_process_with_rights(pid, PROCESS_QUERY_LIMITED_INFORMATION)
}

fn open_process_with_rights(pid: u32, rights: u32) -> Option<OwnedHandle> {
    // SAFETY: scalar arguments; the result is a new handle or null. Only the
    // query and optional wait rights are requested.
    let handle = unsafe { OpenProcess(rights, 0, pid) };
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
    // Query the kernel's image name on the existing handle; no executable is opened.
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

pub(super) fn open_diagnostic_process(pid: u32) -> Option<OwnedHandle> {
    open_process_with_rights(pid, PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE)
}

pub(super) fn process_name(process: &OwnedHandle) -> Option<String> {
    let image = executable(process)?;
    // SAFETY: the borrowed process handle has synchronize rights. A zero
    // timeout probes liveness without waiting; an exited handle is signaled.
    let alive = unsafe { WaitForSingleObject(process.as_raw_handle(), 0) } == WAIT_TIMEOUT;
    alive.then_some(image.file_name()?.to_string_lossy().into_owned())
}
