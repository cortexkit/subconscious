use std::{
    fs,
    path::{Path, PathBuf},
};

use super::{
    components::digest_file,
    inventory::Inventory,
    runtime::{self, CommandRunner, RuntimePaths, RuntimePlatform},
};

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct UninstallReport {
    pub removed: Vec<PathBuf>,
    pub retained: Vec<String>,
}

/// Remove only paths named by the ownership inventory. Configuration and stores
/// are deliberately retention-only categories, even when setup created them.
pub fn uninstall<R: CommandRunner>(
    platform: RuntimePlatform,
    runtime_paths: &RuntimePaths,
    runner: &mut R,
    inventory: &mut Inventory,
    config_path: &Path,
    store_paths: &[PathBuf],
) -> Result<UninstallReport, String> {
    let mut report = UninstallReport::default();
    if inventory.owns_path("runtime-registration", &runtime_paths.definition) {
        runtime::deregister(platform, runtime_paths, runner)?;
        inventory.remove_owned_path("runtime-registration", &runtime_paths.definition);
    }
    remove_owned_path(
        "runtime-definition",
        &runtime_paths.definition,
        inventory,
        &mut report,
    )?;

    for kind in ["managed-link", "managed-binary", "binary-placement"] {
        for path in inventory.paths_for_kind(kind) {
            remove_owned_path(kind, &path, inventory, &mut report)?;
        }
    }

    report
        .retained
        .push(format!("configuration: {}", config_path.display()));
    for store in store_paths {
        report.retained.push(format!("store: {}", store.display()));
    }
    Ok(report)
}

fn remove_owned_path(
    kind: &str,
    path: &Path,
    inventory: &mut Inventory,
    report: &mut UninstallReport,
) -> Result<(), String> {
    if !inventory.owns_path(kind, path) {
        return Ok(());
    }
    if path.exists() {
        if let Some(expected) = inventory
            .entry_for_path(kind, path)
            .and_then(|entry| entry.get("sha256"))
            .and_then(|digest| digest.as_str())
        {
            let actual = digest_file(path)?;
            if actual != expected {
                report.retained.push(format!(
                    "modified managed destination retained: {}",
                    path.display()
                ));
                return Ok(());
            }
        }
        if path.is_dir() {
            return Err(format!(
                "refusal: inventory-owned {} is a directory and will not be removed: {}",
                kind,
                path.display()
            ));
        }
        fs::remove_file(path).map_err(|error| {
            format!(
                "could not remove inventory-owned {}: {error}",
                path.display()
            )
        })?;
        report.removed.push(path.to_path_buf());
    }
    inventory.remove_owned_path(kind, path);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        env, process,
        time::{SystemTime, UNIX_EPOCH},
    };

    use serde_json::Map;

    use super::*;

    #[derive(Default)]
    struct SuccessfulRunner;
    impl CommandRunner for SuccessfulRunner {
        fn run(
            &mut self,
            _program: &str,
            _args: &[String],
        ) -> Result<super::super::runtime::CommandResult, String> {
            Ok(super::super::runtime::CommandResult {
                success: true,
                stdout: String::new(),
            })
        }
    }

    fn fixture_dir(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        env::temp_dir().join(format!("ck-uninstall-{name}-{}-{nonce}", process::id()))
    }

    #[test]
    fn uninstall_removes_only_inventory_paths_and_retains_configuration_and_stores() {
        let root = fixture_dir("owned-only");
        fs::create_dir_all(&root).expect("fixture directory");
        let managed = root.join("ck-subc");
        let unrelated = root.join("notes.txt");
        let config = root.join("subc.jsonc");
        let store = root.join("store.sqlite");
        fs::write(&managed, "managed").expect("managed binary");
        fs::write(&unrelated, "user data").expect("user data");
        fs::write(&config, "user config").expect("config");
        fs::write(&store, "user store").expect("store");
        let runtime_paths = RuntimePaths {
            definition: root.join("unit"),
            daemon: managed.clone(),
        };
        let mut inventory =
            Inventory::load(root.join("installer-manifest.json"), "linux-x64").expect("inventory");
        let mut fields = Map::new();
        fields.insert(
            "sha256".to_string(),
            serde_json::Value::String(digest_file(&managed).expect("digest")),
        );
        inventory.record("managed-binary", &managed, fields);

        let report = uninstall(
            RuntimePlatform::Linux,
            &runtime_paths,
            &mut SuccessfulRunner,
            &mut inventory,
            &config,
            &[store.clone()],
        )
        .expect("uninstall");
        assert!(!managed.exists());
        assert!(unrelated.exists());
        assert!(config.exists());
        assert!(store.exists());
        assert!(report
            .retained
            .iter()
            .any(|line| line.contains("configuration")));
        assert!(report.retained.iter().any(|line| line.contains("store")));
        fs::remove_dir_all(root).expect("remove fixture");
    }
}
