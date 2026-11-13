use super::*;

#[tokio::test]
async fn approval_reconcile_changed_enabled_failed_entry_attempts_start() {
    let fixture = failed_approved_actor(false).await;
    fixture.owner.restore_unused_delivery_spawns_for_test();
    entry(&fixture.rig, "drain_timeout_ms", json!(0));
    let generation = approve(&fixture.rig, &fixture.owner, PlanKind::Change);
    let actor = fixture.handle.get("a").unwrap();
    let before = actor.status().unwrap();
    let result = fixture
        .handler
        .reconcile_exit_for_test(fixture.owner.clone(), generation)
        .await;
    let after = actor.status().unwrap();
    drop(fixture.primary);
    actor.stop().await.unwrap();
    assert!(result.is_ok(), "{result:?}");
    assert!(
        after.process_alive,
        "a changed enabled Failed entry needs a real start: {after:?}"
    );
    assert!(after.spawn_generation > before.spawn_generation);
}

#[tokio::test]
async fn approval_reconcile_old_identity_failure_retries_unchanged_entry() {
    let fixture = failed_approved_actor(false).await;
    fixture.owner.restore_unused_delivery_spawns_for_test();
    let old = fixture
        .owner
        .authority_exit_configuration(1)
        .unwrap()
        .unwrap()
        .3;
    fixture.owner.daemon_entry_reconciled(&old, "a").unwrap();
    let generation = approve(&fixture.rig, &fixture.owner, PlanKind::Change);
    let approved = fixture
        .owner
        .authority_exit_configuration(generation)
        .unwrap()
        .unwrap()
        .3;
    assert!(
        !approved.changes_entry("a"),
        "this leg isolates a previous approved-start failure"
    );
    let actor = fixture.handle.get("a").unwrap();
    let before = actor.status().unwrap();
    let result = fixture
        .handler
        .reconcile_exit_for_test(fixture.owner.clone(), generation)
        .await;
    let after = actor.status().unwrap();
    drop(fixture.primary);
    actor.stop().await.unwrap();
    assert!(result.is_ok(), "{result:?}");
    assert!(
        after.process_alive,
        "a new identity retries its old approved failure: {after:?}"
    );
    assert!(after.spawn_generation > before.spawn_generation);
}

#[tokio::test]
async fn approval_reconcile_unchanged_crash_failed_actor_is_not_revived_or_latched() {
    let fixture = legacy().await;
    entry(&fixture.rig, "protocol", json!("none"));
    let first = fixture.rig.stamp(&fixture.owner);
    fixture
        .handler
        .reconcile_exit_for_test(fixture.owner.clone(), first)
        .await
        .unwrap();
    fixture.module.seed_crash_budget_for_test(100);
    rustix::process::kill_process(
        rustix::process::Pid::from_raw(fixture.module.status().unwrap().pid.unwrap() as i32)
            .unwrap(),
        rustix::process::Signal::KILL,
    )
    .unwrap();
    until(|| {
        fixture.module.state().unwrap() == ModuleState::Failed
            && fixture.module.retry_deadline_for_test().is_some()
    })
    .await;
    let before = fixture.module.status().unwrap();
    let deadline = fixture.module.retry_deadline_for_test();
    let generation = approve(&fixture.rig, &fixture.owner, PlanKind::Change);
    let approved = fixture
        .owner
        .authority_exit_configuration(generation)
        .unwrap()
        .unwrap()
        .3;
    assert!(!approved.changes_entry("a"));
    fixture
        .handler
        .reconcile_exit_for_test(fixture.owner.clone(), generation)
        .await
        .unwrap();
    let after = fixture.module.status().unwrap();
    let after_deadline = fixture.module.retry_deadline_for_test();
    let has_older_failure = fixture
        .module
        .check_approved_start_failure(&approved.identity());
    drop(fixture.primary);
    fixture.module.stop().await.unwrap();
    assert_eq!(after.state, ModuleState::Failed);
    assert!(!after.process_alive);
    assert_eq!(after.spawn_generation, before.spawn_generation);
    assert_eq!(after.restart_count, before.restart_count);
    assert_eq!(after_deadline, deadline);
    assert!(
        !has_older_failure.unwrap(),
        "an unrelated crash cannot acquire an approval failure"
    );
}
