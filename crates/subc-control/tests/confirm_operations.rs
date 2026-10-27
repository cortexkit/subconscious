use serde_json::json;
use subc_control::CatalogEntry;

#[test]
fn catalog_confirm_operations_round_trip_and_absence_preserves_legacy_bytes() {
    let legacy = CatalogEntry::new("legacy", vec![], vec![]);
    let bytes = serde_json::to_vec(&legacy).unwrap();
    assert_eq!(
        bytes,
        br#"{"module_id":"legacy","ready":true,"roles":[],"control_ops":[]}"#
    );
    assert_eq!(
        serde_json::to_vec(&legacy.clone().with_confirm_operations(None)).unwrap(),
        bytes
    );
    let decoded: CatalogEntry = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(decoded.confirm_operations, None);

    for declared in [
        vec!["pair.device".to_string(), "reset".to_string()],
        Vec::new(),
    ] {
        let entry = legacy
            .clone()
            .with_confirm_operations(Some(declared.clone()));
        let encoded = serde_json::to_value(&entry).unwrap();
        assert_eq!(encoded["confirm_operations"], json!(declared));
        let decoded: CatalogEntry = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded, entry);
        assert_eq!(decoded.confirm_operations, Some(declared));
    }
}
