#![allow(unsafe_code)]

// The deprecated file-based keychain ACL APIs are the public route for a
// non-sandboxed command-line daemon without the provisioning profile needed
// for a data-protection keychain access group. No private framework interfaces
// are used here.
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
    array_create: unsafe extern "C" fn(Ref, *const Ref, isize, *const c_void) -> Ref,
    array_len: unsafe extern "C" fn(Ref) -> isize,
    array_get: unsafe extern "C" fn(Ref, isize) -> Ref,
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
    keychain_default: unsafe extern "C" fn(*mut Ref) -> Status,
    trusted_self: unsafe extern "C" fn(*const c_char, *mut Ref) -> Status,
    access_create: unsafe extern "C" fn(Ref, Ref, *mut Ref) -> Status,
    access_acls: unsafe extern "C" fn(Ref, *mut Ref) -> Status,
    acl_remove: unsafe extern "C" fn(Ref) -> Status,
    acl_create: unsafe extern "C" fn(Ref, Ref, Ref, u32, *mut Ref) -> Status,
    acl_authorizations: unsafe extern "C" fn(Ref, Ref) -> Status,
    item_add: unsafe extern "C" fn(Ref, *mut Ref) -> Status,
    item_copy: unsafe extern "C" fn(Ref, *mut Ref) -> Status,
    item_update: unsafe extern "C" fn(Ref, Ref) -> Status,
    item_delete: unsafe extern "C" fn(Ref) -> Status,
    array_callbacks: *const c_void,
    key_callbacks: *const c_void,
    value_callbacks: *const c_void,
    true_value: Ref,
    false_value: Ref,
    class: Ref,
    generic_password: Ref,
    service: Ref,
    account: Ref,
    synchronizable: Ref,
    access: Ref,
    value_data: Ref,
    return_data: Ref,
    use_ui: Ref,
    fail_ui: Ref,
    use_keychain: Ref,
    search_list: Ref,
    auth_any: Ref,
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
            array_create: function(cf, c"CFArrayCreate")?,
            array_len: function(cf, c"CFArrayGetCount")?,
            array_get: function(cf, c"CFArrayGetValueAtIndex")?,
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
            keychain_default: function(sec, c"SecKeychainCopyDefault")?,
            trusted_self: function(sec, c"SecTrustedApplicationCreateFromPath")?,
            access_create: function(sec, c"SecAccessCreate")?,
            access_acls: function(sec, c"SecAccessCopyACLList")?,
            acl_remove: function(sec, c"SecACLRemove")?,
            acl_create: function(sec, c"SecACLCreateWithSimpleContents")?,
            acl_authorizations: function(sec, c"SecACLUpdateAuthorizations")?,
            item_add: function(sec, c"SecItemAdd")?,
            item_copy: function(sec, c"SecItemCopyMatching")?,
            item_update: function(sec, c"SecItemUpdate")?,
            item_delete: function(sec, c"SecItemDelete")?,
            array_callbacks: symbol(cf, c"kCFTypeArrayCallBacks")?,
            key_callbacks: symbol(cf, c"kCFTypeDictionaryKeyCallBacks")?,
            value_callbacks: symbol(cf, c"kCFTypeDictionaryValueCallBacks")?,
            true_value: constant(cf, c"kCFBooleanTrue")?,
            false_value: constant(cf, c"kCFBooleanFalse")?,
            class: constant(sec, c"kSecClass")?,
            generic_password: constant(sec, c"kSecClassGenericPassword")?,
            service: constant(sec, c"kSecAttrService")?,
            account: constant(sec, c"kSecAttrAccount")?,
            synchronizable: constant(sec, c"kSecAttrSynchronizable")?,
            access: constant(sec, c"kSecAttrAccess")?,
            value_data: constant(sec, c"kSecValueData")?,
            return_data: constant(sec, c"kSecReturnData")?,
            use_ui: constant(sec, c"kSecUseAuthenticationUI")?,
            fail_ui: constant(sec, c"kSecUseAuthenticationUIFail")?,
            use_keychain: constant(sec, c"kSecUseKeychain")?,
            search_list: constant(sec, c"kSecMatchSearchList")?,
            auth_any: constant(sec, c"kSecACLAuthorizationAny")?,
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

    fn array(&'static self, values: &[Ref]) -> Result<Owned, KeychainError> {
        // SAFETY: callbacks retain the live CF objects in values.
        Owned::new(self, unsafe {
            (self.array_create)(
                null(),
                values.as_ptr(),
                values.len() as isize,
                self.array_callbacks,
            )
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

    fn access_for_self(&'static self) -> Result<Owned, KeychainError> {
        // SAFETY: NULL means the calling process, not any application. Security
        // captures its designated requirement, allowing same-identity upgrades.
        let app = self.copied(|out| unsafe { (self.trusted_self)(null(), out) })?;
        let apps = self.array(&[app.value])?;
        let label = self.string("Configuration store sealing key and revision anchor")?;
        let access =
            self.copied(|out| unsafe { (self.access_create)(label.value, apps.value, out) })?;
        let defaults = self.copied(|out| unsafe { (self.access_acls)(access.value, out) })?;
        // SecAccessCreate's default safe ACL trusts all applications for some
        // operations. Remove every default, not just its restricted read entry.
        // SAFETY: CopyACLList returns a CFArray of live SecACL references.
        unsafe {
            for index in 0..(self.array_len)(defaults.value) {
                status((self.acl_remove)((self.array_get)(defaults.value, index)))?;
            }
        }
        let acl = self.copied(|out| unsafe {
            (self.acl_create)(access.value, apps.value, label.value, 1, out)
        })?;
        let any = self.array(&[self.auth_any])?;
        // SAFETY: the new ACL is live and the array contains an authorization.
        status(unsafe { (self.acl_authorizations)(acl.value, any.value) })?;
        Ok(access)
    }
}

pub(super) struct Store {
    api: &'static Api,
    service: Owned,
    account: Owned,
    keychain: Owned,
    search: Owned,
}

// SAFETY: the retained strings and search array are immutable, SecKeychain
// supports concurrent calls, and the loaded API lives for the process lifetime.
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
        // SAFETY: valid writable out-pointer, Copy reference adopted below.
        let keychain = api.copied(|out| unsafe { (api.keychain_default)(out) })?;
        let search = api.array(&[keychain.value])?;
        Ok(Self {
            api,
            service: api.string(service)?,
            account: api.string("config-gate-v1")?,
            keychain,
            search,
        })
    }

    fn query(&self, read: bool, add: bool, extra: &[(Ref, Ref)]) -> Result<Owned, KeychainError> {
        let a = self.api;
        let mut pairs = vec![
            (a.class, a.generic_password),
            (a.service, self.service.value),
            (a.account, self.account.value),
            (a.synchronizable, a.false_value),
            (a.use_ui, a.fail_ui),
        ];
        pairs.push(if add {
            (a.use_keychain, self.keychain.value)
        } else {
            (a.search_list, self.search.value)
        });
        if read {
            pairs.push((a.return_data, a.true_value));
        }
        pairs.extend_from_slice(extra);
        a.dictionary(&pairs)
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
        // Recheck before creating an ACL, even for a long-lived handle.
        self.api.signing_identity()?;
        let access = self.api.access_for_self()?;
        let data = self.encoded(&KeychainState {
            key,
            anchor: AnchorState::default(),
        })?;
        let query = self.query(
            false,
            true,
            &[
                (self.api.access, access.value),
                (self.api.value_data, data.value),
            ],
        )?;
        // SAFETY: all inputs remain live and no result is requested.
        status(unsafe { (self.api.item_add)(query.value, null_mut()) })
    }

    pub(super) fn replace(&self, state: &KeychainState) -> Result<(), KeychainError> {
        let data = self.encoded(state)?;
        let query = self.query(false, false, &[])?;
        let attributes = self.api.dictionary(&[(self.api.value_data, data.value)])?;
        // SAFETY: live dictionaries; data-only update preserves the original ACL.
        status(unsafe { (self.api.item_update)(query.value, attributes.value) })
    }

    pub(super) fn delete(&self) -> Result<(), KeychainError> {
        let query = self.query(false, false, &[])?;
        // SAFETY: unique class/service/account query, pinned to one keychain.
        status(unsafe { (self.api.item_delete)(query.value) })
    }
}
