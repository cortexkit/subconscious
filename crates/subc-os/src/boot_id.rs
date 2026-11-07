//! Kernel boot identity for persisted process containers.
//!
//! Process IDs and boot-relative creation times can both be reused after a
//! reboot. A saved process identity must not authorize a signal on another boot.

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
fn read_kernel_id() -> std::io::Result<String> {
    let mut bytes = [0_u8; 128];
    let mut length = bytes.len();
    // SAFETY: the name is a NUL-terminated constant, the output points to the
    // initialized buffer whose length is supplied, and no new value is written.
    let result = unsafe {
        libc::sysctlbyname(
            c"kern.bootsessionuuid".as_ptr(),
            bytes.as_mut_ptr().cast(),
            &mut length,
            std::ptr::null_mut(),
            0,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let value = bytes.get(..length).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "oversized boot identity")
    })?;
    let value = value.strip_suffix(&[0]).unwrap_or(value);
    String::from_utf8(value.to_owned())
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

/// Return the kernel's current boot UUID, normalized to lowercase.
///
/// Linux reads `/proc/sys/kernel/random/boot_id`; macOS reads
/// `kern.bootsessionuuid`. An unreadable or malformed identity is an error,
/// never a fallback identity that could be confused with another boot.
pub fn current() -> std::io::Result<String> {
    #[cfg(target_os = "linux")]
    let value = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
    #[cfg(target_os = "macos")]
    let value = read_kernel_id()?;
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    return Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "kernel boot identity unavailable",
    ));

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    normalize(&value)
}

/// Compare a recorded boot UUID with this boot. Invalid records and failed
/// kernel reads are errors, not evidence that an old container is gone.
pub fn is_current(recorded: &str) -> std::io::Result<bool> {
    Ok(normalize(recorded)? == current()?)
}

fn normalize(value: &str) -> std::io::Result<String> {
    let value = value.trim();
    if value.len() != 36
        || !value.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid kernel boot UUID",
        ));
    }
    Ok(value.to_ascii_lowercase())
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    #[test]
    fn boot_identity_matches_the_kernel_value() {
        #[cfg(target_os = "linux")]
        let expected = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap();
        #[cfg(target_os = "macos")]
        let expected = {
            let output = std::process::Command::new("/usr/sbin/sysctl")
                .args(["-n", "kern.bootsessionuuid"])
                .output()
                .unwrap();
            assert!(output.status.success());
            String::from_utf8(output.stdout).unwrap()
        };
        assert_eq!(
            super::current().unwrap(),
            expected.trim().to_ascii_lowercase()
        );
        assert_eq!(super::current().unwrap().len(), 36);
        assert!(super::is_current(&expected).unwrap());
        assert!(!super::is_current("00000000-0000-0000-0000-000000000000").unwrap());
        assert!(super::is_current("not a boot UUID").is_err());
    }
}
