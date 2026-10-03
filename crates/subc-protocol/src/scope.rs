//! Scope records: owned identity records the daemon holds for sessions.
//!
//! A scope is identified by `(owner, ref)`. The owner is the module whose own
//! registered connection synced it, never a value in the request, and the
//! `ref` is an opaque string unique within that owner only. The design is
//! `docs/designs/daemon-scopes.md`; the wire shapes here are the owner-facing
//! half of it (`scope.sync`, `scope.describe`).
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
pub struct ScopeParent {
    #[serde(deserialize_with = "deserialize_scope_principal")]
    pub owner: Principal,
    #[serde(rename = "ref")]
    pub scope_ref: String,
    pub scope_epoch: u64,
}

/// Who, besides the owner, may open routes under a scope.
///
/// `targets` absent means the carrier may open to any module. Present, it
/// names the only module ids the carrier may open to, and must hold between 1
/// and [`MAX_CARRIER_TARGETS`] entries: an empty list is refused rather than
/// read as either "none" or "all".
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ScopeCarrier {
    #[serde(deserialize_with = "deserialize_scope_principal")]
    pub principal: Principal,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub targets: Option<Vec<String>>,
}

/// The attributes the daemon stamps without interpreting. They bear authority,
/// so only an owner module named in the daemon config's `scope_authority_owners`
/// list (by default the module that owns agent sessions) may set them; a scope
/// owned by any other module must leave them empty.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ScopeAttributes {
    /// The agent the scope's session belongs to. Identity, never permission.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flow_id: Option<String>,
}

impl ScopeAttributes {
    pub fn is_empty(&self) -> bool {
        self.agent_id.is_none() && !self.delegates && self.flow_id.is_none()
    }
}

/// Check a flow id using the shared opaque-token rule: 1–256 printable,
/// non-space ASCII bytes. Errors name `flow_id`. Scope refs remain opaque and
/// are not subject to this token rule.
pub fn validate_flow_id(flow_id: &str) -> Result<(), crate::tool_call::OpaqueFieldError> {
    crate::tool_call::validate_opaque_field("flow_id", flow_id)
}

/// One scope as its owner registers it in `scope.sync`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ScopeRecord {
    #[serde(rename = "ref")]
    pub scope_ref: String,
    /// Owner-supplied session number. The owner keeps it with its own record of
    /// the session, re-sends the same value for the same session after any
    /// restart, and uses a higher one when it reuses the ref for a new session.
    pub scope_epoch: u64,
    pub kind: ScopeKind,
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

/// The scope a `route.open` asks to be admitted under.
///
/// `scope_epoch` is optional on the wire only so that leaving it out is
/// refused by name (`scope_epoch_required`) rather than as a malformed body:
/// every opener must name it, the owner included.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ScopeSelector {
    #[serde(deserialize_with = "deserialize_scope_principal")]
    pub owner: Principal,
    #[serde(rename = "ref")]
    pub scope_ref: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope_epoch: Option<u64>,
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

/// What a `scope.sync` did with one record.
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
    /// The record was refused on its own merits (`code` says why) and the ref
    /// keeps whatever state it had before this sync.
    Refused,
}

/// The per-record result in a `scope.sync` reply, in request order.
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

/// A scope of the syncing owner that the sync ended, by removal or by a
/// higher epoch for the same ref.
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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScopeStamp {
    pub owner: Principal,
    #[serde(rename = "ref")]
    pub scope_ref: String,
    pub scope_epoch: u64,
    pub kind: ScopeKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<ScopeParent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_state: Option<ParentState>,
    #[serde(default, skip_serializing_if = "ScopeAttributes::is_empty")]
    pub attributes: ScopeAttributes,
    /// Whether the owner is listed in the daemon's `scope_authority_owners`.
    /// Providers decide on this flag and keep no copy of the list.
    pub owner_authorized: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_call::OpaqueFieldError;

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
