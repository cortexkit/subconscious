//! Puts a verified, already-staged binary at a managed destination that a
//! running process may be executing.
//!
//! Unix lets a rename replace the file a process is running: the process keeps
//! its open inode, so a single rename is the whole replacement. Windows refuses
//! to overwrite the image of a running process ("Access is denied"), but it does
//! let that image be renamed within its directory. So on Windows the previous
//! image is first moved to a sibling "aside" name, and the staged binary is then
//! renamed onto the freed destination. The old process keeps running from the
//! aside file until the caller restarts it; the aside file is deleted once that
//! restart is verified, or by a later `ck setup` / `ck upgrade` when Windows
//! still held it at that point. `ck`'s own self-update solves the same problem
//! for `ck.exe` (see `self_update_windows`); this is the same rename-aside
//! approach for the daemon and module binaries.

use std::{
    fs, io,
    path::{Path, PathBuf},
    process,
    time::{SystemTime, UNIX_EPOCH},
};

use serde_json::{Map, Value};

use super::inventory::Inventory;

/// Inventory kind recording a previous image moved aside during replacement.
/// The row's `replaces` field names the destination it was moved from.
/// The row is written before the move, so a crash between the two renames
/// still leaves the moved image on record instead of an unexplained file.
pub(super) const REPLACED_IMAGE_KIND: &str = "replaced-image";

/// Renames `staged` onto `destination`. On Windows the previous image is moved
/// aside first and its new path is returned; the caller removes it with
/// [`remove_replaced_image`] once the replacement is running. On error the
/// destination holds what it held before, except in the one case the message
/// spells out (the previous image could not be moved back). The staged file is
/// left for the caller to remove.
pub(super) fn place_staged_binary(
    staged: &Path,
    destination: &Path,
    inventory: &mut Inventory,
) -> Result<Option<PathBuf>, String> {
    place_with(staged, destination, inventory, cfg!(windows))
}

/// [`place_staged_binary`] with the Windows behaviour chosen by the caller:
/// `move_previous_aside` moves the current image aside before the rename.
/// Tests pass `true` to exercise that path on every platform.
pub(super) fn place_with(
    staged: &Path,
    destination: &Path,
    inventory: &mut Inventory,
    move_previous_aside: bool,
) -> Result<Option<PathBuf>, String> {
    if !move_previous_aside || !destination.exists() {
        rename(staged, destination).map_err(|error| replace_error(destination, &error))?;
        return Ok(None);
    }

    let aside = aside_path(destination)?;
    let mut fields = Map::new();
    fields.insert(
        "replaces".to_string(),
        Value::String(destination.to_string_lossy().into_owned()),
    );
    inventory.record(REPLACED_IMAGE_KIND, &aside, fields);
    inventory.save().map_err(|error| {
        format!(
            "could not record {} in installer-manifest.json before moving the running image aside: {error}",
            aside.display()
        )
    })?;

    if let Err(error) = rename(destination, &aside) {
        forget_replaced_image(inventory, &aside);
        return Err(format!(
            "could not move the previous image {} aside to {}: {error}",
            destination.display(),
            aside.display()
        ));
    }
    if let Err(error) = rename(staged, destination) {
        // Put the previous image back so the failed upgrade leaves the
        // destination as it found it, and report the error that stopped the
        // replacement rather than anything the restore ran into.
        return match rename(&aside, destination) {
            Ok(()) => {
                forget_replaced_image(inventory, &aside);
                Err(replace_error(destination, &error))
            }
            Err(restore) => Err(format!(
                "{}; additionally could not move the previous image back from {}: {restore}",
                replace_error(destination, &error),
                aside.display()
            )),
        };
    }
    Ok(Some(aside))
}

/// Deletes a previous image moved aside by [`place_staged_binary`] and drops its
/// inventory row. A file that is already gone counts as removed. An error means
/// Windows still holds the image (its process has not exited yet); the row stays
/// so [`sweep_replaced_images`] tries again later.
pub(super) fn remove_replaced_image(inventory: &mut Inventory, aside: &Path) -> Result<(), String> {
    match fs::remove_file(aside) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "could not delete previous image {}: {error}",
                aside.display()
            ))
        }
    }
    inventory.remove_owned_path(REPLACED_IMAGE_KIND, aside);
    inventory.save()
}

/// Deletes every previous image an earlier replacement left behind because its
/// process was still running then. Images still held are kept for the next
/// sweep, with a warning. An image whose destination is missing is also kept:
/// it may be the only copy of that binary left on disk.
pub(super) fn sweep_replaced_images(inventory: &mut Inventory) {
    for aside in inventory.paths_for_kind(REPLACED_IMAGE_KIND) {
        let destination_present = inventory
            .entry_for_path(REPLACED_IMAGE_KIND, &aside)
            .and_then(|entry| entry.get("replaces"))
            .and_then(Value::as_str)
            .is_some_and(|destination| Path::new(destination).is_file());
        if !destination_present {
            continue;
        }
        if let Err(error) = remove_replaced_image(inventory, &aside) {
            eprintln!("warning: {error}; it will be removed by a later ck setup or ck upgrade");
        }
    }
}

fn forget_replaced_image(inventory: &mut Inventory, aside: &Path) {
    inventory.remove_owned_path(REPLACED_IMAGE_KIND, aside);
    // The row only matters while the aside file exists, and it does not
    // here; a failed save leaves a row the next sweep drops as already gone.
    let _ = inventory.save();
}

fn replace_error(destination: &Path, error: &io::Error) -> String {
    format!(
        "could not replace managed destination {}: {error}",
        destination.display()
    )
}

/// A fresh sibling name per replacement, so an image a still-running process
/// holds from an earlier upgrade never blocks the next one.
fn aside_path(destination: &Path) -> Result<PathBuf, String> {
    let parent = destination.parent().ok_or_else(|| {
        format!(
            "managed destination {} has no parent",
            destination.display()
        )
    })?;
    let name = destination
        .file_name()
        .ok_or_else(|| {
            format!(
                "managed destination {} has no file name",
                destination.display()
            )
        })?
        .to_string_lossy();
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("clock before Unix epoch: {error}"))?
        .as_nanos();
    Ok(parent.join(format!(".{name}.replaced-{}-{nonce}", process::id())))
}

fn rename(from: &Path, to: &Path) -> io::Result<()> {
    #[cfg(test)]
    if test_seam::refuses(from) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "rename refused by test",
        ));
    }
    fs::rename(from, to)
}

/// Lets a test make the rename of one source path fail, the way Windows
/// refuses a rename while something holds the file, without needing a
/// platform that does so. Per thread, so parallel tests do not interfere.
#[cfg(test)]
pub(super) mod test_seam {
    use std::{
        cell::RefCell,
        path::{Path, PathBuf},
    };

    thread_local! {
        static REFUSED_SOURCE: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
    }

    /// Every later rename from `source` on this thread fails until the
    /// returned guard is dropped.
    pub(in crate::setup) fn refuse_rename_from(source: &Path) -> RefusalGuard {
        REFUSED_SOURCE.with(|refused| *refused.borrow_mut() = Some(source.to_path_buf()));
        RefusalGuard
    }

    pub(super) fn refuses(source: &Path) -> bool {
        REFUSED_SOURCE.with(|refused| refused.borrow().as_deref() == Some(source))
    }

    pub(in crate::setup) struct RefusalGuard;

    impl Drop for RefusalGuard {
        fn drop(&mut self) {
            REFUSED_SOURCE.with(|refused| *refused.borrow_mut() = None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cortexkit_test_support::ScratchDir;

    struct Fixture {
        root: ScratchDir,
        destination: PathBuf,
        staged: PathBuf,
        inventory: Inventory,
    }

    fn fixture(name: &str) -> Fixture {
        let root = ScratchDir::new(name);
        let destination = root.join("ckdev-subc.exe");
        let staged = root.join(".ckdev-subc.upgrade");
        fs::write(&destination, "previous image").unwrap();
        fs::write(&staged, "verified candidate").unwrap();
        let inventory =
            Inventory::load(root.join("installer-manifest.json"), "windows-x64").unwrap();
        Fixture {
            root,
            destination,
            staged,
            inventory,
        }
    }

    fn entries(root: &Path) -> Vec<String> {
        let mut names = fs::read_dir(root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    #[test]
    fn plain_placement_renames_the_staged_binary_over_the_destination() {
        let mut f = fixture("managed-replace-plain");
        let aside = place_with(&f.staged, &f.destination, &mut f.inventory, false).unwrap();
        assert_eq!(aside, None);
        assert_eq!(
            fs::read_to_string(&f.destination).unwrap(),
            "verified candidate"
        );
        assert!(!f.staged.exists());
    }

    #[test]
    fn aside_placement_keeps_the_previous_image_until_it_is_removed() {
        let mut f = fixture("managed-replace-aside");
        let aside = place_with(&f.staged, &f.destination, &mut f.inventory, true)
            .unwrap()
            .expect("the previous image is moved aside");
        assert_eq!(aside.parent(), f.destination.parent());
        assert_eq!(
            fs::read_to_string(&f.destination).unwrap(),
            "verified candidate"
        );
        assert_eq!(fs::read_to_string(&aside).unwrap(), "previous image");
        assert!(!f.staged.exists());
        let saved = Inventory::load(f.root.join("installer-manifest.json"), "windows-x64").unwrap();
        assert!(saved.owns_path(REPLACED_IMAGE_KIND, &aside));

        remove_replaced_image(&mut f.inventory, &aside).unwrap();
        assert!(!aside.exists());
        let saved = Inventory::load(f.root.join("installer-manifest.json"), "windows-x64").unwrap();
        assert!(!saved.owns_path(REPLACED_IMAGE_KIND, &aside));
    }

    #[test]
    fn a_refused_second_rename_moves_the_previous_image_back() {
        let mut f = fixture("managed-replace-restore");
        let _refusal = test_seam::refuse_rename_from(&f.staged);
        let error = place_with(&f.staged, &f.destination, &mut f.inventory, true)
            .expect_err("the staged binary cannot be renamed");
        assert!(error.contains("rename refused by test"), "{error}");
        assert!(!error.contains("additionally"), "{error}");
        assert_eq!(
            fs::read_to_string(&f.destination).unwrap(),
            "previous image"
        );
        assert_eq!(
            entries(&f.root),
            [
                ".ckdev-subc.upgrade",
                "ckdev-subc.exe",
                "installer-manifest.json"
            ],
            "only the staged file the caller removes may remain beside the destination"
        );
        assert!(f.inventory.paths_for_kind(REPLACED_IMAGE_KIND).is_empty());
    }

    #[test]
    fn sweep_removes_left_images_but_keeps_one_whose_destination_is_missing() {
        let mut f = fixture("managed-replace-sweep");
        let aside = place_with(&f.staged, &f.destination, &mut f.inventory, true)
            .unwrap()
            .unwrap();
        let orphan_destination = f.root.join("ckdev-aft.exe");
        let orphan = f.root.join(".ckdev-aft.exe.replaced-1-1");
        fs::write(&orphan, "only remaining image").unwrap();
        let mut fields = Map::new();
        fields.insert(
            "replaces".to_string(),
            Value::String(orphan_destination.to_string_lossy().into_owned()),
        );
        f.inventory.record(REPLACED_IMAGE_KIND, &orphan, fields);

        sweep_replaced_images(&mut f.inventory);

        assert!(!aside.exists());
        assert!(orphan.exists());
        let saved = Inventory::load(f.root.join("installer-manifest.json"), "windows-x64").unwrap();
        assert_eq!(saved.paths_for_kind(REPLACED_IMAGE_KIND), [orphan]);
    }
}
