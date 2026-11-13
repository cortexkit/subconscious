use super::*;

#[tokio::test]
async fn content_refusal_off_actor_publishes_hold_before_child_exit() {
    let fixture = gated_claim_fixture().await;
    let before = fixture.module.status().unwrap();
    let (reached, arrived) = tokio::sync::oneshot::channel();
    let (release, resume) = std::sync::mpsc::channel();
    *fixture.owner.content_kill_hook.lock().unwrap() = Some(Box::new(move || {
        reached.send(()).unwrap();
        resume.recv_timeout(Duration::from_secs(10)).unwrap();
    }));
    fixture.spawn.set_content_probe_for_test(Arc::new(|| {
        Err(GateError::new("program_refused", "off-actor image refusal"))
    }));
    let spawn = fixture.spawn.clone();
    let checking = tokio::task::spawn_blocking(move || spawn.check());
    tokio::time::timeout(Duration::from_secs(3), arrived)
        .await
        .unwrap()
        .unwrap();
    until(|| {
        let status = fixture.module.status().unwrap();
        !status.process_alive || status.spawn_generation != before.spawn_generation
    })
    .await;
    let classified = fixture.module.status().unwrap();
    let held_before_checker_returns = fixture.owner.has_recorded_admission_hold("a");
    release.send(()).unwrap();
    assert_eq!(checking.await.unwrap().unwrap_err(), "program_refused");
    drop(fixture.primary);
    fixture.module.stop().await.unwrap();
    assert!(
        held_before_checker_returns,
        "the checker cannot kill before it records the approval's hold"
    );
    assert_eq!(classified.state, ModuleState::Stopped, "{classified:?}");
    assert_eq!(classified.restart_count, before.restart_count);
    assert_eq!(classified.spawn_generation, before.spawn_generation);
}

#[tokio::test]
async fn superseded_program_exit_cannot_park_under_a_different_current_program_hold() {
    let fixture = gated_claim_fixture().await;
    let before = fixture.module.status().unwrap();
    let kill = fixture.spawn.defer_content_kill_for_test();
    let replacement = fixture.rig.path.join("replacement");
    std::fs::copy(fixture.rig.path.join("program"), &replacement).unwrap();
    std::fs::rename(replacement, fixture.rig.path.join("program")).unwrap();
    let generation = approve(&fixture.rig, &fixture.owner, PlanKind::Change);
    fixture
        .owner
        .complete_authority_exit_for_test(generation)
        .unwrap();
    let admission = fixture.owner.prepare_spawn(&spec(&fixture.rig)).unwrap();
    let new_base = admission.approved_base().unwrap().clone();
    assert!(fixture
        .owner
        .record_program_refusal(
            "a",
            &new_base,
            subc_config::raw_digest(b"new approval's separate refusal")
        )
        .unwrap());
    assert!(fixture.owner.has_recorded_admission_hold("a"));
    fixture.spawn.set_content_probe_for_test(Arc::new(|| {
        Err(GateError::new(
            "program_refused",
            "the superseded child's own refusal",
        ))
    }));
    assert_eq!(fixture.spawn.check().unwrap_err(), "program_refused");
    assert!(
        fixture.owner.has_recorded_admission_hold("a"),
        "a different approval's hold must remain current"
    );
    drop(kill);
    until(|| {
        let state = fixture.module.status().unwrap();
        state.state == ModuleState::Stopped && !state.process_alive
    })
    .await;
    let after = fixture.module.status().unwrap();
    drop(fixture.primary);
    fixture.module.stop().await.unwrap();
    assert_eq!(after.restart_count, before.restart_count + 1, "the old program's refusal must take ordinary crash accounting before its new attempted start parks under the new program's hold");
    assert_eq!(
        after.spawn_generation, before.spawn_generation,
        "the new held program must not execute"
    );
}
