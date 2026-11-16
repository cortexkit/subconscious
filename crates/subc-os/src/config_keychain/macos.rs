#![allow(unsafe_code)]

// The data protection keychain restricts the sealing key to the daemon's
// provisioned access group. File-keychain ACLs cannot prevent another program
// from replacing or deleting the item, so the old login-keychain item is never
// queried here.
use super::{requirement, AnchorState, KeychainError, KeychainState, StoreKey};
use std::{
    ffi::{c_char, c_void, CStr},
    mem::size_of,
    ptr::{null, null_mut},
    sync::OnceLock,
};

type Ref = *const c_void;
type Status = i32;

struct Api {
    release: unsafe extern "C" fn(Ref),
    type_id: unsafe extern "C" fn(Ref) -> usize,
    data_type: unsafe extern "C" fn() -> usize,
    data_create: unsafe extern "C" fn(Ref, *const u8, isize) -> Ref,
    data_len: unsafe extern "C" fn(Ref) -> isize,
    data_ptr: unsafe extern "C" fn(Ref) -> *const u8,
    string_type: unsafe extern "C" fn() -> usize,
    string_create: unsafe extern "C" fn(Ref, *const u8, isize, u32, u8) -> Ref,
    string_c: unsafe extern "C" fn(Ref, *mut c_char, isize, u32) -> u8,
    array_type: unsafe extern "C" fn() -> usize,
    array_len: unsafe extern "C" fn(Ref) -> isize,
    dict_create: unsafe extern "C" fn(
        Ref,
        *const Ref,
        *const Ref,
        isize,
        *const c_void,
        *const c_void,
    ) -> Ref,
    dict_type: unsafe extern "C" fn() -> usize,
    dict_get: unsafe extern "C" fn(Ref, Ref) -> Ref,
    number_type: unsafe extern "C" fn() -> usize,
    number_get: unsafe extern "C" fn(Ref, isize, *mut c_void) -> u8,
    code_self: unsafe extern "C" fn(u32, *mut Ref) -> Status,
    code_valid: unsafe extern "C" fn(Ref, u32, Ref) -> Status,
    code_info: unsafe extern "C" fn(Ref, u32, *mut Ref) -> Status,
    code_requirement: unsafe extern "C" fn(Ref, u32, *mut Ref) -> Status,
    requirement_data: unsafe extern "C" fn(Ref, u32, *mut Ref) -> Status,
    item_add: unsafe extern "C" fn(Ref, *mut Ref) -> Status,
    item_copy: unsafe extern "C" fn(Ref, *mut Ref) -> Status,
    item_update: unsafe extern "C" fn(Ref, Ref) -> Status,
    item_delete: unsafe extern "C" fn(Ref) -> Status,
    key_callbacks: *const c_void,
    value_callbacks: *const c_void,
    true_value: Ref,
    false_value: Ref,
    class: Ref,
    generic_password: Ref,
    service: Ref,
    account: Ref,
    synchronizable: Ref,
    access_group: Ref,
    accessible: Ref,
    after_first_unlock_this_device_only: Ref,
    value_data: Ref,
    return_data: Ref,
    use_ui: Ref,
    fail_ui: Ref,
    use_data_protection_keychain: Ref,
    team: Ref,
    identifier: Ref,
    flags: Ref,
    certificates: Ref,
}

// SAFETY: only function pointers and immutable framework constants; their
// dlopen handles are never closed, and these APIs support concurrent callers.
unsafe impl Send for Api {}
unsafe impl Sync for Api {}

fn api() -> Result<&'static Api, KeychainError> {
    static API: OnceLock<Option<Api>> = OnceLock::new();
    API.get_or_init(load)
        .as_ref()
        .ok_or(KeychainError::Unavailable)
}

fn load() -> Option<Api> {
    let cf = open(c"/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation")?;
    let sec = open(c"/System/Library/Frameworks/Security.framework/Security")?;
    // SAFETY: signatures match the public CoreFoundation/Security headers.
    // Constants are either callback-struct addresses or CFTypeRef variables.
    unsafe {
        Some(Api {
            release: function(cf, c"CFRelease")?,
            type_id: function(cf, c"CFGetTypeID")?,
            data_type: function(cf, c"CFDataGetTypeID")?,
            data_create: function(cf, c"CFDataCreate")?,
            data_len: function(cf, c"CFDataGetLength")?,
            data_ptr: function(cf, c"CFDataGetBytePtr")?,
            string_type: function(cf, c"CFStringGetTypeID")?,
            string_create: function(cf, c"CFStringCreateWithBytes")?,
            string_c: function(cf, c"CFStringGetCString")?,
            array_type: function(cf, c"CFArrayGetTypeID")?,
            array_len: function(cf, c"CFArrayGetCount")?,
            dict_create: function(cf, c"CFDictionaryCreate")?,
            dict_type: function(cf, c"CFDictionaryGetTypeID")?,
            dict_get: function(cf, c"CFDictionaryGetValue")?,
            number_type: function(cf, c"CFNumberGetTypeID")?,
            number_get: function(cf, c"CFNumberGetValue")?,
            code_self: function(sec, c"SecCodeCopySelf")?,
            code_valid: function(sec, c"SecCodeCheckValidity")?,
            code_info: function(sec, c"SecCodeCopySigningInformation")?,
            code_requirement: function(sec, c"SecCodeCopyDesignatedRequirement")?,
            requirement_data: function(sec, c"SecRequirementCopyData")?,
            item_add: function(sec, c"SecItemAdd")?,
            item_copy: function(sec, c"SecItemCopyMatching")?,
            item_update: function(sec, c"SecItemUpdate")?,
            item_delete: function(sec, c"SecItemDelete")?,
            key_callbacks: symbol(cf, c"kCFTypeDictionaryKeyCallBacks")?,
            value_callbacks: symbol(cf, c"kCFTypeDictionaryValueCallBacks")?,
            true_value: constant(cf, c"kCFBooleanTrue")?,
            false_value: constant(cf, c"kCFBooleanFalse")?,
            class: constant(sec, c"kSecClass")?,
            generic_password: constant(sec, c"kSecClassGenericPassword")?,
            service: constant(sec, c"kSecAttrService")?,
            account: constant(sec, c"kSecAttrAccount")?,
            synchronizable: constant(sec, c"kSecAttrSynchronizable")?,
            access_group: constant(sec, c"kSecAttrAccessGroup")?,
            accessible: constant(sec, c"kSecAttrAccessible")?,
            after_first_unlock_this_device_only: constant(
                sec,
                c"kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly",
            )?,
            value_data: constant(sec, c"kSecValueData")?,
            return_data: constant(sec, c"kSecReturnData")?,
            use_ui: constant(sec, c"kSecUseAuthenticationUI")?,
            fail_ui: constant(sec, c"kSecUseAuthenticationUIFail")?,
            use_data_protection_keychain: constant(sec, c"kSecUseDataProtectionKeychain")?,
            team: constant(sec, c"kSecCodeInfoTeamIdentifier")?,
            identifier: constant(sec, c"kSecCodeInfoIdentifier")?,
            flags: constant(sec, c"kSecCodeInfoFlags")?,
            certificates: constant(sec, c"kSecCodeInfoCertificates")?,
        })
    }
}

fn open(path: &CStr) -> Option<*mut c_void> {
    // SAFETY: path is NUL-terminated; the framework remains loaded forever.
    let handle = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL) };
    (!handle.is_null()).then_some(handle)
}

// SAFETY: callers provide a live dlopen handle and NUL-terminated symbol name.
unsafe fn symbol(handle: *mut c_void, name: &CStr) -> Option<*const c_void> {
    let address = libc::dlsym(handle, name.as_ptr());
    (!address.is_null()).then_some(address.cast_const())
}

// SAFETY: F must be the public C function-pointer signature of name.
unsafe fn function<F: Copy>(handle: *mut c_void, name: &CStr) -> Option<F> {
    const { assert!(size_of::<F>() == size_of::<*const c_void>()) };
    symbol(handle, name).map(|address| std::mem::transmute_copy(&address))
}

// SAFETY: name must be a public CFTypeRef-valued framework constant.
unsafe fn constant(handle: *mut c_void, name: &CStr) -> Option<Ref> {
    let value = *symbol(handle, name)?.cast::<Ref>();
    (!value.is_null()).then_some(value)
}

struct Owned {
    value: Ref,
    api: &'static Api,
}

impl Owned {
    fn new(api: &'static Api, value: Ref) -> Result<Self, KeychainError> {
        if value.is_null() {
            Err(KeychainError::Unavailable)
        } else {
            Ok(Self { value, api })
        }
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: each Owned takes exactly one Create/Copy reference.
        unsafe { (self.api.release)(self.value) };
    }
}

fn status(value: Status) -> Result<(), KeychainError> {
    match value {
        0 => Ok(()),
        -25299 => Err(KeychainError::AlreadyExists),
        -25300 => Err(KeychainError::Missing),
        -25293 | -25308 | -128 | -25294 | -25315 => Err(KeychainError::Denied),
        -25291 => Err(KeychainError::Unavailable),
        -34018 => Err(KeychainError::NotEntitled),
        other => Err(KeychainError::OsStatus(other)),
    }
}

impl Api {
    fn copied(
        &'static self,
        call: impl FnOnce(*mut Ref) -> Status,
    ) -> Result<Owned, KeychainError> {
        let mut value = null();
        let result = call(&mut value);
        let owned = Owned::new(self, value);
        // Release a returned reference even if the operation also reports failure.
        status(result)?;
        owned
    }

    fn string(&'static self, value: &str) -> Result<Owned, KeychainError> {
        // SAFETY: UTF-8 bytes remain live throughout the copying Create call.
        Owned::new(self, unsafe {
            (self.string_create)(null(), value.as_ptr(), value.len() as isize, 0x0800_0100, 0)
        })
    }

    fn dictionary(&'static self, pairs: &[(Ref, Ref)]) -> Result<Owned, KeychainError> {
        let keys: Vec<_> = pairs.iter().map(|p| p.0).collect();
        let values: Vec<_> = pairs.iter().map(|p| p.1).collect();
        // SAFETY: equal-length live key/value arrays; callbacks retain all objects.
        Owned::new(self, unsafe {
            (self.dict_create)(
                null(),
                keys.as_ptr(),
                values.as_ptr(),
                keys.len() as isize,
                self.key_callbacks,
                self.value_callbacks,
            )
        })
    }

    fn dict_value(&self, dictionary: &Owned, key: Ref) -> Ref {
        // SAFETY: validate the type before calling a CFDictionary function.
        unsafe {
            if (self.type_id)(dictionary.value) != (self.dict_type)() {
                return null();
            }
            (self.dict_get)(dictionary.value, key)
        }
    }

    fn text(&self, value: Ref) -> Result<String, KeychainError> {
        // Signing identifiers are bounded here so malformed metadata cannot
        // force unbounded allocation. CFStringGetCString fails on overflow.
        let mut bytes = [0u8; 4096];
        // SAFETY: borrowed value is type-checked; bytes is a writable C buffer.
        unsafe {
            if value.is_null()
                || (self.type_id)(value) != (self.string_type)()
                || (self.string_c)(
                    value,
                    bytes.as_mut_ptr().cast(),
                    bytes.len() as isize,
                    0x0800_0100,
                ) == 0
            {
                return Err(KeychainError::Unavailable);
            }
        }
        let end = bytes
            .iter()
            .position(|byte| *byte == 0)
            .ok_or(KeychainError::Unavailable)?;
        if end == 0 {
            return Err(KeychainError::Unavailable);
        }
        String::from_utf8(bytes[..end].to_vec()).map_err(|_| KeychainError::Unavailable)
    }

    fn data<'a>(&self, object: &'a Owned) -> Result<&'a [u8], KeychainError> {
        // SAFETY: validate CFData, length and pointer before borrowing its bytes;
        // the returned slice cannot outlive the owning reference.
        unsafe {
            if (self.type_id)(object.value) != (self.data_type)() {
                return Err(KeychainError::InvalidData);
            }
            let length = (self.data_len)(object.value);
            let bytes = (self.data_ptr)(object.value);
            if !(1..=4096).contains(&length) || bytes.is_null() {
                return Err(KeychainError::InvalidData);
            }
            Ok(std::slice::from_raw_parts(bytes, length as usize))
        }
    }

    fn signing_identity(&'static self) -> Result<(), KeychainError> {
        // SAFETY: each Copy out-pointer is valid and adopted by copied(). A
        // null path is never used to inspect an on-disk replacement image.
        let code = self.copied(|out| unsafe { (self.code_self)(0, out) })?;
        let info = match self.copied(|out| unsafe { (self.code_info)(code.value, 1 << 1, out) }) {
            Err(KeychainError::OsStatus(-67062)) => return Err(KeychainError::Unsupported),
            other => other?,
        };
        let flags = self.dict_value(&info, self.flags);
        let mut value: i64 = 0;
        // SAFETY: type-check the number and read kCFNumberSInt64Type into i64.
        if flags.is_null()
            || unsafe {
                (self.type_id)(flags) != (self.number_type)()
                    || (self.number_get)(flags, 4, (&mut value as *mut i64).cast()) == 0
            }
        {
            return Err(KeychainError::Unavailable);
        }
        if value & 2 != 0 {
            return Err(KeychainError::Unsupported);
        }
        let team = self.dict_value(&info, self.team);
        let certificates = self.dict_value(&info, self.certificates);
        if team.is_null() || certificates.is_null() {
            return Err(KeychainError::Unsupported);
        }
        // SAFETY: validate the certificate array before counting it.
        if unsafe {
            (self.type_id)(certificates) != (self.array_type)()
                || (self.array_len)(certificates) == 0
        } {
            return Err(KeychainError::Unsupported);
        }
        let team = self.text(team)?;
        let identifier = self.text(self.dict_value(&info, self.identifier))?;
        let requirement =
            self.copied(|out| unsafe { (self.code_requirement)(code.value, 0, out) })?;
        let data =
            self.copied(|out| unsafe { (self.requirement_data)(requirement.value, 0, out) })?;
        if !requirement::ordinary(self.data(&data)?, &identifier, &team) {
            return Err(KeychainError::InvalidRequirement);
        }
        // SAFETY: validate the running signature against its parsed designated
        // requirement. Its Apple anchor must verify the certificate chain back
        // to Apple, rather than merely trusting the reported team metadata.
        status(unsafe { (self.code_valid)(code.value, 0, requirement.value) })
    }

    fn query(
        &'static self,
        service: Ref,
        account: Ref,
        read: bool,
        add: bool,
        extra: &[(Ref, Ref)],
    ) -> Result<Owned, KeychainError> {
        let access_group = self.string("5R5846NBPW.io.cortexkit.daemon")?;
        let mut pairs = vec![
            (self.class, self.generic_password),
            (self.service, service),
            (self.account, account),
            (self.synchronizable, self.false_value),
            (self.use_ui, self.fail_ui),
            (self.use_data_protection_keychain, self.true_value),
            (self.access_group, access_group.value),
        ];
        if add {
            pairs.push((self.accessible, self.after_first_unlock_this_device_only));
        }
        if read {
            pairs.push((self.return_data, self.true_value));
        }
        pairs.extend_from_slice(extra);
        self.dictionary(&pairs)
    }
}

pub(super) struct Store {
    api: &'static Api,
    service: Owned,
    account: Owned,
}

// SAFETY: the retained strings are immutable, SecItem supports concurrent calls,
// and the loaded API lives for the process lifetime.
// Logical read/modify/write serialization remains the caller's responsibility.
unsafe impl Send for Store {}
unsafe impl Sync for Store {}

impl Store {
    pub(super) fn open(service: &str) -> Result<Self, KeychainError> {
        let api = api()?;
        api.signing_identity()?;
        if service.is_empty() || service.len() > 1024 || service.contains('\0') {
            return Err(KeychainError::InvalidData);
        }
        Ok(Self {
            api,
            service: api.string(service)?,
            account: api.string("config-gate-v1")?,
        })
    }

    fn query(&self, read: bool, add: bool, extra: &[(Ref, Ref)]) -> Result<Owned, KeychainError> {
        self.api
            .query(self.service.value, self.account.value, read, add, extra)
    }

    pub(super) fn load(&self) -> Result<Option<KeychainState>, KeychainError> {
        let query = self.query(true, false, &[])?;
        // SAFETY: query is a retained dictionary; copied owns the result.
        match self
            .api
            .copied(|out| unsafe { (self.api.item_copy)(query.value, out) })
        {
            Err(KeychainError::Missing) => Ok(None),
            Ok(data) => KeychainState::decode(self.api.data(&data)?).map(Some),
            Err(error) => Err(error),
        }
    }

    fn encoded(&self, state: &KeychainState) -> Result<Owned, KeychainError> {
        let bytes = state.encode();
        // SAFETY: the zeroizing buffer is live throughout the copying call.
        Owned::new(self.api, unsafe {
            (self.api.data_create)(null(), bytes.as_ptr(), bytes.len() as isize)
        })
    }

    pub(super) fn create(&self, key: StoreKey) -> Result<(), KeychainError> {
        // Recheck the daemon's signing identity before adding a key, even for a long-lived handle.
        self.api.signing_identity()?;
        let data = self.encoded(&KeychainState {
            key,
            anchor: AnchorState::default(),
        })?;
        let query = self.query(false, true, &[(self.api.value_data, data.value)])?;
        // SAFETY: all inputs remain live and no result is requested.
        status(unsafe { (self.api.item_add)(query.value, null_mut()) })
    }

    pub(super) fn replace(&self, state: &KeychainState) -> Result<(), KeychainError> {
        let data = self.encoded(state)?;
        let query = self.query(false, false, &[])?;
        let attributes = self.api.dictionary(&[(self.api.value_data, data.value)])?;
        // SAFETY: live dictionaries; data-only update preserves access attributes.
        status(unsafe { (self.api.item_update)(query.value, attributes.value) })
    }

    pub(super) fn delete(&self) -> Result<(), KeychainError> {
        let query = self.query(false, false, &[])?;
        // SAFETY: unique class/service/account query, pinned to the access group.
        status(unsafe { (self.api.item_delete)(query.value) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queries_use_daemon_data_protection_access_group() {
        let a = api().expect("public Security and CoreFoundation APIs");
        let service = a.string("config-query-test").unwrap();
        let account = a.string("config-gate-v1").unwrap();
        let sec = open(c"/System/Library/Frameworks/Security.framework/Security").unwrap();
        // SAFETY: the framework is live and these are public CFTypeRef constants.
        let (file_keychain, search_list) = unsafe {
            (
                constant(sec, c"kSecUseKeychain").unwrap(),
                constant(sec, c"kSecMatchSearchList").unwrap(),
            )
        };
        for (operation, read, add) in [
            ("load", true, false),
            ("create", false, true),
            ("replace", false, false),
            ("delete", false, false),
        ] {
            let query = a
                .query(service.value, account.value, read, add, &[])
                .unwrap();
            for (key, value) in [
                (a.class, a.generic_password),
                (a.service, service.value),
                (a.account, account.value),
                (a.synchronizable, a.false_value),
                (a.use_ui, a.fail_ui),
                (a.use_data_protection_keychain, a.true_value),
            ] {
                assert_eq!(a.dict_value(&query, key), value, "{operation}");
            }
            assert_eq!(
                a.text(a.dict_value(&query, a.access_group)).unwrap(),
                "5R5846NBPW.io.cortexkit.daemon",
                "{operation}"
            );
            assert_eq!(
                a.dict_value(&query, a.accessible),
                if add {
                    a.after_first_unlock_this_device_only
                } else {
                    null()
                },
                "{operation}"
            );
            assert_eq!(
                a.dict_value(&query, a.return_data),
                if read { a.true_value } else { null() },
                "{operation}"
            );
            assert!(a.dict_value(&query, file_keychain).is_null(), "{operation}");
            assert!(a.dict_value(&query, search_list).is_null(), "{operation}");
        }
    }

    #[test]
    fn missing_entitlement_has_its_own_error_class() {
        assert!(matches!(status(-34018), Err(KeychainError::NotEntitled)));
        assert!(matches!(status(-25293), Err(KeychainError::Denied)));
        assert!(matches!(status(-25300), Err(KeychainError::Missing)));
        assert!(matches!(status(-25299), Err(KeychainError::AlreadyExists)));
        assert!(matches!(status(-25291), Err(KeychainError::Unavailable)));
        assert!(matches!(status(-1), Err(KeychainError::OsStatus(-1))));
        assert!(status(0).is_ok());
    }
}
