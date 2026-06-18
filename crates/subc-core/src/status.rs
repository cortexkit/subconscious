//! subc-core local channel-0 status/liveness control RPC bodies.
//!
//! These shapes intentionally live outside `subc-protocol`: Story 2.5 carries
//! passive status/liveness over existing channel-0 `Request`/`Response`/`Push`
//! frames so the locked envelope and `FrameType` set stay unchanged. The
//! `status` string is an opaque module-owned payload; subc only stores and
//! returns it verbatim.

use serde::{Deserialize, Serialize};

/// Module-to-subc channel-0 `Push` body that refreshes the latest status for a route.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StatusUpdate {
    pub route_channel: u16,
    pub status: String,
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
#[serde(deny_unknown_fields)]
pub struct StatusReply {
    pub status: String,
}

/// subc-to-client channel-0 `Response` body for a liveness poll.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LivenessReply {
    pub live: bool,
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::forwarding::{AttachRequest, ConfigTier};

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
