use std::{
    fs, io,
    path::{Path, PathBuf},
};

use serde_json::{json, Map, Value};

const SCHEMA_VERSION: u64 = 1;

/// The installer manifest is the sole authority for removal. Its entries retain
/// installer-written fields verbatim while setup appends only its own mutations.
#[derive(Debug)]
pub struct Inventory {
    path: PathBuf,
    document: Value,
    changed: bool,
}

impl Inventory {
    pub fn load(path: impl Into<PathBuf>, platform: &str) -> Result<Self, String> {
        let path = path.into();
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
        Ok(Self {
            path,
            document,
            changed: false,
        })
    }

    pub fn owns_path(&self, kind: &str, path: &Path) -> bool {
        self.mutations().any(|entry| {
            entry.get("kind").and_then(Value::as_str) == Some(kind)
                && entry.get("path").and_then(Value::as_str) == Some(&path.to_string_lossy())
        })
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
                && entry.get("path").and_then(Value::as_str) == Some(&path.to_string_lossy())
        })
    }

    pub fn record(&mut self, kind: &str, path: &Path, fields: Map<String, Value>) {
        if self.owns_path(kind, path) {
            return;
        }
        let mut entry = fields;
        entry.insert("kind".to_string(), Value::String(kind.to_string()));
        entry.insert(
            "path".to_string(),
            Value::String(path.to_string_lossy().into_owned()),
        );
        self.mutations_mut().push(Value::Object(entry));
        self.changed = true;
    }

    pub fn remove_owned_path(&mut self, kind: &str, path: &Path) {
        let path = path.to_string_lossy();
        let mutations = self.mutations_mut();
        let before = mutations.len();
        mutations.retain(|entry| {
            !(entry.get("kind").and_then(Value::as_str) == Some(kind)
                && entry.get("path").and_then(Value::as_str) == Some(path.as_ref()))
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
        let entry = self.mutations_mut().iter_mut().find(|entry| {
            entry.get("kind").and_then(Value::as_str) == Some(kind)
                && entry.get("path").and_then(Value::as_str) == Some(path_text.as_ref())
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
        let entry = self.mutations_mut().iter_mut().find(|entry| {
            entry.get("kind").and_then(Value::as_str) == Some(kind)
                && entry.get("path").and_then(Value::as_str) == Some(path_text.as_ref())
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

    pub fn save(&mut self) -> Result<(), String> {
        if !self.changed {
            return Ok(());
        }
        let parent = self
            .path
            .parent()
            .ok_or_else(|| format!("inventory {} has no parent directory", self.path.display()))?;
        fs::create_dir_all(parent).map_err(|error| {
            format!(
                "could not create inventory directory {}: {error}",
                parent.display()
            )
        })?;
        let serialized = serde_json::to_string_pretty(&self.document).map_err(|error| {
            format!(
                "could not serialize inventory {}: {error}",
                self.path.display()
            )
        })?;
        let temporary = self.path.with_extension("json.tmp");
        fs::write(&temporary, format!("{serialized}\n")).map_err(|error| {
            format!("could not write inventory {}: {error}", temporary.display())
        })?;
        fs::rename(&temporary, &self.path).map_err(|error| {
            format!(
                "could not replace inventory {} with {}: {error}",
                self.path.display(),
                temporary.display()
            )
        })?;
        self.changed = false;
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

#[cfg(test)]
mod tests {
    use super::*;
    use subc_core::test_support::TestTempDir;

    fn fixture_path(name: &str) -> TestTempDir {
        TestTempDir::new(name)
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
}
