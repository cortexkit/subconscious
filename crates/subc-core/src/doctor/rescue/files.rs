use std::{
    ffi::OsStr,
    fs::{self, File, Metadata},
    io,
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

#[cfg(unix)]
#[path = "files_unix.rs"]
mod platform;
#[cfg(windows)]
#[path = "files_windows.rs"]
mod platform;

pub(super) use platform::Directory;

pub(super) const AUTH: &str = "auth.json";

impl Directory {
    pub fn root(data_home: &Path, name: &str, create: bool) -> Result<Option<Self>, String> {
        if !data_home.is_absolute() {
            return Err(format!(
                "rescue data home must be absolute: {}",
                data_home.display()
            ));
        }
        if !single_name(name) {
            return Err(format!("invalid rescue install directory name: {name:?}"));
        }
        let home = at(data_home, Self::open_data_home(data_home))?;
        let path = data_home.join(name);
        // Check the install root's owner without following links or changing
        // permissions. `ck setup` controls install-root privacy; rescue only
        // enforces owner-only access on the directories it manages below it.
        let root = home.open_install_root(name, create);
        if create {
            at(&path, root).map(Some)
        } else {
            at(&path, absent(root))
        }
    }
}

pub(super) fn at<T>(path: &Path, result: io::Result<T>) -> Result<T, String> {
    result.map_err(|error| format!("{}: {error}", path.display()))
}

pub(super) fn absent<T>(result: io::Result<T>) -> io::Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

pub(super) fn refused(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message)
}

pub(super) fn single_name(name: &str) -> bool {
    let mut parts = Path::new(name).components();
    matches!(parts.next(), Some(Component::Normal(part)) if part == OsStr::new(name))
        && parts.next().is_none()
        && !name.contains([':', '/', '\\'])
}

pub(super) fn is_link(meta: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const REPARSE_POINT: u32 = 0x400;
        meta.file_attributes() & REPARSE_POINT != 0
    }
    #[cfg(unix)]
    meta.file_type().is_symlink()
}

pub(super) fn remove_entry(path: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if meta.is_dir() && !is_link(&meta) {
        // std's recursive removal does not traverse symlinks in the tree.
        fs::remove_dir_all(path)
    } else {
        #[cfg(windows)]
        {
            use std::os::windows::fs::FileTypeExt;
            if meta.is_dir() || meta.file_type().is_symlink_dir() {
                return fs::remove_dir(path);
            }
        }
        fs::remove_file(path)
    }
}

pub(super) fn existing_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir() && !is_link(&meta))
}

pub(super) struct Stage(PathBuf);

impl Stage {
    pub fn new(parent: &Directory) -> Result<Self, String> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        loop {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|error| format!("could not name install staging folder: {error}"))?
                .as_nanos();
            let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
            let name = format!(".staging-{}-{counter}-{nanos}", std::process::id());
            let path = parent.path().join(&name);
            match parent.exclusive(&name) {
                Ok(_) => return Ok(Self(path)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return at(&path, Err(error)),
            }
        }
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Stage {
    fn drop(&mut self) {
        if let Err(error) = absent(remove_entry(&self.0)) {
            eprintln!("warning: could not remove {}: {error}", self.0.display());
        }
    }
}

pub(super) fn create_session(parent: &Directory, stamp: &str) -> Result<Directory, String> {
    for attempt in 1.. {
        let name = if attempt == 1 {
            stamp.to_owned()
        } else {
            format!("{stamp}-{attempt}")
        };
        match parent.exclusive(&name) {
            Ok(folder) => return Ok(folder),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return at(&parent.path().join(name), Err(error)),
        }
    }
    unreachable!("session names are tried until one is created or creation fails")
}

pub(super) fn lock_file(directory: &Directory, name: &str) -> Result<File, String> {
    at(&directory.path().join(name), directory.open_lock(name))
}
