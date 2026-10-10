//! Owner-only Windows DACLs for directories and credential files.
//!
//! New objects receive a protected DACL at creation, rather than exposing a
//! secret under an inherited DACL before tightening it. Readers inspect the
//! opened handle and never repair an untrusted file.

#![allow(unsafe_code)]

use std::{
    fs::File,
    io,
    mem::{size_of, zeroed},
    os::windows::{ffi::OsStrExt, io::AsRawHandle, io::FromRawHandle},
    path::Path,
    ptr::{null, null_mut},
};

use windows_sys::Win32::{
    Foundation::{CloseHandle, LocalFree, ERROR_ALREADY_EXISTS, HANDLE, INVALID_HANDLE_VALUE},
    Security::{
        AddAccessAllowedAceEx,
        Authorization::{GetSecurityInfo, SetSecurityInfo, SE_FILE_OBJECT},
        EqualSid, GetAce, GetLengthSid, GetTokenInformation, InitializeAcl,
        InitializeSecurityDescriptor, IsWellKnownSid, SetSecurityDescriptorControl,
        SetSecurityDescriptorDacl, SetSecurityDescriptorOwner, TokenUser,
        WinBuiltinAdministratorsSid, WinLocalSystemSid, ACCESS_ALLOWED_ACE, ACE_HEADER, ACL,
        ACL_REVISION, CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION, OBJECT_INHERIT_ACE,
        OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
        PSID, SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR, SE_DACL_PROTECTED, TOKEN_QUERY, TOKEN_USER,
    },
    Storage::FileSystem::{
        CreateDirectoryW, CreateFileW, GetFileInformationByHandle, GetFileType,
        BY_HANDLE_FILE_INFORMATION, CREATE_NEW, FILE_ALL_ACCESS, FILE_ATTRIBUTE_DIRECTORY,
        FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        FILE_TYPE_DISK, OPEN_EXISTING, READ_CONTROL, WRITE_DAC,
    },
    System::Threading::{GetCurrentProcess, OpenProcessToken},
};

struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: Handle owns a valid handle returned by a successful Win32 call.
        unsafe { CloseHandle(self.0) };
    }
}

fn wide(path: &Path) -> io::Result<Vec<u16>> {
    let mut value: Vec<u16> = path.as_os_str().encode_wide().collect();
    if value.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path contains NUL",
        ));
    }
    value.push(0);
    Ok(value)
}

struct User(Vec<usize>);

impl User {
    fn current() -> io::Result<Self> {
        let mut token = null_mut();
        // SAFETY: The pseudo process handle is valid and token is a writable output.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let token = Handle(token);
        let mut bytes = 0;
        // SAFETY: A null buffer with length zero requests the required buffer size.
        unsafe { GetTokenInformation(token.0, TokenUser, null_mut(), 0, &mut bytes) };
        if (bytes as usize) < size_of::<TOKEN_USER>() {
            return Err(io::Error::last_os_error());
        }
        let mut user = Self(vec![0; (bytes as usize).div_ceil(size_of::<usize>())]);
        // SAFETY: The pointer-aligned buffer has at least bytes bytes and retains
        // the embedded SID for the lifetime of User.
        if unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                user.0.as_mut_ptr().cast(),
                bytes,
                &mut bytes,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(user)
    }

    fn sid(&self) -> PSID {
        // SAFETY: current initialized an aligned TOKEN_USER and its embedded SID.
        unsafe { (*(self.0.as_ptr().cast::<TOKEN_USER>())).User.Sid }
    }
}

struct OwnerAcl(Vec<u32>);

impl OwnerAcl {
    fn new(user: &User, directory: bool) -> io::Result<Self> {
        // SAFETY: user owns a valid token SID for the duration of this call.
        let sid_bytes = unsafe { GetLengthSid(user.sid()) } as usize;
        let bytes =
            size_of::<ACL>() + size_of::<ACCESS_ALLOWED_ACE>() - size_of::<u32>() + sid_bytes;
        let mut acl = Self(vec![0; bytes.div_ceil(size_of::<u32>())]);
        let flags = if directory {
            OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE
        } else {
            0
        };
        // SAFETY: The aligned ACL buffer fits its header and one ACE. The SID is
        // copied into the ACL by AddAccessAllowedAceEx.
        if unsafe {
            InitializeAcl(
                acl.as_mut_ptr(),
                (acl.0.len() * size_of::<u32>()) as u32,
                ACL_REVISION,
            ) == 0
                || AddAccessAllowedAceEx(
                    acl.as_mut_ptr(),
                    ACL_REVISION,
                    flags,
                    FILE_ALL_ACCESS,
                    user.sid(),
                ) == 0
        } {
            return Err(io::Error::last_os_error());
        }
        Ok(acl)
    }

    fn as_mut_ptr(&mut self) -> *mut ACL {
        self.0.as_mut_ptr().cast()
    }
}

fn with_security<T>(
    directory: bool,
    create: impl FnOnce(&SECURITY_ATTRIBUTES) -> io::Result<T>,
) -> io::Result<T> {
    let user = User::current()?;
    let mut acl = OwnerAcl::new(&user, directory)?;
    // SAFETY: SECURITY_DESCRIPTOR is a plain Win32 output structure.
    let mut descriptor: SECURITY_DESCRIPTOR = unsafe { zeroed() };
    let descriptor_ptr = (&mut descriptor as *mut SECURITY_DESCRIPTOR).cast();
    // SAFETY: All buffers are aligned and remain live until create returns. The
    // owner is explicit because an elevated token can default to Administrators.
    if unsafe {
        InitializeSecurityDescriptor(descriptor_ptr, 1) == 0
            || SetSecurityDescriptorOwner(descriptor_ptr, user.sid(), 0) == 0
            || SetSecurityDescriptorDacl(descriptor_ptr, 1, acl.as_mut_ptr(), 0) == 0
            || SetSecurityDescriptorControl(descriptor_ptr, SE_DACL_PROTECTED, SE_DACL_PROTECTED)
                == 0
    } {
        return Err(io::Error::last_os_error());
    }
    create(&SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor_ptr,
        bInheritHandle: 0,
    })
}

/// Create a new file with a protected DACL granting only the current user full
/// access. An existing path is never opened or truncated.
pub fn create_private_file(path: &Path) -> io::Result<File> {
    let name = wide(path)?;
    with_security(false, |attributes| {
        // SAFETY: name is terminated and attributes references live security
        // storage. CREATE_NEW prevents opening an existing file or reparse point.
        let raw = unsafe {
            CreateFileW(
                name.as_ptr(),
                FILE_ALL_ACCESS,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                attributes,
                CREATE_NEW,
                FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
                null_mut(),
            )
        };
        if raw == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: CreateFileW returned a new owned handle; File takes ownership.
        Ok(unsafe { File::from_raw_handle(raw) })
    })
}

fn open_directory(path: &Path, access: u32) -> io::Result<Handle> {
    let name = wide(path)?;
    // SAFETY: name is terminated. OPEN_REPARSE_POINT opens the final link itself.
    let raw = unsafe {
        CreateFileW(
            name.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            null(),
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
            null_mut(),
        )
    };
    if raw == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let handle = Handle(raw);
    let attributes = disk_attributes(handle.0)?;
    if attributes & FILE_ATTRIBUTE_DIRECTORY == 0 || attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not an ordinary directory",
        ));
    }
    Ok(handle)
}

fn disk_attributes(handle: HANDLE) -> io::Result<u32> {
    // SAFETY: BY_HANDLE_FILE_INFORMATION is a plain Win32 output structure.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { zeroed() };
    // SAFETY: handle is live and info is a correctly sized writable output.
    if unsafe { GetFileInformationByHandle(handle, &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: handle is live; GetFileType has no output pointers.
    if unsafe { GetFileType(handle) } != FILE_TYPE_DISK {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a disk file",
        ));
    }
    Ok(info.dwFileAttributes)
}

fn create_all(path: &Path) -> io::Result<bool> {
    if path.as_os_str().is_empty() {
        return Ok(false);
    }
    match std::fs::symlink_metadata(path) {
        Ok(_) => return open_directory(path, READ_CONTROL).map(|_| false),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    if let Some(parent) = path.parent() {
        create_all(parent)?;
    }
    let name = wide(path)?;
    with_security(true, |attributes| {
        // SAFETY: name is terminated and the descriptor/ACL outlive this call.
        // Every missing component receives private security at creation.
        if unsafe { CreateDirectoryW(name.as_ptr(), attributes) } != 0 {
            return Ok(true);
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_ALREADY_EXISTS as i32) {
            open_directory(path, READ_CONTROL).map(|_| false)
        } else {
            Err(error)
        }
    })
}

/// Create missing directories with a protected owner-only DACL and tighten the
/// requested directory if it already exists. Foreign owners and reparse points
/// are refused. Unprotected descendants lose broad inherited grants too;
/// protected child DACLs are deliberately preserved by Windows.
pub fn create_private_dir(path: &Path) -> io::Result<()> {
    // New directories have their final DACL from first visibility. Only an
    // existing directory needs its permissions narrowed after opening it.
    if create_all(path)? {
        return Ok(());
    }
    let handle = open_directory(path, READ_CONTROL | WRITE_DAC)?;
    let user = User::current()?;
    let security = Security::query(handle.0)?;
    security.verify_owner(&user)?;
    let mut acl = OwnerAcl::new(&user, true)?;
    // Windows can bypass directory traversal checks, so narrowing only the
    // parent would not protect existing descendants. SetSecurityInfo propagates
    // the inheritable ACE while preserving explicitly protected child DACLs.
    // SAFETY: handle and ACL are live. Owner, group and SACL are unchanged.
    let error = unsafe {
        SetSecurityInfo(
            handle.0,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            acl.as_mut_ptr(),
            null(),
        )
    };
    if error != 0 {
        return Err(io::Error::from_raw_os_error(error as i32));
    }
    Ok(())
}

struct Security {
    descriptor: PSECURITY_DESCRIPTOR,
    owner: PSID,
    dacl: *mut ACL,
}

impl Drop for Security {
    fn drop(&mut self) {
        // SAFETY: GetSecurityInfo allocated this descriptor with LocalAlloc.
        unsafe { LocalFree(self.descriptor) };
    }
}

fn insecure(reason: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, reason)
}

impl Security {
    fn query(handle: HANDLE) -> io::Result<Self> {
        let mut security = Self {
            descriptor: null_mut(),
            owner: null_mut(),
            dacl: null_mut(),
        };
        // SAFETY: The handle is borrowed from a live File or Handle. Output
        // pointers are writable, and the descriptor retains its owner and DACL.
        let error = unsafe {
            GetSecurityInfo(
                handle,
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut security.owner,
                null_mut(),
                &mut security.dacl,
                null_mut(),
                &mut security.descriptor,
            )
        };
        if error != 0 {
            return Err(io::Error::from_raw_os_error(error as i32));
        }
        Ok(security)
    }

    fn verify_owner(&self, user: &User) -> io::Result<()> {
        // SYSTEM and Administrators are accepted as owners for the same reason
        // they are accepted in the DACL: they can bypass file security anyway.
        // A file written by an elevated process is owned by Administrators, so
        // refusing that owner would lock out files an older daemon wrote while
        // adding no protection.
        // SAFETY: Both SIDs belong to live Win32 token/security buffers.
        let trusted = !self.owner.is_null()
            && unsafe {
                EqualSid(self.owner, user.sid()) != 0
                    || IsWellKnownSid(self.owner, WinLocalSystemSid) != 0
                    || IsWellKnownSid(self.owner, WinBuiltinAdministratorsSid) != 0
            };
        if !trusted {
            return Err(insecure(
                "owner is not the current user, SYSTEM or Administrators",
            ));
        }
        Ok(())
    }

    fn verify(&self, user: &User) -> io::Result<()> {
        self.verify_owner(user)?;
        // A null or absent DACL grants unrestricted access, unlike an empty DACL.
        if self.dacl.is_null() {
            return Err(insecure("null DACL grants unrestricted access"));
        }
        // SAFETY: GetSecurityInfo returned a valid ACL in the live descriptor.
        let count = unsafe { (*self.dacl).AceCount };
        for index in 0..u32::from(count) {
            let mut raw = null_mut();
            // SAFETY: index is within this live ACL's ACE count.
            if unsafe { GetAce(self.dacl, index, &mut raw) } == 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: GetAce returned a valid ACE header inside the descriptor.
            let header = unsafe { &*raw.cast::<ACE_HEADER>() };
            // Native ACE types 0 and 1 are ACCESS_ALLOWED and ACCESS_DENIED.
            // Denials cannot widen access. Unrecognized grant types (including
            // conditional/object ACEs) are refused rather than guessed at.
            match header.AceType {
                1 => continue,
                0 => {}
                _ => return Err(insecure("unsupported DACL entry type")),
            }
            if usize::from(header.AceSize) < size_of::<ACCESS_ALLOWED_ACE>() {
                return Err(insecure("truncated DACL entry"));
            }
            // SAFETY: This is a validated access-allowed ACE; its variable SID
            // is part of the valid security descriptor returned by Windows.
            let ace = unsafe { &*raw.cast::<ACCESS_ALLOWED_ACE>() };
            let sid: PSID = std::ptr::addr_of!(ace.SidStart).cast_mut().cast();
            // SAFETY: The SID and token user buffers remain live. SYSTEM and
            // Administrators are trusted because they can bypass file ACLs anyway.
            let trusted = unsafe {
                EqualSid(sid, user.sid()) != 0
                    || IsWellKnownSid(sid, WinLocalSystemSid) != 0
                    || IsWellKnownSid(sid, WinBuiltinAdministratorsSid) != 0
            };
            if ace.Mask != 0 && !trusted {
                return Err(insecure("DACL grants access to another user or group"));
            }
        }
        Ok(())
    }
}

/// Verify the owner and DACL of an already opened credential file. Grants may
/// name only the current user, SYSTEM or Administrators. Null DACLs and unknown
/// ACE types fail closed. This function does not change security or reopen a path.
pub fn verify_owner_only(file: &File) -> io::Result<()> {
    let handle = file.as_raw_handle();
    if disk_attributes(handle)? & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT) != 0 {
        return Err(insecure("not an ordinary file"));
    }
    Security::query(handle)?.verify(&User::current()?)
}

/// Verify an existing publication directory without changing its security.
/// This prevents publishing into a redirected directory another user controls.
pub fn verify_private_dir(path: &Path) -> io::Result<()> {
    let handle = open_directory(path, READ_CONTROL)?;
    Security::query(handle.0)?.verify(&User::current()?)
}

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
#[cfg(test)]
mod tests;
