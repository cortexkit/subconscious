use std::{
    fs::{self, File},
    io::{self, Read, Write},
    path::Path,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PausePoint {
    BeforeRename,
    AfterRename,
    #[cfg(feature = "test-support")]
    BeforeAnchor,
    #[cfg(feature = "test-support")]
    SpawnBeforeExec,
    #[cfg(feature = "test-support")]
    SwapBeforePromotion,
}

pub(super) fn read(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
        Ok(meta) if !meta.is_file() => {
            return Err(io::Error::other("store is not an ordinary file"))
        }
        _ => {}
    }
    let mut file = File::open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let meta = file.metadata()?;
        if meta.mode() & 0o077 != 0 || meta.uid() != rustix::process::geteuid().as_raw() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "store must be owner-only",
            ));
        }
    }
    #[cfg(windows)]
    subc_os::windows_acl::verify_owner_only(&file)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(Some(bytes))
}

pub(super) fn private_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
        match fs::symlink_metadata(path) {
            Ok(meta) => {
                if !meta.is_dir() || meta.uid() != rustix::process::geteuid().as_raw() {
                    return Err(io::Error::other(
                        "store directory is not an owned ordinary directory",
                    ));
                }
                fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(path)?;
                // The directory entry itself must survive a first-stamp crash.
                if let Some(parent) = path.parent() {
                    File::open(parent)?.sync_all()?;
                }
            }
            Err(e) => return Err(e),
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        subc_os::windows_acl::create_private_dir(path)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Err(io::Error::other("private store directories unsupported"))
    }
}

pub(super) fn publish(
    path: &Path,
    bytes: &[u8],
    mut pause: impl FnMut(PausePoint) -> io::Result<()>,
) -> io::Result<()> {
    let directory = path
        .parent()
        .ok_or_else(|| io::Error::other("store has no parent"))?;
    private_directory(directory)?;
    let mut random = [0; 16];
    getrandom::getrandom(&mut random).map_err(|e| io::Error::other(e.to_string()))?;
    let temp = directory.join(format!(
        ".approved-{:032x}.tmp",
        u128::from_be_bytes(random)
    ));
    let result = (|| {
        #[cfg(windows)]
        let mut file = subc_os::windows_acl::create_private_file(&temp)?;
        #[cfg(not(windows))]
        let mut file = {
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            options.open(&temp)?
        };
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        pause(PausePoint::BeforeRename)?;
        subc_os::durable_replace(&temp, path)?;
        pause(PausePoint::AfterRename)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}
