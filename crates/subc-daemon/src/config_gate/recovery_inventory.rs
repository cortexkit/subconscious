//! The inventory is an index of names, not authority to read or change settings.
use super::{
    capture::{file_object_id, FileObjectId},
    GateError, Result,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    ffi::OsStr,
    fs::File,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard, Weak},
};
use subc_control::config::ConfigSettingsRecoveryCopy;

const DIRECTORY: &str = "settings-recovery";
const INDEX: &str = ".inventory.json";
const INDEX_LIMIT: u64 = 8 * 1024 * 1024;
const STATUS_ROWS: usize = 50;

#[derive(Clone, Deserialize, Serialize)]
struct Entry {
    path: PathBuf,
    recovery_path: PathBuf,
    at: u64,
    #[serde(default)]
    parent: Option<FileObjectId>,
}

#[derive(Default)]
pub(super) struct RecoveryLocks(Mutex<BTreeMap<String, Weak<Mutex<()>>>>);

pub(super) struct RecoveryArchive {
    pub directory: File,
    updates: Arc<RecoveryLocks>,
}

pub(super) struct FolderInventory<'a> {
    archive: &'a RecoveryArchive,
    key: String,
    updates: Arc<Mutex<()>>,
}

impl RecoveryArchive {
    pub(super) fn open(
        data: &Path,
        updates: Arc<RecoveryLocks>,
        probe: impl FnOnce(&File) -> io::Result<()>,
    ) -> Result<Self> {
        let parent = subc_os::open_directory_nofollow_all(data).map_err(refused)?;
        let directory =
            subc_os::open_or_create_private_directory_at(&parent, OsStr::new(DIRECTORY))
                .map_err(refused)?;
        probe(&directory).map_err(refused)?;
        Ok(Self { directory, updates })
    }

    pub(super) fn folder(&self, directory: &File) -> Result<FolderInventory<'_>> {
        let identity = file_object_id(directory)?;
        let digest = subc_config::raw_digest(&serde_json::to_vec(&identity).map_err(refused)?);
        let key = digest.as_str().trim_start_matches("sha256:").to_owned();
        let mut locks = self.updates.0.lock().map_err(refused)?;
        locks.retain(|_, lock| lock.strong_count() != 0);
        let updates = if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
            lock
        } else {
            let lock = Arc::new(Mutex::new(()));
            locks.insert(key.clone(), Arc::downgrade(&lock));
            lock
        };
        Ok(FolderInventory {
            archive: self,
            key,
            updates,
        })
    }
}

impl FolderInventory<'_> {
    pub(super) fn short_key(&self) -> &str {
        &self.key[..24]
    }

    pub(super) fn serialize_updates(&self) -> Result<MutexGuard<'_, ()>> {
        self.updates.lock().map_err(refused)
    }

    /// Write the name before moving bytes. A crash before the move can leave a
    /// harmless index entry; status only returns entries that exist on disk.
    pub(super) fn record(
        &self,
        path: &Path,
        recovery_path: &Path,
        parent: &File,
        at: u64,
    ) -> Result<()> {
        let name = format!(".inventory-{}.json", self.key);
        let name = OsStr::new(&name);
        let directory = &self.archive.directory;
        let (index, mut entries) = read_index(directory, name)?;
        entries.retain(|entry| !proved_absent(entry));
        if entries
            .iter()
            .any(|entry| entry.recovery_path == recovery_path)
        {
            return Err(refused("the recovery inventory name is already reserved"));
        }
        entries.push(Entry {
            path: path.into(),
            recovery_path: recovery_path.into(),
            at,
            parent: Some(file_object_id(parent)?),
        });
        let bytes = serde_json::to_vec(&entries).map_err(refused)?;
        if bytes.len() as u64 > INDEX_LIMIT {
            return Err(refused("settings recovery inventory is full"));
        }
        let mut stage = subc_os::RetainedTempFile::create(directory).map_err(refused)?;
        stage.file_mut().write_all(&bytes).map_err(refused)?;
        if let Some(index) = index {
            stage.replace(name, &index).map_err(refused)
        } else {
            stage.publish_restored(name, None, None).map_err(refused)
        }
    }
}

fn read_index(directory: &File, name: &OsStr) -> Result<(Option<File>, Vec<Entry>)> {
    let Some(index) =
        subc_os::open_single_link_file_nofollow_at(directory, name).map_err(refused)?
    else {
        return Ok((None, Vec::new()));
    };
    let mut bytes = Vec::new();
    (&index)
        .take(INDEX_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(refused)?;
    if bytes.len() as u64 > INDEX_LIMIT {
        return Err(refused("settings recovery inventory is too large"));
    }
    let entries = serde_json::from_slice(&bytes).map_err(refused)?;
    Ok((Some(index), entries))
}

pub(super) fn status(data: &Path) -> Result<(u64, Vec<ConfigSettingsRecoveryCopy>)> {
    let parent = match subc_os::open_directory_nofollow_all(data) {
        Ok(parent) => parent,
        Err(subc_os::OpenRegularError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
            return Ok((0, Vec::new()))
        }
        Err(error) => return Err(refused(error)),
    };
    let Some(directory) =
        subc_os::open_directory_nofollow_at(&parent, OsStr::new(DIRECTORY)).map_err(refused)?
    else {
        return Ok((0, Vec::new()));
    };
    let root = subc_os::retained_directory_path(&directory).map_err(refused)?;
    // Older copies used a single inventory; it is read but never overwritten.
    let (_, mut entries) = read_index(&directory, OsStr::new(INDEX))?;
    let leaves = std::fs::read_dir(&root)
        .map_err(refused)?
        .flatten()
        .collect::<Vec<_>>();
    for leaf in &leaves {
        let name = leaf.file_name();
        if name.to_str().is_some_and(is_folder_index) {
            entries.extend(read_index(&directory, &name)?.1);
        }
    }
    let mut found = BTreeMap::new();
    for entry in entries {
        if !proved_absent(&entry) {
            found.insert(entry.recovery_path.clone(), entry);
        }
    }
    // An archive file is visible even when no index entry recorded its source.
    for entry in leaves {
        let leaf = entry.file_name();
        if leaf == INDEX
            || leaf.to_str().is_some_and(is_folder_index)
            || leaf.to_string_lossy().starts_with(".subc-stage-")
        {
            continue;
        }
        let path = root.join(&leaf);
        if found.contains_key(&path) {
            continue;
        }
        if subc_os::retained_regular_file_at(&directory, &leaf) {
            let at = entry
                .metadata()
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |duration| {
                    duration.as_millis().try_into().unwrap_or(u64::MAX)
                });
            found.insert(
                path.clone(),
                Entry {
                    path: path.clone(),
                    recovery_path: path,
                    at,
                    parent: None,
                },
            );
        }
    }
    let count = found.len() as u64;
    let mut entries = found.into_values().collect::<Vec<_>>();
    entries.sort_unstable_by(|left, right| {
        (right.at, &right.recovery_path).cmp(&(left.at, &left.recovery_path))
    });
    Ok((
        count,
        entries
            .into_iter()
            .take(STATUS_ROWS)
            .map(|entry| ConfigSettingsRecoveryCopy {
                path: entry.path.to_string_lossy().into_owned(),
                recovery_path: entry.recovery_path.to_string_lossy().into_owned(),
            })
            .collect(),
    ))
}

fn is_folder_index(name: &str) -> bool {
    name.strip_prefix(".inventory-")
        .and_then(|name| name.strip_suffix(".json"))
        .is_some_and(|hash| hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

fn proved_absent(entry: &Entry) -> bool {
    let Some(identity) = &entry.parent else {
        return false;
    };
    let Some((parent, leaf)) = entry
        .recovery_path
        .parent()
        .zip(entry.recovery_path.file_name())
    else {
        return false;
    };
    let Ok(parent) = subc_os::open_directory_nofollow_all(parent) else {
        return false;
    };
    // A replacement mount can present the same path with a different parent.
    if file_object_id(&parent).as_ref().ok() != Some(identity) {
        return false;
    }
    matches!(
        subc_os::probe_retained_regular_file_at(&parent, leaf),
        Ok(false)
    )
}

fn refused(error: impl std::fmt::Display) -> GateError {
    GateError::new(
        "write_back_failed",
        format!("settings recovery directory is unavailable: {error}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_pending_settings_inventory_prunes_only_proven_absence() {
        let scratch = cortexkit_test_support::ScratchDir::new("inventory-absence");
        let root = scratch.canonicalize().unwrap();
        let folder = root.join("mount");
        std::fs::create_dir(&folder).unwrap();
        let retained = subc_os::open_directory_nofollow_all(&folder).unwrap();
        let archive = RecoveryArchive::open(
            &root,
            Default::default(),
            subc_os::probe_retained_directory_write,
        )
        .unwrap();
        let inventory = archive.folder(&retained).unwrap();
        let _held = inventory.serialize_updates().unwrap();
        let old = folder.join(".a.jsonc.declined-test");
        let settings = folder.join("a.jsonc");
        inventory.record(&settings, &old, &retained, 1).unwrap();
        std::fs::write(&old, b"retained edit").unwrap();
        // A folder swapped out from under the inventory is not proof that its
        // copy is gone. Only Unix can reach this: the Windows retained handle
        // above is opened without delete sharing, so Windows refuses to rename
        // the folder while it is held.
        #[cfg(unix)]
        let detached = {
            let detached = root.join("detached");
            std::fs::rename(&folder, &detached).unwrap();
            std::fs::create_dir(&folder).unwrap();
            detached
        };
        let archived = root.join(DIRECTORY).join("another-copy");
        inventory
            .record(&settings, &archived, &archive.directory, 2)
            .unwrap();
        assert!(status(&root)
            .unwrap()
            .1
            .iter()
            .any(|copy| copy.recovery_path == old.to_string_lossy()));
        #[cfg(unix)]
        {
            std::fs::remove_dir(&folder).unwrap();
            std::fs::rename(&detached, &folder).unwrap();
        }
        assert_eq!(status(&root).unwrap().0, 1);
        std::fs::remove_file(&old).unwrap();
        assert_eq!(status(&root).unwrap().0, 0);
    }

    #[test]
    fn config_pending_settings_same_leaf_and_timestamp_keep_two_inventory_rows() {
        let scratch = cortexkit_test_support::ScratchDir::new("inventory-root-names");
        let root = scratch.canonicalize().unwrap();
        let archive = RecoveryArchive::open(
            &root,
            Default::default(),
            subc_os::probe_retained_directory_write,
        )
        .unwrap();
        let mut originals = BTreeMap::new();
        for dir in ["a", "b"] {
            let folder = root.join(dir);
            std::fs::create_dir(&folder).unwrap();
            let source = subc_os::open_directory_nofollow_all(&folder).unwrap();
            let inventory = archive.folder(&source).unwrap();
            let _held = inventory.serialize_updates().unwrap();
            let leaf = OsStr::new("same.jsonc");
            let name = super::super::restore::archive_leaf(
                1_700_000_000_000,
                inventory.short_key(),
                "workspace",
                leaf,
            );
            let copy = root.join(DIRECTORY).join(name);
            inventory
                .record(&folder.join(leaf), &copy, &archive.directory, 10)
                .unwrap();
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&copy)
                .unwrap()
                .write_all(dir.as_bytes())
                .unwrap();
            originals.insert(folder.join(leaf), copy);
        }
        let (count, rows) = status(&root).unwrap();
        assert_eq!(count, 2);
        for row in rows {
            assert_eq!(
                originals[Path::new(&row.path)],
                PathBuf::from(row.recovery_path)
            );
        }
    }

    #[test]
    fn config_pending_settings_stalled_folder_lock_does_not_block_another_folder() {
        let scratch = cortexkit_test_support::ScratchDir::new("inventory-root-locks");
        let root = scratch.canonicalize().unwrap();
        let archive = Arc::new(
            RecoveryArchive::open(
                &root,
                Default::default(),
                subc_os::probe_retained_directory_write,
            )
            .unwrap(),
        );
        let a = root.join("a");
        let b = root.join("b");
        for path in [&a, &b] {
            std::fs::create_dir(path).unwrap();
        }
        let a = subc_os::open_directory_nofollow_all(&a).unwrap();
        let a = archive.folder(&a).unwrap();
        let held = a.serialize_updates().unwrap();
        let (done, observed) = std::sync::mpsc::channel();
        let other = archive.clone();
        let copy = root.join(DIRECTORY).join("b-copy");
        let worker = std::thread::spawn(move || {
            let source = subc_os::open_directory_nofollow_all(&b).unwrap();
            let inventory = other.folder(&source).unwrap();
            let _held = inventory.serialize_updates().unwrap();
            inventory
                .record(&b.join("b.jsonc"), &copy, &other.directory, 1)
                .unwrap();
            let _ = done.send(());
        });
        let progress = observed.recv_timeout(std::time::Duration::from_secs(5));
        drop(held);
        worker.join().unwrap();
        assert!(
            progress.is_ok(),
            "B must make progress while A holds a folder lock"
        );
    }

    #[test]
    fn config_pending_settings_disk_inventory_counts_more_than_its_newest_fifty_rows() {
        let scratch = cortexkit_test_support::ScratchDir::new("settings-inventory-window");
        let root = scratch.canonicalize().unwrap();
        let archive = RecoveryArchive::open(
            &root,
            Default::default(),
            subc_os::probe_retained_directory_write,
        )
        .unwrap();
        let directory = subc_os::retained_directory_path(&archive.directory).unwrap();
        let original = root.join("original.jsonc");
        let local = root.join(".original.jsonc.declined-test");
        {
            let source = subc_os::open_directory_nofollow_all(&root).unwrap();
            let inventory = archive.folder(&source).unwrap();
            let _updates = inventory.serialize_updates().unwrap();
            for index in 0..52 {
                let copy = directory.join(format!("copy-{index}"));
                inventory
                    .record(&original, &copy, &archive.directory, index)
                    .unwrap();
                std::fs::write(copy, b"{}\n").unwrap();
            }
            inventory.record(&original, &local, &source, 52).unwrap();
            std::fs::write(&local, b"{}\n").unwrap();
        }
        drop(archive);
        let (count, rows) = status(&root).unwrap();
        assert_eq!(count, 53);
        assert_eq!(rows.len(), 50);
        assert_eq!(rows[0].recovery_path, local.to_string_lossy());
        assert_eq!(
            rows[49].recovery_path,
            directory.join("copy-3").to_string_lossy()
        );
        std::fs::remove_file(&local).unwrap();
        assert_eq!(status(&root).unwrap().0, 52);
    }
}
