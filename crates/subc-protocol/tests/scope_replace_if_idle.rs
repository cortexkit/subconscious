use subc_protocol::{
    scope::{ScopeKind, ScopeRecord, ScopeSelector},
    Principal,
};

#[test]
fn idle_and_presence_unset_preserve_golden_bytes() {
    let record = ScopeRecord::new("s", 1, ScopeKind::Head);
    let selector = ScopeSelector::new(Principal::Direct, "s", Some(1));
    assert_eq!(
        serde_json::to_string(&record).unwrap(),
        r#"{"ref":"s","scope_epoch":1,"kind":"head"}"#
    );
    assert_eq!(
        serde_json::to_string(&selector).unwrap(),
        r#"{"owner":{"kind":"direct"},"ref":"s","scope_epoch":1}"#
    );
    assert_eq!(
        serde_json::from_str::<ScopeRecord>(r#"{"ref":"s","scope_epoch":1,"kind":"head"}"#)
            .unwrap(),
        record
    );
    assert_eq!(
        serde_json::from_str::<ScopeSelector>(
            r#"{"owner":{"kind":"direct"},"ref":"s","scope_epoch":1}"#
        )
        .unwrap(),
        selector
    );
}

#[test]
fn idle_and_presence_flags_round_trip() {
    let record = ScopeRecord::new("s", 1, ScopeKind::Head).with_replace_only_if_idle(true);
    let selector = ScopeSelector::new(Principal::Direct, "s", Some(1)).with_hold_presence(true);
    let json = serde_json::to_string(&record).unwrap();
    assert!(json.contains("\"replace_only_if_idle\":true"));
    assert_eq!(serde_json::from_str::<ScopeRecord>(&json).unwrap(), record);
    let json = serde_json::to_string(&selector).unwrap();
    assert!(json.contains("\"hold_presence\":true"));
    assert_eq!(
        serde_json::from_str::<ScopeSelector>(&json).unwrap(),
        selector
    );
}

#[test]
fn scope_epoch_in_use_is_terminal() {
    assert_eq!(
        subc_protocol::error_codes::SCOPE_EPOCH_IN_USE,
        "scope_epoch_in_use"
    );
    assert!(!subc_protocol::error_codes::is_retryable_route_open(
        "scope_epoch_in_use"
    ));
}
