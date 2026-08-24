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
    BindIdentity, Principal, RouteTarget,
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
    },
    #[serde(rename = "health.check")]
    HealthCheck {},
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
    },
}

/// subc's channel-0 response body for module-originated control RPCs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "op")]
pub enum ModuleControlResponseToModule {
    #[serde(rename = "catalog.update")]
    CatalogUpdate {},
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
