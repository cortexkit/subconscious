use std::{ffi::OsString, io, os::windows::ffi::OsStringExt, path::PathBuf, ptr};

use windows_sys::Win32::{
    System::Com::CoTaskMemFree,
    UI::Shell::{FOLDERID_LocalAppData, SHGetKnownFolderPath, KF_FLAG_DEFAULT},
};

struct KnownFolderBuffer(*mut u16);

impl Drop for KnownFolderBuffer {
    #[allow(unsafe_code)]
    fn drop(&mut self) {
        // SAFETY: SHGetKnownFolderPath allocates with the COM task allocator.
        // CoTaskMemFree also accepts null, including on a failed lookup.
        unsafe { CoTaskMemFree(self.0.cast()) };
    }
}

/// Read the current user's Local AppData known folder without consulting
/// `LOCALAPPDATA` or any other environment variable. The returned path is absolute.
#[allow(unsafe_code)]
pub fn local_app_data_dir() -> io::Result<PathBuf> {
    let mut buffer = KnownFolderBuffer(ptr::null_mut());
    // SAFETY: the folder ID is valid and the output pointer is writable. A null
    // token asks for the current user's folder. The guard frees even error output.
    let result = unsafe {
        SHGetKnownFolderPath(
            &FOLDERID_LocalAppData,
            KF_FLAG_DEFAULT as u32,
            ptr::null_mut(),
            &mut buffer.0,
        )
    };
    if result < 0 {
        return Err(io::Error::other(format!(
            "SHGetKnownFolderPath(LocalAppData) failed (HRESULT {result:#010x})"
        )));
    }
    if buffer.0.is_null() {
        return Err(io::Error::other(
            "SHGetKnownFolderPath(LocalAppData) returned no directory",
        ));
    }
    // SAFETY: on success the API returns a null-terminated UTF-16 allocation,
    // which remains alive until the guard is dropped after copying the path.
    let directory = unsafe {
        let mut len = 0;
        while *buffer.0.add(len) != 0 {
            len += 1;
        }
        PathBuf::from(OsString::from_wide(std::slice::from_raw_parts(
            buffer.0, len,
        )))
    };
    if !directory.is_absolute() {
        return Err(io::Error::other(
            "SHGetKnownFolderPath(LocalAppData) returned a relative directory",
        ));
    }
    Ok(directory)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_app_data_dir_ignores_hostile_environment() {
        const CHILD: &str = "SUBC_OS_LOCAL_APP_DATA_CHILD";
        let directory = local_app_data_dir().unwrap();
        assert!(directory.is_absolute(), "{directory:?}");
        assert!(directory.is_dir(), "{directory:?}");
        if let Some(expected) = std::env::var_os(CHILD) {
            assert_eq!(directory, PathBuf::from(expected));
            return;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "local_app_data::tests::local_app_data_dir_ignores_hostile_environment",
                "--nocapture",
            ])
            .env(CHILD, &directory)
            .env("LOCALAPPDATA", r"Z:\not-the-user-profile\hostile")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "child failed: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
    }
}
