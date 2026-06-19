//! Session route control wire contract.
//!
//! subc has two distinct channel-0 handshakes. Module registration is the
//! module-to-subc `HELLO`/`HELLO_ACK` handshake that registers the manifest and
//! liveness. Route bind is the client-to-subc-to-module request/response
//! handshake that binds one client route to a module route channel.
//!
//! Config is forwarded as an ordered, provenance-tagged tier list. subc treats
//! every config document as opaque text and preserves `tier`, `source`, and
//! `doc` exactly from the client `route.open` request to the module
//! `route.bind`; it never parses, merges, partitions, or relabels config in
//! transit.

use serde::{Deserialize, Serialize};

use crate::{BindIdentity, RouteTarget};

/// One provenance-tagged config tier supplied during route open.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConfigTier {
    /// Trust label for this tier.
    ///
    /// Known v1 values are `"user"` and `"project"`, but this is intentionally
    /// an open string so a future tier is a value, not a struct change.
    ///
    /// SECURITY INVARIANT: the label is stamped from `source` (the file path
    /// that was read), never derived from `doc` content. subc preserves labels
    /// exactly and never relabels them in transit.
    pub tier: String,
    /// Absolute path the document was read from, used for provenance and by AFT
    /// to infer format/JSONC behavior and produce diagnostics.
    pub source: String,
    /// Verbatim config-source text.
    ///
    /// subc treats this as opaque bytes-as-text and does not interpret, parse,
    /// merge, or validate it. AFT owns parsing, including JSONC handling.
    pub doc: String,
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
        #[serde(default)]
        config: Vec<ConfigTier>,
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
