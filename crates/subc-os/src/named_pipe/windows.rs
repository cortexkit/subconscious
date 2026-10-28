//! The Windows implementation: the owner-only DACL and the local-client check.

use std::{
    ffi::c_void,
    io,
    mem::{size_of, zeroed},
    ptr::null_mut,
};

use windows_sys::Win32::{
    Foundation::{CloseHandle, ERROR_PIPE_LOCAL, HANDLE},
    Security::{
        AddAccessAllowedAceEx, CreateWellKnownSid, GetLengthSid, GetTokenInformation,
        InitializeAcl, InitializeSecurityDescriptor, SetSecurityDescriptorControl,
        SetSecurityDescriptorDacl, SetSecurityDescriptorOwner, TokenUser,
        WinBuiltinAdministratorsSid, WinLocalSystemSid, ACCESS_ALLOWED_ACE, ACL, ACL_REVISION,
        PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR,
        SECURITY_MAX_SID_SIZE, SE_DACL_PROTECTED, TOKEN_QUERY, TOKEN_USER,
    },
    System::Pipes::{GetNamedPipeClientComputerNameW, GetNamedPipeClientProcessId},
    System::Threading::{GetCurrentProcess, OpenProcessToken},
};

/// `FILE_ALL_ACCESS`.
const FILE_ALL_ACCESS: u32 = 0x001F_01FF;
/// The descriptor revision `InitializeSecurityDescriptor` documents.
const SECURITY_DESCRIPTOR_REVISION: u32 = 1;

struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: Handle owns a live handle opened by OpenProcessToken.
        unsafe { CloseHandle(self.0) };
    }
}

fn last_error() -> io::Error {
    io::Error::last_os_error()
}

/// The current user's SID, as the bytes `GetTokenInformation(TokenUser)`
/// reports.
fn current_user_sid() -> io::Result<Vec<u8>> {
    // SAFETY: GetCurrentProcess returns a pseudo handle that is not closed.
    let process = unsafe { GetCurrentProcess() };
    let mut token: HANDLE = null_mut();
    // SAFETY: process is the current process; token is a writable out pointer.
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
        return Err(last_error());
    }
    let token = Handle(token);
    let mut size = 0u32;
    // SAFETY: a zero-length query only reports the needed size and fails.
    unsafe { GetTokenInformation(token.0, TokenUser, null_mut(), 0, &mut size) };
    if size == 0 {
        return Err(last_error());
    }
    let mut buffer = vec![0u8; size as usize];
    // SAFETY: buffer is writable for exactly `size` bytes.
    if unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            size,
            &mut size,
        )
    } == 0
    {
        return Err(last_error());
    }
    // SAFETY: TokenUser filled a TOKEN_USER whose Sid field points into
    // `buffer`, and the SID is copied out before `buffer` is dropped.
    let user: TOKEN_USER = unsafe { std::ptr::read_unaligned(buffer.as_ptr().cast()) };
    // SAFETY: user.User.Sid is the SID pointer TokenUser reported.
    let sid_len = unsafe { GetLengthSid(user.User.Sid) };
    if sid_len == 0 {
        return Err(last_error());
    }
    // SAFETY: the SID has sid_len bytes inside `buffer`.
    Ok(
        unsafe { std::slice::from_raw_parts(user.User.Sid as *const u8, sid_len as usize) }
            .to_vec(),
    )
}

fn well_known_sid(kind: i32) -> io::Result<Vec<u8>> {
    let mut sid = vec![0u8; SECURITY_MAX_SID_SIZE as usize];
    let mut len = sid.len() as u32;
    // SAFETY: sid is writable for `len` bytes.
    if unsafe { CreateWellKnownSid(kind, null_mut(), sid.as_mut_ptr().cast(), &mut len) } == 0 {
        return Err(last_error());
    }
    sid.truncate(len as usize);
    Ok(sid)
}

/// A security descriptor whose protected DACL grants full access to exactly the
/// given SIDs.
///
/// The descriptor, the ACL and the SID bytes all live in this value, because
/// the descriptor points into them; it must outlive every `CreateNamedPipeW`
/// call that reads it, which is what
/// [`crate::named_pipe::owner_only_security`] returning it to the caller
/// guarantees.
pub struct OwnerOnlySecurity {
    // Read only by `create_end`, which needs the `tokio` feature.
    #[cfg_attr(not(feature = "tokio"), allow(dead_code))]
    attributes: SECURITY_ATTRIBUTES,
    _descriptor: Box<SECURITY_DESCRIPTOR>,
    _acl: Box<[u8]>,
    _sids: Vec<Vec<u8>>,
}

impl std::fmt::Debug for OwnerOnlySecurity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The DACL names users and processes; printing pointers or SID bytes
        // would be noise in a log line.
        f.debug_struct("OwnerOnlySecurity").finish_non_exhaustive()
    }
}

impl OwnerOnlySecurity {
    /// Build the attributes for the current user.
    pub(super) fn new() -> io::Result<Self> {
        // System and Administrators reach a kernel object whatever its DACL
        // says, so the descriptor grants them explicitly rather than leaving
        // their access to that.
        let sids = vec![
            current_user_sid()?,
            well_known_sid(WinLocalSystemSid)?,
            well_known_sid(WinBuiltinAdministratorsSid)?,
        ];

        let acl_capacity = sids
            .iter()
            .map(|sid| size_of::<ACCESS_ALLOWED_ACE>() + sid.len() + size_of::<u32>())
            .sum::<usize>()
            + size_of::<ACL>()
            + 16;
        let mut acl = vec![0u8; acl_capacity];
        let acl_ptr = acl.as_mut_ptr().cast::<ACL>();
        // SAFETY: acl is writable for acl_capacity bytes.
        if unsafe { InitializeAcl(acl_ptr, acl_capacity as u32, ACL_REVISION) } == 0 {
            return Err(last_error());
        }
        for sid in &sids {
            // SAFETY: the ACL is initialised and sid names a whole SID.
            if unsafe {
                AddAccessAllowedAceEx(
                    acl_ptr,
                    ACL_REVISION,
                    0,
                    FILE_ALL_ACCESS,
                    sid.as_ptr().cast_mut().cast::<c_void>(),
                )
            } == 0
            {
                return Err(last_error());
            }
        }

        // SAFETY: SECURITY_DESCRIPTOR is a struct of scalars and pointers;
        // zeroed is its empty form, which the calls below fill in.
        let mut descriptor = Box::new(unsafe { zeroed::<SECURITY_DESCRIPTOR>() });
        let descriptor_ptr: PSECURITY_DESCRIPTOR = Box::as_mut_ptr(&mut descriptor).cast();
        // SAFETY: descriptor is writable.
        if unsafe { InitializeSecurityDescriptor(descriptor_ptr, SECURITY_DESCRIPTOR_REVISION) }
            == 0
        {
            return Err(last_error());
        }
        // SAFETY: the descriptor is initialised and acl_ptr stays valid in `acl`.
        if unsafe { SetSecurityDescriptorDacl(descriptor_ptr, 1, acl_ptr, 0) } == 0 {
            return Err(last_error());
        }
        // SAFETY: the owner SID stays valid in `sids` for the descriptor's life.
        if unsafe {
            SetSecurityDescriptorOwner(descriptor_ptr, sids[0].as_ptr().cast_mut() as PSID, 0)
        } == 0
        {
            return Err(last_error());
        }
        // Without this, the parent's inherited ACEs can admit other users
        // whatever the explicit entries say.
        // SAFETY: the descriptor is initialised and carries a DACL.
        if unsafe {
            SetSecurityDescriptorControl(descriptor_ptr, SE_DACL_PROTECTED, SE_DACL_PROTECTED)
        } == 0
        {
            return Err(last_error());
        }

        Ok(Self {
            attributes: SECURITY_ATTRIBUTES {
                nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: descriptor_ptr,
                bInheritHandle: 0,
            },
            _descriptor: descriptor,
            _acl: acl.into_boxed_slice(),
            _sids: sids,
        })
    }

    /// Create one end of `pipe_name` with this DACL.
    ///
    /// The end is overlapped (tokio's server), byte mode, and refuses remote
    /// clients. `first` marks the process's first end for that name, so a name
    /// another process holds fails here instead of being shared; every end
    /// otherwise leaves room for further instances.
    #[cfg(feature = "tokio")]
    pub fn create_end(
        &mut self,
        pipe_name: &std::ffi::OsStr,
        first: bool,
    ) -> io::Result<tokio::net::windows::named_pipe::NamedPipeServer> {
        let mut options = tokio::net::windows::named_pipe::ServerOptions::new();
        options
            .first_pipe_instance(first)
            .reject_remote_clients(true);
        // SAFETY: the attributes live in `self`, which outlives this call and
        // everything the attributes point at.
        let attributes = self.attributes_ptr();
        unsafe { options.create_with_security_attributes_raw(pipe_name, attributes) }
    }

    /// The pointer tokio's `create_with_security_attributes_raw` takes.
    ///
    /// The security attributes live in this value, so the pointer stays valid
    /// until the value is dropped.
    #[cfg(feature = "tokio")]
    fn attributes_ptr(&mut self) -> *mut c_void {
        (&mut self.attributes as *mut SECURITY_ATTRIBUTES).cast()
    }
}

pub(super) fn current_user_sid_string() -> io::Result<String> {
    render_sid(&current_user_sid()?)
}

/// Refuse a client that reached the pipe over SMB: a local client has no
/// computer name, and reports `ERROR_PIPE_LOCAL` instead.
pub(super) fn reject_remote_client(raw: std::os::windows::raw::HANDLE) -> io::Result<()> {
    let mut computer = [0u16; 256];
    // SAFETY: raw is a connected pipe server handle; the buffer is writable for
    // the byte length passed with it.
    let wrote = unsafe {
        GetNamedPipeClientComputerNameW(
            raw,
            computer.as_mut_ptr(),
            computer.len() as u32 * size_of::<u16>() as u32,
        )
    };
    if wrote != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "named-pipe client is on another machine: the local endpoint refuses remote clients",
        ));
    }
    let error = last_error();
    if error.raw_os_error() != Some(ERROR_PIPE_LOCAL as i32) {
        return Err(error);
    }
    let mut pid = 0u32;
    // SAFETY: raw is a connected pipe server handle; pid is a writable out pointer.
    if unsafe { GetNamedPipeClientProcessId(raw, &mut pid) } == 0 {
        return Err(last_error());
    }
    if pid == 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "named-pipe client process is unknown, so it cannot be a local process",
        ));
    }
    Ok(())
}

/// Render SID bytes as the familiar `S-1-<authority>-<subauthorities>` form.
fn render_sid(bytes: &[u8]) -> io::Result<String> {
    if bytes.len() < 8 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "a SID has a 8-byte header",
        ));
    }
    let revision = bytes[0];
    let count = bytes[1] as usize;
    if bytes.len() < 8 + count * 4 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "a SID is shorter than its sub-authority count",
        ));
    }
    let mut authority = 0u64;
    for byte in &bytes[2..8] {
        authority = (authority << 8) | u64::from(*byte);
    }
    let mut rendered = format!("S-{revision}-{authority}");
    for index in 0..count {
        let start = 8 + index * 4;
        let value = u32::from_le_bytes([
            bytes[start],
            bytes[start + 1],
            bytes[start + 2],
            bytes[start + 3],
        ]);
        rendered.push_str(&format!("-{value}"));
    }
    Ok(rendered)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sid_render_matches_the_whoami_form() {
        let expected = String::from_utf8(
            std::process::Command::new("whoami")
                .arg("/user")
                .output()
                .expect("run whoami")
                .stdout,
        )
        .expect("whoami output is UTF-8");
        let expected = expected
            .split_whitespace()
            .find(|token| token.starts_with("S-1-"))
            .expect("whoami /user names the SID");
        assert_eq!(current_user_sid_string().unwrap(), expected);
    }

    #[cfg(feature = "tokio")]
    #[test]
    fn owner_only_security_hands_out_a_usable_pointer() {
        let mut security = OwnerOnlySecurity::new().expect("build the owner-only DACL");
        assert!(!security.attributes_ptr().is_null());
    }
}
