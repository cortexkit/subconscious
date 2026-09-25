use std::{
    env,
    error::Error,
    ffi::{OsStr, OsString},
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

/// The daemon's connection-file name. Public so the writer (bootstrap) and
/// every reader spell it once; three private copies of this literal used to
/// exist and nothing asserted they agreed.
pub const CONNECTION_FILE_NAME: &str = "subc-connection.json";
const PROD_CONNECTION_RELATIVE_PATH: &[&str] =
    &[".local", "share", "cortexkit", "run", CONNECTION_FILE_NAME];

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

/// A connection file selected by reader-side discovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovered {
    pub path: PathBuf,
    pub info: ConnectionInfo,
}

/// One connection-file candidate that could not be read or parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriedCandidate {
    pub path: PathBuf,
    pub reason: String,
}

/// Every connection-file candidate reader-side discovery tried.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryError {
    pub tried: Vec<TriedCandidate>,
}

impl fmt::Display for DiscoveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let rendered = self
            .tried
            .iter()
            .map(|attempt| format!("{} ({})", attempt.path.display(), attempt.reason))
            .collect::<Vec<_>>()
            .join(", ");
        write!(f, "no usable subc connection file found; tried: {rendered}")
    }
}

impl Error for DiscoveryError {}

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
    InsecureParentDirectory {
        component: PathBuf,
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
    ensure_parent_directory(parent)?;
    refuse_writable_ancestor(parent)?;
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

/// Create the connection file's parent with owner-only permissions when it is
/// absent.
///
/// The daemon OWNS this directory, so it can make the misconfiguration
/// unproducible rather than only reporting it — which is strictly stronger than
/// the check below and is available to us precisely because we choose the path.
/// A caller that names its own destination has only the check.
///
/// An existing directory is left alone: changing modes under an operator is a
/// bigger act than refusing, and `refuse_writable_ancestor` reports it.
fn ensure_parent_directory(parent: &Path) -> Result<(), ConnectionFileError> {
    if parent.exists() {
        return Ok(());
    }
    #[cfg(unix)]
    let created = {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
    };
    #[cfg(not(unix))]
    let created = fs::create_dir_all(parent);

    created.map_err(|source| ConnectionFileError::Io {
        op: "create connection-file parent",
        path: parent.to_path_buf(),
        source,
    })
}

/// Refuse to publish key material beneath a directory another user can write.
///
/// A LINT AGAINST MISCONFIGURATION, NOT A SECURITY BOUNDARY. It does nothing
/// against a same-uid adversary, who can read the finished 0600 file anyway. It
/// catches the cross-uid case, which the same-uid concession does NOT cover: a
/// group- or world-writable ancestor lets another user UNLINK the 0600 file and
/// substitute their own, because directory write permission governs create and
/// unlink rather than the target's mode. The file's own mode does not close it
/// and neither does its ownership.
///
/// EVERY ANCESTOR, UP TO `/`. Stopping at `$HOME` or an XDG base would read the
/// bound from the environment, so it would be attacker-influenceable and
/// undefined when unset. It is also incorrect: an attacker who can unlink in ANY
/// ancestor renames an intermediate directory aside and substitutes their own
/// tree, so a 0700 leaf under a 0777 grandparent protects nothing. Every
/// component or the guarantee does not compose.
///
/// CANONICALISE FIRST. An unresolved walk checks the modes of a path that is not
/// the one we write through: a symlink component pointing somewhere permissive
/// defeats the walk while every individual `stat` passes.
///
/// STICKY EXEMPTS. `/tmp` and `/Users/Shared` are 1777 by design; without the
/// exemption this fires on correctly-configured systems, and a check that
/// refuses healthy configuration gets disabled — after which it protects nothing
/// at all.
#[cfg(unix)]
fn refuse_writable_ancestor(parent: &Path) -> Result<(), ConnectionFileError> {
    use std::os::unix::fs::PermissionsExt;

    const GROUP_OR_WORLD_WRITABLE: u32 = 0o022;
    const STICKY: u32 = 0o1000;

    // A parent that cannot be canonicalised is reported by the write itself with
    // its own io::Error; refusing here would replace a precise errno with a
    // permissions verdict about a path we could not resolve.
    let Ok(resolved) = fs::canonicalize(parent) else {
        return Ok(());
    };

    let mut component = resolved.as_path();
    loop {
        // A component whose metadata cannot be read is SKIPPED, not refused, and
        // the silence is deliberate. Canonicalising above already required
        // traverse permission on every component, so a failure here is close to
        // unreachable and is a transient io error rather than evidence about
        // permissions; refusing on it would convert that error into a confident
        // verdict about a mode we never observed, which is the same trade the
        // canonicalise arm declines. Argued at the site because an UNARGUED
        // fail-open is the one a later reader tightens into a refusal.
        if let Ok(metadata) = fs::metadata(component) {
            let mode = metadata.permissions().mode();
            if mode & GROUP_OR_WORLD_WRITABLE != 0 && mode & STICKY == 0 {
                return Err(ConnectionFileError::InsecureParentDirectory {
                    component: component.to_path_buf(),
                    mode: mode & 0o7777,
                });
            }
        }
        match component.parent() {
            Some(next) => component = next,
            None => return Ok(()),
        }
    }
}

#[cfg(not(unix))]
fn refuse_writable_ancestor(_parent: &Path) -> Result<(), ConnectionFileError> {
    // Windows ACLs are not a mode bitmask and the Unix reasoning does not carry.
    // Stated rather than silently skipped so the absence is a decision.
    Ok(())
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

/// The paths a reader consults, most specific first. `explicit` is the
/// caller's own override (a `--subc` flag); `env_named` is the value of
/// `SUBC_CONNECTION_FILE` read by the caller. Either, when present, is the
/// ONLY candidate.
pub fn discovery_candidates(explicit: Option<&Path>, env_named: Option<&OsStr>) -> Vec<PathBuf> {
    let runtime_dir = non_empty_os_var("XDG_RUNTIME_DIR");
    let home = non_empty_os_var("HOME");
    discovery_candidates_with_environment(
        explicit,
        env_named,
        runtime_dir.as_deref(),
        home.as_deref(),
        &env::temp_dir(),
    )
}

/// Read the reader-side environment and return the first usable connection file.
/// Unlike writer-side `subc_daemon::bootstrap::connection_file_path()`, this searches
/// every location where an already-running daemon may have written its file.
pub fn discover(explicit: Option<&Path>) -> Result<Discovered, DiscoveryError> {
    let env_named = non_empty_os_var("SUBC_CONNECTION_FILE");
    discover_candidates(discovery_candidates(explicit, env_named.as_deref()))
}

fn discovery_candidates_with_environment(
    explicit: Option<&Path>,
    env_named: Option<&OsStr>,
    runtime_dir: Option<&OsStr>,
    home: Option<&OsStr>,
    temp_dir: &Path,
) -> Vec<PathBuf> {
    if let Some(path) = explicit {
        return vec![path.to_path_buf()];
    }

    let env_named = env_named.filter(|value| !value.is_empty());
    let runtime_dir = runtime_dir.filter(|value| !value.is_empty());

    // SUBC_CONNECTION_FILE names the daemon the caller means, so it is EXCLUSIVE
    // rather than first-in-a-list. It used to be pushed ahead of the discovery
    // candidates, which reads as honouring it and is not: a path that is set and
    // wrong falls through to discovery and answers from whichever daemon is found
    // -- in practice production. The reply is then true and about the wrong
    // machine, and every later verdict inherits that while the operator believes
    // they are reading a rig.
    //
    // A fallback is only a hazard where the primary is optional, so removing the
    // fallback for a deliberately supplied value removes the class. Returning a
    // single candidate keeps the existing error path: the file is stat-ed, and an
    // unreadable one is reported as a failure naming that path.
    if let Some(only) = env_named {
        return vec![PathBuf::from(only)];
    }

    let mut candidates = Vec::new();
    if let Some(runtime_dir) = runtime_dir {
        push_unique(
            &mut candidates,
            PathBuf::from(runtime_dir).join(CONNECTION_FILE_NAME),
        );
    }
    if let Some(home) = home {
        let mut path = PathBuf::from(home);
        for part in PROD_CONNECTION_RELATIVE_PATH {
            path.push(part);
        }
        push_unique(&mut candidates, path);
    }
    push_unique(
        &mut candidates,
        temp_dir.join(format!("subc-{}.connection.json", user_connection_token())),
    );
    candidates
}

fn discover_candidates(candidates: Vec<PathBuf>) -> Result<Discovered, DiscoveryError> {
    let mut tried = Vec::new();
    for path in candidates {
        match read_for_client(&path) {
            Ok(info) => return Ok(Discovered { path, info }),
            Err(source) => tried.push(TriedCandidate {
                path,
                reason: discovery_reason(&source),
            }),
        }
    }
    Err(DiscoveryError { tried })
}

fn push_unique(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.iter().any(|existing| existing == &path) {
        paths.push(path);
    }
}

fn non_empty_os_var(key: &str) -> Option<OsString> {
    let value = env::var_os(key)?;
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

fn discovery_reason(source: &ConnectionFileError) -> String {
    match source {
        ConnectionFileError::Io { source, .. } if source.kind() == io::ErrorKind::NotFound => {
            "not found".to_string()
        }
        other => other.to_string(),
    }
}

/// The per-user component of the temp-fallback connection-file name.
///
/// The daemon writer and every reader use this one implementation so a naming
/// drift cannot make a running daemon appear absent.
pub fn user_connection_token() -> String {
    // On Unix the token is the real uid, read from the kernel. Identity must not
    // depend on a fallible filesystem probe because a transient failure would
    // make the same user derive a different connection-file name.
    #[cfg(unix)]
    {
        rustix::process::getuid().as_raw().to_string()
    }

    #[cfg(not(unix))]
    {
        for key in ["USER", "USERNAME", "HOME", "USERPROFILE"] {
            if let Some(value) = non_empty_os_var(key) {
                return sanitize_token(&value.to_string_lossy());
            }
        }

        "unknown".to_string()
    }
}

#[cfg(not(unix))]
fn sanitize_token(raw: &str) -> String {
    let mut token = String::new();
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
            token.push(ch);
        } else {
            token.push('_');
        }
    }
    if token.is_empty() {
        "unknown".to_string()
    } else {
        token
    }
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
            Self::InsecureParentDirectory { component, mode } => write!(
                f,
                "refusing to publish the connection file: ancestor {} is mode {mode:#o}, \
                 which lets another user replace the file regardless of its own 0600 mode; \
                 this is a misconfiguration check, not a defence against a same-uid caller",
                component.display()
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
            | Self::InsecureParentDirectory { .. }
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
    use subc_test_support::TestTempDir;

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

    fn unique_temp_dir(label: &str) -> TestTempDir {
        TestTempDir::new(label)
    }

    /// A GROUP-writable ancestor must refuse, and the passing control on the same
    /// fixture minus the group bit is what makes this test discriminate.
    ///
    /// Both arms are here because the refusal MESSAGE is not the property: a test
    /// matching on message text passes through a removed check whenever the
    /// wording survives, which is how the adjacent guard in claustrum kept three
    /// green tests while examining only `0o002`. The arms differ by one bit on
    /// one directory and nothing else.
    #[cfg(unix)]
    #[test]
    fn a_group_writable_ancestor_refuses_and_the_same_tree_without_the_bit_publishes() {
        use std::os::unix::fs::PermissionsExt;

        for (ancestor_mode, expect_refusal) in [(0o770, true), (0o750, false)] {
            let root = unique_temp_dir(&format!("ancestor-{ancestor_mode:o}"));
            let ancestor = root.join("ancestor");
            let leaf = ancestor.join("run");
            fs::create_dir_all(&leaf).expect("create leaf");
            fs::set_permissions(&leaf, fs::Permissions::from_mode(0o700))
                .expect("tighten the leaf so only the ancestor differs");
            fs::set_permissions(&ancestor, fs::Permissions::from_mode(ancestor_mode))
                .expect("set ancestor mode");

            let result = write_atomic(leaf.join(CONNECTION_FILE_NAME), &sample_info());

            match (expect_refusal, result) {
                (true, Err(ConnectionFileError::InsecureParentDirectory { component, mode })) => {
                    let expected = ancestor.canonicalize().unwrap_or_else(|_| ancestor.clone());
                    assert_eq!(component, expected);
                    assert_eq!(mode & 0o020, 0o020, "the group bit is what refused");
                }
                (true, other) => panic!("a group-writable ancestor must refuse, got {other:?}"),
                (false, Ok(())) => {}
                (false, other) => panic!("0o750 is not writable by another user: {other:?}"),
            }

            let _ = fs::set_permissions(&ancestor, fs::Permissions::from_mode(0o700));
            let _ = fs::remove_dir_all(root);
        }
    }

    /// `/tmp` is 1777 by design. Without the sticky exemption this check fires on
    /// every correctly-configured system, and a check that refuses healthy
    /// configuration gets disabled -- after which it protects nothing.
    #[cfg(unix)]
    #[test]
    fn a_sticky_world_writable_ancestor_publishes() {
        use std::os::unix::fs::PermissionsExt;

        let root = unique_temp_dir("ancestor-sticky");
        let ancestor = root.join("sticky");
        let leaf = ancestor.join("run");
        fs::create_dir_all(&leaf).expect("create leaf");
        fs::set_permissions(&leaf, fs::Permissions::from_mode(0o700)).expect("tighten leaf");
        fs::set_permissions(&ancestor, fs::Permissions::from_mode(0o1777)).expect("sticky 1777");

        let published = write_atomic(leaf.join(CONNECTION_FILE_NAME), &sample_info());
        assert!(
            published.is_ok(),
            "a sticky 1777 ancestor is /tmp's own shape and must publish: {published:?}"
        );

        let _ = fs::set_permissions(&ancestor, fs::Permissions::from_mode(0o700));
        let _ = fs::remove_dir_all(root);
    }

    /// The daemon owns this directory, so the misconfiguration is unproducible
    /// rather than merely reported when the directory does not yet exist.
    #[cfg(unix)]
    #[test]
    fn an_absent_parent_is_created_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let root = unique_temp_dir("absent-parent");
        let parent = root.join("run");
        assert!(!parent.exists(), "fixture must start with no parent");

        write_atomic(parent.join(CONNECTION_FILE_NAME), &sample_info()).expect("publishes");

        let mode = fs::metadata(&parent)
            .expect("parent exists")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700, "an absent parent is created owner-only");

        let _ = fs::remove_dir_all(root);
    }

    fn prod_connection_file(home: &Path) -> PathBuf {
        let mut path = home.to_path_buf();
        for part in PROD_CONNECTION_RELATIVE_PATH {
            path.push(part);
        }
        path
    }

    #[test]
    fn an_explicit_path_is_the_only_discovery_candidate() {
        let explicit = PathBuf::from("/rig/explicit.json");
        let env_named = OsStr::new("/rig/from-env.json");

        assert_eq!(
            discovery_candidates(Some(&explicit), Some(env_named)),
            vec![explicit],
            "the caller's explicit override must exclude every fallback"
        );
    }

    #[test]
    fn set_and_wrong_environment_path_fails_without_fallback() {
        let root = unique_temp_dir("env-exclusive");
        let runtime = root.join("runtime");
        let home = root.join("home");
        let temp = root.join("temp");
        let named = root.join("missing-rig.json");
        let production = prod_connection_file(&home);
        fs::create_dir_all(production.parent().expect("production parent"))
            .expect("create production parent");
        write_atomic(&production, &sample_info()).expect("write discoverable production file");

        assert_eq!(
            discovery_candidates(None, Some(named.as_os_str())),
            vec![named.clone()],
            "a named connection file must not be followed by discovery paths"
        );
        let candidates = discovery_candidates_with_environment(
            None,
            Some(named.as_os_str()),
            Some(runtime.as_os_str()),
            Some(home.as_os_str()),
            &temp,
        );
        let error = discover_candidates(candidates)
            .expect_err("set-and-wrong SUBC_CONNECTION_FILE must fail rather than use production");

        assert_eq!(
            error.tried,
            vec![TriedCandidate {
                path: named.clone(),
                reason: "not found".to_owned(),
            }],
            "the named rig path must be the only attempted file"
        );
        assert!(
            error.to_string().contains(&named.display().to_string()),
            "the failure must name the operator-selected rig path"
        );
        drop(root);
    }

    #[test]
    fn empty_environment_paths_are_unset_and_fallback_candidates_are_absolute() {
        let root = unique_temp_dir("empty-candidates");
        let runtime = root.join("runtime");
        let home = root.join("home");
        let temp = root.join("temp");
        let empty = OsStr::new("");

        let without_named_override = discovery_candidates_with_environment(
            None,
            None,
            Some(runtime.as_os_str()),
            Some(home.as_os_str()),
            &temp,
        );
        let with_empty_named_override = discovery_candidates_with_environment(
            None,
            Some(empty),
            Some(runtime.as_os_str()),
            Some(home.as_os_str()),
            &temp,
        );
        assert_eq!(with_empty_named_override, without_named_override);

        let without_runtime =
            discovery_candidates_with_environment(None, None, None, Some(home.as_os_str()), &temp);
        let with_empty_runtime = discovery_candidates_with_environment(
            None,
            None,
            Some(empty),
            Some(home.as_os_str()),
            &temp,
        );
        assert_eq!(with_empty_runtime, without_runtime);
        assert!(
            with_empty_named_override.iter().all(|path| path.is_absolute())
                && with_empty_runtime.iter().all(|path| path.is_absolute()),
            "every fallback candidate must be absolute: {with_empty_named_override:?} {with_empty_runtime:?}"
        );

        drop(root);
    }

    #[test]
    fn discovery_without_overrides_keeps_three_rung_order_and_deduplicates() {
        let root = unique_temp_dir("candidate-order");
        let runtime = root.join("runtime");
        let home = root.join("home");
        let temp = root.join("temp");

        let candidates = discovery_candidates_with_environment(
            None,
            None,
            Some(runtime.as_os_str()),
            Some(home.as_os_str()),
            &temp,
        );
        assert_eq!(
            candidates,
            vec![
                runtime.join(CONNECTION_FILE_NAME),
                prod_connection_file(&home),
                temp.join(format!("subc-{}.connection.json", user_connection_token())),
            ],
            "readers must try runtime, production, then the per-user temp fallback"
        );

        let production = prod_connection_file(&home);
        let production_dir = production.parent().expect("production directory");
        let deduplicated = discovery_candidates_with_environment(
            None,
            None,
            Some(production_dir.as_os_str()),
            Some(home.as_os_str()),
            &temp,
        );
        assert_eq!(
            deduplicated,
            vec![
                production,
                temp.join(format!("subc-{}.connection.json", user_connection_token())),
            ],
            "one path reached through two rungs must only be tried once"
        );
        drop(root);
    }

    #[test]
    fn discover_on_a_temp_home_returns_the_parsed_production_file() {
        const CHILD_MARKER: &str = "SUBC_TRANSPORT_DISCOVERY_CHILD_EXPECTED";
        if let Some(expected) = env::var_os(CHILD_MARKER) {
            let expected = PathBuf::from(expected);
            let discovered = discover(None).expect("discover production connection file");
            assert_eq!(discovered.path, expected);
            assert_eq!(discovered.info, sample_info());
            return;
        }

        let root = unique_temp_dir("discover-home");
        let home = root.join("home");
        let temp = root.join("temp");
        fs::create_dir_all(&temp).expect("create child temp directory");
        let production = prod_connection_file(&home);
        fs::create_dir_all(production.parent().expect("production parent"))
            .expect("create production parent");
        write_atomic(&production, &sample_info()).expect("write production connection file");

        // Run the public environment-reading API in a child so this test does not
        // mutate process-global environment while sibling tests execute.
        let output = process::Command::new(env::current_exe().expect("current test executable"))
            .args([
                "--exact",
                "connection_file::tests::discover_on_a_temp_home_returns_the_parsed_production_file",
                "--nocapture",
            ])
            .env(CHILD_MARKER, &production)
            .env_remove("SUBC_CONNECTION_FILE")
            .env_remove("XDG_RUNTIME_DIR")
            .env("HOME", &home)
            .env("TMPDIR", &temp)
            .env("TMP", &temp)
            .env("TEMP", &temp)
            .output()
            .expect("run isolated discovery child");
        assert!(
            output.status.success(),
            "discovery child failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        drop(root);
    }

    #[test]
    fn write_atomic_sweeps_stale_temps_and_spares_recent_and_unrelated_files() {
        let dir = TestTempDir::new("subc-sweep");
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
