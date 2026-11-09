use subc_protocol::scope::{
    FederatedAttributes, FederatedPeerClass, RemoteClaim, ScopeAttributes, ScopeKind, ScopeRecord,
    MAX_SCOPE_ATTRIBUTE_BYTES,
};

#[test]
fn federated_claim_constructors_preserve_wire_fields() {
    let claims = [
        (
            RemoteClaim::agent("agent"),
            serde_json::json!({"kind":"agent","agent_id":"agent"}),
        ),
        (
            RemoteClaim::worker("owner", "a".repeat(64), Some("agent".into())),
            serde_json::json!({"kind":"worker","owner_module_id":"owner","worker_ref":"a".repeat(64),"on_behalf_of":"agent"}),
        ),
        (
            RemoteClaim::run("agent"),
            serde_json::json!({"kind":"run","agent_id":"agent"}),
        ),
        (
            RemoteClaim::flow("flow", 1, "install", "a".repeat(64)),
            serde_json::json!({"kind":"flow","flow_id":"flow","flow_version":1,"flow_install_id":"install","flow_code_hash":"a".repeat(64)}),
        ),
    ];
    for (claim, wire) in claims {
        claim.validate().unwrap();
        assert_eq!(serde_json::to_value(&claim).unwrap(), wire);
        assert_eq!(serde_json::from_value::<RemoteClaim>(wire).unwrap(), claim);
    }
}

#[test]
fn maximal_federated_record_fits_scope_attribute_budget() {
    let facts = FederatedAttributes::new(
        FederatedPeerClass::Computer,
        "a".repeat(32),
        "p".repeat(256),
        "🛰".repeat(80),
    )
    .with_remote_claim(Some(
        RemoteClaim::flow(
            "f".repeat(256),
            9_007_199_254_740_991,
            "i".repeat(256),
            "b".repeat(64),
        )
        .with_flow_name("🛰".repeat(80)),
    ));
    facts.validate().unwrap();
    let record = ScopeRecord::new("peer", 1, ScopeKind::Federated)
        .with_attributes(ScopeAttributes::new().with_federated(Some(facts)));
    for (label, bytes) in [
        (
            "attributes",
            serde_json::to_vec(&record.attributes).unwrap(),
        ),
        ("record", serde_json::to_vec(&record).unwrap()),
    ] {
        println!("maximal federated {label}: {} bytes", bytes.len());
        assert!(
            bytes.len() <= MAX_SCOPE_ATTRIBUTE_BYTES,
            "{label}: {} bytes exceeds {MAX_SCOPE_ATTRIBUTE_BYTES}",
            bytes.len()
        );
    }
}
