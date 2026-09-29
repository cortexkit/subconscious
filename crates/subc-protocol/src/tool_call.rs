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
    /// A key the consumer chose for this call, which a provider may use to
    /// recognise the same call arriving twice. Opaque to the daemon; a
    /// provider checks only its shape, with [`validate_call_key`], and answers
    /// a malformed one with `invalid_request` naming the field
    /// [`CALL_KEY_FIELD`].
    ///
    /// This struct is deliberately not `#[non_exhaustive]`: a consumer that
    /// builds it field by field must decide what key, if any, to send, so a
    /// new field here is meant to stop its struct literal compiling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_key: Option<String>,
}

impl ToolCallRequest {
    /// A call with no consumer id, no progress token and no call key — the
    /// shape older two-field consumers send.
    pub fn new(name: impl Into<String>, arguments: Value) -> Self {
        Self {
            name: name.into(),
            arguments,
            tool_call_id: None,
            progress_token: None,
            call_key: None,
        }
    }
}

/// The wire name of [`ToolCallRequest::call_key`], for the `field` of the
/// `invalid_request` error a provider returns when the key is malformed.
pub const CALL_KEY_FIELD: &str = "call_key";

/// The longest `call_key` accepted, in bytes (every accepted byte is one
/// ASCII character).
pub const CALL_KEY_MAX_LEN: usize = 256;

/// Why a `call_key` was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CallKeyError {
    /// The key was the empty string. An absent key is `None`, never `""`.
    Empty,
    /// The key was longer than [`CALL_KEY_MAX_LEN`] bytes.
    TooLong { length: usize },
    /// The byte at `index` is not printable, non-space ASCII.
    InvalidCharacter { index: usize },
}

impl CallKeyError {
    /// The request field the error is about, always [`CALL_KEY_FIELD`].
    pub fn field(&self) -> &'static str {
        CALL_KEY_FIELD
    }
}

impl std::fmt::Display for CallKeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "{CALL_KEY_FIELD} must not be empty"),
            Self::TooLong { length } => write!(
                f,
                "{CALL_KEY_FIELD} is {length} bytes; at most {CALL_KEY_MAX_LEN} are allowed"
            ),
            Self::InvalidCharacter { index } => write!(
                f,
                "{CALL_KEY_FIELD} has a character at byte {index} outside printable ASCII \
                 (0x21 to 0x7E; space is not allowed)"
            ),
        }
    }
}

impl std::error::Error for CallKeyError {}

/// Check a `call_key`: 1 to [`CALL_KEY_MAX_LEN`] characters, each printable
/// ASCII from 0x21 to 0x7E.
///
/// Space (0x20) is refused. Providers compare keys byte for byte and write
/// them into logs and ledgers, where a leading or trailing space is invisible:
/// two keys that differ only by one would read as the same key and act as
/// different ones. Every other printable character is allowed, so a consumer
/// can use its existing ids (UUIDs, `prefix:id` forms, base64) unchanged.
pub fn validate_call_key(key: &str) -> Result<(), CallKeyError> {
    if key.is_empty() {
        return Err(CallKeyError::Empty);
    }
    if key.len() > CALL_KEY_MAX_LEN {
        return Err(CallKeyError::TooLong { length: key.len() });
    }
    if let Some(index) = key.bytes().position(|byte| !(0x21..=0x7e).contains(&byte)) {
        return Err(CallKeyError::InvalidCharacter { index });
    }
    Ok(())
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
        assert_eq!(request.call_key, None);
    }

    #[test]
    fn call_key_round_trips_as_a_top_level_member() {
        let request = ToolCallRequest {
            name: "grep".to_string(),
            arguments: json!({ "q": "x" }),
            tool_call_id: None,
            progress_token: None,
            call_key: Some("run-7:call-3".to_string()),
        };
        let encoded = serde_json::to_value(&request).expect("encode");
        assert_eq!(
            encoded,
            json!({ "name": "grep", "arguments": { "q": "x" }, "call_key": "run-7:call-3" })
        );
        let decoded: ToolCallRequest = serde_json::from_value(encoded).expect("decode");
        assert_eq!(decoded, request);
    }

    #[test]
    fn a_request_without_a_call_key_omits_the_member_and_round_trips() {
        let request = ToolCallRequest::new("grep", json!({}));
        let encoded = serde_json::to_value(&request).expect("encode");
        assert!(encoded.get("call_key").is_none(), "{encoded}");
        let decoded: ToolCallRequest = serde_json::from_value(encoded).expect("decode");
        assert_eq!(decoded.call_key, None);
        assert_eq!(decoded, request);
    }

    #[test]
    fn call_key_bounds_are_one_to_256_printable_non_space_ascii() {
        assert_eq!(validate_call_key(""), Err(CallKeyError::Empty));
        assert_eq!(validate_call_key("k"), Ok(()));
        assert_eq!(validate_call_key(&"k".repeat(256)), Ok(()));
        assert_eq!(
            validate_call_key(&"k".repeat(257)),
            Err(CallKeyError::TooLong { length: 257 })
        );
        assert_eq!(validate_call_key("!~"), Ok(()), "both ends of 0x21..=0x7E");
        assert_eq!(
            validate_call_key("ké"),
            Err(CallKeyError::InvalidCharacter { index: 1 })
        );
        assert_eq!(
            validate_call_key("a\tb"),
            Err(CallKeyError::InvalidCharacter { index: 1 })
        );
        assert_eq!(
            validate_call_key("a\u{7f}"),
            Err(CallKeyError::InvalidCharacter { index: 1 })
        );
        assert_eq!(
            validate_call_key("a b"),
            Err(CallKeyError::InvalidCharacter { index: 1 })
        );
        assert_eq!(CallKeyError::Empty.field(), "call_key");
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
            call_key: None,
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
