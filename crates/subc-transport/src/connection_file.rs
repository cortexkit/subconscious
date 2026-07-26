use std::{
    error::Error,
    fmt,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    process,
    time::{Duration, SystemTime},
};

use serde::{Deserialize, Serialize};
use subc_protocol::PROTOCOL_VERSION;

pub const SCHEMA_VERSION: u32 = 1;
pub const MIN_KEY_LEN: usize = 32;
pub const KEY_LEN: usize = 32;
pub const DAEMON_ID_LEN: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionInfo {
    pub schema: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wire_version: Option<u8>,
    pub endpoints: Vec<Endpoint>,
    pub key: Vec<u8>,
    pub daemon_id: [u8; DAEMON_ID_LEN],
    pub pid: u32,
    pub daemon_ver: String,
}

// Hand-written so the transport key is never printed. A derived Debug would dump
// the raw key bytes into any log or panic message that formats a ConnectionInfo.
impl fmt::Debug for ConnectionInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConnectionInfo")
            .field("schema", &self.schema)
            .field("wire_version", &self.wire_version)
            .field("endpoints", &self.endpoints)
            .field("key", &format_args!("<{} bytes redacted>", self.key.len()))
            .field("daemon_id", &self.daemon_id)
            .field("pid", &self.pid)
            .field("daemon_ver", &self.daemon_ver)
            .finish()
    }
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

    /// Validates a declared envelope version without rejecting older files that
    /// omit the additive field.
    pub fn validate_wire_version(&self, supported: u8) -> Result<(), ConnectionFileError> {
        if let Some(file) = self.wire_version {
            if file != supported {
                return Err(ConnectionFileError::WireVersionMismatch { file, supported });
            }
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
    WireVersionMismatch {
        file: u8,
        supported: u8,
    },
    Invalid {
        reason: String,
    },
    KeyTooShort {
        len: usize,
        min: usize,
    },
    InsecurePermissions {
        path: PathBuf,
        mode: u32,
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
    // Sweep temps stranded by an earlier writer before creating our own. The
    // error path below removes this call's temp, but nothing removes one left by
    // a process that died BETWEEN create and rename -- and a connection file
    // carries the daemon's auth key, so a stranded temp is a stale credential
    // sitting in the runtime directory indefinitely. Owner-only mode means no
    // other user can read it and the key dies with the daemon that minted it;
    // the objection is to key material with no owner and no expiry, not to an
    // active leak.
    //
    // Best-effort and non-fatal: publishing must not fail because cleanup could
    // not remove somebody else's file.
    sweep_stale_temps(parent, file_name);

    let temp_path = temp_path(parent, file_name)?;
    let result = write_atomic_inner(path, &temp_path, info);
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

/// Remove `.<file_name>.<pid>.<hex>.tmp` siblings older than ten minutes.
///
/// AGE IS THE SOLE PREDICATE. Testing whether the embedded pid is alive reads
/// false-positive on exactly the oldest files, because pid numbers are recycled:
/// an unrelated long-lived process inherits the number and the stalest temp
/// looks owned. That failure direction resembles caution, which is why nobody
/// investigates the survivors. Ten minutes is far longer than the window this
/// guards, which spans two syscalls.
fn sweep_stale_temps(parent: &Path, file_name: &std::ffi::OsStr) {
    const STALE_AFTER: Duration = Duration::from_secs(600);

    let prefix = format!(".{}.", file_name.to_string_lossy());
    let Ok(entries) = fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with(&prefix) || !name.ends_with(".tmp") {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .map(|modified| {
                SystemTime::now()
                    .duration_since(modified)
                    .is_ok_and(|age| age >= STALE_AFTER)
            })
            .unwrap_or(false);
        if stale {
            let _ = fs::remove_file(entry.path());
        }
    }
}

pub fn read(path: impl AsRef<Path>) -> Result<ConnectionInfo, ConnectionFileError> {
    let path = path.as_ref();
    // Refuse to trust a key from a file other local users can read. The key is
    // published owner-only (0600); if the on-disk file is group/world-accessible
    // the secret has leaked and the daemon it points at can't be trusted.
    verify_owner_only(path)?;
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

/// Reads connection information for a client and rejects a declared envelope
/// version this binary cannot decode before a TCP connection is attempted.
pub fn read_for_client(path: impl AsRef<Path>) -> Result<ConnectionInfo, ConnectionFileError> {
    let info = read(path)?;
    info.validate_wire_version(PROTOCOL_VERSION)?;
    Ok(info)
}

#[cfg(unix)]
fn verify_owner_only(path: &Path) -> Result<(), ConnectionFileError> {
    use std::os::unix::fs::PermissionsExt;
    let meta = fs::metadata(path).map_err(|source| ConnectionFileError::Io {
        op: "stat",
        path: path.to_path_buf(),
        source,
    })?;
    let mode = meta.permissions().mode();
    // Any group or other permission bit means the key is exposed beyond the owner.
    // A file owned by a different user that we can still read implies the same.
    if mode & 0o077 != 0 {
        return Err(ConnectionFileError::InsecurePermissions {
            path: path.to_path_buf(),
            mode: mode & 0o777,
        });
    }
    Ok(())
}

#[cfg(not(unix))]
fn verify_owner_only(_path: &Path) -> Result<(), ConnectionFileError> {
    // On Windows the file inherits the per-user profile directory's ACL (owner,
    // SYSTEM, Administrators only) at create time; see open_owner_only_new. There
    // are no portable Unix mode bits to re-check on read here.
    Ok(())
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
            Self::WireVersionMismatch { file, supported } => write!(
                f,
                "connection file wire version {file} does not match supported wire version {supported}; the binary must be upgraded"
            ),
            Self::Invalid { reason } => write!(f, "invalid connection file: {reason}"),
            Self::KeyTooShort { len, min } => write!(
                f,
                "connection file key is too short: {len} bytes, need at least {min}"
            ),
            Self::InsecurePermissions { path, mode } => write!(
                f,
                "connection file {} has insecure permissions {mode:#o}; expected owner-only 0600",
                path.display()
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
            | Self::WireVersionMismatch { .. }
            | Self::Invalid { .. }
            | Self::KeyTooShort { .. }
            | Self::InsecurePermissions { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_info() -> ConnectionInfo {
        ConnectionInfo {
            schema: SCHEMA_VERSION,
            wire_version: None,
            endpoints: vec![Endpoint {
                host: "127.0.0.1".to_owned(),
                port: 8799,
            }],
            key: vec![0xABu8; KEY_LEN],
            daemon_id: [0x11u8; DAEMON_ID_LEN],
            pid: 4242,
            daemon_ver: "subc-test".to_owned(),
        }
    }

    fn unique_temp_path() -> PathBuf {
        let mut suffix = [0u8; 8];
        getrandom::getrandom(&mut suffix).expect("random suffix");
        let mut name = String::from("subc-connfile-test-");
        for byte in suffix {
            name.push_str(&format!("{byte:02x}"));
        }
        name.push_str(".json");
        std::env::temp_dir().join(name)
    }

    #[test]
    fn write_atomic_sweeps_stale_temps_and_spares_recent_and_unrelated_files() {
        let dir = std::env::temp_dir().join(format!("subc-sweep-{}", process::id()));
        fs::create_dir_all(&dir).expect("create dir");
        let target = dir.join("subc-connection.json");

        // A temp stranded by a dead writer: correct shape, old enough to sweep.
        let stale = dir.join(".subc-connection.json.99999.deadbeef.tmp");
        fs::write(&stale, b"stranded").expect("write stale");
        let old = SystemTime::now() - Duration::from_secs(3600);
        File::options()
            .write(true)
            .open(&stale)
            .expect("open stale")
            .set_modified(old)
            .expect("backdate stale");

        // A temp from a writer that may still be mid-rename: same shape, fresh.
        // Sweeping this would race a concurrent publish.
        let recent = dir.join(".subc-connection.json.99998.feedface.tmp");
        fs::write(&recent, b"in flight").expect("write recent");

        // An old file that is not one of our temps. Age alone must not condemn it.
        let unrelated = dir.join("unrelated.txt");
        fs::write(&unrelated, b"not ours").expect("write unrelated");
        File::options()
            .write(true)
            .open(&unrelated)
            .expect("open unrelated")
            .set_modified(old)
            .expect("backdate unrelated");

        write_atomic(&target, &sample_info()).expect("publish");

        assert!(!stale.exists(), "a stale temp must be swept");
        assert!(
            recent.exists(),
            "a recent temp may belong to an in-flight publish and must be spared"
        );
        assert!(
            unrelated.exists(),
            "age alone must not condemn a file that is not one of our temps"
        );
        assert!(target.exists(), "the publish itself must still land");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn debug_redacts_key_bytes() {
        let info = sample_info();
        let rendered = format!("{info:?}");
        assert!(
            rendered.contains("redacted"),
            "Debug must mark the key as redacted: {rendered}"
        );
        // The raw key byte pattern (0xab) must not appear anywhere in the output.
        assert!(
            !rendered.contains("171") && !rendered.to_lowercase().contains("ab, ab"),
            "Debug must not leak raw key bytes: {rendered}"
        );
    }

    #[test]
    fn validate_rejects_unsupported_schema_empty_endpoints_and_short_key() {
        let mut unsupported_schema = sample_info();
        unsupported_schema.schema = SCHEMA_VERSION + 1;
        let before = unsupported_schema.clone();
        let err = unsupported_schema
            .validate()
            .expect_err("unsupported schema must be rejected");
        assert!(matches!(
            err,
            ConnectionFileError::UnsupportedSchema {
                schema,
                supported: SCHEMA_VERSION,
            } if schema == SCHEMA_VERSION + 1
        ));
        assert_eq!(unsupported_schema, before, "validate must not mutate input");

        let mut empty_endpoints = sample_info();
        empty_endpoints.endpoints.clear();
        let before = empty_endpoints.clone();
        let err = empty_endpoints
            .validate()
            .expect_err("empty endpoint list must be rejected");
        assert!(matches!(
            err,
            ConnectionFileError::Invalid { ref reason }
                if reason == "connection file must include at least one endpoint"
        ));
        assert_eq!(empty_endpoints, before, "validate must not mutate input");

        let mut short_key = sample_info();
        short_key.key = vec![0xAB; MIN_KEY_LEN - 1];
        let before = short_key.clone();
        let err = short_key
            .validate()
            .expect_err("short key must be rejected");
        assert!(matches!(
            err,
            ConnectionFileError::KeyTooShort {
                len,
                min: MIN_KEY_LEN,
            } if len == MIN_KEY_LEN - 1
        ));
        assert_eq!(short_key, before, "validate must not mutate input");
    }

    #[test]
    fn optional_wire_version_round_trips() {
        let path = unique_temp_path();
        let legacy = sample_info();
        write_atomic(&path, &legacy).expect("write legacy connection file");
        let legacy_json = fs::read_to_string(&path).expect("read legacy connection file");
        assert!(!legacy_json.contains("wire_version"));
        assert_eq!(
            read_for_client(&path).expect("legacy file remains readable"),
            legacy
        );

        let mut current = sample_info();
        current.wire_version = Some(PROTOCOL_VERSION);
        write_atomic(&path, &current).expect("write current connection file");
        let current_json = fs::read_to_string(&path).expect("read current connection file");
        let current_json: serde_json::Value =
            serde_json::from_str(&current_json).expect("parse current connection file");
        assert_eq!(
            current_json["wire_version"].as_u64(),
            Some(u64::from(PROTOCOL_VERSION))
        );
        assert_eq!(
            read_for_client(&path).expect("current file is readable"),
            current
        );
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn read_for_client_rejects_mismatched_wire_version() {
        let path = unique_temp_path();
        let mut info = sample_info();
        let file_version = PROTOCOL_VERSION + 1;
        info.wire_version = Some(file_version);
        write_atomic(&path, &info).expect("write mismatched connection file");

        let err = read_for_client(&path).expect_err("mismatched wire version must fail discovery");
        assert!(matches!(
            err,
            ConnectionFileError::WireVersionMismatch { file, supported }
                if file == file_version && supported == PROTOCOL_VERSION
        ));
        let rendered = err.to_string();
        assert!(rendered.contains(&file_version.to_string()));
        assert!(rendered.contains(&PROTOCOL_VERSION.to_string()));
        assert!(rendered.contains("binary must be upgraded"));
        let _ = fs::remove_file(&path);
    }

    #[cfg(unix)]
    #[test]
    fn read_rejects_group_or_world_readable_file() {
        use std::os::unix::fs::PermissionsExt;

        let path = unique_temp_path();
        write_atomic(&path, &sample_info()).expect("write owner-only file");
        // Loosen permissions as if the key leaked to other local users.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("relax permissions");

        let err = read(&path).expect_err("group/world-readable key file must be rejected");
        assert!(
            matches!(err, ConnectionFileError::InsecurePermissions { mode, .. } if mode == 0o644),
            "expected InsecurePermissions, got {err:?}"
        );
        let _ = fs::remove_file(&path);
    }
}
