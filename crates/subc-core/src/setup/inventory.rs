use std::{
    fs, io,
    path::{Path, PathBuf},
};

use serde_json::{json, Map, Value};

#[cfg(test)]
use super::file_modes;
use super::install_journal::{transaction::Transaction, Recovery};
use super::{components::OWNED_BINARY_KINDS, install_lock::InstallLock};

const SCHEMA_VERSION: u64 = 1;

/// The installer manifest is the sole authority for removal. Its entries retain
/// installer-written fields verbatim while setup appends only its own mutations.
#[derive(Debug)]
pub struct Inventory {
    path: PathBuf,
    document: Value,
    changed: bool,
    windows_separators: bool,
    /// Held from before the load until this inventory is dropped, when it
    /// was loaded by a command that changes the install.
    _lock: Option<InstallLock>,
    /// Loaded without the install lock to look, never to save: a save from
    /// such a copy could write back rows another, locked, command has since
    /// replaced.
    snapshot: bool,
    active: Option<(Transaction, Value)>,
    #[cfg(test)]
    opened_transaction: bool,
}

#[cfg(test)]
#[test]
fn an_unconfirmed_or_failed_undo_cannot_leak_inventory_rows() {
    use super::install_journal::bootstrap::FileRecovery;
    for unconfirmed in [false, true] {
        let root = cortexkit_test_support::ScratchDir::new("journal-inventory-state");
        let manifest = root.join("installer-manifest.json");
        let target = root.join("managed");
        let mut inventory = Inventory::read(manifest.clone(), "test").unwrap();
        inventory.start_transaction().unwrap();
        inventory
            .transaction()
            .unwrap()
            .write(&target, b"new", 0o600)
            .unwrap();
        inventory.record("managed-binary", &target, Map::new());
        if unconfirmed {
            inventory.leave_transaction_unconfirmed().unwrap();
        } else {
            fs::write(&target, "unexpected bytes").unwrap();
            assert!(inventory.undo_transaction(&mut FileRecovery).is_err());
        }
        assert!(!inventory.owns_path("managed-binary", &target));
        assert!(!inventory.has_changes());
        assert!(inventory.save().unwrap_err().contains("journal commit"));
        assert!(!manifest.exists());
    }
}

impl Inventory {
    /// Loads the inventory for a command that changes the install: first
    /// waits up to `wait` for any other installer command on this install
    /// root to finish (see [`InstallLock`]), then reads the manifest it left,
    /// and keeps the lock until this inventory is dropped.
    pub fn load_locked(
        path: impl Into<PathBuf>,
        platform: &str,
        command: &str,
        wait: std::time::Duration,
    ) -> Result<Self, String> {
        let path = path.into();
        let lock = InstallLock::acquire_within(&path, command, wait)?;
        let mut inventory = Self::read(path, platform)?;
        inventory._lock = Some(lock);
        super::install_journal::bootstrap::refuse_legacy(&inventory.path, &inventory)?;
        Ok(inventory)
    }

    /// A read-only snapshot for discovery, previews and the dashboard.
    pub fn load_snapshot(path: impl Into<PathBuf>, platform: &str) -> Result<Self, String> {
        let mut inventory = Self::read(path.into(), platform)?;
        inventory.snapshot = true;
        Ok(inventory)
    }

    /// A writable load without the lock, for tests that stand in for one
    /// installer command; production writers use [`load_locked`](Self::load_locked).
    #[cfg(test)]
    pub fn load(path: impl Into<PathBuf>, platform: &str) -> Result<Self, String> {
        Self::read(path.into(), platform)
    }

    pub(super) fn read(path: PathBuf, platform: &str) -> Result<Self, String> {
        let document = match fs::read_to_string(&path) {
            Ok(contents) => serde_json::from_str(&contents).map_err(|error| {
                format!("inventory {} is invalid JSON: {error}", path.display())
            })?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => json!({
                "schema_version": SCHEMA_VERSION,
                "platform": platform,
                "mutations": [],
            }),
            Err(error) => {
                return Err(format!(
                    "could not read inventory {}: {error}",
                    path.display()
                ))
            }
        };
        if !document.is_object() {
            return Err(format!(
                "inventory {} must be a JSON object",
                path.display()
            ));
        }
        if !document.get("mutations").is_some_and(Value::is_array) {
            return Err(format!(
                "inventory {} has no mutations array",
                path.display()
            ));
        }
        let inventory = Self {
            path,
            document,
            changed: false,
            windows_separators: matches!(platform, "windows-x64" | "windows-arm64"),
            _lock: None,
            snapshot: false,
            active: None,
            #[cfg(test)]
            opened_transaction: false,
        };
        Ok(inventory)
    }

    pub fn owns_path(&self, kind: &str, path: &Path) -> bool {
        self.entry_for_path(kind, path).is_some()
    }

    pub fn paths_for_kind(&self, kind: &str) -> Vec<PathBuf> {
        self.mutations()
            .filter(|entry| entry.get("kind").and_then(Value::as_str) == Some(kind))
            .filter_map(|entry| entry.get("path").and_then(Value::as_str))
            .map(PathBuf::from)
            .collect()
    }

    pub fn entry_for_path(&self, kind: &str, path: &Path) -> Option<&Value> {
        self.mutations().find(|entry| {
            entry.get("kind").and_then(Value::as_str) == Some(kind)
                && entry_has_path(entry, &path.to_string_lossy(), self.windows_separators)
        })
    }

    /// Records a row for `path`. Among the binary-ownership kinds a path has
    /// ONE owner: recording setup's `managed-binary` over the bootstrap
    /// installer's `binary-placement` (or the reverse) transfers ownership
    /// rather than adding a sibling, because both rows answer the same
    /// question — who placed these bytes — and disagree the moment one
    /// writer replaces the file: the other row keeps a stale digest and
    /// version, and whichever reader finds it first believes it. Kinds that
    /// answer different questions about one path (`runtime-definition` and
    /// `runtime-registration` on the unit file) coexist. Re-recording under
    /// the same kind is a no-op so callers passing empty fields do not erase
    /// digests written by `update_owned_string`.
    pub fn record(&mut self, kind: &str, path: &Path, fields: Map<String, Value>) {
        if self.owns_path(kind, path) {
            return;
        }
        self.transfer_binary_ownership(kind, path);
        let mut entry = fields;
        entry.insert("kind".to_string(), Value::String(kind.to_string()));
        entry.insert(
            "path".to_string(),
            Value::String(path.to_string_lossy().into_owned()),
        );
        self.mutations_mut().push(Value::Object(entry));
        self.changed = true;
    }

    /// When `kind` is a binary-ownership kind, drops the rows the OTHER
    /// binary-ownership kinds hold for `path`. See `record`.
    fn transfer_binary_ownership(&mut self, kind: &str, path: &Path) {
        if !OWNED_BINARY_KINDS.contains(&kind) {
            return;
        }
        for other in OWNED_BINARY_KINDS
            .into_iter()
            .filter(|other| *other != kind)
        {
            self.remove_owned_path(other, path);
        }
    }

    pub fn remove_owned_path(&mut self, kind: &str, path: &Path) {
        let path = path.to_string_lossy();
        let windows_separators = self.windows_separators;
        let mutations = self.mutations_mut();
        let before = mutations.len();
        mutations.retain(|entry| {
            !(entry.get("kind").and_then(Value::as_str) == Some(kind)
                && entry_has_path(entry, path.as_ref(), windows_separators))
        });
        self.changed |= mutations.len() != before;
    }

    /// Keep the ownership digest aligned with a managed replacement. Without
    /// this update, uninstall would correctly see a changed file but would
    /// mistake the upgrade's own replacement for a user modification.
    pub fn update_owned_string(
        &mut self,
        kind: &str,
        path: &Path,
        key: &str,
        value: String,
    ) -> Result<(), String> {
        let path_text = path.to_string_lossy();
        let windows_separators = self.windows_separators;
        let entry = self.mutations_mut().iter_mut().find(|entry| {
            entry.get("kind").and_then(Value::as_str) == Some(kind)
                && entry_has_path(entry, path_text.as_ref(), windows_separators)
        });
        let Some(entry) = entry else {
            return Err(format!(
                "inventory has no {kind} entry for managed path {}",
                path.display()
            ));
        };
        let object = entry
            .as_object_mut()
            .expect("inventory mutation entries are objects written by record");
        object.insert(key.to_string(), Value::String(value));
        self.changed = true;
        Ok(())
    }

    /// Drop a field from an owned entry. Used when a rollback restores a row
    /// that never recorded an archive digest.
    pub fn remove_owned_string(
        &mut self,
        kind: &str,
        path: &Path,
        key: &str,
    ) -> Result<(), String> {
        let path_text = path.to_string_lossy();
        let windows_separators = self.windows_separators;
        let entry = self.mutations_mut().iter_mut().find(|entry| {
            entry.get("kind").and_then(Value::as_str) == Some(kind)
                && entry_has_path(entry, path_text.as_ref(), windows_separators)
        });
        let Some(entry) = entry else {
            return Err(format!(
                "inventory has no {kind} entry for managed path {}",
                path.display()
            ));
        };
        let object = entry
            .as_object_mut()
            .expect("inventory mutation entries are objects written by record");
        if object.remove(key).is_some() {
            self.changed = true;
        }
        Ok(())
    }

    #[cfg(test)]
    pub fn save(&mut self) -> Result<(), String> {
        if self.opened_transaction {
            return Err(format!(
                "refusal: {} must be saved at journal commit after opening an install transaction",
                self.path.display()
            ));
        }
        if !self.changed {
            return Ok(());
        }
        if self.snapshot {
            return Err(format!(
                "refusal: inventory {} was read without the install lock and cannot be saved",
                self.path.display()
            ));
        }
        let parent = self
            .path
            .parent()
            .ok_or_else(|| format!("inventory {} has no parent directory", self.path.display()))?;
        file_modes::ensure_owned_directory(parent, 0o700)?;
        // Sorted keys, so the file's bytes don't depend on whether the build
        // turned on serde_json's `preserve_order`.
        let serialized = self.serialized();
        file_modes::write_atomic(&self.path, serialized.as_bytes(), 0o644, 0o644, "json.tmp")?;
        self.changed = false;
        Ok(())
    }

    pub(super) fn serialized(&self) -> String {
        format!("{}\n", subc_jsonc::to_string_pretty_sorted(&self.document))
    }

    pub(super) fn has_changes(&self) -> bool {
        self.changed
    }

    pub(super) fn manifest(&self) -> &Path {
        &self.path
    }

    pub(super) fn start_transaction(&mut self) -> Result<(), String> {
        if self.snapshot || self.active.is_some() {
            return Err(format!(
                "refusal: cannot start a new install transaction for {}",
                self.path.display()
            ));
        }
        self.active = Some((Transaction::begin(&self.path)?, self.document.clone()));
        #[cfg(test)]
        {
            self.opened_transaction = true;
        }
        Ok(())
    }

    pub(super) fn transaction(&mut self) -> Option<&mut Transaction> {
        self.active.as_mut().map(|(transaction, _)| transaction)
    }

    pub(super) fn transaction_is_open(&self) -> bool {
        self.active.is_some()
    }

    pub(super) fn release_lock(&mut self) {
        self._lock.take();
    }

    pub(super) fn suspend_transaction(&mut self) -> Option<Value> {
        self.active.take().map(|(_, prior)| prior)
    }

    pub(super) fn resume_transaction(&mut self, prior: Value) -> Result<(), String> {
        self.active = Some((Transaction::resume(&self.path)?, prior));
        Ok(())
    }

    pub(super) fn commit_transaction(
        &mut self,
        recovery: &mut impl Recovery,
    ) -> Result<(), String> {
        if let Some((transaction, _)) = self.active.take() {
            transaction.commit(self, recovery)?;
            self.changed = false;
        }
        Ok(())
    }

    pub(super) fn undo_transaction(&mut self, recovery: &mut impl Recovery) -> Result<(), String> {
        if let Some((transaction, prior)) = self.active.take() {
            self.document = prior;
            self.changed = false;
            transaction.undo(recovery)?;
        }
        Ok(())
    }

    pub(super) fn leave_transaction_unconfirmed(&mut self) -> Result<(), String> {
        if let Some((transaction, prior)) = self.active.take() {
            let result = transaction.leave_unconfirmed(self);
            self.document = prior;
            self.changed = false;
            result?;
        }
        Ok(())
    }

    fn mutations(&self) -> impl Iterator<Item = &Value> {
        self.document
            .get("mutations")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
    }

    fn mutations_mut(&mut self) -> &mut Vec<Value> {
        self.document
            .as_object_mut()
            .expect("validated JSON object")
            .entry("mutations")
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .expect("validated mutations array")
    }
}

/// Ordinary Windows paths accept either separator. Do not rewrite recorded
/// strings, fold case, resolve symlinks or normalize verbatim/device paths.
fn entry_has_path(entry: &Value, path: &str, windows_separators: bool) -> bool {
    let Some(recorded) = entry.get("path").and_then(Value::as_str) else {
        return false;
    };
    let exact_prefix = |path: &str| path.starts_with(r"\\?\") || path.starts_with(r"\\.\");
    if !windows_separators || exact_prefix(recorded) || exact_prefix(path) {
        return recorded == path;
    }
    let separator = |byte| if byte == b'/' { b'\\' } else { byte };
    recorded
        .bytes()
        .map(separator)
        .eq(path.bytes().map(separator))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cortexkit_test_support::ScratchDir;

    fn fixture_path(name: &str) -> ScratchDir {
        ScratchDir::new(name)
    }

    #[test]
    fn windows_mixed_separator_paths_find_the_same_owned_entry() {
        let root = fixture_path("inventory-windows-separators");
        let recorded = Path::new(r"C:\Users\alex\.local/share/cortexkit\bin\ck-subc.exe");
        let scanned = Path::new(r"C:\Users\alex\.local/share\cortexkit\bin\ck-subc.exe");
        assert_ne!(recorded.to_string_lossy(), scanned.to_string_lossy());
        for platform in ["windows-x64", "windows-arm64"] {
            let mut inventory =
                Inventory::load(root.join(format!("{platform}.json")), platform).unwrap();
            inventory.record("managed-binary", recorded, Map::new());
            assert!(
                inventory.owns_path("managed-binary", scanned),
                "{platform}: recorded={recorded:?}, scanned={scanned:?}"
            );
            assert!(inventory
                .entry_for_path("managed-binary", scanned)
                .is_some());
            inventory.record("managed-binary", scanned, Map::new());
            let paths = inventory.paths_for_kind("managed-binary");
            assert_eq!(paths.len(), 1);
            assert_eq!(paths[0].to_string_lossy(), recorded.to_string_lossy());
            inventory
                .update_owned_string(
                    "managed-binary",
                    scanned,
                    "archive_sha256",
                    "verified".into(),
                )
                .unwrap();
            assert_eq!(
                inventory
                    .entry_for_path("managed-binary", recorded)
                    .unwrap()["archive_sha256"],
                "verified"
            );
            inventory
                .remove_owned_string("managed-binary", scanned, "archive_sha256")
                .unwrap();
            assert!(inventory
                .entry_for_path("managed-binary", recorded)
                .unwrap()
                .get("archive_sha256")
                .is_none());
            inventory.record("binary-placement", scanned, Map::new());
            assert!(!inventory.owns_path("managed-binary", recorded));
            assert!(inventory.owns_path("binary-placement", recorded));
            inventory.remove_owned_path("binary-placement", recorded);
            assert!(inventory.paths_for_kind("binary-placement").is_empty());
        }
    }

    #[test]
    fn separator_matching_does_not_grant_other_paths_ownership() {
        let root = fixture_path("inventory-path-boundaries");
        let recorded = Path::new(r"C:\Users\alex\.local/share/cortexkit\bin\ck-subc.exe");
        for platform in ["windows-x64", "windows-arm64"] {
            let mut inventory =
                Inventory::load(root.join(format!("{platform}.json")), platform).unwrap();
            inventory.record("managed-binary", recorded, Map::new());
            for other in [
                r"C:\Users\alex\.local/share\cortexkit\bin\ck-subc",
                r"C:\Users\alex\.local/share\cortexkit\bin\CK-SUBC.exe",
                r"C:\Users\alex\.local/share\other\bin\ck-subc.exe",
                r"C:\Users\alex\.local/share\cortexkit\bin\ck-auth.exe",
                r"C:\Users\alex\.local/share\cortexkit\bin\..\bin\ck-subc.exe",
            ] {
                assert!(
                    !inventory.owns_path("managed-binary", Path::new(other)),
                    "{other}"
                );
            }
            assert!(!inventory.owns_path("binary-placement", recorded));
            let verbatim = Path::new(r"\\?\C:\Users\alex\bin\ck.exe");
            inventory.record("managed-binary", verbatim, Map::new());
            assert!(inventory.owns_path("managed-binary", verbatim));
            assert!(
                !inventory.owns_path("managed-binary", Path::new(r"\\?\C:\Users\alex/bin/ck.exe"))
            );
        }
        for platform in ["linux-x64", "darwin-arm64"] {
            let mut inventory =
                Inventory::load(root.join(format!("{platform}.json")), platform).unwrap();
            inventory.record("managed-binary", Path::new(r"bin\ck-subc"), Map::new());
            assert!(inventory.owns_path("managed-binary", Path::new(r"bin\ck-subc")));
            assert!(!inventory.owns_path("managed-binary", Path::new("bin/ck-subc")));
        }
    }

    #[test]
    fn replacement_digest_updates_an_existing_managed_entry() {
        let root = fixture_path("replacement-digest");
        let manifest = root.join("installer-manifest.json");
        let binary = root.join("ck-aft");
        let mut inventory = Inventory::load(&manifest, "linux-x64").expect("load inventory");
        let mut fields = Map::new();
        fields.insert("sha256".to_string(), Value::String("before".to_string()));
        inventory.record("managed-binary", &binary, fields);
        inventory
            .update_owned_string("managed-binary", &binary, "sha256", "after".to_string())
            .expect("update digest");
        inventory.save().expect("save inventory");

        let reloaded = Inventory::load(&manifest, "linux-x64").expect("reload inventory");
        assert_eq!(
            reloaded
                .entry_for_path("managed-binary", &binary)
                .and_then(|entry| entry.get("sha256"))
                .and_then(Value::as_str),
            Some("after")
        );
    }

    // Same rule for a live writer: setup recording a path the bootstrap
    // installer owns takes the row over instead of adding a second one.
    #[test]
    fn recording_a_path_under_another_kind_transfers_ownership() {
        let root = fixture_path("record-transfers-ownership");
        let manifest = root.join("installer-manifest.json");
        let mut inventory = Inventory::load(&manifest, "linux-x64").expect("fresh");
        let ck = Path::new("/managed/ck");
        let mut fields = Map::new();
        fields.insert("archive_sha256".to_string(), Value::String("boot".into()));
        inventory.record("binary-placement", ck, fields);
        let mut fields = Map::new();
        fields.insert("archive_sha256".to_string(), Value::String("setup".into()));
        inventory.record("managed-binary", ck, fields);

        let rows_for_ck = inventory
            .mutations()
            .filter(|entry| entry.get("path").and_then(Value::as_str) == Some("/managed/ck"))
            .count();
        assert_eq!(rows_for_ck, 1);
        assert!(!inventory.owns_path("binary-placement", ck));
        assert_eq!(
            inventory
                .entry_for_path("managed-binary", ck)
                .and_then(|entry| entry.get("archive_sha256"))
                .and_then(Value::as_str),
            Some("setup")
        );

        // Kinds that answer different questions about one path still coexist.
        let unit = Path::new("/units/cortexkit-subc.service");
        inventory.record("runtime-definition", unit, Map::new());
        inventory.record("runtime-registration", unit, Map::new());
        assert!(inventory.owns_path("runtime-definition", unit));
        assert!(inventory.owns_path("runtime-registration", unit));
    }

    #[test]
    fn setup_entries_extend_installer_inventory_without_losing_installer_ownership() {
        let root = fixture_path("inventory");
        let manifest = root.join("installer-manifest.json");
        fs::write(
            &manifest,
            r#"{"schema_version":1,"mutations":[{"kind":"binary-placement","path":"/managed/ck","sha256":"abc"}]}"#,
        )
        .expect("installer inventory");

        let runtime = root.join("cortexkit-subc.service");
        let mut inventory = Inventory::load(&manifest, "linux-x64").expect("load inventory");
        inventory.record("runtime-definition", &runtime, Map::new());
        inventory.save().expect("save inventory");

        let inventory = Inventory::load(&manifest, "linux-x64").expect("reload inventory");
        assert!(inventory.owns_path("binary-placement", Path::new("/managed/ck")));
        assert!(inventory.owns_path("runtime-definition", &runtime));
    }

    /// A snapshot writes nothing and cannot save over a later locked writer.
    #[test]
    fn a_snapshot_read_writes_nothing_and_cannot_overwrite_a_locked_writer() {
        let root = fixture_path("snapshot-read");
        let manifest = root.join("installer-manifest.json");
        let mut seeded = Inventory::load(&manifest, "linux-x64").unwrap();
        seeded.record("managed-binary", Path::new("/managed/ck-subc"), Map::new());
        seeded.save().unwrap();
        let before = fs::read(&manifest).unwrap();

        let mut snapshot = Inventory::load_snapshot(&manifest, "linux-x64").unwrap();

        assert_eq!(
            fs::read(&manifest).unwrap(),
            before,
            "a snapshot wrote the manifest"
        );

        let mut locked = Inventory::load_locked(
            &manifest,
            "linux-x64",
            "ck setup",
            std::time::Duration::ZERO,
        )
        .unwrap();
        locked.record(
            "managed-binary",
            Path::new("/managed/ck-subc-mcp"),
            Map::new(),
        );
        locked.save().unwrap();
        drop(locked);

        snapshot.record("managed-binary", Path::new("/managed/stale"), Map::new());
        let error = snapshot.save().expect_err("a snapshot cannot save");
        assert!(error.contains("without the install lock"), "{error}");
        let reloaded = Inventory::load(&manifest, "linux-x64").unwrap();
        for path in ["/managed/ck-subc", "/managed/ck-subc-mcp"] {
            assert!(
                reloaded.owns_path("managed-binary", Path::new(path)),
                "missing {path}"
            );
        }
        assert!(!reloaded.owns_path("managed-binary", Path::new("/managed/stale")));
    }
}
