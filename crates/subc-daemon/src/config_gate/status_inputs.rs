use super::super::{GateSnapshot, InputKey};

/// Environment overrides are not among the configuration-file bytes a person
/// approves. Retain their names as diagnostics, never potentially secret values.
const STARTUP_BEHAVIOR_ENV: &[&str] = &[
    "SUBC_PORT",
    "SUBC_CGROUP_PLACEMENT",
    "INVOCATION_ID",
    "SUBC_TEST_OPERATOR_SCRIPT",
    // The guarded config test switch has no reader in release builds.
    #[cfg(feature = "test-support")]
    "SUBC_TEST_CONFIG_GATE",
    "SUBC_TEST_OPERATOR_EVENTS",
    "SUBC_TEST_OPERATOR_QUEUE_WAIT_MS",
    "SUBC_TEST_OPERATOR_TIMEOUT_MS",
    "SUBC_TEST_OPERATOR_STUCK_MS",
    "SUBC_TEST_SHUTDOWN_INTENT_WINDOW_MS",
];

pub(super) fn present_overrides(mut present: impl FnMut(&str) -> bool) -> Vec<String> {
    STARTUP_BEHAVIOR_ENV
        .iter()
        .filter(|name| present(name))
        .map(|name| (*name).into())
        .collect()
}

pub(super) fn report_snapshot(
    snapshot: &GateSnapshot,
    status: &mut subc_control::config::ConfigStatus,
) {
    status.ungated_modules = snapshot
        .inputs
        .keys()
        .filter_map(|key| match key {
            InputKey::Entry(module)
                if !snapshot
                    .inputs
                    .contains_key(&InputKey::Declaration(module.clone())) =>
            {
                Some(module.clone())
            }
            _ => None,
        })
        .collect();
    status.interpreter_only_covered = snapshot
        .inputs
        .iter()
        .filter_map(|(key, input)| {
            let InputKey::Program { module, .. } = key else {
                return None;
            };
            let subc_config::RecordedValue::Value { value } = &input.value else {
                return None;
            };
            (value.get("coverage").and_then(serde_json::Value::as_str)
                == Some("interpreter only covered"))
            .then(|| module.clone())
        })
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_status_startup_environment_reads_are_classified() {
        const PATH_ENV: &[&str] = &[
            "HOME",
            "USERPROFILE",
            "APPDATA",
            "XDG_RUNTIME_DIR",
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
        ];
        for source in [
            include_str!("../bootstrap.rs"),
            include_str!("../daemon_config.rs"),
            include_str!("../systemd_kill_mode.rs"),
            include_str!("../../../subc-core/src/main.rs"),
        ] {
            let source = source.split("mod tests {").next().unwrap();
            let env_read = ["env", "::var"].concat();
            for needle in [
                format!("{env_read}("),
                format!("{env_read}_os("),
                "non_empty_os_var(".into(),
            ] {
                for rest in source.split(&needle).skip(1) {
                    let arg = rest.split(')').next().unwrap().trim();
                    // The two environment helpers and the operator-duration loop
                    // use dynamic names; their callers/literal inventory are checked below.
                    if matches!(arg, "key" | "key: &str" | "name" | "SUBC_PORT_ENV") {
                        continue;
                    }
                    let name = arg
                        .strip_prefix('"')
                        .and_then(|v| v.strip_suffix('"'))
                        .unwrap_or_else(|| {
                            panic!("classify new startup environment expression: {arg}")
                        });
                    assert!(
                        STARTUP_BEHAVIOR_ENV.contains(&name)
                            || PATH_ENV.contains(&name)
                            || (name == "SUBC_TEST_CONFIG_GATE" && !cfg!(feature = "test-support")),
                        "classify startup environment variable {name}"
                    );
                }
            }
            for literal in source
                .split('"')
                .skip(1)
                .step_by(2)
                .filter(|name| name.starts_with("SUBC_TEST_OPERATOR_") || *name == "SUBC_PORT")
            {
                assert!(
                    STARTUP_BEHAVIOR_ENV.contains(&literal),
                    "classify startup override {literal}"
                );
            }
        }
        assert_eq!(present_overrides(|name| name == "SUBC_PORT"), ["SUBC_PORT"]);
    }

    #[test]
    fn config_status_environment_read_inventory_is_pinned() {
        use std::{collections::BTreeMap, path::Path};
        fn inventory(root: &Path, path: &Path, found: &mut BTreeMap<String, usize>) {
            for entry in std::fs::read_dir(path).unwrap() {
                let path = entry.unwrap().path();
                let name = path.file_name().unwrap().to_str().unwrap();
                if name == "tests" || name == "tests.rs" || name.ends_with("_tests.rs") {
                    continue;
                }
                if path.is_dir() {
                    inventory(root, &path, found);
                } else if path.extension().is_some_and(|extension| extension == "rs") {
                    let source = std::fs::read_to_string(&path).unwrap();
                    let prefix = ["env", "::var"].concat();
                    let count = source.matches(&format!("{prefix}(")).count()
                        + source.matches(&format!("{prefix}_os(")).count();
                    if count != 0 {
                        found.insert(
                            path.strip_prefix(root)
                                .unwrap()
                                .to_string_lossy()
                                .replace('\\', "/"),
                            count,
                        );
                    }
                }
            }
        }
        // Pin all non-test source files, including their inline test reads. This
        // intentionally conservative inventory catches reads in new startup
        // modules too: classify an added read as an override, path input or test
        // fixture before updating the inventory, rather than silently omitting it.
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut found = BTreeMap::new();
        inventory(&root, &root, &mut found);
        let expected = BTreeMap::from([
            ("app_open.rs", 2),
            // Resource-accounting helper inputs, never daemon startup overrides.
            ("bin/ckdev-resource-helper.rs", 2),
            ("bootstrap.rs", 10),
            // The isolated process CPU/memory reader fixture is test-only.
            ("child_resources.rs", 1),
            ("config_gate/program.rs", 1),
            ("config_gate/runtime.rs", 1),
            ("config_store/writer_lock.rs", 1),
            ("control.rs", 3),
            ("daemon_config.rs", 4),
            ("live_children.rs", 1),
            ("privileged_prompt.rs", 1),
            ("run_dir_lock.rs", 2),
            ("supervise.rs", 12),
            ("systemd_kill_mode.rs", 1),
        ])
        .into_iter()
        .map(|(path, count)| (path.into(), count))
        .collect();
        assert_eq!(
            found, expected,
            "classify the new daemon environment read and update startup status coverage"
        );
    }
}
