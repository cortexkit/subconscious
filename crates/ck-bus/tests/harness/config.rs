use std::{
    fs,
    path::{Path, PathBuf},
};

use serde_json::Value;

const TEMPLATE: &str = include_str!("../fixtures/subc.jsonc");

#[derive(Debug, Clone, Copy, Default)]
pub struct SentinelTiming {
    pub period_ms: Option<u64>,
    pub timeout_ms: Option<u64>,
}

pub fn template_value() -> Value {
    let json = subc_jsonc::jsonc_to_json(TEMPLATE).expect("fixture subc.jsonc must be valid JSONC");
    serde_json::from_str(&json).expect("fixture subc.jsonc must be valid JSON")
}

/// Writes the fixture daemon config under `fixture_root`. The supervised
/// modules run `ck_bus_binary` as placed under its `ckdev-` name in the
/// fixture root, so the processes the daemon starts are never listed under
/// the production `ck-bus` name.
pub fn render(fixture_root: &Path, ck_bus_binary: &Path, timing: SentinelTiming) -> PathBuf {
    let ck_bus_binary =
        &subc_test_support::ckdev_binary_in(ck_bus_binary, fixture_root.join("ckdev-bin"));
    let mut value = template_value();
    let modules = value["modules"]
        .as_object_mut()
        .expect("observable fixture modules must be an object");
    for module_id in ["ckbus", "claustrum", "callosum"] {
        modules[module_id]["program"] = Value::String(ck_bus_binary.display().to_string());
    }
    let env = modules["ckbus"]["env"]
        .as_object_mut()
        .expect("observable ckbus env must be an object");
    env.insert(
        "XDG_DATA_HOME".to_string(),
        Value::String(fixture_root.join("data").display().to_string()),
    );
    for (name, directory) in [("XDG_RUNTIME_DIR", "run"), ("XDG_CONFIG_HOME", "config")] {
        env.insert(
            name.to_string(),
            Value::String(fixture_root.join(directory).display().to_string()),
        );
    }
    if let Some(period_ms) = timing.period_ms {
        env.insert(
            "CKBUS_SENTINEL_PERIOD_MS".to_string(),
            Value::String(period_ms.to_string()),
        );
    }
    if let Some(timeout_ms) = timing.timeout_ms {
        env.insert(
            "CKBUS_SENTINEL_TIMEOUT_MS".to_string(),
            Value::String(timeout_ms.to_string()),
        );
    }

    let path = fixture_root.join("config/subc.jsonc");
    fs::create_dir_all(path.parent().expect("rendered config has a parent"))
        .expect("observable fixture config directory must be creatable");
    fs::write(
        &path,
        serde_json::to_vec_pretty(&value).expect("rendered fixture config must encode"),
    )
    .expect("observable fixture config must be writable");
    path
}
