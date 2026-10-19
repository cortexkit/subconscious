use std::collections::BTreeMap;

use serde_json::json;
use subc_protocol::manifest::{
    Concurrency, ManagementOperation, ManagementOperationKind, ModuleManifest, OperationDelivery,
    ProviderRole,
};

fn manifest() -> ModuleManifest {
    ModuleManifest::builder("delivery-test", "1.0.0")
        .provides(vec![ProviderRole::ManagementSurface {
            operations: [
                ("status", ManagementOperationKind::Query),
                ("future", ManagementOperationKind::Query),
                ("ordinary", ManagementOperationKind::Query),
                ("reset", ManagementOperationKind::Mutate),
            ]
            .into_iter()
            .map(|(name, kind)| ManagementOperation {
                name: name.into(),
                kind,
                description: None,
            })
            .collect(),
            config_schema: json!({}),
            observability: Vec::new(),
            identity_scope: Vec::new(),
            concurrency: Concurrency::Serial,
        }])
        .operation_delivery(BTreeMap::from([
            ("status".into(), OperationDelivery::WATCH),
            (
                "future".into(),
                OperationDelivery::Other("stream-v2".into()),
            ),
            ("reset".into(), OperationDelivery::WATCH),
            ("missing".into(), OperationDelivery::WATCH),
        ]))
        .build()
}

#[test]
fn operation_delivery_round_trips_watch_and_unknown_strings() {
    let manifest = manifest();
    let encoded = serde_json::to_value(&manifest).unwrap();
    assert_eq!(
        encoded["operation_delivery"],
        json!({"status": "watch", "future": "stream-v2", "reset": "watch", "missing": "watch"})
    );
    let decoded: ModuleManifest = serde_json::from_value(encoded.clone()).unwrap();
    assert_eq!(decoded, manifest);
    assert_eq!(serde_json::to_value(&decoded).unwrap(), encoded);
    assert!(decoded.operation_delivery["status"].is_watch());
    assert!(!decoded.operation_delivery["future"].is_watch());
}

#[test]
fn absent_or_empty_operation_delivery_preserves_legacy_bytes() {
    let manifest = ModuleManifest::builder("unary", "1.0.0").build();
    let encoded = serde_json::to_vec(&manifest).unwrap();
    assert_eq!(
        encoded,
        br#"{"module_id":"unary","module_version":"1.0.0","protocol_ver":2,"provides":[]}"#
    );
    let decoded: ModuleManifest = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(decoded, manifest);
    let mut explicit_empty = serde_json::to_value(&manifest).unwrap();
    explicit_empty["operation_delivery"] = json!({});
    let decoded: ModuleManifest = serde_json::from_value(explicit_empty).unwrap();
    assert_eq!(decoded, manifest);
    assert_eq!(serde_json::to_vec(&decoded).unwrap(), encoded);
}

#[test]
fn malformed_operation_delivery_drops_the_whole_field_without_rejecting_manifest() {
    for malformed in [
        json!(null),
        json!("watch"),
        json!(["watch"]),
        json!(42),
        json!(true),
        json!({"status": "watch", "broken": 42}),
        json!({"status": "watch", "broken": null}),
        json!({"status": "watch", "broken": {"mode": "watch"}}),
    ] {
        let mut encoded = serde_json::to_value(manifest()).unwrap();
        encoded["operation_delivery"] = malformed;
        let decoded: ModuleManifest = serde_json::from_value(encoded).unwrap();
        assert!(decoded.operation_delivery.is_empty());
        assert!(!decoded.is_watch_operation("status"));
        assert_eq!(decoded.provides, manifest().provides);
    }
}

#[test]
fn is_watch_operation_requires_a_declared_query() {
    let manifest = manifest();
    assert!(manifest.is_watch_operation("status"));
    assert!(!manifest.is_watch_operation("reset"));
    assert!(!manifest.is_watch_operation("missing"));
    assert!(!manifest.is_watch_operation("future"));
    assert!(!manifest.is_watch_operation("ordinary"));
    assert!(!manifest.is_watch_operation("STATUS"));
    assert!(!ModuleManifest::builder("no-surface", "1.0.0")
        .operation_delivery(BTreeMap::from([(
            "status".into(),
            OperationDelivery::WATCH
        )]))
        .build()
        .is_watch_operation("status"));
}
