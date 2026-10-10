//! Independent native ACL observations and deliberately insecure fixtures.
//! Available only for tests or with the opt-in `test-support` feature.

use super::{wide, Security, User};
use std::{io, mem::size_of, path::Path, ptr::null_mut};
use windows_sys::Win32::{
    Foundation::{CloseHandle, GetLastError, LocalFree, ERROR_NOT_ALL_ASSIGNED, HANDLE},
    Security::{
        AdjustTokenPrivileges,
        Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW, GetNamedSecurityInfoW,
            SetNamedSecurityInfoW, SDDL_REVISION_1, SE_FILE_OBJECT,
        },
        EqualSid, GetAce, GetSecurityDescriptorControl, GetSecurityDescriptorDacl,
        GetSecurityDescriptorOwner, LookupPrivilegeValueW, ACCESS_ALLOWED_ACE, ACL,
        CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION, INHERITED_ACE, LUID_AND_ATTRIBUTES,
        OBJECT_INHERIT_ACE, OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
        PSECURITY_DESCRIPTOR, PSID, SE_DACL_PROTECTED, SE_PRIVILEGE_ENABLED,
        TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
    },
    Storage::FileSystem::FILE_ALL_ACCESS,
    System::Threading::{GetCurrentProcess, OpenProcessToken},
};

/// Enable SeRestorePrivilege on this process's token, if the token holds it.
///
/// Windows lets a process set a file's owner only to itself or a group it owns,
/// unless this privilege is enabled. The foreign-owner fixtures need an owner
/// that is neither, so they enable it first. On an unelevated token the
/// privilege is absent and this returns an error; the fixture then reports a
/// skip instead of a false pass.
fn enable_restore_privilege() -> io::Result<()> {
    let name: Vec<u16> = "SeRestorePrivilege".encode_utf16().chain(Some(0)).collect();
    let mut token: HANDLE = null_mut();
    // SAFETY: The pseudo-handle is valid and the output handle is writable.
    if unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut token,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: The privilege structure is fully initialized before use.
    let mut privileges: TOKEN_PRIVILEGES = unsafe { std::mem::zeroed() };
    privileges.PrivilegeCount = 1;
    let result = (|| {
        // SAFETY: name is terminated and the LUID output is writable.
        if unsafe {
            LookupPrivilegeValueW(
                null_mut(),
                name.as_ptr(),
                &mut privileges.Privileges[0].Luid,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        privileges.Privileges[0] = LUID_AND_ATTRIBUTES {
            Luid: privileges.Privileges[0].Luid,
            Attributes: SE_PRIVILEGE_ENABLED,
        };
        // SAFETY: token was opened with adjust rights; privileges is initialized.
        if unsafe { AdjustTokenPrivileges(token, 0, &privileges, 0, null_mut(), null_mut()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // AdjustTokenPrivileges succeeds even when the token lacks the
        // privilege, and reports that only through the last error.
        // SAFETY: Reads the calling thread's last error.
        if unsafe { GetLastError() } == ERROR_NOT_ALL_ASSIGNED {
            return Err(io::Error::from_raw_os_error(ERROR_NOT_ALL_ASSIGNED as i32));
        }
        Ok(())
    })();
    // SAFETY: token is a handle this function opened.
    unsafe { CloseHandle(token) };
    result
}

/// Assert native owner, protection and exactly one current-user full-control
/// ACE. This reads Windows directly, not the production validator or ACL builder.
pub fn assert_owner_only(path: &Path, directory: bool, protected: bool) {
    let name = wide(path).unwrap();
    let mut security = Security {
        descriptor: null_mut(),
        owner: null_mut(),
        dacl: null_mut(),
    };
    // SAFETY: name is terminated and outputs are writable. The descriptor keeps
    // the returned owner and ACL alive until all assertions have completed.
    assert_eq!(
        unsafe {
            GetNamedSecurityInfoW(
                name.as_ptr(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut security.owner,
                null_mut(),
                &mut security.dacl,
                null_mut(),
                &mut security.descriptor,
            )
        },
        0
    );
    let user = User::current().unwrap();
    assert!(!security.owner.is_null());
    // SAFETY: Both SIDs belong to live Win32 buffers.
    assert_ne!(
        unsafe { EqualSid(security.owner, user.sid()) },
        0,
        "current-user owner"
    );
    let mut control = 0;
    let mut revision = 0;
    // SAFETY: descriptor is live and control/revision are writable outputs.
    assert_ne!(
        unsafe { GetSecurityDescriptorControl(security.descriptor, &mut control, &mut revision) },
        0
    );
    assert_eq!(
        control & SE_DACL_PROTECTED != 0,
        protected,
        "DACL protection"
    );
    assert!(!security.dacl.is_null(), "null DACL grants Everyone access");
    // SAFETY: GetNamedSecurityInfoW returned a valid ACL in the live descriptor.
    assert_eq!(
        unsafe { (*security.dacl).AceCount },
        1,
        "exactly one user ACE"
    );
    let mut raw = null_mut();
    // SAFETY: The ACL contains exactly one ACE, retained by security.
    assert_ne!(unsafe { GetAce(security.dacl, 0, &mut raw) }, 0);
    // SAFETY: GetAce returned a valid ACE; check its header before its fields.
    let ace = unsafe { &*raw.cast::<ACCESS_ALLOWED_ACE>() };
    assert_eq!(ace.Header.AceType, 0, "ACCESS_ALLOWED ACE");
    assert!(usize::from(ace.Header.AceSize) >= size_of::<ACCESS_ALLOWED_ACE>());
    let sid: PSID = std::ptr::addr_of!(ace.SidStart).cast_mut().cast();
    // SAFETY: Both SID pointers remain live through the comparison.
    assert_ne!(
        unsafe { EqualSid(sid, user.sid()) },
        0,
        "current-user trustee"
    );
    assert_eq!(ace.Mask, FILE_ALL_ACCESS, "full control for user");
    let expected = if directory {
        OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE
    } else {
        0
    } | if protected { 0 } else { INHERITED_ACE };
    assert_eq!(
        u32::from(ace.Header.AceFlags),
        expected,
        "exact inheritance flags"
    );
}

struct Descriptor(PSECURITY_DESCRIPTOR);

impl Drop for Descriptor {
    fn drop(&mut self) {
        // SAFETY: The SDDL parser allocated this descriptor with LocalAlloc.
        unsafe { LocalFree(self.0) };
    }
}

/// Apply test-only SDDL to a disposable path, independently of the ACL writer.
/// `owner` selects whether the parsed owner, rather than the DACL, is applied.
pub fn apply_sddl(path: &Path, sddl: &str, owner: bool) -> io::Result<()> {
    let name = wide(path)?;
    let sddl: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
    let mut raw = null_mut();
    // SAFETY: The terminated SDDL and writable output live through this call.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut raw,
            null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let descriptor = Descriptor(raw);
    let mut acl: *mut ACL = null_mut();
    let mut owner_sid = null_mut();
    let mut defaulted = 0;
    let flags = if owner {
        enable_restore_privilege()?;
        // SAFETY: descriptor is live; outputs are writable locals.
        if unsafe { GetSecurityDescriptorOwner(descriptor.0, &mut owner_sid, &mut defaulted) } == 0
        {
            return Err(io::Error::last_os_error());
        }
        OWNER_SECURITY_INFORMATION
    } else {
        let mut present = 0;
        // SAFETY: descriptor is live; outputs are writable locals. A null DACL
        // is intentionally allowed here for the unrestricted-access fixture.
        if unsafe {
            GetSecurityDescriptorDacl(descriptor.0, &mut present, &mut acl, &mut defaulted)
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        assert_ne!(present, 0, "fixture must specify a DACL");
        DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION
    };
    // SAFETY: name is terminated and the parsed descriptor retains the SID/ACL.
    let error = unsafe {
        SetNamedSecurityInfoW(
            name.as_ptr(),
            SE_FILE_OBJECT,
            flags,
            owner_sid,
            null_mut(),
            acl,
            null_mut(),
        )
    };
    if error != 0 {
        return Err(io::Error::from_raw_os_error(error as i32));
    }
    Ok(())
}

/// Grant Everyone inheritable full control on a disposable directory fixture.
pub fn grant_everyone(path: &Path) {
    apply_sddl(path, "D:P(A;OICI;FA;;;WD)", false).unwrap();
}

/// Grant Everyone read access on a disposable file fixture.
pub fn grant_everyone_read(path: &Path) {
    apply_sddl(path, "D:P(A;;FR;;;WD)", false).unwrap();
}
