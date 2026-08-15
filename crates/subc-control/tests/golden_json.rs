use std::{fmt::Debug, fs, path::PathBuf};

use serde::{de::DeserializeOwned, Serialize};
use serde_json::Value;
use subc_control::{
    CatalogEntry, ClientControlRequest, ClientControlResponse, ConsumerIdentity, PollKind,
    StderrCaptureState, StderrTail, StderrTailEntry, SupervisorEntry, SupervisorHealthEntry,
    SupervisorHealthStatus, SupervisorRescanResult,
};
use subc_protocol::{
    manifest::{
        Concurrency, ExecutionMode, IdentityScope, InternalTransport, ManagementOperation,
        ManagementOperationKind, ObservabilityKind, ObservabilitySurface, PipelineAppliesTo,
        PipelineStageKind, ProviderRole, Tool,
    },
    session::HealthStatus,
    BindIdentity, RouteTarget, PROTOCOL_VERSION,
};

// Drift-prevention contract: UPDATE_GOLDEN=1 rewrites the committed JSON
// when a wire-shape change is intentional and the TS mirror is updated too.
#[test]
fn control_wire_shapes_match_golden_json_and_round_trip() {
    for (name, request) in client_control_requests() {
        assert_golden(name, &request);
    }
    for (name, response) in client_control_responses() {
        assert_golden(name, &response);
    }
    assert_golden("catalog_entry", &catalog_entry());
    assert_golden("supervisor_entry", &supervisor_entry());
    assert_golden("poll_kind_status", &PollKind::Status);
    assert_golden("poll_kind_liveness", &PollKind::Liveness);
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

fn client_control_requests() -> Vec<(&'static str, ClientControlRequest)> {
    vec![
        (
            "client_control_request_server_describe",
            ClientControlRequest::ServerDescribe {},
        ),
        (
            "client_control_request_catalog_list",
            ClientControlRequest::CatalogList {
                module_id: Some("aft-tools".to_string()),
            },
        ),
        (
            "client_control_request_route_open",
            ClientControlRequest::RouteOpen {
                target: RouteTarget::InternalService {
                    module_id: "llm-runner".to_string(),
                    service_id: "llm".to_string(),
                },
                identity: bind_identity(),
                consumer_identity: Some(ConsumerIdentity {
                    module_id: "subc-mcp".to_string(),
                    launch_nonce: "0123456789abcdef".to_string(),
                }),
                consumer_capabilities: Some(vec!["elicitation".to_string(), "roots".to_string()]),
                admission_facts: Some(
                    serde_json::json!({"schema": 1, "verified_class": "service"}),
                ),
            },
        ),
        (
            "client_control_request_route_open_without_consumer_capabilities",
            ClientControlRequest::RouteOpen {
                target: RouteTarget::InternalService {
                    module_id: "llm-runner".to_string(),
                    service_id: "llm".to_string(),
                },
                identity: bind_identity(),
                consumer_identity: Some(ConsumerIdentity {
                    module_id: "subc-mcp".to_string(),
                    launch_nonce: "0123456789abcdef".to_string(),
                }),
                consumer_capabilities: None,
                admission_facts: None,
            },
        ),
        (
            "client_control_request_route_poll",
            ClientControlRequest::RoutePoll {
                route_channel: 42,
                route_epoch: 7,
                kind: PollKind::Status,
            },
        ),
        (
            "client_control_request_supervisor_list",
            ClientControlRequest::SupervisorList {},
        ),
        (
            "client_control_request_supervisor_restart",
            ClientControlRequest::SupervisorRestart {
                module_id: "aft-tools".to_string(),
            },
        ),
        (
            "client_control_request_supervisor_reload",
            ClientControlRequest::SupervisorReload {
                module_id: "aft-tools".to_string(),
            },
        ),
        (
            // preview:false is the default and is skipped on the wire, so this
            // vector's bytes are unchanged by the field's addition -- which is the
            // property that lets an existing client keep sending `{}`.
            "client_control_request_supervisor_rescan",
            ClientControlRequest::SupervisorRescan { preview: false },
        ),
        (
            "client_control_request_supervisor_rescan_preview",
            ClientControlRequest::SupervisorRescan { preview: true },
        ),
        (
            "client_control_request_supervisor_set_enabled",
            ClientControlRequest::SupervisorSetEnabled {
                module_id: "aft-tools".to_string(),
                enabled: false,
            },
        ),
        (
            "client_control_request_supervisor_health_probe",
            ClientControlRequest::SupervisorHealthProbe {
                module_id: "aft-tools".to_string(),
            },
        ),
        (
            "client_control_request_supervisor_health",
            ClientControlRequest::SupervisorHealth {},
        ),
    ]
}

fn client_control_responses() -> Vec<(&'static str, ClientControlResponse)> {
    vec![
        (
            "client_control_response_server_describe",
            ClientControlResponse::ServerDescribe {
                protocol_ver: PROTOCOL_VERSION,
                subc_ops: thin_core_ops(),
                capabilities: vec!["manifest_registration_v1".to_string()],
                connected_clients: 2,
                counters: None,
                // Absent in this fixture: pins that a daemon predating the
                // provenance fields serializes no key at all, which is what
                // lets a newer client distinguish "old daemon" from a match.
                build_git_sha: None,
                build_lock_digest: None,
            },
        ),
        (
            "client_control_response_server_describe_with_counters",
            ClientControlResponse::ServerDescribe {
                protocol_ver: PROTOCOL_VERSION,
                subc_ops: thin_core_ops(),
                capabilities: vec!["manifest_registration_v1".to_string()],
                connected_clients: 2,
                counters: Some(serde_json::json!({
                    "module_frames_dropped_no_route": 3,
                    "client_frames_dropped_stale_route": 2,
                    "client_egress_close_delivery_failed": 1,
                    "goodbye_relay_client_failed": 4,
                    "goodbye_relay_module_dropped": 5,
                    "route_released_epoch_fenced": 6,
                    "route_release_stale_skipped": 7,
                })),
                // Distinct values, real shapes: a 40-hex commit and a 64-hex
                // digest, so the pin would catch the two fields transposed.
                build_git_sha: Some("0123456789abcdef0123456789abcdef01234567".to_string()),
                build_lock_digest: Some(
                    "9d2c0d69cd82f2151bbb2b32ab9ac9d861063ffde2f8582afe767ec7e1f2145c".to_string(),
                ),
            },
        ),
        (
            "client_control_response_catalog_list",
            ClientControlResponse::CatalogList {
                generation: 7,
                modules: vec![catalog_entry()],
                subc_ops: thin_core_ops(),
            },
        ),
        (
            "client_control_response_route_open",
            ClientControlResponse::RouteOpen {
                route_channel: 42,
                route_epoch: 7,
            },
        ),
        (
            "client_control_response_route_poll",
            ClientControlResponse::RoutePoll {
                route_channel: 42,
                route_epoch: 7,
                status: Some("indexing".to_string()),
                live: None,
            },
        ),
        (
            "client_control_response_supervisor_list",
            ClientControlResponse::SupervisorList {
                generation: 7,
                modules: vec![supervisor_entry()],
            },
        ),
        (
            "client_control_response_supervisor_ack",
            ClientControlResponse::SupervisorAck {
                module_id: "aft-tools".to_string(),
                applied: true,
            },
        ),
        (
            "client_control_response_supervisor_rescan",
            ClientControlResponse::SupervisorRescan {
                result: SupervisorRescanResult {
                    added: vec!["new-tools".to_string()],
                    removed: vec!["old-tools".to_string()],
                    changed_pending_reload: vec!["aft-tools".to_string()],
                    enabled_changes: vec!["paused-tools".to_string()],
                    unchanged: 3,
                    preview: false,
                    restart_required: vec!["storage".to_string()],
                },
            },
        ),
        (
            "client_control_response_supervisor_health_probe",
            ClientControlResponse::SupervisorHealthProbe {
                module_id: "aft-tools".to_string(),
                status: HealthStatus::Degraded,
                detail: Some("warming model".to_string()),
                metrics: Some(serde_json::json!({"queue_depth": 3})),
            },
        ),
        (
            "client_control_response_supervisor_health",
            ClientControlResponse::SupervisorHealth {
                generation: 7,
                modules: vec![supervisor_health_entry()],
            },
        ),
        (
            "client_control_response_supervisor_stderr_tail",
            ClientControlResponse::SupervisorStderrTail {
                module_id: "aft-tools".to_string(),
                tail: StderrTail {
                    capture: StderrCaptureState::Captured,
                    entries: vec![
                        StderrTailEntry::Line {
                            text: "config error: missing top-level `storage`".to_string(),
                            truncated: false,
                        },
                        StderrTailEntry::ProcessStart,
                        StderrTailEntry::Line {
                            text: "config error: missing top-level `stor".to_string(),
                            truncated: true,
                        },
                    ],
                    dropped_lines: 12,
                },
            },
        ),
        (
            // Pinned separately because it is the state the empty-tail convention
            // could not express, and a fixture is the only thing that keeps the
            // distinction from being collapsed back into an empty list later.
            "client_control_response_supervisor_stderr_tail_not_captured",
            ClientControlResponse::SupervisorStderrTail {
                module_id: "aft-tools".to_string(),
                tail: StderrTail {
                    capture: StderrCaptureState::NotCaptured {
                        reason: "stderr pipe was not available on spawn".to_string(),
                    },
                    entries: Vec::new(),
                    dropped_lines: 0,
                },
            },
        ),
        (
            "client_control_response_supervisor_stderr_tail_incomplete",
            ClientControlResponse::SupervisorStderrTail {
                module_id: "aft-tools".to_string(),
                tail: StderrTail {
                    capture: StderrCaptureState::Incomplete {
                        reason: "stderr read failed: reader failed".to_string(),
                    },
                    entries: vec![StderrTailEntry::Line {
                        text: "config error: missing top-level `storage`".to_string(),
                        truncated: false,
                    }],
                    dropped_lines: 0,
                },
            },
        ),
    ]
}

fn thin_core_ops() -> Vec<String> {
    vec![
        "server.describe".to_string(),
        "catalog.list".to_string(),
        "route.open".to_string(),
        "route.poll".to_string(),
        "supervisor.list".to_string(),
        "supervisor.restart".to_string(),
        "supervisor.reload".to_string(),
        "supervisor.rescan".to_string(),
        "supervisor.set_enabled".to_string(),
        "supervisor.health_probe".to_string(),
        "supervisor.health".to_string(),
        "supervisor.stderr_tail".to_string(),
    ]
}

fn bind_identity() -> BindIdentity {
    BindIdentity {
        project_root: PathBuf::from("/tmp/subc/project"),
        harness: "opencode".to_string(),
        session: "session-0001".to_string(),
    }
}

fn catalog_entry() -> CatalogEntry {
    CatalogEntry {
        module_id: "aft-tools".to_string(),
        module_version: Some("0.9.3".to_string()),
        roles: provider_roles(),
        control_ops: vec!["route.bind".to_string(), "route.status".to_string()],
    }
}

fn supervisor_entry() -> SupervisorEntry {
    SupervisorEntry {
        module_id: "aft-tools".to_string(),
        state: "running".to_string(),
        enabled: true,
        live: true,
        health: SupervisorHealthStatus::Degraded,
        last_probe_ms: Some(1_700_000_000_000),
        last_exit_code: None,
        last_exit_signal: None,
        // Non-equal and non-zero so the pin would catch the two fields being
        // swapped, which equal values or a zeroed count could not.
        restart_count: Some(2),
        max_restarts: Some(3),
    }
}

fn supervisor_health_entry() -> SupervisorHealthEntry {
    SupervisorHealthEntry {
        module_id: "aft-tools".to_string(),
        status: SupervisorHealthStatus::Degraded,
        detail: Some("warming model".to_string()),
        metrics: Some(serde_json::json!({"queue_depth": 3})),
        consecutive_failures: 0,
        late_answer_count: 2,
        last_late_answer_latency_ms: Some(8_000),
        last_action: Some("report".to_string()),
        last_action_ms: Some(1_700_000_000_100),
        last_probe_ms: Some(1_700_000_000_050),
    }
}

fn provider_roles() -> Vec<ProviderRole> {
    vec![
        ProviderRole::ToolProvider {
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
        },
        ProviderRole::PipelineStage {
            stage: PipelineStageKind::Transform,
            applies_to: PipelineAppliesTo {
                provider: "anthropic".to_string(),
                model: "claude".to_string(),
            },
            interface: "proxy-transform-v1".to_string(),
            declares_frozen_floor: true,
            needs_signals: vec!["route.status".to_string()],
            conformance_class: "lossless".to_string(),
        },
        ProviderRole::ManagementSurface {
            operations: vec![ManagementOperation {
                name: "memory.list".to_string(),
                kind: ManagementOperationKind::Query,
            }],
            config_schema: serde_json::json!({"type": "object"}),
            observability: vec![ObservabilitySurface {
                name: "memory.stats".to_string(),
                kind: ObservabilityKind::Snapshot,
            }],
            identity_scope: vec![IdentityScope::Project],
        },
        ProviderRole::InternalService {
            service_id: "llm".to_string(),
            transport: InternalTransport::Bulk,
            agent_facing: true,
            operations: vec!["llm.complete".to_string()],
        },
    ]
}
