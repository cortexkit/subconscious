use super::*;

fn pause_preflight(
    owner: &Arc<crate::config_gate::runtime::RuntimeOwner>,
) -> (
    tokio::sync::oneshot::Receiver<()>,
    std::sync::mpsc::Sender<()>,
) {
    let (reached, observed) = tokio::sync::oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    owner.set_spawn_preflight_hook_for_test(move || {
        let _ = reached.send(());
        wait.recv_timeout(Duration::from_secs(5)).unwrap();
    });
    (observed, release)
}

#[tokio::test]
async fn config_spawn_async_cancel_during_preflight_creates_no_child_or_slot() {
    let (_rig, owner, supervisor, _handler, spec) =
        crate::config_gate::runtime::RuntimeOwner::unactivated_supervisor_fixture_for_test();
    let roster = supervisor.child_roster.clone();
    let gate = owner.module_gate(&spec.module_id).unwrap();
    let (reached, release) = pause_preflight(&owner);
    let launch = tokio::spawn(async move { supervisor.spawn_async(spec).await });
    tokio::time::timeout(Duration::from_secs(5), reached)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(owner.unregistered_spawn_count_for_test(), 1);
    assert_eq!(roster.live_count_for_test(), 0);
    launch.abort();
    assert!(launch.await.unwrap_err().is_cancelled());
    release.send(()).unwrap();
    // The blocking preparation owns the gate until its abandoned result and
    // reservation guard are dropped. Acquiring it observes that cleanup point.
    let _prepared = tokio::time::timeout(Duration::from_secs(5), gate.lock())
        .await
        .unwrap();
    assert_eq!(owner.unregistered_spawn_count_for_test(), 0);
    assert_eq!(roster.live_count_for_test(), 0);
}

#[tokio::test]
async fn config_spawn_async_program_replaced_during_preflight_is_plan_stale() {
    let (_rig, owner, supervisor, _handler, spec) =
        crate::config_gate::runtime::RuntimeOwner::unactivated_supervisor_fixture_for_test();
    let roster = supervisor.child_roster.clone();
    let program = spec.program.clone();
    let (reached, release) = pause_preflight(&owner);
    let launch = tokio::spawn(async move { supervisor.spawn_async(spec).await });
    tokio::time::timeout(Duration::from_secs(5), reached)
        .await
        .unwrap()
        .unwrap();
    let replacement = program.with_extension("replacement");
    std::fs::copy(&program, &replacement).unwrap();
    std::fs::rename(replacement, &program).unwrap();
    release.send(()).unwrap();
    let error = match launch.await.unwrap() {
        Err(error) => error,
        Ok(module) => {
            module.stop().await.unwrap();
            panic!("plan_stale was required, but the replaced program was launched");
        }
    };
    assert!(error.to_string().contains("plan_stale"), "{error}");
    assert_eq!(owner.unregistered_spawn_count_for_test(), 0);
    assert_eq!(roster.live_count_for_test(), 0);
}
