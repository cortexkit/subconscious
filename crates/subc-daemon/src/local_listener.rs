//! The daemon's second, same-user listener: a Unix-domain socket on macOS and
//! Linux, a named pipe on Windows.
//!
//! Beside the loopback TCP listener, the daemon serves this endpoint so a local
//! client can reach it without a TCP port, and so the daemon can read, at
//! accept, which process connected (a later campaign uses that to recognise the
//! desktop app). Both listeners share one handshake and one frame format, so a
//! client's bytes are identical whichever way it arrives.
//!
//! This listener serves clients only. A local endpoint exists to be reached by
//! processes on this machine, so a HELLO that registers a module is refused
//! with [`LOCAL_ENDPOINT_CLIENTS_ONLY`], whether or not it carries a launch
//! nonce, before any registration or route exists.
//!
//! Ownership, per platform:
//!
//! - Unix: the socket file is created `0600` in a directory the daemon owns and
//!   sets to `0700`. The file is removed when the daemon stops serving, and a
//!   leftover socket from a daemon that died is replaced by the next start.
//! - Windows: the pipe name carries a fresh random suffix for every daemon
//!   start, its DACL admits only the current user's SID, and it is created with
//!   `PIPE_REJECT_REMOTE_CLIENTS`, so a network client is refused before the
//!   handshake. A client that nonetheless reports a computer name is refused
//!   again before a byte is read.

use std::path::{Path, PathBuf};

use subc_transport::LocalEndpoint;

#[cfg(test)]
mod tests;

use crate::server::ServerError;

/// The reason a local-endpoint connection is refused before routing: this
/// endpoint serves clients only, and a module must register over TCP.
pub const LOCAL_ENDPOINT_CLIENTS_ONLY: &str = "local_endpoint_clients_only";

/// What a bound, idle local listener brings and what it needs to serve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalListenerConfig {
    /// Where to put the socket (Unix only).
    ///
    /// `None` means this daemon serves no local endpoint. That is the default a
    /// library caller gets, and it is deliberate: an in-process daemon booted
    /// by a test or by a sibling repository must never touch the operator's real
    /// run directory. The shipped daemon's own config names its run directory
    /// with [`LocalListenerConfig::in_socket_dir`].
    ///
    /// The named directory is created with mode `0700` when it is missing, and
    /// an existing one is refused when another user owns it or when group or
    /// other users can reach it. It is never changed.
    pub socket_dir: Option<PathBuf>,
}

impl LocalListenerConfig {
    /// Put the socket in `dir`, creating it with mode `0700` if missing.
    pub fn in_socket_dir(dir: impl Into<PathBuf>) -> Self {
        Self {
            socket_dir: Some(dir.into()),
        }
    }

    /// The socket directory when one was configured.
    pub fn configured_dir(&self) -> Option<&Path> {
        self.socket_dir.as_deref()
    }
}

/// A local listener this daemon bound and has not started serving yet.
#[derive(Debug)]
pub enum BoundLocalListener {
    #[cfg(unix)]
    Unix {
        listener: std::os::unix::net::UnixListener,
        path: PathBuf,
    },
    /// `first_instance` is the end the server waits on before any client has
    /// connected: a client is refused when no end exists yet, so the bind leaves
    /// one in place rather than making the first caller race the accept loop.
    /// It is tokio's overlapped server end, so reads and writes on it run at
    /// once; a synchronous handle would make every write wait behind the
    /// reader's pending read.
    #[cfg(windows)]
    Windows {
        pipe_name: String,
        first_instance: Option<tokio::net::windows::named_pipe::NamedPipeServer>,
    },
}

impl BoundLocalListener {
    /// What a client is told to connect to: the socket path or the pipe name.
    pub fn endpoint(&self) -> LocalEndpoint {
        match self {
            #[cfg(unix)]
            Self::Unix { path, .. } => LocalEndpoint::Unix { path: path.clone() },
            #[cfg(windows)]
            Self::Windows { pipe_name, .. } => LocalEndpoint::Pipe {
                name: pipe_name.clone(),
            },
            // The arms above cover every variant of this platform.
            #[allow(unreachable_patterns)]
            _ => unreachable!("a bound local listener has a variant for this platform"),
        }
    }
}

/// Bind this daemon's local listener. A failure is an error rather than a
/// silent fallback to TCP only: a client that opted into the local transport
/// would otherwise find no endpoint in the connection file and fail later, far
/// from the cause.
pub fn bind(
    config: &LocalListenerConfig,
    connection_file_path: &std::path::Path,
) -> Result<BoundLocalListener, ServerError> {
    #[cfg(unix)]
    {
        let dir = config
            .socket_dir
            .as_deref()
            .ok_or_else(|| ServerError::LocalAccept {
                endpoint: "<none>".to_owned(),
                source: std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "this daemon names no local-endpoint directory",
                ),
            })?;
        unix::create_private_dir(dir)?;
        let _ = connection_file_path;
        let path = dir.join(unix::SOCKET_FILE_NAME);
        unix::refuse_overlong_path(&path)?;
        // A daemon that is starting replaces any socket left by the daemon it
        // supersedes, which a crashed start cannot have removed itself. The
        // directory was checked to belong to this user, so the file is this
        // daemon's to remove.
        unix::remove_socket(&path);
        let listener = std::os::unix::net::UnixListener::bind(&path).map_err(|source| {
            ServerError::LocalAccept {
                endpoint: path.display().to_string(),
                source,
            }
        })?;
        // The socket's own mode is subject to the umask, and the daemon must not
        // depend on the operator's umask for who can connect.
        unix::set_owner_only(&path)?;
        Ok(BoundLocalListener::Unix { listener, path })
    }
    #[cfg(windows)]
    {
        let _ = (config, connection_file_path);
        let pipe_name = windows::fresh_pipe_name();
        // Bind the first end now, so a name another process holds, or a DACL
        // Win32 refuses, fails the bind rather than a listener that never
        // answers. The end stays here until the server accepts its first
        // client: Win32 refuses a client that finds no end at all.
        let first_instance = windows::bind_end(&pipe_name, true)?;
        Ok(BoundLocalListener::Windows {
            pipe_name,
            first_instance: Some(first_instance),
        })
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (config, connection_file_path);
        Err(ServerError::LocalAccept {
            endpoint: "<none>".to_owned(),
            source: std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "this platform has no local listener",
            ),
        })
    }
}

#[cfg(unix)]
mod unix {
    use std::{
        fs, io,
        os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt},
        path::Path,
    };

    use crate::server::ServerError;

    /// The longest Unix-domain socket path the kernel accepts, minus the
    /// terminating NUL.
    pub(super) const MAX_SOCKET_PATH: usize = 104;

    /// The socket's file name in the run directory.
    ///
    /// A fixed name is enough because the run directory belongs to one data
    /// home: two daemons sharing a data home already collide on the activity
    /// ledger and the live-children record, so that setup is not supported. A
    /// restarted daemon therefore finds and replaces its own socket, and the
    /// name leaves as much of the Unix socket path limit as possible for the
    /// home directory in front of it.
    pub(super) const SOCKET_FILE_NAME: &str = "subc.sock";

    /// Create the socket directory when it is missing, with mode `0700`.
    ///
    /// An existing directory is never changed: a daemon does not own the run
    /// directory's mode, and changing it could surprise whoever set it. Instead
    /// it is refused when another user owns it (that user could replace the
    /// socket) or when group or other users can reach it (they could connect to
    /// the socket, whatever the socket file's own mode). A refused directory
    /// leaves the daemon serving TCP only.
    pub(super) fn create_private_dir(dir: &Path) -> Result<(), ServerError> {
        prepare_private_dir(dir).map_err(|source| ServerError::LocalAccept {
            endpoint: dir.display().to_string(),
            source,
        })
    }

    fn prepare_private_dir(dir: &Path) -> io::Result<()> {
        match fs::symlink_metadata(dir) {
            Ok(metadata) => {
                if !metadata.is_dir() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("{} is not a directory", dir.display()),
                    ));
                }
                if metadata.uid() != current_uid() {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        format!(
                            "{} is owned by uid {}, not this user; refusing to place the local socket in it",
                            dir.display(),
                            metadata.uid()
                        ),
                    ));
                }
                let mode = metadata.permissions().mode() & 0o777;
                if mode & 0o077 != 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        format!(
                            "{} is accessible to group or other users (mode {mode:o}); the local socket must live in a directory only this user can reach",
                            dir.display()
                        ),
                    ));
                }
                Ok(())
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir),
            Err(error) => Err(error),
        }
    }

    /// Refuse a socket path the kernel cannot represent, instead of letting the
    /// bind fail with a message that names no cause.
    pub(super) fn refuse_overlong_path(path: &Path) -> Result<(), ServerError> {
        if path.as_os_str().len() < MAX_SOCKET_PATH {
            return Ok(());
        }
        remove_socket(path);
        Err(ServerError::LocalAccept {
            endpoint: path.display().to_string(),
            source: io::Error::new(
                io::ErrorKind::InvalidInput,
                "the local socket path is too long for a Unix-domain socket; put the socket directory somewhere shorter",
            ),
        })
    }

    /// Set the socket file itself to `0600`, so the umask cannot widen it.
    pub(super) fn set_owner_only(path: &Path) -> Result<(), ServerError> {
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|source| {
            ServerError::LocalAccept {
                endpoint: path.display().to_string(),
                source,
            }
        })
    }

    fn current_uid() -> u32 {
        rustix::process::geteuid().as_raw()
    }

    /// The socket file's mode. It matters less than the directory's, which
    /// decides who can reach the socket at all, but both are set and both are
    /// checked by the tests.
    pub(super) fn socket_mode(path: &Path) -> io::Result<u32> {
        Ok(fs::metadata(path)?.permissions().mode() & 0o777)
    }

    /// Remove a socket file, leaving anything that is not a socket alone. A
    /// daemon that is starting calls it to replace the socket of the daemon it
    /// supersedes, and one that is stopping calls it so a later start finds no
    /// leftover.
    pub(super) fn remove_socket(path: &Path) {
        if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_socket()) {
            let _ = fs::remove_file(path);
        }
    }
}

#[cfg(windows)]
mod windows {
    use std::sync::atomic::{AtomicU64, Ordering};

    use tokio::net::windows::named_pipe::NamedPipeServer;

    use crate::server::ServerError;

    /// Create one end of `pipe_name`: overlapped, byte mode, remote clients
    /// rejected, and with the owner-only DACL subc-os builds. `first` marks the
    /// daemon's first end, so a name another process holds fails here rather
    /// than being shared.
    pub(super) fn create_end(pipe_name: &str, first: bool) -> std::io::Result<NamedPipeServer> {
        subc_os::named_pipe::create_end(std::ffi::OsStr::new(pipe_name), first)
    }

    /// [`create_end`] as the daemon's bind reports it.
    pub(super) fn bind_end(pipe_name: &str, first: bool) -> Result<NamedPipeServer, ServerError> {
        create_end(pipe_name, first).map_err(|source| ServerError::LocalAccept {
            endpoint: pipe_name.to_owned(),
            source,
        })
    }

    /// A pipe name no other daemon start shares, so a second daemon (or a
    /// restarted one) never finds the previous daemon's pipe.
    ///
    /// The name is public, not a credential: the DACL is what restricts who may
    /// connect.
    pub(super) fn fresh_pipe_name() -> String {
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let mut random = [0u8; 16];
        let random_hex = match getrandom::getrandom(&mut random) {
            Ok(()) => random.iter().map(|byte| format!("{byte:02x}")).collect(),
            // The random source is the only thing keeping two starts apart; a
            // failure here falls back to the pid and a counter, which still
            // differs between starts.
            Err(_) => format!("{:032x}{sequence:016x}", std::process::id()),
        };
        format!(r"\\.\pipe\subc-{}-{random_hex}", std::process::id())
    }
}

/// Remove whatever a bound local listener left on disk. On Windows the pipe
/// disappears with its handles, so there is nothing to remove.
pub fn release(listener: &BoundLocalListener) {
    #[cfg(unix)]
    match listener {
        BoundLocalListener::Unix { path, .. } => unix::remove_socket(path),
    }
    #[cfg(not(unix))]
    let _ = listener;
}

/// The socket file mode a Unix local listener must have.
#[cfg(unix)]
pub fn unix_socket_mode(path: &std::path::Path) -> std::io::Result<u32> {
    unix::socket_mode(path)
}

/// Remove the socket file a bound listener left behind. Serving does this when
/// it stops; binding also replaces a leftover before it starts, so this covers
/// the paths that neither runs.
///
/// Only a socket file is removed, so this cannot delete anything else a caller
/// happens to name.
#[cfg(unix)]
pub fn remove_unix_socket(path: &std::path::Path) {
    unix::remove_socket(path);
}

/// No socket file exists outside Unix.
#[cfg(not(unix))]
pub fn remove_unix_socket(_path: &std::path::Path) {}

/// Bind the pipe `name` the way the daemon binds its own: a first end that
/// refuses a name another process holds, then a listener that serves clients on
/// further ends of that name.
#[cfg(windows)]
pub fn bind_named_pipe(name: &str) -> Result<BoundLocalListener, ServerError> {
    let first_instance = windows::bind_end(name, true)?;
    Ok(BoundLocalListener::Windows {
        pipe_name: name.to_owned(),
        first_instance: Some(first_instance),
    })
}

/// Create one further end of the pipe `name`, for the server's accept loop.
#[cfg(windows)]
pub fn create_named_pipe_end(
    name: &str,
) -> std::io::Result<tokio::net::windows::named_pipe::NamedPipeServer> {
    windows::create_end(name, false)
}

/// The socket path a daemon with this config would use, without binding it.
/// `None` when the config names no directory. The connection file is accepted
/// for callers that still pass it; the socket's name no longer comes from it.
#[cfg(unix)]
pub fn socket_path_for(
    configured: Option<&std::path::Path>,
    connection_file_path: &std::path::Path,
) -> Option<std::path::PathBuf> {
    let _ = connection_file_path;
    configured.map(|dir| dir.join(unix::SOCKET_FILE_NAME))
}
