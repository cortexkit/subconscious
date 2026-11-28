#![allow(unsafe_code)]

use super::{invalid, OpenRetainedError, Stage};
use crate::{retained_file_information, WindowsFileInformation};
use std::{
    ffi::OsStr,
    fs::File,
    io,
    mem::{offset_of, size_of},
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle},
    },
    ptr::{null, null_mut},
    thread,
    time::Duration,
};
use windows_sys::{
    Wdk::{
        Foundation::OBJECT_ATTRIBUTES,
        Storage::FileSystem::{
            FileRenameInformation, FileRenameInformationEx, NtCreateFile, NtSetInformationFile,
            FILE_CREATE, FILE_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_REPARSE_POINT,
            FILE_SYNCHRONOUS_IO_NONALERT, FILE_WRITE_THROUGH,
        },
    },
    Win32::{
        Foundation::{
            RtlNtStatusToDosError, ERROR_INVALID_FUNCTION, ERROR_INVALID_PARAMETER,
            ERROR_NOT_SUPPORTED, ERROR_SHARING_VIOLATION, NTSTATUS, UNICODE_STRING,
        },
        Security::Cryptography::{BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG},
        Storage::FileSystem::{
            FileDispositionInfo, GetFileType, SetFileInformationByHandle, DELETE,
            FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT,
            FILE_DISPOSITION_INFO, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_READ_ATTRIBUTES,
            FILE_RENAME_INFO, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TYPE_DISK,
            READ_CONTROL, SYNCHRONIZE, WRITE_DAC, WRITE_OWNER,
        },
        System::{
            WindowsProgramming::{
                FILE_RENAME_FLAG_POSIX_SEMANTICS, FILE_RENAME_FLAG_REPLACE_IF_EXISTS,
            },
            IO::IO_STATUS_BLOCK,
        },
    },
};

#[path = "windows/security.rs"]
mod security;
pub(super) fn preserve_metadata(source: &File, destination: &File) -> io::Result<()> {
    security::preserve_metadata(source, destination)
}

const SHARE_ALL: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;

fn nt_result(status: NTSTATUS) -> io::Result<()> {
    if status >= 0 {
        return Ok(());
    }
    // SAFETY: RtlNtStatusToDosError accepts any NTSTATUS value.
    Err(io::Error::from_raw_os_error(
        unsafe { RtlNtStatusToDosError(status) } as i32,
    ))
}

pub(super) fn random(bytes: &mut [u8; 16]) -> io::Result<()> {
    // SAFETY: The buffer is valid for the supplied size; the system RNG
    // requires no provider handle.
    nt_result(unsafe {
        BCryptGenRandom(
            null_mut(),
            bytes.as_mut_ptr(),
            bytes.len() as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    })
}

fn open_relative(
    parent: &File,
    name: &OsStr,
    access: u32,
    share: u32,
    disposition: u32,
    options: u32,
) -> io::Result<File> {
    open_relative_secured(
        parent,
        name,
        access,
        share,
        disposition,
        options,
        null_mut(),
    )
}

fn open_relative_secured(
    parent: &File,
    name: &OsStr,
    access: u32,
    share: u32,
    disposition: u32,
    options: u32,
    descriptor: *mut std::ffi::c_void,
) -> io::Result<File> {
    const OBJ_CASE_INSENSITIVE: u32 = 0x40;
    let mut name: Vec<_> = name.encode_wide().collect();
    let length = u16::try_from(name.len().saturating_mul(size_of::<u16>()))
        .map_err(|_| invalid("relative filename is too long"))?;
    let name = UNICODE_STRING {
        Length: length,
        MaximumLength: length,
        Buffer: name.as_mut_ptr(),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: parent.as_raw_handle(),
        ObjectName: &name,
        Attributes: OBJ_CASE_INSENSITIVE,
        // A null descriptor lets the filesystem inherit the parent's security.
        SecurityDescriptor: descriptor.cast_const().cast(),
        SecurityQualityOfService: null(),
    };
    let mut handle = null_mut();
    let mut status = IO_STATUS_BLOCK::default();
    // SAFETY: The parent and the validated single-component UTF-16 name stay
    // live. The output buffers are writable and the open is synchronous.
    nt_result(unsafe {
        NtCreateFile(
            &mut handle,
            access | SYNCHRONIZE,
            &attributes,
            &mut status,
            null(),
            FILE_ATTRIBUTE_NORMAL,
            share,
            disposition,
            options | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
            null(),
            0,
        )
    })?;
    // SAFETY: A successful synchronous NtCreateFile returned an owned handle.
    Ok(unsafe { File::from_raw_handle(handle) })
}

fn object_info(file: &File) -> io::Result<WindowsFileInformation> {
    match read_object_info(file) {
        Err(OpenRetainedError::Symlink) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "refusing a reparse point",
        )),
        Err(OpenRetainedError::NotRegular) => {
            Err(invalid("retained handle is not a disk file or directory"))
        }
        Err(OpenRetainedError::Io(error)) => Err(error),
        Err(OpenRetainedError::Hardlinked) => Err(invalid("entry is not a singly linked file")),
        Ok(info) => Ok(info),
    }
}

fn read_object_info(file: &File) -> Result<WindowsFileInformation, OpenRetainedError> {
    // SAFETY: file owns the live handle for the duration of the call.
    if unsafe { GetFileType(file.as_raw_handle()) } != FILE_TYPE_DISK {
        return Err(OpenRetainedError::NotRegular);
    }
    let info = retained_file_information(file)?;
    if info.attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(OpenRetainedError::Symlink);
    }
    Ok(info)
}

pub(super) fn open_read_at(
    parent: &File,
    name: &OsStr,
    directory: bool,
) -> Result<File, OpenRetainedError> {
    if read_object_info(parent)?.attributes() & FILE_ATTRIBUTE_DIRECTORY == 0 {
        return Err(OpenRetainedError::NotRegular);
    }
    let file = open_relative(parent, name, FILE_GENERIC_READ, SHARE_ALL, FILE_OPEN, 0)?;
    let info = read_object_info(&file)?;
    if (info.attributes() & FILE_ATTRIBUTE_DIRECTORY != 0) != directory {
        return Err(OpenRetainedError::NotRegular);
    }
    if !directory && info.link_count() != 1 {
        return Err(OpenRetainedError::Hardlinked);
    }
    Ok(file)
}

pub(super) fn ensure_directory(parent: &File) -> io::Result<()> {
    if object_info(parent)?.attributes() & FILE_ATTRIBUTE_DIRECTORY == 0 {
        return Err(invalid("retained parent is not a directory"));
    }
    Ok(())
}

pub(super) fn regular_file_at(parent: &File, leaf: &OsStr) -> io::Result<()> {
    let file = open_relative(parent, leaf, FILE_READ_ATTRIBUTES, SHARE_ALL, FILE_OPEN, 0)?;
    if object_info(&file)?.attributes() & FILE_ATTRIBUTE_DIRECTORY != 0 {
        return Err(invalid("expected a regular file"));
    }
    Ok(())
}

pub(super) fn create(
    parent: &File,
    name: &str,
    directory: bool,
    publish: bool,
) -> io::Result<File> {
    let access = FILE_GENERIC_READ | FILE_GENERIC_WRITE | DELETE;
    let (access, share, options) = if directory {
        (
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            FILE_DIRECTORY_FILE,
        )
    } else if publish {
        (access, 0, 0)
    } else {
        (access | WRITE_DAC | WRITE_OWNER, 0, 0)
    };
    if directory && !publish {
        return crate::windows_acl::with_security(true, |attributes| {
            open_relative_secured(
                parent,
                name.as_ref(),
                access,
                share,
                FILE_CREATE,
                options | FILE_WRITE_THROUGH,
                attributes.lpSecurityDescriptor,
            )
        });
    }
    open_relative(
        parent,
        name.as_ref(),
        access,
        share,
        FILE_CREATE,
        options | FILE_WRITE_THROUGH,
    )
}

pub(super) fn open_target(parent: &File, name: &OsStr) -> io::Result<File> {
    let file = open_relative(
        parent,
        name,
        READ_CONTROL | FILE_READ_ATTRIBUTES,
        SHARE_ALL,
        FILE_OPEN,
        0,
    )?;
    let info = object_info(&file)?;
    if info.attributes() & FILE_ATTRIBUTE_DIRECTORY != 0 || info.link_count() != 1 {
        return Err(invalid("expected a singly linked regular file"));
    }
    Ok(file)
}

pub(super) fn verify_stage_entry(stage: &Stage<'_>) -> io::Result<()> {
    let info = object_info(&stage.file)?;
    if !stage.directory && info.link_count() != 1 {
        return Err(invalid("expected a singly linked staging file"));
    }
    // The source of the rename is the staging handle itself. It does not
    // share deletion, so no other open can rename it out of the directory.
    Ok(())
}

pub(super) fn rename(stage: &Stage<'_>, leaf: &OsStr, replace: bool) -> io::Result<()> {
    rename_open_file(&stage.file, leaf, replace)
}

fn rename_open_file(file: &File, leaf: &OsStr, replace: bool) -> io::Result<()> {
    rename_to(file, None, leaf, replace)
}

fn rename_to(
    file: &File,
    destination: Option<&File>,
    leaf: &OsStr,
    replace: bool,
) -> io::Result<()> {
    let name: Vec<u16> = leaf.encode_wide().collect();
    let name_bytes = name.len() * size_of::<u16>();
    let buffer_bytes =
        (offset_of!(FILE_RENAME_INFO, FileName) + name_bytes).max(size_of::<FILE_RENAME_INFO>());
    let length =
        u32::try_from(buffer_bytes).map_err(|_| invalid("relative filename is too long"))?;
    let mut buffer = vec![0usize; buffer_bytes.div_ceil(size_of::<usize>())];
    let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    let mut status = IO_STATUS_BLOCK::default();
    // SAFETY: The aligned buffer contains the whole variable-length NT rename
    // structure. Both handles and the buffer live throughout each synchronous
    // call. A null RootDirectory with a bare filename renames the source
    // within its current directory. The source denies delete sharing. For a
    // move to a different directory, RootDirectory is the retained destination
    // handle, and the filename remains a single component.
    unsafe {
        if let Some(directory) = destination {
            (*info).RootDirectory = directory.as_raw_handle();
        }
        (*info).FileNameLength = name_bytes as u32;
        std::ptr::copy_nonoverlapping(name.as_ptr(), (*info).FileName.as_mut_ptr(), name.len());
        if replace {
            (*info).Anonymous.Flags =
                FILE_RENAME_FLAG_REPLACE_IF_EXISTS | FILE_RENAME_FLAG_POSIX_SEMANTICS;
            match nt_result(NtSetInformationFile(
                file.as_raw_handle(),
                &mut status,
                info.cast(),
                length,
                FileRenameInformationEx,
            )) {
                Ok(()) => return Ok(()),
                Err(error)
                    if matches!(
                        error.raw_os_error().map(|code| code as u32),
                        Some(
                            ERROR_INVALID_PARAMETER | ERROR_NOT_SUPPORTED | ERROR_INVALID_FUNCTION
                        )
                    ) => {}
                Err(error) => return Err(error),
            }
        }
        // The legacy information class still renames atomically by handle,
        // but can refuse replacement when the target has open readers.
        (*info).Anonymous.Flags = u32::from(replace);
        nt_result(NtSetInformationFile(
            file.as_raw_handle(),
            &mut status,
            info.cast(),
            length,
            FileRenameInformation,
        ))
    }
}

pub(super) fn move_aside(parent: &File, leaf: &OsStr, aside: &OsStr) -> io::Result<File> {
    let file = open_relative(
        parent,
        leaf,
        FILE_GENERIC_READ | DELETE,
        FILE_SHARE_READ,
        FILE_OPEN,
        0,
    )?;
    let info = object_info(&file)?;
    if info.attributes() & FILE_ATTRIBUTE_DIRECTORY != 0 || info.link_count() != 1 {
        return Err(invalid("expected a singly linked regular file"));
    }
    rename_open_file(&file, aside, false)?;
    Ok(file)
}

pub(super) fn put_back(moved: &super::RetainedMovedFile<'_>, leaf: &OsStr) -> io::Result<()> {
    ensure_directory(moved.parent)?;
    rename_open_file(&moved.file, leaf, false)
}

pub(super) fn relocate(
    moved: &super::RetainedMovedFile<'_>,
    dest: &File,
    leaf: &OsStr,
) -> io::Result<()> {
    let directory = (dest.as_raw_handle() != moved.parent.as_raw_handle()).then_some(dest);
    rename_to(&moved.file, directory, leaf, false)
}

pub(super) fn directory_path(parent: &File) -> io::Result<std::path::PathBuf> {
    use std::{ffi::OsString, os::windows::ffi::OsStringExt};
    use windows_sys::Win32::Storage::FileSystem::GetFinalPathNameByHandleW;
    let mut buffer = vec![0u16; 32768];
    // SAFETY: the directory handle is borrowed and the buffer is writable.
    let len = unsafe {
        GetFinalPathNameByHandleW(
            parent.as_raw_handle(),
            buffer.as_mut_ptr(),
            buffer.len() as u32,
            0,
        )
    } as usize;
    if len == 0 {
        return Err(io::Error::last_os_error());
    }
    if len >= buffer.len() {
        return Err(invalid("directory path is too long"));
    }
    Ok(std::path::PathBuf::from(OsString::from_wide(
        &buffer[..len],
    )))
}

pub(super) fn verify_private_directory(parent: &File) -> io::Result<()> {
    ensure_directory(parent)?;
    crate::windows_acl::verify_private_dir_handle(parent)
}

pub(super) fn discard_moved(
    moved: &super::RetainedMovedFile<'_>,
    inspected: crate::RetainedFileIdentity,
) -> io::Result<()> {
    if crate::retained_file_identity(&moved.file)? != inspected {
        return Err(invalid("recovery handle identity changed"));
    }
    remove_by_handle(&moved.file)
}

pub(super) fn retry<T>(mut operation: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    for delay in [5, 10, 20, 40, 80] {
        match operation() {
            Err(error) if error.raw_os_error() == Some(ERROR_SHARING_VIOLATION as i32) => {
                thread::sleep(Duration::from_millis(delay));
            }
            result => return result,
        }
    }
    operation()
}

pub(super) fn flush_staging(file: &File, directory: bool) -> io::Result<()> {
    if directory {
        // The empty directory was created with FILE_WRITE_THROUGH.
        Ok(())
    } else {
        file.sync_all()
    }
}

pub(super) fn flush_publication(stage: &Stage<'_>) -> io::Result<()> {
    // Flush the exact renamed object opened with FILE_WRITE_THROUGH, not a
    // lookup of the final path. Windows provides no POSIX directory fsync.
    stage.file.sync_all()
}

pub(super) fn remove_stage(stage: &Stage<'_>) -> io::Result<()> {
    remove_by_handle(&stage.file)
}

fn remove_by_handle(file: &File) -> io::Result<()> {
    let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
    // SAFETY: The handle is the exact staging object opened with DELETE. The
    // input structure is valid and live through this synchronous call.
    if unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FileDispositionInfo,
            (&disposition as *const FILE_DISPOSITION_INFO).cast(),
            size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    } == 0
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
