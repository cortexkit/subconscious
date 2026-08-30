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
pub fn plan_component(
    path: impl Into<PathBuf>,
    component: Component,
    binary_home: &Path,
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

    let additions = desired_values(component, binary_home);
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
    let after = format!(
        "{}\n",
        serde_json::to_string_pretty(&document).expect("JSON values always serialize")
    );
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

pub fn desired_values(component: Component, binary_home: &Path) -> BTreeMap<String, Value> {
    let mut values = BTreeMap::new();
    match component {
        Component::Core => {
            values.insert("version".to_string(), Value::from(1));
        }
        Component::Aft => {
            values.insert(
                "modules.aft".to_string(),
                serde_json::json!({
                    "program": binary_home.join(platform_binary("ck-aft")).to_string_lossy(),
                    "reserved": true,
                }),
            );
        }
        // MC has no alpha archive. This declaration wires its selected state
        // without inventing a daemon process or changing another module's entry.
        Component::Mc => {
            values.insert("cortexkit.components.mc".to_string(), Value::Bool(true));
        }
    }
    values
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
    use std::{
        env, process,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;

    fn fixture_path(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        env::temp_dir().join(format!("ck-setup-{name}-{}-{nonce}", process::id()))
    }

    #[test]
    fn adding_mc_preserves_the_existing_aft_configuration() {
        let root = fixture_path("config-additive");
        fs::create_dir_all(&root).expect("fixture directory");
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
            written.pointer("/cortexkit/components/mc"),
            Some(&Value::Bool(true))
        );
        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn conflicting_user_value_refuses_without_writing_a_byte() {
        let root = fixture_path("config-conflict");
        fs::create_dir_all(&root).expect("fixture directory");
        let config = root.join("subc.jsonc");
        let original = "{\n  // user-owned version\n  \"version\": 2\n}\n";
        fs::write(&config, original).expect("write fixture");

        let conflict = plan_component(&config, Component::Core, &root).expect_err("must refuse");
        assert_eq!(conflict.key, "version");
        assert_eq!(
            fs::read_to_string(&config).expect("read unchanged"),
            original
        );
        fs::remove_dir_all(root).expect("remove fixture");
    }
}
