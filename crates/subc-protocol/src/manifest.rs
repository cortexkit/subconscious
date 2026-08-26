//! Capability manifest schema for subc modules.
//!
//! All v1 modules are supervised singletons: one long-lived process per
//! per-user machine. The manifest intentionally has **no `cardinality` field**.
//! subc routes by module kind plus channel, while any finer demultiplexing
//! (for example, AFT's per-project actor map) remains internal to the singleton
//! module.

use std::{collections::HashSet, fmt};

use serde::{de::Error as _, Deserialize, Deserializer, Serialize};
use serde_json::Value;

/// A module's full declared participation in the subc mesh.
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct ModuleManifest {
    pub module_id: String,
    pub module_version: String,
    pub protocol_ver: u8,
    pub trust_tier: TrustTier,
    /// Existing role declarations; capability grammar claims deliberately live in
    /// the separate [`CapabilityDeclarations`] block below.
    pub provides: Vec<ProviderRole>,
    pub consumes: Vec<ConsumerRole>,
    pub bindings: Bindings,
    /// Optional capability-grammar declarations.
    ///
    /// Omitting this block preserves the manifest contract used before capability
    /// grammar was introduced. A present block is static discovery metadata that
    /// the daemon validates before accepting a HELLO.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<CapabilityDeclarations>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<ManifestProvenance>,
}

#[derive(Deserialize)]
struct ModuleManifestWire {
    module_id: String,
    module_version: String,
    protocol_ver: u8,
    trust_tier: TrustTier,
    provides: Vec<ProviderRole>,
    consumes: Vec<ConsumerRole>,
    bindings: Bindings,
    #[serde(default)]
    capabilities: Option<CapabilityDeclarations>,
    #[serde(default)]
    provenance: Option<ManifestProvenance>,
    // `runtime_computed` belongs to --manifest output rather than the retained
    // manifest model. Deserialize it only long enough to enforce that capability
    // declarations cannot be omitted as runtime-varying data.
    #[serde(default)]
    runtime_computed: Option<Value>,
}

impl<'de> Deserialize<'de> for ModuleManifest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ModuleManifestWire::deserialize(deserializer)?;
        validate_runtime_computed(wire.runtime_computed.as_ref(), "runtime_computed")
            .map_err(D::Error::custom)?;
        let manifest = Self {
            module_id: wire.module_id,
            module_version: wire.module_version,
            protocol_ver: wire.protocol_ver,
            trust_tier: wire.trust_tier,
            provides: wire.provides,
            consumes: wire.consumes,
            bindings: wire.bindings,
            capabilities: wire.capabilities,
            provenance: wire.provenance,
        };
        manifest
            .validate_capability_grammar()
            .map_err(D::Error::custom)?;
        Ok(manifest)
    }
}

/// Static, versioned capabilities declared by a module.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CapabilityDeclarations {
    #[serde(default)]
    pub provides: Vec<String>,
    #[serde(default)]
    pub requires: Vec<CapabilityRequirement>,
    #[serde(default)]
    pub must_never_reach: Vec<String>,
}

#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct ManifestProvenance {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_git_sha: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_lock_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wire_crate_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store_schema_version: Option<String>,
}

const MAX_PROVENANCE_VALUE_BYTES: usize = 128;

#[derive(Deserialize)]
struct ManifestProvenanceWire {
    #[serde(default)]
    build_git_sha: Option<String>,
    #[serde(default)]
    build_lock_digest: Option<String>,
    #[serde(default)]
    wire_crate_version: Option<String>,
    #[serde(default)]
    store_schema_version: Option<String>,
}

impl<'de> Deserialize<'de> for ManifestProvenance {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ManifestProvenanceWire::deserialize(deserializer)?;
        let provenance = Self {
            build_git_sha: wire.build_git_sha,
            build_lock_digest: wire.build_lock_digest,
            wire_crate_version: wire.wire_crate_version,
            store_schema_version: wire.store_schema_version,
        };
        provenance.validate().map_err(D::Error::custom)?;
        Ok(provenance)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestProvenanceError {
    field: String,
    value: String,
    reason: &'static str,
}

impl ManifestProvenanceError {
    fn new(field: &str, value: &str, reason: &'static str) -> Self {
        Self {
            field: field.to_string(),
            value: safe_error_value(value),
            reason,
        }
    }

    pub fn field(&self) -> &str {
        &self.field
    }
}

impl fmt::Display for ManifestProvenanceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid manifest provenance: field {} has {} (value {:?})",
            self.field, self.reason, self.value
        )
    }
}

impl std::error::Error for ManifestProvenanceError {}

impl ManifestProvenance {
    pub fn validate(&self) -> Result<(), ManifestProvenanceError> {
        for (field, value) in [
            ("build_git_sha", self.build_git_sha.as_deref()),
            ("build_lock_digest", self.build_lock_digest.as_deref()),
            ("wire_crate_version", self.wire_crate_version.as_deref()),
            ("store_schema_version", self.store_schema_version.as_deref()),
        ] {
            let Some(value) = value else { continue };
            if value.is_empty() {
                return Err(ManifestProvenanceError::new(
                    field,
                    value,
                    "must not be empty",
                ));
            }
            if value.len() > MAX_PROVENANCE_VALUE_BYTES {
                return Err(ManifestProvenanceError::new(
                    field,
                    value,
                    "exceeds the 128-byte maximum",
                ));
            }
            if value.bytes().any(|byte| !(0x20..=0x7e).contains(&byte)) {
                return Err(ManifestProvenanceError::new(
                    field,
                    value,
                    "contains non-printable ASCII",
                ));
            }
        }
        Ok(())
    }
}

/// One capability a module consumes and whether its absence is tolerated.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CapabilityRequirement {
    pub capability: String,
    pub need: CapabilityNeed,
}

/// Closed capability requirement strength vocabulary.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityNeed {
    Required,
    Optional,
}

/// A safe-to-report capability-schema validation failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityGrammarError {
    field: String,
    value: String,
}

impl CapabilityGrammarError {
    fn new(field: impl Into<String>, value: impl AsRef<str>) -> Self {
        Self {
            field: field.into(),
            value: safe_error_value(value.as_ref()),
        }
    }

    /// The precise malformed field path.
    pub fn field(&self) -> &str {
        &self.field
    }

    /// The offending value, redacted when it resembles a credential.
    pub fn value(&self) -> &str {
        &self.value
    }
}

impl fmt::Display for CapabilityGrammarError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid capability grammar: field {} has offending value {:?}",
            self.field, self.value
        )
    }
}

impl std::error::Error for CapabilityGrammarError {}

impl ModuleManifest {
    /// Validate the typed capability block after serde has decoded it.
    pub fn validate_capability_grammar(&self) -> Result<(), CapabilityGrammarError> {
        let Some(capabilities) = &self.capabilities else {
            return Ok(());
        };

        validate_capability_list("capabilities.provides", &capabilities.provides)?;
        validate_requires(&capabilities.requires)?;
        validate_capability_list(
            "capabilities.must_never_reach",
            &capabilities.must_never_reach,
        )
    }
}

/// Validate capability grammar in a standalone manifest JSON value.
///
/// The raw-value form lets HELLO distinguish schema failures from malformed JSON,
/// including an unknown `need` that cannot be represented by [`CapabilityNeed`].
pub fn validate_manifest_capability_grammar(
    manifest: &Value,
) -> Result<(), CapabilityGrammarError> {
    let Some(object) = manifest.as_object() else {
        return Ok(());
    };

    validate_capabilities_value(object.get("capabilities"))?;
    validate_runtime_computed(object.get("runtime_computed"), "runtime_computed")
}

/// Validate capability grammar in a raw HELLO body.
///
/// `runtime_computed` is a top-level sibling in --manifest output. HELLO keeps
/// accepting that sibling only so an attempted dynamic capability declaration is
/// refused explicitly instead of being silently ignored by serde.
pub fn validate_hello_capability_grammar(hello: &Value) -> Result<(), CapabilityGrammarError> {
    let Some(object) = hello.as_object() else {
        return Ok(());
    };
    if let Some(manifest) = object.get("manifest") {
        validate_manifest_capability_grammar(manifest)?;
    }
    validate_runtime_computed(object.get("runtime_computed"), "runtime_computed")
}

/// Return whether `identifier` has the exact `<name>/v<N>` capability spelling.
pub fn is_valid_capability_identifier(identifier: &str) -> bool {
    if identifier.chars().any(char::is_whitespace) {
        return false;
    }
    let Some((name, version)) = identifier.split_once("/v") else {
        return false;
    };
    if name.is_empty() || name.len() > 64 || version.is_empty() {
        return false;
    }

    let name_bytes = name.as_bytes();
    if !name_bytes[0].is_ascii_lowercase()
        || (name.len() > 1
            && !name_bytes[name.len() - 1].is_ascii_lowercase()
            && !name_bytes[name.len() - 1].is_ascii_digit())
        || name_bytes.windows(2).any(|pair| pair == b"--")
    {
        return false;
    }
    if !name_bytes
        .iter()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
    {
        return false;
    }

    if version.len() > 1 && version.starts_with('0')
        || !version.bytes().all(|byte| byte.is_ascii_digit())
    {
        return false;
    }
    matches!(
        version.parse::<u64>(),
        Ok(value) if (1..=u64::from(u32::MAX)).contains(&value)
    )
}

fn validate_capabilities_value(value: Option<&Value>) -> Result<(), CapabilityGrammarError> {
    let Some(value) = value else {
        return Ok(());
    };
    let Some(object) = value.as_object() else {
        return Err(CapabilityGrammarError::new(
            "capabilities",
            value_description(value),
        ));
    };

    for (key, value) in object {
        if !matches!(key.as_str(), "provides" | "requires" | "must_never_reach") {
            return Err(CapabilityGrammarError::new(
                field_child("capabilities", key),
                value_description(value),
            ));
        }
    }

    validate_capability_list_value("capabilities.provides", object.get("provides"))?;
    validate_requires_value(object.get("requires"))?;
    validate_capability_list_value(
        "capabilities.must_never_reach",
        object.get("must_never_reach"),
    )
}

fn validate_capability_list_value(
    field: &str,
    value: Option<&Value>,
) -> Result<(), CapabilityGrammarError> {
    let Some(value) = value else {
        return Ok(());
    };
    let Some(values) = value.as_array() else {
        return Err(CapabilityGrammarError::new(field, value_description(value)));
    };

    let mut seen = HashSet::new();
    for (index, value) in values.iter().enumerate() {
        let field = format!("{field}[{index}]");
        let Some(identifier) = value.as_str() else {
            return Err(CapabilityGrammarError::new(field, value_description(value)));
        };
        validate_capability_identifier(&field, identifier)?;
        if !seen.insert(identifier) {
            return Err(CapabilityGrammarError::new(field, identifier));
        }
    }
    Ok(())
}

fn validate_requires_value(value: Option<&Value>) -> Result<(), CapabilityGrammarError> {
    let Some(value) = value else {
        return Ok(());
    };
    let Some(values) = value.as_array() else {
        return Err(CapabilityGrammarError::new(
            "capabilities.requires",
            value_description(value),
        ));
    };

    let mut seen = HashSet::new();
    for (index, value) in values.iter().enumerate() {
        let entry_field = format!("capabilities.requires[{index}]");
        let Some(object) = value.as_object() else {
            return Err(CapabilityGrammarError::new(
                entry_field,
                value_description(value),
            ));
        };
        for (key, value) in object {
            if !matches!(key.as_str(), "capability" | "need") {
                return Err(CapabilityGrammarError::new(
                    field_child(&entry_field, key),
                    value_description(value),
                ));
            }
        }
        let capability_field = format!("{entry_field}.capability");
        let Some(capability) = object.get("capability").and_then(Value::as_str) else {
            return Err(CapabilityGrammarError::new(
                capability_field,
                object
                    .get("capability")
                    .map_or("<missing>".to_string(), value_description),
            ));
        };
        validate_capability_identifier(&capability_field, capability)?;

        let need_field = format!("{entry_field}.need");
        let Some(need) = object.get("need").and_then(Value::as_str) else {
            return Err(CapabilityGrammarError::new(
                need_field,
                object
                    .get("need")
                    .map_or("<missing>".to_string(), value_description),
            ));
        };
        if !matches!(need, "required" | "optional") {
            return Err(CapabilityGrammarError::new(need_field, need));
        }
        if !seen.insert(capability) {
            return Err(CapabilityGrammarError::new(entry_field, capability));
        }
    }
    Ok(())
}

fn validate_capability_list(field: &str, values: &[String]) -> Result<(), CapabilityGrammarError> {
    let mut seen = HashSet::new();
    for (index, identifier) in values.iter().enumerate() {
        let field = format!("{field}[{index}]");
        validate_capability_identifier(&field, identifier)?;
        if !seen.insert(identifier) {
            return Err(CapabilityGrammarError::new(field, identifier));
        }
    }
    Ok(())
}

fn validate_requires(values: &[CapabilityRequirement]) -> Result<(), CapabilityGrammarError> {
    let mut seen = HashSet::new();
    for (index, requirement) in values.iter().enumerate() {
        let field = format!("capabilities.requires[{index}].capability");
        validate_capability_identifier(&field, &requirement.capability)?;
        if !seen.insert(&requirement.capability) {
            return Err(CapabilityGrammarError::new(
                format!("capabilities.requires[{index}]"),
                &requirement.capability,
            ));
        }
    }
    Ok(())
}

fn validate_capability_identifier(
    field: &str,
    identifier: &str,
) -> Result<(), CapabilityGrammarError> {
    if is_valid_capability_identifier(identifier) {
        Ok(())
    } else {
        Err(CapabilityGrammarError::new(field, identifier))
    }
}

fn validate_runtime_computed(
    value: Option<&Value>,
    field: &str,
) -> Result<(), CapabilityGrammarError> {
    let Some(value) = value else {
        return Ok(());
    };
    let Some(pointers) = value.as_array() else {
        return Err(CapabilityGrammarError::new(field, value_description(value)));
    };

    for (index, pointer) in pointers.iter().enumerate() {
        let field = format!("{field}[{index}]");
        let Some(pointer) = pointer.as_str() else {
            return Err(CapabilityGrammarError::new(
                field,
                value_description(pointer),
            ));
        };
        let Some(tokens) = parse_json_pointer(pointer) else {
            return Err(CapabilityGrammarError::new(field, pointer));
        };
        if tokens.first().is_some_and(|token| token == "capabilities") {
            return Err(CapabilityGrammarError::new(field, pointer));
        }
    }
    Ok(())
}

fn parse_json_pointer(pointer: &str) -> Option<Vec<String>> {
    if pointer.is_empty() {
        return Some(Vec::new());
    }
    let raw_tokens = pointer.strip_prefix('/')?;
    raw_tokens
        .split('/')
        .map(unescape_json_pointer_token)
        .collect()
}

fn unescape_json_pointer_token(token: &str) -> Option<String> {
    let mut output = String::with_capacity(token.len());
    let mut characters = token.chars();
    while let Some(character) = characters.next() {
        if character != '~' {
            output.push(character);
            continue;
        }
        match characters.next()? {
            '0' => output.push('~'),
            '1' => output.push('/'),
            _ => return None,
        }
    }
    Some(output)
}

fn field_child(parent: &str, child: &str) -> String {
    let child = safe_error_value(child);
    format!("{parent}.{child}")
}

fn value_description(value: &Value) -> String {
    match value {
        Value::String(value) => safe_error_value(value),
        Value::Null => "null".to_string(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::Array(_) => "<array>".to_string(),
        Value::Object(_) => "<object>".to_string(),
    }
}

fn safe_error_value(value: &str) -> String {
    let lower = value.to_ascii_lowercase();
    if ["secret", "password", "api_key"]
        .iter()
        .any(|marker| lower.contains(marker))
        || lower.starts_with("sk-")
        || lower.starts_with("akia")
        || lower.starts_with("bearer ")
        || lower.starts_with("token=")
        || lower.starts_with("credential=")
    {
        "<redacted>".to_string()
    } else {
        value.to_string()
    }
}

/// Trust gate applied by subc before routing capabilities.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TrustTier {
    FirstParty,
    Reviewed,
    Untrusted,
}

/// Provider capabilities exposed by a module.
///
/// The role set is closed for protocol v1; unknown role tags fail serde decode.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum ProviderRole {
    ToolProvider {
        tools: Vec<Tool>,
        identity_scope: Vec<IdentityScope>,
        concurrency: Concurrency,
        emits_push: bool,
        sub_supervises: bool,
    },
    PipelineStage {
        stage: PipelineStageKind,
        applies_to: PipelineAppliesTo,
        interface: String,
        declares_frozen_floor: bool,
        needs_signals: Vec<String>,
        conformance_class: String,
    },
    ManagementSurface {
        operations: Vec<ManagementOperation>,
        config_schema: Value,
        observability: Vec<ObservabilitySurface>,
        identity_scope: Vec<IdentityScope>,
        #[serde(default)]
        concurrency: Concurrency,
    },
    InternalService {
        service_id: String,
        transport: InternalTransport,
        agent_facing: bool,
        operations: Vec<String>,
    },
}

/// How a tool's side effects are fenced for durable at-most-once handling.
///
/// Classified on a tool's externally-observable effects, never inferred from
/// the module's concurrency lane:
/// - `Pure`: no observable side effect (reads, searches, cache warming) — safe
///   to re-run after an indeterminate outcome.
/// - `Mutating`: a fenceable external side effect such as a file write — a
///   re-run risks a duplicate effect, so an indeterminate outcome must not
///   auto-retry.
/// - `Unfenceable`: a side effect that cannot be fenced or safely replayed,
///   such as running a shell command — never auto-re-run on an indeterminate
///   outcome.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionMode {
    Pure,
    Mutating,
    Unfenceable,
}

/// Tool-plane capability exposed by a `tool_provider`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Tool {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// How the tool's side effects are fenced for durable at-most-once handling.
    /// Observability + durability metadata only; subc's thin core never acts on
    /// this for routing, scheduling, or concurrency — the module's declared
    /// [`Concurrency`] contract governs delivery.
    pub execution_mode: ExecutionMode,
    pub schema: Value,
}

/// How subc may deliver concurrent in-flight calls to the provider.
///
/// subc records and forwards these semantics unchanged; the dispatcher that
/// enforces them lives in subc-core, kept separate from this frozen manifest
/// contract.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Concurrency {
    /// One in-flight call at a time with strict submission and response order.
    Serial,
    /// Concurrent in-flight calls may span channels, while subc preserves FIFO
    /// submission within each channel; the module schedules internally.
    ModuleManaged,
    /// Fully parallel delivery with no ordering guarantee across or within
    /// channels.
    StatelessParallel,
}

#[allow(clippy::derivable_impls)]
// The default is PINNED BY HISTORY, not chosen as the best value. Before this
// field existed, every ManagementSurface received ModuleManaged delivery (32
// concurrent credits) unconditionally, so an absent-field manifest must resolve
// to exactly that behavior -- any other default (including the fail-closed
// Serial) would convert a daemon upgrade into a silent delivery-semantics
// change for every deployed module. A genuinely-Serial module was ALREADY
// receiving concurrent delivery under pre-field daemons; the field's addition
// is what makes declaring Serial possible at all, so the fix for such a module
// is an explicit declaration, and the daemon logs defaulted registrations so
// the fleet's exposure is readable rather than assumed.
impl Default for Concurrency {
    fn default() -> Self {
        Self::ModuleManaged
    }
}

/// Identity keys that route or scope a call.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum IdentityScope {
    Session,
    Project,
}

/// Proxy-plane stage kind.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum PipelineStageKind {
    Transform,
    Codec,
    Auth,
}

/// Provider/model selector for a pipeline stage. `"*"` denotes wildcard.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct PipelineAppliesTo {
    pub provider: String,
    pub model: String,
}

/// Operation exposed on the management plane.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct ManagementOperation {
    pub name: String,
    pub kind: ManagementOperationKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ManagementOperationKind {
    Query,
    Mutate,
}

/// Observable state exposed on the management plane.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct ObservabilitySurface {
    pub name: String,
    pub kind: ObservabilityKind,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ObservabilityKind {
    Snapshot,
    Stream,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum InternalTransport {
    Bulk,
}

/// Consumer capabilities requested by a module.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum ConsumerRole {
    ToolClient { of: Vec<String> },
    LlmClient { via: String, auth: String },
    ServiceClient { of: Vec<String> },
}

/// External storage, vault, and identity bindings supplied through subc.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Bindings {
    pub storage: StorageBinding,
    pub vault_grants: Vec<VaultGrant>,
    pub identity: IdentityBinding,
}

/// Storage backend supplied by subc; the module owns its schema.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct StorageBinding {
    pub kind: StorageKind,
    pub scope: StorageScope,
    pub owns_schema: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum StorageKind {
    Sqlite,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum StorageScope {
    Project,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct VaultGrant {
    pub secret: String,
    pub reason: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct IdentityBinding {
    pub requires: Vec<IdentityScope>,
    pub optional: Vec<IdentityScope>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn aft_manifest_fixture() -> ModuleManifest {
        ModuleManifest {
            module_id: "aft".to_string(),
            module_version: "0.39.2".to_string(),
            protocol_ver: 1,
            trust_tier: TrustTier::FirstParty,
            provides: vec![ProviderRole::ToolProvider {
                tools: vec![
                    Tool {
                        name: "read".to_string(),
                        description: None,
                        execution_mode: ExecutionMode::Pure,
                        schema: json!({"type": "object"}),
                    },
                    Tool {
                        name: "grep".to_string(),
                        description: None,
                        execution_mode: ExecutionMode::Pure,
                        schema: json!({"type": "object"}),
                    },
                    Tool {
                        name: "outline".to_string(),
                        description: None,
                        execution_mode: ExecutionMode::Pure,
                        schema: json!({"type": "object"}),
                    },
                    Tool {
                        name: "semantic_search".to_string(),
                        description: None,
                        execution_mode: ExecutionMode::Pure,
                        schema: json!({"type": "object"}),
                    },
                    Tool {
                        name: "edit".to_string(),
                        description: None,
                        execution_mode: ExecutionMode::Mutating,
                        schema: json!({"type": "object"}),
                    },
                    Tool {
                        name: "write".to_string(),
                        description: None,
                        execution_mode: ExecutionMode::Mutating,
                        schema: json!({"type": "object"}),
                    },
                    Tool {
                        name: "bash".to_string(),
                        description: None,
                        execution_mode: ExecutionMode::Unfenceable,
                        schema: json!({"type": "object"}),
                    },
                ],
                identity_scope: vec![IdentityScope::Session, IdentityScope::Project],
                concurrency: Concurrency::ModuleManaged,
                emits_push: true,
                sub_supervises: true,
            }],
            consumes: vec![ConsumerRole::ServiceClient {
                of: vec!["embedding.v2".to_string()],
            }],
            bindings: Bindings {
                storage: StorageBinding {
                    kind: StorageKind::Sqlite,
                    scope: StorageScope::Project,
                    owns_schema: true,
                },
                vault_grants: vec![VaultGrant {
                    secret: "provider_api_key".to_string(),
                    reason: "cortexkit_native auth".to_string(),
                }],
                identity: IdentityBinding {
                    requires: vec![IdentityScope::Project],
                    optional: vec![IdentityScope::Session],
                },
            },
            capabilities: None,
            provenance: None,
        }
    }

    #[test]
    fn serde_round_trips_representative_manifest() {
        let manifest = aft_manifest_fixture();
        let serialized = serde_json::to_string_pretty(&manifest).unwrap();
        let decoded: ModuleManifest = serde_json::from_str(&serialized).unwrap();

        assert_eq!(manifest, decoded);
    }

    #[test]
    fn aft_manifest_fixture_matches_v1_contract() {
        let manifest = aft_manifest_fixture();

        assert_eq!(manifest.module_id, "aft");
        let ProviderRole::ToolProvider {
            tools,
            identity_scope,
            concurrency,
            emits_push,
            sub_supervises,
        } = &manifest.provides[0]
        else {
            panic!("AFT fixture must expose one tool_provider role");
        };

        assert_eq!(*concurrency, Concurrency::ModuleManaged);
        assert!(*emits_push);
        assert!(*sub_supervises);
        assert_eq!(
            identity_scope,
            &vec![IdentityScope::Session, IdentityScope::Project]
        );
        assert_eq!(
            tools
                .iter()
                .map(|tool| (tool.name.as_str(), tool.execution_mode))
                .collect::<Vec<_>>(),
            vec![
                ("read", ExecutionMode::Pure),
                ("grep", ExecutionMode::Pure),
                ("outline", ExecutionMode::Pure),
                ("semantic_search", ExecutionMode::Pure),
                ("edit", ExecutionMode::Mutating),
                ("write", ExecutionMode::Mutating),
                ("bash", ExecutionMode::Unfenceable),
            ]
        );
    }

    #[test]
    fn tool_provider_role_tag_serializes_as_snake_case() {
        let manifest = aft_manifest_fixture();
        let value = serde_json::to_value(&manifest).unwrap();

        assert_eq!(value["provides"][0]["role"], "tool_provider");
    }

    #[test]
    fn manifest_without_capabilities_preserves_the_existing_wire_shape() {
        let manifest = aft_manifest_fixture();
        let encoded = serde_json::to_value(&manifest).expect("manifest serializes");
        assert!(encoded.get("capabilities").is_none());

        let decoded: ModuleManifest =
            serde_json::from_value(encoded).expect("legacy manifest parses");
        assert_eq!(decoded.capabilities, None);
    }

    #[test]
    fn capability_identifier_lexical_grammar_accepts_only_pinned_forms() {
        for identifier in [
            "a/v1",
            "credentials-provider/v1",
            "a1-b2/v4294967295",
            "a123456789012345678901234567890123456789012345678901234567890123/v1",
        ] {
            assert!(
                is_valid_capability_identifier(identifier),
                "identifier must be accepted: {identifier}"
            );
        }

        for identifier in [
            "credentials-Provider/v1",
            "credentials-provider/v01",
            "credentials-provider-/v1",
            "credentials--provider/v1",
            "Credentials-provider/v1",
            "credentials-provider/1",
            "credentials provider/v1",
            "credentials-provider/v0",
            "credentials-provider/v4294967296",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/v1",
        ] {
            assert!(
                !is_valid_capability_identifier(identifier),
                "identifier must be rejected: {identifier}"
            );
        }
    }

    #[test]
    fn capability_grammar_errors_redact_secret_shaped_values() {
        let error = validate_manifest_capability_grammar(&json!({
            "capabilities": { "provides": ["sk-secret-value/v0"] }
        }))
        .expect_err("secret-shaped capability identifier is malformed");
        assert_eq!(error.field(), "capabilities.provides[0]");
        assert_eq!(error.value(), "<redacted>");
        assert!(!error.to_string().contains("sk-secret-value"));
    }
}
