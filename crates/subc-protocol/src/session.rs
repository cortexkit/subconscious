//! Session route control wire contract.
//!
//! subc has two distinct channel-0 handshakes. Module registration is the
//! module-to-subc `HELLO`/`HELLO_ACK` handshake that registers the manifest and
//! liveness. Route bind is the client-to-subc-to-module request/response
//! handshake that binds one client route to a module route channel.

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
