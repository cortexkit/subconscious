#![allow(unsafe_code)]

use super::{
    CapturedPeer, CodeRequirementOutcome, CodeVerificationError, ForcedFailures, ObservedPeer,
    PeerObservation, PeerUser, VerifiedCode,
};
use std::{
    ffi::{c_char, c_void, CStr, OsString},
    mem::size_of,
    os::{
        fd::{AsRawFd, BorrowedFd},
        unix::ffi::OsStringExt,
    },
    path::PathBuf,
    ptr::null,
    sync::OnceLock,
};

type CFTypeRef = *const c_void;
type CFIndex = isize;
type OSStatus = i32;

const ERR_SEC_SUCCESS: OSStatus = 0;
const SEC_CS_DEFAULT_FLAGS: u32 = 0;
const CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;

/// Darwin's `audit_token_t`: eight 32-bit words naming one process
/// incarnation (bsm/audit.h).
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(super) struct AuditToken {
    val: [u32; 8],
}

#[link(name = "bsm")]
extern "C" {
    fn audit_token_to_euid(token: AuditToken) -> libc::uid_t;
    fn audit_token_to_pid(token: AuditToken) -> libc::pid_t;
}

/// The Core Foundation and Security entry points used here, loaded on first
/// use instead of linked. Every program linking this crate (module SDKs and
/// test fixtures included) would otherwise load both frameworks at startup,
/// and Core Foundation's initialization adds `__CF_USER_TEXT_ENCODING` to the
/// process environment, which then reaches every child it starts.
struct Api {
    cf_data_create: unsafe extern "C" fn(CFTypeRef, *const u8, CFIndex) -> CFTypeRef,
    cf_dictionary_create: unsafe extern "C" fn(
        CFTypeRef,
        *const CFTypeRef,
        *const CFTypeRef,
        CFIndex,
        *const c_void,
        *const c_void,
    ) -> CFTypeRef,
    cf_dictionary_get_value: unsafe extern "C" fn(CFTypeRef, CFTypeRef) -> CFTypeRef,
    cf_get_type_id: unsafe extern "C" fn(CFTypeRef) -> usize,
    cf_string_get_type_id: unsafe extern "C" fn() -> usize,
    cf_dictionary_get_type_id: unsafe extern "C" fn() -> usize,
    cf_number_get_type_id: unsafe extern "C" fn() -> usize,
    cf_number_get_value: unsafe extern "C" fn(CFTypeRef, CFIndex, *mut c_void) -> u8,
    cf_string_create_with_bytes:
        unsafe extern "C" fn(CFTypeRef, *const u8, CFIndex, u32, u8) -> CFTypeRef,
    cf_string_get_length: unsafe extern "C" fn(CFTypeRef) -> CFIndex,
    cf_string_get_maximum_size_for_encoding: unsafe extern "C" fn(CFIndex, u32) -> CFIndex,
    cf_string_get_c_string: unsafe extern "C" fn(CFTypeRef, *mut c_char, CFIndex, u32) -> u8,
    cf_url_get_file_system_representation:
        unsafe extern "C" fn(CFTypeRef, u8, *mut u8, CFIndex) -> u8,
    cf_release: unsafe extern "C" fn(CFTypeRef),
    /// `&kCFTypeDictionaryKeyCallBacks` and `&kCFTypeDictionaryValueCallBacks`.
    dictionary_key_callbacks: *const c_void,
    dictionary_value_callbacks: *const c_void,
    sec_code_copy_guest_with_attributes:
        unsafe extern "C" fn(CFTypeRef, CFTypeRef, u32, *mut CFTypeRef) -> OSStatus,
    sec_code_copy_path: unsafe extern "C" fn(CFTypeRef, u32, *mut CFTypeRef) -> OSStatus,
    sec_code_check_validity: unsafe extern "C" fn(CFTypeRef, u32, CFTypeRef) -> OSStatus,
    sec_requirement_create_with_string:
        unsafe extern "C" fn(CFTypeRef, u32, *mut CFTypeRef, *mut CFTypeRef) -> OSStatus,
    sec_code_copy_signing_information:
        unsafe extern "C" fn(CFTypeRef, u32, *mut CFTypeRef) -> OSStatus,
    /// The values of `kSecGuestAttributeAudit` and `kSecCodeInfoIdentifier`.
    guest_attribute_audit: CFTypeRef,
    code_info_identifier: CFTypeRef,
    code_info_plist: CFTypeRef,
    code_info_flags: CFTypeRef,
}

// SAFETY: the fields are function pointers and pointers to immutable framework
// constants. The frameworks are never unloaded, so all stay valid for the life
// of the process and may be used from any thread.
unsafe impl Send for Api {}
unsafe impl Sync for Api {}

/// The loaded frameworks, or `None` if either could not be loaded.
fn api() -> Option<&'static Api> {
    static API: OnceLock<Option<Api>> = OnceLock::new();
    API.get_or_init(load).as_ref()
}

fn load() -> Option<Api> {
    let cf = open(c"/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation")?;
    let security = open(c"/System/Library/Frameworks/Security.framework/Security")?;
    // SAFETY: each function is given the C signature its framework header
    // declares, and each constant is read as the type it is declared with.
    unsafe {
        Some(Api {
            cf_data_create: function(cf, c"CFDataCreate")?,
            cf_dictionary_create: function(cf, c"CFDictionaryCreate")?,
            cf_dictionary_get_value: function(cf, c"CFDictionaryGetValue")?,
            cf_get_type_id: function(cf, c"CFGetTypeID")?,
            cf_string_get_type_id: function(cf, c"CFStringGetTypeID")?,
            cf_dictionary_get_type_id: function(cf, c"CFDictionaryGetTypeID")?,
            cf_number_get_type_id: function(cf, c"CFNumberGetTypeID")?,
            cf_number_get_value: function(cf, c"CFNumberGetValue")?,
            cf_string_create_with_bytes: function(cf, c"CFStringCreateWithBytes")?,
            cf_string_get_length: function(cf, c"CFStringGetLength")?,
            cf_string_get_maximum_size_for_encoding: function(
                cf,
                c"CFStringGetMaximumSizeForEncoding",
            )?,
            cf_string_get_c_string: function(cf, c"CFStringGetCString")?,
            cf_url_get_file_system_representation: function(
                cf,
                c"CFURLGetFileSystemRepresentation",
            )?,
            cf_release: function(cf, c"CFRelease")?,
            dictionary_key_callbacks: symbol(cf, c"kCFTypeDictionaryKeyCallBacks")?,
            dictionary_value_callbacks: symbol(cf, c"kCFTypeDictionaryValueCallBacks")?,
            sec_code_copy_guest_with_attributes: function(
                security,
                c"SecCodeCopyGuestWithAttributes",
            )?,
            sec_code_copy_path: function(security, c"SecCodeCopyPath")?,
            sec_code_check_validity: function(security, c"SecCodeCheckValidity")?,
            sec_requirement_create_with_string: function(
                security,
                c"SecRequirementCreateWithStringAndErrors",
            )?,
            sec_code_copy_signing_information: function(
                security,
                c"SecCodeCopySigningInformation",
            )?,
            guest_attribute_audit: constant(security, c"kSecGuestAttributeAudit")?,
            code_info_identifier: constant(security, c"kSecCodeInfoIdentifier")?,
            code_info_plist: constant(security, c"kSecCodeInfoPList")?,
            code_info_flags: constant(security, c"kSecCodeInfoFlags")?,
        })
    }
}

fn open(path: &CStr) -> Option<*mut c_void> {
    // SAFETY: path is NUL-terminated. The handle is deliberately never closed.
    let library = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL) };
    (!library.is_null()).then_some(library)
}

/// The address of `name` in `library`.
///
/// # Safety
/// `library` is a live handle from `dlopen`.
unsafe fn symbol(library: *mut c_void, name: &CStr) -> Option<*const c_void> {
    let address = libc::dlsym(library, name.as_ptr());
    (!address.is_null()).then_some(address.cast_const())
}

/// The function `name` in `library`, as the function pointer type `F`.
///
/// # Safety
/// `library` is a live handle from `dlopen`, and `F` is an `extern "C"`
/// function pointer type matching the function's C declaration.
unsafe fn function<F: Copy>(library: *mut c_void, name: &CStr) -> Option<F> {
    const { assert!(size_of::<F>() == size_of::<*const c_void>()) };
    symbol(library, name).map(|address| std::mem::transmute_copy(&address))
}

/// The value of the `CFTypeRef` constant `name` in `library`.
///
/// # Safety
/// `library` is a live handle from `dlopen`, and `name` is declared as a
/// `CFTypeRef` (or a CF type such as `CFStringRef`) variable.
unsafe fn constant(library: *mut c_void, name: &CStr) -> Option<CFTypeRef> {
    let value = *symbol(library, name)?.cast::<CFTypeRef>();
    (!value.is_null()).then_some(value)
}

/// One owned reference to a Core Foundation object, released on drop.
struct Owned {
    value: CFTypeRef,
    api: &'static Api,
}

impl Owned {
    /// Takes ownership of a reference returned by a Create or Copy call.
    fn new(api: &'static Api, value: CFTypeRef) -> Option<Self> {
        (!value.is_null()).then(|| Self { value, api })
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: the reference came from a Create or Copy call and is
        // released exactly once, here.
        unsafe { (self.api.cf_release)(self.value) };
    }
}

pub(super) fn observe(socket: BorrowedFd<'_>, forced: ForcedFailures) -> ObservedPeer {
    capture(socket, forced).observation
}

pub(super) fn capture(socket: BorrowedFd<'_>, forced: ForcedFailures) -> CapturedPeer {
    let Some(token) = audit_token(socket).filter(|_| !forced.user) else {
        return ObservedPeer::Unobserved.into();
    };
    // SAFETY: both functions read fields of the token passed by value.
    let (uid, pid) = unsafe { (audit_token_to_euid(token), audit_token_to_pid(token)) };
    let Ok(pid) = u32::try_from(pid) else {
        return ObservedPeer::Unobserved.into();
    };
    let guest = guest_code(token);
    let exe = guest.as_ref().filter(|_| !forced.exe).and_then(code_path);
    let signing_id = guest
        .as_ref()
        .filter(|code| !forced.signing_id && valid(code))
        .and_then(signing_identifier);
    CapturedPeer {
        observation: ObservedPeer::Observed(PeerObservation {
            user: PeerUser::Uid(uid),
            pid,
            exe,
            signing_id,
        }),
        token: Some(token),
    }
}

/// The audit token the kernel recorded for the peer when it connected.
fn audit_token(socket: BorrowedFd<'_>) -> Option<AuditToken> {
    let mut token = AuditToken::default();
    let mut length = size_of::<AuditToken>() as libc::socklen_t;
    // SAFETY: token and length are live, writable and sized for the
    // audit_token_t this option returns; the socket is borrowed for the call.
    let status = unsafe {
        libc::getsockopt(
            socket.as_raw_fd(),
            libc::SOL_LOCAL,
            libc::LOCAL_PEERTOKEN,
            (&mut token as *mut AuditToken).cast(),
            &mut length,
        )
    };
    (status == 0 && length as usize == size_of::<AuditToken>()).then_some(token)
}

/// The running code the audit token names, looked up by the token rather than
/// the pid so it cannot resolve to a later process reusing that pid.
fn guest_code(token: AuditToken) -> Option<Owned> {
    let api = api()?;
    // SAFETY: the token is read for its declared size; CFDataCreate copies it.
    let data = Owned::new(api, unsafe {
        (api.cf_data_create)(
            null(),
            (&token as *const AuditToken).cast(),
            size_of::<AuditToken>() as CFIndex,
        )
    })?;
    let keys = [api.guest_attribute_audit];
    let values = [data.value];
    // SAFETY: one key and one value, both live CF objects; the standard
    // callbacks retain them, and the framework's callback tables outlive the
    // call.
    let attributes = Owned::new(api, unsafe {
        (api.cf_dictionary_create)(
            null(),
            keys.as_ptr(),
            values.as_ptr(),
            1,
            api.dictionary_key_callbacks,
            api.dictionary_value_callbacks,
        )
    })?;
    let mut guest = null();
    // SAFETY: a null host means the system's code host; attributes is a live
    // dictionary and guest a writable out-pointer.
    let status = unsafe {
        (api.sec_code_copy_guest_with_attributes)(
            null(),
            attributes.value,
            SEC_CS_DEFAULT_FLAGS,
            &mut guest,
        )
    };
    let guest = Owned::new(api, guest);
    (status == ERR_SEC_SUCCESS).then_some(guest).flatten()
}

/// Where the guest's code lives on disk, as `SecCodeCopyPath` reports it.
fn code_path(code: &Owned) -> Option<PathBuf> {
    let api = code.api;
    let mut url = null();
    // SAFETY: code is a live SecCode, which this call accepts in place of a
    // static code; url is a writable out-pointer.
    let status = unsafe { (api.sec_code_copy_path)(code.value, SEC_CS_DEFAULT_FLAGS, &mut url) };
    let url = Owned::new(api, url);
    let url = (status == ERR_SEC_SUCCESS).then_some(url).flatten()?;
    let mut buffer = vec![0u8; libc::PATH_MAX as usize + 1];
    // SAFETY: url is a live CFURL and buffer is writable for its length.
    let converted = unsafe {
        (api.cf_url_get_file_system_representation)(
            url.value,
            1,
            buffer.as_mut_ptr(),
            buffer.len() as CFIndex,
        )
    };
    if converted == 0 {
        return None;
    }
    let path = CStr::from_bytes_until_nul(&buffer).ok()?.to_bytes();
    (!path.is_empty()).then(|| PathBuf::from(OsString::from_vec(path.to_vec())))
}

/// Whether the guest's signature is intact, checked against no requirement.
/// Unsigned code and code whose signature is broken fail.
fn valid(code: &Owned) -> bool {
    // SAFETY: code is a live SecCode; a null requirement checks validity only.
    unsafe {
        (code.api.sec_code_check_validity)(code.value, SEC_CS_DEFAULT_FLAGS, null())
            == ERR_SEC_SUCCESS
    }
}

pub(super) fn check_requirement(
    token: Option<AuditToken>,
    requirement: &str,
) -> CodeRequirementOutcome {
    use CodeRequirementOutcome::{NotSatisfied, Satisfied, Unverifiable};
    use CodeVerificationError::*;
    let Some(token) = token else {
        return Unverifiable(MissingAuditToken);
    };
    let Some(api) = api() else {
        return Unverifiable(FrameworkUnavailable);
    };
    let Some(text) = make_string(api, requirement) else {
        return Unverifiable(InvalidRequirement);
    };
    let mut requirement = null();
    // SAFETY: text is a live CFString; both outputs are writable and any
    // returned Create references are released below, including on parse errors.
    let mut errors = null();
    let status = unsafe {
        (api.sec_requirement_create_with_string)(text.value, 0, &mut errors, &mut requirement)
    };
    let _errors = Owned::new(api, errors);
    let requirement = Owned::new(api, requirement);
    let Some(requirement) = requirement.filter(|_| status == ERR_SEC_SUCCESS) else {
        return Unverifiable(InvalidRequirement);
    };
    let Some(code) = guest_code(token) else {
        return Unverifiable(GuestUnavailable);
    };
    // SAFETY: the guest is resolved from the captured audit token, and both
    // references remain live for the entire check.
    let status = unsafe { (api.sec_code_check_validity)(code.value, 0, requirement.value) };
    if status != ERR_SEC_SUCCESS {
        return validity_failure(status);
    }
    let mut information = null();
    // Copy the sealed plist and flags from the same validated guest.
    // SAFETY: code is live and information is a writable out-pointer.
    let status =
        unsafe { (api.sec_code_copy_signing_information)(code.value, 1 << 1, &mut information) };
    let information = Owned::new(api, information);
    let Some(information) = information.filter(|_| status == ERR_SEC_SUCCESS) else {
        return Unverifiable(SigningInformationUnavailable);
    };
    // Hardened runtime is a CodeDirectory flag, not a requirement-language
    // entitlement. Inspect it only in the information of the validated guest.
    let flags = dictionary_value(api, information.value, api.code_info_flags);
    // SAFETY: check the CF type before reading the number as kCFNumberSInt64Type.
    if flags.is_null() || unsafe { (api.cf_get_type_id)(flags) != (api.cf_number_get_type_id)() } {
        return Unverifiable(SigningInformationUnavailable);
    }
    let mut flags_value: i64 = 0;
    // SAFETY: the CFNumber was type-checked and the output is a live i64.
    if unsafe { (api.cf_number_get_value)(flags, 4, (&mut flags_value as *mut i64).cast()) } == 0 {
        return Unverifiable(SigningInformationUnavailable);
    }
    if flags_value & 0x1_0000 == 0 {
        return NotSatisfied;
    }
    let plist = dictionary_value(api, information.value, api.code_info_plist);
    let short_version = make_string(api, "CFBundleShortVersionString").and_then(|key| {
        let version = dictionary_value(api, plist, key.value);
        // SAFETY: check the CF type before treating the borrowed value as a string.
        if version.is_null()
            || unsafe { (api.cf_get_type_id)(version) != (api.cf_string_get_type_id)() }
        {
            None
        } else {
            cf_string(api, version)
        }
    });
    // Recheck after copying metadata: a changed sealed plist cannot lend a
    // version to code that no longer passes the requirement.
    // SAFETY: the same guest and requirement references are still live.
    let status = unsafe { (api.sec_code_check_validity)(code.value, 0, requirement.value) };
    if status != ERR_SEC_SUCCESS {
        return validity_failure(status);
    }
    Satisfied(VerifiedCode { short_version })
}

fn validity_failure(status: OSStatus) -> CodeRequirementOutcome {
    if status == -67050 {
        // errSecCSReqFailed: a definite requirement mismatch.
        CodeRequirementOutcome::NotSatisfied
    } else {
        CodeRequirementOutcome::Unverifiable(CodeVerificationError::ValidityCheckFailed(status))
    }
}

fn make_string(api: &'static Api, text: &str) -> Option<Owned> {
    // SAFETY: text is readable for its length; the Create call copies its bytes.
    Owned::new(api, unsafe {
        (api.cf_string_create_with_bytes)(
            null(),
            text.as_ptr(),
            text.len().try_into().ok()?,
            CF_STRING_ENCODING_UTF8,
            0,
        )
    })
}

fn dictionary_value(api: &Api, dictionary: CFTypeRef, key: CFTypeRef) -> CFTypeRef {
    // SAFETY: all callers supply live CF references or null. Check the type
    // before using the dictionary API; its borrowed value lives with its owner.
    unsafe {
        if dictionary.is_null()
            || (api.cf_get_type_id)(dictionary) != (api.cf_dictionary_get_type_id)()
        {
            null()
        } else {
            (api.cf_dictionary_get_value)(dictionary, key)
        }
    }
}

fn signing_identifier(code: &Owned) -> Option<String> {
    let api = code.api;
    let mut information = null();
    // SAFETY: code is a live SecCode and information a writable out-pointer.
    let status = unsafe {
        (api.sec_code_copy_signing_information)(code.value, SEC_CS_DEFAULT_FLAGS, &mut information)
    };
    let information = Owned::new(api, information);
    let information = (status == ERR_SEC_SUCCESS)
        .then_some(information)
        .flatten()?;
    // SAFETY: information is a live dictionary and the key a framework
    // constant. The value is borrowed from the dictionary, which outlives it.
    let value =
        unsafe { (api.cf_dictionary_get_value)(information.value, api.code_info_identifier) };
    // SAFETY: value is a live CF object (checked non-null first).
    if value.is_null() || unsafe { (api.cf_get_type_id)(value) != (api.cf_string_get_type_id)() } {
        return None;
    }
    cf_string(api, value)
}

fn cf_string(api: &Api, string: CFTypeRef) -> Option<String> {
    // SAFETY: string is a live CFString.
    let length = unsafe { (api.cf_string_get_length)(string) };
    // SAFETY: a pure size computation.
    let capacity =
        unsafe { (api.cf_string_get_maximum_size_for_encoding)(length, CF_STRING_ENCODING_UTF8) };
    let capacity = usize::try_from(capacity).ok()?.checked_add(1)?;
    let mut buffer = vec![0u8; capacity];
    // SAFETY: buffer is writable for its length, which includes the NUL.
    let copied = unsafe {
        (api.cf_string_get_c_string)(
            string,
            buffer.as_mut_ptr().cast(),
            buffer.len() as CFIndex,
            CF_STRING_ENCODING_UTF8,
        )
    };
    if copied == 0 {
        return None;
    }
    let text = CStr::from_bytes_until_nul(&buffer).ok()?.to_str().ok()?;
    (!text.is_empty()).then(|| text.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audit_token_layout_is_eight_words() {
        assert_eq!(size_of::<AuditToken>(), 32);
    }

    #[test]
    fn null_create_result_is_not_released() {
        assert!(Owned::new(api().expect("macOS frameworks load"), null()).is_none());
    }
}
