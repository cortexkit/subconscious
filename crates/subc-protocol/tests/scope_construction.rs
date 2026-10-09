use serde_json::json;
use subc_protocol::{
    error_codes,
    scope::{
        validate_run_id, ScopeAttributes, ScopeCarrier, ScopeEnd, ScopeEndOutcome, ScopeEndResult,
        ScopeKind, ScopeParent, ScopeRecord, AGENT_RUN_SCOPES_CAPABILITY,
        MAX_SCOPE_EXPIRY_AHEAD_MS, SCOPE_APPLY_OP,
    },
    session::{ModuleControlRequestFromModule, ModuleControlResponseToModule},
    tool_call::OpaqueFieldError,
    Principal,
};

#[test]
fn scope_construction_api_sets_required_and_optional_fields_from_a_dependent_crate() {
    let parent = ScopeParent::new(Principal::Direct, "parent-1", 3);
    let carrier = ScopeCarrier::new(Principal::Unverified)
        .with_targets(Some(vec!["run-provider".to_string()]));
    let attributes = ScopeAttributes::new()
        .with_agent_id(Some("agent-7".to_string()))
        .with_delegates(false)
        .with_flow_id(None)
        .with_run_id(Some("run:7".to_string()));
    let record = ScopeRecord::new("run-1", 9, ScopeKind::Ephemeral)
        .with_expires_at_ms(Some(1_725_000_030_000))
        .with_parent(Some(parent))
        .with_child_owners(vec![Principal::Direct])
        .with_carriers(vec![carrier])
        .with_attributes(attributes);
    assert_eq!(
        serde_json::to_value(&record).unwrap(),
        json!({
            "ref": "run-1",
            "scope_epoch": 9,
            "kind": "ephemeral",
            "expires_at_ms": 1_725_000_030_000u64,
            "parent": {"owner": {"kind": "direct"}, "ref": "parent-1", "scope_epoch": 3},
            "child_owners": [{"kind": "direct"}],
            "carriers": [{"principal": {"kind": "unverified"}, "targets": ["run-provider"]}],
            "attributes": {"agent_id": "agent-7", "run_id": "run:7"}
        })
    );
    let end = ScopeEnd::new("run-1", 9);
    assert_eq!(
        serde_json::to_value(&end).unwrap(),
        json!({"ref": "run-1", "scope_epoch": 9})
    );
    for (outcome, wire) in [
        (ScopeEndOutcome::Ended, "ended"),
        (ScopeEndOutcome::NotLive, "not_live"),
    ] {
        let result = ScopeEndResult::new("run-1", 9, outcome);
        let encoded = json!({"ref": "run-1", "scope_epoch": 9, "outcome": wire});
        assert_eq!(serde_json::to_value(&result).unwrap(), encoded);
        assert_eq!(
            serde_json::from_value::<ScopeEndResult>(encoded).unwrap(),
            result
        );
    }
    assert_eq!(SCOPE_APPLY_OP, "scope.apply");
    assert_eq!(AGENT_RUN_SCOPES_CAPABILITY, "agent-run-scopes/v1");
    assert_eq!(MAX_SCOPE_EXPIRY_AHEAD_MS, 86_400_000);
}

#[test]
fn scope_construction_defaults_omit_new_fields_and_builders_can_clear_optionals() {
    assert_eq!(ScopeAttributes::new(), ScopeAttributes::default());
    assert!(ScopeAttributes::new().is_empty());
    assert_eq!(
        serde_json::to_value(ScopeAttributes::new()).unwrap(),
        json!({})
    );
    let record = ScopeRecord::new("run-1", 9, ScopeKind::Ephemeral);
    let minimal = json!({"ref": "run-1", "scope_epoch": 9, "kind": "ephemeral"});
    assert_eq!(serde_json::to_value(&record).unwrap(), minimal);
    assert_eq!(
        serde_json::from_value::<ScopeRecord>(minimal).unwrap(),
        record
    );

    let cleared = record
        .clone()
        .with_expires_at_ms(Some(42))
        .with_expires_at_ms(None)
        .with_parent(Some(ScopeParent::new(Principal::Direct, "parent", 1)))
        .with_parent(None)
        .with_child_owners(vec![Principal::Direct])
        .with_child_owners(vec![])
        .with_carriers(vec![ScopeCarrier::new(Principal::Direct)])
        .with_carriers(vec![])
        .with_attributes(ScopeAttributes::new().with_run_id(Some("run:7".to_string())))
        .with_attributes(ScopeAttributes::new());
    assert_eq!(cleared, record);
    assert!(ScopeAttributes::new()
        .with_agent_id(Some("agent-7".to_string()))
        .with_agent_id(None)
        .with_delegates(true)
        .with_delegates(false)
        .with_flow_id(Some("flow:7".to_string()))
        .with_flow_id(None)
        .with_run_id(Some("run:7".to_string()))
        .with_run_id(None)
        .is_empty());
    assert_eq!(
        ScopeCarrier::new(Principal::Direct)
            .with_targets(Some(vec!["run-provider".to_string()]))
            .with_targets(None),
        ScopeCarrier::new(Principal::Direct)
    );
}

#[test]
fn run_only_attributes_are_not_empty_and_round_trip_on_bind_stamps() {
    let attributes = ScopeAttributes::new().with_run_id(Some("run:7".to_string()));
    assert!(!attributes.is_empty());
    assert_eq!(
        serde_json::to_value(&attributes).unwrap(),
        json!({"run_id": "run:7"})
    );
    let stamp = subc_protocol::scope::ScopeStamp {
        owner: Principal::Direct,
        scope_ref: "run-1".to_string(),
        scope_epoch: 9,
        kind: ScopeKind::Ephemeral,
        parent: None,
        parent_state: None,
        attributes,
        owner_authorized: true,
    };
    let encoded = serde_json::to_value(&stamp).unwrap();
    assert_eq!(encoded["attributes"], json!({"run_id": "run:7"}));
    assert!(encoded.get("expires_at_ms").is_none());
    assert_eq!(
        serde_json::from_value::<subc_protocol::scope::ScopeStamp>(encoded).unwrap(),
        stamp
    );
}

#[test]
fn run_id_uses_shared_opaque_token_bounds_and_names_its_field() {
    let field = "run_id";
    assert_eq!(validate_run_id(""), Err(OpaqueFieldError::Empty { field }));
    assert_eq!(validate_run_id("r"), Ok(()));
    assert_eq!(validate_run_id(&"r".repeat(256)), Ok(()));
    assert_eq!(
        validate_run_id(&"r".repeat(257)),
        Err(OpaqueFieldError::TooLong { field, length: 257 })
    );
    assert_eq!(validate_run_id("!~Run:7/step"), Ok(()));
    for bad in ["r é", "r\t", "ré", "r\u{7f}"] {
        let error = validate_run_id(bad).unwrap_err();
        assert_eq!(
            error,
            OpaqueFieldError::InvalidCharacter { field, index: 1 }
        );
        assert_eq!(error.field(), "run_id");
    }
}

#[test]
fn scope_apply_empty_lists_round_trip_and_ended_defaults_to_empty() {
    let request = ModuleControlRequestFromModule::ScopeApply {
        generation: 10,
        upsert: vec![],
        end: vec![],
    };
    let request_wire = json!({"op": "scope.apply", "generation": 10, "upsert": [], "end": []});
    assert_eq!(serde_json::to_value(&request).unwrap(), request_wire);
    assert_eq!(
        serde_json::from_value::<ModuleControlRequestFromModule>(request_wire).unwrap(),
        request
    );
    let reply = ModuleControlResponseToModule::ScopeApply {
        generation: 10,
        results: vec![],
        end_results: vec![],
        ended: vec![],
    };
    let reply_wire =
        json!({"op": "scope.apply", "generation": 10, "results": [], "end_results": []});
    assert_eq!(serde_json::to_value(&reply).unwrap(), reply_wire);
    assert_eq!(
        serde_json::from_value::<ModuleControlResponseToModule>(reply_wire).unwrap(),
        reply
    );
}

#[test]
fn scope_apply_end_entries_and_new_fields_keep_strict_wire_types() {
    assert!(serde_json::from_value::<ScopeEnd>(
        json!({"ref": "run-1", "scope_epoch": 9, "extra": true})
    )
    .is_err());
    assert!(serde_json::from_value::<ScopeEnd>(json!({"ref": "run-1"})).is_err());
    assert!(serde_json::from_value::<ScopeEndResult>(
        json!({"ref": "run-1", "scope_epoch": 9, "outcome": "unknown"})
    )
    .is_err());
    assert!(serde_json::from_value::<ScopeEndResult>(
        json!({"ref": "run-1", "scope_epoch": 9, "outcome": "ended", "extra": true})
    )
    .is_err());
    for invalid_expiry in [json!(-1), json!("42"), json!(1.5)] {
        assert!(serde_json::from_value::<ScopeRecord>(json!({
            "ref": "run-1", "scope_epoch": 9, "kind": "ephemeral", "expires_at_ms": invalid_expiry
        }))
        .is_err());
    }
    assert!(serde_json::from_value::<ScopeAttributes>(json!({"run_id": 42})).is_err());
}

#[test]
fn new_scope_refusal_codes_have_canonical_spelling_and_no_route_open_rows() {
    let table: serde_json::Value =
        serde_json::from_str(include_str!("golden/decision_tables.json")).unwrap();
    let rows = table["route_open_retryable"].as_object().unwrap();
    for (code, wire) in [
        (error_codes::SCOPE_EXPIRED, "scope_expired"),
        (
            error_codes::SCOPE_EXPIRY_IMMUTABLE,
            "scope_expiry_immutable",
        ),
        (error_codes::SCOPE_EXPIRY_TOO_FAR, "scope_expiry_too_far"),
        (error_codes::SCOPE_SYNC_REQUIRED, "scope_sync_required"),
        (
            error_codes::SCOPE_RUN_ID_WITHOUT_AGENT,
            "scope_run_id_without_agent",
        ),
        (
            error_codes::SCOPE_RUN_ID_WITH_FLOW_ID,
            "scope_run_id_with_flow_id",
        ),
        (
            error_codes::SCOPE_RUN_ID_DELEGATES,
            "scope_run_id_delegates",
        ),
    ] {
        assert_eq!(code, wire);
        assert!(!rows.contains_key(code), "{code} is not a route-open code");
    }
    assert_eq!(rows["target_agent_run_unsupported"], "terminal");
}
