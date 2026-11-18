use crate::retained_replace::Stage;
use rustix::fs::{self, RenameFlags};
use std::{ffi::OsStr, fs::File, io};

pub(super) fn rename_exclusive(parent: &File, name: &str, leaf: &OsStr) -> io::Result<()> {
    fs::renameat_with(parent, name, parent, leaf, RenameFlags::NOREPLACE)?;
    Ok(())
}

// An ACL can carry more information than the mode bits, including named user
// and group entries. Read the kernel's binary ACL so the entries and mask are
// preserved exactly. Some Linux filesystems expose other ACL forms; refuse a
// replace when those aren't absent instead of reducing them to mode bits.
const POSIX_ACL: &str = "system.posix_acl_access";

pub(super) fn read_acl(file: &File) -> io::Result<Option<Vec<u8>>> {
    let mut names = vec![0; 65_536]; // Linux XATTR_LIST_MAX.
    let length = match fs::flistxattr(file, names.as_mut_slice()) {
        Ok(length) => length,
        Err(rustix::io::Errno::OPNOTSUPP) => 0,
        Err(error) => return Err(error.into()),
    };
    for name in names[..length].split(|byte| *byte == 0) {
        if name.starts_with(b"system.") && name != POSIX_ACL.as_bytes() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "unknown system file metadata",
            ));
        }
    }
    // NFS and SMB may support fetching an ACL that isn't listed in flistxattr.
    for name in [
        "system.nfs4_acl",
        "system.cifs_acl",
        "system.smb3_acl",
        "system.richacl",
    ] {
        if xattr(file, name)?.is_some() {
            return Err(io::Error::new(io::ErrorKind::Unsupported, "non-POSIX ACL"));
        }
    }
    xattr(file, POSIX_ACL)
}

fn xattr(file: &File, name: &str) -> io::Result<Option<Vec<u8>>> {
    let mut data = vec![0; 65_536]; // Linux XATTR_SIZE_MAX.
    match fs::fgetxattr(file, name, data.as_mut_slice()) {
        Ok(length) => {
            data.truncate(length);
            Ok(Some(data))
        }
        Err(rustix::io::Errno::NODATA | rustix::io::Errno::OPNOTSUPP) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub(super) fn write_acl(file: &File, acl: Option<Vec<u8>>) -> io::Result<()> {
    if let Some(acl) = &acl {
        fs::fsetxattr(file, POSIX_ACL, acl, fs::XattrFlags::empty())?;
    } else {
        match fs::fremovexattr(file, POSIX_ACL) {
            Ok(()) | Err(rustix::io::Errno::NODATA | rustix::io::Errno::OPNOTSUPP) => (),
            Err(error) => return Err(error.into()),
        }
    }
    if read_acl(file)? != acl {
        return Err(io::Error::other("filesystem did not retain the POSIX ACL"));
    }
    Ok(())
}

pub(super) fn flush_staging(file: &File, _directory: bool) -> io::Result<()> {
    file.sync_all()
}

pub(super) fn flush_publication(stage: &Stage<'_>) -> io::Result<()> {
    stage.parent.sync_all()
}
