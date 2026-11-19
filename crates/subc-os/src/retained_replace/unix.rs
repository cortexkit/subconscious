#![allow(unsafe_code)]

use super::{invalid, OpenRetainedError, Stage};
use crate::{retained_file_identity, RetainedFileIdentity};
use rustix::fs::{self, AtFlags, FileType, Gid, Mode, OFlags, Uid};
use std::{ffi::OsStr, fs::File, io, os::unix::fs::MetadataExt};

#[cfg_attr(target_os = "linux", path = "unix/linux.rs")]
#[cfg_attr(target_os = "macos", path = "unix/macos.rs")]
mod os;

#[cfg(target_os = "macos")]
pub(super) fn remove_inherited_acl(file: &File) -> io::Result<()> {
    os::write_acl(file, None)
}

pub(super) fn ensure_directory(parent: &File) -> io::Result<()> {
    if !parent.metadata()?.is_dir() {
        return Err(invalid("retained parent is not a directory"));
    }
    Ok(())
}

pub(super) fn random(bytes: &mut [u8; 16]) -> io::Result<()> {
    // SAFETY: The output buffer is writable for the supplied length, below
    // getentropy's 256-byte limit, and the call doesn't retain the pointer.
    if unsafe { libc::getentropy(bytes.as_mut_ptr().cast(), bytes.len()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(super) fn create(
    parent: &File,
    name: &str,
    directory: bool,
    publish: bool,
) -> io::Result<File> {
    let nofollow = OFlags::CLOEXEC | OFlags::NOFOLLOW;
    let fd = if directory {
        fs::mkdirat(parent, name, Mode::from_raw_mode(0o777))?;
        let created = entry(parent, name.as_ref(), true)?;
        let fd = fs::openat(
            parent,
            name,
            nofollow | OFlags::DIRECTORY | OFlags::RDONLY,
            Mode::empty(),
        )?;
        let file = File::from(fd);
        if retained_file_identity(&file)? != created {
            return Err(io::Error::other("staging directory identity changed"));
        }
        return Ok(file);
    } else {
        fs::openat(
            parent,
            name,
            nofollow | OFlags::CREATE | OFlags::EXCL | OFlags::RDWR,
            Mode::from_raw_mode(if publish { 0o666 } else { 0o600 }),
        )?
    };
    Ok(fd.into())
}

pub(super) fn open_target(parent: &File, name: &OsStr) -> io::Result<File> {
    // The pre-check avoids opening devices or FIFOs under normal operation;
    // inspect the opened handle again because the directory entry can change.
    let identity = entry(parent, name, false)?;
    let file = File::from(fs::openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.nlink() != 1 || retained_file_identity(&file)? != identity {
        return Err(io::Error::other("replacement target changed during open"));
    }
    Ok(file)
}

pub(super) fn open_read_at(
    parent: &File,
    name: &OsStr,
    directory: bool,
) -> Result<File, OpenRetainedError> {
    let check = |ty, single_link| match ty {
        FileType::Symlink => Err(OpenRetainedError::Symlink),
        FileType::Directory if directory => Ok(()),
        FileType::RegularFile if !directory && single_link => Ok(()),
        FileType::RegularFile if !directory => Err(OpenRetainedError::Hardlinked),
        _ => Err(OpenRetainedError::NotRegular),
    };
    let check_entry = || {
        let stat = fs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW).map_err(io::Error::from)?;
        check(FileType::from_raw_mode(stat.st_mode), stat.st_nlink == 1)
    };
    // Avoid opening special files, then check the actual handle in case the
    // entry changed. A validated one-component lookup cannot escape parent.
    check_entry()?;
    let mut flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    if directory {
        flags |= OFlags::DIRECTORY;
    }
    let file = match fs::openat(parent, name, flags, Mode::empty()) {
        Ok(fd) => File::from(fd),
        Err(rustix::io::Errno::LOOP) => return Err(OpenRetainedError::Symlink),
        Err(error) => {
            // Linux can report ENOTDIR rather than ELOOP for a directory link.
            if error == rustix::io::Errno::NOTDIR {
                check_entry()?;
            }
            return Err(io::Error::from(error).into());
        }
    };
    let stat = fs::fstat(&file).map_err(io::Error::from)?;
    check(FileType::from_raw_mode(stat.st_mode), stat.st_nlink == 1)?;
    Ok(file)
}

#[allow(clippy::unnecessary_cast)] // Stat's integer types differ across Unix targets.
fn entry(parent: &File, name: &OsStr, directory: bool) -> io::Result<RetainedFileIdentity> {
    let stat = fs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)?;
    let ty = FileType::from_raw_mode(stat.st_mode);
    if ty == FileType::Symlink {
        return Err(io::Error::from_raw_os_error(libc::ELOOP));
    }
    if if directory {
        ty != FileType::Directory
    } else {
        ty != FileType::RegularFile || stat.st_nlink != 1
    } {
        return Err(invalid(
            "expected a directory or a singly linked regular file",
        ));
    }
    Ok(RetainedFileIdentity {
        device: stat.st_dev as u64,
        inode: stat.st_ino as u64,
    })
}

pub(super) fn verify_stage_entry(stage: &Stage<'_>) -> io::Result<()> {
    if entry(stage.parent, stage.name.as_ref(), stage.directory)? != stage.identity {
        return Err(io::Error::other("staging entry identity changed"));
    }
    Ok(())
}

pub(super) fn preserve_metadata(source: &File, destination: &File) -> io::Result<()> {
    let original = source.metadata()?;
    let staging = destination.metadata()?;
    let mode = original.mode() & 0o7777;
    let acl = os::read_acl(source)?;
    if original.uid() != staging.uid() || original.gid() != staging.gid() {
        fs::fchown(
            destination,
            Some(Uid::from_raw(original.uid())),
            Some(Gid::from_raw(original.gid())),
        )?;
    }
    // Ownership changes can clear mode bits. Installing the ACL last also
    // removes an inherited staging ACL when the replaced file has none.
    fs::fchmod(destination, Mode::from_raw_mode(mode as _))?;
    os::write_acl(destination, acl)?;
    let actual = destination.metadata()?;
    if actual.uid() != original.uid()
        || actual.gid() != original.gid()
        || actual.mode() & 0o7777 != mode
    {
        return Err(io::Error::other(
            "filesystem did not retain owner, group and mode",
        ));
    }
    Ok(())
}

pub(super) fn rename(stage: &Stage<'_>, leaf: &OsStr, replace: bool) -> io::Result<()> {
    if replace {
        fs::renameat(stage.parent, &stage.name, stage.parent, leaf)?;
        Ok(())
    } else {
        os::rename_exclusive(stage.parent, &stage.name, leaf)
    }
}

pub(super) fn retry<T>(operation: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    operation()
}

pub(super) fn flush_staging(file: &File, directory: bool) -> io::Result<()> {
    os::flush_staging(file, directory)
}

pub(super) fn flush_publication(stage: &Stage<'_>) -> io::Result<()> {
    os::flush_publication(stage)
}

pub(super) fn remove_stage(stage: &Stage<'_>) {
    let flags = if stage.directory {
        AtFlags::REMOVEDIR
    } else {
        AtFlags::empty()
    };
    let _ = fs::unlinkat(stage.parent, &stage.name, flags);
}
