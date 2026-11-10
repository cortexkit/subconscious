//! Reader-first compatibility checks for optional authority attestations.

use serde_json::{json, Value};

use crate::{
    error_codes,
    manifest::ModuleManifest,
    module_id::module_id_path_hazard,
    peer::{
        encode_route_bind, CallerAttestation, CallerClaim, CallerOrigin, RouteBindPeer,
        CALLER_ATTESTATION_CAPABILITY,
    },
    provider_declarations::{
        catalog_update_has_provider_declarations, validate_hello_provider_declarations,
    },
    session::ModuleControlRequest,
    Flags, RouteCloseReason, ROUTE_CLOSE_REASONS_CAPABILITY,
};

fn hello(declarations: Value) -> Value {
    json!({"manifest": {
        "module_id": "reader", "module_version": "1", "protocol_ver": 1,
        "provides": [], "provider_declarations": declarations
    }})
}

fn refuses(declarations: Value) {
    let raw = hello(declarations);
    let error = validate_hello_provider_declarations(&raw).unwrap_err();
    assert_eq!(error.code(), "invalid_provider_declarations");
    assert_eq!(error.field(), "provider_declarations");
    assert!(error.to_string().contains("provider_declarations"));
    assert!(
        serde_json::from_value::<ModuleManifest>(raw["manifest"].clone()).is_err(),
        "typed decoding must not silently drop malformed declarations"
    );
}

#[test]
fn reader_skew_provider_declarations_absence_and_opaque_round_trip() {
    let manifest = ModuleManifest::builder("reader", "1").build();
    let raw = serde_json::to_value(&manifest).unwrap();
    assert!(raw.get("provider_declarations").is_none());
    assert!(serde_json::from_value::<ModuleManifest>(raw)
        .unwrap()
        .provider_declarations
        .is_none());

    for field in [
        json!({}),
        json!({"mcp:provider": {"future": {"nested": [1, false, null]}}}),
    ] {
        let raw = hello(field.clone());
        validate_hello_provider_declarations(&raw).unwrap();
        let decoded: ModuleManifest = serde_json::from_value(raw["manifest"].clone()).unwrap();
        assert_eq!(
            serde_json::to_value(&decoded).unwrap()["provider_declarations"],
            field
        );
        let rebuilt = ModuleManifest::builder("reader", "1")
            .provider_declarations(decoded.provider_declarations)
            .build();
        assert_eq!(
            serde_json::to_value(rebuilt).unwrap()["provider_declarations"],
            field
        );
    }
}

#[test]
fn reader_skew_provider_declarations_requires_object() {
    for value in [Value::Null, json!([]), json!("object"), json!(true)] {
        refuses(value);
    }
}

#[test]
fn reader_skew_provider_declarations_entry_count() {
    let mut entries = serde_json::Map::new();
    for index in 0..16 {
        entries.insert(format!("provider-{index}"), json!({}));
    }
    validate_hello_provider_declarations(&hello(Value::Object(entries.clone()))).unwrap();
    entries.insert("seventeenth".into(), json!({}));
    refuses(Value::Object(entries));
}

#[test]
fn reader_skew_module_id_grammar() {
    for valid in ["mcp:provider", "with spaces", "...", "é", &"x".repeat(255)] {
        assert!(module_id_path_hazard(valid).is_ok(), "{valid:?}");
    }
    for (invalid, reason) in [
        ("", "empty"),
        ("a/b", "contains a path separator"),
        ("a\\b", "contains a path separator"),
        (".", "is a dot path component"),
        ("..", "is a dot path component"),
        ("a\0b", "contains a control character"),
        ("a\nb", "contains a control character"),
        ("a\u{7f}b", "contains a control character"),
        (&"é".repeat(128), "is longer than 255 bytes"),
    ] {
        assert_eq!(module_id_path_hazard(invalid).unwrap_err(), reason);
        refuses(json!({invalid: {}}));
    }
}

#[test]
fn reader_skew_provider_declarations_values_are_objects() {
    for value in [Value::Null, json!([]), json!(4), json!("opaque")] {
        refuses(json!({"provider": value}));
    }
}

#[test]
fn reader_skew_provider_declarations_entry_compact_byte_limit() {
    // {"p":""} costs eight bytes; UTF-8 and escapes count as relayed bytes.
    let entry = json!({"p": "x".repeat(16_376)});
    assert_eq!(serde_json::to_vec(&entry).unwrap().len(), 16_384);
    let raw = hello(json!({"provider": entry}));
    let pretty = serde_json::to_string_pretty(&raw).unwrap();
    validate_hello_provider_declarations(&serde_json::from_str(&pretty).unwrap()).unwrap();
    refuses(json!({"provider": {"p": "x".repeat(16_377)}}));
    refuses(json!({"provider": {"p": "é".repeat(8_189)}}));
    refuses(json!({"provider": {"p": "\n".repeat(8_189)}}));
}

#[test]
fn reader_skew_provider_declarations_field_compact_byte_limit() {
    // Two entries plus {"a":,"b":} punctuation occupy exactly 24 KiB.
    let field = json!({"a": {"p": "x".repeat(12_274)}, "b": {"p": "x".repeat(12_275)}});
    assert_eq!(serde_json::to_vec(&field).unwrap().len(), 24_576);
    validate_hello_provider_declarations(&hello(field)).unwrap();
    refuses(json!({"a": {"p": "x".repeat(12_274)}, "b": {"p": "x".repeat(12_276)}}));
}

#[test]
fn reader_skew_catalog_update_frozen_key_checked_before_typed_decode() {
    for body in [
        br#"{"op":"catalog.update","provider_declarations":null}"#.as_slice(),
        br#"{"provides":false,"provider_declarations":{}}"#,
        br#"{"provider\u005fdeclarations":[],"provides":42}"#,
    ] {
        assert!(catalog_update_has_provider_declarations(body).unwrap());
    }
    for body in [
        br#"{"op":"catalog.update","provides":[]}"#.as_slice(),
        br#"{"provides":{"provider_declarations":{}}}"#,
        br#"{"description":"provider_declarations"}"#,
    ] {
        assert!(!catalog_update_has_provider_declarations(body).unwrap());
    }
    assert!(
        catalog_update_has_provider_declarations(br#"{"provider_declarations":{}} trailing"#)
            .is_err()
    );
    assert_eq!(
        error_codes::CATALOG_UPDATE_FROZEN_DECLARATIONS,
        "catalog_update_frozen_declarations"
    );
}

fn caller() -> Value {
    json!({"incarnation": "daemon", "spawn": 1, "authority_epoch": 2,
        "claim": "own", "first_party": true, "origin": "release",
        "approved_declarations_digest": "approved"})
}

#[test]
fn reader_skew_caller_absent_or_missing_any_required_field_is_unmarked() {
    for raw in [json!({}), json!({"caller": null})] {
        let decoded: RouteBindPeer = serde_json::from_value(raw).unwrap();
        assert!(decoded.caller.is_none());
    }
    for missing in [
        "incarnation",
        "spawn",
        "authority_epoch",
        "claim",
        "first_party",
        "origin",
        "approved_declarations_digest",
    ] {
        let mut value = caller();
        value.as_object_mut().unwrap().remove(missing);
        let decoded: RouteBindPeer = serde_json::from_value(json!({"caller": value})).unwrap();
        assert!(
            decoded.caller.is_none(),
            "missing {missing} must not attest authority"
        );
    }
    let decoded: RouteBindPeer = serde_json::from_value(json!({"caller": caller()})).unwrap();
    assert!(decoded.caller.unwrap().first_party);
}

#[test]
fn reader_skew_caller_refuses_unsafe_spawn_and_epoch() {
    for field in ["spawn", "authority_epoch"] {
        let mut value = caller();
        value[field] = json!(9_007_199_254_740_991_u64);
        assert!(serde_json::from_value::<RouteBindPeer>(json!({"caller": value.clone()})).is_ok());
        for invalid in [
            json!(9_007_199_254_740_992_u64),
            json!(u64::MAX),
            json!(-1),
            json!(1.5),
        ] {
            value[field] = invalid;
            assert!(
                serde_json::from_value::<RouteBindPeer>(json!({"caller": value.clone()})).is_err()
            );
            assert!(serde_json::from_value::<CallerAttestation>(value.clone()).is_err());
            let mut incomplete = value.clone();
            incomplete.as_object_mut().unwrap().remove("incarnation");
            assert!(
                serde_json::from_value::<RouteBindPeer>(json!({"caller": incomplete})).is_err()
            );
        }
    }
}

#[test]
fn reader_skew_unknown_claim_is_not_own_and_unknown_origin_is_other() {
    let mut value = caller();
    value["claim"] = json!("future_claim");
    value["origin"] = json!("future_origin");
    let peer: RouteBindPeer = serde_json::from_value(json!({"caller": value})).unwrap();
    let attestation = peer.caller.unwrap();
    assert_ne!(attestation.claim, CallerClaim::Own);
    assert_eq!(attestation.origin, CallerOrigin::Other);
}

#[test]
fn reader_skew_caller_is_flattened_without_changing_exhaustive_route_bind() {
    let raw = json!({"op":"route.bind", "route_channel":1, "epoch":2,
        "target":{"kind":"management_surface", "module_id":"provider"}, "identity":{"harness":"test","session":"reader"}});
    let bind: ModuleControlRequest = serde_json::from_value(raw.clone()).unwrap();
    // Exhaustive destructuring remains source-compatible for existing providers.
    match &bind {
        ModuleControlRequest::RouteBind {
            route_channel: _,
            epoch: _,
            target: _,
            identity: _,
            principal: _,
            consumer_capabilities: _,
            role_versions: _,
            admission_facts: _,
            scope: _,
            call_mode: _,
        } => {}
        ModuleControlRequest::HealthCheck {} => panic!("expected bind"),
    }
    let empty = RouteBindPeer::new();
    assert!(empty.is_empty());
    assert_eq!(
        encode_route_bind(&bind, &empty).unwrap(),
        serde_json::to_vec(&bind).unwrap()
    );
    let attestation: CallerAttestation = serde_json::from_value(caller()).unwrap();
    let peer = RouteBindPeer::new().with_caller(Some(attestation.clone()));
    assert!(!peer.is_empty());
    let encoded = encode_route_bind(&bind, &peer).unwrap();
    let value: Value = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(value["caller"], caller());
    assert!(value.get("peer").is_none());
    assert_eq!(
        serde_json::from_slice::<RouteBindPeer>(&encoded)
            .unwrap()
            .caller,
        Some(attestation)
    );
    assert_eq!(
        serde_json::from_slice::<ModuleControlRequest>(&encoded).unwrap(),
        bind
    );
    assert_eq!(CALLER_ATTESTATION_CAPABILITY, "caller-attestation/v1");
}

#[test]
fn reader_skew_authority_changed_reopens_and_stale_epoch_refusal_decodes() {
    let reason: RouteCloseReason = serde_json::from_str("\"authority_changed\"").unwrap();
    assert!(reason.may_reopen());
    assert!(!RouteCloseReason::Disable.may_reopen());
    assert_eq!(serde_json::to_value(reason).unwrap(), "authority_changed");
    let error: crate::ErrorBody = serde_json::from_value(json!({
        "code":"stale_authority_epoch", "message":"authority changed during bind"}))
    .unwrap();
    assert_eq!(error.code, error_codes::STALE_AUTHORITY_EPOCH);
    assert!(error_codes::is_retryable_route_open(&error.code));
    assert!(!error_codes::is_established_route_dead(
        Flags::new(false, crate::Priority::Interactive, true),
        &error.code
    ));
    assert_eq!(ROUTE_CLOSE_REASONS_CAPABILITY, "route-close-reasons/v1");
    let table: Value =
        serde_json::from_str(include_str!("../tests/golden/decision_tables.json")).unwrap();
    assert_eq!(table["route_open_retryable"][&error.code], "retryable");
    assert_eq!(
        table["route_close_disposition"]["authority_changed"],
        "may_reopen"
    );
}
