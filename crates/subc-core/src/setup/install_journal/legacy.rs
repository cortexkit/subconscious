//! One-time reconciliation of records written by released installers.

use super::{bootstrap, files};
use crate::setup::components::OWNED_BINARY_KINDS;
#[cfg(windows)]
use crate::setup::inventory::Inventory;
use serde_json::{Map, Value};
use std::{
    fs, io,
    path::{Path, PathBuf},
};

struct Sidecar {
    binary: Map<String, Value>,
    history: Vec<(String, PathBuf, Map<String, Value>)>,
}

pub(super) fn reconcile(manifest: &Path) -> Result<(), String> {
    let mut inventory = bootstrap::read_inventory(manifest)?;
    bootstrap::refuse_legacy(manifest, &inventory)?;
    let root = manifest
        .parent()
        .ok_or("installer manifest has no parent")?;
    let sidecar = root.join("installer-manifest.bootstrap.json");
    match read_sidecar(&sidecar, &bootstrap::destination(root)) {
        Ok(Some(released)) => {
            inventory.start_transaction()?;
            if let Err(error) = inventory.transaction().unwrap().remove(&sidecar) {
                inventory.undo_transaction(&mut bootstrap::FileRecovery)?;
                warn(&sidecar, &error);
            } else {
                let target = bootstrap::destination(root);
                for kind in OWNED_BINARY_KINDS {
                    for old in inventory.paths_for_kind(kind) {
                        if same_sibling(&old, &target) {
                            inventory.remove_owned_path(kind, &old);
                        }
                    }
                }
                inventory.record("managed-binary", &target, released.binary);
                for (kind, path, row) in released.history {
                    inventory.remove_owned_path(&kind, &path);
                    inventory.record(&kind, &path, row);
                }
                inventory.commit_transaction(&mut bootstrap::FileRecovery)?;
            }
        }
        Ok(None) => {}
        Err(error) => warn(&sidecar, &error),
    }
    #[cfg(windows)]
    cleanup_images(root, &mut inventory)?;
    Ok(())
}

fn read_sidecar(path: &Path, target: &Path) -> Result<Option<Sidecar>, String> {
    let file = match subc_os::open_regular_nofollow(path) {
        Ok(file) => file,
        Err(subc_os::OpenRegularError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(None)
        }
        Err(error) => return Err(error.to_string()),
    };
    let document: Value = serde_json::from_reader(file).map_err(|error| error.to_string())?;
    let rows = document["mutations"]
        .as_array()
        .ok_or("missing bootstrap rows")?;
    let mut placements = rows.iter().filter(|row| row["kind"] == "binary-placement");
    let row = placements
        .next()
        .ok_or("missing bootstrap executable placement")?;
    let recorded = row["path"]
        .as_str()
        .map(Path::new)
        .ok_or("missing bootstrap executable path")?;
    if document["schema_version"] != 1
        || document["installer"] != "ck"
        || placements.next().is_some()
        || !same_sibling(recorded, target)
    {
        return Err("not a released bootstrap placement at the expected path".into());
    }
    let root = path.parent().ok_or("missing install directory")?;
    let parent = target.parent().ok_or("missing executable directory")?;
    let canonical = |path: &Path| fs::canonicalize(path).map_err(|error| error.to_string());
    let canonical_root = canonical(root)?;
    if !canonical(parent)?.starts_with(&canonical_root) {
        return Err("bootstrap executable directory is outside the install root".into());
    }
    let expected = row["sha256"]
        .as_str()
        .filter(|digest| valid_digest(digest))
        .ok_or("missing or invalid bootstrap executable digest")?;
    if files::digest(target)?.as_deref() != Some(expected) {
        return Err(format!(
            "recorded executable digest does not match {}",
            target.display()
        ));
    }
    let mut fields = Map::from_iter([
        ("component".into(), Value::String("core".into())),
        ("sha256".into(), Value::String(expected.into())),
    ]);
    if let Some(archive) = row["archive_sha256"]
        .as_str()
        .filter(|digest| valid_digest(digest))
    {
        fields.insert("archive_sha256".into(), Value::String(archive.into()));
    }
    let history = rows
        .iter()
        .filter_map(|row| {
            let kind = row["kind"].as_str()?;
            let path = Path::new(row["path"].as_str()?);
            match kind {
                "shell-profile-path" | "user-path-registry" => {}
                "legacy-cli-retirement" => {
                    let expected = row["sha256"]
                        .as_str()
                        .filter(|digest| valid_digest(digest))?;
                    let backup = Path::new(row["backup_path"].as_str()?);
                    let valid = path.is_absolute()
                        && backup.is_absolute()
                        && same_sibling(path, backup)
                        && backup
                            .parent()
                            .and_then(|parent| canonical(parent).ok())
                            .is_some_and(|parent| parent.starts_with(&canonical_root))
                        && files::digest(backup)
                            .is_ok_and(|digest| digest.as_deref() == Some(expected));
                    if !valid {
                        warn(
                            backup,
                            "retirement backup does not match a file inside the install root",
                        );
                        return None;
                    }
                }
                _ => return None,
            }
            Some((kind.to_owned(), path.to_owned(), row.as_object()?.clone()))
        })
        .collect();
    Ok(Some(Sidecar {
        binary: fields,
        history,
    }))
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn warn(path: &Path, error: &str) {
    crate::setup::messages::warning(format!(
        "left legacy installer file {} untouched: {error}",
        path.display()
    ));
}

pub(super) fn same_sibling(left: &Path, right: &Path) -> bool {
    left == right
        || (left.file_name() == right.file_name()
            && left
                .parent()
                .and_then(|parent| fs::canonicalize(parent).ok())
                .zip(
                    right
                        .parent()
                        .and_then(|parent| fs::canonicalize(parent).ok()),
                )
                .is_some_and(|(left, right)| left == right))
}

#[cfg(windows)]
fn cleanup_images(root: &Path, inventory: &mut Inventory) -> Result<(), String> {
    for kind in ["self-update-rollback", "replaced-image"] {
        for leftover in inventory.paths_for_kind(kind) {
            let row = inventory
                .entry_for_path(kind, &leftover)
                .expect("known legacy row");
            let present = match check_image(root, kind, &leftover, row, inventory) {
                Ok(present) => present,
                Err(error) => {
                    warn_image(&leftover, &error);
                    continue;
                }
            };
            inventory.start_transaction()?;
            if present {
                if let Err(error) = inventory.transaction().unwrap().remove(&leftover) {
                    inventory.undo_transaction(&mut bootstrap::FileRecovery)?;
                    warn_image(&leftover, &error);
                    continue;
                }
            }
            inventory.remove_owned_path(kind, &leftover);
            inventory.commit_transaction(&mut bootstrap::FileRecovery)?;
        }
    }
    Ok(())
}

#[cfg(windows)]
fn warn_image(path: &Path, error: &str) {
    warn(path, &format!("{error}. If it is no longer needed, remove this file yourself; the installer will drop the row on the next run"));
}

#[cfg(windows)]
fn check_image(
    root: &Path,
    kind: &str,
    leftover: &Path,
    row: &Value,
    inventory: &Inventory,
) -> Result<bool, String> {
    use std::os::windows::fs::OpenOptionsExt;
    match fs::symlink_metadata(leftover) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.to_string()),
        Ok(_) => {}
    }
    let replaced = row["replaces"]
        .as_str()
        .map(Path::new)
        .ok_or("missing released destination")?;
    let filename = replaced
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("missing destination name")?;
    let parent = leftover.parent().ok_or("legacy image has no parent")?;
    let canonical_root = fs::canonicalize(root).map_err(|error| error.to_string())?;
    let canonical_parent = fs::canonicalize(parent).map_err(|error| error.to_string())?;
    if !leftover.is_absolute()
        || !replaced.is_absolute()
        || !canonical_parent.starts_with(&canonical_root)
    {
        return Err("legacy image is outside the installation root".into());
    }
    let replaced_in_parent = parent.join(filename);
    if !same_sibling(replaced, &replaced_in_parent)
        || !OWNED_BINARY_KINDS
            .into_iter()
            .chain(["managed-link"])
            .any(|kind| {
                inventory
                    .paths_for_kind(kind)
                    .iter()
                    .any(|path| same_sibling(path, replaced))
            })
    {
        return Err("released destination is not an installer-owned sibling".into());
    }
    let name = leftover
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("legacy image has no filename")?;
    let released_name = if kind == "self-update-rollback" {
        filename.eq_ignore_ascii_case("ck.exe") && name.eq_ignore_ascii_case("ck.exe.old")
    } else {
        name.strip_prefix(&format!(".{filename}.replaced-"))
            .and_then(|suffix| suffix.split_once('-'))
            .is_some_and(|(pid, time)| pid.parse::<u32>().is_ok() && time.parse::<u128>().is_ok())
    };
    if !released_name {
        return Err("file does not have the released rollback filename".into());
    }
    let Some(digest) = files::digest(leftover)? else {
        return Ok(false);
    };
    if row
        .get("sha256")
        .is_some_and(|expected| expected.as_str() != Some(&digest))
    {
        return Err("file does not match the recorded digest".into());
    }
    if files::digest(replaced)?.is_none() {
        return Err("the released destination is missing; preserving its possible backup".into());
    }
    // A program can keep running from an executable after it is renamed. Opening for write
    // without changing any bytes distinguishes that case before we move it.
    const OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    let write = fs::OpenOptions::new()
        .write(true)
        .custom_flags(OPEN_REPARSE_POINT)
        .open(leftover)
        .map_err(|error| format!("image is still mapped or cannot be checked: {error}"))?;
    let read = subc_os::open_regular_nofollow(leftover).map_err(|error| error.to_string())?;
    if subc_os::retained_file_identity(&write).map_err(|error| error.to_string())?
        != subc_os::retained_file_identity(&read).map_err(|error| error.to_string())?
    {
        return Err("file identity changed while checking it".into());
    }
    Ok(true)
}

#[cfg(test)]
#[test]
fn a_sidecar_with_no_verified_placement_is_kept_without_refusing() {
    let root = cortexkit_test_support::ScratchDir::new("legacy-sidecar");
    let manifest = root.join("installer-manifest.json");
    let sidecar = root.join("installer-manifest.bootstrap.json");
    fs::write(&sidecar, "unverified").unwrap();
    let _hold = crate::setup::install_lock::InstallHold::take(
        &manifest,
        "test legacy cleanup",
        std::time::Duration::ZERO,
    )
    .unwrap();
    reconcile(&manifest).unwrap();
    assert_eq!(fs::read_to_string(sidecar).unwrap(), "unverified");
    assert!(!manifest.exists());
}
