use std::{
    env,
    error::Error,
    ffi::OsString,
    fmt, fs, io,
    path::{Path, PathBuf},
    process,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use subc_protocol::{Flags, FrameType, Priority};
use tokio::{
    net::{UnixListener, UnixStream},
    time::{sleep, timeout},
};
use tracing::{info, warn};

use crate::{
    read_frame,
    server::{serve_listener, ServerError},
    write_frame, Frame, FrameBuildError, FrameIoError, Router,
};

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};

const SOCKET_MODE: u32 = 0o600;
const PING_CORR: u64 = 0x5355_4243_5049_4e47; // "SUBCPING"
const PING_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_BIND_RACE_RETRIES: usize = 8;
const START_LOCK_RETRIES: usize = 40;
const START_LOCK_RETRY_DELAY: Duration = Duration::from_millis(25);

/// Result of singleton discovery.
#[derive(Debug)]
pub enum Outcome {
    /// A live daemon answered on the socket path; this invocation should exit 0.
    AlreadyRunning,
    /// This process won the singleton race and owns the bound listener.
    Bound(UnixListener),
}

/// Resolve subc's per-user Unix-domain socket path.
///
/// `$XDG_RUNTIME_DIR/subc.sock` is preferred because the runtime directory is
/// already per-user on Unix desktops. Without it, subc falls back to the system
/// temp dir with a user token in the filename so different OS users do not
/// collide on shared `/tmp`-style directories.
pub fn socket_path() -> PathBuf {
    if let Some(runtime_dir) = non_empty_os_var("XDG_RUNTIME_DIR") {
        return PathBuf::from(runtime_dir).join("subc.sock");
    }

    env::temp_dir().join(format!("subc-{}.sock", user_socket_token()))
}

/// Resolve, claim, and serve the per-user daemon singleton.
///
/// A second invocation is successful: if a live daemon answers PING/PONG, this
/// returns `Ok(())` after logging and the caller exits with status 0.
pub async fn run() -> Result<(), BootstrapError> {
    let path = socket_path();
    match ensure_singleton(&path).await? {
        Outcome::AlreadyRunning => {
            info!(socket = %path.display(), "subc daemon already running");
            Ok(())
        }
        Outcome::Bound(listener) => {
            info!(socket = %path.display(), "subc daemon starting");
            let router = Arc::new(Router::with_default_self_handler());
            serve_listener(listener, router)
                .await
                .map_err(BootstrapError::Serve)
        }
    }
}

/// Find an existing daemon or atomically bind `path` for this daemon.
///
/// The algorithm is intentionally connect-first: a listener that accepts a
/// connection and answers the channel-0 PING/PONG control frame is treated as
/// live. A listener that accepts but does not PONG is considered live-but-foreign
/// and is never clobbered. Stale socket files are reclaimed only while holding a
/// short-lived start lock, then `bind` remains the final atomic singleton guard.
pub async fn ensure_singleton(path: impl AsRef<Path>) -> Result<Outcome, BootstrapError> {
    let path = path.as_ref().to_path_buf();

    for attempt in 0..MAX_BIND_RACE_RETRIES {
        if matches!(probe_existing(&path).await?, Probe::Live) {
            return Ok(Outcome::AlreadyRunning);
        }

        let _lock = StartLock::acquire(&path).await?;

        // Re-probe after acquiring the start lock so a peer that won the race
        // between our first failed connect and the lock acquisition is observed
        // instead of unlinked.
        if matches!(probe_existing(&path).await?, Probe::Live) {
            return Ok(Outcome::AlreadyRunning);
        }

        remove_stale_path_if_present(&path)?;

        match UnixListener::bind(&path) {
            Ok(listener) => {
                let listener = set_owner_only_permissions(&path, listener)?;
                return Ok(Outcome::Bound(listener));
            }
            Err(err) if err.kind() == io::ErrorKind::AddrInUse => {
                warn!(
                    socket = %path.display(),
                    attempt = attempt + 1,
                    "socket bind raced with another process; retrying discovery"
                );
                continue;
            }
            Err(source) => return Err(BootstrapError::Bind { path, source }),
        }
    }

    Err(BootstrapError::BindRaceExhausted {
        path,
        attempts: MAX_BIND_RACE_RETRIES,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Probe {
    Live,
    StaleOrAbsent,
}

async fn probe_existing(path: &Path) -> Result<Probe, BootstrapError> {
    match UnixStream::connect(path).await {
        Ok(stream) => {
            confirm_subc_pong(path, stream).await?;
            Ok(Probe::Live)
        }
        Err(err) if is_stale_or_absent_connect_error(&err) => Ok(Probe::StaleOrAbsent),
        Err(source) => Err(BootstrapError::ConnectProbe {
            path: path.to_path_buf(),
            source,
        }),
    }
}

async fn confirm_subc_pong(path: &Path, mut stream: UnixStream) -> Result<(), BootstrapError> {
    let ping = Frame::build(
        FrameType::Ping,
        Flags::new(false, Priority::Passive, false),
        0,
        PING_CORR,
        Vec::new(),
    )
    .map_err(BootstrapError::PingFrameBuild)?;

    write_frame(&mut stream, &ping)
        .await
        .map_err(|source| BootstrapError::PingWrite {
            path: path.to_path_buf(),
            source,
        })?;

    let frame = timeout(PING_TIMEOUT, read_frame(&mut stream))
        .await
        .map_err(|_| BootstrapError::PingTimeout {
            path: path.to_path_buf(),
            timeout: PING_TIMEOUT,
        })?
        .map_err(|source| BootstrapError::PingRead {
            path: path.to_path_buf(),
            source,
        })?;

    let Some(frame) = frame else {
        return Err(BootstrapError::ForeignSocket {
            path: path.to_path_buf(),
            reason: "peer closed before PONG".to_string(),
        });
    };

    if frame.header.ty == FrameType::Pong
        && frame.header.channel == 0
        && frame.header.corr == PING_CORR
        && frame.body.is_empty()
    {
        return Ok(());
    }

    Err(BootstrapError::ForeignSocket {
        path: path.to_path_buf(),
        reason: format!(
            "expected channel-0 PONG corr {PING_CORR}, got {:?} channel {} corr {} ({} body bytes)",
            frame.header.ty,
            frame.header.channel,
            frame.header.corr,
            frame.body.len()
        ),
    })
}

fn remove_stale_path_if_present(path: &Path) -> Result<(), BootstrapError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(BootstrapError::RemoveStale {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn set_owner_only_permissions(
    path: &Path,
    listener: UnixListener,
) -> Result<UnixListener, BootstrapError> {
    if let Err(source) = fs::set_permissions(path, fs::Permissions::from_mode(SOCKET_MODE)) {
        drop(listener);
        let _ = fs::remove_file(path);
        return Err(BootstrapError::SetPermissions {
            path: path.to_path_buf(),
            source,
        });
    }

    Ok(listener)
}

fn is_stale_or_absent_connect_error(err: &io::Error) -> bool {
    if matches!(
        err.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
    ) {
        return true;
    }

    // Connecting to a regular file at the socket path returns ENOTSOCK. The
    // value is 88 on Linux and 38 on Darwin/BSD; keep the constants local to
    // avoid pulling in libc or unsafe just for this classification.
    matches!(err.raw_os_error(), Some(88) | Some(38))
}

struct StartLock {
    path: PathBuf,
}

impl StartLock {
    async fn acquire(socket_path: &Path) -> Result<Self, BootstrapError> {
        let path = start_lock_path(socket_path);
        for _ in 0..START_LOCK_RETRIES {
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                    sleep(START_LOCK_RETRY_DELAY).await;
                }
                Err(source) => return Err(BootstrapError::StartLockCreate { path, source }),
            }
        }

        Err(BootstrapError::StartLockBusy {
            path,
            attempts: START_LOCK_RETRIES,
        })
    }
}

impl Drop for StartLock {
    fn drop(&mut self) {
        let _ = fs::remove_dir(&self.path);
    }
}

fn start_lock_path(socket_path: &Path) -> PathBuf {
    let file_name = socket_path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_else(|| "subc.sock".into());
    let lock_name = format!("{file_name}.lock");
    socket_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .join(lock_name)
}

fn non_empty_os_var(key: &str) -> Option<OsString> {
    let value = env::var_os(key)?;
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

fn user_socket_token() -> String {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let probe_path = env::temp_dir().join(format!(".subc-uid-probe-{}-{nonce}", process::id()));

    let uid = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe_path)
        .ok()
        .and_then(|file| {
            let uid = file.metadata().ok().map(|metadata| metadata.uid());
            drop(file);
            let _ = fs::remove_file(&probe_path);
            uid
        });

    uid.map_or_else(|| "unknown".to_string(), |uid| uid.to_string())
}

/// Bootstrap-layer errors are deliberately typed so startup never panics for
/// ordinary daemon-discovery races or stale filesystem state.
#[derive(Debug)]
pub enum BootstrapError {
    ConnectProbe { path: PathBuf, source: io::Error },
    PingFrameBuild(FrameBuildError),
    PingWrite { path: PathBuf, source: FrameIoError },
    PingRead { path: PathBuf, source: FrameIoError },
    PingTimeout { path: PathBuf, timeout: Duration },
    ForeignSocket { path: PathBuf, reason: String },
    StartLockCreate { path: PathBuf, source: io::Error },
    StartLockBusy { path: PathBuf, attempts: usize },
    RemoveStale { path: PathBuf, source: io::Error },
    Bind { path: PathBuf, source: io::Error },
    BindRaceExhausted { path: PathBuf, attempts: usize },
    SetPermissions { path: PathBuf, source: io::Error },
    Serve(ServerError),
}

impl fmt::Display for BootstrapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ConnectProbe { path, source } => {
                write!(f, "failed to probe socket {}: {source}", path.display())
            }
            Self::PingFrameBuild(err) => write!(f, "failed to build startup PING: {err}"),
            Self::PingWrite { path, source } => {
                write!(f, "failed to write PING to {}: {source}", path.display())
            }
            Self::PingRead { path, source } => {
                write!(f, "failed to read PONG from {}: {source}", path.display())
            }
            Self::PingTimeout { path, timeout } => write!(
                f,
                "timed out after {:?} waiting for PONG from {}",
                timeout,
                path.display()
            ),
            Self::ForeignSocket { path, reason } => write!(
                f,
                "socket {} is occupied by a live non-subc peer: {reason}",
                path.display()
            ),
            Self::StartLockCreate { path, source } => {
                write!(f, "failed to create start lock {}: {source}", path.display())
            }
            Self::StartLockBusy { path, attempts } => write!(
                f,
                "start lock {} remained busy after {attempts} attempts",
                path.display()
            ),
            Self::RemoveStale { path, source } => {
                write!(f, "failed to remove stale socket {}: {source}", path.display())
            }
            Self::Bind { path, source } => {
                write!(f, "failed to bind socket {}: {source}", path.display())
            }
            Self::BindRaceExhausted { path, attempts } => write!(
                f,
                "socket {} was won by another process but never became connectable after {attempts} retries",
                path.display()
            ),
            Self::SetPermissions { path, source } => write!(
                f,
                "failed to set socket permissions 0600 on {}: {source}",
                path.display()
            ),
            Self::Serve(err) => write!(f, "daemon server failed: {err}"),
        }
    }
}

impl Error for BootstrapError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ConnectProbe { source, .. }
            | Self::StartLockCreate { source, .. }
            | Self::RemoveStale { source, .. }
            | Self::Bind { source, .. }
            | Self::SetPermissions { source, .. } => Some(source),
            Self::PingFrameBuild(err) => Some(err),
            Self::PingWrite { source, .. } | Self::PingRead { source, .. } => Some(source),
            Self::Serve(err) => Some(err),
            Self::PingTimeout { .. }
            | Self::ForeignSocket { .. }
            | Self::StartLockBusy { .. }
            | Self::BindRaceExhausted { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::Mutex, time::SystemTime};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &Path) -> Self {
            let previous = env::var_os(key);
            env::set_var(key, value);
            Self { key, previous }
        }

        fn unset(key: &'static str) -> Self {
            let previous = env::var_os(key);
            env::remove_var(key);
            Self { key, previous }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => env::set_var(self.key, value),
                None => env::remove_var(self.key),
            }
        }
    }

    fn unique_temp_dir(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        env::temp_dir().join(format!("subc-core-{name}-{}-{nonce}", process::id()))
    }

    fn temp_socket_path(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        // Keep the whole UDS path below Darwin/BSD sun_path limits;
        // std::env::temp_dir() is often too long on macOS.
        let dir = PathBuf::from("/tmp").join(format!("sc-{}-{nonce}", process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir.join(format!("{name}.sock"))
    }

    fn cleanup_socket_path(path: &Path) {
        let _ = fs::remove_file(path);
        let _ = fs::remove_dir(start_lock_path(path));
        if let Some(parent) = path.parent() {
            let _ = fs::remove_dir_all(parent);
        }
    }

    #[test]
    fn socket_path_uses_xdg_runtime_dir_when_set() {
        let _env_lock = ENV_LOCK.lock().unwrap();
        let runtime_dir = unique_temp_dir("xdg-runtime");
        fs::create_dir_all(&runtime_dir).unwrap();
        let _xdg = EnvGuard::set("XDG_RUNTIME_DIR", &runtime_dir);

        assert_eq!(socket_path(), runtime_dir.join("subc.sock"));

        let _ = fs::remove_dir_all(runtime_dir);
    }

    #[test]
    fn socket_path_falls_back_to_temp_dir_with_user_token_when_xdg_unset() {
        let _env_lock = ENV_LOCK.lock().unwrap();
        let _xdg = EnvGuard::unset("XDG_RUNTIME_DIR");

        assert_eq!(
            socket_path(),
            env::temp_dir().join(format!("subc-{}.sock", user_socket_token()))
        );
    }

    #[tokio::test]
    async fn second_singleton_probe_against_served_socket_reports_already_running() {
        let path = temp_socket_path("already-running");

        let listener = match ensure_singleton(&path).await.unwrap() {
            Outcome::Bound(listener) => listener,
            Outcome::AlreadyRunning => panic!("fresh temp socket unexpectedly had a daemon"),
        };
        let server = tokio::spawn(serve_listener(
            listener,
            Arc::new(Router::with_default_self_handler()),
        ));

        let second = ensure_singleton(&path).await.unwrap();
        assert!(matches!(second, Outcome::AlreadyRunning));

        server.abort();
        let _ = server.await;
        cleanup_socket_path(&path);
    }

    #[tokio::test]
    async fn stale_socket_file_is_reclaimed() {
        let path = temp_socket_path("stale-reclaim");
        let stale_listener = UnixListener::bind(&path).unwrap();
        drop(stale_listener);

        let listener = match ensure_singleton(&path).await.unwrap() {
            Outcome::Bound(listener) => listener,
            Outcome::AlreadyRunning => panic!("stale socket was misdetected as live"),
        };

        drop(listener);
        cleanup_socket_path(&path);
    }

    #[tokio::test]
    async fn touched_stale_file_is_reclaimed() {
        let path = temp_socket_path("regular-file-reclaim");
        fs::write(&path, b"not a socket").unwrap();

        let listener = match ensure_singleton(&path).await.unwrap() {
            Outcome::Bound(listener) => listener,
            Outcome::AlreadyRunning => panic!("regular file was misdetected as live"),
        };

        drop(listener);
        cleanup_socket_path(&path);
    }

    #[tokio::test]
    async fn bound_socket_permissions_are_owner_only() {
        let path = temp_socket_path("permissions");
        let listener = match ensure_singleton(&path).await.unwrap() {
            Outcome::Bound(listener) => listener,
            Outcome::AlreadyRunning => panic!("fresh temp socket unexpectedly had a daemon"),
        };

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, SOCKET_MODE);

        drop(listener);
        cleanup_socket_path(&path);
    }
}
