//! Session-attach data-plane wire contract.
//!
//! subc has two distinct channel-0 handshakes. Module registration is the
//! module-to-subc `HELLO`/`HELLO_ACK` handshake that registers the manifest and
//! liveness. Session attach is the client-to-subc-to-module request/response
//! handshake that binds one `(project_root, harness, session)` triple to a
//! route channel.
//!
//! Session attach is vetoed by the module: subc relays an [`AttachRelay`] to
//! the singleton module, commits the route only after an accepted
//! [`AttachRelayResponse`], and returns module rejection as an [`ErrorBody`]
//! `ERROR` frame. The triple binds once at attach time; subsequent data-plane
//! frames carry only the envelope `channel` and opaque body bytes.
//!
//! Config is forwarded as an ordered, provenance-tagged tier list. subc treats
//! every config document as opaque text and preserves `tier`, `source`, and
//! `doc` exactly from [`AttachRequest`] to [`AttachRelay`]; it never parses,
//! merges, partitions, or relabels config in transit.
//!
//! [`ErrorBody`]: crate::ErrorBody

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// One provenance-tagged config tier supplied during session attach.
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

/// Client-originated channel-0 control RPC body for binding a harness session
/// to a module route.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AttachRequest {
    pub project_root: PathBuf,
    pub harness: String,
    pub session: String,
    /// Ordered config tiers; precedence is list order, with later tiers winning.
    pub config: Vec<ConfigTier>,
}

/// subc's channel-0 response body for an accepted session attach.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AttachAck {
    pub route_channel: u16,
}

/// subc-to-module channel-0 control RPC body asking the singleton module to bind
/// a route channel.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AttachRelay {
    pub route_channel: u16,
    pub project_root: PathBuf,
    pub harness: String,
    pub session: String,
    /// Ordered config tiers forwarded from [`AttachRequest`] unchanged; subc
    /// never merges, partitions, parses, or relabels config.
    pub config: Vec<ConfigTier>,
}

/// Module response body for an accepted attach relay.
///
/// A rejected attach is returned as an [`ErrorBody`] `ERROR` frame.
///
/// [`ErrorBody`]: crate::ErrorBody
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AttachRelayResponse {
    pub accept: bool,
}

/// subc-to-module channel-0 control RPC body telling the module a route channel
/// is gone after client `GOODBYE` or client drop.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DetachRelay {
    pub route_channel: u16,
}
