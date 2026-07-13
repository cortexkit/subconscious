use std::{fmt::Debug, fs, path::PathBuf};

use serde::{de::DeserializeOwned, Serialize};
use serde_json::Value;
use subc_protocol::{
    manifest::{
        Bindings, Concurrency, ExecutionMode, IdentityBinding, IdentityScope, ModuleManifest,
        ProviderRole, StorageBinding, StorageKind, StorageScope, Tool, TrustTier,
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
    assert_golden("principal_reserved", &principal_reserved());
    assert_golden("principal_direct", &Principal::Direct);
    assert_golden("principal_unverified", &Principal::Unverified);
    assert_golden("module_hello_body", &module_hello_body());
    assert_golden("module_hello_ack_body", &module_hello_ack_body());
    assert_golden(
        "module_control_request_route_bind",
        &module_control_request(Some(vec!["elicitation".to_string(), "roots".to_string()])),
    );
    assert_golden(
        "module_control_request_route_bind_without_consumer_capabilities",
        &module_control_request(None),
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
        "module_control_push_route_status",
        &ModuleControlPush::RouteStatus {
            route_channel: 42,
            route_epoch: 7,
            status: "indexing".to_string(),
        },
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
    ErrorBody {
        code: "config_divergence".to_string(),
        message: "active config differs".to_string(),
    }
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

fn module_control_request(consumer_capabilities: Option<Vec<String>>) -> ModuleControlRequest {
    ModuleControlRequest::RouteBind {
        route_channel: 42,
        epoch: 7,
        target: RouteTarget::ToolProvider {
            module_id: "aft-tools".to_string(),
        },
        identity: bind_identity(),
        principal: Some(Principal::Direct),
        consumer_capabilities,
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
        scheduled_tasks: Vec::new(),
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
