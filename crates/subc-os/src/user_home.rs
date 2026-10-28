use std::{ffi::CStr, io, mem::MaybeUninit, os::unix::ffi::OsStrExt, path::PathBuf};

/// Read the process's effective Unix uid from the kernel, without environment input.
#[allow(unsafe_code)]
pub fn effective_uid() -> u32 {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() }
}

/// Read the real user's home directory from the password database, using
/// `getpwuid_r(getuid())` rather than `HOME` or any other environment variable.
/// Returns an error if the user has no entry or the home directory is not absolute.
#[allow(unsafe_code)]
pub fn real_user_home_dir() -> io::Result<PathBuf> {
    // SAFETY: getuid has no preconditions and returns the real uid, not the effective uid.
    let uid = unsafe { libc::getuid() };
    let mut entry = MaybeUninit::<libc::passwd>::uninit();
    let mut buffer = Vec::<u8>::new();
    let mut size = 1024;
    loop {
        buffer
            .try_reserve(size - buffer.len())
            .map_err(io::Error::other)?;
        buffer.resize(size, 0);
        let mut result = std::ptr::null_mut();
        // SAFETY: entry and result are writable; buffer remains valid for its
        // advertised size until the returned home directory has been copied.
        let status = unsafe {
            libc::getpwuid_r(
                uid,
                entry.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        if status == libc::ERANGE {
            size = size
                .checked_mul(2)
                .ok_or_else(|| io::Error::other("password entry is too large"))?;
            continue;
        }
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status));
        }
        if result.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "real uid has no password entry",
            ));
        }
        // SAFETY: a successful getpwuid_r with a non-null result initialized entry.
        let entry = unsafe { entry.assume_init() };
        if entry.pw_dir.is_null() {
            return Err(io::Error::other("password entry has no home directory"));
        }
        // SAFETY: pw_dir is a NUL-terminated string in the still-live buffer
        // supplied to getpwuid_r. Copy it before dropping that buffer.
        let directory = unsafe { CStr::from_ptr(entry.pw_dir) };
        let directory = PathBuf::from(std::ffi::OsStr::from_bytes(directory.to_bytes()));
        if !directory.is_absolute() {
            return Err(io::Error::other(
                "password entry home directory is not absolute",
            ));
        }
        return Ok(directory);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_user_home_dir_is_absolute() {
        assert!(real_user_home_dir().unwrap().is_absolute());
    }
}
