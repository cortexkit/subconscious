use crate::{KillOutcome, Placement};
use std::{
    fs,
    io::{self, Write as _},
    path::{Component, Path, PathBuf},
};

fn invalid_path() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "cgroup path must be a canonical descendant of this placement's modules root, without symlinks",
    )
}

impl Placement {
    /// Resolve an existing cgroup beneath this daemon's own modules root.
    ///
    /// This lookup never creates directories or interfaces. Missing paths are
    /// gone; outside-root paths, dot components and symlinks are refused before
    /// any control file is opened. The placement, not a persisted record, must
    /// supply the trusted root.
    pub fn existing_module_path(&self, path: &Path) -> io::Result<Option<PathBuf>> {
        if !path.is_absolute()
            || path
                .components()
                .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
        {
            return Err(invalid_path());
        }
        let root = match fs::canonicalize(&self.modules) {
            Ok(root) => root,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let suffix = path.strip_prefix(&root).map_err(|_| invalid_path())?;
        if suffix.as_os_str().is_empty() {
            return Err(invalid_path());
        }
        // Inspect every component rather than canonicalizing only the final
        // path: following a symlink, even one pointing back inside, is refused.
        let mut current = root.clone();
        match fs::symlink_metadata(&self.modules) {
            Ok(metadata) if metadata.file_type().is_symlink() => return Err(invalid_path()),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        }
        for part in suffix.components() {
            let Component::Normal(part) = part else {
                return Err(invalid_path());
            };
            current.push(part);
            let metadata = match fs::symlink_metadata(&current) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error),
            };
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                return Err(invalid_path());
            }
        }
        if path.as_os_str() != current.as_os_str() {
            return Err(invalid_path());
        }
        match fs::canonicalize(&current) {
            Ok(canonical) if canonical != current => return Err(invalid_path()),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        }
        Ok(Some(current))
    }

    /// Remove an existing empty cgroup after verifying its recorded path.
    /// A missing cgroup is already removed; no directory is ever recreated.
    pub fn remove_path(&self, path: &Path) -> io::Result<()> {
        let Some(path) = self.existing_module_path(path)? else {
            return Ok(());
        };
        match fs::remove_dir(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            result => result,
        }
    }
}

pub(crate) fn open_control(module: &Path, name: &str) -> io::Result<fs::File> {
    let path = module.join(name);
    if fs::symlink_metadata(&path)?.file_type().is_symlink() {
        return Err(invalid_path());
    }
    fs::OpenOptions::new().write(true).open(path)
}

/// Kill an existing cgroup tree by its recorded path, without creating it.
/// The path is accepted only beneath the caller's trusted placement root.
pub fn kill_path(placement: &Placement, path: &Path) -> KillOutcome {
    let module = match placement.existing_module_path(path) {
        Ok(Some(module)) => module,
        Ok(None) => return KillOutcome::NotPlaced,
        Err(error) => {
            return KillOutcome::IoError {
                path: path.to_owned(),
                error,
            }
        }
    };
    let path = module.join("cgroup.kill");
    let mut file = match open_control(&module, "cgroup.kill") {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return KillOutcome::Unsupported,
        Err(error) => return KillOutcome::IoError { path, error },
    };
    match file.write_all(b"1") {
        Ok(()) => KillOutcome::Killed,
        Err(error) => KillOutcome::IoError { path, error },
    }
}
