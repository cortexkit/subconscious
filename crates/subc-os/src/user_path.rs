#![allow(unsafe_code)]

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::ptr::null_mut;
use windows_sys::Win32::{
    Foundation::{ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_SUCCESS},
    System::Registry::{
        RegCloseKey, RegDeleteValueW, RegFlushKey, RegGetValueW, RegOpenKeyExW, RegSetValueExW,
        HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_EXPAND_SZ, REG_SZ, RRF_NOEXPAND,
        RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ,
    },
    UI::WindowsAndMessaging::{
        SendMessageTimeoutW, HWND_BROADCAST, SMTO_ABORTIFHUNG, SMTO_ERRORONEXIT, WM_SETTINGCHANGE,
    },
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UserPathValue {
    Literal(OsString),
    Expandable(OsString),
}

/// Reads the current user's unexpanded Environment\Path value and its type.
pub fn read_user_path() -> io::Result<Option<UserPathValue>> {
    read_value(OsStr::new("Path"))
}

/// Writes, or removes, only the current user's Environment\Path value.
/// Flushes the registry change before returning.
pub fn write_user_path(value: Option<&UserPathValue>) -> io::Result<()> {
    write_value(OsStr::new("Path"), value)
}

/// Tells other applications to reload the user's environment. Hung windows
/// are skipped rather than being allowed to hold an install open indefinitely.
pub fn broadcast_user_environment_change() {
    let environment = terminated(OsStr::new("Environment")).expect("fixed key has no nul");
    // SAFETY: the nul-terminated wide string lives through the synchronous send;
    // no return buffer is requested.
    unsafe {
        SendMessageTimeoutW(
            HWND_BROADCAST,
            WM_SETTINGCHANGE,
            0,
            environment.as_ptr() as isize,
            SMTO_ABORTIFHUNG | SMTO_ERRORONEXIT,
            100,
            null_mut(),
        )
    };
}

fn terminated(text: &OsStr) -> io::Result<Vec<u16>> {
    let mut units = text.encode_wide().collect::<Vec<_>>();
    if units.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "registry string contains a nul",
        ));
    }
    units.push(0);
    Ok(units)
}

fn status(status: u32) -> io::Result<()> {
    if status == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(status as i32))
    }
}

fn read_value(name: &OsStr) -> io::Result<Option<UserPathValue>> {
    let key = terminated(OsStr::new("Environment"))?;
    let name = terminated(name)?;
    let flags = RRF_NOEXPAND | RRF_RT_REG_EXPAND_SZ | RRF_RT_REG_SZ;
    let mut dtype = 0;
    let mut size = 0;
    // SAFETY: key and name are nul-terminated, and both output pointers are valid.
    let first = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            name.as_ptr(),
            flags,
            &mut dtype,
            null_mut(),
            &mut size,
        )
    };
    if first == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    status(first)?;
    let mut units = vec![0_u16; (size as usize).div_ceil(2)];
    for _ in 0..4 {
        size = u32::try_from(units.len() * 2).map_err(io::Error::other)?;
        // SAFETY: the buffer holds exactly size bytes and all three outputs are writable.
        let result = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                name.as_ptr(),
                flags,
                &mut dtype,
                units.as_mut_ptr().cast(),
                &mut size,
            )
        };
        if result == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        if result == ERROR_MORE_DATA {
            units.resize((size as usize).div_ceil(2), 0);
            continue;
        }
        status(result)?;
        if size % 2 != 0 || size as usize / 2 > units.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "registry Path is not UTF-16",
            ));
        }
        let units = &units[..size as usize / 2];
        let value = OsString::from_wide(units.strip_suffix(&[0]).unwrap_or(units));
        return Ok(Some(match dtype {
            REG_SZ => UserPathValue::Literal(value),
            REG_EXPAND_SZ => UserPathValue::Expandable(value),
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "registry Path has an unsupported value type",
                ))
            }
        }));
    }
    Err(io::Error::other(
        "registry Path kept changing while it was being read",
    ))
}

struct Key(HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: the handle returned by RegOpenKeyExW belongs to this Key.
        unsafe { RegCloseKey(self.0) };
    }
}

fn write_value(name: &OsStr, value: Option<&UserPathValue>) -> io::Result<()> {
    let key = terminated(OsStr::new("Environment"))?;
    let name = terminated(name)?;
    let mut raw = null_mut();
    // SAFETY: the key is nul-terminated and raw can receive a handle.
    let opened =
        unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, key.as_ptr(), 0, KEY_SET_VALUE, &mut raw) };
    if opened == ERROR_FILE_NOT_FOUND && value.is_none() {
        return Ok(());
    }
    status(opened)?;
    let key = Key(raw);
    if let Some(value) = value {
        let (kind, text) = match value {
            UserPathValue::Literal(text) => (REG_SZ, text),
            UserPathValue::Expandable(text) => (REG_EXPAND_SZ, text),
        };
        let data = terminated(text)?;
        let size = u32::try_from(data.len() * 2).map_err(io::Error::other)?;
        // SAFETY: key and name are valid and the data buffer has size bytes.
        status(unsafe {
            RegSetValueExW(key.0, name.as_ptr(), 0, kind, data.as_ptr().cast(), size)
        })?;
    } else {
        // SAFETY: key is open with KEY_SET_VALUE and the name is nul-terminated.
        let deleted = unsafe { RegDeleteValueW(key.0, name.as_ptr()) };
        if deleted != ERROR_FILE_NOT_FOUND {
            status(deleted)?;
        }
    }
    // SAFETY: key is held until the flush returns.
    status(unsafe { RegFlushKey(key.0) })
}

#[cfg(test)]
mod tests;
