use crate::retained_replace::Stage;
use std::{
    ffi::{c_int, c_void, CString, OsStr},
    fs::File,
    io,
    os::{fd::AsRawFd, unix::ffi::OsStrExt},
    ptr::NonNull,
};

const ACL_TYPE_EXTENDED: c_int = 0x100;
// Darwin's acl_set_fd_np accepts the _FILESEC_REMOVE_ACL sentinel from <sys/fcntl.h>.
const REMOVE_ACL: *mut c_void = std::ptr::without_provenance_mut(1);

extern "C" {
    fn acl_get_fd_np(fd: c_int, acl_type: c_int) -> *mut c_void;
    fn acl_set_fd_np(fd: c_int, acl: *mut c_void, acl_type: c_int) -> c_int;
    fn acl_free(acl: *mut c_void) -> c_int;
    fn acl_size(acl: *mut c_void) -> isize;
    fn acl_copy_ext(buffer: *mut c_void, acl: *mut c_void, size: isize) -> isize;
}

pub(super) fn rename_exclusive(
    parent: &File,
    name: &OsStr,
    dest: &File,
    leaf: &OsStr,
) -> io::Result<()> {
    let name = CString::new(name.as_bytes())?;
    let leaf = CString::new(leaf.as_bytes())?;
    let fd = parent.as_raw_fd();
    // SAFETY: The descriptor is borrowed from parent, and both names are
    // checked, NUL-terminated leaves that outlive the synchronous call.
    if unsafe {
        libc::renameatx_np(
            fd,
            name.as_ptr(),
            dest.as_raw_fd(),
            leaf.as_ptr(),
            libc::RENAME_EXCL,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(super) fn directory_path(parent: &File) -> io::Result<std::path::PathBuf> {
    let mut bytes = [0u8; libc::PATH_MAX as usize];
    // SAFETY: F_GETPATH writes at most PATH_MAX bytes through this live buffer.
    if unsafe { libc::fcntl(parent.as_raw_fd(), libc::F_GETPATH, bytes.as_mut_ptr()) } == -1 {
        return Err(io::Error::last_os_error());
    }
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .ok_or_else(|| io::Error::other("directory path is unterminated"))?;
    Ok(std::path::PathBuf::from(OsStr::from_bytes(&bytes[..end])))
}

pub(super) struct Acl(NonNull<c_void>);

impl Drop for Acl {
    fn drop(&mut self) {
        // SAFETY: Acl owns the non-null allocation from acl_get_fd_np.
        unsafe { acl_free(self.0.as_ptr()) };
    }
}

impl Acl {
    fn encoded(&self) -> io::Result<Vec<u32>> {
        // SAFETY: The allocation remains live for both Darwin ACL calls.
        let size = unsafe { acl_size(self.0.as_ptr()) };
        if size < 0 {
            return Err(io::Error::last_os_error());
        }
        // The ACL representation includes 32-bit fields; allocate a buffer
        // aligned for those fields as well as large enough for the ACL.
        let mut storage = vec![0u32; (size as usize).div_ceil(size_of::<u32>())];
        // SAFETY: storage is writable for at least size bytes and the Darwin
        // ACL remains valid. The call initializes exactly size bytes.
        if unsafe { acl_copy_ext(storage.as_mut_ptr().cast(), self.0.as_ptr(), size) } != size {
            return Err(io::Error::last_os_error());
        }
        Ok(storage)
    }
}

pub(super) fn read_acl(file: &File) -> io::Result<Option<Acl>> {
    // SAFETY: The fd remains open and ACL_TYPE_EXTENDED is Darwin's ACL type.
    match NonNull::new(unsafe { acl_get_fd_np(file.as_raw_fd(), ACL_TYPE_EXTENDED) }) {
        Some(acl) => Ok(Some(Acl(acl))),
        None => {
            let error = io::Error::last_os_error();
            match error.raw_os_error() {
                Some(libc::ENOENT) => Ok(None),
                _ => Err(error),
            }
        }
    }
}

pub(super) fn write_acl(file: &File, acl: Option<Acl>) -> io::Result<()> {
    let fd = file.as_raw_fd();
    let payload = match &acl {
        Some(acl) => acl.0.as_ptr(),
        None if read_acl(file)?.is_some() => REMOVE_ACL,
        None => return Ok(()),
    };
    // SAFETY: The fd remains live and the argument is either a live Darwin
    // ACL or Darwin's remove sentinel, both accepted by acl_set_fd_np.
    if unsafe { acl_set_fd_np(fd, payload, ACL_TYPE_EXTENDED) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let encoded = |acl: Option<Acl>| acl.as_ref().map(Acl::encoded).transpose();
    if encoded(read_acl(file)?)? != encoded(acl)? {
        return Err(io::Error::other(
            "filesystem did not retain the extended ACL",
        ));
    }
    Ok(())
}

pub(super) fn flush_staging(file: &File, directory: bool) -> io::Result<()> {
    if directory {
        file.sync_all()
    } else {
        full_sync(file)
    }
}

pub(super) fn flush_publication(stage: &Stage<'_>) -> io::Result<()> {
    stage.parent.sync_all()?;
    // On Darwin, fsync alone may only send data to a volatile drive cache.
    full_sync(&stage.file)
}

fn full_sync(file: &File) -> io::Result<()> {
    // SAFETY: F_FULLFSYNC takes no third argument and the fd stays open.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_FULLFSYNC) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
