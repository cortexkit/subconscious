//! File-object identity taken from the same handle that supplied captured bytes.

use std::{fs::File, io};

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
