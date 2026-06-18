//! Capability manifest schema for subc modules.
//!
//! All v1 modules are supervised singletons: one long-lived process per
//! per-user machine. The manifest intentionally has **no `cardinality` field**.
//! subc routes by module kind plus channel, while any finer demultiplexing
//! (for example, AFT's per-project actor map) remains internal to the singleton
//! module.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

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

/// Tool-plane capability exposed by a `tool_provider`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Tool {
    pub name: String,
    /// Observability/client metadata only. subc's thin core never acts on this
    /// bit for routing, scheduling, or concurrency; the module's declared
    /// [`Concurrency`] contract governs delivery.
    pub mutates: bool,
    pub schema: Value,
}

/// How subc may deliver concurrent in-flight calls to the provider.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Concurrency {
    Serial,
    ModuleManaged,
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

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct CircuitBreaker {
    pub identical_failures: u32,
}

/// External resources and identity/config bindings supplied through subc.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Bindings {
    pub storage: StorageBinding,
    pub config: ConfigBinding,
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

/// Layered config transport. subc stores tiered raw documents and transports
/// literal token references; modules merge and expand them at use time.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct ConfigBinding {
    pub source: ConfigSource,
    pub tiers: Vec<String>,
    pub expansion: BTreeMap<String, Vec<TokenExpansion>>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ConfigSource {
    SubcMediated,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TokenExpansion {
    Env,
    File,
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
        let expansion = BTreeMap::from([
            (
                "user".to_string(),
                vec![TokenExpansion::Env, TokenExpansion::File],
            ),
            ("project".to_string(), vec![]),
        ]);

        ModuleManifest {
            module_id: "aft".to_string(),
            module_version: "0.39.2".to_string(),
            protocol_ver: 1,
            trust_tier: TrustTier::FirstParty,
            provides: vec![ProviderRole::ToolProvider {
                tools: vec![
                    Tool {
                        name: "read".to_string(),
                        mutates: false,
                        schema: json!({"type": "object"}),
                    },
                    Tool {
                        name: "grep".to_string(),
                        mutates: false,
                        schema: json!({"type": "object"}),
                    },
                    Tool {
                        name: "outline".to_string(),
                        mutates: false,
                        schema: json!({"type": "object"}),
                    },
                    Tool {
                        name: "semantic_search".to_string(),
                        mutates: false,
                        schema: json!({"type": "object"}),
                    },
                    Tool {
                        name: "edit".to_string(),
                        mutates: true,
                        schema: json!({"type": "object"}),
                    },
                    Tool {
                        name: "write".to_string(),
                        mutates: true,
                        schema: json!({"type": "object"}),
                    },
                    Tool {
                        name: "bash".to_string(),
                        mutates: false,
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
                config: ConfigBinding {
                    source: ConfigSource::SubcMediated,
                    tiers: vec!["user".to_string(), "project".to_string()],
                    expansion,
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
                .map(|tool| (tool.name.as_str(), tool.mutates))
                .collect::<Vec<_>>(),
            vec![
                ("read", false),
                ("grep", false),
                ("outline", false),
                ("semantic_search", false),
                ("edit", true),
                ("write", true),
                ("bash", false),
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
