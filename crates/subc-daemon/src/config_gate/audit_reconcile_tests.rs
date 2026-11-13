use super::*;

#[tokio::test]
async fn approval_changed_crash_failed_success_cancels_predecessor_retry() {
    changed_crash_failed_start_cancels_retry(false).await;
}

#[tokio::test]
async fn approval_changed_crash_failed_failure_cancels_predecessor_retry() {
    changed_crash_failed_start_cancels_retry(true).await;
}

async fn changed_crash_failed_start_cancels_retry(fail_start: bool) {
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
    let old_deadline = fixture.module.retry_deadline_for_test().unwrap();
    entry(&fixture.rig, "drain_timeout_ms", json!(0));
    let generation = approve(&fixture.rig, &fixture.owner, PlanKind::Change);
    let approved = fixture
        .owner
        .authority_exit_configuration(generation)
        .unwrap()
        .unwrap()
        .3;
    assert!(approved.changes_entry("a"));
    assert!(
        !fixture
            .module
            .check_approved_start_failure(&approved.identity())
            .unwrap(),
        "this fixture must isolate the changed-entry path, not an old approval latch"
    );
    if fail_start {
        fixture.owner.fail_delivery_spawns_for_module_for_test("a");
    }
    let result = fixture
        .handler
        .reconcile_exit_for_test(fixture.owner.clone(), generation)
        .await;
    let after = fixture.module.status().unwrap();
    let after_deadline = fixture.module.retry_deadline_for_test();
    let latched = fixture
        .module
        .has_approved_start_failure(&approved.identity());
    drop(fixture.primary);
    fixture.module.stop().await.unwrap();
    if fail_start {
        assert_eq!(result.unwrap_err().code, "rescan_failed");
        assert_eq!(after.state, ModuleState::Failed);
        assert!(latched);
    } else {
        result.unwrap();
        assert!(after.process_alive, "{after:?}");
        assert!(!latched);
    }
    assert_eq!(
        after_deadline, None,
        "the approved start must cancel its predecessor's daemon retry at {old_deadline:?}"
    );
}

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
    // The fixture retains the approval captured before its failed reconciliation
    // completes the global exit. Its failed start did not change the actor's entry.
    let generation = approve(&fixture.rig, &fixture.owner, PlanKind::Change);
    let effective = fixture
        .owner
        .authority_exit_configuration(generation)
        .unwrap()
        .unwrap()
        .3;
    fixture
        .owner
        .daemon_entry_reconciled(&effective, "a")
        .unwrap();
    fixture.owner.complete_authority_exit(&effective).unwrap();
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

#[tokio::test]
async fn approval_reconcile_latched_module_does_not_withhold_generation_completion() {
    let fixture = failed_approved_actor(false).await;
    let path = fixture.rig.path.join("subc.jsonc");
    let mut config: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    config["modules"]["b"] =
        json!({"program":"/bin/sleep", "args":["600"], "protocol":"none", "enabled":true});
    std::fs::write(path, serde_json::to_vec(&config).unwrap()).unwrap();
    let generation = approve(&fixture.rig, &fixture.owner, PlanKind::Change);
    let identity = fixture
        .owner
        .authority_exit_configuration(generation)
        .unwrap()
        .unwrap()
        .3
        .identity();
    fixture.owner.restore_unused_delivery_spawns_for_test();
    fixture.owner.fail_delivery_spawns_for_module_for_test("a");
    let error = fixture
        .handler
        .reconcile_exit_for_test(fixture.owner.clone(), generation)
        .await
        .unwrap_err();
    assert_eq!(error.code, "rescan_failed");
    assert!(error.detail.starts_with("a: "), "{error}");
    assert!(!error.detail.contains("b:"), "{error}");
    let a = fixture.handle.get("a").unwrap();
    let b = fixture.handle.get("b").unwrap();
    let b_before = b.status().unwrap();
    assert_eq!(a.state().unwrap(), ModuleState::Failed);
    assert!(a.has_approved_start_failure(&identity));
    assert!(b_before.process_alive);
    assert_eq!(
        fixture.owner.pending_authority_exit().unwrap(),
        None,
        "a current module-local failure must not retain the generation's exit"
    );
    assert!(fixture.owner.check_module_hold("b").is_ok());
    let repeated = fixture
        .handler
        .reconcile_exit_for_test(fixture.owner.clone(), generation)
        .await
        .unwrap_err();
    assert_eq!(repeated.code, "rescan_failed");
    assert!(repeated.detail.starts_with("a: "), "{repeated}");
    assert_eq!(
        b.status().unwrap().spawn_generation,
        b_before.spawn_generation
    );
    assert!(!a.release_config_hold().await.unwrap());
    drop(fixture.primary);
    a.stop().await.unwrap();
    b.stop().await.unwrap();
}
