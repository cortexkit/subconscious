use subc_control::{SupervisorEntry, SupervisorHealthStatus};

#[test]
fn observed_windows_nonce_source_survives_supervisor_list_wire_roundtrip() {
    let entry = SupervisorEntry::new(
        "module",
        "running",
        true,
        true,
        SupervisorHealthStatus::Unknown,
    )
    .with_launch_nonce_source(Some("pipe".into()));
    let wire = serde_json::to_value(&entry).unwrap();
    assert_eq!(wire["launch_nonce_source"], "pipe");
    let decoded: SupervisorEntry = serde_json::from_value(wire).unwrap();
    assert_eq!(decoded.launch_nonce_source.as_deref(), Some("pipe"));
}

#[test]
fn absent_observed_nonce_source_remains_absent_on_the_wire() {
    let entry = SupervisorEntry::new(
        "module",
        "stopped",
        true,
        false,
        SupervisorHealthStatus::Unknown,
    );
    let wire = serde_json::to_value(&entry).unwrap();
    assert!(wire.get("launch_nonce_source").is_none());
    let decoded: SupervisorEntry = serde_json::from_value(wire).unwrap();
    assert_eq!(decoded.launch_nonce_source, None);
}
