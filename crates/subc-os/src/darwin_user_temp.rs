use std::{ffi::OsString, io, os::unix::ffi::OsStringExt, path::PathBuf};

/// Read Darwin's per-user temporary directory without consulting `TMPDIR` or
/// any other environment variable. The returned path is absolute.
#[allow(unsafe_code)]
pub fn darwin_user_temp_dir() -> io::Result<PathBuf> {
    // SAFETY: a null buffer with length zero asks confstr for the required size.
    let size = unsafe { libc::confstr(libc::_CS_DARWIN_USER_TEMP_DIR, std::ptr::null_mut(), 0) };
    if size == 0 {
        return Err(io::Error::other(
            "confstr(_CS_DARWIN_USER_TEMP_DIR) returned no directory",
        ));
    }
    let mut bytes = vec![0u8; size];
    // SAFETY: the writable buffer has exactly the advertised length.
    let written = unsafe {
        libc::confstr(
            libc::_CS_DARWIN_USER_TEMP_DIR,
            bytes.as_mut_ptr().cast(),
            bytes.len(),
        )
    };
    if written == 0 || written > bytes.len() || bytes[written - 1] != 0 {
        return Err(io::Error::other(
            "confstr(_CS_DARWIN_USER_TEMP_DIR) did not return a complete directory",
        ));
    }
    bytes.truncate(written - 1);
    let directory = PathBuf::from(OsString::from_vec(bytes));
    if !directory.is_absolute() {
        return Err(io::Error::other(
            "confstr(_CS_DARWIN_USER_TEMP_DIR) returned a relative directory",
        ));
    }
    Ok(directory)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn darwin_user_temp_dir_is_absolute_and_exists() {
        let directory = darwin_user_temp_dir().unwrap();
        assert!(directory.is_absolute(), "{directory:?}");
        assert!(directory.is_dir(), "{directory:?}");
    }
}
