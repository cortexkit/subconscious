//! Additive activity ledger bodies, dispatched separately from the exhaustive
//! client control enums by their `op` tag.

use std::path::PathBuf;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};
use subc_protocol::Principal;

use crate::ops;

/// Advertised in HELLO_ACK and server.describe before activity operations are used.
pub const ACTIVITY_LEDGER_CAPABILITY: &str = "activity-ledger/v1";

pub const ACTIVITY_KIND_FORBIDDEN: &str = "activity_kind_forbidden";
pub const ACTIVITY_ACTOR_UNKNOWN: &str = "activity_actor_unknown";
pub const ACTIVITY_RATE_LIMITED: &str = "activity_rate_limited";
pub const ACTIVITY_READ_FORBIDDEN: &str = "activity_read_forbidden";
pub const ACTIVITY_TOO_LARGE: &str = "activity_too_large";
pub const ACTIVITY_MALFORMED: &str = "activity_malformed";
pub const ACTIVITY_UNAVAILABLE: &str = "activity_unavailable";
pub const ACTIVITY_CURSOR_INCARNATION_MISMATCH: &str = "activity_cursor_incarnation_mismatch";
pub const ACTIVITY_CURSOR_TOO_OLD: &str = "activity_cursor_too_old";
pub const ACTIVITY_SUBSCRIBER_LAGGED: &str = "activity_subscriber_lagged";

/// A live-feed position. Query continuation positions are sequence numbers only.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActivityCursor {
    pub daemon_incarnation: String,
    pub seq: u64,
}

/// A short typed pointer, never a secret value.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ActivitySubject {
    #[serde(rename = "type")]
    pub subject_type: String,
    pub id: String,
}

/// Verified identity stamped by the daemon, not supplied by a recording module.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActivityActor {
    pub principal: Principal,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope_owner: Option<Principal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flow_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_root: Option<PathBuf>,
    /// Canonical folder from the route's scope, not the caller's bind identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workdir: Option<PathBuf>,
    /// Entorhinal project id from the scope; attribution only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
}

/// One durably appended event. `prev` is reserved for a future hash chain and is
/// deliberately absent from this format. Identity is (daemon_incarnation, seq),
/// and entries are ordered by seq rather than by their wall-clock timestamps.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ActivityEntry {
    pub seq: u64,
    pub at_ms: u64,
    pub daemon_incarnation: String,
    pub module: String,
    pub kind: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<ActivitySubject>,
    pub details: Map<String, Value>,
    pub actor: ActivityActor,
}

/// A route binding owned by the recording module, checked at this epoch.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ActivityRoute {
    pub channel: u16,
    pub epoch: u32,
}

/// A live scope owned by the recording module, checked at this epoch.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ActivityScope {
    #[serde(rename = "ref")]
    pub scope_ref: String,
    pub epoch: u64,
}

/// Exactly one citation from which the daemon resolves the actor.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ActivityOn {
    Route(ActivityRoute),
    Scope(ActivityScope),
}

// Serde's struct `tag` adds an op during serialization but does not validate it
// during deserialization. A private tagged enum validates the op without adding
// variants to the public client control enums or exposing an editable op field.
macro_rules! activity_body {
    ($(#[$doc:meta])* $name:ident, $op:path, $wire_name:literal,
     $(#[$wire_meta:meta])* { $( $(#[$field_meta:meta])* pub $field:ident: $ty:ty, )* }) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Serialize, PartialEq)]
        #[serde(tag = "op", rename = $wire_name)]
        pub struct $name {
            $( $(#[$field_meta])* pub $field: $ty, )*
        }

        impl $name {
            pub const OP: &'static str = $op;
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where D: Deserializer<'de> {
                #[derive(Deserialize)]
                #[serde(tag = "op")]
                $(#[$wire_meta])*
                enum Wire {
                    #[serde(rename = $wire_name)]
                    Body { $( $(#[$field_meta])* $field: $ty, )* },
                }
                let Wire::Body { $( $field, )* } = Wire::deserialize(deserializer)?;
                Ok(Self { $( $field, )* })
            }
        }
    };
}

fn deserialize_details<'de, D>(deserializer: D) -> Result<Option<Map<String, Value>>, D::Error>
where
    D: Deserializer<'de>,
{
    Map::deserialize(deserializer).map(Some)
}

activity_body! {
/// Report an event. Shape checking lives here; kind, size, ownership and rate
/// validation remain the daemon's responsibility. There is no actor field.
ActivityRecordRequest, ops::ACTIVITY_RECORD, "activity.record",
#[serde(deny_unknown_fields)] {
    pub kind: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<ActivitySubject>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(deserialize_with = "deserialize_details")]
    pub details: Option<Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on: Option<ActivityOn>,
}

}

activity_body! {
/// Sent only after the assigned sequence has been durably appended.
ActivityRecordResponse, ops::ACTIVITY_RECORD, "activity.record", {
    pub seq: u64,
}

}

activity_body! {
/// Filter a newest-first scan. `cursor` is exclusive: only lower seqs follow it.
ActivityQueryRequest, ops::ACTIVITY_QUERY, "activity.query", {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub module: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until_ms: Option<u64>,
    pub limit: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<u64>,
}

}

activity_body! {
/// A page plus the current incarnation's head at scan capture, even on an empty
/// page. Subscribe from head to move from the scan to live entries without a gap.
ActivityQueryResponse, ops::ACTIVITY_QUERY, "activity.query", {
    pub entries: Vec<ActivityEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<u64>,
    pub head: ActivityCursor,
}

}

activity_body! {
/// Replay after since and remain open for live entries. A refused or lagged feed
/// ends with an Error frame carrying one of the activity refusal-code constants.
ActivitySubscribeRequest, ops::ACTIVITY_SUBSCRIBE, "activity.subscribe", {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<ActivityCursor>,
}

}

activity_body! {
/// One successful append delivered on the subscription's correlation id. There
/// is no separate acknowledgement; each response is one streamed entry.
ActivitySubscribeResponse, ops::ACTIVITY_SUBSCRIBE, "activity.subscribe", {
    pub entry: ActivityEntry,
}

}
