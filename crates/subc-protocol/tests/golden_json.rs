use std::{fmt::Debug, fs, path::PathBuf};

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;
use subc_protocol::{
    error_codes,
    manifest::{
        Bindings, CapabilityDeclarations, Concurrency, ExecutionMode, IdentityBinding,
        IdentityScope, ManagementOperation, ManagementOperationKind, ManifestProvenance,
        ModuleManifest, ObservabilityKind, ObservabilitySurface, ProviderRole, SelfSignalEffect,
        SelfSignalKind, StorageBinding, StorageKind, StorageScope, Tool, TrustTier,
    },
    session::{
        HealthStatus, ModuleControlPush, ModuleControlRequest, ModuleControlRequestFromModule,
        ModuleControlResponse, ModuleControlResponseToModule,
    },
    BindIdentity, ErrorBody, ModuleHelloAckBody, ModuleHelloBody, Principal, RouteTarget,
    PROTOCOL_VERSION,
};

// Drift-prevention contract: UPDATE_GOLDEN=1 rewrites the committed JSON
// when a wire-shape change is intentional and the TS mirror is updated too.
#[test]
fn protocol_wire_shapes_match_golden_json_and_round_trip() {
    assert_golden("bind_identity", &bind_identity());
    assert_golden(
        "route_target_tool_provider",
        &RouteTarget::ToolProvider {
            module_id: "aft-tools".to_string(),
        },
    );
    assert_golden(
        "route_target_management_surface",
        &RouteTarget::ManagementSurface {
            module_id: "memory-mc".to_string(),
        },
    );
    assert_golden(
        "route_target_internal_service",
        &RouteTarget::InternalService {
            module_id: "llm-runner".to_string(),
            service_id: "llm".to_string(),
        },
    );
    assert_golden("error_body", &error_body());
    assert_golden("error_body_with_detail", &error_body_with_detail());
    assert_golden("error_body_module_removed", &error_body_module_removed());
    assert_golden(
        "error_body_capability_forbidden",
        &error_body_capability_forbidden(),
    );
    assert_golden("principal_reserved", &principal_reserved());
    assert_golden("principal_direct", &Principal::Direct);
    assert_golden("principal_unverified", &Principal::Unverified);
    assert_golden("module_hello_body", &module_hello_body());
    assert_golden(
        "module_hello_body_with_provenance",
        &module_hello_body_with_provenance(),
    );
    assert_golden("module_hello_ack_body", &module_hello_ack_body());
    assert_golden(
        "module_control_request_route_bind",
        &module_control_request(
            Some(vec!["elicitation".to_string(), "roots".to_string()]),
            Some(serde_json::json!({"schema": 1, "verified_class": "member"})),
        ),
    );
    assert_golden(
        "module_control_request_route_bind_without_consumer_capabilities",
        &module_control_request(None, None),
    );
    assert_golden(
        "module_control_response_route_bind_ack",
        &ModuleControlResponse::RouteBindAck {},
    );
    assert_golden(
        "module_control_request_health_check",
        &ModuleControlRequest::HealthCheck {},
    );
    assert_golden(
        "module_control_response_health_check",
        &ModuleControlResponse::HealthCheck {
            status: HealthStatus::Degraded,
            detail: Some("warming model".to_string()),
            metrics: Some(serde_json::json!({"queue_depth": 3})),
        },
    );
    assert_golden(
        "module_control_request_from_module_catalog_update",
        &ModuleControlRequestFromModule::CatalogUpdate {
            provides: provider_roles(),
            capabilities: None,
        },
    );
    assert_golden(
        "module_control_request_from_module_catalog_update_capabilities",
        &ModuleControlRequestFromModule::CatalogUpdate {
            provides: provider_roles(),
            capabilities: Some(CapabilityDeclarations {
                provides: vec!["credentials-provider/v1".to_string()],
                requires: Vec::new(),
                must_never_reach: vec!["federation-transport/v1".to_string()],
            }),
        },
    );
    assert_golden(
        "module_control_response_to_module_catalog_update",
        &ModuleControlResponseToModule::CatalogUpdate {},
    );
    assert_golden(
        "tool_with_description",
        &Tool {
            name: "memory.write".to_string(),
            description: Some("Persist a memory item".to_string()),
            execution_mode: ExecutionMode::Mutating,
            schema: serde_json::json!({"type": "object"}),
        },
    );
    assert_golden(
        "management_surface_manifest_with_description",
        &management_surface_manifest(Some(
            "List managed records and return their identifiers and metadata.",
        )),
    );
    assert_golden(
        "management_surface_manifest_without_description",
        &management_surface_manifest(None),
    );
    assert_golden(
        "module_control_push_route_status",
        &ModuleControlPush::RouteStatus {
            route_channel: 42,
            route_epoch: 7,
            status: "indexing".to_string(),
        },
    );
}

#[derive(Deserialize)]
struct LegacyModuleManifest {
    module_id: String,
}

#[test]
fn manifest_without_provenance_preserves_the_existing_hello_wire_shape() {
    let encoded = serde_json::to_value(module_hello_body()).expect("HELLO serializes");

    assert!(encoded["manifest"].get("provenance").is_none());
    assert_eq!(
        encoded,
        serde_json::from_str::<Value>(
            &fs::read_to_string(golden_path("module_hello_body")).expect("existing HELLO golden"),
        )
        .expect("existing HELLO golden is JSON"),
        "an absent provenance declaration must preserve the existing HELLO bytes"
    );
}

#[test]
fn self_signal_declaration_vectors_round_trip_and_refuse_missing_axes() {
    let with_signals_bytes = fs::read(golden_path("module_manifest_with_self_signals"))
        .expect("self-signal manifest vector is readable");
    let with_signals: Value =
        serde_json::from_slice(&with_signals_bytes).expect("self-signal manifest vector is JSON");
    let manifest: ModuleManifest =
        serde_json::from_value(with_signals.clone()).expect("self-signal manifest deserializes");
    let signals = manifest
        .self_signals
        .as_ref()
        .expect("present self_signals remain present");
    assert_eq!(signals.len(), 2);
    assert_eq!(signals[0].effect, SelfSignalEffect::Observe);
    assert_eq!(signals[1].effect, SelfSignalEffect::Mutate);
    assert_eq!(
        serde_json::to_vec(&manifest).expect("self-signal manifest serializes"),
        with_signals_bytes,
        "the declaration vector must round-trip byte-for-byte"
    );

    let without_signals = read_manifest_vector("module_manifest_without_self_signals");
    let manifest: ModuleManifest = serde_json::from_value(without_signals.clone())
        .expect("legacy manifest without self_signals deserializes");
    assert!(manifest.self_signals.is_none());
    let reserialized = serde_json::to_value(manifest).expect("legacy manifest serializes");
    assert!(
        reserialized.get("self_signals").is_none(),
        "an absent self_signals block must remain absent"
    );
    assert_eq!(reserialized, without_signals);

    for (name, field) in [
        ("module_manifest_self_signal_missing_effect", "effect"),
        (
            "module_manifest_self_signal_missing_anchored_to",
            "anchored_to",
        ),
    ] {
        let error = serde_json::from_value::<ModuleManifest>(read_manifest_vector(name))
            .expect_err("missing self-signal analysis axis must refuse the manifest");
        assert!(
            error.to_string().contains(field),
            "{name} refusal must name {field}: {error}"
        );
    }

    let unknown_kind = read_manifest_vector("module_manifest_self_signal_unknown_kind");
    let manifest: ModuleManifest = serde_json::from_value(unknown_kind.clone())
        .expect("unknown self-signal kind remains skew-tolerant");
    let signals = manifest
        .self_signals
        .as_ref()
        .expect("unknown kind declaration remains present");
    assert_eq!(signals[0].effect, SelfSignalEffect::Observe);
    assert_eq!(
        signals[0].kind,
        SelfSignalKind::Other("provider_pulse".to_string())
    );
    assert_eq!(
        serde_json::to_value(manifest).expect("unknown kind reserializes"),
        unknown_kind,
        "an unknown kind must survive re-serialization unchanged"
    );
}

#[test]
fn manifest_provenance_round_trips_all_facts_through_the_real_manifest_deserializer() {
    let hello = module_hello_body_with_provenance();
    let encoded = serde_json::to_value(&hello).expect("HELLO serializes");

    assert_eq!(
        encoded["manifest"]["provenance"],
        serde_json::json!({
            "build_git_sha": "0123456789abcdef0123456789abcdef01234567-dirty",
            "build_lock_digest": "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
            "wire_crate_version": "0.13.0",
            "store_schema_version": "42"
        })
    );
    let decoded: ModuleHelloBody = serde_json::from_value(encoded).expect("HELLO deserializes");
    assert_eq!(decoded, hello);
}

#[test]
fn manifest_provenance_omits_each_unavailable_fact_independently() {
    for field in [
        "build_git_sha",
        "build_lock_digest",
        "wire_crate_version",
        "store_schema_version",
    ] {
        let mut provenance = ManifestProvenance {
            build_git_sha: Some("commit".to_string()),
            build_lock_digest: Some("lock".to_string()),
            wire_crate_version: Some("wire".to_string()),
            store_schema_version: Some("schema".to_string()),
        };
        match field {
            "build_git_sha" => provenance.build_git_sha = None,
            "build_lock_digest" => provenance.build_lock_digest = None,
            "wire_crate_version" => provenance.wire_crate_version = None,
            "store_schema_version" => provenance.store_schema_version = None,
            _ => unreachable!("fixed provenance field list"),
        }

        let encoded = serde_json::to_value(&provenance).expect("provenance serializes");
        assert!(
            encoded.get(field).is_none(),
            "{field} must be omitted when unavailable"
        );
        let decoded: ManifestProvenance =
            serde_json::from_value(encoded).expect("partial provenance deserializes");
        assert_eq!(decoded, provenance);
    }
}

#[test]
fn manifest_provenance_rejects_non_printable_and_overlong_values() {
    for field in [
        "build_git_sha",
        "build_lock_digest",
        "wire_crate_version",
        "store_schema_version",
    ] {
        let mut encoded =
            serde_json::to_value(module_hello_body_with_provenance()).expect("HELLO serializes");
        encoded["manifest"]["provenance"][field] = Value::String("\u{1b}[2J".to_string());
        let error = serde_json::from_value::<ModuleHelloBody>(encoded)
            .expect_err("non-printable provenance must refuse the manifest");
        assert!(error.to_string().contains(field), "error: {error}");

        let mut encoded =
            serde_json::to_value(module_hello_body_with_provenance()).expect("HELLO serializes");
        encoded["manifest"]["provenance"][field] = Value::String("x".repeat(129));
        let error = serde_json::from_value::<ModuleHelloBody>(encoded)
            .expect_err("overlong provenance must refuse the manifest");
        assert!(error.to_string().contains(field), "error: {error}");
    }
}

#[test]
fn manifest_provenance_rejects_empty_values_for_every_field() {
    for field in [
        "build_git_sha",
        "build_lock_digest",
        "wire_crate_version",
        "store_schema_version",
    ] {
        let mut encoded =
            serde_json::to_value(module_hello_body_with_provenance()).expect("HELLO serializes");
        encoded["manifest"]["provenance"][field] = Value::String(String::new());
        let error = serde_json::from_value::<ModuleHelloBody>(encoded)
            .expect_err("empty provenance must refuse the manifest");
        assert!(error.to_string().contains(field), "error: {error}");
        assert!(
            error.to_string().contains("must not be empty"),
            "error: {error}"
        );
    }
}

#[test]
fn legacy_manifest_decoder_ignores_the_additive_provenance_block() {
    let encoded = serde_json::to_value(module_hello_body_with_provenance().manifest)
        .expect("current manifest serializes");

    let legacy: LegacyModuleManifest =
        serde_json::from_value(encoded).expect("old decoder ignores additive fields");
    assert_eq!(legacy.module_id, "aft-tools");
}

#[test]
fn deployed_management_surface_manifest_without_concurrency_defaults_to_module_managed() {
    let fixture = fs::read_to_string(golden_path(
        "management_surface_manifest_without_concurrency",
    ))
    .unwrap();
    let raw: Value = serde_json::from_str(&fixture).unwrap();
    assert!(raw["provides"][0].get("concurrency").is_none());

    let manifest: ModuleManifest = serde_json::from_value(raw).unwrap();
    let ProviderRole::ManagementSurface { concurrency, .. } = &manifest.provides[0] else {
        panic!("compatibility fixture must contain a management surface");
    };
    assert_eq!(*concurrency, Concurrency::ModuleManaged);

    let reserialized = serde_json::to_value(manifest).unwrap();
    assert_eq!(
        reserialized["provides"][0]["concurrency"],
        Value::String("module_managed".to_string())
    );
}

/// The wire constants are transcribed into all three client languages, and only
/// three of the four are protected by anything.
///
/// PROTOCOL_VERSION, HEADER_LEN and FROZEN_PREFIX_LEN all APPEAR IN ENCODED
/// BYTES, so a value drifting in one language changes that language's output and
/// the committed frame vectors catch it. MAX_FRAME_BODY_LEN is a THRESHOLD: it
/// appears in no byte of any frame, so no byte-parity fixture can observe it.
///
/// What each language had instead was a test importing its OWN constant and
/// asserting against itself -- true by construction in every language
/// independently, and therefore silent if one of them changed. Rust referenced
/// the constant in no test at all. A cap that drifted low would refuse frames a
/// peer considers legal; one that drifted high would accept an allocation the
/// daemon refuses, and both surface as a live wire failure rather than a build
/// one.
///
/// Publishing the values as a fixture gives the other languages something to
/// compare against that is not themselves.
#[test]
fn protocol_constants_are_published_for_cross_language_comparison() {
    let actual = serde_json::json!({
        "protocol_version": subc_protocol::PROTOCOL_VERSION,
        "min_supported_version": subc_protocol::MIN_SUPPORTED_VERSION,
        "header_len": subc_protocol::HEADER_LEN,
        "frozen_prefix_len": subc_protocol::FROZEN_PREFIX_LEN,
        "max_frame_body_len": subc_protocol::MAX_FRAME_BODY_LEN,
    });
    let path = golden_path("protocol_constants");
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        fs::write(
            &path,
            format!("{}\n", serde_json::to_string_pretty(&actual).unwrap()),
        )
        .unwrap();
    }
    let expected: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        actual, expected,
        "protocol constant drift; every client transcribes these, so changing one \
         here means changing it in the TypeScript and Swift clients in the same commit"
    );
}

fn assert_golden<T>(name: &str, value: &T)
where
    T: Serialize + DeserializeOwned + PartialEq + Debug,
{
    let actual = serde_json::to_value(value).unwrap();
    let path = golden_path(name);
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        fs::write(
            &path,
            format!("{}\n", serde_json::to_string_pretty(&actual).unwrap()),
        )
        .unwrap();
    }

    let expected: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        actual, expected,
        "golden JSON drift for {name}; rerun with UPDATE_GOLDEN=1 only after updating the TS mirror"
    );
    let decoded: T = serde_json::from_value(expected).unwrap();
    assert_eq!(
        &decoded, value,
        "golden JSON no longer decodes to canonical Rust {name}"
    );
}

fn read_manifest_vector(name: &str) -> Value {
    serde_json::from_str(
        &fs::read_to_string(golden_path(name)).expect("self-signal vector is readable"),
    )
    .expect("self-signal vector is JSON")
}

fn golden_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join(format!("{name}.json"))
}

fn bind_identity() -> BindIdentity {
    BindIdentity {
        project_root: PathBuf::from("/tmp/subc/project"),
        harness: "opencode".to_string(),
        session: "session-0001".to_string(),
    }
}

fn error_body() -> ErrorBody {
    // Deliberately detail-less: this golden pins that the absent field
    // serializes to NOTHING, keeping detail-less bodies byte-identical to the
    // pre-detail wire every deployed reader parses.
    ErrorBody {
        code: "config_divergence".to_string(),
        message: "active config differs".to_string(),
        detail: None,
    }
}

fn error_body_with_detail() -> ErrorBody {
    ErrorBody::new("spawn_failed", "child could not start").with_detail(serde_json::json!({
        "cause": "credential_resolution",
        "retry_after_ms": 60000,
    }))
}

fn error_body_module_removed() -> ErrorBody {
    ErrorBody::new(
        error_codes::MODULE_REMOVED,
        "module_id 'vault' was removed 1200 ms ago",
    )
}

fn error_body_capability_forbidden() -> ErrorBody {
    ErrorBody::new(
        "capability_forbidden",
        "module_id 'runner' must never reach capability 'credentials-provider/v1' provided by 'vault'",
    )
}

fn principal_reserved() -> Principal {
    Principal::Reserved {
        module_id: "subc-mcp".to_string(),
    }
}

fn module_hello_body() -> ModuleHelloBody {
    ModuleHelloBody {
        manifest: module_manifest("aft-tools"),
        protocol_ver: PROTOCOL_VERSION,
        control_ops: Some(vec!["route.bind".to_string(), "route.status".to_string()]),
        // None + skip_serializing_if keeps the golden bytes byte-identical: an absent
        // launch_nonce serializes to no field, so existing modules and AFT are
        // unaffected by the added field.
        launch_nonce: None,
    }
}

fn module_hello_body_with_provenance() -> ModuleHelloBody {
    let mut hello = module_hello_body();
    hello.manifest.provenance = Some(ManifestProvenance {
        build_git_sha: Some("0123456789abcdef0123456789abcdef01234567-dirty".to_string()),
        build_lock_digest: Some(
            "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".to_string(),
        ),
        wire_crate_version: Some("0.13.0".to_string()),
        store_schema_version: Some("42".to_string()),
    });
    hello
}

fn module_hello_ack_body() -> ModuleHelloAckBody {
    ModuleHelloAckBody {
        negotiated_ver: PROTOCOL_VERSION,
        subc_ops: vec![
            "server.describe".to_string(),
            "catalog.list".to_string(),
            "route.open".to_string(),
            "route.poll".to_string(),
            "catalog.update".to_string(),
        ],
        subc_capabilities: vec!["manifest_registration_v1".to_string()],
        storage: None,
    }
}

fn module_control_request(
    consumer_capabilities: Option<Vec<String>>,
    admission_facts: Option<Value>,
) -> ModuleControlRequest {
    ModuleControlRequest::RouteBind {
        route_channel: 42,
        epoch: 7,
        target: RouteTarget::ToolProvider {
            module_id: "aft-tools".to_string(),
        },
        identity: bind_identity(),
        principal: Some(Principal::Direct),
        consumer_capabilities,
        admission_facts,
    }
}

fn module_manifest(module_id: &str) -> ModuleManifest {
    ModuleManifest {
        module_id: module_id.to_string(),
        module_version: "1.2.3".to_string(),
        protocol_ver: PROTOCOL_VERSION,
        trust_tier: TrustTier::FirstParty,
        provides: provider_roles(),
        consumes: Vec::new(),
        bindings: Bindings {
            storage: StorageBinding {
                kind: StorageKind::Sqlite,
                scope: StorageScope::Project,
                owns_schema: true,
            },
            vault_grants: Vec::new(),
            identity: IdentityBinding {
                requires: vec![IdentityScope::Project],
                optional: vec![IdentityScope::Session],
            },
        },
        capabilities: None,
        self_signals: None,
        provenance: None,
    }
}

fn management_surface_manifest(description: Option<&str>) -> ModuleManifest {
    ModuleManifest {
        module_id: "management-surface".to_string(),
        module_version: "1.2.3".to_string(),
        protocol_ver: PROTOCOL_VERSION,
        trust_tier: TrustTier::FirstParty,
        provides: vec![ProviderRole::ManagementSurface {
            operations: vec![ManagementOperation {
                name: "records.list".to_string(),
                kind: ManagementOperationKind::Query,
                description: description.map(ToOwned::to_owned),
            }],
            config_schema: serde_json::json!({"type": "object"}),
            observability: vec![ObservabilitySurface {
                name: "records.stats".to_string(),
                kind: ObservabilityKind::Snapshot,
            }],
            identity_scope: vec![IdentityScope::Project],
            concurrency: Concurrency::ModuleManaged,
        }],
        consumes: Vec::new(),
        bindings: Bindings {
            storage: StorageBinding {
                kind: StorageKind::Sqlite,
                scope: StorageScope::Project,
                owns_schema: false,
            },
            vault_grants: Vec::new(),
            identity: IdentityBinding {
                requires: vec![IdentityScope::Project],
                optional: Vec::new(),
            },
        },
        capabilities: None,
        self_signals: None,
        provenance: None,
    }
}

fn provider_roles() -> Vec<ProviderRole> {
    vec![ProviderRole::ToolProvider {
        tools: vec![Tool {
            name: "memory.read".to_string(),
            description: None,
            execution_mode: ExecutionMode::Pure,
            schema: serde_json::json!({"type": "object", "required": ["id"]}),
        }],
        identity_scope: vec![IdentityScope::Project, IdentityScope::Session],
        concurrency: Concurrency::ModuleManaged,
        emits_push: true,
        sub_supervises: true,
    }]
}
