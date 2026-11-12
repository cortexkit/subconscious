#![cfg(target_os = "linux")]
use super::*;

#[tokio::test]
async fn audit_registration_nack_keeps_in_force_declaration_across_respawn() {
    let (rig, owner, supervisor, handler) = supervised_fixture(false);
    applied(&owner, INCUMBENT, &pending(&owner, INCUMBENT), true).unwrap();
    std::fs::write(rig.path.join("a.seen"), b"keep running").unwrap();
    let path = rig.path.join("subc.jsonc");
    let (config, _) =
        crate::daemon_config::parse_with_activity(&std::fs::read_to_string(&path).unwrap(), &path)
            .unwrap();
    let launch = config
        .modules
        .iter()
        .find(|entry| entry.module_id == "a")
        .unwrap()
        .module_spec();
    let module = supervisor.spawn_async(launch).await.unwrap();
    let initial = module.status().unwrap().spawn_generation;
    let mut document: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    document["modules"]["a"]["registration_channel"] = json!(false);
    std::fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
    write(&rig, 2);
    approve_kinds(&rig, &owner, &[subc_config::ApplyKind::Live]);
    let rejected = pending(&owner, INCUMBENT);
    applied(&owner, INCUMBENT, &rejected, false).unwrap();
    assert_eq!(
        owner.registration_declaration("a").as_deref(),
        Some("declared")
    );
    module.restart(None).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while module.status().unwrap().spawn_generation == initial {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(module.status().unwrap().process_alive);
    let list = registration_declaration_poll(handler.clone(), "supervisor.list").await;
    let a = list["modules"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["module_id"] == "a")
        .unwrap();
    assert!(a.get("admission_refused").is_none(), "{a}");
    let provenance = registration_declaration_poll(handler, "supervisor.provenance").await;
    let a = provenance["modules"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["module_id"] == "a")
        .unwrap();
    assert_eq!(a["registration_declaration"], "declared", "{a}");
    module.stop().await.unwrap();
}

#[tokio::test]
async fn audit_registration_status_uses_one_publication_across_a_commit() {
    let rig = std::sync::Arc::new(Rig::new());
    std::fs::write(
        rig.path.join("subc.jsonc"),
        br#"{"version":1,"modules":{}}"#,
    )
    .unwrap();
    let owner = std::sync::Arc::new(rig.owner("atomic-status"));
    rig.stamp(&owner);
    owner.complete_authority_exit(1).unwrap();
    let handler = std::sync::Arc::new(
        crate::ControlHandler::new(std::sync::Arc::new(crate::Registry::default()))
            .with_config_runtime(owner.clone()),
    );
    let revision = owner.polled_status().unwrap().revision;
    let during_owner = owner.clone();
    let during_rig = rig.clone();
    *owner.polled_capture_hook.lock().unwrap() = Some(Box::new(move || {
        let path = during_rig.path.join("subc.jsonc");
        let mut document: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        document["route_bind_relay_timeout_ms"] = json!(30000);
        std::fs::write(path, serde_json::to_vec(&document).unwrap()).unwrap();
        approve(&during_rig, &during_owner, 2);
    }));
    let old = registration_declaration_poll(handler.clone(), "config.status").await;
    assert_eq!(old["status"]["revision"], revision);
    assert!(
        !old["status"]["approved_but_inactive"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["entry"] == "route_bind_relay_timeout_ms"),
        "{old}"
    );
    let new = registration_declaration_poll(handler, "config.status").await;
    assert!(new["status"]["revision"].as_u64().unwrap() > revision);
    assert_eq!(new["status"]["pending_proposals"], 0);
    assert!(
        new["status"]["approved_but_inactive"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["entry"] == "route_bind_relay_timeout_ms" && row["generation"] == 2),
        "{new}"
    );
}
