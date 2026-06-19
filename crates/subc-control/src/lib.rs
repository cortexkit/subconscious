//! Client-facing subc channel-0 control wire shapes.
//!
//! This crate is the client ↔ subc control-plane boundary. It depends only on
//! [`subc-protocol`] for shared primitives such as `ConfigTier`; clients can use
//! it without depending on the daemon implementation.

#![forbid(unsafe_code)]

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use subc_protocol::session::ConfigTier;

/// Reserved dotted operation prefixes for the v0.4 control vocabulary.
///
/// These prefixes are documentation-only in P1; dispatch continues to use the
/// existing untagged shapes until the P2 tagged-enum rewrite.
pub mod ops {
    pub const SERVER: &str = "server.";
    pub const CATALOG: &str = "catalog.";
    pub const ROUTE: &str = "route.";
    pub const SUPERVISOR: &str = "supervisor.";
    pub const SCHEDULER: &str = "scheduler.";
    pub const CONFIG: &str = "config.";
    pub const WATCH: &str = "watch.";
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

/// Client-to-subc channel-0 `Request` body for a passive local poll.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PassivePoll {
    pub op: PollOp,
    pub route_channel: u16,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PollOp {
    Status,
    Liveness,
}

/// subc-to-client channel-0 `Response` body for a status poll.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StatusReply {
    pub status: String,
}

/// subc-to-client channel-0 `Response` body for a liveness poll.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LivenessReply {
    pub live: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passive_poll_and_attach_request_bodies_are_unambiguous() {
        let attach_body = serde_json::to_vec(&AttachRequest {
            project_root: PathBuf::from("/tmp/project"),
            harness: "opencode".to_string(),
            session: "session-1".to_string(),
            config: vec![ConfigTier {
                tier: "project".to_string(),
                source: "/tmp/project/aft.jsonc".to_string(),
                doc: "{}".to_string(),
            }],
        })
        .unwrap();
        assert!(serde_json::from_slice::<PassivePoll>(&attach_body).is_err());

        let poll_body = serde_json::to_vec(&PassivePoll {
            op: PollOp::Status,
            route_channel: 7,
        })
        .unwrap();
        assert!(serde_json::from_slice::<AttachRequest>(&poll_body).is_err());
    }
}
