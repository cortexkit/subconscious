use super::*;

#[tokio::test]
async fn staged_unstamp_enable_preserves_a_live_child_instead_of_parking_it() {
    let (_scratch, owner, supervisor, _, spec) =
        crate::config_gate::runtime::RuntimeOwner::unactivated_supervisor_fixture_for_test();
    std::fs::write(&spec.args[0], b"keep running").unwrap();
    let RegisteredConfiguredModule {
        mut spec,
        mut runtime,
        module,
        mut commands,
    } = supervisor
        .register_configured_with_health(
            spec,
            true,
            Default::default(),
            None,
            Default::default(),
            DEFAULT_REGISTER_TIMEOUT,
        )
        .unwrap();
    let mut child = Some(
        spawn_and_mark_running_async(&spec, &runtime, &module.inner.snapshot)
            .await
            .unwrap(),
    );
    #[cfg(target_os = "macos")]
    child.as_mut().unwrap().confirm_privacy_exec().await;
    let child_spawned_at = child.as_ref().unwrap().spawned_at;
    let before = module.status().unwrap();
    // Model the desired-state update before reaping: the actor still owns a
    // live child even though its desired enabled flag has already been cleared.
    update_snapshot(&module.inner.snapshot, Some("a"), |state| {
        state.enabled = false
    })
    .unwrap();
    let plan = owner
        .finish_plan(
            owner
                .prepare(crate::config_gate::PlanKind::Unstamp)
                .unwrap(),
            crate::config_gate::runtime::daemon_review,
        )
        .unwrap();
    let facts = crate::config_gate::ApprovalFacts {
        connection: 7,
        owner_uid: true,
        desktop_app: true,
        attested_operator_app: true,
        os_prompt_available: true,
        phone: None,
        caller: Default::default(),
    };
    owner
        .desktop_view_delivered(ConnectionId::new(7), &plan)
        .unwrap();
    let approval = owner.begin_approval(&plan.digest, &facts).unwrap();
    owner
        .finish_approval(
            approval.attempt(),
            &facts,
            crate::config_gate::ApprovalReply::Presence(
                crate::config_gate::PresenceDecision::InteractiveApproval,
            ),
            2,
        )
        .unwrap();
    assert!(owner.can_stage_unstamp_enable());
    let (tx, rx) = oneshot::channel();
    let command = SupervisorCommand::SetEnabled {
        enabled: true,
        reset_budget: true,
        requester: Some(Principal::Direct),
        reply: tx,
    };
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
            &mut VecDeque::new()
        )
        .await
    );
    assert!(rx.await.unwrap().unwrap());
    let staged = module.status().unwrap();
    let parked = lock_snapshot(&module.inner.snapshot)
        .unwrap()
        .config_hold_parked;
    assert_eq!(child.as_ref().unwrap().spawned_at, child_spawned_at);
    let (tx, rx) = oneshot::channel();
    assert!(
        !handle_supervisor_command(
            SupervisorCommand::Drain {
                requester: None,
                reply: tx
            },
            &mut spec,
            &mut runtime,
            &supervisor.registry,
            &supervisor.process_liveness,
            &module.inner.snapshot,
            &mut child,
            &mut commands,
            &mut VecDeque::new()
        )
        .await
    );
    rx.await.unwrap().unwrap();
    assert!(staged.enabled && staged.process_alive);
    assert_eq!(staged.state, ModuleState::Running);
    assert_eq!(staged.spawn_generation, before.spawn_generation);
    assert!(
        !parked,
        "an actor with its live child is not parked for release"
    );
}

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

    // No task processes supervisor commands until the recorder has queued its
    // hold behind the restart. Waiting on the queue length fixes their order.
    let recording = tokio::spawn(async move { handler.record_config_exit_for_test(exited).await });
    tokio::time::timeout(Duration::from_secs(1), async {
        while commands.len() < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
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
        let (recorded, ()) = tokio::join!(recording, drive);
        recorded.unwrap();
    })
    .await
    .expect("an earlier restart must not deadlock the exit hold behind the module gate");
    assert!(owner.delivery_exit_fence("a").unwrap().is_none());
    let status = module.status().unwrap();
    assert_eq!(status.state, ModuleState::Stopped);
    assert!(!status.process_alive);
    assert!(child.is_none());
    assert_eq!(status.restart_count, 0);
    assert!(
        status.stop_reason.is_none(),
        "a successful first-generation hold is not an infrastructure failure"
    );
}

#[tokio::test]
async fn config_exit_record_failure_retries_then_retains_stopped_reason_activity_and_fence() {
    assert_exit_reconciliation_failure(true).await;
}

#[tokio::test]
async fn config_exit_hold_failure_retries_then_retains_stopped_reason_activity_and_fence() {
    assert_exit_reconciliation_failure(false).await;
}

async fn assert_exit_reconciliation_failure(tamper_store: bool) {
    let (_scratch, owner, supervisor, handler, spec) =
        crate::config_gate::runtime::RuntimeOwner::unactivated_supervisor_fixture_for_test();
    let root = spec.program.parent().unwrap().to_owned();
    let handler = match Arc::try_unwrap(handler) {
        Ok(handler) => Arc::new(handler.with_activity_ledger(&root, "exit-failure".into())),
        Err(_) => panic!("the unactivated fixture must not retain its handler"),
    };
    let RegisteredConfiguredModule {
        module, commands, ..
    } = supervisor
        .register_configured_with_health(
            spec,
            true,
            HealthConfig::default(),
            None,
            RestartPolicy::default(),
            DEFAULT_REGISTER_TIMEOUT,
        )
        .unwrap();
    // A real closed command channel makes the hold fail without a synthetic
    // supervisor error. The process has already exited; a restart was pending.
    drop(commands);
    update_snapshot(&module.inner.snapshot, Some("a"), |state| {
        state.state = ModuleState::Failed;
        state.respawn_pending = true;
    })
    .unwrap();
    let spawn = owner
        .delivery_spawned_in_slot(
            "a",
            Instant::now(),
            Duration::from_secs(5),
            Duration::from_secs(2),
            true,
        )
        .unwrap()
        .unwrap();
    owner.delivery_child_exited(&spawn).unwrap();
    let exited = owner.delivery_exits().unwrap().pop().unwrap();
    if tamper_store {
        // Corrupt the actual authenticated store, not a mock return value.
        std::fs::write(
            root.join("store").join(crate::config_store::STORE_FILE),
            b"{}",
        )
        .unwrap();
    }
    let retry_again = tokio::time::timeout(
        Duration::from_secs(2),
        handler.record_config_exit_for_test(exited),
    )
    .await
    .unwrap();
    assert_eq!(owner.exit_record_attempts_for_test(), 3);
    assert!(
        !retry_again,
        "terminal reconciliation must not begin a fresh retry cycle on notification"
    );
    let status = module.status().unwrap();
    let (code, kind) = if tamper_store {
        ("config_exit_record_failed", "store_tampered")
    } else {
        ("config_exit_hold_failed", "supervisor_command_closed")
    };
    assert_eq!(
        status.stop_reason,
        Some(ModuleStopReason {
            code: code.into(),
            error_kind: kind.into()
        })
    );
    assert_eq!(status.state, ModuleState::Stopped);
    assert!(!status.process_alive);
    assert!(
        !lock_snapshot(&module.inner.snapshot)
            .unwrap()
            .respawn_pending
    );
    assert_eq!(status.restart_count, 0);
    assert!(owner.delivery_exit_fence("a").unwrap().is_some());
    assert_eq!(supervisor.child_roster.live_count_for_test(), 0);
    let ledger = supervisor
        .supervisor_handle
        .as_ref()
        .unwrap()
        .activity()
        .unwrap();
    // A successful append is a FIFO writer barrier for the preceding terminal
    // lifecycle event. No sleeps or speculative file-read timing are needed.
    ledger
        .record(
            "barrier".into(),
            serde_json::from_value(serde_json::json!({
                "op":"activity.record", "kind":"barrier.flushed", "summary":"Queue flushed"
            }))
            .unwrap(),
            subc_control::ActivityActor {
                principal: subc_protocol::Principal::Direct,
                scope_owner: None,
                agent_id: None,
                run_id: None,
                flow_id: None,
                project_root: None,
                workdir: None,
                project_id: None,
            },
            1,
        )
        .await
        .unwrap();
    let entries = ledger
        .query(
            serde_json::from_value(serde_json::json!({
                "op":"activity.query", "limit":1000
            }))
            .unwrap(),
        )
        .await
        .unwrap()
        .entries;
    let event = entries
        .iter()
        .find(|entry| entry.kind == "config.exit_failure")
        .expect("the terminal stopped reason must reach the actual activity log");
    let activity = serde_json::to_value(event).unwrap();
    assert_eq!(activity["subject"]["id"], "a");
    assert_eq!(activity["details"]["reason"], code);
    assert_eq!(activity["details"]["error_kind"], kind);
}
