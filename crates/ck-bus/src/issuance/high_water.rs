//! `epoch_high_water.json`: the highest credential epoch ck-bus has issued for each
//! (module, spawn generation).
//!
//! The entry for a generation is advanced and fsynced BEFORE the user JWT for that epoch
//! is signed, so it bounds every epoch ever issued: a restarted ck-bus that lost its
//! memory still issues strictly above anything a previous process signed. The census
//! cannot stand in for it, because a revoked epoch's census entry is gone.
//!
//! Shape: one JSON object whose keys are `<module_id>.g<generation>` and whose values
//! are `{"epoch": <u64>}`. Keeping one entry per generation is what lets damage fail
//! closed per generation: an entry that does not read as an epoch refuses further
//! epochs for that generation only, and is kept verbatim when a later generation's entry
//! is added. A file that does not parse at all has no readable entry, so every
//! generation is refused and the file is never rewritten (nothing could be preserved).
//!
//! Every write is an atomic durable replacement: a sibling `*.tmp`, fsynced, renamed over
//! the file, the directory fsynced. Entries for exited generations are not pruned here.

use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::Mutex,
};

use serde_json::{json, Map, Value};

pub const HIGH_WATER_FILE: &str = "epoch_high_water.json";

/// Why no epoch can be issued for a generation. The file is left as it is in every case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HighWaterRefusal {
    /// The whole file is unreadable or not a JSON object.
    FileDamaged { path: PathBuf, reason: String },
    /// This generation's entry does not read as an epoch.
    EntryDamaged {
        path: PathBuf,
        key: String,
        reason: String,
    },
    /// The advanced entry could not be made durable, so nothing may be signed for it.
    WriteFailed { path: PathBuf, error: String },
}

impl HighWaterRefusal {
    pub fn path(&self) -> &Path {
        match self {
            Self::FileDamaged { path, .. }
            | Self::EntryDamaged { path, .. }
            | Self::WriteFailed { path, .. } => path,
        }
    }

    /// Damage needs the operator; a failed write may clear by itself.
    pub fn is_damage(&self) -> bool {
        !matches!(self, Self::WriteFailed { .. })
    }
}

impl std::fmt::Display for HighWaterRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FileDamaged { path, reason } => write!(
                f,
                "{} is damaged ({reason}); no epoch is issued for any generation until the \
                 operator repairs it, and the file is left as it is",
                path.display()
            ),
            Self::EntryDamaged { path, key, reason } => write!(
                f,
                "{} entry {key} is damaged ({reason}); no further epoch is issued for that \
                 generation, and the file is left as it is",
                path.display()
            ),
            Self::WriteFailed { path, error } => {
                write!(f, "{} could not be written: {error}", path.display())
            }
        }
    }
}

/// The file, with one lock so concurrent issues for different modules never lose each
/// other's entries in a read-modify-write.
#[derive(Debug)]
pub struct HighWater {
    path: PathBuf,
    lock: Mutex<()>,
}

impl HighWater {
    pub fn new(store_root: &Path) -> Self {
        Self {
            path: store_root.join(HIGH_WATER_FILE),
            lock: Mutex::new(()),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The entry key for one generation of one module.
    pub fn key(module_id: &str, generation: u64) -> String {
        format!("{module_id}.g{generation}")
    }

    /// Removes a stale `*.tmp` left by a write that died before its rename. It was never
    /// the file of record.
    pub fn remove_stale_tmp(&self) {
        let _ = fs::remove_file(tmp_path(&self.path));
    }

    /// The highest epoch recorded for a generation, `None` when it has no entry.
    pub fn read(&self, module_id: &str, generation: u64) -> Result<Option<u64>, HighWaterRefusal> {
        let _guard = self
            .lock
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let entries = self.load()?;
        self.epoch_of(&entries, &Self::key(module_id, generation))
    }

    /// Picks the next epoch for a generation (0 for a generation with no entry, one above
    /// the recorded epoch otherwise), records it and fsyncs it. Only after this returns
    /// may a JWT at that epoch be signed.
    pub fn advance(&self, module_id: &str, generation: u64) -> Result<u64, HighWaterRefusal> {
        let _guard = self
            .lock
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let mut entries = self.load()?;
        let key = Self::key(module_id, generation);
        let next = match self.epoch_of(&entries, &key)? {
            None => 0,
            Some(epoch) => epoch
                .checked_add(1)
                .ok_or_else(|| HighWaterRefusal::EntryDamaged {
                    path: self.path.clone(),
                    key: key.clone(),
                    reason: "the recorded epoch is the largest representable".to_string(),
                })?,
        };
        // Every other entry, a damaged one included, is carried over byte-for-byte as
        // parsed, so adding a generation never repairs or drops another's record.
        entries.insert(key, json!({ "epoch": next }));
        write_atomic(&self.path, &Value::Object(entries)).map_err(|error| {
            HighWaterRefusal::WriteFailed {
                path: self.path.clone(),
                error: error.to_string(),
            }
        })?;
        Ok(next)
    }

    fn load(&self) -> Result<Map<String, Value>, HighWaterRefusal> {
        let damaged = |reason: String| HighWaterRefusal::FileDamaged {
            path: self.path.clone(),
            reason,
        };
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Map::new()),
            Err(error) => return Err(damaged(format!("unreadable: {error}"))),
        };
        match serde_json::from_slice::<Value>(&bytes) {
            Ok(Value::Object(entries)) => Ok(entries),
            Ok(_) => Err(damaged("not a JSON object".to_string())),
            Err(error) => Err(damaged(format!("not JSON: {error}"))),
        }
    }

    fn epoch_of(
        &self,
        entries: &Map<String, Value>,
        key: &str,
    ) -> Result<Option<u64>, HighWaterRefusal> {
        let Some(entry) = entries.get(key) else {
            return Ok(None);
        };
        entry
            .get("epoch")
            .and_then(Value::as_u64)
            .map(Some)
            .ok_or_else(|| HighWaterRefusal::EntryDamaged {
                path: self.path.clone(),
                key: key.to_string(),
                reason: format!("{entry} is not {{\"epoch\": <unsigned integer>}}"),
            })
    }
}

fn tmp_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!("{name}.tmp"))
}

fn write_atomic(path: &Path, value: &Value) -> io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::other("store file has no parent directory"))?;
    fs::create_dir_all(dir)?;
    let tmp = tmp_path(path);
    {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?;
        file.write_all(&serde_json::to_vec_pretty(value).map_err(io::Error::other)?)?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    sync_dir(dir)
}

#[cfg(unix)]
fn sync_dir(dir: &Path) -> io::Result<()> {
    fs::File::open(dir)?.sync_all()
}

/// Windows cannot open a directory as a file to flush it; the rename is the durable step
/// there.
#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, HighWater) {
        let dir = tempfile::tempdir().expect("temp dir");
        let high_water = HighWater::new(dir.path());
        (dir, high_water)
    }

    #[test]
    fn a_new_generation_starts_at_epoch_zero_and_each_advance_is_durable_and_higher() {
        let (_dir, high_water) = store();
        assert_eq!(high_water.read("participant", 1).unwrap(), None);
        assert_eq!(high_water.advance("participant", 1).unwrap(), 0);
        assert_eq!(high_water.advance("participant", 1).unwrap(), 1);
        // A fresh handle reads what the first one wrote: the value is on disk.
        let reread = HighWater::new(high_water.path().parent().unwrap());
        assert_eq!(reread.read("participant", 1).unwrap(), Some(1));
        assert_eq!(reread.advance("participant", 2).unwrap(), 0);
        assert!(!tmp_path(high_water.path()).exists());
    }

    #[test]
    fn a_damaged_entry_refuses_its_generation_only_and_is_kept_verbatim() {
        let (_dir, high_water) = store();
        fs::write(
            high_water.path(),
            r#"{"participant.g1": {"epoch": "seven"}, "other.g4": {"epoch": 2}}"#,
        )
        .unwrap();
        let before = fs::read(high_water.path()).unwrap();
        let refusal = high_water.advance("participant", 1).unwrap_err();
        assert!(
            matches!(refusal, HighWaterRefusal::EntryDamaged { ref key, .. } if key == "participant.g1")
        );
        assert_eq!(fs::read(high_water.path()).unwrap(), before);

        assert_eq!(high_water.advance("participant", 2).unwrap(), 0);
        let after: Value = serde_json::from_slice(&fs::read(high_water.path()).unwrap()).unwrap();
        assert_eq!(after["participant.g1"], json!({"epoch": "seven"}));
        assert_eq!(after["other.g4"], json!({"epoch": 2}));
        assert_eq!(after["participant.g2"], json!({"epoch": 0}));
    }

    #[test]
    fn an_unparsable_file_refuses_every_generation_and_is_never_rewritten() {
        let (_dir, high_water) = store();
        fs::write(high_water.path(), b"{\"participant.g1\": {\"epo").unwrap();
        for generation in [1, 2] {
            let refusal = high_water.advance("participant", generation).unwrap_err();
            assert!(matches!(refusal, HighWaterRefusal::FileDamaged { .. }));
        }
        assert_eq!(
            fs::read(high_water.path()).unwrap(),
            b"{\"participant.g1\": {\"epo"
        );
    }
}
