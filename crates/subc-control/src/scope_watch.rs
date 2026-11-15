//! Scope lifecycle subscriptions, separate from the exhaustive control enums.

use serde::{Deserialize, Serialize};
use subc_protocol::Principal;

/// An owner-qualified scope reference, without selecting an epoch.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ScopeRef {
    pub owner: Principal,
    #[serde(rename = "ref")]
    pub scope_ref: String,
}

/// One epoch whose lifetime the caller wants to observe.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ScopeWatchSelector {
    pub scope_ref: ScopeRef,
    pub epoch: u64,
}

/// Open an immutable set of at most 256 scope epochs on channel zero.
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScopeWatchRequest {
    op: ScopeWatchOp,
    pub scopes: Vec<ScopeWatchSelector>,
}

impl ScopeWatchRequest {
    pub const OP: &'static str = crate::ops::SCOPE_WATCH;

    pub fn new(scopes: Vec<ScopeWatchSelector>) -> Self {
        Self {
            op: ScopeWatchOp::Watch,
            scopes,
        }
    }
}

open_string_enum! {
    /// Current state of a requested epoch; unknown never establishes liveness.
    ScopeWatchStatus {
        Live => "live",
        Ended => "ended",
        Superseded => "superseded",
        UnknownScope => "unknown",
    }
}

/// One entry in the initial snapshot, in request order.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScopeWatchState {
    pub scope_ref: ScopeRef,
    pub epoch: u64,
    pub status: ScopeWatchStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by_epoch: Option<u64>,
}

/// Initial nonterminal RESPONSE, followed by lifecycle STREAM_DATA pushes.
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScopeWatchResponse {
    op: ScopeWatchOp,
    pub scopes: Vec<ScopeWatchState>,
}

impl ScopeWatchResponse {
    pub fn new(scopes: Vec<ScopeWatchState>) -> Self {
        Self {
            op: ScopeWatchOp::Watch,
            scopes,
        }
    }
}

open_string_enum! {
    /// Terminal lifecycle transition for the watched epoch.
    ScopeWatchEvent {
        Ended => "ended",
        Superseded => "superseded",
    }
}

/// A terminal event delivered at most once per selector within one watch.
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScopeWatchPush {
    op: ScopeWatchOp,
    pub event: ScopeWatchEvent,
    pub scope_ref: ScopeRef,
    pub epoch: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by_epoch: Option<u64>,
}

impl ScopeWatchPush {
    pub fn new(
        event: ScopeWatchEvent,
        scope_ref: ScopeRef,
        epoch: u64,
        by_epoch: Option<u64>,
    ) -> Self {
        Self {
            op: ScopeWatchOp::Watch,
            event,
            scope_ref,
            epoch,
            by_epoch,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
enum ScopeWatchOp {
    #[serde(rename = "scope.watch")]
    Watch,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn scope_watch_wire_shapes_require_op_and_accept_future_fields() {
        let selector: ScopeWatchSelector = serde_json::from_value(json!({
            "scope_ref":{"owner":{"kind":"reserved","module_id":"owner"},"ref":"session"},
            "epoch":7
        }))
        .unwrap();
        let request = ScopeWatchRequest::new(vec![selector.clone()]);
        assert_eq!(
            serde_json::to_value(&request).unwrap(),
            json!({
                "op":"scope.watch", "scopes":[selector]
            })
        );
        let snapshot = json!({"op":"scope.watch","scopes":[{
            "scope_ref":selector.scope_ref,"epoch":7,"status":"superseded","by_epoch":8
        }]});
        let reply: ScopeWatchResponse = serde_json::from_value(snapshot.clone()).unwrap();
        assert_eq!(reply.scopes[0].status, ScopeWatchStatus::Superseded);
        assert_eq!(serde_json::to_value(reply).unwrap(), snapshot);
        for (event, by_epoch) in [
            (ScopeWatchEvent::Ended, None),
            (ScopeWatchEvent::Superseded, Some(8)),
        ] {
            let push = ScopeWatchPush::new(event, selector.scope_ref.clone(), 7, by_epoch);
            let mut value = serde_json::to_value(&push).unwrap();
            assert_eq!(value.get("by_epoch").is_some(), by_epoch.is_some());
            value["future"] = json!(true);
            assert_eq!(
                serde_json::from_value::<ScopeWatchPush>(value.clone()).unwrap(),
                push
            );
            value["op"] = json!("scope.describe");
            assert!(serde_json::from_value::<ScopeWatchPush>(value).is_err());
        }
        assert_eq!(
            serde_json::from_value::<ScopeWatchEvent>(json!("future")).unwrap(),
            ScopeWatchEvent::Unknown("future".into())
        );
        assert!(serde_json::from_value::<ScopeWatchStatus>(json!(false)).is_err());
    }
}
