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
    /// Which version of the tool's schema the consumer built its arguments
    /// against, so a provider can tell a call made against a schema it no
    /// longer serves. Opaque here: the tool-provider role defines what it
    /// holds. A provider checks only its shape, with [`validate_schema_pin`],
    /// and answers a malformed one with `invalid_request` naming the field
    /// [`SCHEMA_PIN_FIELD`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_pin: Option<String>,
}

impl ToolCallRequest {
    /// A call with no consumer id, no progress token, no call key and no
    /// schema pin — the shape older two-field consumers send.
    pub fn new(name: impl Into<String>, arguments: Value) -> Self {
        Self {
            name: name.into(),
            arguments,
            tool_call_id: None,
            progress_token: None,
            call_key: None,
            schema_pin: None,
        }
    }
}

/// The wire name of [`ToolCallRequest::call_key`], for the `field` of the
/// `invalid_request` error a provider returns when the key is malformed.
pub const CALL_KEY_FIELD: &str = "call_key";

/// The wire name of [`ToolCallRequest::schema_pin`], for the `field` of the
/// `invalid_request` error a provider returns when the pin is malformed.
pub const SCHEMA_PIN_FIELD: &str = "schema_pin";

/// The longest opaque token field accepted, in bytes (every accepted byte is
/// one ASCII character). Shared by `call_key` and `schema_pin`.
pub const OPAQUE_FIELD_MAX_LEN: usize = 256;

/// The longest `call_key` accepted.
pub const CALL_KEY_MAX_LEN: usize = OPAQUE_FIELD_MAX_LEN;

/// The longest `schema_pin` accepted.
pub const SCHEMA_PIN_MAX_LEN: usize = OPAQUE_FIELD_MAX_LEN;

/// Why an opaque token field (`call_key`, `schema_pin`) was refused. Every
/// variant names the request field it is about, so a provider can put it in
/// its `invalid_request` error without tracking which check ran.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpaqueFieldError {
    /// The value was the empty string. An absent value is `None`, never `""`.
    Empty { field: &'static str },
    /// The value was longer than [`OPAQUE_FIELD_MAX_LEN`] bytes.
    TooLong { field: &'static str, length: usize },
    /// The byte at `index` is not printable, non-space ASCII.
    InvalidCharacter { field: &'static str, index: usize },
}

/// The error [`validate_call_key`] returns. Kept as a name so code that only
/// formats the error or reads [`OpaqueFieldError::field`] keeps compiling.
pub type CallKeyError = OpaqueFieldError;

impl OpaqueFieldError {
    /// The request field the error is about.
    pub fn field(&self) -> &'static str {
        match self {
            Self::Empty { field }
            | Self::TooLong { field, .. }
            | Self::InvalidCharacter { field, .. } => field,
        }
    }
}

impl std::fmt::Display for OpaqueFieldError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty { field } => write!(f, "{field} must not be empty"),
            Self::TooLong { field, length } => write!(
                f,
                "{field} is {length} bytes; at most {OPAQUE_FIELD_MAX_LEN} are allowed"
            ),
            Self::InvalidCharacter { field, index } => write!(
                f,
                "{field} has a character at byte {index} outside printable ASCII \
                 (0x21 to 0x7E; space is not allowed)"
            ),
        }
    }
}

impl std::error::Error for OpaqueFieldError {}

/// Check an opaque token field: 1 to [`OPAQUE_FIELD_MAX_LEN`] characters, each
/// printable ASCII from 0x21 to 0x7E. `field` is the wire name the error
/// reports.
///
/// Space (0x20) is refused. Providers compare these values byte for byte and
/// write them into logs and ledgers, where a leading or trailing space is
/// invisible: two values that differ only by one would read as the same value
/// and act as different ones. Every other printable character is allowed, so
/// a consumer can use its existing ids (UUIDs, `prefix:id` forms, base64,
/// digests) unchanged.
fn validate_opaque_field(field: &'static str, value: &str) -> Result<(), OpaqueFieldError> {
    if value.is_empty() {
        return Err(OpaqueFieldError::Empty { field });
    }
    if value.len() > OPAQUE_FIELD_MAX_LEN {
        return Err(OpaqueFieldError::TooLong {
            field,
            length: value.len(),
        });
    }
    if let Some(index) = value
        .bytes()
        .position(|byte| !(0x21..=0x7e).contains(&byte))
    {
        return Err(OpaqueFieldError::InvalidCharacter { field, index });
    }
    Ok(())
}

/// Check a `call_key` with the shared opaque-field rule; errors name
/// [`CALL_KEY_FIELD`].
pub fn validate_call_key(key: &str) -> Result<(), CallKeyError> {
    validate_opaque_field(CALL_KEY_FIELD, key)
}

/// Check a `schema_pin` with the shared opaque-field rule; errors name
/// [`SCHEMA_PIN_FIELD`].
pub fn validate_schema_pin(pin: &str) -> Result<(), OpaqueFieldError> {
    validate_opaque_field(SCHEMA_PIN_FIELD, pin)
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
        assert_eq!(request.schema_pin, None);
    }

    #[test]
    fn call_key_round_trips_as_a_top_level_member() {
        let request = ToolCallRequest {
            name: "grep".to_string(),
            arguments: json!({ "q": "x" }),
            tool_call_id: None,
            progress_token: None,
            call_key: Some("run-7:call-3".to_string()),
            schema_pin: None,
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

    /// The same bounds for both fields, with each refusal naming its own
    /// field: one validator, two names.
    #[test]
    fn opaque_field_bounds_are_one_to_256_printable_non_space_ascii() {
        type Validate = fn(&str) -> Result<(), OpaqueFieldError>;
        let validators: [(&str, Validate); 2] = [
            (CALL_KEY_FIELD, validate_call_key),
            (SCHEMA_PIN_FIELD, validate_schema_pin),
        ];
        for (field, validate) in validators {
            assert_eq!(validate(""), Err(OpaqueFieldError::Empty { field }));
            assert_eq!(validate("k"), Ok(()));
            assert_eq!(validate(&"k".repeat(256)), Ok(()));
            assert_eq!(
                validate(&"k".repeat(257)),
                Err(OpaqueFieldError::TooLong { field, length: 257 })
            );
            assert_eq!(validate("!~"), Ok(()), "both ends of 0x21..=0x7E");
            for bad in ["ké", "a\tb", "a\u{7f}", "a b"] {
                assert_eq!(
                    validate(bad),
                    Err(OpaqueFieldError::InvalidCharacter { field, index: 1 }),
                    "{field}: {bad:?}"
                );
            }
            let error = validate("").unwrap_err();
            assert_eq!(error.field(), field);
            assert!(error.to_string().starts_with(field), "{error}");
        }
    }

    #[test]
    fn schema_pin_round_trips_as_a_top_level_member() {
        let request = ToolCallRequest {
            schema_pin: Some("sha256:0f1e2d".to_string()),
            ..ToolCallRequest::new("grep", json!({ "q": "x" }))
        };
        let encoded = serde_json::to_value(&request).expect("encode");
        assert_eq!(
            encoded,
            json!({ "name": "grep", "arguments": { "q": "x" }, "schema_pin": "sha256:0f1e2d" })
        );
        let decoded: ToolCallRequest = serde_json::from_value(encoded).expect("decode");
        assert_eq!(decoded, request);
    }

    #[test]
    fn a_request_without_a_schema_pin_omits_the_member_and_round_trips() {
        let request = ToolCallRequest::new("grep", json!({}));
        let encoded = serde_json::to_value(&request).expect("encode");
        assert!(encoded.get("schema_pin").is_none(), "{encoded}");
        let decoded: ToolCallRequest = serde_json::from_value(encoded).expect("decode");
        assert_eq!(decoded.schema_pin, None);
        assert_eq!(decoded, request);
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
            schema_pin: None,
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
