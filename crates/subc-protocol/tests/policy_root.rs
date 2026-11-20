use subc_protocol::session::{ModuleControlRequestFromModule, ModuleControlResponseToModule};

#[test]
fn policy_root_module_bodies_round_trip() {
    for wire in [
        include_str!("golden/module-policy-root-get.json"),
        include_str!("golden/module-policy-root-rotate.json"),
    ] {
        let request: ModuleControlRequestFromModule = serde_json::from_str(wire).unwrap();
        assert_eq!(serde_json::to_string(&request).unwrap(), wire.trim_end());
    }
    for wire in [
        include_str!("golden/module-policy-root-get-reply.json"),
        include_str!("golden/module-policy-root-rotate-reply.json"),
    ] {
        let reply: ModuleControlResponseToModule = serde_json::from_str(wire).unwrap();
        assert_eq!(serde_json::to_string(&reply).unwrap(), wire.trim_end());
    }
}

#[test]
fn policy_root_module_bodies_refuse_unknown_fields() {
    for request in [
        r#"{"op":"policy_root.get","key_id":"extra"}"#,
        r#"{"op":"policy_root.rotate","statement_b64":"e30=","signature_hex":"00","extra":true}"#,
    ] {
        assert!(serde_json::from_str::<ModuleControlRequestFromModule>(request).is_err());
    }
}

#[test]
fn policy_root_fingerprint_uses_eight_groups_of_four() {
    use subc_protocol::policy_root::{fingerprint, key_id};

    assert_eq!(key_id(&[0; 32]), "66687aadf862bd77");
    assert_eq!(
        fingerprint(&[0; 32]),
        "6668 7aad f862 bd77 6c8f c18b 8e9f 8e20"
    );
}
