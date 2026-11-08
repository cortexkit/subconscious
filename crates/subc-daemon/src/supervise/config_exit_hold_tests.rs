use super::*;

#[tokio::test]
async fn config_exit_hold_completes_behind_a_queued_config_restart_without_an_active_generation() {
    let (_scratch, owner, supervisor, handler, spec) =
        crate::config_gate::runtime::RuntimeOwner::unactivated_supervisor_fixture_for_test();
    let registered = supervisor
        .register_configured_with_health(
            spec,
            true,
            HealthConfig::default(),
            None,
            RestartPolicy::default(),
            DEFAULT_REGISTER_TIMEOUT,
        )
        .unwrap();
    let RegisteredConfiguredModule {
        mut spec,
        mut runtime,
        module,
        mut commands,
    } = registered;
    let spawn = owner
        .delivery_spawned_in_slot(
            &spec.module_id,
            Instant::now(),
            Duration::from_secs(5),
            Duration::from_secs(2),
            true,
        )
        .unwrap()
        .unwrap();
    owner.delivery_child_exited(&spawn).unwrap();
    let exited = owner.delivery_exits().unwrap().pop().unwrap();
    module.config_apply_restart(1).await.unwrap();

    // With no task processing supervisor commands, this first poll records the
    // failure and queues the hold behind the restart. Scheduling cannot reorder them.
    let mut recording = Box::pin(handler.record_config_exit_for_test(exited));
    std::future::poll_fn(|cx| {
        assert!(std::future::Future::poll(recording.as_mut(), cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    assert!(owner.delivery_exit_fence("a").unwrap().is_some());
    let mut child = None;
    let drive = async {
        for index in 0..2 {
            let command = commands.recv().await.unwrap();
            if index == 0 {
                assert!(matches!(
                    command,
                    SupervisorCommand::ConfigApplyRestart { .. }
                ));
            } else {
                assert!(matches!(command, SupervisorCommand::ConfigExitHold { .. }));
            }
            assert!(
                handle_supervisor_command(
                    command,
                    &mut spec,
                    &mut runtime,
                    &supervisor.registry,
                    &supervisor.process_liveness,
                    &module.inner.snapshot,
                    &mut child,
                    &mut commands,
                    &mut VecDeque::new(),
                )
                .await
            );
        }
    };
    tokio::time::timeout(Duration::from_secs(1), async {
        tokio::join!(recording, drive);
    })
    .await
    .expect("an earlier restart must not deadlock the exit hold behind the module gate");
    assert!(owner.delivery_exit_fence("a").unwrap().is_none());
    let status = module.status().unwrap();
    assert_eq!(status.state, ModuleState::Stopped);
    assert!(!status.process_alive);
    assert!(child.is_none());
    assert_eq!(status.restart_count, 0);
}
