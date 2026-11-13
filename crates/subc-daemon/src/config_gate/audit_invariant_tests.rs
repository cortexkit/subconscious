use super::*;

#[tokio::test]
async fn current_program_refusal_parks_only_under_its_recorded_hold() {
    let fixture = gated_claim_fixture().await;
    let before = fixture.module.status().unwrap();
    fixture.spawn.set_content_probe_for_test(Arc::new(|| {
        Err(GateError::new(
            "program_refused",
            "current approved program was refused",
        ))
    }));
    assert_eq!(fixture.spawn.check().unwrap_err(), "program_refused");
    assert!(fixture.owner.has_recorded_admission_hold("a"));
    tokio::time::timeout(Duration::from_secs(3), async {
        while fixture.module.status().unwrap().state != ModuleState::Stopped {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let parked = fixture.module.status().unwrap();
    drop(fixture.primary);
    fixture.module.stop().await.unwrap();
    assert!(!parked.process_alive);
    assert_eq!(parked.restart_count, before.restart_count);
    assert_eq!(parked.spawn_generation, before.spawn_generation);
}

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
    assert_eq!(error.detail, format!("a: reload failed for module 'a': failed to spawn module '{}': delivery_spawn_exhausted: delivery spawn identifiers exhausted", fixture.rig.path.join("program").display()));
    for (id, state) in ["b", "c"].into_iter().zip(later) {
        assert!(
            state.is_some(),
            "failed module a must not block reconciling {id}"
        );
        assert!(!state.unwrap().enabled);
    }
}

#[tokio::test]
async fn approval_latch_does_not_alias_an_identical_recovery_stamp() {
    let fixture = failed_approved_actor(false).await;
    let old = fixture
        .owner
        .authority_exit_configuration(1)
        .unwrap()
        .unwrap()
        .3;
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
    let fresh = fixture
        .owner
        .authority_exit_configuration(1)
        .unwrap()
        .unwrap()
        .3;
    assert_eq!(old.generation, fresh.generation);
    assert_eq!(old.approval_digest, fresh.approval_digest);
    assert!(!Arc::ptr_eq(
        &old.authority_incarnation,
        &fresh.authority_incarnation
    ));
    let actor = fixture.handle.get("a").unwrap();
    let result = actor.check_approved_start_failure(&fresh.identity());
    drop(fixture.primary);
    actor.stop().await.unwrap();
    assert!(result.is_ok(), "an identical recovery stamp is new authority even if its generation and plan digest repeat; old={:?}, fresh={:?}: {result:?}", old.identity(), fresh.identity());
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

#[tokio::test]
async fn superseded_program_refusal_exits_with_crash_accounting_and_starts_new_approval() {
    superseded_program_exit(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reapproval_release_before_old_program_park_does_not_strand_replacement() {
    superseded_program_exit(true).await;
}

async fn superseded_program_exit(release_before_park: bool) {
    let fixture = gated_claim_fixture().await;
    let before = fixture.module.status().unwrap();
    let mut kill = Some(fixture.spawn.defer_content_kill_for_test());
    let (reached, arrived) = tokio::sync::oneshot::channel();
    let (release, resume) = std::sync::mpsc::channel();
    if release_before_park {
        *fixture.owner.config_park_hook.lock().unwrap() = Some(Box::new(move || {
            reached.send(()).unwrap();
            resume.recv_timeout(Duration::from_secs(10)).unwrap();
        }));
    }
    let refuse = || {
        fixture.spawn.set_content_probe_for_test(Arc::new(|| {
            Err(GateError::new(
                "program_refused",
                "the superseded child failed its content witness",
            ))
        }))
    };
    if release_before_park {
        refuse();
        assert_eq!(fixture.spawn.check().unwrap_err(), "program_refused");
        assert!(fixture.owner.has_recorded_admission_hold("a"));
        drop(kill.take());
        tokio::time::timeout(Duration::from_secs(3), arrived)
            .await
            .unwrap()
            .unwrap();
        assert!(fixture.module.status().unwrap().process_alive);
    }
    let replacement = fixture.rig.path.join("replacement");
    std::fs::copy(fixture.rig.path.join("program"), &replacement).unwrap();
    std::fs::rename(replacement, fixture.rig.path.join("program")).unwrap();
    let generation = approve(&fixture.rig, &fixture.owner, PlanKind::Change);
    fixture
        .owner
        .complete_authority_exit_for_test(generation)
        .unwrap();
    assert!(!fixture.owner.has_recorded_admission_hold("a"));
    if release_before_park {
        for exit in fixture.owner.delivery_exits().unwrap() {
            fixture.owner.delivery_release_exit(&exit).unwrap();
        }
        let (wakes, mut observed) = tokio::sync::mpsc::unbounded_channel();
        let handler = Arc::new(
            (*fixture.handler)
                .clone()
                .with_delivery_wakes_for_test(wakes),
        );
        let worker = handler.spawn_config_delivery_worker().unwrap();
        tokio::time::timeout(Duration::from_secs(3), observed.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(fixture.owner.lock().unwrap().admission_releases.is_empty());
        assert!(fixture.module.status().unwrap().process_alive);
        worker.abort();
        let _ = worker.await;
        release.send(()).unwrap();
    } else {
        refuse();
        assert_eq!(fixture.spawn.check().unwrap_err(), "program_refused");
        assert!(!fixture.owner.has_recorded_admission_hold("a"));
        drop(kill.take());
    }
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let state = fixture.module.status().unwrap();
            if !state.process_alive || state.spawn_generation > before.spawn_generation {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    fixture.module.config_exit_hold().await.unwrap();
    let classified = fixture.module.status().unwrap();
    if classified.state == ModuleState::Stopped {
        drop(fixture.primary);
        fixture.module.stop().await.unwrap();
        panic!("the superseded refusal has no current hold and cannot park: {classified:?}");
    }
    tokio::time::timeout(Duration::from_secs(3), async {
        while fixture.module.status().unwrap().spawn_generation == before.spawn_generation {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let restarted = fixture.module.status().unwrap();
    drop(fixture.primary);
    fixture.module.stop().await.unwrap();
    assert!(restarted.process_alive);
    assert_eq!(
        restarted.restart_count,
        before.restart_count + 1,
        "superseded refusal must use ordinary crash accounting, not a budget-free 100ms Spawn"
    );
    assert_eq!(restarted.spawn_generation, before.spawn_generation + 1);
}

#[tokio::test]
async fn delivery_failure_park_saves_the_daemon_attempt_before_release() {
    let fixture = gated_claim_fixture().await;
    entry(&fixture.rig, "drain_timeout_ms", json!(0));
    let configured = approve(&fixture.rig, &fixture.owner, PlanKind::Change);
    let approved = fixture
        .owner
        .authority_exit_configuration(configured)
        .unwrap()
        .unwrap()
        .3;
    fixture
        .module
        .reconcile_approved_configuration(approved)
        .await
        .unwrap();
    fixture
        .owner
        .complete_authority_exit_for_test(configured)
        .unwrap();
    let connection = fixture.primary.connection_id(&fixture.handler);
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
    fixture
        .module
        .config_delivery_failed(connection, true)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while fixture.module.status().unwrap().state != ModuleState::Stopped {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let parked = fixture.module.status().unwrap();
    assert!(!parked.process_alive);
    let generation = approve(&fixture.rig, &fixture.owner, PlanKind::Unstamp);
    fixture
        .owner
        .complete_authority_exit_for_test(generation)
        .unwrap();
    let mut missing = spec(&fixture.rig);
    missing.program = fixture.rig.path.join("missing-program");
    fixture
        .module
        .update_configuration(missing, Default::default(), None)
        .await
        .unwrap();
    assert!(fixture.module.release_config_hold().await.is_err());
    let next_retry = fixture.module.next_retry_at_ms().unwrap();
    drop(fixture.primary);
    fixture.module.stop().await.unwrap();
    assert!(next_retry.is_some(), "a delivery-failure park must retain daemon attribution so a failing release schedules its slow retry");
}
