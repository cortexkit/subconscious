use std::{fs, io, path::Path};

use crate::setup::install_journal::{is_reparse_point, sync_parent};

use super::paths::{error_at, info, plain_dir};

pub fn parent(path: &Path) -> Result<(), String> {
    #[cfg(test)]
    EVENTS.with(|events| events.borrow_mut().push(("parent", path.into())));
    sync_parent(path)
}

pub fn file(path: &Path, file: &fs::File) -> Result<(), String> {
    #[cfg(test)]
    if FAIL_SYNC.with(|path_to_fail| path_to_fail.borrow().as_deref() == Some(path)) {
        FAIL_SYNC.with(|path_to_fail| path_to_fail.take());
        return Err(error_at(path, "injected file sync failure"));
    }
    file.sync_all().map_err(|error| error_at(path, error))?;
    #[cfg(test)]
    EVENTS.with(|events| events.borrow_mut().push(("file", path.into())));
    Ok(())
}

pub fn create_dir_exclusive(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new().mode(0o700).create(path)
    }
    #[cfg(not(unix))]
    {
        fs::create_dir(path)
    }
}

/// Create missing descendants one at a time so each name is synced in its parent.
/// The caller must supply a root that already exists and a path beneath it.
pub fn ensure_dir(root: &Path, path: &Path) -> Result<(), String> {
    let relative = path
        .strip_prefix(root)
        .map_err(|error| error_at(path, error))?;
    let mut current = root.to_path_buf();
    for part in relative.components() {
        let std::path::Component::Normal(name) = part else {
            return Err(error_at(path, "invalid directory component"));
        };
        current.push(name);
        match create_dir_exclusive(&current) {
            Ok(()) => parent(&current)?,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                if !plain_dir(&current)? {
                    return Err(error_at(&current, "directory disappeared"));
                }
                // A prior attempt may have created the name but failed its sync.
                parent(&current)?;
            }
            Err(error) => return Err(error_at(&current, error)),
        }
    }
    Ok(())
}

/// Rename a new entry and sync both parents if the move crosses directories.
pub fn rename_new(from: &Path, to: &Path) -> Result<(), String> {
    match try_rename_new(from, to).map_err(|error| error.message)? {
        true => Ok(()),
        false => Err(error_at(to, "rename crosses filesystems")),
    }
}

pub(crate) struct RenameError {
    pub message: String,
    pub renamed: bool,
}

impl RenameError {
    pub(crate) fn before(message: String) -> Self {
        Self {
            message,
            renamed: false,
        }
    }

    pub(crate) fn after(message: String) -> Self {
        Self {
            message,
            renamed: true,
        }
    }
}

/// `Ok(false)` means the filesystem refused a cross-device rename and neither
/// name changed. A failure after rename is never a reason to fall back to copy.
pub(crate) fn try_rename_new(from: &Path, to: &Path) -> Result<bool, RenameError> {
    if info(to).map_err(RenameError::before)?.is_some() {
        return Err(RenameError::before(error_at(
            to,
            "destination already exists",
        )));
    }
    #[cfg(test)]
    if CROSS_DEVICE_FROM.with(|source| source.borrow().as_deref() == Some(from)) {
        return Ok(false);
    }
    #[cfg(windows)]
    let renamed = subc_os::durable_move_to_new_name(from, to);
    #[cfg(not(windows))]
    let renamed = fs::rename(from, to);
    if let Err(error) = renamed {
        const WINDOWS_NOT_SAME_DEVICE: i32 = 17;
        if error.kind() == io::ErrorKind::CrossesDevices
            || cfg!(windows) && error.raw_os_error() == Some(WINDOWS_NOT_SAME_DEVICE)
        {
            return Ok(false);
        }
        return Err(RenameError::before(error_at(to, error)));
    }
    parent(to).map_err(RenameError::after)?;
    if from.parent() != to.parent() {
        parent(from).map_err(RenameError::after)?;
    }
    Ok(true)
}

/// Remove an entry without traversing links. Sync each change, including partial cleanup.
pub fn remove(path: &Path) -> Result<(), String> {
    remove_inner(path, false)
}

/// Remove a disposable saved copy even if its recorded mode is read-only.
pub(crate) fn remove_artifact(path: &Path) -> Result<(), String> {
    remove_inner(path, true)
}

fn remove_inner(path: &Path, artifact: bool) -> Result<(), String> {
    let Some(info) = info(path)? else {
        return Ok(());
    };
    if info.is_dir() && !is_reparse_point(&info) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // A saved bundle can contain read-only directories. Its disposable
            // copy must be writable to remove the contents before the parent.
            let mode = info.permissions().mode();
            if artifact && mode & 0o700 != 0o700 {
                fs::set_permissions(path, fs::Permissions::from_mode(mode | 0o700))
                    .map_err(|error| error_at(path, error))?;
            }
        }
        for entry in fs::read_dir(path).map_err(|error| error_at(path, error))? {
            let entry = entry.map_err(|error| error_at(path, error))?;
            remove_inner(&entry.path(), artifact)?;
        }
        fs::remove_dir(path).map_err(|error| error_at(path, error))?;
    } else {
        #[cfg(windows)]
        #[allow(clippy::permissions_set_readonly_false)]
        // Windows clears only the read-only attribute.
        let directory_link = {
            use std::os::windows::fs::{FileTypeExt, MetadataExt};
            if artifact && !is_reparse_point(&info) && info.permissions().readonly() {
                let mut permission = info.permissions();
                permission.set_readonly(false);
                fs::set_permissions(path, permission).map_err(|error| error_at(path, error))?;
            }
            info.file_type().is_symlink_dir() || info.file_attributes() & 0x10 != 0
        };
        #[cfg(not(windows))]
        let directory_link = false;
        (if directory_link {
            fs::remove_dir(path)
        } else {
            fs::remove_file(path)
        })
        .map_err(|error| error_at(path, error))?;
    }
    parent(path)
}

#[cfg(test)]
thread_local! {
    pub(crate) static EVENTS: std::cell::RefCell<Vec<(&'static str, std::path::PathBuf)>> = const { std::cell::RefCell::new(Vec::new()) };
    pub(crate) static FAIL_SYNC: std::cell::RefCell<Option<std::path::PathBuf>> = const { std::cell::RefCell::new(None) };
    pub(crate) static CROSS_DEVICE_FROM: std::cell::RefCell<Option<std::path::PathBuf>> = const { std::cell::RefCell::new(None) };
}
