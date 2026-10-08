//! Session route control wire contract.
//!
//! subc has two distinct channel-0 handshakes. Module registration is the
//! module-to-subc `HELLO`/`HELLO_ACK` handshake that registers the manifest and
//! liveness. Route bind is the client-to-subc-to-module request/response
//! handshake that binds one client route to a module route channel.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    manifest::{CapabilityDeclarations, ProviderRole},
    scope::{ScopeEnded, ScopeRecord, ScopeRecordResult, ScopeStamp, ScopeStatus},
    BindIdentity, Principal, RouteCloseReason, RouteTarget,
};

pub const MODULE_CONTROL_OP_HEALTH_CHECK: &str = "health.check";
pub const MODULE_TO_SUBC_OP_CATALOG_UPDATE: &str = "catalog.update";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HealthStatus {
    Ok,
    Degraded,
    Failing,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HealthReport {
    pub status: HealthStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metrics: Option<Value>,
}

impl HealthReport {
    pub fn ok() -> Self {
        Self {
            status: HealthStatus::Ok,
            detail: None,
            metrics: None,
        }
    }
}

/// subc-to-module channel-0 control RPC body.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op")]
// RouteBind carries the complete bind metadata, while HealthCheck is a marker;
// preserving the direct wire shape is more useful than boxing every bind field.
#[allow(clippy::large_enum_variant)]
pub enum ModuleControlRequest {
    #[serde(rename = "route.bind")]
    RouteBind {
        route_channel: u16,
        epoch: u32,
        target: RouteTarget,
        identity: BindIdentity,
        /// The daemon's attestation of the consumer, and the only field here a
        /// provider may grant privilege on.
        ///
        /// `Reserved` is minted at exactly one place in the daemon, on the branch
        /// where the consumer's launch nonce matched a supervised spawn — the
        /// function that checks is the function that mints, so the value cannot
        /// exist without the check having run. That property is what a provider is
        /// relying on, and it is the reason to key authority on this rather than on
        /// `identity`, which is client-supplied and unattested (see BindIdentity).
        ///
        /// Absent means the daemon made no attestation, which is not the same as a
        /// denial: it is the shape a pre-attestation peer sends. Treat it as
        /// unattested rather than as trusted-by-default.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        principal: Option<Principal>,
        /// Consumer-declared reverse-request capabilities for the route. This is
        /// an unverified declaration, not a privilege grant; if a consumer
        /// over-declares, providers may still send reverse requests that later
        /// time out or deny. Providers must treat an absent field as no
        /// reverse-request capability. The vocabulary is open strings; known MCP
        /// method-family values today are "elicitation", "sampling", and
        /// "roots".
        #[serde(default, skip_serializing_if = "Option::is_none")]
        consumer_capabilities: Option<Vec<String>>,
        /// The versions of provider roles the consumer speaks on this route,
        /// role name to version (`{"tool-provider": "v1"}`), copied from the
        /// consumer's `route.open` unchanged. Like `consumer_capabilities` it
        /// is the consumer's unverified declaration and grants nothing; a
        /// provider uses it to choose which version of a role's wire shape to
        /// speak. The daemon has checked it with [`validate_role_versions`] and
        /// never sends an empty map. Absent means the consumer declared none:
        /// a legacy consumer, or a daemon that predates the field.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        role_versions: Option<BTreeMap<String, String>>,
        /// Opaque admission facts supplied by the configured carrier module.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        admission_facts: Option<Value>,
        /// The daemon's stamp of the scope the route was admitted under, taken
        /// from the owner's synced record at admission. Like `principal`, it is
        /// the daemon's, never the opener's: a provider may act on it (on
        /// `owner_authorized`, `delegates` and `agent_id` together), and must
        /// treat it as fixed for the route's life, because a change that
        /// revokes authority closes the route.
        ///
        /// Absent means the route was opened without a scope, or by a daemon
        /// that predates scopes. A provider that needs a scope refuses the bind.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scope: Option<ScopeStamp>,
    },
    #[serde(rename = "health.check")]
    HealthCheck {},
}

/// One-way subc-to-module channel-0 control command.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "op")]
pub enum ModuleControlCommand {
    #[serde(rename = "module.draining")]
    Draining {
        reason: RouteCloseReason,
        /// Absolute Unix-millisecond deadline for this drain.
        ///
        /// WALL CLOCK, WHILE THE DAEMON ENFORCES THE CEILING ON A
        /// SUSPEND-EXCLUDING MONOTONIC CLOCK (`Instant`, supervise.rs). Both
        /// processes share one host so `CLOCK_REALTIME` agrees exactly, and the
        /// two clocks diverge only across host sleep: `Instant` stops, wall does
        /// not. So a module that sleeps mid-drain wakes to a deadline further in
        /// the past than the daemon's own ceiling, computes LESS remaining time
        /// than it has, and seals early.
        ///
        /// That direction is deliberate and is the safe one — a module stopping
        /// early loses nothing, since the daemon kills at its own ceiling
        /// regardless. The reverse (a module believing it has time the daemon
        /// has already spent) is the failure this ordering avoids. A module must
        /// therefore treat this as "no later than", never as a grant.
        deadline_ms: u64,
    },
}

/// Module-to-subc channel-0 response body.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op")]
pub enum ModuleControlResponse {
    /// ACK-only success. Rejections use the `FrameType::Error` lane.
    #[serde(rename = "route.bind")]
    RouteBindAck {},
    #[serde(rename = "health.check")]
    HealthCheck {
        status: HealthStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metrics: Option<Value>,
    },
}

/// Module-originated channel-0 control RPC body.
///
/// This is intentionally separate from [`ModuleControlRequest`]: that enum is the
/// daemon-to-module direction (`route.bind`, `health.check`), while these bodies
/// are sent by an already-registered module to subc on a `REQUEST` frame.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op")]
pub enum ModuleControlRequestFromModule {
    #[serde(rename = "catalog.update")]
    CatalogUpdate {
        provides: Vec<ProviderRole>,
        /// An attested replacement for the static capability declaration emitted
        /// by the module's current manifest. `None` preserves the prior
        /// declaration so existing role-only catalog updates remain byte-identical.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        capabilities: Option<CapabilityDeclarations>,
        /// Updates readiness without re-registering. `None` leaves it unchanged.
        ///
        /// Both directions are allowed, but repeatedly flapping readiness looks
        /// like a restart storm to callers and is a defect in the module.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ready: Option<bool>,
    },
    #[serde(rename = "supervisor.live_roots")]
    LiveRoots {},
    /// Register this module's full scope set. The owner is the module whose
    /// registered connection sends it; nothing in the body names the owner.
    /// Per-record refusals come back in the reply; a refusal of the whole sync
    /// (not the owner's sync authority, a stale generation, a bound exceeded)
    /// is an `Error` frame and changes nothing.
    #[serde(rename = "scope.sync")]
    ScopeSync {
        generation: u64,
        scopes: Vec<ScopeRecord>,
    },
    /// Read one scope's current state.
    #[serde(rename = "scope.describe")]
    ScopeDescribe {
        owner: Principal,
        #[serde(rename = "ref")]
        scope_ref: String,
    },
}

/// Counts of routes for one canonical project root.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LiveRoot {
    pub project_root: std::path::PathBuf,
    pub bound: u64,
    pub pending: u64,
}

/// subc's channel-0 response body for module-originated control RPCs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "op")]
pub enum ModuleControlResponseToModule {
    #[serde(rename = "catalog.update")]
    CatalogUpdate {},
    #[serde(rename = "supervisor.live_roots")]
    LiveRoots {
        roots: Vec<LiveRoot>,
        unknown_root_bindings: u64,
        total_bindings: u64,
    },
    #[serde(rename = "scope.sync")]
    ScopeSync {
        generation: u64,
        results: Vec<ScopeRecordResult>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        ended: Vec<ScopeEnded>,
    },
    #[serde(rename = "scope.describe")]
    ScopeDescribe {
        status: ScopeStatus,
        /// The live epoch, or for `ended` the most recent epoch that ended.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scope_epoch: Option<u64>,
        daemon_incarnation: String,
        /// Whether the owner has synced since this daemon incarnation started.
        owner_synced: bool,
        /// Whether the owner is a module in the daemon's supervised roster.
        owner_configured: bool,
        /// The stamp fields, present only when `status` is `live`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scope: Option<ScopeStamp>,
    },
}

/// A module's channel-0 request to confirm one in-flight write with the person.
///
/// This standalone body keeps the existing exhaustive module-control enums
/// unchanged. The daemon validates the summary and the asking connection's
/// permission before showing a prompt; constructing a request grants nothing.
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OperatorConfirmRequest {
    op: OperatorConfirmOp,
    pub summary: String,
    /// The caller's route channel as seen on the asking module's connection.
    pub route_channel: u16,
    /// The epoch of that route on the asking module's connection.
    pub route_epoch: u32,
}

impl OperatorConfirmRequest {
    pub fn new(summary: impl Into<String>, route_channel: u16, route_epoch: u32) -> Self {
        Self {
            op: OperatorConfirmOp::Confirm,
            summary: summary.into(),
            route_channel,
            route_epoch,
        }
    }
}

/// A successful channel-0 reply to an [`OperatorConfirmRequest`].
///
/// Only `outcome: "confirmed"` confirms the write. Refusals use [`crate::ErrorBody`]
/// instead. Readers must not treat an unknown outcome as confirmation.
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OperatorConfirmReply {
    op: OperatorConfirmOp,
    pub outcome: String,
}

impl OperatorConfirmReply {
    pub fn confirmed() -> Self {
        Self {
            op: OperatorConfirmOp::Confirm,
            outcome: "confirmed".to_string(),
        }
    }
}

// A required, single-valued field both emits the op and refuses a missing or
// different op when decoding, without exposing a caller-editable tag.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
enum OperatorConfirmOp {
    #[serde(rename = "operator.confirm")]
    Confirm,
}

impl From<HealthReport> for ModuleControlResponse {
    fn from(report: HealthReport) -> Self {
        Self::HealthCheck {
            status: report.status,
            detail: report.detail,
            metrics: report.metrics,
        }
    }
}

impl ModuleControlResponse {
    pub fn health_report(&self) -> Option<HealthReport> {
        match self {
            Self::HealthCheck {
                status,
                detail,
                metrics,
            } => Some(HealthReport {
                status: *status,
                detail: detail.clone(),
                metrics: metrics.clone(),
            }),
            Self::RouteBindAck {} => None,
        }
    }
}

/// Module-to-subc channel-0 push body.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "op")]
pub enum ModuleControlPush {
    #[serde(rename = "route.status")]
    RouteStatus {
        route_channel: u16,
        route_epoch: u32,
        status: String,
    },
}

/// The wire name of the `role_versions` field on `route.open` and
/// `route.bind`, for the `detail.field` of the `invalid_request` error that
/// refuses a malformed one.
pub const ROLE_VERSIONS_FIELD: &str = "role_versions";

/// Most entries a `role_versions` map may hold.
pub const MAX_ROLE_VERSIONS: usize = 8;

/// The longest role name accepted in `role_versions`, in bytes.
pub const MAX_ROLE_NAME_LEN: usize = 64;

/// Why a `role_versions` map was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RoleVersionsError {
    /// The map has more than [`MAX_ROLE_VERSIONS`] entries.
    TooMany { count: usize },
    /// A role name is not lowercase ASCII letters and digits in words joined
    /// by single hyphens (`tool-provider`), or is longer than
    /// [`MAX_ROLE_NAME_LEN`] bytes.
    InvalidRole { role: String },
    /// A version is not `v` followed by a positive integer without leading
    /// zeros (`v1`, `v12`).
    InvalidVersion { role: String, version: String },
}

impl RoleVersionsError {
    /// The request field the error is about: always [`ROLE_VERSIONS_FIELD`].
    pub fn field(&self) -> &'static str {
        ROLE_VERSIONS_FIELD
    }
}

impl std::fmt::Display for RoleVersionsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooMany { count } => write!(
                f,
                "{ROLE_VERSIONS_FIELD} has {count} entries; at most {MAX_ROLE_VERSIONS} are allowed"
            ),
            Self::InvalidRole { role } => write!(
                f,
                "{ROLE_VERSIONS_FIELD} names role {role:?}, which is not lowercase letters and \
                 digits in words joined by '-', at most {MAX_ROLE_NAME_LEN} bytes"
            ),
            Self::InvalidVersion { role, version } => write!(
                f,
                "{ROLE_VERSIONS_FIELD} gives role {role:?} version {version:?}, which is not 'v' \
                 followed by a positive integer without leading zeros"
            ),
        }
    }
}

impl std::error::Error for RoleVersionsError {}

/// Check a `role_versions` map: at most [`MAX_ROLE_VERSIONS`] entries, each
/// role name matching `^[a-z0-9]+(-[a-z0-9]+)*$` in at most
/// [`MAX_ROLE_NAME_LEN`] bytes, and each version matching `^v[1-9][0-9]*$`.
///
/// The daemon refuses a `route.open` that fails this, so a consumer can run
/// the same check before sending. An empty map passes; the daemon treats it
/// as no declaration at all.
pub fn validate_role_versions(
    role_versions: &BTreeMap<String, String>,
) -> Result<(), RoleVersionsError> {
    if role_versions.len() > MAX_ROLE_VERSIONS {
        return Err(RoleVersionsError::TooMany {
            count: role_versions.len(),
        });
    }
    for (role, version) in role_versions {
        if !is_role_name(role) {
            return Err(RoleVersionsError::InvalidRole { role: role.clone() });
        }
        if !is_role_version(version) {
            return Err(RoleVersionsError::InvalidVersion {
                role: role.clone(),
                version: version.clone(),
            });
        }
    }
    Ok(())
}

fn is_role_name(role: &str) -> bool {
    !role.is_empty()
        && role.len() <= MAX_ROLE_NAME_LEN
        && role.split('-').all(|word| {
            !word.is_empty()
                && word
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        })
}

fn is_role_version(version: &str) -> bool {
    let bytes = version.as_bytes();
    bytes.len() >= 2
        && bytes[0] == b'v'
        && (b'1'..=b'9').contains(&bytes[1])
        && bytes[2..].iter().all(u8::is_ascii_digit)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        entries
            .iter()
            .map(|(role, version)| (role.to_string(), version.to_string()))
            .collect()
    }

    #[test]
    fn role_versions_accept_role_names_and_positive_versions() {
        for entries in [
            vec![],
            vec![("tool-provider", "v1")],
            vec![("a", "v9"), ("b2", "v10"), ("x-1-y", "v1203")],
        ] {
            assert_eq!(
                validate_role_versions(&map(&entries)),
                Ok(()),
                "{entries:?}"
            );
        }
        let longest = "a".repeat(MAX_ROLE_NAME_LEN);
        assert_eq!(validate_role_versions(&map(&[(&longest, "v1")])), Ok(()));
        let full: BTreeMap<String, String> = (0..MAX_ROLE_VERSIONS)
            .map(|index| (format!("role-{index}"), "v1".to_string()))
            .collect();
        assert_eq!(validate_role_versions(&full), Ok(()));
    }

    #[test]
    fn role_versions_refuse_malformed_role_names() {
        let too_long = "a".repeat(MAX_ROLE_NAME_LEN + 1);
        for role in [
            "",
            "Tool-provider",
            "tool_provider",
            "tool provider",
            "-tool",
            "tool-",
            "tool--provider",
            "tool.provider",
            "outil-é",
            too_long.as_str(),
        ] {
            let error = validate_role_versions(&map(&[(role, "v1")])).unwrap_err();
            assert_eq!(
                error,
                RoleVersionsError::InvalidRole {
                    role: role.to_string()
                },
                "{role:?}"
            );
            assert_eq!(error.field(), "role_versions");
        }
    }

    #[test]
    fn role_versions_refuse_malformed_versions() {
        for version in [
            "", "v", "v0", "v01", "1", "V1", "v1.0", "v-1", "v1 ", " v1", "vx",
        ] {
            let error = validate_role_versions(&map(&[("tool-provider", version)])).unwrap_err();
            assert_eq!(
                error,
                RoleVersionsError::InvalidVersion {
                    role: "tool-provider".to_string(),
                    version: version.to_string(),
                },
                "{version:?}"
            );
            assert_eq!(error.field(), "role_versions");
        }
    }

    #[test]
    fn role_versions_refuse_more_than_eight_entries() {
        let nine: BTreeMap<String, String> = (0..=MAX_ROLE_VERSIONS)
            .map(|index| (format!("role-{index}"), "v1".to_string()))
            .collect();
        let error = validate_role_versions(&nine).unwrap_err();
        assert_eq!(error, RoleVersionsError::TooMany { count: 9 });
        assert_eq!(error.field(), "role_versions");
        assert!(error.to_string().starts_with("role_versions"), "{error}");
    }

    #[test]
    fn route_bind_omits_absent_role_versions_and_carries_present_ones_verbatim() {
        let bind = |role_versions| ModuleControlRequest::RouteBind {
            route_channel: 1,
            epoch: 1,
            target: crate::RouteTarget::ToolProvider {
                module_id: "aft".to_string(),
            },
            identity: crate::BindIdentity::new("/tmp/p", "h", "s"),
            principal: None,
            consumer_capabilities: None,
            role_versions,
            admission_facts: None,
            scope: None,
        };
        let absent = serde_json::to_value(bind(None)).unwrap();
        assert!(absent.get("role_versions").is_none(), "{absent}");
        let present = bind(Some(map(&[("tool-provider", "v1")])));
        let encoded = serde_json::to_value(&present).unwrap();
        assert_eq!(
            encoded["role_versions"],
            serde_json::json!({ "tool-provider": "v1" })
        );
        let decoded: ModuleControlRequest = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded, present);
    }
}
