use super::*;

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
