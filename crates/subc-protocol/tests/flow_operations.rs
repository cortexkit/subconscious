use serde_json::{json, Value};
use subc_protocol::manifest::{
    validate_hello_flow_operations, CapabilityDeclarations, Concurrency, FlowOperationsError,
    ManagementOperation, ManagementOperationKind, ModuleManifest, ProviderRole,
    MAX_FLOW_OPERATIONS,
};

fn management(names: &[String]) -> ProviderRole {
    ProviderRole::ManagementSurface {
        operations: names
            .iter()
            .map(|name| ManagementOperation {
                name: name.clone(),
                kind: ManagementOperationKind::Query,
                description: None,
            })
            .collect(),
        config_schema: json!({}),
        observability: Vec::new(),
        identity_scope: Vec::new(),
        concurrency: Concurrency::Serial,
    }
}

fn capabilities(provides: &[&str]) -> Option<CapabilityDeclarations> {
    Some(CapabilityDeclarations {
        provides: provides.iter().map(|name| name.to_string()).collect(),
        requires: Vec::new(),
        must_never_reach: Vec::new(),
    })
}

fn names(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| name.to_string()).collect()
}

/// A manifest declaring `diagnostics.findings` and `reset`, with the given
/// capabilities and no flow-operation list.
fn base(provides: &[&str]) -> ModuleManifest {
    ModuleManifest::builder("flow-ops", "1.0.0")
        .provides(vec![management(&names(&["diagnostics.findings", "reset"]))])
        .capabilities(capabilities(provides))
        .build()
}

/// Encodes `manifest`, sets the raw `flow_operations` value, and decodes it the
/// way the daemon does at HELLO.
fn decode_with(manifest: &ModuleManifest, flow_operations: Value) -> (ModuleManifest, Value) {
    let mut wire = serde_json::to_value(manifest).unwrap();
    wire["flow_operations"] = flow_operations;
    let hello = json!({ "manifest": wire.clone() });
    (serde_json::from_value(wire).unwrap(), hello)
}

#[test]
fn absent_flow_operations_preserve_legacy_bytes() {
    let bytes = br#"{"module_id":"flow-ops","module_version":"1.0.0","protocol_ver":2,"provides":[],"capabilities":{"provides":["flow-scopes/v1"],"requires":[],"must_never_reach":[]}}"#;
    let decoded: ModuleManifest = serde_json::from_slice(bytes).unwrap();
    assert_eq!(decoded.flow_operations, None);
    assert_eq!(serde_json::to_vec(&decoded).unwrap(), bytes);
    assert!(validate_hello_flow_operations(&json!({ "manifest": decoded })).is_ok());
    // JSON null reads as absent, not as a list and not as an error.
    let (nulled, hello) = decode_with(&decoded, Value::Null);
    assert_eq!(nulled, decoded);
    assert!(validate_hello_flow_operations(&hello).is_ok());
}

#[test]
fn present_flow_operations_round_trip_unchanged() {
    for declared in [
        names(&["reset", "diagnostics.findings"]),
        names(&["diagnostics.findings"]),
        Vec::new(),
    ] {
        for capability in ["flow-scopes/v1", "call-modes/v1"] {
            let manifest = ModuleManifest::builder("flow-ops", "1.0.0")
                .provides(vec![management(&names(&["diagnostics.findings", "reset"]))])
                .capabilities(capabilities(&[capability]))
                .flow_operations(Some(declared.clone()))
                .build();
            assert!(manifest.validate_flow_operations().is_ok());
            let encoded = serde_json::to_value(&manifest).unwrap();
            assert_eq!(encoded["flow_operations"], json!(declared));
            let decoded: ModuleManifest = serde_json::from_value(encoded.clone()).unwrap();
            assert_eq!(decoded, manifest);
            assert_eq!(decoded.flow_operations.as_ref(), Some(&declared));
            assert!(validate_hello_flow_operations(&json!({ "manifest": encoded })).is_ok());
        }
    }
}

#[test]
fn unknown_operation_drops_whole_list_but_keeps_manifest() {
    let manifest = base(&["flow-scopes/v1"]);
    let (decoded, hello) = decode_with(&manifest, json!(["diagnostics.findings", "missing"]));
    assert_eq!(
        decoded.flow_operations, None,
        "the valid name is dropped too"
    );
    assert_eq!(decoded, manifest, "the rest of the manifest is untouched");
    let err = validate_hello_flow_operations(&hello).unwrap_err();
    assert_eq!(
        err,
        FlowOperationsError::UnknownOperation {
            operation: "missing".into()
        }
    );
    assert!(err.to_string().contains("'missing'"));
}

#[test]
fn flow_operations_without_flow_capability_are_dropped() {
    for provides in [&[][..], &["agent-run-scopes/v1"][..]] {
        let manifest = base(provides);
        let (decoded, hello) = decode_with(&manifest, json!(["diagnostics.findings"]));
        assert_eq!(decoded.flow_operations, None);
        assert_eq!(
            validate_hello_flow_operations(&hello),
            Err(FlowOperationsError::FlowScopesNotDeclared)
        );
    }
    let no_capabilities = ModuleManifest::builder("flow-ops", "1.0.0")
        .provides(vec![management(&names(&["reset"]))])
        .flow_operations(Some(Vec::new()))
        .build();
    assert_eq!(
        no_capabilities.validate_flow_operations(),
        Err(FlowOperationsError::FlowScopesNotDeclared)
    );
    let decoded: ModuleManifest =
        serde_json::from_value(serde_json::to_value(&no_capabilities).unwrap()).unwrap();
    assert_eq!(decoded.flow_operations, None);
}

#[test]
fn duplicate_or_malformed_flow_operations_are_dropped() {
    let manifest = base(&["flow-scopes/v1"]);
    for (value, expected) in [
        (
            json!(["reset", "diagnostics.findings", "reset"]),
            FlowOperationsError::DuplicateOperation {
                operation: "reset".into(),
            },
        ),
        (json!("reset"), FlowOperationsError::NotAList),
        (json!([1]), FlowOperationsError::NotAList),
        (json!({"reset": true}), FlowOperationsError::NotAList),
    ] {
        let (decoded, hello) = decode_with(&manifest, value.clone());
        assert_eq!(decoded.flow_operations, None, "{value}");
        assert_eq!(
            validate_hello_flow_operations(&hello),
            Err(expected),
            "{value}"
        );
    }
}

#[test]
fn flow_operations_accept_exactly_the_cap() {
    assert_eq!(MAX_FLOW_OPERATIONS, 256);
    let declared: Vec<String> = (0..=MAX_FLOW_OPERATIONS)
        .map(|index| format!("op.{index}"))
        .collect();
    let manifest = ModuleManifest::builder("flow-ops", "1.0.0")
        .provides(vec![management(&declared)])
        .capabilities(capabilities(&["flow-scopes/v1"]))
        .build();
    let at_cap = &declared[..MAX_FLOW_OPERATIONS];
    let (decoded, hello) = decode_with(&manifest, json!(at_cap));
    assert_eq!(decoded.flow_operations.as_deref(), Some(at_cap));
    assert!(validate_hello_flow_operations(&hello).is_ok());

    let (decoded, hello) = decode_with(&manifest, json!(declared));
    assert_eq!(decoded.flow_operations, None);
    assert_eq!(
        validate_hello_flow_operations(&hello),
        Err(FlowOperationsError::TooMany { count: 257 })
    );
}

/// A decoder that predates `flow_operations` sees it as an unknown top-level
/// key. Manifest decoding drops unknown top-level keys instead of refusing the
/// manifest, so an older daemon registers a module that declares the list and
/// simply does not relay it. This pins that leniency with keys this decoder
/// does not know either.
#[test]
fn unknown_manifest_keys_beside_flow_operations_do_not_refuse_decoding() {
    let mut wire = serde_json::to_value(base(&["flow-scopes/v1"])).unwrap();
    wire["flow_operations"] = json!(["diagnostics.findings"]);
    wire["flow_operations_v2"] = json!({"future": ["diagnostics.findings"]});
    let decoded: ModuleManifest = serde_json::from_value(wire).unwrap();
    assert_eq!(
        decoded.flow_operations,
        Some(names(&["diagnostics.findings"]))
    );
    let mut without = decoded.clone();
    without.flow_operations = None;
    assert_eq!(without, base(&["flow-scopes/v1"]));
}
