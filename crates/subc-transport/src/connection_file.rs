use std::{
    error::Error,
    fmt,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    process,
};

use serde::{Deserialize, Serialize};

pub const SCHEMA_VERSION: u32 = 1;
pub const MIN_KEY_LEN: usize = 32;
pub const KEY_LEN: usize = 32;
pub const DAEMON_ID_LEN: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionInfo {
    pub schema: u32,
    pub endpoints: Vec<Endpoint>,
    pub key: Vec<u8>,
    pub daemon_id: [u8; DAEMON_ID_LEN],
    pub pid: u32,
    pub daemon_ver: String,
}

impl ConnectionInfo {
    pub fn validate(&self) -> Result<(), ConnectionFileError> {
        if self.schema != SCHEMA_VERSION {
            return Err(ConnectionFileError::UnsupportedSchema {
                schema: self.schema,
                supported: SCHEMA_VERSION,
            });
        }
        if self.endpoints.is_empty() {
            return Err(ConnectionFileError::Invalid {
                reason: "connection file must include at least one endpoint".to_owned(),
            });
        }
        if self.key.len() < MIN_KEY_LEN {
            return Err(ConnectionFileError::KeyTooShort {
                len: self.key.len(),
                min: MIN_KEY_LEN,
            });
        }
        Ok(())
    }
}

#[derive(Debug)]
pub enum ConnectionFileError {
    MissingParent {
        path: PathBuf,
    },
    MissingFileName {
        path: PathBuf,
    },
    Io {
        op: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    JsonRead {
        path: PathBuf,
        source: serde_json::Error,
    },
    JsonWrite {
        path: PathBuf,
        source: serde_json::Error,
    },
    Random(getrandom::Error),
    UnsupportedSchema {
        schema: u32,
        supported: u32,
    },
    Invalid {
        reason: String,
    },
    KeyTooShort {
        len: usize,
        min: usize,
    },
}

pub fn write_atomic(
    path: impl AsRef<Path>,
    info: &ConnectionInfo,
) -> Result<(), ConnectionFileError> {
    let path = path.as_ref();
    info.validate()?;

    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| ConnectionFileError::MissingParent {
            path: path.to_path_buf(),
        })?;
    let file_name = path
        .file_name()
        .ok_or_else(|| ConnectionFileError::MissingFileName {
            path: path.to_path_buf(),
        })?;
    let temp_path = temp_path(parent, file_name)?;
    let result = write_atomic_inner(path, &temp_path, info);
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

pub fn read(path: impl AsRef<Path>) -> Result<ConnectionInfo, ConnectionFileError> {
    let path = path.as_ref();
    let bytes = fs::read(path).map_err(|source| ConnectionFileError::Io {
        op: "read",
        path: path.to_path_buf(),
        source,
    })?;
    let info: ConnectionInfo =
        serde_json::from_slice(&bytes).map_err(|source| ConnectionFileError::JsonRead {
            path: path.to_path_buf(),
            source,
        })?;
    info.validate()?;
    Ok(info)
}

pub fn generate_key() -> Result<Vec<u8>, ConnectionFileError> {
    let mut key = vec![0u8; KEY_LEN];
    getrandom::getrandom(&mut key).map_err(ConnectionFileError::Random)?;
    Ok(key)
}

pub fn generate_daemon_id() -> Result<[u8; DAEMON_ID_LEN], ConnectionFileError> {
    let mut daemon_id = [0u8; DAEMON_ID_LEN];
    getrandom::getrandom(&mut daemon_id).map_err(ConnectionFileError::Random)?;
    Ok(daemon_id)
}

fn write_atomic_inner(
    path: &Path,
    temp_path: &Path,
    info: &ConnectionInfo,
) -> Result<(), ConnectionFileError> {
    let json =
        serde_json::to_vec_pretty(info).map_err(|source| ConnectionFileError::JsonWrite {
            path: path.to_path_buf(),
            source,
        })?;

    {
        let mut file =
            open_owner_only_new(temp_path).map_err(|source| ConnectionFileError::Io {
                op: "create_temp",
                path: temp_path.to_path_buf(),
                source,
            })?;
        file.write_all(&json)
            .and_then(|()| file.sync_all())
            .map_err(|source| ConnectionFileError::Io {
                op: "write_temp",
                path: temp_path.to_path_buf(),
                source,
            })?;
    }

    fs::rename(temp_path, path).map_err(|source| ConnectionFileError::Io {
        op: "rename",
        path: path.to_path_buf(),
        source,
    })?;
    Ok(())
}

fn open_owner_only_new(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    #[cfg(windows)]
    {
        // No explicit DACL is set: the connection file is published under the
        // per-user profile (XDG_RUNTIME_DIR is unset on Windows, so
        // connection_file_path() falls back to %TEMP% =
        // %LOCALAPPDATA%\Temp). That directory's inherited ACL already grants
        // access to only the owning user, SYSTEM, and Administrators — so the
        // same-host, non-admin attacker (the threat 0600 guards against on the
        // world-readable Unix /tmp) cannot read the key here. Administrators can
        // read any file (SeBackup/SeTakeOwnership) on either platform and are
        // out of scope for a same-host secret. Revisit an explicit owner-only
        // SECURITY_DESCRIPTOR only if the connection file ever moves off the
        // per-user profile directory.
    }
    options.open(path)
}

fn temp_path(parent: &Path, file_name: &std::ffi::OsStr) -> Result<PathBuf, ConnectionFileError> {
    let mut suffix = [0u8; 16];
    getrandom::getrandom(&mut suffix).map_err(ConnectionFileError::Random)?;
    let file_name = file_name.to_string_lossy();
    Ok(parent.join(format!(
        ".{file_name}.{}.{}.tmp",
        process::id(),
        hex(&suffix)
    )))
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

impl fmt::Display for ConnectionFileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingParent { path } => {
                write!(f, "connection file path has no parent: {}", path.display())
            }
            Self::MissingFileName { path } => {
                write!(
                    f,
                    "connection file path has no file name: {}",
                    path.display()
                )
            }
            Self::Io { op, path, source } => write!(
                f,
                "connection file {op} failed for {}: {source}",
                path.display()
            ),
            Self::JsonRead { path, source } => write!(
                f,
                "connection file JSON read failed for {}: {source}",
                path.display()
            ),
            Self::JsonWrite { path, source } => write!(
                f,
                "connection file JSON write failed for {}: {source}",
                path.display()
            ),
            Self::Random(source) => write!(f, "connection file random generation failed: {source}"),
            Self::UnsupportedSchema { schema, supported } => write!(
                f,
                "unsupported connection file schema {schema}; expected {supported}"
            ),
            Self::Invalid { reason } => write!(f, "invalid connection file: {reason}"),
            Self::KeyTooShort { len, min } => write!(
                f,
                "connection file key is too short: {len} bytes, need at least {min}"
            ),
        }
    }
}

impl Error for ConnectionFileError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::JsonRead { source, .. } | Self::JsonWrite { source, .. } => Some(source),
            Self::Random(_) => None,
            Self::MissingParent { .. }
            | Self::MissingFileName { .. }
            | Self::UnsupportedSchema { .. }
            | Self::Invalid { .. }
            | Self::KeyTooShort { .. } => None,
        }
    }
}
