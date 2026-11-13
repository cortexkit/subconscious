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
