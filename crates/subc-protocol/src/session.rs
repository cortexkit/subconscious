//! Session route control wire contract.
//!
//! subc has two distinct channel-0 handshakes. Module registration is the
//! module-to-subc `HELLO`/`HELLO_ACK` handshake that registers the manifest and
//! liveness. Route bind is the client-to-subc-to-module request/response
//! handshake that binds one client route to a module route channel.

use serde::{Deserialize, Serialize};

use crate::{BindIdentity, RouteTarget};

/// subc-to-module channel-0 control RPC body.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op")]
pub enum ModuleControlRequest {
    #[serde(rename = "route.bind")]
    RouteBind {
        route_channel: u16,
        target: RouteTarget,
        identity: BindIdentity,
    },
}

/// Module-to-subc channel-0 response body.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "op")]
pub enum ModuleControlResponse {
    /// ACK-only success. Rejections use the `FrameType::Error` lane.
    #[serde(rename = "route.bind")]
    RouteBindAck {},
}

/// Module-to-subc channel-0 push body.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "op")]
pub enum ModuleControlPush {
    #[serde(rename = "route.status")]
    RouteStatus { route_channel: u16, status: String },
}
