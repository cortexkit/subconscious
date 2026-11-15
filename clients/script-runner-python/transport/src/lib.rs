//! The launch contract ck-script-runner-python shares with the Bun runner.
//!
//! The daemon writes the approved script to a private channel: on Unix a pipe on descriptor 6,
//! named by `SUBC_SCRIPT_FD=6:<inode>`; on Windows a named pipe whose server must be the daemon
//! process named by `SUBC_DAEMON_PID`. The bytes are framed by a 52-byte header (`CKSCRIPT`,
//! version 1, little-endian length, SHA-256 of the body), and the runner refuses anything that
//! doesn't match exactly before a single line of the script runs.

use std::ffi::OsString;
use std::io::{self, Read};

pub mod sha256;
#[cfg(windows)]
pub mod windows_pipe;

/// Largest script the daemon sends, the same limit the Bun runner enforces.
pub const MAX_SCRIPT_BYTES: u64 = 32 * 1024 * 1024;
/// Length of the frame header the daemon writes before the script bytes.
pub const HEADER_LEN: usize = 52;
const MAGIC: &[u8; 8] = b"CKSCRIPT";
const VERSION: u32 = 1;

/// Why the runner refused to start. The message carries the same `script_*_refused:` prefix the
/// Bun runner prints, so the daemon and `ck` can report both runners the same way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused(pub String);

impl std::fmt::Display for Refused {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for Refused {}

fn pipe_refused(reason: &str) -> Refused {
    Refused(format!("script_pipe_refused: {reason}"))
}

/// Environment variables that would change what runs: every `PYTHON*` variable CPython reads,
/// and the Bun and Node loader variables the daemon already strips for the Bun runner.
/// Windows names are case-insensitive, so they are compared upper-cased there.
pub fn environment_refusal<I>(names: I, windows: bool) -> Option<Refused>
where
    I: IntoIterator<Item = OsString>,
{
    for name in names {
        let shown = name.to_string_lossy().into_owned();
        let key = if windows {
            shown.to_uppercase()
        } else {
            shown.clone()
        };
        let loader = ["NODE_OPTIONS", "BUN_OPTIONS", "BUN_INSTALL", "NODE_PATH"]
            .contains(&key.as_str())
            || key.starts_with("BUN_CONFIG_");
        if key.starts_with("PYTHON") || loader {
            return Some(Refused(format!("script_environment_refused: {shown}")));
        }
    }
    None
}

fn read_exact_or_refuse<R: Read>(reader: &mut R, length: usize) -> Result<Vec<u8>, Refused> {
    let mut bytes = vec![0u8; length];
    let mut offset = 0;
    while offset < length {
        match reader.read(&mut bytes[offset..]) {
            Ok(0) => return Err(pipe_refused("short pipe")),
            Ok(count) => offset += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return Err(pipe_refused("pipe read failed")),
        }
    }
    Ok(bytes)
}

/// Reads one framed script and checks its header, length, trailing bytes and digest.
pub fn read_framed<R: Read>(reader: &mut R) -> Result<Vec<u8>, Refused> {
    let header = read_exact_or_refuse(reader, HEADER_LEN)?;
    let version = u32::from_le_bytes(header[8..12].try_into().expect("four header bytes"));
    if &header[..8] != MAGIC || version != VERSION {
        return Err(pipe_refused("bad magic or version"));
    }
    let length = u64::from_le_bytes(header[12..20].try_into().expect("eight header bytes"));
    if length > MAX_SCRIPT_BYTES {
        return Err(pipe_refused("script exceeds 32 MiB"));
    }
    let bytes = read_exact_or_refuse(reader, length as usize)?;
    let mut extra = [0u8; 1];
    loop {
        match reader.read(&mut extra) {
            Ok(0) => break,
            Ok(_) => return Err(pipe_refused("extra byte")),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return Err(pipe_refused("pipe read failed")),
        }
    }
    if sha256::digest(&bytes)[..] != header[20..] {
        return Err(pipe_refused("digest mismatch"));
    }
    Ok(bytes)
}

/// Builds the frame the daemon writes for `script`, for tests and the signing smoke check.
pub fn frame(script: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(HEADER_LEN + script.len());
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&VERSION.to_le_bytes());
    bytes.extend_from_slice(&(script.len() as u64).to_le_bytes());
    bytes.extend_from_slice(&sha256::digest(script));
    bytes.extend_from_slice(script);
    bytes
}

/// Parses `SUBC_SCRIPT_FD` on Unix. Only descriptor 6 is accepted, as for the Bun runner.
#[cfg(unix)]
fn unix_identity(value: &str) -> Result<u64, Refused> {
    let invalid = || pipe_refused("invalid descriptor identity");
    let (fd, inode) = value.split_once(':').ok_or_else(invalid)?;
    let digits = |text: &str| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit());
    if !digits(fd) || !digits(inode) || fd.parse::<u64>().ok() != Some(6) {
        return Err(invalid());
    }
    inode.parse::<u64>().map_err(|_| invalid())
}

/// Opens descriptor 6 after checking that it is a FIFO with the inode the daemon named.
#[cfg(unix)]
fn open_unix_pipe(value: &str) -> Result<std::fs::File, Refused> {
    use std::mem::ManuallyDrop;
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    use std::os::unix::io::FromRawFd;

    let inode = unix_identity(value)?;
    // The descriptor isn't ours until it proves to be the daemon's pipe: if the check fails it
    // must stay open, because it may belong to other code in this process.
    // SAFETY: descriptor 6 is only borrowed here; ManuallyDrop keeps it from being closed.
    let borrowed = ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(6) });
    let metadata = borrowed
        .metadata()
        .map_err(|_| pipe_refused("wrong pipe identity"))?;
    if !metadata.file_type().is_fifo() || metadata.ino() != inode {
        return Err(pipe_refused("wrong pipe identity"));
    }
    Ok(ManuallyDrop::into_inner(borrowed))
}

/// Receives the approved script from the daemon's private channel.
pub fn read_script() -> Result<Vec<u8>, Refused> {
    let value = std::env::var_os("SUBC_SCRIPT_FD")
        .ok_or_else(|| pipe_refused("SUBC_SCRIPT_FD is required"))?
        .to_string_lossy()
        .into_owned();
    #[cfg(unix)]
    let mut pipe = open_unix_pipe(&value)?;
    #[cfg(windows)]
    let mut pipe = windows_pipe::open(&value, std::env::var("SUBC_DAEMON_PID").ok().as_deref())?;
    // The script code must not see where it came from.
    std::env::remove_var("SUBC_SCRIPT_FD");
    read_framed(&mut pipe)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vector() -> (Vec<u8>, Vec<u8>) {
        // The shared vector was produced by Python's struct and hashlib, not by frame().
        // It holds two JSON strings; this reads them without a JSON crate.
        let text = include_str!("../../../script-runner/tests/script-pipe-vector.json");
        let field = |name: &str| {
            let start =
                text.find(&format!("\"{name}\": \"")).expect("vector field") + name.len() + 5;
            let mut value = String::new();
            let mut chars = text[start..].chars();
            while let Some(c) = chars.next() {
                match c {
                    '"' => return value,
                    '\\' => value.push(match chars.next().expect("escape") {
                        'n' => '\n',
                        't' => '\t',
                        other => other,
                    }),
                    other => value.push(other),
                }
            }
            panic!("unterminated vector field {name}");
        };
        let source = field("source").into_bytes();
        let header_hex = field("header_hex");
        let header = (0..header_hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&header_hex[i..i + 2], 16).expect("hex"))
            .collect();
        (source, header)
    }

    #[test]
    fn frame_header_matches_shared_pipe_vector() {
        let (source, header) = vector();
        assert_eq!(frame(&source)[..HEADER_LEN], header[..]);
        let mut bytes = header.clone();
        bytes.extend_from_slice(&source);
        assert_eq!(read_framed(&mut &bytes[..]), Ok(source));
    }

    #[test]
    fn shared_vector_header_with_other_bytes_is_refused() {
        let (source, header) = vector();
        let mut bytes = header;
        let mut swapped = source.clone();
        swapped[0] ^= 1;
        bytes.extend_from_slice(&swapped);
        assert_eq!(
            read_framed(&mut &bytes[..]),
            Err(pipe_refused("digest mismatch"))
        );
    }

    #[test]
    fn malformed_frames_are_refused() {
        let good = frame(b"print('x')\n");
        let cases: Vec<(Vec<u8>, &str)> = vec![
            (good[..51].to_vec(), "short pipe"),
            (good[..good.len() - 1].to_vec(), "short pipe"),
            ([good.clone(), vec![0]].concat(), "extra byte"),
            (
                {
                    let mut b = good.clone();
                    b[0] ^= 1;
                    b
                },
                "bad magic or version",
            ),
            (
                {
                    let mut b = good.clone();
                    b[8] = 2;
                    b
                },
                "bad magic or version",
            ),
            (
                {
                    let mut b = good.clone();
                    b[20] ^= 1;
                    b
                },
                "digest mismatch",
            ),
            (
                {
                    let mut b = good.clone();
                    b[12..20].copy_from_slice(&(MAX_SCRIPT_BYTES + 1).to_le_bytes());
                    b
                },
                "script exceeds 32 MiB",
            ),
        ];
        for (bytes, reason) in cases {
            assert_eq!(
                read_framed(&mut &bytes[..]),
                Err(pipe_refused(reason)),
                "{reason}"
            );
        }
    }

    #[test]
    fn python_and_loader_variables_are_refused() {
        let refused = |names: &[&str], windows| {
            environment_refusal(names.iter().map(OsString::from), windows)
        };
        for name in [
            "PYTHONPATH",
            "PYTHONHOME",
            "PYTHONSTARTUP",
            "PYTHONSAFEPATH",
            "NODE_OPTIONS",
            "BUN_CONFIG_X",
        ] {
            assert_eq!(
                refused(&["HOME", name], false),
                Some(Refused(format!("script_environment_refused: {name}")))
            );
        }
        assert_eq!(
            refused(&["Pythonpath"], true),
            Some(Refused("script_environment_refused: Pythonpath".into()))
        );
        assert_eq!(
            refused(&["Pythonpath", "HOME", "SUBC_SCRIPT_FD"], false),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn descriptor_identity_must_name_descriptor_six() {
        assert_eq!(unix_identity("6:42"), Ok(42));
        for value in ["5:42", "6:", ":42", "6:4x", "06:42x", "6:42:1", "+6:42"] {
            assert_eq!(
                unix_identity(value),
                Err(pipe_refused("invalid descriptor identity")),
                "{value}"
            );
        }
    }
}
