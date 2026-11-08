//! File-object identity taken from the same handle that supplied captured bytes.

use std::{fs::File, io};

#[derive(Debug)]
pub enum OpenRegularError {
    Io(io::Error),
    Symlink,
    NotRegularFile,
}
impl std::fmt::Display for OpenRegularError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "input open failed: {error}"),
            Self::Symlink => f.write_str("input leaf is a symlink or reparse point"),
            Self::NotRegularFile => f.write_str("input handle is not a regular file"),
        }
    }
}
impl std::error::Error for OpenRegularError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}
impl From<io::Error> for OpenRegularError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Open a regular input without following its leaf. Parent directories remain
/// the caller's responsibility. Nonblocking open prevents a FIFO substitution
/// from hanging before the descriptor's type can be checked.
#[cfg(unix)]
pub fn open_regular_nofollow(path: &std::path::Path) -> Result<File, OpenRegularError> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|error| {
            if error.raw_os_error() == Some(libc::ELOOP) {
                OpenRegularError::Symlink
            } else {
                OpenRegularError::Io(error)
            }
        })?;
    // File::metadata uses fstat on this descriptor, not a fresh path lookup.
    if !file.metadata()?.is_file() {
        return Err(OpenRegularError::NotRegularFile);
    }
    Ok(file)
}

/// Windows equivalent of the regular-input opener. Reparse-point and type
/// checks use the opened handle, never a second lookup through the pathname.
#[cfg(windows)]
#[allow(unsafe_code)]
pub fn open_regular_nofollow(path: &std::path::Path) -> Result<File, OpenRegularError> {
    use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, GetFileType, BY_HANDLE_FILE_INFORMATION,
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_TYPE_DISK,
    };
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    // SAFETY: file owns the live handle for the entire query.
    if unsafe { GetFileType(file.as_raw_handle()) } != FILE_TYPE_DISK {
        return Err(OpenRegularError::NotRegularFile);
    }
    let mut information: BY_HANDLE_FILE_INFORMATION = Default::default();
    // SAFETY: file owns the handle; information is a correctly sized writable
    // output buffer and both remain live throughout the synchronous query.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut information) } == 0 {
        return Err(io::Error::last_os_error().into());
    }
    if information.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(OpenRegularError::Symlink);
    }
    if information.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
        return Err(OpenRegularError::NotRegularFile);
    }
    Ok(file)
}

#[cfg(test)]
#[path = "regular_open_tests.rs"]
mod open_tests;

#[cfg(unix)]
pub type RetainedFileIdentity = crate::FileIdentity;
#[cfg(windows)]
pub type RetainedFileIdentity = crate::WindowsFileIdentity;

/// Identify an already-open file, without resolving its mutable pathname again.
/// Unix identities contain device and inode; Windows identities contain the
/// volume serial and all sixteen file-id bytes. Content hashes remain separate.
#[cfg(unix)]
pub fn retained_file_identity(file: &File) -> io::Result<RetainedFileIdentity> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file.metadata()?;
    Ok(RetainedFileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

/// Identify an already-open file using the complete Windows file-object id.
#[cfg(windows)]
#[allow(unsafe_code)]
pub fn retained_file_identity(file: &File) -> io::Result<RetainedFileIdentity> {
    use std::{mem::size_of, os::windows::io::AsRawHandle};
    use windows_sys::Win32::Storage::FileSystem::{
        FileIdInfo, GetFileInformationByHandleEx, FILE_ID_INFO,
    };
    let mut info: FILE_ID_INFO = Default::default();
    // SAFETY: file owns a live handle; the writable buffer has the FileIdInfo
    // layout and size, and neither the handle nor the buffer escapes this call.
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileIdInfo,
            (&mut info as *mut FILE_ID_INFO).cast(),
            size_of::<FILE_ID_INFO>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(RetainedFileIdentity {
        volume_serial_number: info.VolumeSerialNumber,
        file_id: info.FileId.Identifier,
    })
}

#[cfg(all(test, any(unix, windows)))]
mod tests {
    use super::*;

    #[test]
    fn retained_file_identity_hard_links_share_identity_but_copies_do_not() {
        let root = std::env::temp_dir().join(format!(
            "subc-retained-file-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let first = root.join("first");
        let link = root.join("link");
        let copy = root.join("copy");
        std::fs::write(&first, b"same bytes").unwrap();
        std::fs::hard_link(&first, &link).unwrap();
        std::fs::copy(&first, &copy).unwrap();
        let id = |path| retained_file_identity(&File::open(path).unwrap()).unwrap();
        assert_eq!(id(&first), id(&link));
        assert_ne!(id(&first), id(&copy));
        std::fs::remove_dir_all(root).unwrap();
    }
}
