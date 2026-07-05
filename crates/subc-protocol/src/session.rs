//! Session route control wire contract.
//!
//! subc has two distinct channel-0 handshakes. Module registration is the
//! module-to-subc `HELLO`/`HELLO_ACK` handshake that registers the manifest and
//! liveness. Route bind is the client-to-subc-to-module request/response
//! handshake that binds one client route to a module route channel.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{manifest::ProviderRole, BindIdentity, Principal, RouteTarget};

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
pub enum ModuleControlRequest {
    #[serde(rename = "route.bind")]
    RouteBind {
        route_channel: u16,
        target: RouteTarget,
        identity: BindIdentity,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        principal: Option<Principal>,
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
    CatalogUpdate { provides: Vec<ProviderRole> },
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
    RouteStatus { route_channel: u16, status: String },
}
