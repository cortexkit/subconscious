use serde_json::{json, Value};
use subc_protocol::manifest::{
    validate_hello_role_declarations, ModuleManifest, MAX_ROLE_DECLARATIONS_BYTES,
};

fn decode(declarations: Value) -> ModuleManifest {
    let mut wire = serde_json::to_value(ModuleManifest::builder("roles", "1.0.0").build()).unwrap();
    wire["role_declarations"] = declarations;
    serde_json::from_value(wire).unwrap()
}

#[test]
fn role_declarations_round_trip_opaque_objects() {
    let declarations = json!({
        "routing/v1": {"consumers": [{"name": "chat", "default_demand": 2}]},
        "future/v2": {"unknown": [null, true, 3, {"nested": "λ"}]}
    });
    let manifest = ModuleManifest::builder("roles", "1.0.0")
        .role_declarations(serde_json::from_value(declarations.clone()).unwrap())
        .build();
    let bytes = serde_json::to_vec(&manifest).unwrap();
    assert_eq!(
        serde_json::from_slice::<ModuleManifest>(&bytes).unwrap(),
        manifest
    );
    assert_eq!(
        serde_json::to_value(&manifest).unwrap()["role_declarations"],
        declarations
    );
    assert_eq!(decode(declarations), manifest);
}

#[test]
fn absent_or_empty_role_declarations_preserve_legacy_bytes() {
    let bytes = br#"{"module_id":"roles","module_version":"1.0.0","protocol_ver":2,"provides":[]}"#;
    let absent: ModuleManifest = serde_json::from_slice(bytes).unwrap();
    assert!(absent.role_declarations.is_empty());
    assert_eq!(serde_json::to_vec(&absent).unwrap(), bytes);
    let empty = decode(json!({}));
    assert_eq!(empty, absent);
    assert_eq!(serde_json::to_vec(&empty).unwrap(), bytes);
    assert!(validate_hello_role_declarations(&json!({"manifest": {}})).is_ok());
    assert!(
        validate_hello_role_declarations(&json!({"manifest": {"role_declarations": {}}})).is_ok()
    );
}

#[test]
fn malformed_role_declarations_drop_whole_map() {
    for malformed in [
        json!(null),
        json!(false),
        json!(42),
        json!("opaque"),
        json!([]),
        json!({"routing/v1": {}, "bad key": {}}),
        json!({"routing/v1": {}, "Upper/v1": {}}),
        json!({"routing/v1": {}, "broken/v1": []}),
        json!({"routing/v1": {}, "broken/v1": null}),
        json!({"routing/v1": {}, "broken/v1": 1}),
        json!({"routing/v1": {}, "broken/v1": "object"}),
        json!({"routing/v1": {}, "broken/v1": true}),
    ] {
        assert!(
            validate_hello_role_declarations(&json!({"manifest": {
                "role_declarations": malformed.clone()
            }}))
            .is_err(),
            "{malformed}"
        );
        assert!(decode(malformed).role_declarations.is_empty());
    }
}

#[test]
fn role_declarations_accept_exact_compact_byte_cap() {
    assert_eq!(MAX_ROLE_DECLARATIONS_BYTES, 16_384);
    let mut declarations = json!({"routing/v1": {"padding": ""}});
    let overhead = serde_json::to_vec(&declarations).unwrap().len();
    declarations["routing/v1"]["padding"] = json!("x".repeat(16_384 - overhead));
    assert_eq!(serde_json::to_vec(&declarations).unwrap().len(), 16_384);
    assert!(validate_hello_role_declarations(&json!({"manifest": {
        "role_declarations": declarations.clone()
    }}))
    .is_ok());
    assert_eq!(
        serde_json::to_value(decode(declarations.clone())).unwrap()["role_declarations"],
        declarations
    );
    declarations["routing/v1"]["padding"] = json!("x".repeat(16_385 - overhead));
    assert!(decode(declarations).role_declarations.is_empty());
}

#[test]
fn oversized_role_declarations_drop_whole_map() {
    let declarations = json!({
        "routing/v1": {"consumers": []},
        "overflow/v1": {"padding": "λ".repeat(8192)}
    });
    assert!(serde_json::to_vec(&declarations).unwrap().len() > 16_384);
    assert_eq!(
        validate_hello_role_declarations(&json!({"manifest": {
            "role_declarations": declarations.clone()
        }}))
        .unwrap_err()
        .to_string(),
        "role_declarations exceeds 16384 compact JSON bytes"
    );
    assert!(
        decode(declarations).role_declarations.is_empty(),
        "even the small valid entry must be dropped"
    );
    let combined = json!({
        "routing/v1": {"padding": "x".repeat(9000)},
        "overflow/v1": {"padding": "x".repeat(9000)}
    });
    assert!(
        decode(combined).role_declarations.is_empty(),
        "the cap covers the whole map, not individual entries"
    );
}
