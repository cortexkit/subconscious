use std::{fmt::Debug, fs, path::PathBuf};

use serde::{de::DeserializeOwned, Serialize};
use serde_json::Value;
use subc_control::{
    CatalogEntry, ClientControlPush, ClientControlRequest, ClientControlResponse, ConsumerIdentity,
    DaemonBuildProvenance, DaemonObservedProcess, ModuleDeclaredProvenance, PollKind,
    RouteCloseReason, RunningImageAgreement, RunningImageEvidence, StderrCaptureState, StderrTail,
    StderrTailEntry, SupervisorDaemonProvenance, SupervisorEntry, SupervisorHealthEntry,
    SupervisorHealthStatus, SupervisorModuleProvenance, SupervisorObservedProcess,
    SupervisorRescanResult, SupervisorRoute, SupervisorRouteConsumer, SupervisorRouteModule,
};
use subc_protocol::{
    manifest::{
        CapabilityDeclarations, CapabilityNeed, CapabilityRequirement, Concurrency, ExecutionMode,
        IdentityScope, InternalTransport, ManagementOperation, ManagementOperationKind,
        ManifestProvenance, ObservabilityKind, ObservabilitySurface, PipelineAppliesTo,
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
    for (name, push) in client_control_pushes() {
        assert_golden(name, &push);
    }
    assert_golden("catalog_entry", &catalog_entry());
    assert_golden(
        "catalog_entry_without_operation_description",
        &catalog_entry_without_operation_description(),
    );
    assert_golden(
        "client_control_response_catalog_list_without_capabilities",
        &ClientControlResponse::CatalogList {
            generation: 8,
            modules: vec![catalog_entry_without_capabilities()],
            subc_ops: thin_core_ops(),
        },
    );
    assert_golden(
        "client_control_response_catalog_list_without_operation_description",
        &ClientControlResponse::CatalogList {
            generation: 9,
            modules: vec![catalog_entry_without_operation_description()],
            subc_ops: thin_core_ops(),
        },
    );
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
            // drain_timeout_ms: None is skipped on the wire, so this vector's
            // bytes are unchanged by the field's addition -- an old client's
            // restart request and a new flag-less one are byte-identical.
            "client_control_request_supervisor_restart",
            ClientControlRequest::SupervisorRestart {
                module_id: "aft-tools".to_string(),
                drain_timeout_ms: None,
            },
        ),
        (
            // The wedge-bounce form: an explicit 0 must SERIALIZE (it is the
            // override, not the default), so it gets its own golden.
            "client_control_request_supervisor_restart_drain_now",
            ClientControlRequest::SupervisorRestart {
                module_id: "aft-tools".to_string(),
                drain_timeout_ms: Some(0),
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
            "client_control_request_supervisor_release_reserved",
            ClientControlRequest::SupervisorReleaseReserved {
                module_id: "vault".to_string(),
            },
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
        (
            "client_control_request_supervisor_provenance_filtered",
            ClientControlRequest::SupervisorProvenance {
                module_id: Some("aft".to_string()),
            },
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
                build_commit: None,
                build_lock_digest: None,
                capability_requirements: Vec::new(),
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
                    "module_frames_dropped_no_route_by_module": { "vault": 3 },
                    "module_frames_dropped_no_route_last_10m": 3,
                    "module_frames_dropped_no_route_nonzero_minutes_last_10m": 3,
                    "client_frames_dropped_stale_route": 2,
                    "client_egress_close_delivery_failed": 1,
                    "goodbye_relay_client_failed": 4,
                    "goodbye_relay_module_dropped": 5,
                    "goodbye_relay_module_dropped_by_module": { "vault": 5 },
                    "route_released_epoch_fenced": 6,
                    "route_release_stale_skipped": 7,
                })),
                // Distinct values, real shapes: a 40-hex commit and a 64-hex
                // digest, so the pin would catch the two fields transposed.
                build_commit: Some("0123456789abcdef0123456789abcdef01234567".to_string()),
                build_lock_digest: Some(
                    "9d2c0d69cd82f2151bbb2b32ab9ac9d861063ffde2f8582afe767ec7e1f2145c".to_string(),
                ),
                capability_requirements: Vec::new(),
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
                    capability_warnings: Vec::new(),
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
            "client_control_response_supervisor_routes",
            ClientControlResponse::SupervisorRoutes {
                modules: vec![SupervisorRouteModule {
                    module_id: "target".to_string(),
                    routes: vec![
                        // Mirrors the REAL-HANDLER-generated golden exactly (the
                        // census golden's authoritative producer is the handler
                        // test in subc-core control.rs; this fixture must agree
                        // byte-for-byte or the two tests fight over the file).
                        // The old-daemon absent-reason wire is pinned separately
                        // below in a decode-tolerance test, not in this golden.
                        SupervisorRoute {
                            consumer: SupervisorRouteConsumer::Direct { connection_id: 102 },
                            age_ms: 0,
                            draining: true,
                            drain_reason: Some(RouteCloseReason::Reload),
                        },
                        SupervisorRoute {
                            consumer: SupervisorRouteConsumer::Reserved {
                                module_id: "fed".to_string(),
                            },
                            age_ms: 0,
                            draining: true,
                            drain_reason: Some(RouteCloseReason::Reload),
                        },
                    ],
                }],
            },
        ),
        (
            "client_control_response_supervisor_provenance_reported",
            ClientControlResponse::SupervisorProvenance {
                daemon: SupervisorDaemonProvenance {
                    daemon_build: DaemonBuildProvenance {
                        build_commit: Some(
                            "0123456789abcdef0123456789abcdef01234567-dirty".to_string(),
                        ),
                        build_lock_digest: Some(
                            "9d2c0d69cd82f2151bbb2b32ab9ac9d861063ffde2f8582afe767ec7e1f2145c"
                                .to_string(),
                        ),
                    },
                    daemon_observed: DaemonObservedProcess {
                        pid: Some(4200),
                        started_at_ms: Some(1_725_000_000_001),
                        running_image: RunningImageAgreement::Match {
                            evidence: RunningImageEvidence::LinuxProcSha256 {
                                digest: "1111111111111111111111111111111111111111111111111111111111111111"
                                    .to_string(),
                            },
                        },
                    },
                },
                modules: vec![SupervisorModuleProvenance {
                    module_id: "aft".to_string(),
                    module_declared: ModuleDeclaredProvenance::Reported {
                        build: ManifestProvenance {
                            build_commit: Some(
                                "0123456789abcdef0123456789abcdef01234567".to_string(),
                            ),
                            build_lock_digest: Some(
                                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                                    .to_string(),
                            ),
                            wire_crate_version: Some("0.13.0".to_string()),
                            store_schema_version: Some("2".to_string()),
                        },
                    },
                    daemon_observed: SupervisorObservedProcess {
                        pid: Some(4201),
                        spawned_at_ms: Some(1_725_000_000_002),
                        spawned_from: Some(PathBuf::from("/opt/subc/bin/aft")),
                        running_image: RunningImageAgreement::Match {
                            evidence: RunningImageEvidence::LinuxProcSha256 {
                                digest: "2222222222222222222222222222222222222222222222222222222222222222"
                                    .to_string(),
                            },
                        },
                    },
                }],
            },
        ),
        (
            "client_control_response_supervisor_provenance_unverifiable",
            ClientControlResponse::SupervisorProvenance {
                daemon: SupervisorDaemonProvenance {
                    daemon_build: DaemonBuildProvenance {
                        build_commit: None,
                        build_lock_digest: None,
                    },
                    daemon_observed: DaemonObservedProcess {
                        pid: Some(4300),
                        started_at_ms: Some(1_725_000_000_003),
                        running_image: RunningImageAgreement::Match {
                            evidence: RunningImageEvidence::MacosSpawnInode {
                                device: 2_049,
                                inode: 99_001,
                            },
                        },
                    },
                },
                modules: vec![SupervisorModuleProvenance {
                    module_id: "vault".to_string(),
                    module_declared: ModuleDeclaredProvenance::Unverifiable,
                    daemon_observed: SupervisorObservedProcess {
                        pid: Some(4301),
                        spawned_at_ms: Some(1_725_000_000_004),
                        spawned_from: Some(PathBuf::from("/opt/subc/bin/vault")),
                        running_image: RunningImageAgreement::Match {
                            evidence: RunningImageEvidence::MacosSpawnInode {
                                device: 2_049,
                                inode: 99_002,
                            },
                        },
                    },
                }],
            },
        ),
        (
            "client_control_response_supervisor_provenance_mismatch",
            ClientControlResponse::SupervisorProvenance {
                daemon: SupervisorDaemonProvenance {
                    daemon_build: DaemonBuildProvenance {
                        build_commit: Some(
                            "fedcba9876543210fedcba9876543210fedcba98".to_string(),
                        ),
                        build_lock_digest: Some(
                            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                                .to_string(),
                        ),
                    },
                    daemon_observed: DaemonObservedProcess {
                        pid: Some(4400),
                        started_at_ms: Some(1_725_000_000_005),
                        running_image: RunningImageAgreement::Match {
                            evidence: RunningImageEvidence::LinuxProcSha256 {
                                digest: "3333333333333333333333333333333333333333333333333333333333333333"
                                    .to_string(),
                            },
                        },
                    },
                },
                modules: vec![SupervisorModuleProvenance {
                    module_id: "mcp".to_string(),
                    module_declared: ModuleDeclaredProvenance::Reported {
                        build: ManifestProvenance {
                            build_commit: Some(
                                "fedcba9876543210fedcba9876543210fedcba98-dirty".to_string(),
                            ),
                            build_lock_digest: Some(
                                "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
                                    .to_string(),
                            ),
                            wire_crate_version: Some("0.13.0".to_string()),
                            store_schema_version: Some("3".to_string()),
                        },
                    },
                    daemon_observed: SupervisorObservedProcess {
                        pid: Some(4401),
                        spawned_at_ms: Some(1_725_000_000_006),
                        spawned_from: Some(PathBuf::from("/opt/subc/bin/mcp")),
                        running_image: RunningImageAgreement::Mismatch {
                            running: RunningImageEvidence::LinuxProcSha256 {
                                digest: "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"
                                    .to_string(),
                            },
                            disk: RunningImageEvidence::LinuxProcSha256 {
                                digest: "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
                                    .to_string(),
                            },
                        },
                    },
                }],
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

fn client_control_pushes() -> Vec<(&'static str, ClientControlPush)> {
    vec![
        (
            "client_control_push_route_closing",
            ClientControlPush::RouteClosing {
                module_id: "aft-tools".to_string(),
                reason: RouteCloseReason::Reload,
            },
        ),
        (
            // Disable pinned on RouteClosing so the reason enum's full range is
            // covered without a fifth combination of drained/abandoned.
            "client_control_push_route_closing_disable",
            ClientControlPush::RouteClosing {
                module_id: "aft-tools".to_string(),
                reason: RouteCloseReason::Disable,
            },
        ),
        (
            "client_control_push_route_closed_drained",
            ClientControlPush::RouteClosed {
                module_id: "aft-tools".to_string(),
                reason: RouteCloseReason::Restart,
                drained: true,
                abandoned: 0,
                terminal: Some(false),
            },
        ),
        (
            // The forced-teardown case: a planned drain (reload) that timed out
            // with binds abandoned. drained:false with a non-zero abandoned count
            // is the combination carrying the most semantic weight, since
            // drained:true must never appear over abandoned routes.
            "client_control_push_route_closed_abandoned",
            ClientControlPush::RouteClosed {
                module_id: "aft-tools".to_string(),
                reason: RouteCloseReason::Reload,
                drained: false,
                abandoned: 3,
                terminal: Some(false),
            },
        ),
        (
            // Planned disable closures also come from begin_forwarding_drain_with,
            // but only this reason leaves the module down until operator action.
            "client_control_push_route_closed_disable",
            ClientControlPush::RouteClosed {
                module_id: "aft-tools".to_string(),
                reason: RouteCloseReason::Disable,
                drained: true,
                abandoned: 0,
                terminal: Some(true),
            },
        ),
        (
            // A capability census closes just the violating route immediately;
            // it does not wait for a module drain or leave the target terminal.
            "client_control_push_route_closed_capability_denied",
            ClientControlPush::RouteClosed {
                module_id: "credentials-provider".to_string(),
                reason: RouteCloseReason::CapabilityDenied,
                drained: false,
                abandoned: 0,
                terminal: Some(false),
            },
        ),
        (
            // Matches cleanup_connection's crash-teardown shape exactly: a crash
            // has no drain, so abandoned is always 0 -- unlike the planned-drain
            // reasons above, crash + abandoned>0 is unreachable on the wire.
            "client_control_push_route_closed_crash",
            ClientControlPush::RouteClosed {
                module_id: "aft-tools".to_string(),
                reason: RouteCloseReason::Crash,
                drained: false,
                abandoned: 0,
                terminal: Some(false),
            },
        ),
        (
            // cleanup_connection emits the same reachable crash shape when the
            // supervisor has exhausted its restart budget.
            "client_control_push_route_closed_crash_terminal",
            ClientControlPush::RouteClosed {
                module_id: "aft-tools".to_string(),
                reason: RouteCloseReason::Crash,
                drained: false,
                abandoned: 0,
                terminal: Some(true),
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
        "route.closing".to_string(),
        "route.closed".to_string(),
        "supervisor.list".to_string(),
        "supervisor.restart".to_string(),
        "supervisor.reload".to_string(),
        "supervisor.rescan".to_string(),
        "supervisor.release_reserved".to_string(),
        "supervisor.set_enabled".to_string(),
        "supervisor.health_probe".to_string(),
        "supervisor.health".to_string(),
        "supervisor.stderr_tail".to_string(),
        "supervisor.terminals".to_string(),
        "supervisor.routes".to_string(),
        "supervisor.provenance".to_string(),
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
        capabilities: Some(CapabilityDeclarations {
            provides: vec!["credentials-provider/v1".to_string()],
            requires: vec![CapabilityRequirement {
                capability: "context-transform/v1".to_string(),
                need: CapabilityNeed::Optional,
            }],
            must_never_reach: vec!["federation-transport/v1".to_string()],
        }),
    }
}

fn catalog_entry_without_capabilities() -> CatalogEntry {
    CatalogEntry {
        module_id: "legacy-tools".to_string(),
        module_version: Some("0.8.0".to_string()),
        roles: Vec::new(),
        control_ops: vec!["route.bind".to_string(), "route.status".to_string()],
        capabilities: None,
    }
}

fn catalog_entry_without_operation_description() -> CatalogEntry {
    CatalogEntry {
        module_id: "legacy-management".to_string(),
        module_version: Some("0.7.0".to_string()),
        roles: vec![ProviderRole::ManagementSurface {
            operations: vec![ManagementOperation {
                name: "records.list".to_string(),
                kind: ManagementOperationKind::Query,
                description: None,
            }],
            config_schema: serde_json::json!({"type": "object"}),
            observability: Vec::new(),
            identity_scope: vec![IdentityScope::Project],
            concurrency: Concurrency::ModuleManaged,
        }],
        control_ops: Vec::new(),
        capabilities: None,
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
        last_exit_ms: Some(1_700_000_000_123),
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
                description: Some(
                    "List stored memory items and return their identifiers and metadata."
                        .to_string(),
                ),
            }],
            config_schema: serde_json::json!({"type": "object"}),
            observability: vec![ObservabilitySurface {
                name: "memory.stats".to_string(),
                kind: ObservabilityKind::Snapshot,
            }],
            identity_scope: vec![IdentityScope::Project],
            concurrency: Concurrency::ModuleManaged,
        },
        ProviderRole::InternalService {
            service_id: "llm".to_string(),
            transport: InternalTransport::Bulk,
            agent_facing: true,
            operations: vec!["llm.complete".to_string()],
        },
    ]
}

/// The pre-reason census wire stays decodable: an older daemon omits
/// `drain_reason` entirely, and the field resolves to None rather than failing
/// the row. Pinned as a JSON literal because the shared golden file mirrors the
/// NEW handler's output — this is the arm that file can no longer carry.
#[test]
fn a_census_route_without_a_drain_reason_still_decodes() {
    let old_wire =
        r#"{"consumer":{"kind":"direct","connection_id":7},"age_ms":12,"draining":true}"#;
    let route: SupervisorRoute = serde_json::from_str(old_wire).expect("old census wire decodes");
    assert!(route.draining);
    assert_eq!(route.drain_reason, None);
    // And absence round-trips: a None reason must not appear on the wire, so a
    // new consumer relaying an old daemon's census cannot invent the field.
    let reserialized = serde_json::to_string(&route).expect("serializes");
    assert!(
        !reserialized.contains("drain_reason"),
        "absent reason must stay absent: {reserialized}"
    );
}
