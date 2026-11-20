//! Reads and combines gateway settings from the user and project files, including per-app overlays.

use std::{
    collections::HashMap,
    fs, io,
    path::{Path, PathBuf},
    time::SystemTime,
};

use serde::{de, Deserialize, Deserializer};
use subc_jsonc::jsonc_to_json;

use super::{non_empty_os_var, other_error, prompts, Result};

const MCP_CONFIG_RELATIVE_PATH: &str = "cortexkit/mcp.jsonc";
const PROJECT_MCP_CONFIG_RELATIVE_PATH: &str = ".cortexkit/mcp.jsonc";
const FACADE_DEFAULT_DISABLED: &[&str] = &["magic-context", "llm-runner"];

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum SurfaceMode {
    #[default]
    Full,
    Search,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ToolMode {
    #[default]
    Forward,
    AckOnly,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum RefreshMode {
    #[default]
    OnAttach,
    Immediate,
}

impl<'de> Deserialize<'de> for RefreshMode {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        match String::deserialize(deserializer)?.as_str() {
            "on-attach" => Ok(Self::OnAttach),
            "immediate" => Ok(Self::Immediate),
            "on-hard" | "on-soft" => Err(de::Error::custom(
                "refresh value requires a bust-signal source; not available on the MCP path",
            )),
            other => Err(de::Error::unknown_variant(
                other,
                &["on-attach", "immediate"],
            )),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub(super) struct GatewayConfig {
    pub(super) surface_mode: SurfaceMode,
    pub(super) refresh: RefreshMode,
    prompts: PromptConfig,
    providers: HashMap<String, ProviderConfig>,
}

#[derive(Debug, Clone, Default)]
struct ProviderConfig {
    enabled: Option<bool>,
    namespace: Option<String>,
    tools: ToolConfig,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ToolConfig {
    default_enabled: Option<bool>,
    overrides: HashMap<String, ToolOverride>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ToolOverride {
    enabled: Option<bool>,
    description: Option<String>,
    mode: Option<ToolMode>,
    upfront: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct PromptConfig {
    default_enabled: Option<bool>,
    overrides: HashMap<String, PromptOverride>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct PromptOverride {
    enabled: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ConfigFileState {
    path: PathBuf,
    modified: Option<SystemTime>,
    len: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ConfigFileSnapshot {
    user: ConfigFileState,
    project: ConfigFileState,
}

#[derive(Debug, Clone)]
pub(super) struct ConfigSnapshot {
    pub(super) project_root: PathBuf,
    pub(super) effective: GatewayConfig,
    files: ConfigFileSnapshot,
}

#[derive(Debug, Default)]
struct RawGatewayLayer {
    surface_mode: MaybeSet<SurfaceMode>,
    refresh: MaybeSet<RefreshMode>,
    prompts: MaybeSet<RawPromptConfig>,
    providers: HashMap<String, RawProviderConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawGatewayConfig {
    version: u8,
    #[serde(
        default,
        rename = "surfaceMode",
        deserialize_with = "deserialize_maybe_set"
    )]
    surface_mode: MaybeSet<SurfaceMode>,
    #[serde(default, deserialize_with = "deserialize_maybe_set")]
    refresh: MaybeSet<RefreshMode>,
    #[serde(default, deserialize_with = "deserialize_maybe_set")]
    prompts: MaybeSet<RawPromptConfig>,
    #[serde(default)]
    providers: HashMap<String, RawProviderConfig>,
    #[serde(default)]
    harness: HashMap<String, RawGatewayOverlayConfig>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawGatewayOverlayConfig {
    #[serde(
        default,
        rename = "surfaceMode",
        deserialize_with = "deserialize_maybe_set"
    )]
    surface_mode: MaybeSet<SurfaceMode>,
    #[serde(default, deserialize_with = "deserialize_maybe_set")]
    refresh: MaybeSet<RefreshMode>,
    #[serde(default, deserialize_with = "deserialize_maybe_set")]
    prompts: MaybeSet<RawPromptConfig>,
    #[serde(default)]
    providers: HashMap<String, RawProviderConfig>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawProviderConfig {
    #[serde(default, deserialize_with = "deserialize_maybe_set")]
    enabled: MaybeSet<bool>,
    #[serde(default, deserialize_with = "deserialize_maybe_set")]
    namespace: MaybeSet<String>,
    #[serde(default, deserialize_with = "deserialize_maybe_set")]
    tools: MaybeSet<RawToolConfig>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawToolConfig {
    #[serde(
        default,
        rename = "defaultEnabled",
        deserialize_with = "deserialize_maybe_set"
    )]
    default_enabled: MaybeSet<bool>,
    #[serde(default)]
    overrides: HashMap<String, RawToolOverrideValue>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawPromptConfig {
    #[serde(
        default,
        rename = "defaultEnabled",
        deserialize_with = "deserialize_maybe_set"
    )]
    default_enabled: MaybeSet<bool>,
    #[serde(default)]
    overrides: HashMap<String, RawPromptOverrideValue>,
}

#[derive(Debug)]
enum RawPromptOverrideValue {
    Object(RawPromptOverrideObject),
    Bool(bool),
    Null(()),
}

impl<'de> Deserialize<'de> for RawPromptOverrideValue {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        match value {
            serde_json::Value::Null => Ok(Self::Null(())),
            serde_json::Value::Bool(enabled) => Ok(Self::Bool(enabled)),
            serde_json::Value::Object(_) => serde_json::from_value(value)
                .map(Self::Object)
                .map_err(de::Error::custom),
            other => Err(de::Error::custom(format!(
                "prompt override must be bool, null, or object, got {other}"
            ))),
        }
    }
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawPromptOverrideObject {
    #[serde(default, deserialize_with = "deserialize_maybe_set")]
    enabled: MaybeSet<bool>,
}

#[derive(Debug)]
enum RawToolOverrideValue {
    Object(RawToolOverrideObject),
    Bool(bool),
    Null(()),
}

impl<'de> Deserialize<'de> for RawToolOverrideValue {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        match value {
            serde_json::Value::Null => Ok(Self::Null(())),
            serde_json::Value::Bool(enabled) => Ok(Self::Bool(enabled)),
            serde_json::Value::Object(_) => serde_json::from_value(value)
                .map(Self::Object)
                .map_err(de::Error::custom),
            other => Err(de::Error::custom(format!(
                "tool override must be bool, null, or object, got {other}"
            ))),
        }
    }
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawToolOverrideObject {
    #[serde(default, deserialize_with = "deserialize_maybe_set")]
    enabled: MaybeSet<bool>,
    #[serde(default, deserialize_with = "deserialize_maybe_set")]
    description: MaybeSet<String>,
    #[serde(default, deserialize_with = "deserialize_maybe_set")]
    mode: MaybeSet<ToolMode>,
    #[serde(default, deserialize_with = "deserialize_maybe_set")]
    upfront: MaybeSet<bool>,
}

#[derive(Debug, Clone, Default)]
enum MaybeSet<T> {
    #[default]
    Missing,
    Null,
    Value(T),
}

impl PromptConfig {
    fn enabled(&self, prompt_name: &str) -> bool {
        self.overrides
            .get(prompt_name)
            .and_then(|override_config| override_config.enabled)
            .unwrap_or_else(|| self.default_enabled.unwrap_or(false))
    }
}

// Magic Context's everyday context tools should not cost Claude Code a tool-search
// round trip; other providers and tools remain deferred unless configured upfront.
const DEFAULT_UPFRONT_TOOLS: &[(&str, &str)] = &[
    ("magic-context", "ctx_reduce"),
    ("magic-context", "ctx_expand"),
    ("magic-context", "ctx_search"),
    ("magic-context", "ctx_memory"),
    ("magic-context", "ctx_note"),
];

impl GatewayConfig {
    pub(super) fn facade_default() -> Self {
        let mut config = Self::default();
        for module_id in FACADE_DEFAULT_DISABLED {
            config
                .providers
                .entry((*module_id).to_owned())
                .or_default()
                .enabled = Some(false);
        }
        config
    }

    pub(super) fn prompt_enabled(&self, prompt_name: &str) -> bool {
        self.prompts.enabled(prompt_name)
    }

    pub(super) fn visible_prompt_names(&self) -> Vec<&'static str> {
        prompts::prompt_names()
            .into_iter()
            .filter(|prompt_name| self.prompt_enabled(prompt_name))
            .collect()
    }

    pub(super) fn provider_enabled(&self, module_id: &str) -> bool {
        self.providers
            .get(module_id)
            .and_then(|provider| provider.enabled)
            .unwrap_or(true)
    }

    pub(super) fn provider_namespace(&self, module_id: &str) -> String {
        self.providers
            .get(module_id)
            .and_then(|provider| provider.namespace.clone())
            .unwrap_or_else(|| module_id.to_owned())
    }

    pub(super) fn tool_enabled(&self, module_id: &str, tool_name: &str) -> bool {
        let Some(provider) = self.providers.get(module_id) else {
            return true;
        };
        provider
            .tools
            .overrides
            .get(tool_name)
            .and_then(|override_config| override_config.enabled)
            .unwrap_or_else(|| provider.tools.default_enabled.unwrap_or(true))
    }

    pub(super) fn tool_description(&self, module_id: &str, tool_name: &str) -> Option<String> {
        self.providers
            .get(module_id)?
            .tools
            .overrides
            .get(tool_name)?
            .description
            .clone()
    }

    pub(super) fn tool_mode(&self, module_id: &str, tool_name: &str) -> ToolMode {
        self.providers
            .get(module_id)
            .and_then(|provider| provider.tools.overrides.get(tool_name))
            .and_then(|override_config| override_config.mode)
            .unwrap_or_default()
    }

    pub(super) fn tool_upfront(&self, module_id: &str, tool_name: &str) -> bool {
        self.providers
            .get(module_id)
            .and_then(|provider| provider.tools.overrides.get(tool_name))
            .and_then(|override_config| override_config.upfront)
            .unwrap_or_else(|| DEFAULT_UPFRONT_TOOLS.contains(&(module_id, tool_name)))
    }
}

pub(super) fn read_gateway_config(
    project_root: &Path,
    harness_name: &str,
) -> Result<ConfigSnapshot> {
    let user_path = user_mcp_config_path();
    let project_path = project_root.join(PROJECT_MCP_CONFIG_RELATIVE_PATH);
    let user = read_raw_gateway_config("user", &user_path)?;
    let project = read_raw_gateway_config("project", &project_path)?;
    let (effective, notes) = compose_gateway_config(user, project, harness_name);
    for ProjectNote { field, reason } in notes {
        tracing::warn!(target: "catalog", "dropping project MCP config field {field}: {reason}");
    }

    Ok(ConfigSnapshot {
        project_root: project_root.to_path_buf(),
        effective,
        files: gateway_config_file_snapshot(user_path, project_path)?,
    })
}

fn compose_gateway_config(
    user: Option<RawGatewayConfig>,
    project: Option<RawGatewayConfig>,
    harness_name: &str,
) -> (GatewayConfig, Vec<ProjectNote>) {
    let mut effective = GatewayConfig::facade_default();
    let mut notes = Vec::new();
    if let Some(raw) = user {
        let (top_level, harness_layers) = raw.into_parts();
        merge_gateway_config(&mut effective, top_level);
        for harness_layer in matching_harness_layers(harness_layers, harness_name) {
            merge_gateway_config(&mut effective, harness_layer);
        }
    }

    if let Some(raw) = project {
        let (top_level, harness_layers) = raw.into_parts();
        merge_project_gateway_config(&mut effective, top_level, &mut notes);
        for harness_layer in matching_harness_layers(harness_layers, harness_name) {
            merge_project_gateway_config(&mut effective, harness_layer, &mut notes);
        }
    }

    (effective, notes)
}

fn read_raw_gateway_config(tier: &str, path: &Path) -> Result<Option<RawGatewayConfig>> {
    let doc = match fs::read_to_string(path) {
        Ok(doc) => doc,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(other_error(format!(
                "failed to read {tier} MCP config {}: {err}",
                path.display()
            )))
        }
    };
    parse_gateway_config_doc(&doc, path).map(Some)
}

fn gateway_config_file_snapshot(
    user_path: PathBuf,
    project_path: PathBuf,
) -> Result<ConfigFileSnapshot> {
    Ok(ConfigFileSnapshot {
        user: config_file_state(user_path)?,
        project: config_file_state(project_path)?,
    })
}

fn config_file_state(path: PathBuf) -> Result<ConfigFileState> {
    match fs::metadata(&path) {
        Ok(metadata) => Ok(ConfigFileState {
            path,
            modified: metadata.modified().ok(),
            len: Some(metadata.len()),
        }),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(ConfigFileState {
            path,
            modified: None,
            len: None,
        }),
        Err(err) => Err(other_error(format!(
            "failed to stat MCP config {}: {err}",
            path.display()
        ))),
    }
}

pub(super) fn config_files_changed(snapshot: &ConfigSnapshot) -> Result<bool> {
    Ok(gateway_config_file_snapshot(
        snapshot.files.user.path.clone(),
        snapshot.files.project.path.clone(),
    )? != snapshot.files)
}

fn user_mcp_config_path() -> PathBuf {
    if let Some(config_home) = non_empty_os_var("XDG_CONFIG_HOME") {
        return PathBuf::from(config_home).join(MCP_CONFIG_RELATIVE_PATH);
    }
    if let Some(home) = non_empty_os_var("HOME") {
        return PathBuf::from(home)
            .join(".config")
            .join(MCP_CONFIG_RELATIVE_PATH);
    }
    PathBuf::from(".config").join(MCP_CONFIG_RELATIVE_PATH)
}

fn parse_gateway_config_doc(doc: &str, path: &Path) -> Result<RawGatewayConfig> {
    let json = jsonc_to_json(doc).map_err(|message| {
        other_error(format!(
            "invalid JSONC in MCP config {}: {message}",
            path.display()
        ))
    })?;
    let raw: RawGatewayConfig = serde_json::from_str(&json).map_err(|source| {
        other_error(format!("invalid MCP config {}: {source}", path.display()))
    })?;
    if raw.version != 1 {
        return Err(other_error(format!(
            "invalid MCP config {}: version {} is unsupported (expected 1)",
            path.display(),
            raw.version
        )));
    }
    validate_raw_gateway_config(&raw, path)?;
    Ok(raw)
}

fn validate_raw_gateway_config(raw: &RawGatewayConfig, path: &Path) -> Result<()> {
    validate_raw_prompts(&raw.prompts, path, "prompts")?;
    validate_raw_providers(&raw.providers, path, "providers")?;
    for (harness_name, harness) in &raw.harness {
        validate_raw_prompts(
            &harness.prompts,
            path,
            &format!("harness.{harness_name}.prompts"),
        )?;
        validate_raw_providers(
            &harness.providers,
            path,
            &format!("harness.{harness_name}.providers"),
        )?;
    }
    Ok(())
}

fn validate_raw_prompts(
    prompts: &MaybeSet<RawPromptConfig>,
    path: &Path,
    prefix: &str,
) -> Result<()> {
    let MaybeSet::Value(prompts) = prompts else {
        return Ok(());
    };
    let known_prompts = prompts::prompt_names();
    for (prompt_name, override_value) in &prompts.overrides {
        if !known_prompts.contains(&prompt_name.as_str()) {
            return Err(other_error(format!(
                "invalid MCP config {}: {prefix}.overrides.{prompt_name} names an unknown prompt",
                path.display()
            )));
        }
        if let RawPromptOverrideValue::Object(object) = override_value {
            if matches!(object.enabled, MaybeSet::Null) {
                return Err(other_error(format!(
                    "invalid MCP config {}: {prefix}.overrides.{prompt_name}.enabled must be omitted instead of null; use null for the whole override entry to delete it",
                    path.display()
                )));
            }
        }
    }
    Ok(())
}

fn validate_raw_providers(
    providers: &HashMap<String, RawProviderConfig>,
    path: &Path,
    prefix: &str,
) -> Result<()> {
    for (module_id, provider) in providers {
        if let MaybeSet::Value(tools) = &provider.tools {
            validate_raw_tool_config(tools, path, &format!("{prefix}.{module_id}.tools"))?;
        }
    }
    Ok(())
}

fn validate_raw_tool_config(tools: &RawToolConfig, path: &Path, prefix: &str) -> Result<()> {
    for (tool_name, override_value) in &tools.overrides {
        if let RawToolOverrideValue::Object(object) = override_value {
            if matches!(object.enabled, MaybeSet::Null) {
                return Err(other_error(format!(
                    "invalid MCP config {}: {prefix}.overrides.{tool_name}.enabled must be omitted instead of null; use null for the whole override entry to delete it",
                    path.display()
                )));
            }
            if matches!(object.description, MaybeSet::Null) {
                return Err(other_error(format!(
                    "invalid MCP config {}: {prefix}.overrides.{tool_name}.description must be omitted instead of null",
                    path.display()
                )));
            }
            if matches!(object.mode, MaybeSet::Null) {
                return Err(other_error(format!(
                    "invalid MCP config {}: {prefix}.overrides.{tool_name}.mode must be omitted instead of null",
                    path.display()
                )));
            }
            if matches!(object.upfront, MaybeSet::Null) {
                return Err(other_error(format!(
                    "invalid MCP config {}: {prefix}.overrides.{tool_name}.upfront must be omitted instead of null; use null for the whole override entry to delete it",
                    path.display()
                )));
            }
        }
    }
    Ok(())
}

impl RawGatewayConfig {
    fn into_parts(self) -> (RawGatewayLayer, HashMap<String, RawGatewayOverlayConfig>) {
        (
            RawGatewayLayer {
                surface_mode: self.surface_mode,
                refresh: self.refresh,
                prompts: self.prompts,
                providers: self.providers,
            },
            self.harness,
        )
    }
}

impl From<RawGatewayOverlayConfig> for RawGatewayLayer {
    fn from(raw: RawGatewayOverlayConfig) -> Self {
        Self {
            surface_mode: raw.surface_mode,
            refresh: raw.refresh,
            prompts: raw.prompts,
            providers: raw.providers,
        }
    }
}

fn matching_harness_layers(
    harness_layers: HashMap<String, RawGatewayOverlayConfig>,
    harness_name: &str,
) -> Vec<RawGatewayLayer> {
    let wanted = harness_name.to_ascii_lowercase();
    let mut matches = harness_layers
        .into_iter()
        .filter(|(name, _)| name.to_ascii_lowercase() == wanted)
        .collect::<Vec<_>>();
    matches.sort_by(|(left, _), (right, _)| left.cmp(right));
    matches
        .into_iter()
        .map(|(_, layer)| RawGatewayLayer::from(layer))
        .collect()
}

fn merge_gateway_config(effective: &mut GatewayConfig, raw: RawGatewayLayer) {
    match raw.surface_mode {
        MaybeSet::Missing => {}
        MaybeSet::Null => effective.surface_mode = SurfaceMode::Full,
        MaybeSet::Value(surface_mode) => effective.surface_mode = surface_mode,
    }
    match raw.refresh {
        MaybeSet::Missing => {}
        MaybeSet::Null => effective.refresh = RefreshMode::OnAttach,
        MaybeSet::Value(refresh) => effective.refresh = refresh,
    }
    match raw.prompts {
        MaybeSet::Missing => {}
        MaybeSet::Null => effective.prompts = PromptConfig::default(),
        MaybeSet::Value(prompts) => merge_prompt_config(&mut effective.prompts, prompts),
    }

    for (module_id, raw_provider) in raw.providers {
        let provider = effective.providers.entry(module_id).or_default();
        match raw_provider.enabled {
            MaybeSet::Missing => {}
            MaybeSet::Null => provider.enabled = None,
            MaybeSet::Value(enabled) => provider.enabled = Some(enabled),
        }
        match raw_provider.namespace {
            MaybeSet::Missing => {}
            MaybeSet::Null => provider.namespace = None,
            MaybeSet::Value(namespace) => provider.namespace = Some(namespace),
        }
        match raw_provider.tools {
            MaybeSet::Missing => {}
            MaybeSet::Null => provider.tools = ToolConfig::default(),
            MaybeSet::Value(tools) => merge_tool_config(&mut provider.tools, tools),
        }
    }
}

fn merge_project_gateway_config(
    effective: &mut GatewayConfig,
    raw: RawGatewayLayer,
    notes: &mut Vec<ProjectNote>,
) {
    match raw.surface_mode {
        MaybeSet::Missing => {}
        MaybeSet::Value(SurfaceMode::Search) => effective.surface_mode = SurfaceMode::Search,
        MaybeSet::Value(SurfaceMode::Full) | MaybeSet::Null => {
            if effective.surface_mode == SurfaceMode::Search {
                note_project_drop(
                    notes,
                    "surfaceMode",
                    "project MCP config cannot widen a search surface back to full",
                );
            }
        }
    }
    if !matches!(raw.refresh, MaybeSet::Missing) {
        note_project_drop(
            notes,
            "refresh",
            "project MCP config cannot weaken user-chosen refresh latency",
        );
    }
    match raw.prompts {
        MaybeSet::Missing => {}
        MaybeSet::Null => effective.prompts = PromptConfig::default(),
        MaybeSet::Value(prompts) => {
            let baseline = effective.prompts.clone();
            merge_project_prompt_config(&mut effective.prompts, &baseline, prompts, notes);
        }
    }

    for (module_id, raw_provider) in raw.providers {
        let baseline = effective.clone();
        {
            let provider = effective.providers.entry(module_id.clone()).or_default();
            match raw_provider.enabled {
                MaybeSet::Missing => {}
                MaybeSet::Value(false) => provider.enabled = Some(false),
                MaybeSet::Value(true) => {
                    if baseline.provider_enabled(&module_id) {
                        provider.enabled = Some(true);
                    } else {
                        note_project_drop(
                    notes,
                            &format!("providers.{module_id}.enabled"),
                            "project MCP config cannot enable a provider disabled by the user baseline",
                        );
                    }
                }
                MaybeSet::Null => {
                    if baseline.provider_enabled(&module_id) {
                        provider.enabled = None;
                    } else {
                        note_project_drop(
                    notes,
                            &format!("providers.{module_id}.enabled"),
                            "project MCP config cannot delete a provider deny from the user baseline",
                        );
                    }
                }
            }
            if !matches!(raw_provider.namespace, MaybeSet::Missing) {
                note_project_drop(
                    notes,
                    &format!("providers.{module_id}.namespace"),
                    "project MCP config cannot rename model-facing tool identities",
                );
            }
        }

        match raw_provider.tools {
            MaybeSet::Missing => {}
            MaybeSet::Null => note_project_drop(
                notes,
                &format!("providers.{module_id}.tools"),
                "project MCP config cannot reset inherited tool policy",
            ),
            MaybeSet::Value(tools) => {
                merge_project_tool_config(effective, &baseline, &module_id, tools, notes);
            }
        }
    }
}

fn merge_prompt_config(effective: &mut PromptConfig, raw: RawPromptConfig) {
    match raw.default_enabled {
        MaybeSet::Missing => {}
        MaybeSet::Null => effective.default_enabled = None,
        MaybeSet::Value(default_enabled) => effective.default_enabled = Some(default_enabled),
    }
    for (prompt_name, override_value) in raw.overrides {
        match override_value {
            RawPromptOverrideValue::Null(()) => {
                effective.overrides.remove(&prompt_name);
            }
            RawPromptOverrideValue::Bool(enabled) => {
                effective.overrides.entry(prompt_name).or_default().enabled = Some(enabled);
            }
            RawPromptOverrideValue::Object(object) => match object.enabled {
                MaybeSet::Missing => {}
                MaybeSet::Null => unreachable!("validated prompt override enabled null"),
                MaybeSet::Value(enabled) => {
                    effective.overrides.entry(prompt_name).or_default().enabled = Some(enabled);
                }
            },
        }
    }
}

fn merge_project_prompt_config(
    effective: &mut PromptConfig,
    baseline: &PromptConfig,
    raw: RawPromptConfig,
    notes: &mut Vec<ProjectNote>,
) {
    let baseline_default_enabled = baseline.default_enabled.unwrap_or(false);
    match raw.default_enabled {
        MaybeSet::Missing => {}
        MaybeSet::Value(false) | MaybeSet::Null => effective.default_enabled = Some(false),
        MaybeSet::Value(true) => {
            if baseline_default_enabled {
                effective.default_enabled = Some(true);
            } else {
                note_project_drop(
                    notes,
                    "prompts.defaultEnabled",
                    "project MCP config cannot enable prompts disabled by the user baseline",
                );
            }
        }
    }

    for (prompt_name, override_value) in raw.overrides {
        let field = format!("prompts.overrides.{prompt_name}");
        let baseline_enabled = baseline.enabled(&prompt_name);
        match override_value {
            RawPromptOverrideValue::Null(()) => {
                let enabled_without_override = effective.default_enabled.unwrap_or(false);
                if baseline_enabled || !enabled_without_override {
                    effective.overrides.remove(&prompt_name);
                } else {
                    note_project_drop(
                        notes,
                        &field,
                        "project MCP config cannot delete a prompt deny from the user baseline",
                    );
                }
            }
            RawPromptOverrideValue::Bool(enabled) => merge_project_prompt_override_enabled(
                effective,
                &field,
                &prompt_name,
                enabled,
                baseline_enabled,
                notes,
            ),
            RawPromptOverrideValue::Object(object) => match object.enabled {
                MaybeSet::Missing => {}
                MaybeSet::Null => unreachable!("validated prompt override enabled null"),
                MaybeSet::Value(enabled) => merge_project_prompt_override_enabled(
                    effective,
                    &format!("{field}.enabled"),
                    &prompt_name,
                    enabled,
                    baseline_enabled,
                    notes,
                ),
            },
        }
    }
}

fn merge_project_prompt_override_enabled(
    effective: &mut PromptConfig,
    field: &str,
    prompt_name: &str,
    enabled: bool,
    baseline_enabled: bool,
    notes: &mut Vec<ProjectNote>,
) {
    if !enabled || baseline_enabled {
        effective
            .overrides
            .entry(prompt_name.to_owned())
            .or_default()
            .enabled = Some(enabled);
    } else {
        note_project_drop(
            notes,
            field,
            "project MCP config cannot enable a prompt disabled by the user baseline",
        );
    }
}

fn merge_tool_config(effective: &mut ToolConfig, raw: RawToolConfig) {
    match raw.default_enabled {
        MaybeSet::Missing => {}
        MaybeSet::Null => effective.default_enabled = None,
        MaybeSet::Value(default_enabled) => effective.default_enabled = Some(default_enabled),
    }
    for (tool_name, override_value) in raw.overrides {
        match override_value {
            RawToolOverrideValue::Null(()) => {
                effective.overrides.remove(&tool_name);
            }
            RawToolOverrideValue::Bool(enabled) => {
                effective.overrides.entry(tool_name).or_default().enabled = Some(enabled);
            }
            RawToolOverrideValue::Object(object) => {
                let override_config = effective.overrides.entry(tool_name).or_default();
                match object.enabled {
                    MaybeSet::Missing => {}
                    MaybeSet::Null => unreachable!("validated object override enabled null"),
                    MaybeSet::Value(enabled) => override_config.enabled = Some(enabled),
                }
                match object.description {
                    MaybeSet::Missing => {}
                    MaybeSet::Null => unreachable!("validated object override description null"),
                    MaybeSet::Value(description) => override_config.description = Some(description),
                }
                match object.mode {
                    MaybeSet::Missing => {}
                    MaybeSet::Null => unreachable!("validated object override mode null"),
                    MaybeSet::Value(mode) => override_config.mode = Some(mode),
                }
                match object.upfront {
                    MaybeSet::Missing => {}
                    MaybeSet::Null => unreachable!("validated object override upfront null"),
                    MaybeSet::Value(upfront) => override_config.upfront = Some(upfront),
                }
            }
        }
    }
}

fn merge_project_tool_config(
    effective: &mut GatewayConfig,
    baseline: &GatewayConfig,
    module_id: &str,
    raw: RawToolConfig,
    notes: &mut Vec<ProjectNote>,
) {
    let baseline_default_enabled = baseline
        .providers
        .get(module_id)
        .map(|provider| provider.tools.default_enabled.unwrap_or(true))
        .unwrap_or(true);
    let provider = effective.providers.entry(module_id.to_owned()).or_default();
    match raw.default_enabled {
        MaybeSet::Missing => {}
        MaybeSet::Value(false) => provider.tools.default_enabled = Some(false),
        MaybeSet::Value(true) => {
            if baseline_default_enabled {
                provider.tools.default_enabled = Some(true);
            } else {
                note_project_drop(
                    notes,
                    &format!("providers.{module_id}.tools.defaultEnabled"),
                    "project MCP config cannot enable tools disabled by the user baseline",
                );
            }
        }
        MaybeSet::Null => {
            if baseline_default_enabled {
                provider.tools.default_enabled = None;
            } else {
                note_project_drop(
                    notes,
                    &format!("providers.{module_id}.tools.defaultEnabled"),
                    "project MCP config cannot delete a default tool deny from the user baseline",
                );
            }
        }
    }

    for (tool_name, override_value) in raw.overrides {
        let field = format!("providers.{module_id}.tools.overrides.{tool_name}");
        let baseline_callable =
            baseline.provider_enabled(module_id) && baseline.tool_enabled(module_id, &tool_name);
        let baseline_mode = baseline.tool_mode(module_id, &tool_name);
        match override_value {
            RawToolOverrideValue::Null(()) => {
                let has_description = provider
                    .tools
                    .overrides
                    .get(&tool_name)
                    .and_then(|override_config| override_config.description.as_ref())
                    .is_some();
                if baseline_callable && !has_description && baseline_mode == ToolMode::Forward {
                    provider.tools.overrides.remove(&tool_name);
                } else {
                    note_project_drop(
                    notes,
                        &field,
                        "project MCP config cannot delete a deny, ack-only mode, or inherited tool description",
                    );
                }
            }
            RawToolOverrideValue::Bool(enabled) => merge_project_override_enabled(
                &mut provider.tools,
                &field,
                &tool_name,
                enabled,
                baseline_callable,
                notes,
            ),
            RawToolOverrideValue::Object(object) => {
                if !matches!(object.description, MaybeSet::Missing) {
                    note_project_drop(
                        notes,
                        &format!("{field}.description"),
                        "project MCP config cannot rewrite model-facing tool descriptions",
                    );
                }
                match object.enabled {
                    MaybeSet::Missing => {}
                    MaybeSet::Null => unreachable!("validated object override enabled null"),
                    MaybeSet::Value(enabled) => merge_project_override_enabled(
                        &mut provider.tools,
                        &format!("{field}.enabled"),
                        &tool_name,
                        enabled,
                        baseline_callable,
                        notes,
                    ),
                }
                match object.mode {
                    MaybeSet::Missing => {}
                    MaybeSet::Null => unreachable!("validated object override mode null"),
                    MaybeSet::Value(mode) => merge_project_override_mode(
                        &mut provider.tools,
                        &format!("{field}.mode"),
                        &tool_name,
                        mode,
                        baseline_mode,
                        notes,
                    ),
                }
                match object.upfront {
                    MaybeSet::Missing => {}
                    MaybeSet::Null => unreachable!("validated object override upfront null"),
                    MaybeSet::Value(upfront) => {
                        provider
                            .tools
                            .overrides
                            .entry(tool_name)
                            .or_default()
                            .upfront = Some(upfront);
                    }
                }
            }
        }
    }
}

fn merge_project_override_enabled(
    effective: &mut ToolConfig,
    field: &str,
    tool_name: &str,
    enabled: bool,
    baseline_callable: bool,
    notes: &mut Vec<ProjectNote>,
) {
    if !enabled || baseline_callable {
        effective
            .overrides
            .entry(tool_name.to_owned())
            .or_default()
            .enabled = Some(enabled);
    } else {
        note_project_drop(
            notes,
            field,
            "project MCP config cannot enable a tool disabled by the user baseline",
        );
    }
}

fn merge_project_override_mode(
    effective: &mut ToolConfig,
    field: &str,
    tool_name: &str,
    mode: ToolMode,
    baseline_mode: ToolMode,
    notes: &mut Vec<ProjectNote>,
) {
    if mode == ToolMode::AckOnly || baseline_mode == ToolMode::Forward {
        effective
            .overrides
            .entry(tool_name.to_owned())
            .or_default()
            .mode = Some(mode);
    } else {
        note_project_drop(
            notes,
            field,
            "project MCP config cannot forward a tool acknowledged by the user baseline",
        );
    }
}

#[derive(Debug, PartialEq, Eq)]
struct ProjectNote {
    field: String,
    reason: &'static str,
}

fn note_project_drop(notes: &mut Vec<ProjectNote>, field: &str, reason: &'static str) {
    notes.push(ProjectNote {
        field: field.to_owned(),
        reason,
    });
}

fn deserialize_maybe_set<'de, D, T>(deserializer: D) -> std::result::Result<MaybeSet<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(|value| match value {
        Some(value) => MaybeSet::Value(value),
        None => MaybeSet::Null,
    })
}

#[cfg(test)]
impl ConfigSnapshot {
    pub(super) fn test_default() -> Self {
        Self {
            project_root: PathBuf::from("/tmp/project"),
            effective: GatewayConfig::default(),
            files: ConfigFileSnapshot {
                user: ConfigFileState {
                    path: PathBuf::from("user-mcp.jsonc"),
                    modified: None,
                    len: None,
                },
                project: ConfigFileState {
                    path: PathBuf::from("project-mcp.jsonc"),
                    modified: None,
                    len: None,
                },
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        desired_session_from_catalog, CatalogEntry, ExecutionMode, ManifestTool, ProviderRole,
        DEFAULT_HARNESS,
    };

    fn parse_test_config(doc: &str) -> RawGatewayConfig {
        parse_gateway_config_doc(doc, Path::new("test-mcp.jsonc")).unwrap()
    }

    fn compose_test_config(
        user_doc: Option<&str>,
        project_doc: Option<&str>,
        harness_name: &str,
    ) -> GatewayConfig {
        compose_gateway_config(
            user_doc.map(parse_test_config),
            project_doc.map(parse_test_config),
            harness_name,
        )
        .0
    }

    #[test]
    fn schema_accepts_legacy_bool_and_object_overrides() {
        let config = compose_test_config(
            Some(
                r#"
                {
                  "version": 1,
                  "providers": {
                    "aft": {
                      "tools": {
                        "defaultEnabled": false,
                        "overrides": {
                          "read": true,
                          "write": { "enabled": false, "description": "curated write", "mode": "ack_only" },
                          "drop_me": null
                        }
                      }
                    }
                  }
                }
                "#,
            ),
            None,
            DEFAULT_HARNESS,
        );

        let tools = &config.providers["aft"].tools;
        assert_eq!(tools.default_enabled, Some(false));
        assert_eq!(tools.overrides["read"].enabled, Some(true));
        assert_eq!(tools.overrides["write"].enabled, Some(false));
        assert_eq!(tools.overrides["write"].mode, Some(ToolMode::AckOnly));
        assert_eq!(
            tools.overrides["write"].description.as_deref(),
            Some("curated write")
        );
        assert!(!tools.overrides.contains_key("drop_me"));
    }

    #[test]
    fn upfront_defaults_are_scoped_to_magic_contexts_five_tools() {
        let config = GatewayConfig::facade_default();
        let expected = [
            "ctx_reduce",
            "ctx_expand",
            "ctx_search",
            "ctx_memory",
            "ctx_note",
        ];
        assert_eq!(DEFAULT_UPFRONT_TOOLS.len(), expected.len());
        for name in expected {
            assert!(config.tool_upfront("magic-context", name), "{name}");
            assert!(!config.tool_upfront("other-provider", name), "{name}");
        }
        for name in [
            "ctx_status",
            "ctx_reduce_extra",
            "reduce",
            "tools_search",
            "",
        ] {
            assert!(!config.tool_upfront("magic-context", name), "{name}");
        }
    }

    #[test]
    fn upfront_tools_cannot_shadow_reserved_search_meta_tools() {
        let config = compose_test_config(
            Some(
                r#"{"version":1,"providers":{"example":{"namespace":"tools","tools":{"overrides":{
                "search":{"upfront":true},"invoke":{"upfront":true},"read":{"upfront":true}
            }}}}}"#,
            ),
            None,
            DEFAULT_HARNESS,
        );
        let entry = CatalogEntry::new(
            "example",
            vec![ProviderRole::ToolProvider {
                tools: ["search", "invoke", "read"]
                    .into_iter()
                    .map(|name| ManifestTool {
                        name: name.to_owned(),
                        description: None,
                        execution_mode: ExecutionMode::Pure,
                        schema: serde_json::json!({"type": "object"}),
                    })
                    .collect(),
                identity_scope: Vec::new(),
                concurrency: subc_protocol::manifest::Concurrency::Serial,
                emits_push: false,
                sub_supervises: false,
            }],
            Vec::new(),
        );
        let desired = desired_session_from_catalog(&config, &[entry]).unwrap();
        let tools = &desired.providers[0].tools;
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].exposed_tool.manifest.name, "tools_read");
        assert!(tools[0].exposed_tool.upfront);
    }

    #[test]
    fn upfront_overrides_compose_across_user_project_and_harness_layers() {
        let user = r#"{
            "version": 1,
            "providers": {"magic-context": {"enabled": true, "tools": {"overrides": {
                "ctx_reduce": {"upfront": false},
                "ctx_expand": {"upfront": false},
                "custom": {"upfront": true}
            }}}},
            "harness": {"mcp:generic": {"providers": {"magic-context": {"tools": {"overrides": {
                "custom": {"upfront": false}
            }}}}}}
        }"#;
        let baseline = compose_test_config(Some(user), None, DEFAULT_HARNESS);
        assert!(!baseline.tool_upfront("magic-context", "ctx_reduce"));
        assert!(!baseline.tool_upfront("magic-context", "custom"));
        let config = compose_test_config(
            Some(user),
            Some(
                r#"{
                "version": 1,
                "providers": {"magic-context": {"tools": {"overrides": {
                    "ctx_reduce": {"upfront": true},
                    "ctx_expand": {"enabled": true},
                    "ctx_search": {"upfront": false},
                    "custom": {"upfront": true}
                }}}},
                "harness": {"mcp:generic": {"providers": {"magic-context": {"tools": {"overrides": {
                    "custom": {"upfront": false}
                }}}}}}
            }"#,
            ),
            DEFAULT_HARNESS,
        );
        assert!(config.tool_upfront("magic-context", "ctx_reduce"));
        assert!(!config.tool_upfront("magic-context", "ctx_expand"));
        assert!(!config.tool_upfront("magic-context", "ctx_search"));
        assert!(!config.tool_upfront("magic-context", "custom"));
    }

    #[test]
    fn upfront_null_uses_whole_override_deletion_not_field_reset() {
        let err = parse_gateway_config_doc(
            r#"{"version":1,"providers":{"magic-context":{"tools":{"overrides":{
                "ctx_reduce":{"upfront":null}
            }}}}}"#,
            Path::new("upfront-null.jsonc"),
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("upfront must be omitted instead of null"),
            "{err}"
        );

        let user = r#"{"version":1,"providers":{"magic-context":{"enabled":true,"tools":{"overrides":{
            "ctx_reduce":{"upfront":false},"custom":{"upfront":true}
        }}}}}"#;
        let deletion = r#"{"version":1,"providers":{"magic-context":{"tools":{"overrides":{
            "ctx_reduce":null,"custom":null
        }}}}}"#;
        let config = compose_test_config(Some(user), Some(deletion), DEFAULT_HARNESS);
        assert!(config.tool_upfront("magic-context", "ctx_reduce"));
        assert!(!config.tool_upfront("magic-context", "custom"));

        let user_with_harness = r#"{"version":1,"providers":{"magic-context":{"tools":{"overrides":{
            "ctx_reduce":{"upfront":false}
        }}}},"harness":{"mcp:generic":{"providers":{"magic-context":{"tools":{"overrides":{
            "ctx_reduce":null
        }}}}}}}"#;
        let config = compose_test_config(Some(user_with_harness), None, DEFAULT_HARNESS);
        assert!(config.tool_upfront("magic-context", "ctx_reduce"));
    }

    #[test]
    fn schema_rejects_reserved_refresh_values_unknown_fields_and_object_nulls() {
        let reserved = parse_gateway_config_doc(
            r#"{ "version": 1, "refresh": "on-hard" }"#,
            Path::new("reserved.jsonc"),
        )
        .unwrap_err()
        .to_string();
        assert!(
            reserved.contains("requires a bust-signal source; not available on the MCP path"),
            "unexpected reserved-refresh error: {reserved}"
        );

        let unknown_field_docs = [
            r#"{ "version": 1, "unknown": true }"#,
            r#"{ "version": 1, "harness": { "mcp:generic": { "unknown": true } } }"#,
            r#"{ "version": 1, "providers": { "aft": { "unknown": true } } }"#,
            r#"{ "version": 1, "providers": { "aft": { "tools": { "unknown": true } } } }"#,
            r#"{ "version": 1, "providers": { "aft": { "tools": { "overrides": { "read": { "unknown": true } } } } } }"#,
            r#"{ "version": 1, "providers": { "aft": { "tools": { "overrides": { "read": { "upfront": true, "unknown": true } } } } } }"#,
        ];
        for doc in unknown_field_docs {
            let err = parse_gateway_config_doc(doc, Path::new("unknown.jsonc"))
                .unwrap_err()
                .to_string();
            assert!(err.contains("unknown field"), "unexpected error: {err}");
        }

        let null_inside_object = parse_gateway_config_doc(
            r#"
            {
              "version": 1,
              "providers": {
                "aft": {
                  "tools": {
                    "overrides": { "read": { "enabled": null } }
                  }
                }
              }
            }
            "#,
            Path::new("null-object.jsonc"),
        )
        .unwrap_err()
        .to_string();
        assert!(
            null_inside_object.contains("enabled must be omitted instead of null"),
            "unexpected object-null error: {null_inside_object}"
        );

        let mode_null = parse_gateway_config_doc(
            r#"
            {
              "version": 1,
              "providers": {
                "aft": {
                  "tools": {
                    "overrides": { "read": { "mode": null } }
                  }
                }
              }
            }
            "#,
            Path::new("mode-null-object.jsonc"),
        )
        .unwrap_err()
        .to_string();
        assert!(
            mode_null.contains("mode must be omitted instead of null"),
            "unexpected mode-null error: {mode_null}"
        );
    }

    #[test]
    fn prompt_policy_rejects_unknown_override_names() {
        let error = parse_gateway_config_doc(
            r#"{
                "version": 1,
                "prompts": { "overrides": { "stats": true } }
            }"#,
            Path::new("prompt-typo.jsonc"),
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("prompts.overrides.stats"), "{error}");
        assert!(error.contains("unknown prompt"), "{error}");
    }

    #[test]
    fn prompt_policy_is_default_hidden_and_project_cannot_widen_user_denies() {
        let default = GatewayConfig::facade_default();
        assert!(default.visible_prompt_names().is_empty());

        let effective = compose_test_config(
            Some(
                r#"{
                    "version": 1,
                    "prompts": {
                        "defaultEnabled": false,
                        "overrides": { "status": false, "wrapup": false }
                    }
                }"#,
            ),
            Some(
                r#"{
                    "version": 1,
                    "prompts": {
                        "defaultEnabled": true,
                        "overrides": { "status": true, "wrapup": { "enabled": true } }
                    }
                }"#,
            ),
            DEFAULT_HARNESS,
        );

        assert!(!effective.prompt_enabled("status"));
        assert!(!effective.prompt_enabled("wrapup"));
        assert!(effective.visible_prompt_names().is_empty());
    }

    #[test]
    fn project_prompt_policy_can_narrow_user_enabled_prompts() {
        let effective = compose_test_config(
            Some(
                r#"{
                    "version": 1,
                    "prompts": { "defaultEnabled": true }
                }"#,
            ),
            Some(
                r#"{
                    "version": 1,
                    "prompts": { "overrides": { "status": false } }
                }"#,
            ),
            DEFAULT_HARNESS,
        );

        assert!(!effective.prompt_enabled("status"));
        assert!(effective.prompt_enabled("wrapup"));
    }

    #[test]
    fn global_harness_sections_compose_and_unknown_harness_is_ignored() {
        let config = compose_test_config(
            Some(
                r#"
                {
                  "version": 1,
                  "surfaceMode": "search",
                  "refresh": "immediate",
                  "providers": {
                    "aft": {
                      "namespace": "global",
                      "tools": {
                        "defaultEnabled": false,
                        "overrides": {
                          "read": false,
                          "write": { "enabled": true, "description": "global write" }
                        }
                      }
                    }
                  },
                  "harness": {
                    "MCP:GENERIC": {
                      "surfaceMode": "full",
                      "refresh": null,
                      "providers": {
                        "aft": {
                          "namespace": "harnessed",
                          "tools": {
                            "defaultEnabled": null,
                            "overrides": {
                              "read": null,
                              "write": { "description": "harness write" }
                            }
                          }
                        }
                      }
                    },
                    "other": {
                      "providers": { "aft": { "enabled": false } }
                    }
                  }
                }
                "#,
            ),
            None,
            "mcp:generic",
        );

        assert_eq!(config.surface_mode, SurfaceMode::Full);
        assert_eq!(config.refresh, RefreshMode::OnAttach);
        assert_eq!(config.provider_namespace("aft"), "harnessed");
        let tools = &config.providers["aft"].tools;
        assert_eq!(tools.default_enabled, None);
        assert!(!tools.overrides.contains_key("read"));
        assert_eq!(tools.overrides["write"].enabled, Some(true));
        assert_eq!(
            tools.overrides["write"].description.as_deref(),
            Some("harness write")
        );
        assert!(config.provider_enabled("aft"));
    }

    #[test]
    fn project_tier_only_narrows_and_preserves_global_model_facing_strings() {
        let config = compose_test_config(
            Some(
                r#"
                {
                  "version": 1,
                  "refresh": "immediate",
                  "providers": {
                    "aft": {
                      "enabled": false,
                      "namespace": "global",
                      "tools": {
                        "defaultEnabled": false,
                        "overrides": {
                          "read": false,
                          "write": { "enabled": true, "description": "global write" }
                        }
                      }
                    }
                  }
                }
                "#,
            ),
            Some(
                r#"
                {
                  "version": 1,
                  "surfaceMode": "search",
                  "refresh": "on-attach",
                  "providers": {
                    "aft": {
                      "enabled": true,
                      "namespace": "project",
                      "tools": {
                        "defaultEnabled": true,
                        "overrides": {
                          "read": null,
                          "grant": true,
                          "write": { "enabled": false, "description": "project write" }
                        }
                      }
                    }
                  }
                }
                "#,
            ),
            DEFAULT_HARNESS,
        );

        assert_eq!(config.surface_mode, SurfaceMode::Search);
        assert_eq!(config.refresh, RefreshMode::Immediate);
        assert!(!config.provider_enabled("aft"));
        assert_eq!(config.provider_namespace("aft"), "global");
        let tools = &config.providers["aft"].tools;
        assert_eq!(tools.default_enabled, Some(false));
        assert_eq!(tools.overrides["read"].enabled, Some(false));
        assert!(!tools.overrides.contains_key("grant"));
        assert_eq!(tools.overrides["write"].enabled, Some(false));
        assert_eq!(
            tools.overrides["write"].description.as_deref(),
            Some("global write")
        );
    }

    #[test]
    fn project_tier_can_narrow_tool_mode_but_cannot_widen_ack_only() {
        let project_narrowed = compose_test_config(
            Some(
                r#"
                {
                  "version": 1,
                  "providers": {
                    "aft": {
                      "tools": { "overrides": { "read": { "mode": "forward" } } }
                    }
                  }
                }
                "#,
            ),
            Some(
                r#"
                {
                  "version": 1,
                  "providers": {
                    "aft": {
                      "tools": { "overrides": { "read": { "mode": "ack_only" } } }
                    }
                  }
                }
                "#,
            ),
            DEFAULT_HARNESS,
        );
        assert_eq!(project_narrowed.tool_mode("aft", "read"), ToolMode::AckOnly);

        let attempted_widen = compose_test_config(
            Some(
                r#"
                {
                  "version": 1,
                  "providers": {
                    "aft": {
                      "tools": { "overrides": { "read": { "mode": "ack_only" } } }
                    }
                  }
                }
                "#,
            ),
            Some(
                r#"
                {
                  "version": 1,
                  "providers": {
                    "aft": {
                      "tools": { "overrides": { "read": { "mode": "forward" } } }
                    }
                  }
                }
                "#,
            ),
            DEFAULT_HARNESS,
        );
        assert_eq!(attempted_widen.tool_mode("aft", "read"), ToolMode::AckOnly);
    }

    #[test]
    fn facade_default_disabled_modules_require_global_enable() {
        let baseline = GatewayConfig::facade_default();
        assert!(!baseline.provider_enabled("magic-context"));
        assert!(!baseline.provider_enabled("llm-runner"));

        let project_only = compose_test_config(
            None,
            Some(r#"{ "version": 1, "providers": { "magic-context": { "enabled": true } } }"#),
            DEFAULT_HARNESS,
        );
        assert!(!project_only.provider_enabled("magic-context"));

        let global_enabled = compose_test_config(
            Some(r#"{ "version": 1, "providers": { "magic-context": { "enabled": true } } }"#),
            None,
            DEFAULT_HARNESS,
        );
        assert!(global_enabled.provider_enabled("magic-context"));
    }

    #[test]
    fn widening_project_values_are_ignored_and_recorded() {
        let user = parse_test_config(
            r#"{
            "version": 1,
            "surfaceMode": "search",
            "providers": {"aft": {"tools": {"overrides": {"read": {"mode": "ack_only"}}}}}
        }"#,
        );
        let project = parse_test_config(
            r#"{
            "version": 1,
            "surfaceMode": "full",
            "harness": {"mcp:generic": {"providers": {
                "aft": {"tools": {"overrides": {"read": {"mode": "forward"}, "write": false}}}
            }}}
        }"#,
        );
        let (config, notes) = compose_gateway_config(Some(user), Some(project), DEFAULT_HARNESS);
        assert_eq!(config.surface_mode, SurfaceMode::Search);
        assert_eq!(config.tool_mode("aft", "read"), ToolMode::AckOnly);
        assert!(!config.tool_enabled("aft", "write"));
        assert_eq!(
            notes,
            [
                ProjectNote {
                    field: "surfaceMode".to_owned(),
                    reason: "project MCP config cannot widen a search surface back to full",
                },
                ProjectNote {
                    field: "providers.aft.tools.overrides.read.mode".to_owned(),
                    reason:
                        "project MCP config cannot forward a tool acknowledged by the user baseline",
                },
            ]
        );
    }
}
