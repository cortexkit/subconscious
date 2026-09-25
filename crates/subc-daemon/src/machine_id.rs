//! The machine id file: where the daemon's [`MachineId`] lives on disk, how the
//! daemon mints it once, and how an operator replaces it.
//!
//! The file is `<data home>/cortexkit/machine-id`, one line of 32 lowercase hex
//! characters. The daemon reads it at startup, before it accepts a connection,
//! and mints it only when it is absent. It never rewrites a present file: a file
//! that does not parse stops the daemon with [`MachineIdFileError::Corrupt`],
//! because silently replacing a corrupt identity would hand every module a new
//! name for the same machine. Only `ck machine adopt` replaces the value, and it
//! takes effect at the next daemon start.
//!
//! See [`MachineId`] for the rule every holder of the value must follow: it is a
//! name, never an authority.

use std::{
    error::Error,
    fmt, fs,
    io::{self, Write as _},
    path::{Path, PathBuf},
};

pub use subc_protocol::{MachineId, MachineIdError};

/// File name of the machine id under `<data home>/cortexkit/`.
pub const MACHINE_ID_FILE_NAME: &str = "machine-id";

/// Resolve `<data home>/cortexkit/machine-id` from the same data home the daemon
/// uses for storage.
///
/// A relative data home (HOME and XDG_DATA_HOME both unset) is refused rather
/// than resolved against the working directory: the machine's identity must not
/// depend on where the daemon happened to be started from.
pub fn default_machine_id_path() -> Result<PathBuf, MachineIdFileError> {
    let data_home = crate::daemon_config::default_data_home();
    if !data_home.is_absolute() {
        return Err(MachineIdFileError::RelativeDataHome { data_home });
    }
    Ok(data_home.join("cortexkit").join(MACHINE_ID_FILE_NAME))
}

/// Read the machine id at `path`, minting and writing a fresh one when the file
/// is absent. A present file is only ever read, never rewritten.
///
/// The caller must hold whatever exclusion keeps two daemons from booting at
/// once (the daemon calls this under its start lock); two concurrent mints
/// would otherwise race to the rename and the loser's modules would have seen a
/// value that no longer exists.
pub fn load_or_mint(path: &Path) -> Result<MachineId, MachineIdFileError> {
    if let Some(existing) = read(path)? {
        return Ok(existing);
    }
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).map_err(MachineIdFileError::Random)?;
    write(path, &MachineId::from_bytes(bytes))?;
    // Serve what is on disk, not what was generated: if the write landed
    // anything other than the minted value, the next boot would disagree with
    // this one, so this boot fails now instead.
    read(path)?.ok_or_else(|| MachineIdFileError::Write {
        path: path.to_path_buf(),
        source: io::Error::new(
            io::ErrorKind::NotFound,
            "machine id file vanished right after it was written",
        ),
    })
}

/// Read and validate the machine id at `path`. `Ok(None)` means the file does
/// not exist; any other read failure, or content that is not exactly 32
/// lowercase hex characters (one trailing newline allowed), is an error.
pub fn read(path: &Path) -> Result<Option<MachineId>, MachineIdFileError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(MachineIdFileError::Read {
                path: path.to_path_buf(),
                source,
            })
        }
    };
    let text = String::from_utf8(bytes).map_err(|_| MachineIdFileError::Corrupt {
        path: path.to_path_buf(),
        reason: "the file is not UTF-8".to_string(),
    })?;
    parse_file_contents(&text)
        .map(Some)
        .map_err(|reason| MachineIdFileError::Corrupt {
            path: path.to_path_buf(),
            reason: reason.to_string(),
        })
}

/// Write `id` to `path` through a temporary file in the same directory and a
/// rename, so a reader sees either the old file or the complete new one.
///
/// This replaces an existing file. The daemon only calls it when the file is
/// absent; `ck machine adopt` calls it deliberately.
pub fn write(path: &Path, id: &MachineId) -> Result<(), MachineIdFileError> {
    let write_error = |source: io::Error| MachineIdFileError::Write {
        path: path.to_path_buf(),
        source,
    };
    let dir = path.parent().ok_or_else(|| {
        write_error(io::Error::new(
            io::ErrorKind::InvalidInput,
            "machine id path has no parent directory",
        ))
    })?;
    fs::create_dir_all(dir).map_err(write_error)?;

    let mut suffix = [0u8; 8];
    getrandom::getrandom(&mut suffix).map_err(MachineIdFileError::Random)?;
    let tmp = dir.join(format!(
        ".{MACHINE_ID_FILE_NAME}.tmp-{}-{:016x}",
        std::process::id(),
        u64::from_be_bytes(suffix)
    ));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        file.write_all(format!("{id}\n").as_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, path)
    })();
    if let Err(source) = result {
        let _ = fs::remove_file(&tmp);
        return Err(write_error(source));
    }
    Ok(())
}

/// Parse file content: the id, optionally followed by one line ending.
fn parse_file_contents(text: &str) -> Result<MachineId, MachineIdError> {
    let line = text
        .strip_suffix("\r\n")
        .or_else(|| text.strip_suffix('\n'))
        .unwrap_or(text);
    MachineId::parse(line)
}

/// Why the daemon could not establish its machine id.
#[derive(Debug)]
pub enum MachineIdFileError {
    /// The data home resolved to a relative path, so the file's location would
    /// depend on the working directory.
    RelativeDataHome { data_home: PathBuf },
    /// The file exists but does not hold a machine id. The daemon refuses to
    /// start rather than replace it.
    Corrupt { path: PathBuf, reason: String },
    /// The file exists but could not be read.
    Read { path: PathBuf, source: io::Error },
    /// A new file could not be written.
    Write { path: PathBuf, source: io::Error },
    /// The operating system's random source failed.
    Random(getrandom::Error),
}

impl fmt::Display for MachineIdFileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RelativeDataHome { data_home } => write!(
                f,
                "machine id: the data home resolved to the relative path {}; set HOME or XDG_DATA_HOME so the machine id has a fixed location",
                data_home.display()
            ),
            Self::Corrupt { path, reason } => write!(
                f,
                "machine id file {} is corrupt ({reason}); the daemon will not replace a machine identity. Restore the file from backup, or run `ck machine adopt <id>` with the id this machine had",
                path.display()
            ),
            Self::Read { path, source } => {
                write!(f, "machine id file {} could not be read: {source}", path.display())
            }
            Self::Write { path, source } => write!(
                f,
                "machine id file {} could not be written: {source}",
                path.display()
            ),
            Self::Random(source) => {
                write!(f, "machine id: the random source failed: {source}")
            }
        }
    }
}

impl Error for MachineIdFileError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Read { source, .. } | Self::Write { source, .. } => Some(source),
            // getrandom's error implements std's Error only with its `std`
            // feature, which this crate does not enable; Display carries it.
            Self::Random(_) | Self::RelativeDataHome { .. } | Self::Corrupt { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> subc_test_support::TestTempDir {
        subc_test_support::TestTempDir::new(&format!("subc-machine-id-{name}"))
    }

    #[test]
    fn mint_writes_one_line_and_a_second_load_reads_the_same_id() {
        let dir = temp_dir("mint");
        let path = dir.join("cortexkit").join(MACHINE_ID_FILE_NAME);
        let first = load_or_mint(&path).expect("mint");
        assert_eq!(fs::read_to_string(&path).unwrap(), format!("{first}\n"));
        let second = load_or_mint(&path).expect("reload");
        assert_eq!(first, second);
        // No temporary file is left behind beside the id.
        let entries: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(
            entries,
            vec![std::ffi::OsString::from(MACHINE_ID_FILE_NAME)]
        );
    }

    #[test]
    fn a_file_without_a_trailing_newline_or_with_crlf_is_accepted() {
        let dir = temp_dir("endings");
        let path = dir.join(MACHINE_ID_FILE_NAME);
        for content in [
            "0123456789abcdef0123456789abcdef",
            "0123456789abcdef0123456789abcdef\n",
            "0123456789abcdef0123456789abcdef\r\n",
        ] {
            fs::write(&path, content).unwrap();
            assert_eq!(
                load_or_mint(&path).unwrap().as_str(),
                "0123456789abcdef0123456789abcdef"
            );
            assert_eq!(fs::read_to_string(&path).unwrap(), content);
        }
    }

    #[test]
    fn a_corrupt_file_is_refused_by_name_and_left_untouched() {
        let dir = temp_dir("corrupt");
        let path = dir.join(MACHINE_ID_FILE_NAME);
        for content in [
            "",
            "0123456789ABCDEF0123456789abcdef\n",
            "0123456789abcdef0123456789abcdef\n\n",
            " 0123456789abcdef0123456789abcdef",
            "not an id",
        ] {
            fs::write(&path, content).unwrap();
            let err = load_or_mint(&path).expect_err("corrupt file must refuse");
            assert!(
                matches!(&err, MachineIdFileError::Corrupt { path: p, .. } if p == &path),
                "{content:?}: {err}"
            );
            assert!(err.to_string().contains(&path.display().to_string()));
            assert_eq!(fs::read_to_string(&path).unwrap(), content);
        }
        fs::write(&path, [0xffu8, 0xfe]).unwrap();
        assert!(matches!(
            load_or_mint(&path),
            Err(MachineIdFileError::Corrupt { .. })
        ));
        assert_eq!(fs::read(&path).unwrap(), vec![0xff, 0xfe]);
    }

    #[test]
    fn write_replaces_the_value_atomically() {
        let dir = temp_dir("write");
        let path = dir.join(MACHINE_ID_FILE_NAME);
        let first = load_or_mint(&path).unwrap();
        let adopted = MachineId::parse("fedcba9876543210fedcba9876543210").unwrap();
        write(&path, &adopted).unwrap();
        assert_ne!(first, adopted);
        assert_eq!(read(&path).unwrap(), Some(adopted));
    }
}
