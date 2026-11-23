//! User-only settings declaration and pure review of daemon-supplied layers.

mod migration;

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::{LazyLock, Mutex},
};

use serde_json::{json, Value};
use subc_client_rs::config::{
    ApplyKind, ConfigDeclareRequest, ConfigReviewRefusal, ConfigReviewReply, ConfigReviewRequest,
    ConfigStatus, ConfigView, FieldChange, Layer, LayerValue, RecordedValue, Schema,
    ValidationContext,
};

use crate::{
    constants::{DEFAULT_DEADLINE_MS, DEFAULT_FRAME_CEILING_BYTES, DEFAULT_IDLE_TTL_MS},
    registry::{parse_effective, parse_jsonc, pointer, ServerRegistry},
};

pub const MODULE_ID: &str = "mcp-stdio-adapter";
const USER_FILE: &str = "mcp-stdio-adapter.jsonc";
const LEGACY_FILE: &str = "mcp-servers.jsonc";

static DECLARED_SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    let environment = json!({
        "type": "object", "unevaluatedProperties": false, "default": {},
        "title": "Environment",
        "description": "Environment variables passed to the program",
        "patternProperties": {"^[^=\\u0000]+$": {
            "type": "object", "unevaluatedProperties": false,
            "title": "Variable",
            "properties": {
                "credential_ref": {"type": "object", "x-ck-kind": "credential_ref",
                    "title": "Vault credential",
                    "description": "Vault credential to resolve when the child starts"},
                "value": {"type": "string", "x-ck-required": false,
                    "title": "Value",
                    "description": "Ordinary non-secret environment value"}
            }
        }}
    });
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object", "unevaluatedProperties": false,
        "title": "MCP Servers",
        "properties": {"servers": {
            "type": "object", "unevaluatedProperties": false, "default": {},
            "title": "Servers",
            "description": "Third-party MCP servers available to the adapter",
            "x-ck-layers": ["user"], "x-ck-apply": "live",
            "patternProperties": {"^[a-z0-9-]{1,64}$": {
                "type": "object", "unevaluatedProperties": false,
                "title": "Server",
                "properties": {
                    "command": {"type": "string", "title": "Program", "description": "Program to launch", "x-ck-required": true},
                    "args": {
                        "type": "array", "title": "Arguments", "description": "Arguments passed to the program", "default": [],
                        "items": {"type": "string", "description": "Program argument", "x-ck-required": true}
                    },
                    "cwd": {"type": "string", "title": "Working folder", "description": "Working directory for the program", "x-ck-required": false},
                    "disabled": {"type": "boolean", "title": "Disabled", "description": "Prevent this server from starting", "default": false},
                    "idle_ttl_ms": {"type": "integer", "minimum": 0, "default": DEFAULT_IDLE_TTL_MS,
                        "title": "Idle stop (ms)",
                        "description": "Milliseconds before stopping an idle server; zero uses the default"},
                    "deadline_ms": {"type": "integer", "minimum": 0, "default": DEFAULT_DEADLINE_MS,
                        "title": "Request deadline (ms)",
                        "description": "Milliseconds allowed for a request; zero uses the default"},
                    "frame_ceiling_bytes": {"type": "integer", "minimum": 0, "default": DEFAULT_FRAME_CEILING_BYTES,
                        "title": "Largest reply (bytes)",
                        "description": "Maximum size of a child reply frame in bytes"},
                    "cache_tools_list": {"type": "boolean", "default": true,
                        "title": "Cache tool list",
                        "description": "Cache successful tool listings"},
                    "env": environment
                }
            }}
        }}
    })
});

static SCHEMA: LazyLock<Schema> = LazyLock::new(|| {
    Schema::new(DECLARED_SCHEMA.clone()).expect("adapter's compiled settings schema is valid")
});

pub struct UserSettings {
    pub(crate) path: String,
    data_dir: PathBuf,
    migration: Mutex<MigrationStatus>,
}

#[derive(Default)]
struct MigrationStatus {
    notice: Option<String>,
    failed: bool,
    legacy_in_effect: bool,
}

impl UserSettings {
    pub fn locate() -> std::io::Result<Self> {
        let config_home = PathBuf::from(cortexkit_store_types::resolve_config_home());
        if !config_home.is_absolute() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "set XDG_CONFIG_HOME to an absolute path to locate MCP adapter settings",
            ));
        }
        let data_home = PathBuf::from(cortexkit_store_types::resolve_data_home());
        let config_dir = config_home.join("cortexkit");
        let user_file = config_dir.join(USER_FILE);
        let path = user_file.into_os_string().into_string().map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "MCP adapter settings path must be Unicode",
            )
        })?;
        Ok(Self {
            path,
            data_dir: data_home.join("cortexkit").join(MODULE_ID),
            migration: Mutex::default(),
        })
    }

    pub(crate) fn convert_legacy_file(&self) {
        let config_dir = Path::new(&self.path)
            .parent()
            .expect("user file has a directory");
        let result = migration::convert(config_dir, &self.data_dir);
        let failed = result.is_err();
        let notice = result.unwrap_or_else(|error| {
            Some(format!(
                "could not convert {}: {error}",
                config_dir.join(LEGACY_FILE).display()
            ))
        });
        if let Some(notice) = &notice {
            tracing::warn!("{notice}");
        }
        *self.migration.lock().expect("migration status lock") = MigrationStatus {
            notice,
            failed,
            legacy_in_effect: false,
        };
    }

    pub(crate) fn migration_notice(&self) -> Option<String> {
        let migration = self.migration.lock().expect("migration status lock");
        migration.notice.as_ref().map(|notice| {
            if migration.legacy_in_effect {
                format!("{notice}; {LEGACY_FILE} remains in effect")
            } else {
                notice.clone()
            }
        })
    }

    pub(crate) fn set_legacy_in_effect(&self, active: bool) {
        self.migration
            .lock()
            .expect("migration status lock")
            .legacy_in_effect = active;
    }

    /// Load settings directly only when the daemon does not manage their approval.
    pub(crate) fn unprotected_registry(&self) -> Result<(ServerRegistry, bool), String> {
        let path = Path::new(&self.path);
        match std::fs::read_to_string(path) {
            Ok(text) => read_user_document(
                path,
                parse_jsonc(path, &text).map_err(|error| error.to_string())?,
            )
            .map(|registry| (registry, false)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let legacy = self.legacy_registry_after_failure()?;
                let in_effect = legacy.is_some();
                Ok((legacy.unwrap_or_default(), in_effect))
            }
            Err(error) => Err(format!("could not read {}: {error}", path.display())),
        }
    }

    pub(crate) fn registry_after_migration_failure(
        &self,
        view: &ConfigView,
    ) -> Result<Option<ServerRegistry>, Vec<String>> {
        // Only an explicitly unstamped user view with no canonical file can
        // use the old file. A protected view (even empty) is authoritative.
        if view.status != ConfigStatus::Unstamped
            || view.generation != 0
            || view.layers.iter().any(|layer| {
                layer.bytes.is_some() || layer.layer.known_layer() != Some(Layer::User)
            })
            || view.effective.get("servers").is_some_and(|servers| {
                servers
                    .as_object()
                    .is_none_or(|entries| !entries.is_empty())
            })
        {
            return Ok(None);
        }
        self.legacy_registry_after_failure()
            .map_err(|error| vec![error])
    }

    fn legacy_registry_after_failure(&self) -> Result<Option<ServerRegistry>, String> {
        if !self.migration.lock().expect("migration status lock").failed {
            return Ok(None);
        }
        let legacy = Path::new(&self.path).with_file_name(LEGACY_FILE);
        let Some(text) = migration::read_legacy(&legacy).map_err(|error| error.to_string())? else {
            return Err(format!(
                "legacy settings {} are no longer available",
                legacy.display()
            ));
        };
        let (document, _) =
            migration::legacy_document(&legacy, &text).map_err(|error| error.to_string())?;
        read_user_document(&legacy, document).map(Some)
    }
}

pub(crate) fn declaration(path: &str) -> ConfigDeclareRequest {
    ConfigDeclareRequest::new(DECLARED_SCHEMA.clone(), path, vec![])
}

pub(crate) fn review(request: ConfigReviewRequest) -> ConfigReviewReply {
    match review_fields(request) {
        Ok(fields) => ConfigReviewReply::new(fields, vec![]),
        Err(refusal) => ConfigReviewReply::new(vec![], vec![refusal]),
    }
}

fn refusal(code: &str, path: String, reason: impl Into<String>) -> ConfigReviewRefusal {
    ConfigReviewRefusal::new(code, vec![path], vec![reason.into()])
}

fn review_fields(request: ConfigReviewRequest) -> Result<Vec<FieldChange>, ConfigReviewRefusal> {
    let project_id = request
        .base
        .as_ref()
        .and_then(|view| view.project_id.clone());
    let mut layers = Vec::new();
    for layer in request.layers {
        let Some(bytes) = layer.bytes else { continue };
        if layer.layer.known_layer() != Some(Layer::User) {
            return Err(refusal(
                "layer_forbidden",
                "/servers".into(),
                "MCP servers can be configured only in the user layer",
            ));
        }
        let path = PathBuf::from(layer.path.unwrap_or_else(|| USER_FILE.into()));
        let text = std::str::from_utf8(&bytes).map_err(|_| {
            refusal(
                "value_invalid",
                String::new(),
                "settings must be UTF-8 JSONC",
            )
        })?;
        let document = parse_jsonc(&path, text)
            .map_err(|error| refusal("value_invalid", error.pointer(), error.to_string()))?;
        layers.push(LayerValue::new(Layer::User, document));
    }
    let tuple = SCHEMA
        .review_tuple(
            &layers,
            &ValidationContext::new(MODULE_ID, project_id.clone()),
        )
        .map_err(|error| {
            refusal(
                "value_invalid",
                error.path,
                "settings do not match the adapter schema",
            )
        })?;
    parse_effective(&tuple.effective)
        .map_err(|error| refusal("value_invalid", error.pointer(), error.to_string()))?;
    let candidate = tuple.effective;
    let base = request.base.as_ref();
    let old = base.map_or(&Value::Null, |base| &base.effective);
    let record = |value| {
        SCHEMA.record(value).map_err(|_| {
            refusal(
                "value_invalid",
                String::new(),
                "settings could not be recorded for approval",
            )
        })
    };
    let old_recorded = record(old)?;
    let new_recorded = record(&candidate)?;
    let mut paths = BTreeSet::new();
    leaf_paths(old, "", &mut paths);
    leaf_paths(&candidate, "", &mut paths);
    let mut fields = Vec::new();
    for path in paths {
        let old_value = old.pointer(&path);
        let new_value = candidate.pointer(&path);
        let old_source = base.map_or(Layer::Default, |base| source(&base.sources, &path));
        let new_source = source(&tuple.sources, &path);
        if old_value == new_value && old_source == new_source {
            continue;
        }
        let mut field = FieldChange::new(
            MODULE_ID,
            &path,
            record_leaf(old_recorded.pointer(&path)),
            record_leaf(new_recorded.pointer(&path)),
            ApplyKind::Live,
        );
        field.old_source = old_source;
        field.new_source = new_source;
        field.project_id = project_id.clone();
        fields.push(field);
    }
    Ok(fields)
}

// Arrays, empty sections and vault references are single review fields.
fn leaf_paths(value: &Value, path: &str, paths: &mut BTreeSet<String>) {
    if let Some(object) = value.as_object() {
        if !object.is_empty() && !(object.len() == 1 && object.contains_key("vault_ref")) {
            for (key, child) in object {
                leaf_paths(child, &pointer(path, key), paths);
            }
            return;
        }
    }
    if !path.is_empty() {
        paths.insert(path.to_string());
    }
}

fn record_leaf(value: Option<&Value>) -> RecordedValue {
    let value = value.cloned().unwrap_or(Value::Null);
    if value.get("kind").and_then(Value::as_str) == Some("redacted") {
        if let Ok(record) = serde_json::from_value(value.clone()) {
            return record;
        }
    }
    RecordedValue::new(value)
}

fn source(sources: &std::collections::BTreeMap<String, Layer>, mut path: &str) -> Layer {
    let prefix = format!("{path}/");
    if let Some((_, layer)) = sources
        .range(prefix.clone()..)
        .next()
        .filter(|(p, _)| p.starts_with(&prefix))
    {
        return *layer;
    }
    loop {
        if let Some(layer) = sources.get(path) {
            return *layer;
        }
        let Some((parent, _)) = path.rsplit_once('/') else {
            return Layer::Default;
        };
        path = parent;
    }
}

pub(crate) fn registry_from_view(view: &ConfigView) -> Result<ServerRegistry, Vec<String>> {
    // A view for a project, even one the daemon supplies, must never supply this
    // module's servers: only the user's own layer may start a program.
    if view.project_id.is_some()
        || view
            .sources
            .values()
            .any(|layer| !matches!(layer, Layer::User | Layer::Default))
    {
        return Err(vec![
            "MCP servers can be configured only in the user layer".into()
        ]);
    }
    registry_from_effective(&view.effective).map_err(|reason| vec![reason])
}

fn registry_from_effective(effective: &Value) -> Result<ServerRegistry, String> {
    let (registry, warnings) = parse_effective(effective).map_err(|error| error.to_string())?;
    for warning in warnings {
        tracing::warn!(target: "registry", "{warning}");
    }
    Ok(registry)
}

fn read_user_document(path: &Path, document: Value) -> Result<ServerRegistry, String> {
    let tuple = SCHEMA
        .review_tuple(
            &[LayerValue::new(Layer::User, document)],
            &ValidationContext::new(MODULE_ID, None),
        )
        .map_err(|error| format!("invalid settings in {} at {}", path.display(), error.path))?;
    registry_from_effective(&tuple.effective)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::{AdapterHandler, ClaustrumCredentialResolver};
    use subc_client_rs::{
        config::{ConfigLayer, ConfigLayerSnapshot},
        HandlerOutcome, ModuleHandler,
    };
    use subc_protocol::error_codes::MODULE_WARMING;

    fn starting_adapter(root: &Path) -> AdapterHandler {
        adapter_with_settings(root, settings_at(root))
    }

    fn settings_at(root: &Path) -> UserSettings {
        UserSettings {
            path: root.join(USER_FILE).into_os_string().into_string().unwrap(),
            data_dir: root.join("data"),
            migration: Mutex::default(),
        }
    }

    fn adapter_with_settings(root: &Path, settings: UserSettings) -> AdapterHandler {
        AdapterHandler::managed(
            settings,
            std::sync::Arc::new(ClaustrumCredentialResolver::new(
                root.join("connection.json"),
            )),
        )
    }

    async fn route_refusal(handler: &AdapterHandler, server: &str) -> String {
        let request =
            json!({"server":server, "op":"tools/list", "payload":{"method":"tools/list"}});
        match handler.route_outcome(request.to_string().as_bytes()).await {
            HandlerOutcome::ErrorWithDetail { code, .. } => code,
            outcome => panic!("expected a refusal, got {outcome:?}"),
        }
    }

    #[tokio::test]
    async fn adapter_refuses_routes_as_warming_until_the_first_view_is_applied() {
        let root = cortexkit_test_support::ScratchDir::new("adapter-first-view");
        let handler = starting_adapter(root.path());
        assert_eq!(route_refusal(&handler, "fixture").await, MODULE_WARMING);

        let view =
            |value| ConfigView::new(1, value, Default::default(), vec![], ConfigStatus::Active);
        assert!(handler
            .config_apply(view(json!({"servers":null})))
            .await
            .is_err());
        assert_eq!(route_refusal(&handler, "fixture").await, MODULE_WARMING);

        handler
            .config_apply(view(
                json!({"servers":{"fixture":{"command":"never-start","disabled":true}}}),
            ))
            .await
            .unwrap();
        assert_eq!(route_refusal(&handler, "fixture").await, "server_disabled");
        assert_eq!(route_refusal(&handler, "missing").await, "server_unknown");
    }

    #[tokio::test]
    async fn loading_an_empty_legacy_registry_completes_readiness() {
        let root = cortexkit_test_support::ScratchDir::new("adapter-empty-file");
        let handler = starting_adapter(root.path());
        std::fs::write(root.join(USER_FILE), "invalid").unwrap();
        handler.config_files().await;
        assert_eq!(route_refusal(&handler, "missing").await, MODULE_WARMING);

        std::fs::write(root.join(USER_FILE), r#"{"servers":{}}"#).unwrap();
        handler.config_files().await;
        assert_eq!(route_refusal(&handler, "missing").await, "server_unknown");
    }

    const OLD_SETTINGS: &str = r#"{"fixture":{"command":"never-start","disabled":true,"env":{"TOKEN":{"handle":"vault:fixture"}}}}"#;

    fn migration_fails_at_backup(root: &Path) -> UserSettings {
        let settings = settings_at(root);
        std::fs::write(root.join(LEGACY_FILE), OLD_SETTINGS).unwrap();
        // Reserving the data directory's name as a regular file blocks backup
        // creation without platform-specific permissions or an invalid registry.
        std::fs::write(&settings.data_dir, b"not a directory").unwrap();
        settings.convert_legacy_file();
        assert!(settings
            .migration_notice()
            .unwrap()
            .contains("could not convert"));
        assert!(!root.join(USER_FILE).exists());
        settings
    }

    async fn assert_legacy_is_in_effect(handler: &AdapterHandler, root: &Path) {
        assert_eq!(route_refusal(handler, "fixture").await, "server_disabled");
        let health = handler.health().await.metrics.unwrap();
        let notice = health["settings"]["migration_notice"].as_str().unwrap();
        assert!(notice.contains("could not convert"), "{notice}");
        assert!(
            notice.contains("mcp-servers.jsonc remains in effect"),
            "{notice}"
        );
        assert_eq!(
            std::fs::read_to_string(root.join(LEGACY_FILE)).unwrap(),
            OLD_SETTINGS
        );
    }

    #[tokio::test]
    async fn failed_conversion_keeps_legacy_settings_on_an_old_daemon() {
        let root = cortexkit_test_support::ScratchDir::new("adapter-backup-blocked");
        let handler = adapter_with_settings(root.path(), migration_fails_at_backup(root.path()));
        handler.config_files().await;
        assert_legacy_is_in_effect(&handler, root.path()).await;
    }

    fn empty_view(status: ConfigStatus, generation: u64, bytes: Option<Vec<u8>>) -> ConfigView {
        ConfigView::new(
            generation,
            json!({"servers":{}}),
            Default::default(),
            vec![ConfigLayerSnapshot::file(
                MODULE_ID,
                ConfigLayer::User,
                USER_FILE,
                bytes,
            )],
            status,
        )
    }

    #[tokio::test]
    async fn failed_conversion_keeps_legacy_settings_only_when_unstamped_and_canonical_is_absent() {
        let root = cortexkit_test_support::ScratchDir::new("adapter-unstamped-backup-blocked");
        let handler = adapter_with_settings(root.path(), migration_fails_at_backup(root.path()));
        handler
            .config_apply(empty_view(ConfigStatus::Unstamped, 0, None))
            .await
            .unwrap();
        assert_legacy_is_in_effect(&handler, root.path()).await;

        handler
            .config_apply(empty_view(
                ConfigStatus::Unstamped,
                0,
                Some(br#"{"servers":{}}"#.to_vec()),
            ))
            .await
            .unwrap();
        assert_eq!(route_refusal(&handler, "fixture").await, "server_unknown");
        let health = handler.health().await.metrics.unwrap();
        assert!(!health["settings"]["migration_notice"]
            .as_str()
            .unwrap()
            .contains("remains in effect"));
    }

    #[tokio::test]
    async fn failed_conversion_never_reads_legacy_settings_for_protected_views() {
        let root = cortexkit_test_support::ScratchDir::new("adapter-stamped-backup-blocked");
        let handler = adapter_with_settings(root.path(), migration_fails_at_backup(root.path()));
        for status in [ConfigStatus::Active, ConfigStatus::Pending] {
            handler
                .config_apply(empty_view(status, 1, None))
                .await
                .unwrap();
            assert_eq!(route_refusal(&handler, "fixture").await, "server_unknown");
        }
        // A read of this file would reject the delivered view, not apply it.
        std::fs::write(root.join(LEGACY_FILE), "invalid").unwrap();
        handler
            .config_apply(empty_view(ConfigStatus::Active, 2, None))
            .await
            .unwrap();
        let health = handler.health().await.metrics.unwrap();
        let notice = health["settings"]["migration_notice"].as_str().unwrap();
        assert!(notice.contains("could not convert"), "{notice}");
        assert!(!notice.contains("remains in effect"), "{notice}");
    }

    fn request(
        layer: ConfigLayer,
        document: &str,
        base: Option<ConfigView>,
    ) -> ConfigReviewRequest {
        ConfigReviewRequest::new(
            1,
            vec![ConfigLayerSnapshot::file(
                MODULE_ID,
                layer,
                USER_FILE,
                Some(document.as_bytes().to_vec()),
            )],
            base,
        )
    }

    // Settings shows `title` as a field's label and falls back to the raw key
    // without one. Array items describe a single element and need none.
    fn untitled(node: &Value, path: &str, missing: &mut Vec<String>) {
        let Some(object) = node.as_object() else {
            return;
        };
        if object.contains_key("description")
            && !object.contains_key("title")
            && !path.ends_with("/items")
        {
            missing.push(path.to_owned());
        }
        // Walk subschemas only: the keys of a `properties` or
        // `patternProperties` map are setting names (one is "description").
        for keyword in ["properties", "patternProperties"] {
            for (key, child) in object
                .get(keyword)
                .and_then(Value::as_object)
                .into_iter()
                .flatten()
            {
                untitled(child, &format!("{path}/{key}"), missing);
            }
        }
        if let Some(items) = object.get("items") {
            untitled(items, &format!("{path}/items"), missing);
        }
    }

    #[test]
    fn every_described_setting_has_a_title() {
        let mut missing = Vec::new();
        untitled(&DECLARED_SCHEMA, "", &mut missing);
        assert!(missing.is_empty(), "settings without a title: {missing:?}");
        assert_eq!(DECLARED_SCHEMA["title"], "MCP Servers");
    }

    #[test]
    fn settings_declaration_and_review_refuse_project_and_workspace_servers() {
        let declaration = declaration("mcp-stdio-adapter.jsonc");
        assert!(declaration.project_dirs.is_empty());
        assert_eq!(
            declaration.schema["properties"]["servers"]["x-ck-layers"],
            json!(["user"])
        );
        let document = r#"{"servers":{"example":{"command":"never-start"}}}"#;
        for layer in [ConfigLayer::Project, ConfigLayer::Workspace] {
            let reply = review(request(layer, document, None));
            assert!(reply.fields.is_empty());
            assert_eq!(reply.refusals[0].code, "layer_forbidden");
            assert_eq!(reply.refusals[0].paths, ["/servers"]);
        }
        let mut forged = ConfigView::new(
            1,
            serde_json::from_str(document).unwrap(),
            Default::default(),
            vec![],
            ConfigStatus::Active,
        );
        forged.project_id = Some("project".into());
        assert!(registry_from_view(&forged).is_err());
        forged.project_id = None;
        forged
            .sources
            .insert("/servers/example/command".into(), Layer::Project);
        assert!(registry_from_view(&forged).is_err());
    }

    #[test]
    fn settings_review_refuses_missing_or_multiple_tags_with_the_environment_pointer() {
        for binding in [
            json!({}),
            json!({"value":"ordinary","credential_ref":{"vault_ref":"vault:never-log"}}),
        ] {
            let document =
                json!({"servers":{"example":{"command":"fixture","env":{"A~/B":binding}}}});
            let reply = review(request(ConfigLayer::User, &document.to_string(), None));
            assert!(reply.fields.is_empty());
            assert_eq!(reply.refusals[0].paths, ["/servers/example/env/A~0~1B"]);
            assert!(!serde_json::to_string(&reply)
                .unwrap()
                .contains("vault:never-log"));
        }
    }

    #[test]
    fn settings_review_compares_against_the_supplied_base_and_redacts_credentials() {
        let before = json!({"servers":{"example":{"command":"old","env":{"TOKEN":{"credential_ref":{"vault_ref":"old-private-id"}}}}}});
        let after = json!({"servers":{"example":{"command":"new","env":{"TOKEN":{"credential_ref":{"vault_ref":"new-private-id"}},"MODE":{"value":"debug"}}}}});
        let tuple = SCHEMA
            .review_tuple(
                &[LayerValue::new(Layer::User, before)],
                &ValidationContext::new(MODULE_ID, None),
            )
            .unwrap();
        let base = ConfigView::new(
            1,
            tuple.effective,
            tuple.sources,
            vec![],
            ConfigStatus::Active,
        );
        let reply = review(request(ConfigLayer::User, &after.to_string(), Some(base)));
        assert!(reply.refusals.is_empty(), "{:?}", reply.refusals);
        assert_eq!(reply.fields.len(), 3);
        let command = &reply.fields[0];
        assert_eq!(command.path, "/servers/example/command");
        assert_eq!(command.old, RecordedValue::new(json!("old")));
        assert_eq!(command.new, RecordedValue::new(json!("new")));
        assert_eq!(
            (command.old_source, command.new_source, command.apply),
            (Layer::User, Layer::User, ApplyKind::Live)
        );
        assert_eq!(reply.fields[1].new, RecordedValue::new(json!("debug")));
        let credential = &reply.fields[2];
        assert_eq!(credential.path, "/servers/example/env/TOKEN/credential_ref");
        assert!(matches!(credential.old, RecordedValue::Redacted { .. }));
        assert!(matches!(credential.new, RecordedValue::Redacted { .. }));
        assert!(!serde_json::to_string(&reply)
            .unwrap()
            .contains("private-id"));
    }
}
