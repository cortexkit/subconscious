use serde_json::json;
use subc_control::{
    CatalogEntry, ChildResourceUnavailableReason, ChildResourceUsage, ModuleProtocol,
    NotReadyReason, PendingReloadVerdict, ReloadPathAgreement, RunningImageAgreement,
    RunningImageUnavailableReason, SupervisorEntry, SupervisorHealthStatus, TerminalExitKind,
};
use subc_protocol::manifest::{
    CapabilityDeclarations, EventDeclaration, SelfSignalDeclaration, SelfSignalEffect,
    SelfSignalKind, SignalAnchor,
};

#[test]
fn catalog_constructor_matches_wire_defaults() {
    let entry = CatalogEntry::new("provider", vec![], vec!["route.bind".to_string()]);
    let legacy = json!({"module_id": "provider", "roles": [], "control_ops": ["route.bind"]});
    assert_eq!(
        serde_json::from_value::<CatalogEntry>(legacy).unwrap(),
        entry
    );
    assert_eq!(
        serde_json::to_string(&entry).unwrap(),
        r#"{"module_id":"provider","ready":true,"roles":[],"control_ops":["route.bind"]}"#
    );
    assert_eq!(
        serde_json::to_value(entry).unwrap(),
        json!({"module_id": "provider", "ready": true, "roles": [], "control_ops": ["route.bind"]})
    );
}

#[test]
fn catalog_events_use_the_manifest_declaration_shape() {
    let events = vec![
        EventDeclaration::new("task_completed", 2)
            .with_headers(vec!["project_id".to_string()])
            .with_summary(Some("A task completed".to_string())),
        EventDeclaration::new("task_failed", 1),
    ];
    let entry = CatalogEntry::new("publisher", vec![], vec![]).with_events(Some(events.clone()));
    let wire = json!({
        "module_id": "publisher",
        "ready": true,
        "roles": [],
        "control_ops": [],
        "events": [
            {"name": "task_completed", "version": 2, "headers": ["project_id"], "summary": "A task completed"},
            {"name": "task_failed", "version": 1, "headers": []}
        ]
    });

    assert_eq!(serde_json::to_value(&entry).unwrap(), wire);
    assert_eq!(serde_json::from_value::<CatalogEntry>(wire).unwrap(), entry);
}

#[test]
fn catalog_builders_set_and_clear_every_defaulted_field() {
    let base = CatalogEntry::new("provider", vec![], vec![]);
    let entry = base
        .clone()
        .with_ready(false)
        .with_not_ready(Some(NotReadyReason {
            reason: "declared_not_ready".to_string(),
            capability: None,
        }))
        .with_module_version(Some("1.2.3".to_string()))
        .with_capabilities(Some(CapabilityDeclarations {
            provides: vec!["provider/v1".to_string()],
            requires: vec![],
            must_never_reach: vec![],
        }))
        .with_self_signals(Some(vec![SelfSignalDeclaration {
            name: "usage".to_string(),
            kind: SelfSignalKind::Poller,
            effect: SelfSignalEffect::Observe,
            anchored_to: SignalAnchor::FixedInterval,
            cadence: None,
            domain: None,
            note: None,
        }]));
    let wire = json!({
        "module_id": "provider", "ready": false,
        "not_ready": {"reason": "declared_not_ready"},
        "module_version": "1.2.3", "roles": [], "control_ops": [],
        "capabilities": {"provides": ["provider/v1"], "requires": [], "must_never_reach": []},
        "self_signals": [{"name": "usage", "kind": "poller", "effect": "observe", "anchored_to": "fixed_interval"}]
    });
    assert_eq!(serde_json::to_value(&entry).unwrap(), wire);
    assert_eq!(serde_json::from_value::<CatalogEntry>(wire).unwrap(), entry);
    assert_eq!(
        entry
            .with_ready(true)
            .with_not_ready(None)
            .with_module_version(None)
            .with_capabilities(None)
            .with_self_signals(None),
        base
    );
}

#[test]
fn supervisor_constructor_matches_wire_defaults() {
    let entry = SupervisorEntry::new(
        "provider",
        "starting",
        true,
        false,
        SupervisorHealthStatus::Ok,
    );
    let legacy = json!({
        "module_id": "provider", "state": "starting", "enabled": true,
        "live": false, "health": "ok"
    });
    assert_eq!(
        serde_json::from_value::<SupervisorEntry>(legacy).unwrap(),
        entry
    );
    assert_eq!(
        serde_json::to_value(entry).unwrap(),
        json!({
            "module_id": "provider", "state": "starting", "enabled": true,
            "live": false, "protocol": "subc", "health": "ok", "last_probe_ms": null
        })
    );
}

#[test]
fn supervisor_builders_set_and_clear_every_defaulted_field() {
    let base = SupervisorEntry::new(
        "provider",
        "running",
        true,
        true,
        SupervisorHealthStatus::Degraded,
    );
    let entry = base
        .clone()
        .with_protocol(ModuleProtocol::None)
        .with_launch_nonce_env(Some(false))
        .with_pending_reload(Some(PendingReloadVerdict {
            path: ReloadPathAgreement::Match,
            image: RunningImageAgreement::Unavailable {
                reason: RunningImageUnavailableReason::NotRunning,
            },
        }))
        .with_last_probe_ms(Some(101))
        .with_last_exit_code(Some(2))
        .with_last_exit_signal(Some(3))
        .with_last_exit_ms(Some(104))
        .with_last_exit_kind(Some(TerminalExitKind::Crash))
        .with_restart_count(Some(5))
        .with_max_restarts(Some(6))
        .with_lifetime_restarts(Some(7))
        .with_spawn_generation(Some(8))
        .with_restart_window_secs(Some(9))
        .with_drain_timeout_ms(Some(110))
        .with_restart_backoff_ms(Some(111))
        .with_restart_max_backoff_ms(Some(112))
        .with_resources(Some(ChildResourceUsage::Unavailable {
            reason: ChildResourceUnavailableReason::Unreadable,
        }));
    let wire = json!({
        "module_id": "provider", "state": "running", "enabled": true, "live": true,
        "protocol": "none", "launch_nonce_env": false, "health": "degraded",
        "pending_reload": {"path": {"status": "match"}, "image": {"status": "unavailable", "reason": "not_running"}},
        "last_probe_ms": 101, "last_exit_code": 2, "last_exit_signal": 3,
        "last_exit_ms": 104, "last_exit_kind": "crash", "restart_count": 5,
        "max_restarts": 6, "lifetime_restarts": 7, "spawn_generation": 8,
        "restart_window_secs": 9, "drain_timeout_ms": 110, "restart_backoff_ms": 111,
        "restart_max_backoff_ms": 112, "resources": {"status": "unavailable", "reason": "unreadable"}
    });
    assert_eq!(serde_json::to_value(&entry).unwrap(), wire);
    assert_eq!(
        serde_json::from_value::<SupervisorEntry>(wire).unwrap(),
        entry
    );
    assert_eq!(
        entry
            .with_protocol(ModuleProtocol::Subc)
            .with_launch_nonce_env(None)
            .with_pending_reload(None)
            .with_last_probe_ms(None)
            .with_last_exit_code(None)
            .with_last_exit_signal(None)
            .with_last_exit_ms(None)
            .with_last_exit_kind(None)
            .with_restart_count(None)
            .with_max_restarts(None)
            .with_lifetime_restarts(None)
            .with_spawn_generation(None)
            .with_restart_window_secs(None)
            .with_drain_timeout_ms(None)
            .with_restart_backoff_ms(None)
            .with_restart_max_backoff_ms(None)
            .with_resources(None),
        base
    );
}
