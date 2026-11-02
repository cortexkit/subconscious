//! The Windows profile copy of the connection file.
//!
//! When the connection file falls back to the system temporary directory, the
//! primary file is `subc-<token>.connection.json` there, named from the
//! environment. The profile copy is the same information at
//! `<profile>\AppData\Local\cortexkit\run\subc-connection.json`, where
//! `<profile>` is the profile folder Windows records for the account, so a
//! client can find it without the environment. The primary file is the one the
//! connection-file `AlreadyRunning` check uses (run-directory ownership is
//! checked separately) and the only required write. Preparing the copy takes
//! the adjacent `.start-lock` file, so concurrent startups and publications
//! serialize. Failing to publish the copy doesn't stop the daemon starting.

use super::*;

pub(super) fn resolve(config: &BootstrapConfig) -> Option<PathBuf> {
    if config.connection_file_source != ConnectionFileSource::TempDirFallback {
        return None;
    }
    // Unit tests must opt into a fixture base, never the user's installed file.
    #[cfg(test)]
    let base = Ok::<_, io::Error>(config.profile_override.clone()?);
    #[cfg(not(test))]
    let base = subc_os::profile_dir();
    match base {
        Ok(base) => Some(
            base.join("AppData")
                .join("Local")
                .join("cortexkit")
                .join("run")
                .join(CONNECTION_FILE_NAME),
        ),
        Err(error) => {
            warn_unavailable(&config.connection_file_path, None, &error);
            None
        }
    }
}

pub(super) fn warn_unavailable(primary: &Path, copy: Option<&Path>, error: &dyn fmt::Display) {
    warn!(
        primary = %primary.display(),
        copy = ?copy,
        %error,
        "Windows fixed connection-file copy unavailable; using the primary only"
    );
}

async fn claim(path: &Path) -> Result<StartLock, BootstrapError> {
    // Create any missing parent directories with protected owner-only access
    // control lists, as the atomic writer does. The start-lock opener doesn't
    // install a protected DACL on the lock file itself; its permissions may be
    // inherited from the parent. The credential JSON must go through the atomic
    // writer, which gives it explicit private security, never a plain copy.
    let parent = path.parent().expect("fixed connection path has a parent");
    if !parent.exists() {
        subc_os::windows_acl::create_private_dir(parent).map_err(|source| {
            BootstrapError::StartLockCreate {
                path: parent.to_path_buf(),
                source,
            }
        })?;
    }
    StartLock::acquire(path).await
}

pub(super) async fn reclaim(primary: &Path, copy: Option<PathBuf>) -> Option<PathBuf> {
    let path = copy?;
    let result = async {
        let _lock = claim(&path).await?;
        // Probing the primary path doesn't test a daemon that uses another
        // primary path but publishes to this same profile file. So, under this
        // file's own start lock, apply the usual owner-checked read and the two
        // authenticated loopback checks 1.5 s apart. Keep a file that passes
        // both. Remove one judged absent or stale: malformed or insecure, or
        // failing to connect or authenticate. On any other read error, leave
        // the file untouched, warn, and skip the profile copy.
        if stable_peer(&path).await? {
            return Err(io::Error::other("fixed copy belongs to a live daemon").into());
        }
        remove_stale_connection_file_if_present(&path)?;
        Ok::<_, CopyError>(())
    }
    .await;
    match result {
        Ok(()) => Some(path),
        Err(error) => {
            warn_unavailable(primary, Some(&path), &error);
            None
        }
    }
}

pub(super) async fn claim_for_publication(
    primary: &Path,
    copy: Option<&Path>,
    info: &ConnectionInfo,
) -> Option<(PathBuf, StartLock)> {
    let path = copy?;
    let result = async {
        let lock = claim(path).await?;
        // Recheck under the start lock: another daemon may have published here
        // since startup cleanup. A file with our own `daemon_id` is republished
        // without a probe. For another `daemon_id`, skip this publication if
        // the file passes both liveness probes; a file judged stale is replaced.
        let ours = match connection_file::read(path) {
            Ok(existing) => existing.daemon_id == info.daemon_id,
            Err(error) if is_absent_or_stale_connection_file(&error) => false,
            Err(error) => return Err(CopyError::Read(error)),
        };
        if !ours && stable_peer(path).await? {
            return Err(io::Error::other("fixed copy belongs to a live foreign daemon").into());
        }
        Ok(lock)
    }
    .await;
    match result {
        Ok(lock) => Some((path.to_path_buf(), lock)),
        Err(error) => {
            warn_unavailable(primary, Some(path), &error);
            None
        }
    }
}

#[derive(Debug)]
enum CopyError {
    Bootstrap(BootstrapError),
    Read(ConnectionFileError),
    Io(io::Error),
}

impl From<BootstrapError> for CopyError {
    fn from(error: BootstrapError) -> Self {
        Self::Bootstrap(error)
    }
}

impl From<io::Error> for CopyError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl fmt::Display for CopyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bootstrap(error) => error.fmt(formatter),
            Self::Read(error) => error.fmt(formatter),
            Self::Io(error) => error.fmt(formatter),
        }
    }
}

#[cfg(test)]
mod tests;
