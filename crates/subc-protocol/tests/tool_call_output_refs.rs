//! `ToolCallRequest.output_refs` against the 0.32 wire shape.
//!
//! The member was added in 0.33.0. These tests hold the two promises that
//! make it safe to add: a request without it keeps exactly the bytes 0.32
//! wrote, and a decoder built on 0.32 ignores it. Both compare against
//! `ToolCallRequest032`, a copy of the 0.32 struct kept here verbatim.

use std::{fs, path::PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use subc_protocol::tool_call::{
    CallOrigin, OutputRefs, OutputRefsError, ToolCallRequest, MAX_OUTPUT_REFS_BYTES,
    OUTPUT_REFS_FIELD,
};

/// `subc_protocol::tool_call::ToolCallRequest` as 0.32.11 defined it, before
/// `output_refs` existed. `CallOrigin` did not change between the two.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
struct ToolCallRequest032 {
    name: String,
    arguments: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    progress_token: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    call_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    schema_pin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    preset: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    origin: Option<CallOrigin>,
}

/// The tool-call fixtures committed before `output_refs` existed.
const FIXTURES_WITHOUT_OUTPUT_REFS: [&str; 6] = [
    "tool_call_request_minimal",
    "tool_call_request_with_id",
    "tool_call_request_with_id_and_progress",
    "tool_call_request_with_origin",
    "tool_call_request_with_preset",
    "tool_call_request_without_origin",
];

fn fixture(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(format!("{name}.json"));
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

/// A descriptor shaped like the llm-runner contract's. Its members mean
/// nothing to this crate; they only make the fixture realistic.
fn descriptor() -> Value {
    json!({
        "set_id": "ors_01J9Z8",
        "mid": "msg_01AbC",
        "call_key": "run-7:call-3",
        "dialect": "anthropic",
        "count": 2,
        "digest": "sha256:4bb06f8e4e3a7715d201d573d0aa4237"
    })
}

fn request_with_output_refs() -> ToolCallRequest {
    ToolCallRequest {
        call_key: Some("run-7:call-3".to_string()),
        output_refs: Some(OutputRefs::new(descriptor()).expect("a small object is accepted")),
        ..ToolCallRequest::new("show_ui", json!({ "view": "diff" }))
    }
}

/// An object whose compact JSON encoding is exactly `length` bytes.
fn object_of_encoded_length(length: usize) -> Value {
    let overhead = serde_json::to_vec(&json!({ "pad": "" })).unwrap().len();
    let value = json!({ "pad": "x".repeat(length - overhead) });
    assert_eq!(serde_json::to_vec(&value).unwrap().len(), length);
    value
}

/// Every request a 0.32 producer could write, read from the committed
/// fixtures, encodes to the same bytes through the 0.33 type as through the
/// 0.32 copy: the absent member adds nothing, not even `null`.
#[test]
fn a_request_without_output_refs_encodes_byte_identically_to_0_32() {
    for name in FIXTURES_WITHOUT_OUTPUT_REFS {
        let text = fixture(name);
        let old: ToolCallRequest032 = serde_json::from_str(&text).expect(name);
        let new: ToolCallRequest = serde_json::from_str(&text).expect(name);
        assert_eq!(new.output_refs, None, "{name}");
        let old_bytes = serde_json::to_string(&old).unwrap();
        let new_bytes = serde_json::to_string(&new).unwrap();
        assert_eq!(new_bytes, old_bytes, "{name}");
        assert!(
            !new_bytes.contains(OUTPUT_REFS_FIELD),
            "{name}: {new_bytes}"
        );
    }
    // The minimal fixture's exact wire bytes, spelled out.
    let minimal: ToolCallRequest =
        serde_json::from_str(&fixture("tool_call_request_minimal")).unwrap();
    assert_eq!(
        minimal,
        ToolCallRequest::new("grep", json!({ "pattern": "needle" }))
    );
    assert_eq!(
        serde_json::to_string(&minimal).unwrap(),
        r#"{"name":"grep","arguments":{"pattern":"needle"}}"#
    );
}

#[test]
fn a_request_with_output_refs_round_trips_as_a_top_level_member() {
    let request = request_with_output_refs();
    let encoded = serde_json::to_string(&request).unwrap();
    let as_value: Value = serde_json::from_str(&encoded).unwrap();
    assert_eq!(as_value[OUTPUT_REFS_FIELD], descriptor());
    assert!(as_value["arguments"].get(OUTPUT_REFS_FIELD).is_none());
    let decoded: ToolCallRequest = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded, request);
    assert_eq!(
        decoded.output_refs.unwrap().into_value(),
        descriptor(),
        "the descriptor comes back member for member"
    );
}

/// A decoder built on 0.32 never heard of `output_refs`; the struct does not
/// deny unknown fields, so it decodes the rest of the call and drops the
/// member.
#[test]
fn a_0_32_decoder_ignores_output_refs() {
    let request = request_with_output_refs();
    let encoded = serde_json::to_string(&request).unwrap();
    let old: ToolCallRequest032 =
        serde_json::from_str(&encoded).expect("a 0.32 decoder accepts the new member");
    assert_eq!(
        old,
        ToolCallRequest032 {
            name: "show_ui".to_string(),
            arguments: json!({ "view": "diff" }),
            call_key: Some("run-7:call-3".to_string()),
            ..ToolCallRequest032::default()
        }
    );
}

#[test]
fn a_non_object_descriptor_is_refused() {
    for value in [
        json!("ors_01J9Z8"),
        json!(["ors_01J9Z8"]),
        json!(4),
        json!(true),
        Value::Null,
    ] {
        let error = OutputRefs::new(value.clone()).expect_err(&value.to_string());
        assert_eq!(error, OutputRefsError::NotAnObject, "{value}");
        assert_eq!(error.field(), "output_refs");
        assert!(error.to_string().starts_with("output_refs"), "{error}");
    }
    for value in [json!("ors_01J9Z8"), json!(["ors_01J9Z8"]), json!(4)] {
        let body = json!({ "name": "show_ui", "arguments": {}, "output_refs": value });
        let error =
            serde_json::from_value::<ToolCallRequest>(body.clone()).expect_err(&body.to_string());
        assert!(
            error
                .to_string()
                .contains("output_refs must be a JSON object"),
            "{body}: {error}"
        );
    }
    // `null` decodes as an absent member, as it does for every other
    // optional member; encoding never writes it.
    let request: ToolCallRequest =
        serde_json::from_value(json!({ "name": "show_ui", "arguments": {}, "output_refs": null }))
            .unwrap();
    assert_eq!(request.output_refs, None);
}

#[test]
fn a_descriptor_is_bounded_to_4096_bytes_of_compact_json() {
    assert_eq!(MAX_OUTPUT_REFS_BYTES, 4096);

    let at_limit = object_of_encoded_length(MAX_OUTPUT_REFS_BYTES);
    let accepted = OutputRefs::new(at_limit.clone()).expect("exactly the limit is accepted");
    assert_eq!(accepted.as_map(), at_limit.as_object().unwrap());
    let body = json!({ "name": "show_ui", "arguments": {}, "output_refs": at_limit });
    serde_json::from_value::<ToolCallRequest>(body).expect("a request at the limit decodes");

    let over = object_of_encoded_length(MAX_OUTPUT_REFS_BYTES + 1);
    assert_eq!(
        OutputRefs::new(over.clone()),
        Err(OutputRefsError::TooLarge { length: 4097 })
    );
    let body = json!({ "name": "show_ui", "arguments": {}, "output_refs": over });
    let error = serde_json::from_value::<ToolCallRequest>(body).expect_err("one byte over");
    assert!(
        error.to_string().contains("output_refs is 4097 bytes"),
        "{error}"
    );
}
