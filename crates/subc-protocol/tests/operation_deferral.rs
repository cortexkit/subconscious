use serde_json::{json, Value};
use subc_protocol::manifest::{validate_hello_operation_deferral, ModuleManifest};

fn wire() -> Value {
    json!({"module_id":"provider","module_version":"1.0.0","protocol_ver":2,
        "capabilities":{"provides":["scope-federated/v1"]},
        "provides":[{"role":"management_surface","operations":[{"name":"reset","kind":"mutate"}],"config_schema":{},"observability":[],"identity_scope":[]}]})
}

#[test]
fn operation_deferral_valid_map_roundtrips_and_absence_preserves_bytes() {
    for value in [
        json!({}),
        json!({"tool:read.file":true,"management:reset":true}),
    ] {
        let mut wire = wire();
        wire["operation_deferral"] = value.clone();
        let manifest: ModuleManifest = serde_json::from_value(wire.clone()).unwrap();
        assert!(manifest.validate_operation_deferral().is_ok());
        assert_eq!(
            serde_json::to_value(&manifest).unwrap()["operation_deferral"],
            value
        );
        assert!(validate_hello_operation_deferral(&json!({"manifest":wire})).is_ok());
    }
    let manifest: ModuleManifest = serde_json::from_value(wire()).unwrap();
    assert!(serde_json::to_value(&manifest)
        .unwrap()
        .get("operation_deferral")
        .is_none());
}

#[test]
fn operation_deferral_invalid_map_is_dropped_whole_without_refusing_manifest() {
    let oversized: serde_json::Map<String, Value> = (0..257)
        .map(|n| (format!("tool:t{n}"), json!(true)))
        .collect();
    for invalid in [
        json!({"tool:valid":true,"management:missing":true}),
        json!({"tool:valid":true,"tool:bad":false}),
        json!({"tool:t":1}),
        json!({"tool:t":"true"}),
        json!({"tool:t":null}),
        json!([]),
        json!({"unknown:op":true}),
        json!({"tool:":true}),
        json!({"tool:bad:name":true}),
        json!({"tool:with space":true}),
        json!({format!("tool:{}", "x".repeat(129)):true}),
        Value::Object(oversized),
    ] {
        let mut wire = wire();
        wire["operation_deferral"] = invalid;
        let manifest: ModuleManifest = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(manifest.operation_deferral, None);
        assert!(validate_hello_operation_deferral(&json!({"manifest":wire})).is_err());
    }
    let mut wire = wire();
    wire["capabilities"]["provides"] = json!([]);
    wire["operation_deferral"] = json!({"tool:read":true});
    assert!(serde_json::from_value::<ModuleManifest>(wire.clone())
        .unwrap()
        .operation_deferral
        .is_none());
    assert!(validate_hello_operation_deferral(&json!({"manifest":wire})).is_err());
}
