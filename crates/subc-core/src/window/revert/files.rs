use std::{
    fs,
    path::{Path, PathBuf},
};

use crate::setup::install_journal::is_reparse_point;

use super::super::journal::{
    content, durable,
    paths::{self, error_at},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    File,
    Bundle,
}

impl Kind {
    pub(super) fn digest(self, path: &Path) -> Result<Option<String>, String> {
        match self {
            Self::File => content::file_digest(path),
            Self::Bundle => content::bundle_digest(path),
        }
    }

    pub(super) fn snapshot(self, path: &Path) -> Result<Reading, String> {
        check_parent(path)?;
        let Some(info) = paths::info(path)? else {
            check_absent_parent(path)?;
            return Ok(Reading::default());
        };
        check_type(path, &info, self == Self::Bundle)?;
        check_owner(path, &info, self == Self::Bundle)?;
        Ok(Reading {
            digest: Some(
                self.digest(path)?
                    .ok_or_else(|| error_at(path, "target disappeared"))?,
            ),
            mode: if self == Self::File {
                content::mode(&info)
            } else {
                None
            },
        })
    }

    pub(super) fn verify(self, path: &Path, expected: &str) -> Result<(), String> {
        match self.digest(path)? {
            Some(actual) if actual == expected => Ok(()),
            actual => Err(error_at(
                path,
                format!(
                    "expected sha256 {expected}, found {}",
                    actual.as_deref().unwrap_or("absent")
                ),
            )),
        }
    }

    fn copy(self, source: &Path, dest: &Path, sha: &str, mode: Option<u32>) -> Result<(), String> {
        let actual = match self {
            Self::File => content::copy_checked(source, dest)?,
            Self::Bundle => content::bundle_copy_checked(source, dest)?,
        };
        if actual != sha {
            return Err(error_at(
                dest,
                format!("expected sha256 {sha}, copied {actual}"),
            ));
        }
        #[cfg(unix)]
        if let Some(mode) = mode {
            use std::os::unix::fs::PermissionsExt;
            let file =
                subc_os::open_regular_nofollow(dest).map_err(|error| error_at(dest, error))?;
            file.set_permissions(fs::Permissions::from_mode(mode))
                .map_err(|error| error_at(dest, error))?;
            durable::file(dest, &file)?;
        }
        #[cfg(not(unix))]
        let _ = mode;
        Ok(())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Reading {
    pub digest: Option<String>,
    pub mode: Option<u32>,
}

pub(super) fn check_parent(path: &Path) -> Result<&Path, String> {
    let parent = path
        .parent()
        .ok_or_else(|| error_at(path, "no parent directory"))?;
    let info =
        paths::info(parent)?.ok_or_else(|| error_at(parent, "parent directory is absent"))?;
    check_type(parent, &info, true)?;
    // The journal records canonical paths. Reject an ancestor redirected since
    // recording, even if the immediate parent itself is an ordinary directory.
    if !paths::same_path(parent, &paths::canonical(parent)?) {
        return Err(error_at(parent, "parent directory has been redirected"));
    }
    Ok(parent)
}

pub(super) fn parent_identity(path: &Path) -> Result<subc_os::RetainedFileIdentity, String> {
    let parent = check_parent(path)?;
    let directory =
        subc_os::open_directory_nofollow_all(parent).map_err(|error| error_at(parent, error))?;
    subc_os::retained_file_identity(&directory).map_err(|error| error_at(parent, error))
}

pub(super) fn require_parent(
    path: &Path,
    expected: &subc_os::RetainedFileIdentity,
) -> Result<(), String> {
    if &parent_identity(path)? != expected {
        return Err(error_at(
            path,
            "parent directory changed since the revert check",
        ));
    }
    Ok(())
}

fn check_type(path: &Path, info: &fs::Metadata, directory: bool) -> Result<(), String> {
    if is_reparse_point(info)
        || (if directory {
            !info.is_dir()
        } else {
            !info.is_file()
        })
    {
        return Err(error_at(
            path,
            if directory {
                "not a plain directory"
            } else {
                "not a plain file"
            },
        ));
    }
    Ok(())
}

fn check_owner(path: &Path, _info: &fs::Metadata, _directory: bool) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if _info.uid() != rustix::process::geteuid().as_raw() {
            return Err(error_at(path, "not owned by the current user"));
        }
    }
    #[cfg(windows)]
    subc_os::windows_acl::verify_current_owner(path, _directory)
        .map_err(|error| error_at(path, error))?;
    Ok(())
}

pub(super) fn check_absent_parent(path: &Path) -> Result<(), String> {
    let parent = check_parent(path)?;
    let info = fs::symlink_metadata(parent).map_err(|error| error_at(parent, error))?;
    check_type(parent, &info, true)?;
    check_owner(parent, &info, true)
}

pub(super) fn suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    name.into()
}

/// The destination is private to the window. A published copy may outlive a
/// crash before source removal; never replace it, including on that retry.
pub(super) fn set_aside(kind: Kind, source: &Path, dest: &Path, sha: &str) -> Result<(), String> {
    let parent = parent_identity(source)?;
    let live = kind.snapshot(source)?;
    let partial = suffix(dest, ".partial");
    durable::remove_artifact(&partial)?;
    if paths::info(dest)?.is_some() {
        kind.verify(dest, sha)?;
        durable::parent(dest)?;
        if live.digest.as_deref() == Some(sha) {
            require_parent(source, &parent)?;
            remove_checked(kind, source, &live)?;
        }
        return durable::parent(source);
    }
    if live.digest.as_deref() != Some(sha) {
        return Err(error_at(source, "changed before set-aside"));
    }
    require_parent(source, &parent)?;
    match try_move_new(source, dest)? {
        true => return Ok(()),
        false => kind.copy(source, &partial, sha, live.mode)?,
    }
    require_reading(kind, source, &live)?;
    move_new(&partial, dest)?;
    require_parent(source, &parent)?;
    remove_checked(kind, source, &live)
}

pub(super) fn move_new(source: &Path, dest: &Path) -> Result<(), String> {
    if !try_move_new(source, dest)? {
        return Err(error_at(dest, "staged copy is on a different filesystem"));
    }
    Ok(())
}

/// An existence check followed by a plain rename could replace a file that
/// appeared in between. Use the platform's exclusive rename for publication.
fn try_move_new(from: &Path, to: &Path) -> Result<bool, String> {
    #[cfg(test)]
    if durable::CROSS_DEVICE_FROM.with(|source| source.borrow().as_deref() == Some(from)) {
        return Ok(false);
    }
    #[cfg(windows)]
    let renamed = subc_os::durable_move_to_new_name(from, to);
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let renamed = rustix::fs::renameat_with(
        rustix::fs::CWD,
        from,
        rustix::fs::CWD,
        to,
        rustix::fs::RenameFlags::NOREPLACE,
    )
    .map_err(std::io::Error::from);
    if let Err(error) = renamed {
        if error.kind() == std::io::ErrorKind::CrossesDevices
            || cfg!(windows) && error.raw_os_error() == Some(17)
        // ERROR_NOT_SAME_DEVICE
        {
            return Ok(false);
        }
        return Err(error_at(to, error));
    }
    durable::parent(to)?;
    if from.parent() != to.parent() {
        durable::parent(from)?;
    }
    Ok(true)
}

pub(super) fn require_reading(kind: Kind, path: &Path, expected: &Reading) -> Result<(), String> {
    if kind.snapshot(path)? != *expected {
        return Err(error_at(
            path,
            "content or mode changed since the revert check",
        ));
    }
    Ok(())
}

pub(super) fn remove_checked(kind: Kind, path: &Path, expected: &Reading) -> Result<(), String> {
    require_reading(kind, path, expected)?;
    durable::remove(path)
}

/// Stage on the target volume in an exclusive scratch directory. A stale
/// scratch directory from a crash cannot collide with a later copy or with a
/// real target path, and the rename is the only publication of a staged copy.
pub(super) fn restore_copy(
    kind: Kind,
    source: &Path,
    target: &Path,
    sha: &str,
    mode: Option<u32>,
    expected: &Reading,
) -> Result<(), String> {
    let parent = check_parent(target)?;
    let parent_id = parent_identity(target)?;
    require_reading(kind, target, expected)?;
    let scratch = scratch_directory(parent)?;
    let stage = scratch.join("restore.partial");
    let scratch_id = parent_identity(&stage)?;
    let result = (|| {
        durable::parent(&scratch)?;
        kind.copy(source, &stage, sha, mode)?;
        require_parent(target, &parent_id)?;
        require_parent(&stage, &scratch_id)?;
        require_reading(kind, target, expected)?;
        if expected.digest.is_some() {
            subc_os::durable_replace(&stage, target).map_err(|error| error_at(target, error))?;
            durable::parent(&stage)?;
            durable::parent(target)
        } else {
            move_new(&stage, target)
        }
    })();
    if require_parent(&stage, &scratch_id).is_ok() {
        let _ = durable::remove_artifact(&scratch);
    }
    result
}

fn scratch_directory(parent: &Path) -> Result<PathBuf, String> {
    for _ in 0..=u16::MAX {
        let name = format!(".ck-window-{:04x}", super::journal::id::suffix());
        let path = parent.join(name);
        match durable::create_dir_exclusive(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error_at(&path, error)),
        }
    }
    Err(error_at(
        parent,
        "could not reserve a restore scratch directory",
    ))
}
