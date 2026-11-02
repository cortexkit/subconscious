use std::{
    ffi::OsString, io, mem::size_of, os::windows::ffi::OsStringExt, path::PathBuf, ptr::null_mut,
};

use windows_sys::Win32::{
    Foundation::{CloseHandle, LocalFree, ERROR_MORE_DATA, ERROR_SUCCESS, HANDLE},
    Security::{
        Authorization::ConvertSidToStringSidW, GetTokenInformation, TokenUser, TOKEN_QUERY,
        TOKEN_USER,
    },
    System::{
        Registry::{
            RegGetValueW, HKEY_LOCAL_MACHINE, RRF_NOEXPAND, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ,
        },
        Threading::{GetCurrentProcess, OpenProcessToken},
    },
};

struct Token(HANDLE);

impl Drop for Token {
    #[allow(unsafe_code)]
    fn drop(&mut self) {
        // SAFETY: Token owns the handle returned by OpenProcessToken.
        unsafe { CloseHandle(self.0) };
    }
}

struct SidString(*mut u16);

impl Drop for SidString {
    #[allow(unsafe_code)]
    fn drop(&mut self) {
        // SAFETY: ConvertSidToStringSidW allocates with LocalAlloc. LocalFree
        // accepts null, including when the conversion failed without allocating.
        unsafe { LocalFree(self.0.cast()) };
    }
}

fn call_error(call: &str, source: io::Error) -> io::Error {
    io::Error::new(source.kind(), format!("{call}: {source}"))
}

#[allow(unsafe_code)]
fn process_user_sid() -> io::Result<String> {
    let mut token = null_mut();
    // SAFETY: the current-process pseudo handle is valid and token is writable.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(call_error("OpenProcessToken", io::Error::last_os_error()));
    }
    let token = Token(token);
    let mut bytes = 0;
    // SAFETY: a null buffer with zero capacity asks for the required size.
    unsafe { GetTokenInformation(token.0, TokenUser, null_mut(), 0, &mut bytes) };
    if (bytes as usize) < size_of::<TOKEN_USER>() {
        return Err(call_error(
            "GetTokenInformation(TokenUser size)",
            io::Error::last_os_error(),
        ));
    }
    let mut buffer = vec![0usize; (bytes as usize).div_ceil(size_of::<usize>())];
    // SAFETY: usize storage aligns TOKEN_USER and covers the requested bytes.
    if unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            bytes,
            &mut bytes,
        )
    } == 0
    {
        return Err(call_error(
            "GetTokenInformation(TokenUser)",
            io::Error::last_os_error(),
        ));
    }
    // SAFETY: the successful query initialized TOKEN_USER and its embedded SID.
    let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    let mut sid = SidString(null_mut());
    // SAFETY: the SID points into the still-live token buffer; output is writable.
    if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut sid.0) } == 0 {
        return Err(call_error(
            "ConvertSidToStringSidW",
            io::Error::last_os_error(),
        ));
    }
    let mut length = 0;
    // SAFETY: successful conversion returned a null-terminated UTF-16 allocation,
    // kept alive by SidString until its contents have been copied.
    let text = unsafe {
        while *sid.0.add(length) != 0 {
            length += 1;
        }
        std::slice::from_raw_parts(sid.0, length)
    };
    String::from_utf16(text).map_err(|source| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid user SID: {source}"),
        )
    })
}

/// The profile folder Windows records for the account that owns the current
/// process: the literal `ProfileImagePath` value under
/// `HKEY_LOCAL_MACHINE\SOFTWARE\Microsoft\Windows NT\CurrentVersion\ProfileList\<SID>`,
/// where `<SID>` is the security identifier of the process token's user. The
/// value is read with `RRF_NOEXPAND` and never expanded, so no environment
/// variable changes the result. A failed token or registry read returns its
/// `io::Error`; an empty, non-absolute, `%`-containing or embedded-NUL value
/// returns `InvalidData`. The function doesn't check that the folder exists.
#[allow(unsafe_code)]
pub fn profile_dir() -> io::Result<PathBuf> {
    let sid = process_user_sid()?;
    let key: Vec<u16> = format!(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\ProfileList\{sid}")
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let name: Vec<u16> = "ProfileImagePath".encode_utf16().chain(Some(0)).collect();
    let flags = RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ | RRF_NOEXPAND;
    let mut bytes = 0;
    // SAFETY: key and name are terminated UTF-16 strings; a null data buffer
    // requests only the size. RRF_NOEXPAND forbids environment expansion.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            name.as_ptr(),
            flags,
            null_mut(),
            null_mut(),
            &mut bytes,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(call_error(
            "read ProfileList ProfileImagePath size",
            io::Error::from_raw_os_error(status as i32),
        ));
    }
    loop {
        if bytes == 0 {
            return Err(invalid_profile("is empty"));
        }
        if !(bytes as usize).is_multiple_of(size_of::<u16>()) {
            return Err(invalid_profile("is not UTF-16 data"));
        }
        let mut value = vec![0u16; bytes as usize / size_of::<u16>()];
        // SAFETY: value is writable for bytes bytes. Registry handles are opened
        // and closed internally by RegGetValueW. The raw value is never expanded.
        let status = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                key.as_ptr(),
                name.as_ptr(),
                flags,
                null_mut(),
                value.as_mut_ptr().cast(),
                &mut bytes,
            )
        };
        if status == ERROR_MORE_DATA {
            continue;
        }
        if status != ERROR_SUCCESS {
            return Err(call_error(
                "read ProfileList ProfileImagePath",
                io::Error::from_raw_os_error(status as i32),
            ));
        }
        value.truncate(bytes as usize / size_of::<u16>());
        return profile_from_value(&value);
    }
}

fn invalid_profile(reason: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("ProfileImagePath {reason}"),
    )
}

fn profile_from_value(value: &[u16]) -> io::Result<PathBuf> {
    let Some((&0, value)) = value.split_last() else {
        return Err(invalid_profile("is not null-terminated"));
    };
    if value.is_empty() {
        return Err(invalid_profile("is empty"));
    }
    if value.contains(&u16::from(b'%')) {
        return Err(invalid_profile(
            "contains '%' and cannot be expanded safely",
        ));
    }
    if value.contains(&0) {
        return Err(invalid_profile("contains an embedded null"));
    }
    let directory = PathBuf::from(OsString::from_wide(value));
    if !directory.is_absolute() {
        return Err(invalid_profile("is not absolute"));
    }
    Ok(directory)
}

#[cfg(test)]
mod tests;
