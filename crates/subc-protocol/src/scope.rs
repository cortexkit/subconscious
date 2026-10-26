//! Scope records: owned identity records the daemon holds for sessions.
//!
//! A scope is identified by `(owner, ref)`. The owner is the module whose own
//! registered connection synced it, never a value in the request, and the
//! `ref` is an opaque string unique within that owner only. The design is
//! `docs/designs/daemon-scopes.md`; the wire shapes here are the owner-facing
//! half of it (`scope.sync`, `scope.apply`, `scope.describe`).
//!
//! Every record type refuses unknown fields. A field this daemon does not know
//! may be one that narrows authority in a later version (a carrier's target
//! list, say), and silently dropping it would widen what the scope grants, so
//! an owner sending one is told its body is malformed instead.

use serde::{Deserialize, Serialize};

use crate::Principal;

// Scope principals are authority-bearing input: an unrecognized constraint must
// not silently widen a grant. Principal elsewhere is a forward-compatible caller
// fact, so keep its general decoder lenient and enforce this only on scope input.
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum ScopePrincipal {
    Reserved { module_id: String },
    Direct {},
    Unverified {},
}

impl From<ScopePrincipal> for Principal {
    fn from(value: ScopePrincipal) -> Self {
        match value {
            ScopePrincipal::Reserved { module_id } => Self::Reserved { module_id },
            ScopePrincipal::Direct {} => Self::Direct,
            ScopePrincipal::Unverified {} => Self::Unverified,
        }
    }
}

fn deserialize_scope_principal<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Principal, D::Error> {
    ScopePrincipal::deserialize(deserializer).map(Into::into)
}

fn deserialize_scope_principals<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<Principal>, D::Error> {
    Vec::<ScopePrincipal>::deserialize(deserializer)
        .map(|principals| principals.into_iter().map(Into::into).collect())
}

/// A scope bind-proof hash is the lowercase hexadecimal SHA-256 digest.
pub fn validate_bind_proof_sha256(hash: &str) -> Result<(), &'static str> {
    if hash.len() == 64
        && hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        Ok(())
    } else {
        Err("bind_proof_sha256 must be exactly 64 lowercase hexadecimal characters")
    }
}

fn deserialize_bind_proof_sha256<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    let hash = Option::<String>::deserialize(deserializer)?;
    if let Some(hash) = &hash {
        validate_bind_proof_sha256(hash).map_err(serde::de::Error::custom)?;
    }
    Ok(hash)
}

/// The `server.describe` capability a daemon advertises when it admits routes
/// under scopes. A carrier that needs a scoped route and does not see it fails
/// the call (`scope_unsupported`) instead of opening an unscoped route.
pub const CAP_SCOPES_V1: &str = "scopes/v1";

/// The `server.describe` capability a daemon advertises when it checks a
/// `route.open`'s `role_versions` and forwards them on the module's bind. A
/// daemon without it drops the field silently, so a consumer that relies on
/// the provider seeing its role versions checks for this first.
pub const CAP_ROUTE_ROLE_VERSIONS_V1: &str = "route-role-versions/v1";

/// Module-to-subc op that registers an owner's full scope set.
pub const SCOPE_SYNC_OP: &str = "scope.sync";
/// Module-to-subc op that upserts or ends scopes without replacing the owner's
/// full scope set.
pub const SCOPE_APPLY_OP: &str = "scope.apply";
/// Module-to-subc op that reads one scope's current state.
pub const SCOPE_DESCRIBE_OP: &str = "scope.describe";

/// Most live scopes one owner may hold. A sync naming more is refused whole.
pub const MAX_LIVE_SCOPES_PER_OWNER: usize = 10_000;
/// Most bytes one scope's `attributes` may take, measured as compact JSON. A
/// sync carrying a larger record is refused whole.
pub const MAX_SCOPE_ATTRIBUTE_BYTES: usize = 4 * 1024;
/// Most ended scopes the daemon remembers per owner. The oldest is forgotten
/// first, and reaching the bound never refuses a sync.
pub const MAX_SCOPE_TOMBSTONES_PER_OWNER: usize = 1_000;
/// Most modules one targeted carrier entry may list.
pub const MAX_CARRIER_TARGETS: usize = 16;
/// Most milliseconds a new scope epoch's deadline may be ahead of the daemon's
/// current Unix wall clock: 24 hours.
pub const MAX_SCOPE_EXPIRY_AHEAD_MS: u64 = 86_400_000;

/// What a scope stands for. Closed, and fixed for the life of one
/// `scope_epoch`: a different kind needs a new epoch, which ends the old scope.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ScopeKind {
    Head,
    Worker,
    Ephemeral,
}

/// A link from a scope to another scope, pinned to that scope's session by its
/// epoch.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct ScopeParent {
    #[serde(deserialize_with = "deserialize_scope_principal")]
    pub owner: Principal,
    #[serde(rename = "ref")]
    pub scope_ref: String,
    pub scope_epoch: u64,
}

impl ScopeParent {
    pub fn new(owner: Principal, scope_ref: impl Into<String>, scope_epoch: u64) -> Self {
        Self {
            owner,
            scope_ref: scope_ref.into(),
            scope_epoch,
        }
    }
}

/// Who, besides the owner, may open routes under a scope.
///
/// `targets` absent means the carrier may open to any module. Present, it
/// names the only module ids the carrier may open to, and must hold between 1
/// and [`MAX_CARRIER_TARGETS`] entries: an empty list is refused rather than
/// read as either "none" or "all".
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct ScopeCarrier {
    #[serde(deserialize_with = "deserialize_scope_principal")]
    pub principal: Principal,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub targets: Option<Vec<String>>,
}

impl ScopeCarrier {
    pub fn new(principal: Principal) -> Self {
        Self {
            principal,
            targets: None,
        }
    }

    #[must_use]
    pub fn with_targets(mut self, targets: Option<Vec<String>>) -> Self {
        self.targets = targets;
        self
    }
}

/// Declaring this in a manifest's `capabilities.provides` promises that the
/// module recognises a scope carrying `flow_id` and applies flow behaviour:
/// it never treats the flow as its owner agent.
/// During migration, either this capability or
/// [`crate::call_mode::CALL_MODES_CAPABILITY`] admits flow scopes.
pub const FLOW_SCOPES_CAPABILITY: &str = "flow-scopes/v1";

/// Declaring this capability promises that a reader accepts the installed flow
/// identity fields in scope stamps. It does not replace flow-scope admission.
pub const FLOW_IDENTITY_SCOPES_CAPABILITY: &str = "flow-identity-scopes/v1";

/// Declaring this in a manifest's `capabilities.provides` promises that the
/// module recognises `run_id` as one agent run and does not exercise the agent's
/// delegated authority under that scope.
/// During migration it admits non-delegating run scopes; delegating runs require
/// [`crate::call_mode::CALL_MODES_CAPABILITY`] instead.
pub const AGENT_RUN_SCOPES_CAPABILITY: &str = "agent-run-scopes/v1";

/// Declaring this in a manifest's `capabilities.provides` promises that the
/// module recognises `on_behalf_of_agent` as attribution only, never authority.
/// The daemon also advertises this capability to scope-authority owners.
pub const SCOPE_ON_BEHALF_OF_CAPABILITY: &str = "scope-on-behalf-of/v1";

/// A *worker run* is a codemode run scope with `run_id` and
/// `on_behalf_of_agent` but no `agent_id`: it works for an agent without that
/// agent's authority. Declaring this in a manifest's `capabilities.provides`
/// promises that the module handles a codemode bind carrying `run_id` and no
/// `agent_id` as attribution only, and never acts as, consents for, or hands out
/// per-agent handles of the attributed agent. A worker run binds only to a
/// target declaring it; `call-modes/v1` and `agent-run-scopes/v1` alone do not
/// admit one, because those providers assume a run carries its agent's
/// identity. A declarer receives `call_mode: codemode` and the stamp's
/// `on_behalf_of_agent` on a worker-run bind even without
/// [`SCOPE_ON_BEHALF_OF_CAPABILITY`]. The daemon also advertises this capability
/// so owners register worker runs only with a daemon that accepts them.
pub const WORKER_RUNS_CAPABILITY: &str = "worker-runs/v1";

/// The daemon can answer whether a module acknowledged a bind under an owned scope epoch.
pub const SCOPE_SERVED_BY_CAPABILITY: &str = "scope-served-by/v1";

/// The authority-controlled attributes the daemon copies into scope stamps,
/// subject to each field's target capability gate.
/// Only an owner module named in the daemon config's `scope_authority_owners`
/// list (by default the module that owns agent sessions) may set them; a scope
/// owned by any other module must leave them empty.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct ScopeAttributes {
    /// The agent the scope's session belongs to. Identity, never permission.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// Attribution only, never authority. A provider may use this to choose
    /// identity-scoped presentation (bot choice, read budgets, bound
    /// repositories) and for logging. It must never treat this as authority to
    /// act as that agent, hand out the agent's per-agent handles or consent on
    /// its strength, or treat it as `agent_id`.
    ///
    /// Set only by a scope-authority owner and validated with `agent_id`'s
    /// token rule. Mutually exclusive with `agent_id`, `delegates` and `flow_id`.
    /// With `run_id` and no `agent_id` it marks a worker run (see
    /// [`WORKER_RUNS_CAPABILITY`]). Adding, changing or removing this within an
    /// epoch increments the content version and closes routes with
    /// `scope_delegation_changed`. Bind stamps include it only for targets
    /// providing [`SCOPE_ON_BEHALF_OF_CAPABILITY`], except that a worker run's
    /// bind, which reaches only a target providing [`WORKER_RUNS_CAPABILITY`],
    /// always includes it. Describe replies include it only for callers
    /// providing [`SCOPE_ON_BEHALF_OF_CAPABILITY`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_behalf_of_agent: Option<String>,
    /// Whether a provider may act as `agent_id`. Refused without `agent_id`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub delegates: bool,
    /// This scope belongs to the named flow, an automated workflow run on an
    /// agent's behalf. Set only by an authority owner (see above), validated
    /// with [`validate_flow_id`], and stamped verbatim. It needs neither
    /// `agent_id` nor `delegates`. Providers treat a module other than the
    /// owner that opens a route under a flow scope as the flow's carrier.
    ///
    /// Like an `agent_id` change, changing this within the same scope epoch is
    /// accepted: the scope's content version increases (the number providers
    /// compare to notice a change), and every route under the scope is closed
    /// with the reason `scope_delegation_changed`, so no live route keeps the
    /// old identity.
    /// A target must provide [`FLOW_SCOPES_CAPABILITY`] or
    /// [`crate::call_mode::CALL_MODES_CAPABILITY`] before the daemon may send a
    /// bind stamped with this field. Decoding it alone is not enough:
    /// the target must also apply flow behaviour instead of agent behaviour.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flow_id: Option<String>,
    /// The approved installed version, from 1 through 2^53 - 1. Together with
    /// `flow_code_hash` and `flow_install_id`, requires `flow_id` and is immutable
    /// within an epoch (`scope_flow_identity_changed`). Stamped only for readers
    /// providing [`FLOW_IDENTITY_SCOPES_CAPABILITY`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flow_version: Option<u64>,
    /// SHA-256 of the approved flow code: exactly 64 lowercase hex characters.
    /// Present together with `flow_version` and `flow_install_id`, under the same
    /// authority, immutability and capability rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flow_code_hash: Option<String>,
    /// Opaque installed-flow token, validated with [`validate_flow_install_id`].
    /// Present together with `flow_version` and `flow_code_hash`, under the same
    /// authority, immutability and capability rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flow_install_id: Option<String>,
    /// Identifies one run: an agent run on behalf of `agent_id`, or a worker run
    /// attributed to `on_behalf_of_agent` without that agent's authority. A
    /// record with `run_id` carries exactly one of the two, forbids `flow_id`,
    /// and uses [`validate_run_id`]'s token rule.
    /// Adding, changing or removing it within an epoch increments the scope's
    /// content version and closes routes under the scope with
    /// `scope_delegation_changed`. The daemon sends an agent run's bind only to
    /// a target module providing [`AGENT_RUN_SCOPES_CAPABILITY`] or
    /// [`crate::call_mode::CALL_MODES_CAPABILITY`]. A delegating run scope is
    /// accepted as a record, but binds only to a target providing the latter.
    /// A worker run binds only to a target providing [`WORKER_RUNS_CAPABILITY`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
}

impl ScopeAttributes {
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_agent_id(mut self, agent_id: Option<String>) -> Self {
        self.agent_id = agent_id;
        self
    }

    #[must_use]
    pub fn with_on_behalf_of_agent(mut self, on_behalf_of_agent: Option<String>) -> Self {
        self.on_behalf_of_agent = on_behalf_of_agent;
        self
    }

    #[must_use]
    pub fn with_delegates(mut self, delegates: bool) -> Self {
        self.delegates = delegates;
        self
    }

    #[must_use]
    pub fn with_flow_id(mut self, flow_id: Option<String>) -> Self {
        self.flow_id = flow_id;
        self
    }

    #[must_use]
    pub fn with_flow_version(mut self, flow_version: Option<u64>) -> Self {
        self.flow_version = flow_version;
        self
    }

    #[must_use]
    pub fn with_flow_code_hash(mut self, flow_code_hash: Option<String>) -> Self {
        self.flow_code_hash = flow_code_hash;
        self
    }

    #[must_use]
    pub fn with_flow_install_id(mut self, flow_install_id: Option<String>) -> Self {
        self.flow_install_id = flow_install_id;
        self
    }

    #[must_use]
    pub fn with_run_id(mut self, run_id: Option<String>) -> Self {
        self.run_id = run_id;
        self
    }

    pub fn is_empty(&self) -> bool {
        self.agent_id.is_none()
            && self.on_behalf_of_agent.is_none()
            && !self.delegates
            && self.flow_id.is_none()
            && self.flow_version.is_none()
            && self.flow_code_hash.is_none()
            && self.flow_install_id.is_none()
            && self.run_id.is_none()
    }
}

/// Check a flow id using the shared opaque-token rule: 1–256 printable,
/// non-space ASCII bytes. Errors name `flow_id`. Scope refs remain opaque and
/// are not subject to this token rule.
pub fn validate_flow_id(flow_id: &str) -> Result<(), crate::tool_call::OpaqueFieldError> {
    crate::tool_call::validate_opaque_field("flow_id", flow_id)
}

/// Check an installed flow version: 1 through 9007199254740991 (2^53 - 1), the
/// largest integer every JSON reader, including JavaScript, decodes exactly.
pub fn validate_flow_version(flow_version: u64) -> Result<(), &'static str> {
    if !(1..=9_007_199_254_740_991).contains(&flow_version) {
        return Err("flow_version must be from 1 to 9007199254740991");
    }
    Ok(())
}

/// Check the approved flow code's SHA-256. Errors name `flow_code_hash`.
pub fn validate_flow_code_hash(flow_code_hash: &str) -> Result<(), &'static str> {
    if flow_code_hash.len() != 64
        || !flow_code_hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err("flow_code_hash must contain exactly 64 lowercase hex characters");
    }
    Ok(())
}

/// Check an installed flow token using the same opaque-token rule as `flow_id`.
/// Errors name `flow_install_id`.
pub fn validate_flow_install_id(
    flow_install_id: &str,
) -> Result<(), crate::tool_call::OpaqueFieldError> {
    crate::tool_call::validate_opaque_field("flow_install_id", flow_install_id)
}

/// Check a run id using the shared opaque-token rule: 1–256 ASCII bytes in
/// `0x21`–`0x7E`. Errors name `run_id`.
pub fn validate_run_id(run_id: &str) -> Result<(), crate::tool_call::OpaqueFieldError> {
    crate::tool_call::validate_opaque_field("run_id", run_id)
}

/// Check attribution using the agent token rule: 1–256 printable, non-space
/// ASCII bytes. Errors name `on_behalf_of_agent`.
pub fn validate_on_behalf_of_agent(agent: &str) -> Result<(), crate::tool_call::OpaqueFieldError> {
    crate::tool_call::validate_opaque_field("on_behalf_of_agent", agent)
}

/// Capability declaring that a provider accepts session folder and project stamps.
pub const SCOPE_WORKDIR_CAPABILITY: &str = "scope-workdir/v1";

/// Check an Entorhinal project id without interpreting its opaque contents.
pub fn validate_project_id(value: &str) -> Result<(), &'static str> {
    if value.is_empty() || value.len() > 128 {
        return Err("project_id must contain 1 to 128 bytes");
    }
    if !value.bytes().all(|byte| (0x21..=0x7e).contains(&byte)) {
        return Err("project_id must be printable ASCII without whitespace");
    }
    Ok(())
}

/// One scope as its owner registers it in `scope.sync` or `scope.apply`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct ScopeRecord {
    #[serde(rename = "ref")]
    pub scope_ref: String,
    /// Owner-supplied session number. The owner keeps it with its own record of
    /// the session, re-sends the same value for the same session after any
    /// restart, and uses a higher one when it reuses the ref for a new session.
    pub scope_epoch: u64,
    pub kind: ScopeKind,
    /// Absolute session folder, canonicalized by the daemon and fixed per epoch.
    /// Attribution only: this grants no authority.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workdir: Option<String>,
    /// Opaque Entorhinal project id, fixed per epoch and never authority-gated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    /// Only the holder of the scope's bind proof can bind it when this hash is
    /// present. Without a hash, admission is unchanged and proofs are ignored.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_bind_proof_sha256"
    )]
    pub bind_proof_sha256: Option<String>,
    /// Refuse an epoch replacement or bind-proof change while routes are bound
    /// to the live epoch, including binds admitted but not yet committed, or
    /// connected presence holders remain. Send only after capability discovery.
    /// This is a request condition, not a stored scope attribute.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub replace_only_if_idle: bool,
    /// Absolute Unix wall-clock deadline in milliseconds. It cannot be added,
    /// changed or removed within this scope epoch; absent means no deadline.
    /// This is not an authority attribute.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<ScopeParent>,
    /// Principals, other than the owner, allowed to register child scopes under
    /// this one. Listing a principal here grants it nothing else.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "deserialize_scope_principals"
    )]
    pub child_owners: Vec<Principal>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub carriers: Vec<ScopeCarrier>,
    #[serde(default, skip_serializing_if = "ScopeAttributes::is_empty")]
    pub attributes: ScopeAttributes,
}

impl ScopeRecord {
    pub fn new(scope_ref: impl Into<String>, scope_epoch: u64, kind: ScopeKind) -> Self {
        Self {
            scope_ref: scope_ref.into(),
            scope_epoch,
            kind,
            workdir: None,
            project_id: None,
            bind_proof_sha256: None,
            replace_only_if_idle: false,
            expires_at_ms: None,
            parent: None,
            child_owners: Vec::new(),
            carriers: Vec::new(),
            attributes: ScopeAttributes::default(),
        }
    }

    #[must_use]
    pub fn with_workdir(mut self, workdir: Option<String>) -> Self {
        self.workdir = workdir;
        self
    }

    #[must_use]
    pub fn with_project_id(mut self, project_id: Option<String>) -> Self {
        self.project_id = project_id;
        self
    }

    #[must_use]
    pub fn with_bind_proof_sha256(mut self, hash: Option<String>) -> Self {
        self.bind_proof_sha256 = hash;
        self
    }

    #[must_use]
    pub fn with_replace_only_if_idle(mut self, replace_only_if_idle: bool) -> Self {
        self.replace_only_if_idle = replace_only_if_idle;
        self
    }

    #[must_use]
    pub fn with_expires_at_ms(mut self, expires_at_ms: Option<u64>) -> Self {
        self.expires_at_ms = expires_at_ms;
        self
    }

    #[must_use]
    pub fn with_parent(mut self, parent: Option<ScopeParent>) -> Self {
        self.parent = parent;
        self
    }

    #[must_use]
    pub fn with_child_owners(mut self, child_owners: Vec<Principal>) -> Self {
        self.child_owners = child_owners;
        self
    }

    #[must_use]
    pub fn with_carriers(mut self, carriers: Vec<ScopeCarrier>) -> Self {
        self.carriers = carriers;
        self
    }

    #[must_use]
    pub fn with_attributes(mut self, attributes: ScopeAttributes) -> Self {
        self.attributes = attributes;
        self
    }
}

/// A request to end one scope session in `scope.apply`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct ScopeEnd {
    #[serde(rename = "ref")]
    pub scope_ref: String,
    pub scope_epoch: u64,
}

impl ScopeEnd {
    pub fn new(scope_ref: impl Into<String>, scope_epoch: u64) -> Self {
        Self {
            scope_ref: scope_ref.into(),
            scope_epoch,
        }
    }
}

/// What `scope.apply` did with one requested end.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ScopeEndOutcome {
    /// The named scope ref was live at this epoch and was ended.
    Ended,
    /// The named scope ref was not live at this epoch; this end request changed
    /// nothing.
    NotLive,
}

/// The per-end result in a `scope.apply` reply, in request order.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct ScopeEndResult {
    #[serde(rename = "ref")]
    pub scope_ref: String,
    pub scope_epoch: u64,
    pub outcome: ScopeEndOutcome,
}

impl ScopeEndResult {
    pub fn new(scope_ref: impl Into<String>, scope_epoch: u64, outcome: ScopeEndOutcome) -> Self {
        Self {
            scope_ref: scope_ref.into(),
            scope_epoch,
            outcome,
        }
    }
}

/// The scope a `route.open` asks to be admitted under.
///
/// `scope_epoch` is optional on the wire only so that leaving it out is
/// refused by name (`scope_epoch_required`) rather than as a malformed body:
/// every opener must name it, the owner included.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct ScopeSelector {
    #[serde(deserialize_with = "deserialize_scope_principal")]
    pub owner: Principal,
    #[serde(rename = "ref")]
    pub scope_ref: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope_epoch: Option<u64>,
    /// Base64url-encoded 32-byte secret, without padding. Ignored for records
    /// without a bind-proof hash, so holders may always send their proof.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bind_proof: Option<String>,
    /// Keep connection-held presence after an admitted open, even if its route
    /// closes. Send only to daemons advertising `scope-replace-if-idle/v1`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hold_presence: bool,
}

impl ScopeSelector {
    pub fn new(owner: Principal, scope_ref: impl Into<String>, scope_epoch: Option<u64>) -> Self {
        Self {
            owner,
            scope_ref: scope_ref.into(),
            scope_epoch,
            bind_proof: None,
            hold_presence: false,
        }
    }

    #[must_use]
    pub fn with_bind_proof(mut self, proof: Option<String>) -> Self {
        self.bind_proof = proof;
        self
    }

    #[must_use]
    pub fn with_hold_presence(mut self, hold_presence: bool) -> Self {
        self.hold_presence = hold_presence;
        self
    }
}

impl std::fmt::Debug for ScopeSelector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScopeSelector")
            .field("owner", &self.owner)
            .field("scope_ref", &self.scope_ref)
            .field("scope_epoch", &self.scope_epoch)
            .field(
                "bind_proof",
                &self.bind_proof.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

impl std::fmt::Display for ScopeSelector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self, f)
    }
}

/// The state of a scope's parent link.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ParentState {
    /// The parent is live at the named epoch and the link was permitted.
    Linked,
    /// The parent's owner has not synced in this daemon incarnation, so the
    /// link is unverified and grants nothing yet.
    Pending,
    /// The parent is gone or live at another epoch, or the link was refused
    /// when the parent's owner synced. Final for this link.
    Ended,
}

/// What a `scope.sync` or `scope.apply` did with one record.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ScopeRecordOutcome {
    /// The ref was not live before; the scope was created.
    Created,
    /// The ref was live at a lower epoch; that scope ended and this one began.
    Replaced,
    /// The ref was live at this epoch and its content changed.
    Updated,
    /// The ref was live at this epoch with identical content; its `version`
    /// did not move.
    Unchanged,
    /// The record was refused on its own merits (`code` says why). The refusal
    /// itself does not change the ref or undo any expiry processing.
    Refused,
}

/// The per-record result in a `scope.sync` or `scope.apply` reply, in request order.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScopeRecordResult {
    #[serde(rename = "ref")]
    pub scope_ref: String,
    /// The epoch the record named, which for a refusal may differ from the
    /// epoch still held.
    pub scope_epoch: u64,
    pub outcome: ScopeRecordOutcome,
    /// The refusal code when `outcome` is `refused`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// The daemon's content counter for the scope held under this ref after
    /// the sync; absent when no scope is live under it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u64>,
    /// The parent link's state after the sync, for a scope with a parent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_state: Option<ParentState>,
}

/// A scope session of the calling owner that ended through omission from
/// `scope.sync`, an explicit `scope.apply` end, replacement by a higher epoch
/// for the same ref or expiry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScopeEnded {
    #[serde(rename = "ref")]
    pub scope_ref: String,
    pub scope_epoch: u64,
}

/// `scope.describe`'s answer about one `(owner, ref)`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ScopeStatus {
    Live,
    /// Ended in this daemon incarnation, and still remembered.
    Ended,
    /// Neither live nor remembered as ended. With `owner_synced` true the scope
    /// is gone: an owner's first sync of an incarnation is its full set. With
    /// `owner_synced` false and `owner_configured` true the owner has not
    /// re-synced since a daemon restart, so a reader waits. With
    /// `owner_configured` false the owner will never sync.
    NotLive,
}

/// The fields the daemon stamps for a live scope.
/// Build with [`ScopeStamp::new`] and the setters so future fields are additive.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[non_exhaustive]
pub struct ScopeStamp {
    pub owner: Principal,
    #[serde(rename = "ref")]
    pub scope_ref: String,
    pub scope_epoch: u64,
    pub kind: ScopeKind,
    /// Canonical session folder. Sent on binds only to providers declaring
    /// [`SCOPE_WORKDIR_CAPABILITY`]; absent fields keep older binds unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workdir: Option<String>,
    /// Entorhinal project id for attribution, never authority.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<ScopeParent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_state: Option<ParentState>,
    #[serde(default, skip_serializing_if = "ScopeAttributes::is_empty")]
    pub attributes: ScopeAttributes,
    /// Whether the owner is listed in the daemon's `scope_authority_owners`.
    /// Providers decide on this flag and keep no copy of the list.
    pub owner_authorized: bool,
    /// Absolute Unix wall-clock deadline in milliseconds, fixed for this epoch.
    /// On route binds the daemon sends this deadline only to targets declaring
    /// [`crate::call_mode::CALL_MODES_CAPABILITY`], because older providers may
    /// reject unfamiliar scope-stamp fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<u64>,
}

impl ScopeStamp {
    /// A stamp with no parent, attributes or deadline, and no owner authority.
    pub fn new(
        owner: Principal,
        scope_ref: impl Into<String>,
        scope_epoch: u64,
        kind: ScopeKind,
    ) -> Self {
        Self {
            owner,
            scope_ref: scope_ref.into(),
            scope_epoch,
            kind,
            workdir: None,
            project_id: None,
            parent: None,
            parent_state: None,
            attributes: ScopeAttributes::new(),
            owner_authorized: false,
            expires_at_ms: None,
        }
    }

    #[must_use]
    pub fn with_workdir(mut self, workdir: Option<String>) -> Self {
        self.workdir = workdir;
        self
    }

    #[must_use]
    pub fn with_project_id(mut self, project_id: Option<String>) -> Self {
        self.project_id = project_id;
        self
    }

    #[must_use]
    pub fn with_parent(mut self, parent: Option<ScopeParent>) -> Self {
        self.parent = parent;
        self
    }

    #[must_use]
    pub fn with_parent_state(mut self, parent_state: Option<ParentState>) -> Self {
        self.parent_state = parent_state;
        self
    }

    #[must_use]
    pub fn with_attributes(mut self, attributes: ScopeAttributes) -> Self {
        self.attributes = attributes;
        self
    }

    #[must_use]
    pub fn with_owner_authorized(mut self, owner_authorized: bool) -> Self {
        self.owner_authorized = owner_authorized;
        self
    }

    #[must_use]
    pub fn with_expires_at_ms(mut self, expires_at_ms: Option<u64>) -> Self {
        self.expires_at_ms = expires_at_ms;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_call::OpaqueFieldError;

    #[test]
    fn project_id_is_an_opaque_non_space_ascii_token_of_at_most_128_bytes() {
        for valid in ["p", "!~Project:7/path", &"p".repeat(128)] {
            assert_eq!(validate_project_id(valid), Ok(()));
        }
        for invalid in ["", "p p", "p\t", "pé", "p\u{7f}", &"p".repeat(129)] {
            assert!(validate_project_id(invalid).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn scope_project_fields_round_trip_and_absence_keeps_exact_bytes() {
        let record = ScopeRecord::new("session", 1, ScopeKind::Head);
        assert_eq!(
            serde_json::to_string(&record).unwrap(),
            r#"{"ref":"session","scope_epoch":1,"kind":"head"}"#
        );
        let record = record
            .with_workdir(Some("/folder".into()))
            .with_project_id(Some("project:1".into()));
        let encoded = serde_json::to_string(&record).unwrap();
        assert_eq!(
            serde_json::from_str::<ScopeRecord>(&encoded).unwrap(),
            record
        );
        let stamp = ScopeStamp::new(Principal::Direct, "session", 1, ScopeKind::Head);
        assert_eq!(
            serde_json::to_string(&stamp).unwrap(),
            r#"{"owner":{"kind":"direct"},"ref":"session","scope_epoch":1,"kind":"head","owner_authorized":false}"#
        );
        let stamp = stamp
            .with_workdir(record.workdir)
            .with_project_id(record.project_id);
        let encoded = serde_json::to_string(&stamp).unwrap();
        assert_eq!(serde_json::from_str::<ScopeStamp>(&encoded).unwrap(), stamp);
    }

    #[test]
    fn flow_id_uses_the_shared_opaque_token_bounds_and_names_its_field() {
        let field = "flow_id";
        assert_eq!(validate_flow_id(""), Err(OpaqueFieldError::Empty { field }));
        assert_eq!(validate_flow_id("f"), Ok(()));
        assert_eq!(validate_flow_id(&"f".repeat(256)), Ok(()));
        assert_eq!(
            validate_flow_id(&"f".repeat(257)),
            Err(OpaqueFieldError::TooLong { field, length: 257 })
        );
        assert_eq!(validate_flow_id("!~Flow:7/step"), Ok(()));
        for bad in ["f é", "f\t", "fé", "f\u{7f}"] {
            let error = validate_flow_id(bad).unwrap_err();
            assert_eq!(
                error,
                OpaqueFieldError::InvalidCharacter { field, index: 1 }
            );
            assert_eq!(error.field(), "flow_id");
        }
    }

    #[test]
    fn flow_identity_validators_enforce_boundaries_and_name_fields() {
        for valid in [1, 9_007_199_254_740_991] {
            assert_eq!(validate_flow_version(valid), Ok(()));
        }
        for invalid in [0, 9_007_199_254_740_992, u64::MAX] {
            assert!(validate_flow_version(invalid)
                .unwrap_err()
                .contains("flow_version"));
        }
        assert_eq!(
            validate_flow_code_hash(&"0123456789abcdef".repeat(4)),
            Ok(())
        );
        for invalid in [
            "a".repeat(63),
            "a".repeat(65),
            "A".repeat(64),
            "g".repeat(64),
        ] {
            assert!(validate_flow_code_hash(&invalid)
                .unwrap_err()
                .contains("flow_code_hash"));
        }
        for valid in ["i", "!~Install:7/path", &"i".repeat(256)] {
            assert_eq!(validate_flow_install_id(valid), Ok(()));
        }
        for invalid in ["", "i i", "i\t", "ié", "i\u{7f}", &"i".repeat(257)] {
            assert_eq!(
                validate_flow_install_id(invalid).unwrap_err().field(),
                "flow_install_id"
            );
        }
    }

    #[test]
    fn flow_identity_round_trips_and_legacy_stamp_keeps_exact_bytes() {
        let attributes = ScopeAttributes::new().with_flow_id(Some("flow:7".into()));
        let stamp = ScopeStamp::new(Principal::Direct, "session", 1, ScopeKind::Head)
            .with_attributes(attributes.clone());
        assert_eq!(
            serde_json::to_string(&stamp).unwrap(),
            r#"{"owner":{"kind":"direct"},"ref":"session","scope_epoch":1,"kind":"head","attributes":{"flow_id":"flow:7"},"owner_authorized":false}"#
        );
        let attributes = attributes
            .with_flow_version(Some(9_007_199_254_740_991))
            .with_flow_code_hash(Some("a".repeat(64)))
            .with_flow_install_id(Some("install:7".into()));
        let stamp = stamp.with_attributes(attributes.clone());
        let encoded = serde_json::to_string(&stamp).unwrap();
        assert_eq!(serde_json::from_str::<ScopeStamp>(&encoded).unwrap(), stamp);
        assert_eq!(
            serde_json::from_str::<ScopeAttributes>(&serde_json::to_string(&attributes).unwrap())
                .unwrap(),
            attributes
        );
        assert!(
            serde_json::from_str::<ScopeAttributes>(r#"{"flow_version":1,"unknown":true}"#)
                .is_err()
        );
        assert!(!ScopeAttributes::new().with_flow_version(Some(1)).is_empty());
        assert!(!ScopeAttributes::new()
            .with_flow_code_hash(Some("a".repeat(64)))
            .is_empty());
        assert!(!ScopeAttributes::new()
            .with_flow_install_id(Some("i".into()))
            .is_empty());
    }

    #[test]
    fn flow_only_attributes_round_trip_and_absence_keeps_the_bytes() {
        let attributes = ScopeAttributes::default();
        assert!(attributes.is_empty());
        assert_eq!(serde_json::to_string(&attributes).unwrap(), "{}");
        assert_eq!(
            serde_json::from_str::<ScopeAttributes>("{}").unwrap(),
            attributes
        );
        let attributes = ScopeAttributes {
            flow_id: Some("flow:7".to_string()),
            ..ScopeAttributes::default()
        };
        assert!(!attributes.is_empty());
        let encoded = serde_json::to_string(&attributes).unwrap();
        assert_eq!(encoded, r#"{"flow_id":"flow:7"}"#);
        assert_eq!(
            serde_json::from_str::<ScopeAttributes>(&encoded).unwrap(),
            attributes
        );
        assert!(
            serde_json::from_str::<ScopeAttributes>(r#"{"flow_id":"flow:7","unknown":true}"#)
                .is_err()
        );
    }
}
