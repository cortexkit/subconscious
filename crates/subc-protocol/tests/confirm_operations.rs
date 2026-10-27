use serde_json::{json, Value};
use subc_protocol::manifest::{
    validate_hello_confirm_operations, Concurrency, ConfirmOperationsError, ManagementOperation,
    ManagementOperationKind, ModuleManifest, ProviderRole, MAX_CONFIRM_OPERATIONS,
};

fn management(names: &[String]) -> ProviderRole {
    ProviderRole::ManagementSurface {
        operations: names
            .iter()
            .map(|name| ManagementOperation {
                name: name.clone(),
                kind: ManagementOperationKind::Mutate,
                description: None,
            })
            .collect(),
        config_schema: json!({}),
        observability: Vec::new(),
        identity_scope: Vec::new(),
        concurrency: Concurrency::Serial,
    }
}

fn names(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| name.to_string()).collect()
}

/// A manifest declaring `pair.device` and `reset`, with no confirm list.
fn base() -> ModuleManifest {
    ModuleManifest::builder("confirm-ops", "1.0.0")
        .provides(vec![management(&names(&["pair.device", "reset"]))])
        .build()
}

/// Encodes `manifest`, sets the raw `confirm_operations` value, and decodes it
/// the way the daemon does at HELLO.
fn decode_with(manifest: &ModuleManifest, confirm_operations: Value) -> (ModuleManifest, Value) {
    let mut wire = serde_json::to_value(manifest).unwrap();
    wire["confirm_operations"] = confirm_operations;
    let hello = json!({ "manifest": wire.clone() });
    (serde_json::from_value(wire).unwrap(), hello)
}

#[test]
fn absent_confirm_operations_preserve_legacy_bytes() {
    let bytes =
        br#"{"module_id":"confirm-ops","module_version":"1.0.0","protocol_ver":2,"provides":[]}"#;
    let decoded: ModuleManifest = serde_json::from_slice(bytes).unwrap();
    assert_eq!(decoded.confirm_operations, None);
    assert_eq!(serde_json::to_vec(&decoded).unwrap(), bytes);
    assert!(validate_hello_confirm_operations(&json!({ "manifest": decoded })).is_ok());
    // JSON null reads as absent, not as a list and not as an error.
    let (nulled, hello) = decode_with(&decoded, Value::Null);
    assert_eq!(nulled, decoded);
    assert!(validate_hello_confirm_operations(&hello).is_ok());
}

/// Unlike `flow_operations`, the list needs no capability beside it.
#[test]
fn present_confirm_operations_round_trip_unchanged() {
    for declared in [
        names(&["reset", "pair.device"]),
        names(&["reset"]),
        Vec::new(),
    ] {
        let manifest = ModuleManifest::builder("confirm-ops", "1.0.0")
            .provides(vec![management(&names(&["pair.device", "reset"]))])
            .confirm_operations(Some(declared.clone()))
            .build();
        assert!(manifest.validate_confirm_operations().is_ok());
        let encoded = serde_json::to_value(&manifest).unwrap();
        assert_eq!(encoded["confirm_operations"], json!(declared));
        let decoded: ModuleManifest = serde_json::from_value(encoded.clone()).unwrap();
        assert_eq!(decoded, manifest);
        assert_eq!(decoded.confirm_operations.as_ref(), Some(&declared));
        assert!(validate_hello_confirm_operations(&json!({ "manifest": encoded })).is_ok());
    }
}

#[test]
fn unknown_operation_drops_whole_list_but_keeps_manifest() {
    let manifest = base();
    let (decoded, hello) = decode_with(&manifest, json!(["reset", "missing"]));
    assert_eq!(
        decoded.confirm_operations, None,
        "the valid name is dropped too"
    );
    assert_eq!(decoded, manifest, "the rest of the manifest is untouched");
    let err = validate_hello_confirm_operations(&hello).unwrap_err();
    assert_eq!(
        err,
        ConfirmOperationsError::UnknownOperation {
            operation: "missing".into()
        }
    );
    assert!(err.to_string().contains("'missing'"));

    // A manifest without a management surface declares no operation at all.
    let no_surface = ModuleManifest::builder("confirm-ops", "1.0.0")
        .confirm_operations(Some(names(&["reset"])))
        .build();
    assert_eq!(
        no_surface.validate_confirm_operations(),
        Err(ConfirmOperationsError::UnknownOperation {
            operation: "reset".into()
        })
    );
    let decoded: ModuleManifest =
        serde_json::from_value(serde_json::to_value(&no_surface).unwrap()).unwrap();
    assert_eq!(decoded.confirm_operations, None);
}

#[test]
fn duplicate_or_malformed_confirm_operations_are_dropped() {
    let manifest = base();
    for (value, expected) in [
        (
            json!(["reset", "pair.device", "reset"]),
            ConfirmOperationsError::DuplicateOperation {
                operation: "reset".into(),
            },
        ),
        (json!("reset"), ConfirmOperationsError::NotAList),
        (json!([1]), ConfirmOperationsError::NotAList),
        (json!({"reset": true}), ConfirmOperationsError::NotAList),
    ] {
        let (decoded, hello) = decode_with(&manifest, value.clone());
        assert_eq!(decoded.confirm_operations, None, "{value}");
        assert_eq!(decoded, manifest, "{value}");
        assert_eq!(
            validate_hello_confirm_operations(&hello),
            Err(expected),
            "{value}"
        );
    }
}

#[test]
fn confirm_operations_accept_exactly_the_cap() {
    assert_eq!(MAX_CONFIRM_OPERATIONS, 256);
    let declared: Vec<String> = (0..=MAX_CONFIRM_OPERATIONS)
        .map(|index| format!("op.{index}"))
        .collect();
    let manifest = ModuleManifest::builder("confirm-ops", "1.0.0")
        .provides(vec![management(&declared)])
        .build();
    let at_cap = &declared[..MAX_CONFIRM_OPERATIONS];
    let (decoded, hello) = decode_with(&manifest, json!(at_cap));
    assert_eq!(decoded.confirm_operations.as_deref(), Some(at_cap));
    assert!(validate_hello_confirm_operations(&hello).is_ok());

    let (decoded, hello) = decode_with(&manifest, json!(declared));
    assert_eq!(decoded.confirm_operations, None);
    assert_eq!(
        validate_hello_confirm_operations(&hello),
        Err(ConfirmOperationsError::TooMany { count: 257 })
    );
}

/// The two lists are checked independently: dropping one leaves the other.
#[test]
fn an_invalid_confirm_list_leaves_a_valid_flow_list_alone() {
    let mut wire = serde_json::to_value(
        ModuleManifest::builder("confirm-ops", "1.0.0")
            .provides(vec![management(&names(&["pair.device", "reset"]))])
            .capabilities(Some(subc_protocol::manifest::CapabilityDeclarations {
                provides: vec!["flow-scopes/v1".to_string()],
                requires: Vec::new(),
                must_never_reach: Vec::new(),
            }))
            .build(),
    )
    .unwrap();
    wire["flow_operations"] = json!(["reset"]);
    wire["confirm_operations"] = json!(["reset", "reset"]);
    let decoded: ModuleManifest = serde_json::from_value(wire.clone()).unwrap();
    assert_eq!(decoded.flow_operations, Some(names(&["reset"])));
    assert_eq!(decoded.confirm_operations, None);

    wire["flow_operations"] = json!(["missing"]);
    wire["confirm_operations"] = json!(["pair.device"]);
    let decoded: ModuleManifest = serde_json::from_value(wire).unwrap();
    assert_eq!(decoded.flow_operations, None);
    assert_eq!(decoded.confirm_operations, Some(names(&["pair.device"])));
}
