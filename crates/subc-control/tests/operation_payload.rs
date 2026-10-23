use std::collections::BTreeMap;

use serde_json::json;
use subc_control::CatalogEntry;
use subc_protocol::manifest::OperationPayload;

#[test]
fn catalog_payload_map_round_trips_and_empty_preserves_legacy_bytes() {
    let legacy = CatalogEntry::new("legacy", vec![], vec![]);
    let bytes = serde_json::to_vec(&legacy).unwrap();
    assert_eq!(
        bytes,
        br#"{"module_id":"legacy","ready":true,"roles":[],"control_ops":[]}"#
    );
    assert_eq!(
        serde_json::from_slice::<CatalogEntry>(&bytes).unwrap(),
        legacy
    );
    assert_eq!(
        serde_json::to_vec(&legacy.clone().with_operation_payload(BTreeMap::new())).unwrap(),
        bytes
    );

    let declared = legacy.with_operation_payload(BTreeMap::from([(
        "chunk".into(),
        OperationPayload::new(8_388_608),
    )]));
    let encoded = serde_json::to_value(&declared).unwrap();
    assert_eq!(
        encoded["operation_payload"],
        json!({"chunk":{"max_bytes":8_388_608}})
    );
    assert_eq!(
        serde_json::from_value::<CatalogEntry>(encoded).unwrap(),
        declared
    );
}
