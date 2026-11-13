use super::*;

#[tokio::test]
async fn current_hold_notification_preserves_pending_reload() {
    held_pending_work("reload", false).await;
}

#[tokio::test]
async fn current_hold_notification_preserves_crash_backoff() {
    held_pending_work("spawn", false).await;
}

#[tokio::test]
async fn current_hold_notification_preserves_previous_instance_wait() {
    held_pending_work("wait", false).await;
}

#[tokio::test]
async fn refused_restart_preserves_pending_reload_and_daemon_attribution() {
    held_pending_work("reload", true).await;
}

async fn held_pending_work(work: &str, operator_restart: bool) {
    let (_scratch, owner, supervisor, _, spec) =
        crate::config_gate::runtime::RuntimeOwner::unactivated_supervisor_fixture_for_test();
    std::fs::write(
        spec.program
            .parent()
            .unwrap()
            .join("store")
            .join(crate::config_store::STORE_FILE),
        b"tampered",
    )
    .unwrap();
    assert!(owner.check_recovery().unwrap());
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
    let deadline = Instant::now() + Duration::from_secs(120);
    update_snapshot(&module.inner.snapshot, Some("a"), |state| {
        if work == "wait" {
            state.waiting_for_previous_pid = Some(4242);
            state.waiting_for_previous_detail = Some("previous process still alive".into());
        } else {
            state.state = ModuleState::Restarting;
            state.respawn_pending = true;
        }
    })
    .unwrap();
    if work != "wait" {
        *runtime.scheduled_respawn.lock().unwrap() = Some(PendingRespawn {
            deadline,
            kind: if work == "reload" {
                RespawnKind::Reload
            } else {
                RespawnKind::Spawn
            },
            daemon_initiated: true,
        });
    }
    let (reload_reply, mut reload_result) = oneshot::channel();
    if work == "reload" {
        *runtime.deferred_reload_reply.lock().unwrap() = Some(reload_reply);
    }
    runtime.recovery_is_daemon = true;
    let before = module.inner.snapshot.lock().unwrap().clone();
    let (reply, result) = oneshot::channel();
    let command = if operator_restart {
        SupervisorCommand::Restart {
            requester: None,
            drain_timeout_ms: None,
            received_at_generation: 0,
            queued_at: Instant::now(),
            reply,
        }
    } else {
        SupervisorCommand::ConfigExitHold { reply }
    };
    assert!(
        handle_supervisor_command(
            command,
            &mut spec,
            &mut runtime,
            &supervisor.registry,
            &supervisor.process_liveness,
            &module.inner.snapshot,
            &mut None,
            &mut commands,
            &mut VecDeque::new()
        )
        .await
    );
    let answer = result.await.unwrap();
    if operator_restart {
        assert!(matches!(answer, Err(SuperviseError::ConfigHeld { .. })));
    } else {
        answer.unwrap();
    }
    assert_eq!(
        *module.inner.snapshot.lock().unwrap(),
        before,
        "a notification or refused command cannot own the pending work"
    );
    assert!(
        runtime.recovery_is_daemon,
        "refused commands cannot relabel pending daemon work"
    );
    let scheduled = *runtime.scheduled_respawn.lock().unwrap();
    if work == "wait" {
        assert!(scheduled.is_none());
    } else {
        let pending = scheduled.expect("the command erased pending work");
        assert_eq!(pending.deadline, deadline);
        assert_eq!(
            matches!(pending.kind, RespawnKind::Reload),
            work == "reload"
        );
        assert!(pending.daemon_initiated);
    }
    if work == "reload" {
        assert!(runtime.deferred_reload_reply.lock().unwrap().is_some());
        assert!(matches!(
            reload_result.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
    }
}

#[tokio::test]
async fn deferred_restart_keeps_target_unavailable_wire_code() {
    deferred_wire_code("restart", "target_unavailable").await;
}

#[tokio::test]
async fn deferred_reload_keeps_reload_failed_wire_code() {
    deferred_wire_code("reload", "reload_failed").await;
}

#[tokio::test]
async fn deferred_enable_keeps_target_unavailable_wire_code() {
    deferred_wire_code("set_enabled", "target_unavailable").await;
}

async fn deferred_wire_code(operation: &str, expected: &str) {
    let (_scratch, _, supervisor, _, spec) =
        crate::config_gate::runtime::RuntimeOwner::unactivated_supervisor_fixture_for_test();
    let RegisteredConfiguredModule {
        module,
        mut commands,
        ..
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
    let responder = tokio::spawn(async move {
        let error = SuperviseError::ConfigDeliveryDeferred {
            module_id: "a".into(),
        };
        match commands.recv().await.unwrap() {
            SupervisorCommand::Restart { reply, .. } | SupervisorCommand::Reload { reply, .. } => {
                reply.send(Err(error)).unwrap();
            }
            SupervisorCommand::SetEnabled { reply, .. } => {
                reply.send(Err(error)).unwrap();
            }
            other => panic!("unexpected request: {other:?}"),
        }
    });
    let handler = crate::ControlHandler::new(supervisor.registry.clone())
        .with_supervisor(supervisor.supervisor_handle.as_ref().unwrap().clone());
    let (tx, _rx) = mpsc::channel(8);
    let ctx = crate::RouteCtx {
        connection_id: ConnectionId::new(51),
        egress: crate::FrameSink::new(tx),
    };
    let request = serde_json::json!({"op":format!("supervisor.{operation}"),"module_id":module.module_id(),"enabled":true});
    let frames = handler
        .handle_control_frame(&ctx, crate::control::claims::tests::request(request))
        .await
        .unwrap();
    responder.await.unwrap();
    let reply: serde_json::Value = serde_json::from_slice(&frames[0].body).unwrap();
    assert_eq!(reply["code"], expected, "{reply}");
    assert!(
        reply["message"]
            .as_str()
            .unwrap()
            .contains("exited delivery target"),
        "{reply}"
    );
}

#[tokio::test]
async fn stale_config_exit_hold_keeps_reload_work_and_answers_its_caller() {
    let (_scratch, owner, supervisor, _, spec) =
        crate::config_gate::runtime::RuntimeOwner::unactivated_supervisor_fixture_for_test();
    assert!(!owner.has_recorded_admission_hold("a"));
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
    update_snapshot(&module.inner.snapshot, Some("a"), |state| {
        state.state = ModuleState::Restarting;
        state.respawn_pending = true;
    })
    .unwrap();
    let (reply, result) = oneshot::channel();
    *runtime.deferred_reload_reply.lock().unwrap() = Some(reply);
    *runtime.scheduled_respawn.lock().unwrap() = Some(PendingRespawn {
        deadline: Instant::now() + Duration::from_millis(20),
        kind: RespawnKind::Reload,
        daemon_initiated: false,
    });
    let (reply, held) = oneshot::channel();
    let mut child = None;
    assert!(
        handle_supervisor_command(
            SupervisorCommand::ConfigExitHold { reply },
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
    held.await.unwrap().unwrap();
    let reload_retained = matches!(
        runtime
            .scheduled_respawn
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .kind,
        RespawnKind::Reload
    );
    supervisor.start_monitor(spec, runtime, &module, None, commands);
    let answer = tokio::time::timeout(Duration::from_secs(3), result).await;
    module.set_enabled(false).await.unwrap();
    module.stop().await.unwrap();
    assert!(reload_retained, "a stale hold replaced Reload with Spawn");
    // The native fixture never registers, so completion is the normal explicit
    // registration failure. A plain Spawn would leave this caller unanswered.
    assert!(
        matches!(answer, Ok(Ok(Err(SuperviseError::ReloadFailed { ref reason, .. })))
        if reason.contains("new child did not register")),
        "the reload caller must receive its result: {answer:?}"
    );
}

#[tokio::test]
async fn stale_config_exit_hold_preserves_crash_backoff_deadline() {
    let (_scratch, owner, supervisor, _, spec) =
        crate::config_gate::runtime::RuntimeOwner::unactivated_supervisor_fixture_for_test();
    assert!(!owner.has_recorded_admission_hold("a"));
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
    update_snapshot(&module.inner.snapshot, Some("a"), |state| {
        state.state = ModuleState::Restarting;
        state.respawn_pending = true;
    })
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(120);
    *runtime.scheduled_respawn.lock().unwrap() = Some(PendingRespawn {
        deadline,
        kind: RespawnKind::Spawn,
        daemon_initiated: true,
    });
    let before = format!("{:?}", module.status().unwrap());
    let (reply, held) = oneshot::channel();
    assert!(
        handle_supervisor_command(
            SupervisorCommand::ConfigExitHold { reply },
            &mut spec,
            &mut runtime,
            &supervisor.registry,
            &supervisor.process_liveness,
            &module.inner.snapshot,
            &mut None,
            &mut commands,
            &mut VecDeque::new()
        )
        .await
    );
    held.await.unwrap().unwrap();
    let pending = runtime.scheduled_respawn.lock().unwrap().unwrap();
    assert_eq!(
        pending.deadline, deadline,
        "stale holds must not shorten crash backoff"
    );
    assert!(pending.daemon_initiated);
    assert_eq!(format!("{:?}", module.status().unwrap()), before);
}

#[tokio::test]
async fn stale_config_exit_hold_preserves_previous_instance_wait() {
    let (_scratch, owner, supervisor, _, spec) =
        crate::config_gate::runtime::RuntimeOwner::unactivated_supervisor_fixture_for_test();
    assert!(!owner.has_recorded_admission_hold("a"));
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
    update_snapshot(&module.inner.snapshot, Some("a"), |state| {
        state.waiting_for_previous_pid = Some(4242);
        state.waiting_for_previous_detail = Some("previous instance is still alive".into());
    })
    .unwrap();
    let before = format!("{:?}", module.status().unwrap());
    let (reply, held) = oneshot::channel();
    assert!(
        handle_supervisor_command(
            SupervisorCommand::ConfigExitHold { reply },
            &mut spec,
            &mut runtime,
            &supervisor.registry,
            &supervisor.process_liveness,
            &module.inner.snapshot,
            &mut None,
            &mut commands,
            &mut VecDeque::new()
        )
        .await
    );
    held.await.unwrap().unwrap();
    assert_eq!(
        module.status().unwrap().waiting_for_previous_pid,
        Some(4242)
    );
    assert_eq!(format!("{:?}", module.status().unwrap()), before);
    assert!(runtime.scheduled_respawn.lock().unwrap().is_none());
}

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
    std::fs::write(
        spec.program
            .parent()
            .unwrap()
            .join("store")
            .join(crate::config_store::STORE_FILE),
        b"tampered",
    )
    .unwrap();
    assert!(owner.check_recovery().unwrap());
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
    assert!(owner.has_recorded_admission_hold("a"));
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
    let retained_child = child.as_ref().unwrap().spawned_at;
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
    assert_eq!(retained_child, child_spawned_at);
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

#[tokio::test]
async fn operator_enable_park_error_does_not_inherit_daemon_retry_attribution() {
    let (_scratch, owner, supervisor, _, spec) =
        crate::config_gate::runtime::RuntimeOwner::unactivated_supervisor_fixture_for_test();
    let RegisteredConfiguredModule {
        spec,
        mut runtime,
        module,
        commands,
    } = supervisor
        .register_configured_with_health(
            spec,
            false,
            Default::default(),
            None,
            Default::default(),
            DEFAULT_REGISTER_TIMEOUT,
        )
        .unwrap();
    runtime.recovery_is_daemon = true;
    std::fs::write(
        spec.program
            .parent()
            .unwrap()
            .join("store")
            .join(crate::config_store::STORE_FILE),
        b"tampered",
    )
    .unwrap();
    assert!(
        owner.check_module_hold("a").is_ok(),
        "the cached projection must admit the operator's preflight"
    );
    owner.poison_config_owner_at_park_for_test();
    supervisor.start_monitor(spec, runtime, &module, None, commands);
    let error = module.set_enabled(true).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("configuration owner is poisoned"),
        "{error}"
    );
    // The reply can precede the monitor's next loop iteration. This command must
    // cross that loop before the snapshot and retry deadline are sampled.
    module.config_exit_hold().await.unwrap();
    let status = module.status().unwrap();
    let deadline = module.retry_deadline_for_test();
    module.stop().await.unwrap();
    assert_eq!(status.state, ModuleState::Failed);
    assert!(!status.process_alive);
    assert_eq!(
        deadline, None,
        "a failed operator enable cannot arm the stale daemon work's retry"
    );
}
