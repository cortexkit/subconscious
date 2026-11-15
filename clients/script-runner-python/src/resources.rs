//! The compiled standard library embedded in the runner.
//!
//! build.rs compiles the archive's pure-Python standard library to bytecode and writes one table:
//! `CKPYLIB1`, a little-endian u32 entry count, then for each module a flags byte (1 = package),
//! a u16 name length, a u32 data length, the dotted module name and the marshalled code object.
//! The interpreter reads it through `ck_resource_lookup`, so no import ever touches the disk.

use std::collections::HashMap;
use std::ffi::c_int;
use std::sync::OnceLock;

static TABLE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/stdlib.bin"));

struct Module {
    is_package: bool,
    code: &'static [u8],
}

fn parse(table: &'static [u8]) -> HashMap<&'static [u8], Module> {
    let malformed = "the embedded standard-library table is malformed";
    assert!(table.starts_with(b"CKPYLIB1"), "{malformed}");
    let mut at = 8;
    let mut take = |count: usize| {
        let slice = table.get(at..at + count).expect(malformed);
        at += count;
        slice
    };
    let count = u32::from_le_bytes(take(4).try_into().expect(malformed));
    let mut modules = HashMap::with_capacity(count as usize);
    for _ in 0..count {
        let flags = take(1)[0];
        let name_len = u16::from_le_bytes(take(2).try_into().expect(malformed)) as usize;
        let data_len = u32::from_le_bytes(take(4).try_into().expect(malformed)) as usize;
        let name = take(name_len);
        let code = take(data_len);
        modules.insert(name, Module { is_package: flags & 1 == 1, code });
    }
    modules
}

fn modules() -> &'static HashMap<&'static [u8], Module> {
    static MODULES: OnceLock<HashMap<&'static [u8], Module>> = OnceLock::new();
    MODULES.get_or_init(|| parse(TABLE))
}

/// Looks up one module for the interpreter's in-memory finder (src/embed.c).
///
/// Returns 1 and fills the out pointers when the table holds `name`, 0 otherwise.
///
/// # Safety
/// `name` must point to `name_len` readable bytes and every out pointer must be writable.
#[no_mangle]
pub unsafe extern "C" fn ck_resource_lookup(
    name: *const u8,
    name_len: usize,
    data: *mut *const u8,
    data_len: *mut usize,
    is_package: *mut c_int,
) -> c_int {
    // SAFETY: the caller guarantees `name` covers `name_len` bytes.
    let key = unsafe { std::slice::from_raw_parts(name, name_len) };
    match modules().get(key) {
        Some(module) => {
            // SAFETY: the caller guarantees the out pointers are writable.
            unsafe {
                *data = module.code.as_ptr();
                *data_len = module.code.len();
                *is_package = c_int::from(module.is_package);
            }
            1
        }
        None => 0,
    }
}
