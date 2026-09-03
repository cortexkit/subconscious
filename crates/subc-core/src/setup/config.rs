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

    let additions = desired_values_with_key(component, binary_home, claustrum_key_path);
    let mut changed = false;
    for (key, desired) in additions {
        match existing_value(&document, &key) {
            Some(actual) if actual == &desired => {}
            Some(_) => return Err(ConfigConflict { key }),
            None => {
                insert_value(&mut document, &key, desired).map_err(|key| ConfigConflict { key })?;
                changed = true;
            }
        }
    }
    if !changed {
        return Ok(None);
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
            "could not replace configuration {}: {error}",
            change.path.display()
        )
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use subc_core::test_support::TestTempDir;

    fn fixture_path(name: &str) -> TestTempDir {
        TestTempDir::new(name)
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
}
