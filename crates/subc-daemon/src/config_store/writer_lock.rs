//! A service store has one live writer, independent of daemon discovery paths.

use fs4::{FileExt, TryLockError};
use std::{
    fs::{self, File},
    io,
    path::{Component, Path, PathBuf},
};

#[derive(Debug)]
pub enum WriterLockError {
    Busy { root: PathBuf },
    Invalid { root: PathBuf, source: io::Error },
}

impl std::fmt::Display for WriterLockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy { root } => write!(
                f,
                "config_store_busy: configuration store {} already has a live owner",
                root.display()
            ),
            Self::Invalid { root, source } => write!(
                f,
                "config_store_lock_invalid: configuration store {}: {source}",
                root.display()
            ),
        }
    }
}
impl std::error::Error for WriterLockError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Invalid { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// An OS lock held until the store owner exits. The sibling file is never
/// deleted: removing a locked inode would let another daemon lock its replacement.
#[derive(Debug)]
pub struct StoreWriterLock {
    file: File,
}

impl StoreWriterLock {
    /// Canonicalize the owned parent and lock a sibling without creating the
    /// store root. This preserves the store's read-only startup contract.
    pub fn acquire(root: &Path) -> Result<(super::StoreRoot, Self), WriterLockError> {
        let invalid = |source| WriterLockError::Invalid {
            root: root.into(),
            source,
        };
        let leaf = match root.components().next_back() {
            Some(Component::Normal(leaf)) if root.is_absolute() => leaf,
            _ => {
                return Err(invalid(io::Error::other(
                    "store root must have an absolute ordinary name",
                )))
            }
        };
        let parent = root
            .parent()
            .ok_or_else(|| invalid(io::Error::other("store root has no parent")))?;
        ensure_parent(parent).map_err(invalid)?;
        let parent = fs::canonicalize(parent).map_err(invalid)?;
        verify_directory(&parent).map_err(invalid)?;
        let canonical_root = parent.join(leaf);
        match fs::symlink_metadata(&canonical_root) {
            Ok(_) => verify_directory(&canonical_root).map_err(invalid)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(invalid(error)),
        }
        let mut name = leaf.to_os_string();
        name.push(".lock");
        let path = parent.join(name);
        let file = match create_lock(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                subc_os::open_regular_nofollow(&path)
                    .map_err(|error| invalid(io::Error::other(error)))?
            }
            Err(error) => return Err(invalid(error)),
        };
        verify_file(&file).map_err(invalid)?;
        match FileExt::try_lock(&file) {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(WriterLockError::Busy {
                    root: canonical_root,
                })
            }
            Err(TryLockError::Error(error)) => return Err(invalid(error)),
        }
        let root = super::StoreRoot::from_service_definition(canonical_root).map_err(invalid)?;
        Ok((root, Self { file }))
    }
}

impl Drop for StoreWriterLock {
    fn drop(&mut self) {
        // Explicit unlock also releases a description briefly inherited by fork
        // before exec; an unrelated child must not prolong daemon ownership.
        let _ = FileExt::unlock(&self.file);
    }
}

fn ensure_parent(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => verify_directory(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent() {
                ensure_parent(parent)?;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                match fs::DirBuilder::new().mode(0o700).create(path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error),
                }
            }
            #[cfg(windows)]
            subc_os::windows_acl::create_private_dir(path)?;
            verify_directory(path)
        }
        Err(error) => Err(error),
    }
}

fn verify_directory(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(io::Error::other(
            "store parent/root must be an owned non-symlink directory",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != subc_os::effective_uid() || metadata.mode() & 0o022 != 0 {
            return Err(io::Error::other(
                "store parent/root must be owned and not writable by other users",
            ));
        }
    }
    #[cfg(windows)]
    subc_os::windows_acl::verify_private_dir(path)?;
    Ok(())
}

fn create_lock(path: &Path) -> io::Result<File> {
    #[cfg(windows)]
    {
        subc_os::windows_acl::create_private_file(path)
    }
    #[cfg(not(windows))]
    {
        let mut options = fs::OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options.open(path)
    }
}

fn verify_file(file: &File) -> io::Result<()> {
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::other("writer lock is not a regular file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != subc_os::effective_uid() || metadata.mode() & 0o077 != 0 {
            return Err(io::Error::other("writer lock must be owner-only"));
        }
    }
    #[cfg(windows)]
    subc_os::windows_acl::verify_owner_only(file)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        process::{Child, Command, Stdio},
        time::{Duration, Instant},
    };

    struct Rig(PathBuf);
    impl Rig {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("subc-store-lock-{}-{}", std::process::id(), {
                    static NEXT: std::sync::atomic::AtomicU64 =
                        std::sync::atomic::AtomicU64::new(0);
                    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                }));
            fs::create_dir(&path).unwrap();
            #[cfg(windows)]
            subc_os::windows_acl::create_private_dir(&path).unwrap();
            Self(path)
        }
        fn root(&self) -> PathBuf {
            self.0.join("authority")
        }
    }
    impl Drop for Rig {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    struct KillOnDrop(Child);
    impl Drop for KillOnDrop {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn config_store_writer_lock_refuses_second_owner_without_creating_root() {
        let rig = Rig::new();
        let root = rig.root();
        let (canonical, first) = StoreWriterLock::acquire(&root).unwrap();
        assert_eq!(
            canonical.path(),
            fs::canonicalize(&rig.0).unwrap().join("authority")
        );
        assert!(!root.exists());
        assert!(
            matches!(StoreWriterLock::acquire(&root),Err(WriterLockError::Busy { root: refused }) if refused == canonical.path())
        );
        let text = StoreWriterLock::acquire(&root).err().unwrap().to_string();
        assert!(text.contains("config_store_busy") && text.contains("authority"));
        drop(first);
        let (_, _next) = StoreWriterLock::acquire(&root).unwrap();
        assert!(!root.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(
                fs::metadata(rig.0.join("authority.lock")).unwrap().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn config_store_writer_lock_releases_when_owning_process_exits() {
        let rig = Rig::new();
        let root = rig.root();
        let mut child = KillOnDrop(
            Command::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg("config_store::writer_lock::tests::config_store_writer_lock_child_probe")
                .arg("--ignored")
                .env("SUBC_WRITER_LOCK_CHILD_ROOT", &root)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while !rig.0.join("held").exists() {
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "writer-lock probe exited before acquiring"
            );
            assert!(
                Instant::now() < deadline,
                "writer-lock probe did not acquire before deadline"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(matches!(
            StoreWriterLock::acquire(&root),
            Err(WriterLockError::Busy { .. })
        ));
        child.0.kill().unwrap();
        child.0.wait().unwrap();
        let (_, _next) = StoreWriterLock::acquire(&root).unwrap();
        assert!(!root.exists());
    }

    #[test]
    #[ignore = "child probe invoked by its bounded parent test"]
    fn config_store_writer_lock_child_probe() {
        let root =
            PathBuf::from(std::env::var_os("SUBC_WRITER_LOCK_CHILD_ROOT").expect("isolated root"));
        let (_, _lock) = StoreWriterLock::acquire(&root).unwrap();
        fs::write(root.parent().unwrap().join("held"), b"held").unwrap();
        loop {
            std::thread::park();
        }
    }

    #[test]
    fn config_store_writer_lock_creates_only_private_missing_parents() {
        let rig = Rig::new();
        let root = rig.0.join("missing/parent/authority");
        let (_, _lock) = StoreWriterLock::acquire(&root).unwrap();
        assert!(!root.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            for parent in [rig.0.join("missing"), rig.0.join("missing/parent")] {
                assert_eq!(fs::metadata(parent).unwrap().mode() & 0o777, 0o700);
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn config_store_writer_lock_refuses_symlink_parent_or_lock_and_broad_lock() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let rig = Rig::new();
        symlink(&rig.0, rig.0.join("alias")).unwrap();
        assert!(matches!(
            StoreWriterLock::acquire(&rig.0.join("alias/authority")),
            Err(WriterLockError::Invalid { .. })
        ));

        let ordinary = rig.0.join("ordinary");
        let lock = rig.0.join("authority.lock");
        fs::write(&ordinary, b"not a lock").unwrap();
        fs::set_permissions(&ordinary, fs::Permissions::from_mode(0o600)).unwrap();
        symlink(&ordinary, &lock).unwrap();
        assert!(matches!(
            StoreWriterLock::acquire(&rig.root()),
            Err(WriterLockError::Invalid { .. })
        ));
        fs::remove_file(&lock).unwrap();

        fs::write(&lock, b"broad").unwrap();
        fs::set_permissions(&lock, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            StoreWriterLock::acquire(&rig.root()),
            Err(WriterLockError::Invalid { .. })
        ));
        fs::set_permissions(&lock, fs::Permissions::from_mode(0o600)).unwrap();
        drop(StoreWriterLock::acquire(&rig.root()).unwrap());

        fs::set_permissions(&rig.0, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(matches!(
            StoreWriterLock::acquire(&rig.root()),
            Err(WriterLockError::Invalid { .. })
        ));
        fs::set_permissions(&rig.0, fs::Permissions::from_mode(0o700)).unwrap();
        drop(StoreWriterLock::acquire(&rig.root()).unwrap());
    }
}
