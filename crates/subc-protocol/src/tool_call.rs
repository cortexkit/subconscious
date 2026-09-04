//! The body of a tool-call `REQUEST` frame on a bound route.
//!
//! The daemon splices route frames without reading their bodies, so this
//! shape is a contract between consumers (the MCP gateway, model runners)
//! and provider modules, not something the daemon enforces. Before this
//! type existed every consumer carried its own struct and every provider its
//! own reader, and the fields drifted: the gateway sent `progress_token`,
//! a model runner sent only `name` and `arguments`, and a provider that
//! needed the caller's tool-call id had no field to read it from.
//!
//! Decoding is deliberately tolerant of unknown members: a provider must
//! never refuse a call because a newer consumer added a key it does not
//! know. Omitted optionals decode as `None`; `None` optionals are omitted on
//! the wire, so a body carrying neither optional serializes exactly as the
//! two-field shape older consumers already send — this type is drop-in for
//! them without a wire change.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A tool invocation as carried on a route `REQUEST` frame.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct ToolCallRequest {
    /// The provider's bare manifest tool name (no gateway prefix).
    pub name: String,
    /// The arguments exactly as the caller supplied them; consumers never
    /// translate them, and the provider's manifest schema is what accepts
    /// or rejects their shape.
    pub arguments: Value,
    /// The consumer's own identifier for this call, minted by whatever
    /// dispatched it (a model runner's WAL intent id, a gateway request id).
    /// Opaque to the daemon and to subc; unique per call on the consumer's
    /// side, so a provider's at-most-once fence can key on it directly
    /// instead of synthesizing an id from the call's contents.
    ///
    /// `None` is a statement about the PRODUCER, not the call: it means this
    /// consumer did not supply an id, never that the call has no identity.
    /// A reader must not collapse the two — the moment a legacy producer is
    /// on the other end, treating `None` as "no id exists" and synthesizing
    /// one silently reproduces exactly the failure this field exists to end.
    /// A reader that synthesizes a fallback id when this is `None` must
    /// record that the fallback fired (a fallback that never reports firing
    /// is indistinguishable from a working component that is quietly wrong).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// An MCP progress token the consumer wants progress notifications
    /// correlated to, when the caller requested progress. Opaque here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress_token: Option<Value>,
}

impl ToolCallRequest {
    /// A call with no consumer id and no progress token — the shape older
    /// two-field consumers send.
    pub fn new(name: impl Into<String>, arguments: Value) -> Self {
        Self {
            name: name.into(),
            arguments,
            tool_call_id: None,
            progress_token: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn omitted_optionals_decode_as_none() {
        let request: ToolCallRequest =
            serde_json::from_value(json!({ "name": "grep", "arguments": { "q": "x" } }))
                .expect("two-field body decodes");
        assert_eq!(request.tool_call_id, None);
        assert_eq!(request.progress_token, None);
    }

    #[test]
    fn none_optionals_are_omitted_so_the_wire_matches_the_two_field_shape() {
        let request = ToolCallRequest::new("grep", json!({ "q": "x" }));
        let encoded = serde_json::to_value(&request).expect("encode");
        assert_eq!(
            encoded,
            json!({ "name": "grep", "arguments": { "q": "x" } })
        );
    }

    #[test]
    fn tool_call_id_round_trips() {
        let request = ToolCallRequest {
            name: "grep".to_string(),
            arguments: json!({ "q": "x" }),
            tool_call_id: Some("wal-intent-42".to_string()),
            progress_token: None,
        };
        let encoded = serde_json::to_value(&request).expect("encode");
        assert_eq!(encoded["tool_call_id"], json!("wal-intent-42"));
        let decoded: ToolCallRequest = serde_json::from_value(encoded).expect("decode");
        assert_eq!(decoded, request);
    }

    #[test]
    fn unknown_members_do_not_fail_a_provider_decode() {
        // A newer consumer added a key this provider has never heard of; the
        // call must still decode rather than refuse.
        let request: ToolCallRequest = serde_json::from_value(json!({
            "name": "grep",
            "arguments": {},
            "some_future_key": { "nested": true }
        }))
        .expect("unknown members are tolerated");
        assert_eq!(request.name, "grep");
    }
}
