use super::*;

async fn failed_approved_actor(with_later_entries: bool) -> Legacy {
    let fixture = legacy().await;
    fixture.module.set_enabled(false).await.unwrap();
    fixture.module.stop().await.unwrap();
    fixture.handle.retire("a").unwrap();
    if with_later_entries {
        let path = fixture.rig.path.join("subc.jsonc");
        let mut config: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        for id in ["b", "c"] {
            config["modules"][id] =
                json!({"program":"/bin/sleep", "args":["600"], "protocol":"none", "enabled":false});
        }
        std::fs::write(path, serde_json::to_vec(&config).unwrap()).unwrap();
    }
    assert_eq!(fixture.rig.stamp(&fixture.owner), 1);
    fixture.owner.exhaust_delivery_spawns_for_test();
    let error = fixture
        .handler
        .reconcile_exit_for_test(fixture.owner.clone(), 1)
        .await
        .unwrap_err();
    assert_eq!(error.code, "rescan_failed");
    assert!(error.detail.contains("delivery_spawn_exhausted"), "{error}");
    let actor = fixture.handle.get("a").unwrap();
    assert_eq!(actor.state().unwrap(), ModuleState::Failed);
    actor.config_exit_hold().await.unwrap();
    fixture
}

#[tokio::test]
async fn approval_latch_does_not_alias_a_recovery_restamp_generation() {
    let fixture = failed_approved_actor(false).await;
    let old_digest = fixture
        .owner
        .lock()
        .unwrap()
        .store
        .body()
        .unwrap()
        .generations[&1]
        .generation
        .plan_digest
        .clone();
    entry(&fixture.rig, "enabled", json!(false));
    std::fs::write(
        fixture
            .rig
            .path
            .join("store")
            .join(crate::config_store::STORE_FILE),
        b"tampered",
    )
    .unwrap();
    assert!(fixture.owner.check_recovery().unwrap());
    assert_eq!(fixture.rig.stamp(&fixture.owner), 1);
    let new_digest = fixture
        .owner
        .lock()
        .unwrap()
        .store
        .body()
        .unwrap()
        .generations[&1]
        .generation
        .plan_digest
        .clone();
    assert_ne!(
        old_digest, new_digest,
        "a fresh recovery approval must have its own identity"
    );
    let result = fixture
        .handler
        .reconcile_exit_for_test(fixture.owner.clone(), 1)
        .await;
    let actor = fixture.handle.get("a").unwrap();
    let state = actor.status().unwrap();
    drop(fixture.primary);
    actor.stop().await.unwrap();
    assert!(
        result.is_ok(),
        "the old generation-1 failure cannot reject this new approval: {result:?}"
    );
    assert!(!state.enabled);
    assert!(fixture
        .owner
        .authority_exit_configuration(1)
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn approval_latch_does_not_block_later_modules_in_the_same_reconcile() {
    let fixture = failed_approved_actor(true).await;
    let result = fixture
        .handler
        .reconcile_exit_for_test(fixture.owner.clone(), 1)
        .await;
    let later = ["b", "c"].map(|id| {
        fixture
            .handle
            .get(id)
            .map(|module| module.status().unwrap())
    });
    drop(fixture.primary);
    for module in fixture.handle.list() {
        module.stop().await.unwrap();
    }
    let error = result.unwrap_err();
    assert_eq!(error.code, "rescan_failed");
    assert!(error.detail.contains('a'), "{error}");
    for (id, state) in ["b", "c"].into_iter().zip(later) {
        assert!(
            state.is_some(),
            "failed module a must not block reconciling {id}"
        );
        assert!(!state.unwrap().enabled);
    }
}

#[tokio::test]
async fn approval_latch_clears_after_a_successful_registered_reload() {
    let fixture = failed_approved_actor(false).await;
    fixture.owner.restore_unused_delivery_spawns_for_test();
    let actor = fixture.handle.get("a").unwrap();
    let reloading = actor.clone();
    let reload = tokio::spawn(async move { reloading.reload().await });
    tokio::time::timeout(Duration::from_secs(3), async {
        while actor.status().unwrap().spawn_generation == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(actor.status().unwrap().process_alive);
    assert!(fixture.registry.get_module("a").unwrap().is_none());
    // Supply only HELLO's published registry result. The real Reload still
    // starts its child and must observe registration before acknowledging it;
    // authentication is covered by the separate registration-channel fixtures.
    let connection = ConnectionId::new(445);
    fixture
        .registry
        .register_with_control_ops(
            subc_protocol::manifest::ModuleManifest::builder("a", "1.0.0").build(),
            subc_protocol::PROTOCOL_VERSION,
            connection,
            vec![],
        )
        .unwrap();
    reload.await.unwrap().unwrap();
    let reconcile = fixture
        .handler
        .reconcile_exit_for_test(fixture.owner.clone(), 1)
        .await;
    fixture.registry.deregister_connection(connection).unwrap();
    drop(fixture.primary);
    actor.stop().await.unwrap();
    assert!(
        reconcile.is_ok(),
        "a successful reload must clear its old approved-start failure: {reconcile:?}"
    );
}
