use super::super::{call_error, Handle};
use std::{
    io,
    mem::{size_of, zeroed},
    ptr::{addr_of, null_mut},
};
use windows_sys::Win32::{
    Foundation::{
        GetLastError, SetLastError, ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS, HANDLE, LUID,
    },
    Security::{
        AdjustTokenPrivileges, GetTokenInformation, LookupPrivilegeValueW, TokenElevation,
        TokenPrivileges, LUID_AND_ATTRIBUTES, SE_PRIVILEGE_ENABLED, SE_PRIVILEGE_REMOVED,
        TOKEN_ADJUST_PRIVILEGES, TOKEN_ELEVATION, TOKEN_PRIVILEGES, TOKEN_QUERY,
    },
    System::Threading::{GetCurrentProcess, OpenProcessToken},
};

#[derive(Clone, Copy, PartialEq)]
enum DebugPrivilege {
    Absent,
    PresentDisabled,
    Enabled,
}

impl DebugPrivilege {
    fn label(self) -> &'static str {
        match self {
            Self::Absent => "absent",
            Self::PresentDisabled => "present-disabled",
            Self::Enabled => "enabled",
        }
    }
}

struct TokenState {
    elevated: bool,
    debug: DebugPrivilege,
}

impl std::fmt::Display for TokenState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "elevated={}, SeDebugPrivilege={}",
            self.elevated,
            self.debug.label()
        )
    }
}

fn token(access: u32) -> io::Result<Handle> {
    let mut handle = null_mut();
    // SAFETY: The pseudo handle is valid and handle is a writable output.
    if unsafe { OpenProcessToken(GetCurrentProcess(), access, &mut handle) } == 0 {
        return Err(call_error(
            "OpenProcessToken(opener diagnostics)",
            io::Error::last_os_error(),
        ));
    }
    Ok(Handle(handle))
}

fn debug_luid() -> io::Result<LUID> {
    let name: Vec<u16> = "SeDebugPrivilege".encode_utf16().chain(Some(0)).collect();
    // SAFETY: LUID is a plain Win32 output structure.
    let mut luid = unsafe { zeroed() };
    // SAFETY: The privilege name is terminated and luid is a writable output.
    if unsafe { LookupPrivilegeValueW(null_mut(), name.as_ptr(), &mut luid) } == 0 {
        return Err(call_error(
            "LookupPrivilegeValueW(SeDebugPrivilege)",
            io::Error::last_os_error(),
        ));
    }
    Ok(luid)
}

fn debug_state(token: HANDLE, debug: LUID) -> io::Result<DebugPrivilege> {
    let mut bytes = 0;
    // SAFETY: A null buffer requests the variable-length token information size.
    let probe = unsafe { GetTokenInformation(token, TokenPrivileges, null_mut(), 0, &mut bytes) };
    let error = io::Error::last_os_error();
    if probe != 0 || error.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32) || bytes < 4 {
        return Err(call_error(
            "GetTokenInformation(TokenPrivileges size)",
            error,
        ));
    }
    let mut buffer = vec![0usize; (bytes as usize).div_ceil(size_of::<usize>())];
    // SAFETY: The aligned output buffer has at least bytes bytes.
    if unsafe {
        GetTokenInformation(
            token,
            TokenPrivileges,
            buffer.as_mut_ptr().cast(),
            bytes,
            &mut bytes,
        )
    } == 0
    {
        return Err(call_error(
            "GetTokenInformation(TokenPrivileges)",
            io::Error::last_os_error(),
        ));
    }
    let privileges = buffer.as_ptr().cast::<TOKEN_PRIVILEGES>();
    // SAFETY: GetTokenInformation initialized the count and trailing array. The
    // count is bounded against the returned byte length before reading entries.
    let count = unsafe { (*privileges).PrivilegeCount } as usize;
    let offset = std::mem::offset_of!(TOKEN_PRIVILEGES, Privileges);
    if count > (bytes as usize - offset) / size_of::<LUID_AND_ATTRIBUTES>() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "GetTokenInformation(TokenPrivileges): truncated entries",
        ));
    }
    // SAFETY: The checked array lies inside the aligned, initialized buffer.
    let entries = unsafe {
        std::slice::from_raw_parts(
            addr_of!((*privileges).Privileges).cast::<LUID_AND_ATTRIBUTES>(),
            count,
        )
    };
    Ok(entries
        .iter()
        .find(|entry| entry.Luid.LowPart == debug.LowPart && entry.Luid.HighPart == debug.HighPart)
        .map_or(DebugPrivilege::Absent, |entry| {
            if entry.Attributes & SE_PRIVILEGE_ENABLED != 0 {
                DebugPrivilege::Enabled
            } else {
                DebugPrivilege::PresentDisabled
            }
        }))
}

fn state(token: HANDLE, debug: LUID) -> io::Result<TokenState> {
    // SAFETY: TOKEN_ELEVATION holds a single u32, for which all-zero bytes are
    // a valid value; GetTokenInformation overwrites it.
    let mut elevation: TOKEN_ELEVATION = unsafe { zeroed() };
    let mut bytes = 0;
    // SAFETY: The fixed-size output and returned length are writable locals.
    if unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            (&mut elevation as *mut TOKEN_ELEVATION).cast(),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut bytes,
        )
    } == 0
    {
        return Err(call_error(
            "GetTokenInformation(TokenElevation)",
            io::Error::last_os_error(),
        ));
    }
    Ok(TokenState {
        elevated: elevation.TokenIsElevated != 0,
        debug: debug_state(token, debug)?,
    })
}

/// Describe the caller without changing its token.
pub(super) fn current_state() -> io::Result<String> {
    let token = token(TOKEN_QUERY)?;
    Ok(state(token.0, debug_luid()?)?.to_string())
}

/// Remove SeDebugPrivilege from this process's token. Only the short-lived
/// opener child calls this, so the test process keeps its own token. An enabled
/// SeDebugPrivilege (as on elevated CI runners) makes OpenProcess ignore the
/// target's DACL, so the denial checks would pass or fail regardless of the
/// DACL under test. The before and after states are returned for diagnostics.
pub(super) fn remove_debug_privilege() -> io::Result<String> {
    let token = token(TOKEN_QUERY | TOKEN_ADJUST_PRIVILEGES)?;
    let debug = debug_luid()?;
    let before = state(token.0, debug)?;
    // An ordinary user's token may already lack this privilege. In that case
    // request zero changes rather than treating absence as a removal failure.
    let privileges = TOKEN_PRIVILEGES {
        PrivilegeCount: u32::from(before.debug != DebugPrivilege::Absent),
        Privileges: [LUID_AND_ATTRIBUTES {
            Luid: debug,
            Attributes: SE_PRIVILEGE_REMOVED,
        }],
    };
    // SAFETY: SetLastError changes only this helper thread's Win32 error state.
    unsafe { SetLastError(ERROR_SUCCESS) };
    // SAFETY: The token has adjust rights and the initialized structure contains
    // either one removal request or zero requests when debug privilege is absent.
    let adjusted =
        unsafe { AdjustTokenPrivileges(token.0, 0, &privileges, 0, null_mut(), null_mut()) };
    // SAFETY: Read the result immediately; AdjustTokenPrivileges can return true
    // while setting ERROR_NOT_ALL_ASSIGNED, which must also fail this test.
    let error = unsafe { GetLastError() };
    if adjusted == 0 || error != ERROR_SUCCESS {
        return Err(call_error(
            "AdjustTokenPrivileges(SE_PRIVILEGE_REMOVED, SeDebugPrivilege)",
            io::Error::from_raw_os_error(error as i32),
        ));
    }
    let after = state(token.0, debug)?;
    if after.debug != DebugPrivilege::Absent {
        return Err(io::Error::other(format!("AdjustTokenPrivileges(SE_PRIVILEGE_REMOVED): SeDebugPrivilege was not removed; before [{before}], after [{after}]")));
    }
    Ok(format!("before [{before}], after [{after}]"))
}
