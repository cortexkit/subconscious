//! Read-only macOS code-directory identities for approved files and live images.

use std::{fmt, fs::File, io};
mod macho;

/// The kernel's twenty-byte code-directory hash, not a pathname or inode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CodeDirectoryHash(pub [u8; 20]);

#[derive(Debug)]
pub enum SignatureError {
    NoSuchProcess,
    NotSigned,
    NotValid,
    Other(io::Error),
    Parse,
}

impl fmt::Display for SignatureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoSuchProcess => f.write_str("process no longer exists"),
            Self::NotSigned => f.write_str("image has no code signature"),
            Self::NotValid => f.write_str("running code signature is not valid"),
            Self::Other(error) => error.fmt(f),
            Self::Parse => f.write_str("invalid or unsupported Mach-O code signature"),
        }
    }
}
impl std::error::Error for SignatureError {}

pub fn process_cdhash(pid: u32) -> Result<CodeDirectoryHash, SignatureError> {
    let pid = libc::pid_t::try_from(pid)
        .ok()
        .filter(|pid| *pid > 0)
        .ok_or_else(|| {
            SignatureError::Other(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid process id",
            ))
        })?;
    process_cdhash_with_query(|operation, buffer| {
        // SAFETY: pid is positive and representable as pid_t. The initialized
        // writable buffer has the operation's required size.
        #[allow(unsafe_code)]
        let status =
            unsafe { ffi::csops(pid, operation, buffer.as_mut_ptr().cast(), buffer.len()) };
        if status == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    })
}

fn process_cdhash_with_query(
    mut query: impl FnMut(u32, &mut [u8]) -> io::Result<()>,
) -> Result<CodeDirectoryHash, SignatureError> {
    let map_error = |error: io::Error| match error.raw_os_error() {
        Some(libc::ESRCH) => SignatureError::NoSuchProcess,
        Some(libc::EINVAL) => SignatureError::NotSigned,
        _ => SignatureError::Other(error),
    };
    let mut hash = [0; 20];
    query(5, &mut hash).map_err(map_error)?;
    let mut flags = [0; 4];
    query(0, &mut flags).map_err(map_error)?;
    if u32::from_ne_bytes(flags) & 1 == 0 {
        return Err(SignatureError::NotValid);
    }
    let mut after = [0; 20];
    query(5, &mut after).map_err(map_error)?;
    if after != hash {
        // Exec preserves the process birth time. Do not combine the old image's
        // hash with a status sampled from its replacement; let callers retry.
        return Err(SignatureError::Other(io::Error::new(
            io::ErrorKind::WouldBlock,
            "process code hash changed while sampling signature status",
        )));
    }
    Ok(CodeDirectoryHash(hash))
}

/// Signature identities extracted from the exact bytes sent to the witness callback.
#[derive(Debug, PartialEq, Eq)]
pub enum FileSignature {
    NotMachO,
    Unsigned,
    Invalid,
    Signed(Vec<CodeDirectoryHash>),
}

/// Stream one retained descriptor once. The caller hashes the same chunks from
/// which bounded Mach-O signature ranges are captured; no pathname is reopened.
pub fn file_signature(
    file: &File,
    mut witness: impl FnMut(&[u8]),
) -> Result<FileSignature, SignatureError> {
    use std::os::unix::fs::FileExt;
    macho::witness::extract(
        file.metadata().map_err(SignatureError::Other)?.len(),
        |buffer, offset| file.read_at(buffer, offset),
        &mut witness,
        64 * 1024,
    )
}

#[allow(unsafe_code)]
mod ffi {
    use std::ffi::c_void;
    unsafe extern "C" {
        pub(super) fn csops(
            pid: libc::pid_t,
            ops: libc::c_uint,
            address: *mut c_void,
            size: libc::size_t,
        ) -> libc::c_int;
        pub(super) fn CC_SHA1(data: *const c_void, length: u32, output: *mut u8) -> *mut u8;
        pub(super) fn CC_SHA256(data: *const c_void, length: u32, output: *mut u8) -> *mut u8;
        pub(super) fn CC_SHA384(data: *const c_void, length: u32, output: *mut u8) -> *mut u8;
        pub(super) fn CC_SHA512(data: *const c_void, length: u32, output: *mut u8) -> *mut u8;
    }
}

pub(super) fn hash_bytes(bytes: &[u8], kind: u8) -> Result<Vec<u8>, SignatureError> {
    let length = u32::try_from(bytes.len()).map_err(|_| SignatureError::Parse)?;
    let mut hash = [0u8; 64];
    // SAFETY: the bounded input length fits CC_LONG; the initialized output has
    // room for the largest supported CommonCrypto digest (SHA-512, 64 bytes).
    #[allow(unsafe_code)]
    let result = unsafe {
        match kind {
            1 => ffi::CC_SHA1(bytes.as_ptr().cast(), length, hash.as_mut_ptr()),
            2 | 3 => ffi::CC_SHA256(bytes.as_ptr().cast(), length, hash.as_mut_ptr()),
            4 => ffi::CC_SHA384(bytes.as_ptr().cast(), length, hash.as_mut_ptr()),
            5 => ffi::CC_SHA512(bytes.as_ptr().cast(), length, hash.as_mut_ptr()),
            _ => return Err(SignatureError::Parse),
        }
    };
    if result.is_null() {
        return Err(SignatureError::Other(io::Error::other(
            "CodeDirectory digest unavailable",
        )));
    }
    let size = match kind {
        1 => 20,
        2 | 3 => 32,
        4 => 48,
        5 => 64,
        _ => return Err(SignatureError::Parse),
    };
    Ok(hash[..size].to_vec())
}

#[cfg(test)]
mod tests {
    #[test]
    fn content_binding_defers_a_code_hash_change_during_status_query() {
        let mut hashes = 0;
        let result = super::process_cdhash_with_query(|operation, output| {
            match operation {
                0 => output.copy_from_slice(&1u32.to_ne_bytes()),
                5 => {
                    hashes += 1;
                    output.fill(hashes);
                }
                _ => panic!("unexpected csops operation"),
            }
            Ok(())
        });
        assert!(
            matches!(result,Err(super::SignatureError::Other(ref error)) if error.kind()==std::io::ErrorKind::WouldBlock),
            "a changed hash cannot pair another image's valid status: {result:?}"
        );
        assert_eq!(hashes, 2);
    }

    #[test]
    fn content_binding_refuses_a_kernel_hash_without_cs_valid() {
        for flags in [0u32, 2, 0x200] {
            let result = super::process_cdhash_with_query(|operation, output| {
                match operation {
                    0 => output.copy_from_slice(&flags.to_ne_bytes()),
                    5 => output.fill(42),
                    _ => panic!("unexpected csops operation"),
                }
                Ok(())
            });
            assert!(
                matches!(result, Err(super::SignatureError::NotValid)),
                "csops flags {flags:#x} do not attest valid running code"
            );
        }
        assert!(super::process_cdhash_with_query(|operation, output| {
            if operation == 0 {
                output.copy_from_slice(&1u32.to_ne_bytes());
            } else {
                output.fill(42);
            }
            Ok(())
        })
        .is_ok());
    }
    use super::*;
    use std::{
        path::Path,
        process::Command,
        sync::atomic::{AtomicU64, Ordering},
        time::{Duration, Instant},
    };

    #[test]
    fn content_binding_cdhash_matches_codesign_and_the_running_child() {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join(format!(
            "../../target/scratch/cdhash-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();
        let binary = root.join("sleep");
        std::fs::copy("/bin/sleep", &binary).unwrap();
        assert!(Command::new("/usr/bin/codesign")
            .args(["--force", "--sign", "-"])
            .arg(&binary)
            .status()
            .unwrap()
            .success());
        let mut witness = Vec::new();
        let FileSignature::Signed(expected) =
            file_signature(&File::open(&binary).unwrap(), |chunk| {
                witness.extend_from_slice(chunk)
            })
            .unwrap()
        else {
            panic!("signed fixture must have code directories");
        };
        assert_eq!(witness, std::fs::read(&binary).unwrap());
        let architectures = Command::new("/usr/bin/lipo")
            .arg("-archs")
            .arg(&binary)
            .output()
            .unwrap();
        assert!(architectures.status.success());
        for architecture in String::from_utf8(architectures.stdout)
            .unwrap()
            .split_whitespace()
        {
            let output = Command::new("/usr/bin/codesign")
                .args(["-dvvv", "--arch", architecture])
                .arg(&binary)
                .output()
                .unwrap();
            assert!(output.status.success());
            let stderr = String::from_utf8(output.stderr).unwrap();
            let hash = stderr
                .lines()
                .find_map(|line| line.strip_prefix("CDHash="))
                .unwrap();
            let hash = CodeDirectoryHash(std::array::from_fn(|index| {
                u8::from_str_radix(&hash[index * 2..index * 2 + 2], 16).unwrap()
            }));
            assert!(
                expected.contains(&hash),
                "missing codesign hash for {architecture}: {hash:?}; {expected:?}"
            );
        }
        let mut child = Command::new(&binary).arg("30").spawn().unwrap();
        let result = std::panic::catch_unwind(|| {
            let deadline = Instant::now() + Duration::from_secs(5);
            let process = crate::Process::open(child.id()).unwrap().unwrap();
            while process
                .observe()
                .and_then(|observation| observation.executable)
                != crate::file_identity(&binary)
            {
                assert!(
                    Instant::now() < deadline,
                    "owned child did not exec its signed image"
                );
                std::thread::yield_now();
            }
            loop {
                match process_cdhash(child.id()) {
                    Ok(actual) => {
                        assert!(
                            expected.contains(&actual),
                            "current image {actual:?} not in {expected:?}"
                        );
                        break;
                    }
                    Err(SignatureError::NotSigned | SignatureError::NoSuchProcess)
                        if Instant::now() < deadline =>
                    {
                        std::thread::yield_now()
                    }
                    Err(error) => panic!("running child signature: {error}"),
                }
            }
        });
        child.kill().unwrap();
        child.wait().unwrap();
        std::fs::remove_dir_all(&root).unwrap();
        result.unwrap();
    }
}
