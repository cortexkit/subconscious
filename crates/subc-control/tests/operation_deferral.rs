use subc_control::CatalogEntry;

#[test]
fn operation_deferral_catalog_roundtrips_and_omits_absence() {
    let legacy = CatalogEntry::new("provider", vec![], vec![]);
    assert!(serde_json::to_value(&legacy)
        .unwrap()
        .get("operation_deferral")
        .is_none());
    let map = [
        ("tool:read".to_string(), true),
        ("management:reset".to_string(), true),
    ]
    .into_iter()
    .collect();
    let entry = legacy.with_operation_deferral(Some(map));
    let value = serde_json::to_value(&entry).unwrap();
    assert_eq!(value["operation_deferral"]["tool:read"], true);
    assert_eq!(
        serde_json::from_value::<CatalogEntry>(value).unwrap(),
        entry
    );
}
