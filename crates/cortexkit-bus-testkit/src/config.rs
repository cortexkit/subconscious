use crate::RigError;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

/// A module the private subc daemon spawns and supervises, like a production module entry in subc.jsonc. Environment entries augment the daemon's inherited
/// environment; do not override its isolated XDG homes.
#[derive(Clone, Debug)]
pub struct Module {
    pub id: String,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub reserved: bool,
    pub enabled: bool,
}

impl Module {
    pub fn new(id: impl Into<String>, program: impl Into<PathBuf>) -> Self {
        Self {
            id: id.into(),
            program: program.into(),
            args: vec![],
            env: BTreeMap::new(),
            reserved: false,
            enabled: true,
        }
    }
}

/// Build JSON (also valid JSONC) with a reserved ckbus and caller-supplied modules.
/// All program paths are staged copies, even when a module is initially disabled.
pub fn subc_config(
    ck_bus: &Path,
    ckbus_env: &[(String, String)],
    modules: &[Module],
) -> Result<String, RigError> {
    let mut entries = serde_json::Map::new();
    entries.insert(
        "ckbus".into(),
        json!({
            "program": cortexkit_test_support::ckdev_binary(ck_bus), "args": [],
            "env": ckbus_env.iter().cloned().collect::<BTreeMap<_, _>>(),
            "enabled": true, "reserved": true, "protocol": "subc", "drain_timeout_ms": 2000,
            "health": {"on_degraded": "report", "on_failing": "report"},
        }),
    );
    for module in modules {
        if module.id.is_empty() || entries.contains_key(&module.id) {
            return Err(RigError::Invalid(format!(
                "empty or duplicate module id {:?}",
                module.id
            )));
        }
        entries.insert(module.id.clone(), json!({
            "program": cortexkit_test_support::ckdev_binary(&module.program), "args": module.args,
            "env": module.env, "reserved": module.reserved, "enabled": module.enabled,
            "protocol": "subc", "drain_timeout_ms": 2000,
        }));
    }
    serde_json::to_string_pretty(
        &json!({"version": 1, "port": 0, "modules": Value::Object(entries)}),
    )
    .map_err(|error| RigError::Invalid(error.to_string()))
}
