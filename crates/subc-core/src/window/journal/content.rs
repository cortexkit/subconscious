use std::{
    fs, io,
    path::{Path, PathBuf},
};

use sha2::{Digest, Sha256};

use crate::setup::install_journal::{is_reparse_point, open_journal};

use super::{
    durable, entries,
    paths::{error_at, info},
};

pub fn file_info(path: &Path) -> Result<Option<fs::Metadata>, String> {
    let info = info(path)?;
    if info
        .as_ref()
        .is_some_and(|info| !info.is_file() || is_reparse_point(info))
    {
        return Err(error_at(path, "not a plain file"));
    }
    Ok(info)
}

/// Hash the bytes of a plain file without following a final symlink.
pub fn file_digest(path: &Path) -> Result<Option<String>, String> {
    if file_info(path)?.is_none() {
        return Ok(None);
    }
    let mut file = subc_os::open_regular_nofollow(path).map_err(|error| error_at(path, error))?;
    let mut digest = Sha256::new();
    io::copy(&mut file, &mut digest).map_err(|error| error_at(path, error))?;
    Ok(Some(format!("{:x}", digest.finalize())))
}

/// Digest the bundle tree, using the installer's digest on Unix.
#[cfg(unix)]
pub fn bundle_digest(path: &Path) -> Result<Option<String>, String> {
    crate::setup::install_journal::bundle::tree_digest(path)
}

#[cfg(windows)]
pub fn bundle_digest(path: &Path) -> Result<Option<String>, String> {
    if !super::paths::plain_dir(path)? {
        return Ok(None);
    }
    let mut digest = Sha256::new();
    hash_windows_tree(path, path, &mut digest)?;
    Ok(Some(format!("{:x}", digest.finalize())))
}

#[cfg(windows)]
fn hash_windows_tree(root: &Path, path: &Path, digest: &mut Sha256) -> Result<(), String> {
    use std::os::windows::{ffi::OsStrExt, fs::FileTypeExt};
    let put_path = |digest: &mut Sha256, path: &Path| {
        let units: Vec<_> = path.as_os_str().encode_wide().collect();
        digest.update((units.len() as u64).to_le_bytes());
        for unit in units {
            digest.update(unit.to_le_bytes());
        }
    };
    let metadata = fs::symlink_metadata(path).map_err(|error| error_at(path, error))?;
    put_path(digest, path.strip_prefix(root).expect("bundle descendant"));
    if metadata.file_type().is_symlink() {
        digest.update(if metadata.file_type().is_symlink_dir() {
            b"LD"
        } else {
            b"LF"
        });
        put_path(
            digest,
            &fs::read_link(path).map_err(|error| error_at(path, error))?,
        );
    } else if is_reparse_point(&metadata) {
        return Err(error_at(
            path,
            "bundle contains an unsupported reparse point",
        ));
    } else if metadata.is_dir() {
        digest.update(b"D");
        for entry in entries(path)? {
            hash_windows_tree(root, &entry, digest)?;
        }
    } else if metadata.is_file() {
        digest.update(b"F");
        digest.update(file_digest(path)?.ok_or_else(|| error_at(path, "bundle file disappeared"))?);
    } else {
        return Err(error_at(path, "bundle contains a special file"));
    }
    Ok(())
}

pub(crate) fn mode(metadata: &fs::Metadata) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Some(metadata.permissions().mode() & 0o7777)
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        None
    }
}

pub(crate) fn tree_size(path: &Path) -> Result<u64, String> {
    let metadata = fs::symlink_metadata(path).map_err(|error| error_at(path, error))?;
    if metadata.file_type().is_symlink() {
        return Ok(metadata.len());
    }
    if is_reparse_point(&metadata) {
        return Err(error_at(path, "unsupported reparse point"));
    }
    if metadata.is_dir() {
        entries(path)?.into_iter().try_fold(0_u64, |sum, entry| {
            Ok(sum.saturating_add(tree_size(&entry)?))
        })
    } else if metadata.is_file() {
        Ok(metadata.len())
    } else {
        Err(error_at(path, "not a plain file or directory"))
    }
}

pub(crate) fn require_space(path: &Path, needed: u64) -> Result<(), String> {
    #[cfg(test)]
    let supplied = SPACE.with(|space| space.get());
    #[cfg(not(test))]
    let supplied = None;
    let free = supplied
        .or_else(|| subc_os::host::disk_space(path).map(|usage| usage.free_bytes))
        .ok_or_else(|| error_at(path, "could not determine available space"))?;
    if free < needed {
        return Err(error_at(
            path,
            format!("not enough free space: need {needed} bytes, have {free}"),
        ));
    }
    Ok(())
}

pub(crate) fn copy_checked(source: &Path, copy: &Path) -> Result<String, String> {
    let mut input =
        subc_os::open_regular_nofollow(source).map_err(|error| error_at(source, error))?;
    let mut staged = open_journal(copy, true).map_err(|error| error_at(copy, error))?;
    io::copy(&mut input, &mut staged).map_err(|error| error_at(copy, error))?;
    #[cfg(test)]
    if TRUNCATE.with(|truncate| truncate.replace(false)) {
        staged.set_len(0).map_err(|error| error_at(copy, error))?;
    }
    durable::file(copy, &staged)?;
    drop(staged);
    durable::parent(copy)?;
    verify_copy(source, copy, file_digest)
}

/// Transfer an already-created snapshot if its filesystem supports the rename.
/// Hash the open file before publication and confirm the rename moved that file.
pub(crate) fn move_snapshot(
    source: &Path,
    destination: &Path,
) -> Result<Option<String>, durable::RenameError> {
    use durable::RenameError;

    let before = |error: io::Error| RenameError::before(error_at(source, error));
    let mut file = open_journal(source, false).map_err(before)?;
    let info = file.metadata().map_err(before)?;
    if !info.is_file() || is_reparse_point(&info) {
        return Err(RenameError::before(error_at(
            source,
            "not a plain snapshot file",
        )));
    }
    let identity = subc_os::retained_file_identity(&file).map_err(before)?;
    let mut digest = Sha256::new();
    io::copy(&mut file, &mut digest).map_err(before)?;
    durable::file(source, &file).map_err(RenameError::before)?;
    if !durable::try_rename_new(source, destination)? {
        return Ok(None);
    }
    let file = subc_os::open_regular_nofollow(destination)
        .map_err(|error| RenameError::after(error_at(destination, error)))?;
    let moved = subc_os::retained_file_identity(&file)
        .map_err(|error| RenameError::after(error_at(destination, error)))?;
    if identity != moved {
        return Err(RenameError::after(error_at(
            destination,
            "snapshot identity changed during rename",
        )));
    }
    Ok(Some(format!("{:x}", digest.finalize())))
}

pub(crate) fn bundle_copy_checked(source: &Path, copy: &Path) -> Result<String, String> {
    copy_bundle(source, copy)?;
    sync_tree(copy)?;
    durable::parent(copy)?;
    verify_copy(source, copy, bundle_digest)
}

fn verify_copy(
    source: &Path,
    copy: &Path,
    digest: impl Fn(&Path) -> Result<Option<String>, String>,
) -> Result<String, String> {
    let expected = digest(source)?.ok_or_else(|| error_at(source, "source disappeared"))?;
    let actual = digest(copy)?;
    if Some(expected.as_str()) != actual.as_deref() {
        return Err(error_at(
            copy,
            format!(
                "saved copy digest {:?} does not match source digest {expected}",
                actual.as_deref().unwrap_or("absent")
            ),
        ));
    }
    Ok(expected)
}

#[cfg(unix)]
fn copy_bundle(source: &Path, copy: &Path) -> Result<(), String> {
    use std::process::Command;
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = Command::new("/usr/bin/ditto");
        command.args(["--rsrc", "--extattr", "--acl"]);
        command
    };
    #[cfg(not(target_os = "macos"))]
    let mut command = {
        let mut command = Command::new("cp");
        command.args(["-RP", "--preserve=mode,timestamps,links,xattr", "--"]);
        command
    };
    let result = command
        .arg(source)
        .arg(copy)
        .output()
        .map_err(|error| error_at(copy, error))?;
    if !result.status.success() {
        return Err(error_at(
            copy,
            String::from_utf8_lossy(&result.stderr).trim(),
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn copy_bundle(source: &Path, copy: &Path) -> Result<(), String> {
    use std::os::windows::fs::{symlink_dir, symlink_file, FileTypeExt};
    let result = (|| -> io::Result<()> {
        let metadata = fs::symlink_metadata(source)?;
        if metadata.file_type().is_symlink_dir() {
            symlink_dir(fs::read_link(source)?, copy)?;
        } else if metadata.file_type().is_symlink_file() {
            symlink_file(fs::read_link(source)?, copy)?;
        } else if is_reparse_point(&metadata) {
            return Err(io::Error::other("unsupported reparse point"));
        } else if metadata.is_dir() {
            fs::create_dir(copy)?;
            durable::parent(copy).map_err(io::Error::other)?;
            for entry in fs::read_dir(source)? {
                let entry = entry?;
                copy_bundle(&entry.path(), &copy.join(entry.file_name()))
                    .map_err(io::Error::other)?;
            }
        } else if metadata.is_file() {
            fs::copy(source, copy)?;
        } else {
            return Err(io::Error::other("not a plain file or directory"));
        }
        Ok(())
    })();
    result.map_err(|error| error_at(copy, error))
}

fn sync_tree(path: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path).map_err(|error| error_at(path, error))?;
    if metadata.file_type().is_symlink() {
        return durable::parent(path);
    }
    if is_reparse_point(&metadata) {
        return Err(error_at(path, "unsupported reparse point"));
    }
    if metadata.is_dir() {
        for entry in entries(path)? {
            sync_tree(&entry)?;
        }
        // Every directory name, including one containing only links, matters.
        durable::parent(&path.join(".directory-sync"))?;
    } else if metadata.is_file() {
        let file = fs::OpenOptions::new()
            .read(true)
            .write(cfg!(windows))
            .open(path)
            .map_err(|error| error_at(path, error))?;
        durable::file(path, &file)?;
    } else {
        return Err(error_at(path, "not a plain file or directory"));
    }
    durable::parent(path)
}

pub(crate) fn floor_paths(tree: &Path) -> Result<Vec<PathBuf>, String> {
    let mut floors = Vec::new();
    let mut directories = vec![tree.to_owned()];
    while let Some(dir) = directories.pop() {
        for path in entries(&dir)? {
            if path
                .file_name()
                .is_some_and(|name| name == "format-floor.json")
            {
                if file_info(&path)?.is_none() {
                    return Err(error_at(&path, "floor file disappeared"));
                }
                floors.push(
                    path.strip_prefix(tree)
                        .expect("store descendant")
                        .to_owned(),
                );
            } else if info(&path)?.is_some_and(|value| value.is_dir() && !is_reparse_point(&value))
            {
                directories.push(path);
            }
        }
    }
    floors.sort();
    Ok(floors)
}

#[cfg(test)]
thread_local! {
    pub(crate) static SPACE: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
    pub(crate) static TRUNCATE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
