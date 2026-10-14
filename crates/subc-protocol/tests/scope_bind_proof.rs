use subc_protocol::{
    scope::{validate_bind_proof_sha256, ScopeKind, ScopeRecord, ScopeSelector},
    Principal,
};

const PROOF: &str = "BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc";
const HASH: &str = "4bb06f8e4e3a7715d201d573d0aa423762e55dabd61a2c02278fa56cc6d294e0";

#[test]
fn scope_bind_proof_hash_requires_lowercase_sha256() {
    assert!(validate_bind_proof_sha256(HASH).is_ok());
    for hash in [HASH.to_uppercase(), HASH[..63].to_string(), "g".repeat(64)] {
        assert!(validate_bind_proof_sha256(&hash).is_err());
        let json = serde_json::json!({"ref":"s", "scope_epoch":1, "kind":"head", "bind_proof_sha256":hash});
        let err = serde_json::from_value::<ScopeRecord>(json).unwrap_err();
        assert!(err.to_string().contains("bind_proof_sha256"));
    }
}

#[test]
fn scope_bind_proof_unset_fields_keep_original_bytes() {
    assert_eq!(
        serde_json::to_string(&ScopeRecord::new("s", 1, ScopeKind::Head)).unwrap(),
        r#"{"ref":"s","scope_epoch":1,"kind":"head"}"#
    );
    assert_eq!(
        serde_json::to_string(&ScopeSelector::new(Principal::Direct, "s", Some(1))).unwrap(),
        r#"{"owner":{"kind":"direct"},"ref":"s","scope_epoch":1}"#
    );
}

#[test]
fn scope_bind_proof_debug_and_display_are_redacted() {
    let selector =
        ScopeSelector::new(Principal::Direct, "s", Some(1)).with_bind_proof(Some(PROOF.into()));
    for output in [format!("{selector:?}"), selector.to_string()] {
        assert!(output.contains("<redacted>"));
        assert!(!output.contains(PROOF));
    }
}

#[test]
fn scope_bind_proof_codes_are_terminal() {
    for (code, expected) in [
        (
            subc_protocol::error_codes::SCOPE_BIND_PROOF_REQUIRED,
            "scope_bind_proof_required",
        ),
        (
            subc_protocol::error_codes::SCOPE_BIND_PROOF_MISMATCH,
            "scope_bind_proof_mismatch",
        ),
    ] {
        assert_eq!(code, expected);
        assert!(!subc_protocol::error_codes::is_retryable_route_open(code));
    }
}
