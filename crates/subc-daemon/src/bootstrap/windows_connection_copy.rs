//! Optional Windows publication at `<Local AppData>\cortexkit\run\subc-connection.json`
//! for daemons whose connection file falls back to the system temporary
//! directory. The original connection file still decides this daemon's
//! singleton check and is the only required publication; readers may prefer
//! the Local AppData file.

use super::*;

pub(super) fn resolve(config: &BootstrapConfig) -> Option<PathBuf> {
    if config.connection_file_source != ConnectionFileSource::TempDirFallback {
        return None;
    }
    // Unit tests must opt into a fixture base, never the user's installed file.
    #[cfg(test)]
    let base = Ok::<_, io::Error>(config.local_app_data_override.clone()?);
    #[cfg(not(test))]
    let base = subc_os::local_app_data_dir();
    match base {
        Ok(base) => Some(
            base.join("cortexkit")
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
    // control lists, as the atomic writer does. The start-lock file opened here
    // isn't ACL-protected; the published JSON must still go through the atomic
    // writer, never a plain filesystem copy.
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
        // Probing the original path doesn't test a daemon that uses another
        // original path but publishes to this same Local AppData file. So, under
        // this file's own start lock, apply the usual owner-checked read and
        // authenticated liveness probes: keep the file if both probes succeed,
        // otherwise remove it as stale. Stale includes malformed or insecure
        // files and failed connection or authentication probes.
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
