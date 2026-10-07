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
//! the wire, so a body carrying no optionals serializes exactly as the
//! two-field shape older consumers already send — this type is drop-in for
//! them without a wire change.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Principal;

/// Who really asked for a call that another module relays.
///
/// When a module forwards a tool call on behalf of a different caller, the
/// provider sees the forwarding module as the route's principal and the
/// forwarder's own `call_key`. This records the caller behind it, so the
/// provider can attribute the call in its logs and ledgers.
///
/// It is a claim made by the forwarding module, not something the daemon
/// checks: a provider may record it but must never grant or refuse anything
/// because of it. Authority stays with the route's own principal.
///
/// `#[non_exhaustive]` so a later member is an additive change; build it with
/// [`CallOrigin::new`].
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct CallOrigin {
    /// The original caller, in the same form the daemon stamps on a route.
    pub carrier: Principal,
    /// The original caller's key for the call, with the same bounds as
    /// [`ToolCallRequest::call_key`]; check it with [`validate_call_origin`].
    pub call_key: String,
    /// Where the call sits in the model message that asked for it, when the
    /// forwarder knows. `None` means unknown, never "first" or "alone".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_position: Option<MessagePosition>,
}

impl CallOrigin {
    /// An origin naming `carrier` as the caller and `call_key` as its key, with
    /// no message position.
    pub fn new(carrier: Principal, call_key: impl Into<String>) -> Self {
        Self {
            carrier,
            call_key: call_key.into(),
            message_position: None,
        }
    }

    /// Set where the call sits in the model message that asked for it.
    pub fn with_message_position(mut self, position: MessagePosition) -> Self {
        self.message_position = Some(position);
        self
    }
}

/// A relayed call's place among the tool calls of one model message.
///
/// A host may run one message's tool calls concurrently, so a provider whose
/// actions must follow the model's intended order (for example input events on
/// a shared screen) reads this to sequence them. The message id and the index
/// travel together, because an index means nothing without the message it
/// counts within; a position missing either one does not decode.
///
/// Like the rest of [`CallOrigin`], it is a claim by the forwarding module and
/// never authority.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct MessagePosition {
    /// The model message's id, an opaque token with the same bounds as
    /// [`ToolCallRequest::call_key`].
    pub message_id: String,
    /// The call's 0-based position among that message's tool calls, in the
    /// order the model wrote them.
    pub index: u32,
}

impl MessagePosition {
    /// The call at `index` (0-based) among the tool calls of `message_id`.
    pub fn new(message_id: impl Into<String>, index: u32) -> Self {
        Self {
            message_id: message_id.into(),
            index,
        }
    }
}

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
    /// The session's tool preset, assigned by the module that owns the session
    /// and forwarded by its carrier on every call, outside the model's
    /// arguments. Opaque to the
    /// daemon. A provider checks its shape with [`validate_preset`] and answers
    /// a malformed value with `invalid_request` naming [`PRESET_FIELD`].
    ///
    /// Absent means the caller did not say: a provider must decide explicitly
    /// what an absent preset gets, and must not default it to the most capable
    /// preset. A provider refuses a preset it does not serve, by name, rather
    /// than falling back to a default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    /// The caller behind this call when the consumer is relaying it for
    /// someone else; `None` when the consumer is the caller. For attribution
    /// only, never authority: see [`CallOrigin`]. A provider checks it with
    /// [`validate_call_origin`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<CallOrigin>,
}

impl ToolCallRequest {
    /// A call with no consumer id, no progress token, no call key, no schema
    /// pin, no preset and no origin — the shape older two-field consumers send.
    pub fn new(name: impl Into<String>, arguments: Value) -> Self {
        Self {
            name: name.into(),
            arguments,
            tool_call_id: None,
            progress_token: None,
            call_key: None,
            schema_pin: None,
            preset: None,
            origin: None,
        }
    }
}

/// The wire name of [`ToolCallRequest::call_key`], for the `field` of the
/// `invalid_request` error a provider returns when the key is malformed.
pub const CALL_KEY_FIELD: &str = "call_key";

/// The wire name of [`ToolCallRequest::schema_pin`], for the `field` of the
/// `invalid_request` error a provider returns when the pin is malformed.
pub const SCHEMA_PIN_FIELD: &str = "schema_pin";

/// The wire name of [`ToolCallRequest::preset`], for a provider's
/// `invalid_request` error's `field`.
pub const PRESET_FIELD: &str = "preset";

/// The longest preset accepted, in ASCII characters.
pub const PRESET_MAX_LEN: usize = 64;

/// Why a tool preset was refused. Each error names [`PRESET_FIELD`], so a
/// provider can use the same `invalid_request` path as for a schema pin.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PresetError {
    Empty,
    TooLong { length: usize },
    InvalidCharacter { index: usize },
}

impl PresetError {
    pub fn field(&self) -> &'static str {
        PRESET_FIELD
    }
}

impl std::fmt::Display for PresetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "preset must not be empty"),
            Self::TooLong { length } => write!(
                f,
                "preset is {length} bytes; at most {PRESET_MAX_LEN} are allowed"
            ),
            Self::InvalidCharacter { index } => {
                write!(
                    f,
                    "preset has a character at byte {index} outside [a-z0-9_-]"
                )
            }
        }
    }
}

impl std::error::Error for PresetError {}

/// The wire path of [`CallOrigin::call_key`] inside a request, for the `field`
/// of the `invalid_request` error a provider returns when it is malformed.
pub const ORIGIN_CALL_KEY_FIELD: &str = "origin.call_key";

/// The wire path of [`MessagePosition::message_id`] inside a request, for the
/// `field` of the `invalid_request` error a provider returns when it is
/// malformed.
pub const ORIGIN_MESSAGE_ID_FIELD: &str = "origin.message_position.message_id";

/// The longest opaque token field accepted, in bytes (every accepted byte is
/// one ASCII character). Shared by `call_key`, `schema_pin` and scope `flow_id`.
pub const OPAQUE_FIELD_MAX_LEN: usize = 256;

/// The longest `call_key` accepted.
pub const CALL_KEY_MAX_LEN: usize = OPAQUE_FIELD_MAX_LEN;

/// The longest `schema_pin` accepted.
pub const SCHEMA_PIN_MAX_LEN: usize = OPAQUE_FIELD_MAX_LEN;

/// Why an opaque token field (`call_key`, `schema_pin`, `flow_id`) was refused. Every
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
pub(crate) fn validate_opaque_field(
    field: &'static str,
    value: &str,
) -> Result<(), OpaqueFieldError> {
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

/// Check a preset: 1 to [`PRESET_MAX_LEN`] ASCII characters in `[a-z0-9_-]`.
/// Errors name [`PRESET_FIELD`]. An absent preset is `None`, not an empty string.
pub fn validate_preset(preset: &str) -> Result<(), PresetError> {
    if preset.is_empty() {
        return Err(PresetError::Empty);
    }
    if preset.len() > PRESET_MAX_LEN {
        return Err(PresetError::TooLong {
            length: preset.len(),
        });
    }
    if let Some(index) = preset.bytes().position(|byte| {
        !(byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_-".contains(&byte))
    }) {
        return Err(PresetError::InvalidCharacter { index });
    }
    Ok(())
}

/// Check a [`CallOrigin`]: its `call_key`, and its message position's
/// `message_id` when present, must pass the shared opaque-field rule. Errors
/// name [`ORIGIN_CALL_KEY_FIELD`] or [`ORIGIN_MESSAGE_ID_FIELD`]. Every
/// [`Principal`] is accepted as the carrier, and every `u32` as the index.
pub fn validate_call_origin(origin: &CallOrigin) -> Result<(), OpaqueFieldError> {
    validate_opaque_field(ORIGIN_CALL_KEY_FIELD, &origin.call_key)?;
    if let Some(position) = &origin.message_position {
        validate_opaque_field(ORIGIN_MESSAGE_ID_FIELD, &position.message_id)?;
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
        assert_eq!(request.schema_pin, None);
        assert_eq!(request.preset, None);
        assert_eq!(request.origin, None);
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
            preset: None,
            origin: None,
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
    fn preset_round_trips_outside_arguments_and_absence_keeps_the_bytes() {
        let mut request = ToolCallRequest::new("grep", json!({ "q": "x" }));
        let absent = serde_json::to_string(&request).unwrap();
        assert_eq!(absent, r#"{"name":"grep","arguments":{"q":"x"}}"#);
        let decoded: ToolCallRequest = serde_json::from_str(&absent).unwrap();
        assert_eq!(decoded, request);
        assert_eq!(decoded.preset, None);

        request.preset = Some("read_only-2".to_string());
        let encoded = serde_json::to_value(&request).unwrap();
        assert_eq!(
            encoded,
            json!({
                "name": "grep", "arguments": { "q": "x" }, "preset": "read_only-2"
            })
        );
        assert_eq!(
            serde_json::from_value::<ToolCallRequest>(encoded).unwrap(),
            request
        );
    }

    #[test]
    fn preset_bounds_are_one_to_64_lowercase_digits_underscore_or_hyphen() {
        assert_eq!(validate_preset(""), Err(PresetError::Empty));
        assert_eq!(validate_preset("a"), Ok(()));
        assert_eq!(validate_preset(&"a".repeat(64)), Ok(()));
        assert_eq!(
            validate_preset(&"a".repeat(65)),
            Err(PresetError::TooLong { length: 65 })
        );
        assert_eq!(validate_preset("a0_-z9"), Ok(()));
        for bad in ["A", ".", "é", " "] {
            let error = validate_preset(bad).unwrap_err();
            assert_eq!(error, PresetError::InvalidCharacter { index: 0 });
            assert_eq!(error.field(), "preset");
            assert!(error.to_string().starts_with("preset"));
            let refusal =
                crate::ErrorBody::new(crate::error_codes::INVALID_REQUEST, error.to_string())
                    .with_detail(json!({ "field": error.field() }));
            assert_eq!(refusal.code, "invalid_request");
            assert_eq!(refusal.detail.unwrap()["field"], "preset");
        }
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
            preset: None,
            origin: None,
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

    fn relayed_origin() -> CallOrigin {
        CallOrigin::new(
            Principal::Reserved {
                module_id: "broca".to_string(),
            },
            "broca:run-7/call-3",
        )
    }

    /// The carrier travels as the tagged `Principal` object the daemon stamps
    /// on a route, never as a bare string.
    #[test]
    fn origin_round_trips_as_a_top_level_member_with_a_tagged_carrier() {
        let request = ToolCallRequest {
            call_key: Some("pf:relay/991".to_string()),
            origin: Some(relayed_origin()),
            ..ToolCallRequest::new("grep", json!({ "q": "x" }))
        };
        let encoded = serde_json::to_value(&request).expect("encode");
        assert_eq!(
            encoded,
            json!({
                "name": "grep",
                "arguments": { "q": "x" },
                "call_key": "pf:relay/991",
                "origin": {
                    "carrier": { "kind": "reserved", "module_id": "broca" },
                    "call_key": "broca:run-7/call-3"
                }
            })
        );
        let decoded: ToolCallRequest = serde_json::from_value(encoded).expect("decode");
        assert_eq!(decoded, request);
    }

    /// The position travels inside `origin` as one object, and an origin without
    /// one keeps exactly the bytes it had before positions existed.
    #[test]
    fn message_position_round_trips_and_is_omitted_when_unknown() {
        let origin = relayed_origin().with_message_position(MessagePosition::new("msg_01AbC", 2));
        let encoded = serde_json::to_value(&origin).expect("encode");
        assert_eq!(
            encoded,
            json!({
                "carrier": { "kind": "reserved", "module_id": "broca" },
                "call_key": "broca:run-7/call-3",
                "message_position": { "message_id": "msg_01AbC", "index": 2 }
            })
        );
        let decoded: CallOrigin = serde_json::from_value(encoded).expect("decode");
        assert_eq!(decoded, origin);

        let without = serde_json::to_value(relayed_origin()).expect("encode");
        assert!(without.get("message_position").is_none(), "{without}");
        let decoded: CallOrigin = serde_json::from_value(without).expect("decode");
        assert_eq!(decoded.message_position, None);
    }

    /// An index means nothing without its message, so a position missing either
    /// member is refused at decode rather than read as a half-known order.
    #[test]
    fn a_message_position_missing_either_member_does_not_decode() {
        for position in [json!({ "index": 0 }), json!({ "message_id": "m" })] {
            let body = json!({
                "carrier": { "kind": "direct" },
                "call_key": "k",
                "message_position": position
            });
            assert!(
                serde_json::from_value::<CallOrigin>(body.clone()).is_err(),
                "{body}"
            );
        }
    }

    /// Unknown members inside a message position are ignored when decoding, so
    /// a reader tolerates members a later version adds.
    #[test]
    fn unknown_members_inside_a_message_position_are_tolerated() {
        let decoded: CallOrigin = serde_json::from_value(json!({
            "carrier": { "kind": "direct" },
            "call_key": "k",
            "message_position": { "message_id": "m", "index": 4, "later": true }
        }))
        .expect("unknown members inside a position are tolerated");
        assert_eq!(decoded.message_position, Some(MessagePosition::new("m", 4)));
    }

    #[test]
    fn a_malformed_message_id_is_refused_by_its_field() {
        for bad in [
            String::new(),
            "has space".to_string(),
            "x".repeat(OPAQUE_FIELD_MAX_LEN + 1),
        ] {
            let origin = CallOrigin::new(Principal::Direct, "k")
                .with_message_position(MessagePosition::new(bad.clone(), 0));
            let error = validate_call_origin(&origin).expect_err(&bad);
            assert_eq!(error.field(), ORIGIN_MESSAGE_ID_FIELD, "{bad:?}");
        }
        let good = CallOrigin::new(Principal::Direct, "k")
            .with_message_position(MessagePosition::new("msg_01AbC", u32::MAX));
        assert_eq!(validate_call_origin(&good), Ok(()));
    }

    #[test]
    fn a_request_without_an_origin_omits_the_member_and_decodes_as_none() {
        let request = ToolCallRequest::new("grep", json!({}));
        let encoded = serde_json::to_value(&request).expect("encode");
        assert!(encoded.get("origin").is_none(), "{encoded}");
        let decoded: ToolCallRequest = serde_json::from_value(encoded).expect("decode");
        assert_eq!(decoded.origin, None);
        assert_eq!(decoded, request);
    }

    #[test]
    fn a_request_with_an_origin_and_an_unknown_member_still_decodes() {
        let request: ToolCallRequest = serde_json::from_value(json!({
            "name": "grep",
            "arguments": {},
            "origin": {
                "carrier": { "kind": "direct" },
                "call_key": "k",
                "some_future_origin_key": 1
            },
            "some_future_key": { "nested": true }
        }))
        .expect("unknown members are tolerated");
        assert_eq!(
            request.origin,
            Some(CallOrigin::new(Principal::Direct, "k"))
        );
    }

    #[test]
    fn call_origin_key_refusals_name_the_origin_call_key_field() {
        let carrier = Principal::Direct;
        let field = ORIGIN_CALL_KEY_FIELD;
        assert_eq!(field, "origin.call_key");
        let cases = [
            (String::new(), OpaqueFieldError::Empty { field }),
            (
                "k".repeat(257),
                OpaqueFieldError::TooLong { field, length: 257 },
            ),
            (
                "pf:relay 991".to_string(),
                OpaqueFieldError::InvalidCharacter { field, index: 8 },
            ),
        ];
        for (key, expected) in cases {
            let error = validate_call_origin(&CallOrigin::new(carrier.clone(), key.clone()))
                .expect_err("malformed origin key is refused");
            assert_eq!(error, expected, "{key:?}");
            assert_eq!(error.field(), "origin.call_key", "{key:?}");
        }
    }

    #[test]
    fn call_origin_accepts_every_carrier_kind() {
        let carriers = [
            Principal::Reserved {
                module_id: "prefrontal-core".to_string(),
            },
            Principal::Direct,
            Principal::Unverified,
        ];
        for carrier in carriers {
            assert_eq!(
                validate_call_origin(&CallOrigin::new(carrier.clone(), "pf:relay/991")),
                Ok(()),
                "{carrier:?}"
            );
        }
    }
}
