use std::{fs::File, io, mem::size_of, os::windows::io::AsRawHandle, slice};
use windows_sys::Win32::{
    Foundation::LocalFree,
    Security::{
        AclSizeInformation,
        Authorization::{GetSecurityInfo, SetSecurityInfo, SE_FILE_OBJECT},
        EqualSid, GetAclInformation, GetSecurityDescriptorControl, ACL, ACL_SIZE_INFORMATION,
        DACL_SECURITY_INFORMATION, GROUP_SECURITY_INFORMATION, LABEL_SECURITY_INFORMATION,
        OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
        PSID, SECURITY_DESCRIPTOR_CONTROL, SE_DACL_AUTO_INHERITED, SE_DACL_PRESENT,
        SE_DACL_PROTECTED, UNPROTECTED_DACL_SECURITY_INFORMATION,
    },
};

#[derive(Default)]
struct Security {
    descriptor: PSECURITY_DESCRIPTOR,
    owner: PSID,
    group: PSID,
    dacl: *mut ACL,
    label: *mut ACL,
    control: SECURITY_DESCRIPTOR_CONTROL,
}

impl Drop for Security {
    fn drop(&mut self) {
        // SAFETY: The descriptor is null or an allocation from GetSecurityInfo.
        unsafe { LocalFree(self.descriptor) };
    }
}

impl Security {
    fn read(file: &File) -> io::Result<Self> {
        let mut data = Self::default();
        // SAFETY: The file handle and output pointers remain valid. The
        // allocated descriptor owns all the returned pointers to its fields.
        let error = unsafe {
            GetSecurityInfo(
                file.as_raw_handle(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION
                    | GROUP_SECURITY_INFORMATION
                    | DACL_SECURITY_INFORMATION
                    | LABEL_SECURITY_INFORMATION,
                &mut data.owner,
                &mut data.group,
                &mut data.dacl,
                &mut data.label,
                &mut data.descriptor,
            )
        };
        if error != 0 {
            return Err(io::Error::from_raw_os_error(error as i32));
        }
        let mut revision = 0;
        // SAFETY: The descriptor is owned by data and the outputs are writable.
        if unsafe {
            GetSecurityDescriptorControl(data.descriptor, &mut data.control, &mut revision)
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(data)
    }

    fn differences(&self, other: &Self) -> io::Result<u32> {
        let mut fields = 0;
        if !equal_sids(self.owner, other.owner) {
            fields |= OWNER_SECURITY_INFORMATION;
        }
        if !equal_sids(self.group, other.group) {
            fields |= GROUP_SECURITY_INFORMATION;
        }
        let dacl_flags = SE_DACL_PROTECTED | SE_DACL_PRESENT | SE_DACL_AUTO_INHERITED;
        if !equal_acls(self.dacl, other.dacl)? || (self.control ^ other.control) & dacl_flags != 0 {
            fields |= DACL_SECURITY_INFORMATION;
            fields |= if self.control & SE_DACL_PROTECTED != 0 {
                PROTECTED_DACL_SECURITY_INFORMATION
            } else {
                UNPROTECTED_DACL_SECURITY_INFORMATION
            };
        }
        if !equal_acls(self.label, other.label)? {
            fields |= LABEL_SECURITY_INFORMATION;
        }
        Ok(fields)
    }
}

fn equal_sids(a: PSID, b: PSID) -> bool {
    if a.is_null() || b.is_null() {
        return a == b;
    }
    // SAFETY: Both non-null pointers refer to SIDs in live OS descriptors.
    unsafe { EqualSid(a, b) != 0 }
}

fn equal_acls(a: *mut ACL, b: *mut ACL) -> io::Result<bool> {
    if a.is_null() || b.is_null() {
        return Ok(a == b);
    }
    fn size(acl: *mut ACL) -> io::Result<usize> {
        let mut data = ACL_SIZE_INFORMATION::default();
        // SAFETY: The ACL is live inside a security descriptor. The output has
        // the AclSizeInformation layout and size.
        if unsafe {
            GetAclInformation(
                acl,
                (&mut data as *mut ACL_SIZE_INFORMATION).cast(),
                size_of::<ACL_SIZE_INFORMATION>() as u32,
                AclSizeInformation,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(data.AclBytesInUse as usize)
    }
    let (a_size, b_size) = (size(a)?, size(b)?);
    // Compare ACE bytes, not spare capacity or reserved header bytes.
    // SAFETY: The headers and returned sizes describe ACLs in live OS
    // descriptors. The slices do not outlive those descriptors.
    Ok(unsafe {
        (*a).AclRevision == (*b).AclRevision
            && (*a).AceCount == (*b).AceCount
            && a_size == b_size
            && a_size >= size_of::<ACL>()
            && slice::from_raw_parts(a.cast::<u8>(), a_size)[size_of::<ACL>()..]
                == slice::from_raw_parts(b.cast::<u8>(), b_size)[size_of::<ACL>()..]
    })
}

pub(super) fn preserve_metadata(source: &File, destination: &File) -> io::Result<()> {
    if source.metadata()?.permissions().readonly() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "target is read-only",
        ));
    }
    let original = Security::read(source)?;
    let staging = Security::read(destination)?;
    let fields = original.differences(&staging)?;
    if fields == 0 {
        return Ok(());
    }
    // SAFETY: The destination handle is live and the pointers are borrowed
    // from original, which retains the backing descriptor through the call.
    let error = unsafe {
        SetSecurityInfo(
            destination.as_raw_handle(),
            SE_FILE_OBJECT,
            fields,
            original.owner,
            original.group,
            original.dacl,
            original.label,
        )
    };
    if error != 0 {
        return Err(io::Error::from_raw_os_error(error as i32));
    }
    if original.differences(&Security::read(destination)?)? != 0 {
        return Err(io::Error::other(
            "filesystem did not retain the security descriptor",
        ));
    }
    Ok(())
}
