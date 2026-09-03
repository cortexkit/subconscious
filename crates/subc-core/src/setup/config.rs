use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
};

use serde_json::{Map, Value};
use subc_jsonc::jsonc_to_json;

use super::model::Component;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigConflict {
    pub key: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigChange {
    pub path: PathBuf,
    pub before: String,
    pub after: String,
}

impl ConfigChange {
    pub fn render_diff(&self) -> String {
        format!(
            "--- {}\n+++ {}\n@@ proposed CortexKit setup change @@\n-{}\n+{}",
            self.path.display(),
            self.path.display(),
            self.before.trim_end(),
            self.after.trim_end()
        )
    }
}

/// Inspect one component's exact leaf values without changing the file. Existing
/// values are never authorization to replace a different user-owned value.
#[allow(dead_code)]
pub fn plan_component(
    path: impl Into<PathBuf>,
    component: Component,
    binary_home: &Path,
) -> Result<Option<ConfigChange>, ConfigConflict> {
    plan_component_with_key(path, component, binary_home, None)
}

/// Plans one component using the same claustrum key path for its generated
/// environment entry that bootstrap receives.
pub fn plan_component_with_key(
    path: impl Into<PathBuf>,
    component: Component,
    binary_home: &Path,
    claustrum_key_path: Option<&Path>,
) -> Result<Option<ConfigChange>, ConfigConflict> {
    let path = path.into();
    let before = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(_) => {
            return Err(ConfigConflict {
                key: path.display().to_string(),
            })
        }
    };
    let mut document = if before.is_empty() {
        Value::Object(Map::new())
    } else {
        let strict = jsonc_to_json(&before).map_err(|_| ConfigConflict {
            key: path.display().to_string(),
        })?;
        serde_json::from_str::<Value>(&strict).map_err(|_| ConfigConflict {
            key: path.display().to_string(),
        })?
    };
    if !document.is_object() {
        return Err(ConfigConflict {
            key: "<root>".to_string(),
        });
    }

    let pending = pending_keys_against(&document, component, binary_home, claustrum_key_path)?;
    if pending.is_empty() {
        return Ok(None);
    }
    let additions = desired_values_with_key(component, binary_home, claustrum_key_path);
    for (key, desired) in additions {
        if pending.iter().any(|pending_key| pending_key == &key) {
            insert_value(&mut document, &key, desired).map_err(|key| ConfigConflict { key })?;
        }
    }
    let mut after = format!(
        "{}\n",
        serde_json::to_string_pretty(&document).expect("JSON values always serialize")
    );
    if component == Component::Claustrum {
        insert_claustrum_reserved_comment(&mut after);
    }
    Ok(Some(ConfigChange {
        path,
        before,
        after,
    }))
}

/// Dotted keys core (or another component) would write. Empty when the file
/// already carries every desired leaf.
pub fn pending_dotted_keys(
    path: impl Into<PathBuf>,
    component: Component,
    binary_home: &Path,
    claustrum_key_path: Option<&Path>,
) -> Result<Vec<String>, ConfigConflict> {
    let path = path.into();
    let before = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(_) => {
            return Err(ConfigConflict {
                key: path.display().to_string(),
            })
        }
    };
    let document = if before.is_empty() {
        Value::Object(Map::new())
    } else {
        let strict = jsonc_to_json(&before).map_err(|_| ConfigConflict {
            key: path.display().to_string(),
        })?;
        serde_json::from_str::<Value>(&strict).map_err(|_| ConfigConflict {
            key: path.display().to_string(),
        })?
    };
    if !document.is_object() {
        return Err(ConfigConflict {
            key: "<root>".to_string(),
        });
    }
    pending_keys_against(&document, component, binary_home, claustrum_key_path)
}

fn pending_keys_against(
    document: &Value,
    component: Component,
    binary_home: &Path,
    claustrum_key_path: Option<&Path>,
) -> Result<Vec<String>, ConfigConflict> {
    let mut pending = Vec::new();
    for (key, desired) in desired_values_with_key(component, binary_home, claustrum_key_path) {
        match existing_value(document, &key) {
            Some(actual) if actual == &desired => {}
            Some(_) => return Err(ConfigConflict { key }),
            None => pending.push(key),
        }
    }
    Ok(pending)
}

/// Sections a live daemon would have to restart to apply, derived locally from
/// the keys core setup would write. No RPC: at dry-run time the new section is
/// not on disk yet, so asking the daemon about the current file always answers
/// empty. A stopped daemon starts later on the new file and needs no restart.
pub fn restart_required_from_pending_keys(
    runtime_live: bool,
    pending_dotted_keys: impl IntoIterator<Item = impl AsRef<str>>,
) -> Vec<String> {
    if !runtime_live {
        return Vec::new();
    }
    let mut sections = Vec::new();
    for key in pending_dotted_keys {
        let top = key.as_ref().split('.').next().unwrap_or("");
        if subc_core::daemon_config::RestartRequiredSection::ALL
            .iter()
            .map(|section| section.label())
            .any(|label| label == top)
            && !sections.iter().any(|section| section == top)
        {
            sections.push(top.to_string());
        }
    }
    sections
}

pub fn apply(change: &ConfigChange) -> Result<(), String> {
    let parent = change.path.parent().ok_or_else(|| {
        format!(
            "configuration path {} has no parent directory",
            change.path.display()
        )
    })?;
    fs::create_dir_all(parent).map_err(|error| {
        format!(
            "could not create configuration directory {}: {error}",
            parent.display()
        )
    })?;
    let temporary = change.path.with_extension("jsonc.tmp");
    fs::write(&temporary, &change.after)
        .map_err(|error| format!("could not write {}: {error}", temporary.display()))?;
    fs::rename(&temporary, &change.path).map_err(|error| {
        format!(
            "could not replace configuration {} with {}: {error}",
            change.path.display(),
            temporary.display()
        )
    })
}

/// Removes only a component's values when setup must unwind a later failure.
/// Matching the generated values prevents rollback from deleting a user edit
/// that occurred after setup wrote the original component entry.
pub fn remove_component(
    path: &Path,
    component: Component,
    binary_home: &Path,
    claustrum_key_path: Option<&Path>,
) -> Result<bool, String> {
    let before = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(format!("could not read {}: {error}", path.display())),
    };
    let strict = jsonc_to_json(&before)
        .map_err(|error| format!("could not parse {} for rollback: {error}", path.display()))?;
    let mut document: Value = serde_json::from_str(&strict)
        .map_err(|error| format!("could not parse {} for rollback: {error}", path.display()))?;
    let Some(object) = document.as_object_mut() else {
        return Err(format!(
            "could not roll back non-object configuration {}",
            path.display()
        ));
    };
    let mut removed = false;
    for (key, desired) in desired_values_with_key(component, binary_home, claustrum_key_path) {
        removed |= remove_exact_value(object, &key, &desired);
    }
    if !removed {
        return Ok(false);
    }
    let change = ConfigChange {
        path: path.to_path_buf(),
        before,
        after: format!(
            "{}\n",
            serde_json::to_string_pretty(&document).expect("JSON values always serialize")
        ),
    };
    apply(&change)?;
    Ok(true)
}

#[allow(dead_code)]
pub fn desired_values(component: Component, binary_home: &Path) -> BTreeMap<String, Value> {
    desired_values_with_key(component, binary_home, None)
}

fn desired_values_with_key(
    component: Component,
    binary_home: &Path,
    claustrum_key_path: Option<&Path>,
) -> BTreeMap<String, Value> {
    let mut values = BTreeMap::new();
    match component {
        Component::Core => {
            values.insert("version".to_string(), Value::from(1));
            // Without a storage section the daemon delivers no storage
            // descriptor in HELLO_ACK. Modules that open their store from the
            // descriptor (claustrum does; it is the honest shape) then refuse
            // to start with "HELLO_ACK carried no storage descriptor", while
            // modules that self-key from the environment run regardless — so
            // the omission passed every drive until the first descriptor-
            // honouring module was installed. `data_home` is left to the
            // daemon's platform default so a config written on one machine
            // carries no other machine's home directory. Keyed on the leaf,
            // not the object: an operator config that already carries an
            // explicit `data_home` matches on the backend and keeps its home,
            // instead of conflicting on a whole-object comparison.
            values.insert(
                "storage.backend".to_string(),
                Value::String("sqlite".to_string()),
            );
        }
        Component::Aft | Component::Mc | Component::Insula | Component::Synapse => {
            let module_id = component.module_id().expect("non-core modules have an id");
            values.insert(
                format!("modules.{module_id}"),
                serde_json::json!({
                    "program": binary_home
                        .join(platform_binary(daemon_binary(component)))
                        .to_string_lossy(),
                }),
            );
        }
        Component::Claustrum => {
            let mut entry = Map::new();
            entry.insert(
                "program".to_string(),
                Value::String(
                    binary_home
                        .join(platform_binary(daemon_binary(component)))
                        .to_string_lossy()
                        .into_owned(),
                ),
            );
            entry.insert("reserved".to_string(), Value::Bool(true));
            if let Some(key_path) = claustrum_key_path {
                entry.insert(
                    "env".to_string(),
                    serde_json::json!({
                        "CK_MASTER_KEY_PATH": key_path.to_string_lossy(),
                    }),
                );
            }
            values.insert("modules.claustrum".to_string(), Value::Object(entry));
        }
    }
    values
}

fn insert_claustrum_reserved_comment(rendered: &mut String) {
    let Some(claustrum) = rendered.find("\"claustrum\": {") else {
        return;
    };
    let Some(reserved) = rendered[claustrum..].find("\"reserved\": true") else {
        return;
    };
    let reserved = claustrum + reserved;
    let line_start = rendered[..reserved].rfind('\n').unwrap_or(0) + 1;
    let indentation = &rendered[line_start..reserved];
    rendered.insert_str(
        line_start,
        &format!(
            "{indentation}// without it any local process completing the handshake can claim the vault's module id and be handed bearer capability handles\n"
        ),
    );
}

/// The binary the daemon spawns for a module, read from the components
/// table rather than restated here: a second copy of the names once
/// pointed the config at `bin/aft` after the archive was corrected to
/// install `ck-aft`, and the daemon failed to spawn a path that no longer
/// existed. The name is target-independent (only the sidecar set varies),
/// so writing a module's configuration never depends on the host — the
/// per-target availability gate is the planner's, upstream of this.
fn daemon_binary(component: Component) -> &'static str {
    super::components::module_program(component)
        .expect("only modules the daemon spawns are written into the configuration")
}

fn platform_binary(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

fn existing_value<'a>(document: &'a Value, dotted_key: &str) -> Option<&'a Value> {
    dotted_key
        .split('.')
        .try_fold(document, |value, key| value.as_object()?.get(key))
}

fn insert_value(document: &mut Value, dotted_key: &str, desired: Value) -> Result<(), String> {
    let mut keys = dotted_key.split('.').peekable();
    let mut current = document
        .as_object_mut()
        .ok_or_else(|| "<root>".to_string())?;
    while let Some(key) = keys.next() {
        if keys.peek().is_none() {
            current.insert(key.to_string(), desired);
            return Ok(());
        }
        let value = current
            .entry(key.to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        current = value.as_object_mut().ok_or_else(|| key.to_string())?;
    }
    unreachable!("a desired configuration key always has a segment")
}

fn remove_exact_value(object: &mut Map<String, Value>, dotted_key: &str, desired: &Value) -> bool {
    let keys = dotted_key.split('.').collect::<Vec<_>>();
    remove_exact_value_at(object, &keys, desired)
}

fn remove_exact_value_at(object: &mut Map<String, Value>, keys: &[&str], desired: &Value) -> bool {
    let Some((key, rest)) = keys.split_first() else {
        return false;
    };
    if rest.is_empty() {
        return object.get(*key).is_some_and(|actual| actual == desired)
            && object.remove(*key).is_some();
    }
    let Some(child) = object.get_mut(*key).and_then(Value::as_object_mut) else {
        return false;
    };
    let removed = remove_exact_value_at(child, rest, desired);
    if removed && child.is_empty() {
        object.remove(*key);
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use subc_core::test_support::TestTempDir;

    fn fixture_path(name: &str) -> TestTempDir {
        TestTempDir::new(name)
    }

    #[test]
    fn rollback_removes_only_the_failed_component_configuration() {
        let root = fixture_path("config-rollback-component");
        let config = root.join("subc.jsonc");
        let aft = plan_component(&config, Component::Aft, &root)
            .expect("aft configuration")
            .expect("aft missing");
        apply(&aft).expect("write aft");
        let claustrum = plan_component(&config, Component::Claustrum, &root)
            .expect("claustrum configuration")
            .expect("claustrum missing");
        apply(&claustrum).expect("write claustrum");

        assert!(remove_component(&config, Component::Claustrum, &root, None)
            .expect("remove failed component"));

        let written: Value = serde_json::from_str(&fs::read_to_string(&config).unwrap()).unwrap();
        assert!(written.pointer("/modules/aft").is_some());
        assert!(written.pointer("/modules/claustrum").is_none());
    }

    #[test]
    fn adding_mc_preserves_the_existing_aft_configuration() {
        let root = fixture_path("config-additive");
        let config = root.join("subc.jsonc");
        fs::write(
            &config,
            r#"{"version":1,"modules":{"aft":{"program":"/user/aft","reserved":true}}}"#,
        )
        .expect("write fixture");
        let change = plan_component(&config, Component::Mc, &root)
            .expect("MC configuration is additive")
            .expect("MC is absent");
        apply(&change).expect("apply additive change");
        let written: Value =
            serde_json::from_str(&fs::read_to_string(&config).expect("read config"))
                .expect("written config parses");
        assert_eq!(
            written.pointer("/modules/aft"),
            Some(&serde_json::json!({"program":"/user/aft","reserved":true}))
        );
        assert_eq!(
            written.pointer("/modules/magic-context/program"),
            Some(&Value::String(
                root.join(platform_binary("ck-mc"))
                    .to_string_lossy()
                    .into_owned()
            ))
        );
    }

    /// Eleventh finding of the macOS operator drive: the archive installed
    /// `ck-aft` (the corrected managed name) while the config pointed the
    /// daemon at `bin/aft`, and the spawn failed on a path that did not
    /// exist. The program the config names must be the first binary the
    /// component installs — the same table, so they cannot disagree.
    #[test]
    fn configured_program_is_the_component_s_first_installed_binary() {
        for component in [
            Component::Aft,
            Component::Mc,
            Component::Insula,
            Component::Claustrum,
            Component::Synapse,
        ] {
            let root = fixture_path(&format!("program-name-{component}"));
            let config = root.join("subc.jsonc");
            let change = plan_component(&config, component, &root)
                .expect("additive")
                .expect("absent");
            apply(&change).expect("apply");
            // The written file is JSONC (claustrum's entry carries a comment
            // explaining `reserved`); parse it the way the daemon does.
            let stripped =
                jsonc_to_json(&fs::read_to_string(&config).expect("read")).expect("jsonc strips");
            let written: Value = serde_json::from_str(&stripped).expect("jsonc parses");
            let module_id = component.module_id().expect("module");
            let program = written
                .pointer(&format!("/modules/{module_id}/program"))
                .and_then(Value::as_str)
                .expect("program written");
            // The program is target-independent: on a target without the
            // component (mc on Windows) the config is still writable, and
            // the planner's availability gate is what refuses the install.
            let expected = root.join(platform_binary(
                super::super::components::module_program(component).expect("module program"),
            ));
            assert_eq!(
                Path::new(program),
                expected.as_path(),
                "{component}: config must spawn the module program"
            );
        }
    }

    #[test]
    fn claustrum_uses_one_key_path_for_the_generated_environment() {
        let root = fixture_path("claustrum-key");
        let config = root.join("subc.jsonc");
        let key_path = root.join("keys/master.key");
        let change = plan_component_with_key(&config, Component::Claustrum, &root, Some(&key_path))
            .expect("claustrum configuration is additive")
            .expect("claustrum entry is absent");
        assert!(change.after.contains("CK_MASTER_KEY_PATH"));
        // Compare the JSON-encoded form: on Windows the path's separators are
        // escaped in the written text, so a raw substring check reads false
        // against a correct file.
        let encoded_key_path = serde_json::to_string(&*key_path.to_string_lossy())
            .expect("path encodes as a JSON string");
        assert!(
            change.after.contains(&encoded_key_path),
            "generated config does not carry the key path {encoded_key_path}: {}",
            change.after
        );
        assert!(change
            .after
            .contains("without it any local process completing the handshake"));
    }

    /// The daemon delivers a storage descriptor in HELLO_ACK only when the
    /// config has a storage section; modules that open their store from the
    /// descriptor refuse to start without one. Core setup must write it, and
    /// must not conflict with an operator's explicit `data_home`.
    #[test]
    fn core_configuration_declares_sqlite_storage_and_keeps_an_explicit_data_home() {
        let root = fixture_path("core-storage");
        let fresh = root.join("fresh.jsonc");
        let change = plan_component(&fresh, Component::Core, root.path())
            .expect("plan")
            .expect("fresh config changes");
        apply(&change).expect("apply");
        let written: Value = serde_json::from_str(
            &jsonc_to_json(&fs::read_to_string(&fresh).expect("read")).expect("jsonc"),
        )
        .expect("json");
        assert_eq!(written["storage"]["backend"], "sqlite");
        assert!(
            written["storage"].get("data_home").is_none(),
            "no host path is written"
        );

        let explicit = root.join("explicit.jsonc");
        fs::write(
            &explicit,
            r#"{"version":1,"storage":{"backend":"sqlite","data_home":"/srv/ck"},"modules":{}}"#,
        )
        .expect("write");
        assert!(
            plan_component(&explicit, Component::Core, root.path())
                .expect("plan")
                .is_none(),
            "an explicit data_home under the same backend is already correct"
        );

        let other = root.join("other.jsonc");
        fs::write(
            &other,
            r#"{"version":1,"storage":{"backend":"postgres"},"modules":{}}"#,
        )
        .expect("write");
        let conflict = plan_component(&other, Component::Core, root.path()).expect_err("conflict");
        assert_eq!(conflict.key, "storage.backend");
    }

    #[test]
    fn conflicting_user_value_refuses_without_writing_a_byte() {
        let root = fixture_path("config-conflict");
        let config = root.join("subc.jsonc");
        let original = "{\n  // user-owned version\n  \"version\": 2\n}\n";
        fs::write(&config, original).expect("write fixture");

        let conflict = plan_component(&config, Component::Core, &root).expect_err("must refuse");
        assert_eq!(conflict.key, "version");
        assert_eq!(
            fs::read_to_string(&config).expect("read unchanged"),
            original
        );
    }

    #[test]
    fn live_host_missing_storage_is_a_restart_required_section() {
        let root = fixture_path("pending-storage-restart");
        let config = root.join("subc.jsonc");
        fs::write(&config, r#"{"version":1}"#).expect("write");
        let keys =
            pending_dotted_keys(&config, Component::Core, root.path(), None).expect("pending");
        assert!(
            keys.iter().any(|key| key == "storage.backend"),
            "core would write storage.backend: {keys:?}"
        );
        assert_eq!(restart_required_from_pending_keys(true, &keys), ["storage"]);
        assert!(
            restart_required_from_pending_keys(false, &keys).is_empty(),
            "a stopped daemon starts on the new file"
        );
    }

    #[test]
    fn live_host_already_carrying_storage_backend_needs_no_restart() {
        let root = fixture_path("has-storage-restart");
        let config = root.join("subc.jsonc");
        fs::write(&config, r#"{"version":1,"storage":{"backend":"sqlite"}}"#).expect("write");
        let keys =
            pending_dotted_keys(&config, Component::Core, root.path(), None).expect("pending");
        assert!(keys.is_empty(), "no core keys remain to write: {keys:?}");
        assert!(restart_required_from_pending_keys(true, &keys).is_empty());
    }
}
