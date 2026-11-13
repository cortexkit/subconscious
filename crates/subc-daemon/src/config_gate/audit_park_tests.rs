use super::*;

#[tokio::test]
async fn park_ownership_approved_release_failure_is_latched() {
    approved_park_release_failure(false).await;
}

#[tokio::test]
async fn park_ownership_deferred_approved_release_failure_is_latched() {
    approved_park_release_failure(true).await;
}

async fn approved_park_release_failure(defer_first: bool) {
    let fixture = legacy().await;
    entry(&fixture.rig, "protocol", json!("none"));
    let first = fixture.rig.stamp(&fixture.owner);
    fixture
        .handler
        .reconcile_exit_for_test(fixture.owner.clone(), first)
        .await
        .unwrap();
    fixture.module.set_enabled(false).await.unwrap();
    entry(&fixture.rig, "drain_timeout_ms", json!(0));
    let store = fixture
        .rig
        .path
        .join("store")
        .join(crate::config_store::STORE_FILE);
    if !defer_first {
        std::fs::write(&store, b"tampered").unwrap();
        assert!(fixture.owner.check_recovery().unwrap());
    }
    let generation = approve(
        &fixture.rig,
        &fixture.owner,
        if defer_first {
            PlanKind::Change
        } else {
            PlanKind::Stamp
        },
    );
    let approved = fixture
        .owner
        .authority_exit_configuration(generation)
        .unwrap()
        .unwrap()
        .3;
    let gate = fixture.owner.module_gate("a").unwrap();
    let held = if defer_first {
        Some(gate.lock_owned().await)
    } else {
        None
    };
    fixture
        .handler
        .reconcile_exit_for_test(fixture.owner.clone(), generation)
        .await
        .unwrap();
    if defer_first {
        assert_eq!(fixture.module.state().unwrap(), ModuleState::Restarting);
        std::fs::write(&store, b"tampered").unwrap();
        assert!(fixture.owner.check_recovery().unwrap());
        drop(held);
        until(|| fixture.module.state().unwrap() == ModuleState::Stopped).await;
        let recovery = approve(&fixture.rig, &fixture.owner, PlanKind::Stamp);
        fixture
            .handler
            .reconcile_exit_for_test(fixture.owner.clone(), recovery)
            .await
            .unwrap();
    } else {
        drop(held);
    }
    assert_eq!(fixture.module.state().unwrap(), ModuleState::Stopped);
    let (daemon, saved) = fixture.module.config_hold_origin_for_test();
    assert!(!daemon);
    assert_eq!(saved, Some(approved.identity()));
    fixture.owner.fail_delivery_spawns_for_module_for_test("a");
    let failure = fixture.module.release_config_hold().await.unwrap_err();
    assert!(failure.to_string().contains("delivery_spawn_exhausted"));
    assert_eq!(fixture.module.state().unwrap(), ModuleState::Failed);
    assert!(
        fixture
            .module
            .has_approved_start_failure(&approved.identity()),
        "a failed release must retain the approval that owned the parked attempt"
    );
    assert_eq!(fixture.module.retry_deadline_for_test(), None);
    fixture.owner.clear_delivery_spawn_failure_for_test();
    let next = approve(&fixture.rig, &fixture.owner, PlanKind::Change);
    let next_approved = fixture
        .owner
        .authority_exit_configuration(next)
        .unwrap()
        .unwrap()
        .3;
    assert!(!next_approved.changes_entry("a"));
    fixture
        .handler
        .reconcile_exit_for_test(fixture.owner.clone(), next)
        .await
        .unwrap();
    let after = fixture.module.status().unwrap();
    let latched = fixture
        .module
        .has_approved_start_failure(&next_approved.identity());
    drop(fixture.primary);
    fixture.module.stop().await.unwrap();
    assert!(after.process_alive, "{after:?}");
    assert!(!latched);
}

#[tokio::test]
async fn park_ownership_daemon_attempt_has_no_approval() {
    let fixture = gated_claim_fixture().await;
    entry(&fixture.rig, "drain_timeout_ms", json!(0));
    let generation = approve(&fixture.rig, &fixture.owner, PlanKind::Change);
    let approved = fixture
        .owner
        .authority_exit_configuration(generation)
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
        .complete_authority_exit_for_test(generation)
        .unwrap();
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
        .config_delivery_failed(fixture.primary.connection_id(&fixture.handler), true)
        .await
        .unwrap();
    until(|| fixture.module.state().unwrap() == ModuleState::Stopped).await;
    let origin = fixture.module.config_hold_origin_for_test();
    drop(fixture.primary);
    fixture.module.stop().await.unwrap();
    assert!(
        origin.0,
        "a delivery-failure recovery belongs to the daemon"
    );
    assert!(origin.1.is_none(), "it cannot borrow the latest approval");
}

async fn without_advancing_time<T>(future: impl std::future::Future<Output = T>) -> T {
    let started = std::time::Instant::now();
    tokio::pin!(future);
    loop {
        tokio::select! {
            biased;
            result = &mut future => return result,
            _ = tokio::task::yield_now() => {
                assert!(started.elapsed() < Duration::from_secs(10), "real process or blocking work did not complete");
            }
        }
    }
}

#[tokio::test]
async fn delivery_failure_release_retry_is_timed_from_its_own_failure() {
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
    until(|| fixture.module.state().unwrap() == ModuleState::Stopped).await;
    assert!(!fixture.module.status().unwrap().process_alive);
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(3600)).await;
    let generation = approve(&fixture.rig, &fixture.owner, PlanKind::Unstamp);
    fixture
        .owner
        .complete_authority_exit_for_test(generation)
        .unwrap();
    let mut missing = spec(&fixture.rig);
    missing.program = fixture.rig.path.join("missing-program");
    without_advancing_time(
        fixture
            .module
            .update_configuration(missing, Default::default(), None),
    )
    .await
    .unwrap();
    let failed_at = tokio::time::Instant::now();
    assert!(without_advancing_time(fixture.module.release_config_hold())
        .await
        .is_err());
    assert_eq!(tokio::time::Instant::now(), failed_at);
    let deadline = fixture.module.retry_deadline_for_test();
    let after_release = fixture.module.status().unwrap();
    without_advancing_time(fixture.module.config_exit_hold())
        .await
        .unwrap();
    let after_barrier = fixture.module.status().unwrap();
    drop(fixture.primary);
    fixture.module.stop().await.unwrap();
    assert_eq!(
        deadline,
        Some(failed_at + Duration::from_secs(600)),
        "a long park cannot backdate the release failure or skip a retry tier"
    );
    assert_eq!(after_release.state, ModuleState::Failed);
    assert_eq!(
        after_barrier.spawn_generation,
        after_release.spawn_generation
    );
}

#[tokio::test]
async fn terminal_config_exit_failure_cannot_be_revived_by_a_hold_release() {
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
        .config_delivery_failed(fixture.primary.connection_id(&fixture.handler), true)
        .await
        .unwrap();
    until(|| fixture.module.state().unwrap() == ModuleState::Stopped).await;
    let reason = crate::supervise::ModuleStopReason {
        code: "config_exit_failed".into(),
        error_kind: "injected_terminal_failure".into(),
    };
    assert!(fixture.module.config_hold_origin_for_test().0);
    fixture.module.config_exit_failed(reason.clone()).unwrap();
    assert_eq!(fixture.module.config_hold_origin_for_test(), (false, None));
    let generation = approve(&fixture.rig, &fixture.owner, PlanKind::Unstamp);
    fixture
        .owner
        .complete_authority_exit_for_test(generation)
        .unwrap();
    let released = fixture.module.release_config_hold().await;
    let after = fixture.module.status().unwrap();
    drop(fixture.primary);
    fixture.module.stop().await.unwrap();
    assert!(
        !released.unwrap(),
        "a terminal configuration-exit failure is no longer parked"
    );
    assert_eq!(after.state, ModuleState::Stopped);
    assert_eq!(after.stop_reason, Some(reason));
    assert!(!after.process_alive);
}
