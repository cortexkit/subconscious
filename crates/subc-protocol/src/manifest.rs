//! Capability manifest schema for subc modules.
//!
//! All v1 modules are supervised singletons: one long-lived process per
//! per-user machine. The manifest intentionally has **no `cardinality` field**.
//! subc routes by module kind plus channel, while any finer demultiplexing
//! (for example, AFT's per-project actor map) remains internal to the singleton
//! module.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A module's full declared participation in the subc mesh.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct ModuleManifest {
    pub module_id: String,
    pub module_version: String,
    pub protocol_ver: u8,
    pub trust_tier: TrustTier,
    pub provides: Vec<ProviderRole>,
    pub consumes: Vec<ConsumerRole>,
    pub scheduled_tasks: Vec<ScheduledTask>,
    pub bindings: Bindings,
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

/// Scheduler-owned task declaration. The runner module executes the loop; subc
/// owns eligibility checks and the lease.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct ScheduledTask {
    pub task_id: String,
    pub eligibility: TaskEligibility,
    pub lease_scope: LeaseScope,
    pub renews_during_calls: bool,
    pub toolset: Vec<String>,
    pub model_policy: ModelPolicy,
    pub step_cap: u32,
    pub circuit_breaker: CircuitBreaker,
}

/// Time/window gates for a scheduled task. Values are serialized policy strings
/// (for example, durations or cron/window expressions) owned by the scheduler.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct TaskEligibility {
    pub cooldown: String,
    pub window: String,
}

/// Scope at which subc enforces one active scheduler lease.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum LeaseScope {
    Project,
}

/// Model selection policy for the LLM-runner that executes a scheduled task.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct ModelPolicy {
    pub tier: String,
    pub fallback_chain: Vec<String>,
}

/// Declared trip threshold for a scheduled task's circuit breaker: stop after this
/// many IDENTICAL consecutive failures.
///
/// SCOPE, because the name invites a wider reading than the field supports. The
/// alarm condition here is "this failure looks like the last one", so it detects a
/// task stuck failing the SAME way and is silent on a task failing MANY DIFFERENT
/// ways -- and it goes quieter the more varied the failures become, which is often
/// the more alarming case. A module treating this as its only stop condition will
/// find it mutest during the messiest outage. Pair it with a signal that counts
/// failures regardless of their kind.
///
/// The daemon carries this field and does not act on it: enforcement belongs to the
/// module running the task, since only it can compare two failures for identity.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct CircuitBreaker {
    pub identical_failures: u32,
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
            scheduled_tasks: vec![],
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
}
