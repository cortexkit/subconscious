use super::*;
use std::collections::BTreeMap;

async fn wait_for(reason: &str, mut ready: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let tick = Instant::now();
    while !ready() {
        assert!(std::time::Instant::now() < deadline, "timed out: {reason}");
        tokio::task::yield_now().await;
    }
    assert_eq!(Instant::now(), tick, "{reason} must not advance time");
}

fn crashing_spec() -> ModuleSpec {
    ModuleSpec {
        module_id: "slow-retry-child".into(),
        program: PathBuf::from("/bin/sh"),
        args: vec!["-c".into(), "exit 17".into()],
        env: vec![],
        reserved: false,
        reserved_prefixes: vec![],
        protocol: ModuleProtocol::None,
        overlap: Default::default(),
    }
}

async fn failed_module() -> SupervisedModule {
    let supervisor = Supervisor::new_for_test(
        Arc::new(Registry::default()),
        RestartPolicy::new(1, Duration::ZERO).with_window(Duration::from_secs(3600)),
    );
    let module = supervisor.spawn(crashing_spec()).unwrap();
    wait_failed(&module, 2).await;
    module
}

async fn wait_failed(module: &SupervisedModule, generation: u64) {
    wait_for(
        &format!("budget failure at generation {generation}"),
        || {
            let status = module.status().unwrap();
            status.state == ModuleState::Failed && status.spawn_generation == generation
        },
    )
    .await;
}

fn assert_no_timer(module: &SupervisedModule) {
    assert!(lock_snapshot(&module.inner.snapshot)
        .unwrap()
        .failed_retry
        .pending
        .is_none());
}

fn sleeping_spec() -> ModuleSpec {
    ModuleSpec {
        program: PathBuf::from("/bin/sleep"),
        args: vec!["600".into()],
        ..crashing_spec()
    }
}

#[tokio::test(start_paused = true)]
async fn revision_daemon_initial_spawn_failure_schedules_recovery() {
    let supervisor =
        Supervisor::new_for_test(Arc::new(Registry::default()), RestartPolicy::default());
    let module = supervisor
        .supervise_configured(
            ModuleSpec {
                program: PathBuf::from("/missing/slow-retry-module"),
                ..sleeping_spec()
            },
            true,
        )
        .unwrap();
    assert_eq!(module.state().unwrap(), ModuleState::Failed);
    assert!(module.next_retry_at_ms().unwrap().is_some());
    module.update_spec_for_test(sleeping_spec()).await.unwrap();
    tokio::time::advance(Duration::from_secs(600)).await;
    wait_for("initial spawn recovery", || {
        module.state().unwrap() == ModuleState::Running
    })
    .await;
    module.stop().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn revision_no_health_checks_reset_after_an_uninterrupted_budget_window() {
    let module = failed_module().await;
    clock_sample(&module, "failed");
    module.update_spec_for_test(sleeping_spec()).await.unwrap();
    clock_sample(&module, "before-600");
    tokio::time::advance(Duration::from_secs(600)).await;
    clock_sample(&module, "after-600");
    wait_for("slow recovery", || {
        module.state().unwrap() == ModuleState::Running
    })
    .await;
    let deadline = running_window_deadline(&module).await;
    clock_sample(&module, "ready-before-window");
    advance_to(deadline - Duration::from_secs(1)).await;
    clock_sample(&module, "one-second-before-deadline");
    tokio::task::yield_now().await;
    assert_eq!(
        lock_snapshot(&module.inner.snapshot)
            .unwrap()
            .failed_retry
            .attempt,
        1
    );
    // Tokio rounds timers to milliseconds. Cross the recorded deadline rather
    // than assuming that observing Running started the uptime window.
    advance_to(deadline + Duration::from_millis(1)).await;
    clock_sample(&module, "after-recorded-deadline");
    wait_for("uptime reset", || {
        lock_snapshot(&module.inner.snapshot)
            .unwrap()
            .failed_retry
            .attempt
            == 0
    })
    .await;
    clock_sample(&module, "reset");
    module.stop().await.unwrap();
}

async fn running_window_deadline(module: &SupervisedModule) -> Instant {
    // Running is published before macOS exec confirmation. Wait until this
    // generation reaches its select, keeping paused time frozen during real I/O.
    wait_for("running supervisor select", || {
        let state = lock_snapshot(&module.inner.snapshot).unwrap();
        state.state == ModuleState::Running
            && state.reported_pid().is_some()
            && state
                .actor_select
                .is_some_and(|select| select.generation == state.spawn_generation)
    })
    .await;
    let state = lock_snapshot(&module.inner.snapshot).unwrap();
    state.failed_retry.running_since.unwrap() + module.inner.restart_policy.window
}

async fn advance_to(deadline: Instant) {
    tokio::time::advance(deadline.saturating_duration_since(Instant::now())).await;
}

fn clock_sample(module: &SupervisedModule, step: &str) {
    if std::env::var_os("SUBC_SLOW_RETRY_CLOCK_TRACE").is_some() {
        let state = lock_snapshot(&module.inner.snapshot).unwrap();
        eprintln!("CLOCK step={step} now={:?} spawned_at_ms={:?} running_since={:?} deadline={:?} attempt={} actor_select={:?}",
            Instant::now(), state.spawned_at_ms, state.failed_retry.running_since,
            state.failed_retry.running_since.map(|since| since + module.inner.restart_policy.window),
            state.failed_retry.attempt, state.actor_select);
    }
}

#[tokio::test(start_paused = true)]
async fn delayed_timer_poll_cannot_postpone_uptime_reset() {
    let running_since = Instant::now();
    let window = Duration::from_secs(30);
    let mut retry = FailedRetry {
        attempt: 1,
        running_since: Some(running_since),
        ..FailedRetry::default()
    };
    let deadline = retry.uptime_reset_at(true, window).unwrap();
    let timer = supervisor_sleep(Some(deadline));
    tokio::pin!(timer);
    eprintln!(
        "CLOCK delayed-poll created now={:?} running_since={running_since:?} deadline={deadline:?}",
        Instant::now()
    );
    // Make the first poll unambiguously overdue even with millisecond timers.
    tokio::time::advance(window + Duration::from_millis(1)).await;
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
    let observed = std::future::Future::poll(timer.as_mut(), &mut context);
    eprintln!("CLOCK delayed-poll polled now={:?} running_since={running_since:?} deadline={deadline:?} poll={observed:?}", Instant::now());
    assert!(
        observed.is_ready(),
        "an elapsed uptime deadline must be ready on its first poll"
    );
    retry.uptime_reset_at(true, window);
    assert_eq!(retry.attempt, 0);
}

#[tokio::test(start_paused = true)]
async fn operator_start_clears_the_slow_retry_timer_and_starts_now() {
    let module = failed_module().await;
    module.update_spec_for_test(sleeping_spec()).await.unwrap();
    assert!(module.set_enabled(true).await.unwrap());
    let _ = running_window_deadline(&module).await;
    let status = module.status().unwrap();
    assert_eq!(status.state, ModuleState::Running);
    assert_eq!(module.next_retry_at_ms().unwrap(), None);
    assert_no_timer(&module);
    assert_eq!(status.restart_count, 0);
    tokio::time::advance(Duration::from_secs(600)).await;
    tokio::task::yield_now().await;
    assert_eq!(
        module.status().unwrap().spawn_generation,
        status.spawn_generation
    );
    module.stop().await.unwrap();
    assert_no_timer(&module);
}

#[tokio::test(start_paused = true)]
async fn operator_stop_cancels_a_failed_modules_retry() {
    let module = failed_module().await;
    module.stop().await.unwrap();
    assert_eq!(module.next_retry_at_ms().unwrap(), None);
    assert_no_timer(&module);
    tokio::time::advance(Duration::from_secs(7200)).await;
    tokio::task::yield_now().await;
    assert_eq!(module.state().unwrap(), ModuleState::Stopped);
    assert_eq!(module.status().unwrap().spawn_generation, 2);
}

#[tokio::test(start_paused = true)]
async fn disable_cancels_a_failed_modules_retry() {
    let module = failed_module().await;
    assert!(module.set_enabled(false).await.unwrap());
    assert_no_timer(&module);
    assert_eq!(module.next_retry_at_ms().unwrap(), None);
    tokio::time::advance(Duration::from_secs(7200)).await;
    tokio::task::yield_now().await;
    assert_eq!(module.state().unwrap(), ModuleState::Disabled);
    assert_eq!(module.status().unwrap().spawn_generation, 2);
}

#[tokio::test(start_paused = true)]
async fn removal_cancels_a_failed_modules_retry() {
    let module = failed_module().await;
    module.retire_requested(None).await.unwrap();
    assert_no_timer(&module);
    assert_eq!(module.next_retry_at_ms().unwrap(), None);
    tokio::time::advance(Duration::from_secs(7200)).await;
    tokio::task::yield_now().await;
    assert_eq!(module.state().unwrap(), ModuleState::Stopped);
    assert_eq!(module.status().unwrap().spawn_generation, 2);
}

#[tokio::test(start_paused = true)]
async fn an_operator_stopped_module_never_gets_a_slow_retry() {
    let supervisor = Supervisor::new_for_test(
        Arc::new(Registry::default()),
        RestartPolicy::new(0, Duration::ZERO),
    );
    let module = supervisor.spawn(sleeping_spec()).unwrap();
    module.stop().await.unwrap();
    assert_no_timer(&module);
    tokio::time::advance(Duration::from_secs(7200)).await;
    tokio::task::yield_now().await;
    assert_eq!(module.next_retry_at_ms().unwrap(), None);
    assert_eq!(module.status().unwrap().spawn_generation, 1);
    assert_eq!(module.state().unwrap(), ModuleState::Stopped);
}

#[tokio::test(start_paused = true)]
async fn rescan_keeps_the_existing_retry_deadline() {
    let module = failed_module().await;
    let deadline = lock_snapshot(&module.inner.snapshot)
        .unwrap()
        .failed_retry
        .pending
        .unwrap()
        .deadline;
    tokio::time::advance(Duration::from_secs(60)).await;
    module.update_spec_for_test(sleeping_spec()).await.unwrap();
    assert_eq!(
        lock_snapshot(&module.inner.snapshot)
            .unwrap()
            .failed_retry
            .pending
            .unwrap()
            .deadline,
        deadline
    );
    tokio::time::advance(Duration::from_secs(540)).await;
    wait_for("rescan retry", || {
        module.state().unwrap() == ModuleState::Running
    })
    .await;
    module.stop().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn healthy_checks_for_the_restart_policy_window_reset_the_schedule() {
    let supervisor = Supervisor::new_for_test(
        Arc::new(Registry::default()),
        RestartPolicy::new(0, Duration::ZERO).with_window(Duration::from_secs(90)),
    );
    let mut runtime = supervisor.runtime_config();
    runtime.health.on_failing = HealthAction::Report;
    let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::starting()));
    let spec = sleeping_spec();
    let mut child = Some(spawn_and_mark_running(&spec, &runtime, &snapshot).unwrap());
    health_restart_child(
        &spec,
        &runtime,
        &supervisor.registry,
        &supervisor.process_liveness,
        &snapshot,
        &mut child,
        SupervisorHealthStatus::Failing,
        None,
        1,
    )
    .await
    .unwrap();
    assert!(child.is_none());
    assert_eq!(
        lock_snapshot(&snapshot)
            .unwrap()
            .failed_retry
            .pending
            .unwrap()
            .delay,
        Duration::from_secs(600)
    );
    lock_snapshot(&snapshot).unwrap().failed_retry.pending = None;
    set_child_enabled(
        &spec,
        &runtime,
        &supervisor.registry,
        &supervisor.process_liveness,
        &snapshot,
        &mut child,
        true,
        &runtime.failure_context(false),
    )
    .await
    .unwrap();
    for (status, advance) in [
        (HealthStatus::Ok, 89),
        (HealthStatus::Failing, 1),
        (HealthStatus::Ok, 89),
        (HealthStatus::Ok, 1),
    ] {
        handle_health_report(
            &spec,
            &runtime,
            &supervisor.registry,
            &supervisor.process_liveness,
            &snapshot,
            &mut child,
            HealthReport {
                status,
                detail: None,
                metrics: None,
            },
            1,
        )
        .await;
        assert_eq!(lock_snapshot(&snapshot).unwrap().failed_retry.attempt, 1);
        tokio::time::advance(Duration::from_secs(advance)).await;
    }
    handle_health_report(
        &spec,
        &runtime,
        &supervisor.registry,
        &supervisor.process_liveness,
        &snapshot,
        &mut child,
        HealthReport {
            status: HealthStatus::Ok,
            detail: None,
            metrics: None,
        },
        1,
    )
    .await;
    assert_eq!(lock_snapshot(&snapshot).unwrap().failed_retry.attempt, 0);
    health_restart_child(
        &spec,
        &runtime,
        &supervisor.registry,
        &supervisor.process_liveness,
        &snapshot,
        &mut child,
        SupervisorHealthStatus::Failing,
        None,
        1,
    )
    .await
    .unwrap();
    assert_eq!(
        lock_snapshot(&snapshot)
            .unwrap()
            .failed_retry
            .pending
            .unwrap()
            .delay,
        Duration::from_secs(600)
    );
}

#[tokio::test(start_paused = true)]
async fn operator_failure_never_arms_a_slow_retry() {
    let supervisor = Supervisor::new_for_test(
        Arc::new(Registry::default()),
        RestartPolicy::new(0, Duration::ZERO),
    );
    let spec = sleeping_spec();
    let runtime = supervisor.runtime_config();
    let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::starting()));
    fail_snapshot(
        &snapshot,
        Some(&spec.module_id),
        None,
        &FailureContext::operator(),
    );
    let (module, commands) = supervisor.register_module(&spec, &runtime, snapshot);
    supervisor.start_monitor(spec, runtime, &module, None, commands);
    tokio::time::advance(Duration::from_secs(7200)).await;
    tokio::task::yield_now().await;
    assert_no_timer(&module);
    assert_eq!(module.next_retry_at_ms().unwrap(), None);
    assert_eq!(module.status().unwrap().spawn_generation, 0);
}

fn missing_spec() -> ModuleSpec {
    ModuleSpec {
        program: PathBuf::from("/missing/slow-retry-module"),
        ..sleeping_spec()
    }
}

fn parked_actor(
    initial: ModuleState,
    pending: Option<bool>,
    previous: bool,
    registered: bool,
    daemon: bool,
) -> SupervisedModule {
    let supervisor =
        Supervisor::new_for_test(Arc::new(Registry::default()), RestartPolicy::default());
    let spec = missing_spec();
    let mut runtime = supervisor.runtime_config();
    runtime.recovery_is_daemon = daemon;
    let mut state = SupervisorSnapshot::new(initial, true);
    if let Some(daemon_initiated) = pending {
        state.respawn_pending = true;
        *runtime.scheduled_respawn.lock().unwrap() = Some(PendingRespawn {
            deadline: Instant::now(),
            kind: RespawnKind::Spawn,
            daemon_initiated,
        });
    }
    if previous {
        state.waiting_for_previous_pid = Some(41);
    }
    if registered {
        let manifest =
            subc_protocol::manifest::ModuleManifest::builder(&spec.module_id, "1.0.0").build();
        supervisor
            .registry
            .register_with_control_ops(
                manifest,
                subc_protocol::PROTOCOL_VERSION,
                crate::registry::ConnectionId::new(1),
                vec![],
            )
            .unwrap();
    }
    let snapshot = Arc::new(Mutex::new(state));
    let (module, commands) = supervisor.register_module(&spec, &runtime, snapshot);
    supervisor.start_monitor(spec, runtime, &module, None, commands);
    module
}

async fn advance_to_failure(module: &SupervisedModule) {
    for _ in 0..20 {
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        if module.state().unwrap() == ModuleState::Failed {
            return;
        }
    }
    panic!("daemon operation did not publish Failed");
}

fn assert_first_retry(module: &SupervisedModule) {
    let state = lock_snapshot(&module.inner.snapshot).unwrap();
    let retry = state
        .failed_retry
        .pending
        .expect("daemon failure schedules recovery");
    assert_eq!(retry.attempt, 1);
    assert_eq!(retry.delay, Duration::from_secs(600));
}

#[tokio::test(start_paused = true)]
async fn daemon_respawn_creation_failure_schedules_recovery() {
    let module = parked_actor(ModuleState::Restarting, Some(true), false, false, true);
    advance_to_failure(&module).await;
    assert_first_retry(&module);
    module.stop().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn daemon_respawn_registration_failure_schedules_once() {
    let module = parked_actor(ModuleState::Restarting, Some(true), false, true, true);
    advance_to_failure(&module).await;
    assert_first_retry(&module);
    assert_eq!(module.status().unwrap().spawn_generation, 0);
    drop(module);
}

#[tokio::test(start_paused = true)]
async fn daemon_previous_instance_start_failure_schedules_recovery() {
    let module = parked_actor(ModuleState::Starting, None, true, false, false);
    advance_to_failure(&module).await;
    assert_first_retry(&module);
    module.stop().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn daemon_stranded_operation_schedules_recovery() {
    let module = parked_actor(ModuleState::Starting, None, false, false, true);
    advance_to_failure(&module).await;
    assert_first_retry(&module);
    module.stop().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn operator_deferred_spawn_failure_does_not_schedule_recovery() {
    let module = parked_actor(ModuleState::Restarting, Some(false), false, false, false);
    advance_to_failure(&module).await;
    assert_no_timer(&module);
}

#[tokio::test(start_paused = true)]
async fn operator_stranded_operation_does_not_schedule_recovery() {
    let module = parked_actor(ModuleState::Starting, None, false, false, false);
    advance_to_failure(&module).await;
    assert_no_timer(&module);
}

#[tokio::test(start_paused = true)]
async fn daemon_wait_failure_schedules_recovery() {
    let supervisor =
        Supervisor::new_for_test(Arc::new(Registry::default()), RestartPolicy::default());
    let runtime = supervisor.runtime_config();
    let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::starting()));
    fail_after_wait_error(
        "wait-failed",
        &snapshot,
        &runtime.terminal_ring,
        &runtime.spawn_events,
        "wait failed".into(),
        &runtime.failure_context(true),
    );
    let state = lock_snapshot(&snapshot).unwrap();
    assert_eq!(state.state, ModuleState::Failed);
    assert_eq!(
        state.failed_retry.pending.unwrap().delay,
        Duration::from_secs(600)
    );
}

#[tokio::test(start_paused = true)]
async fn daemon_failure_during_shutdown_does_not_schedule_recovery() {
    let supervisor =
        Supervisor::new_for_test(Arc::new(Registry::default()), RestartPolicy::default());
    let runtime = supervisor.runtime_config();
    let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::starting()));
    let context = runtime.failure_context(true);
    supervisor.begin_daemon_shutdown();
    fail_snapshot(&snapshot, Some("shutdown-failed"), None, &context);
    assert_eq!(lock_snapshot(&snapshot).unwrap().state, ModuleState::Failed);
    assert!(lock_snapshot(&snapshot)
        .unwrap()
        .failed_retry
        .pending
        .is_none());
}

#[tokio::test(start_paused = true)]
async fn repeated_failure_publication_keeps_one_retry_and_deadline() {
    let supervisor =
        Supervisor::new_for_test(Arc::new(Registry::default()), RestartPolicy::default());
    let runtime = supervisor.runtime_config();
    let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::starting()));
    let context = runtime.failure_context(true);
    fail_snapshot(&snapshot, Some("same-failure"), None, &context);
    let first = lock_snapshot(&snapshot).unwrap().failed_retry.pending;
    tokio::time::advance(Duration::from_secs(1)).await;
    fail_snapshot(&snapshot, Some("same-failure"), None, &context);
    assert_eq!(
        lock_snapshot(&snapshot).unwrap().failed_retry.pending,
        first
    );
}

#[tokio::test(start_paused = true)]
async fn daemon_registration_release_failure_schedules_recovery() {
    let supervisor =
        Supervisor::new_for_test(Arc::new(Registry::default()), RestartPolicy::default());
    let runtime = supervisor.runtime_config();
    let spec = sleeping_spec();
    let manifest =
        subc_protocol::manifest::ModuleManifest::builder(&spec.module_id, "1.0.0").build();
    supervisor
        .registry
        .register_with_control_ops(
            manifest,
            subc_protocol::PROTOCOL_VERSION,
            crate::registry::ConnectionId::new(1),
            vec![],
        )
        .unwrap();
    let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::starting()));
    assert!(release_dead_registration(
        &supervisor.registry,
        None,
        &snapshot,
        &spec.module_id,
        &runtime.failure_context(true)
    )
    .await
    .is_err());
    let state = lock_snapshot(&snapshot).unwrap();
    assert_eq!(state.state, ModuleState::Failed);
    assert_eq!(
        state.failed_retry.pending.unwrap().delay,
        Duration::from_secs(600)
    );
}

#[tokio::test(start_paused = true)]
async fn failed_slow_retry_spawn_schedules_the_next_step() {
    let supervisor =
        Supervisor::new_for_test(Arc::new(Registry::default()), RestartPolicy::default());
    let module = supervisor
        .supervise_configured(missing_spec(), true)
        .unwrap();
    assert_first_retry(&module);
    tokio::time::advance(Duration::from_secs(600)).await;
    wait_for("next slow retry after spawn refusal", || {
        lock_snapshot(&module.inner.snapshot)
            .unwrap()
            .failed_retry
            .pending
            .is_some_and(|retry| retry.attempt == 2)
    })
    .await;
    assert_eq!(
        lock_snapshot(&module.inner.snapshot)
            .unwrap()
            .failed_retry
            .pending
            .unwrap()
            .delay,
        Duration::from_secs(1200)
    );
    module.stop().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn operator_configured_start_failure_does_not_schedule_recovery() {
    let supervisor =
        Supervisor::new_for_test(Arc::new(Registry::default()), RestartPolicy::default());
    let module = supervisor
        .supervise_configured_requested(
            missing_spec(),
            true,
            HealthConfig::default(),
            None,
            RestartPolicy::default(),
            DEFAULT_REGISTER_TIMEOUT,
            Some(Principal::Direct),
        )
        .unwrap();
    assert_eq!(module.state().unwrap(), ModuleState::Failed);
    assert_no_timer(&module);
}

#[tokio::test(start_paused = true)]
async fn configured_health_does_not_reset_from_uptime_alone() {
    let module = failed_module().await;
    let health = HealthConfig {
        http: Some("http://127.0.0.1:1/healthz".into()),
        cadence: Duration::from_secs(10_000),
        ..HealthConfig::default()
    };
    module
        .update_configuration(sleeping_spec(), health, None)
        .await
        .unwrap();
    tokio::time::advance(Duration::from_secs(600)).await;
    wait_for("HTTP child recovery", || {
        module.state().unwrap() == ModuleState::Running
    })
    .await;
    let deadline = running_window_deadline(&module).await;
    advance_to(deadline + Duration::from_millis(1)).await;
    tokio::task::yield_now().await;
    assert_eq!(
        lock_snapshot(&module.inner.snapshot)
            .unwrap()
            .failed_retry
            .attempt,
        1
    );
    module.stop().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn exhausted_reload_spawn_budget_also_schedules_a_slow_retry() {
    let supervisor = Supervisor::new_for_test(
        Arc::new(Registry::default()),
        RestartPolicy::new(0, Duration::ZERO),
    );
    let runtime = supervisor.runtime_config();
    let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::starting()));
    let mut child = None;
    assert!(handle_reload_spawn_failure(
        &sleeping_spec(),
        &runtime,
        &supervisor.process_liveness,
        &snapshot,
        &mut child,
        "spawn refused".into()
    )
    .await
    .is_err());
    let state = lock_snapshot(&snapshot).unwrap();
    assert_eq!(state.state, ModuleState::Failed);
    assert_eq!(
        state.failed_retry.pending.unwrap().delay,
        Duration::from_secs(600)
    );
}

#[tokio::test(start_paused = true)]
async fn retry_estimate_counts_monotonic_time_not_wall_time() {
    let module = failed_module().await;
    let remaining = module.next_retry_at_ms().unwrap().unwrap() - unix_ms_now();
    assert!((599_000..=600_000).contains(&remaining));
    tokio::time::advance(Duration::from_secs(60)).await;
    let remaining = module.next_retry_at_ms().unwrap().unwrap() - unix_ms_now();
    assert!((539_000..=540_000).contains(&remaining));
    module.stop().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn slow_release_does_not_postpone_the_failure_deadline() {
    let failed_at = Instant::now();
    let mut retry = FailedRetry {
        failed_at: Some(failed_at),
        ..FailedRetry::default()
    };
    tokio::time::advance(Duration::from_secs(5)).await;
    retry.schedule("slow-release");
    assert_eq!(
        retry.pending.unwrap().deadline,
        failed_at + Duration::from_secs(600)
    );
}

#[tokio::test(start_paused = true)]
async fn shutdown_exit_never_schedules_a_retry() {
    let supervisor = Supervisor::new_for_test(
        Arc::new(Registry::default()),
        RestartPolicy::new(0, Duration::ZERO),
    );
    let runtime = supervisor.runtime_config();
    let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::starting()));
    supervisor.begin_daemon_shutdown();
    on_child_exit(
        &crashing_spec(),
        runtime.restart_policy,
        &supervisor.registry,
        &snapshot,
        &runtime.terminal_ring,
        &runtime.spawn_events,
        &runtime.child_roster,
        ExitReport {
            kind: ExitKind::Crash,
            code: Some(17),
            signal: None,
            at_ms: 1,
        },
        true,
    )
    .await;
    let state = lock_snapshot(&snapshot).unwrap();
    assert_eq!(state.state, ModuleState::Stopped);
    assert!(state.failed_retry.pending.is_none());
}

#[tokio::test(start_paused = true)]
async fn slow_retry_logs_name_module_attempt_and_delay_once_per_transition() {
    use tracing_subscriber::prelude::*;
    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<BTreeMap<String, String>>>>);
    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Capture {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            struct Fields(BTreeMap<String, String>);
            impl tracing::field::Visit for Fields {
                fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn fmt::Debug) {
                    self.0.insert(field.name().into(), format!("{value:?}"));
                }
            }
            if *event.metadata().level() == tracing::Level::INFO {
                let mut fields = Fields(BTreeMap::new());
                event.record(&mut fields);
                self.0.lock().unwrap().push(fields.0);
            }
        }
    }
    let capture = Capture::default();
    // Keep a second subscriber alive so logging in unsubscribed parallel tests
    // cannot make tracing suppress the structured capture's events.
    let (_text, _interest_guard) = crate::router::test_log::log_capture(tracing::Level::INFO);
    let subscriber = tracing_subscriber::registry().with(capture.clone());
    let _guard = tracing::subscriber::set_default(subscriber);
    let module = failed_module().await;
    tokio::time::advance(Duration::from_secs(600)).await;
    wait_failed(&module, 4).await;
    let events = capture.0.lock().unwrap().clone();
    for (message, expected) in [
        (
            "failed module retry scheduled",
            vec![("1", "600"), ("2", "1200")],
        ),
        ("starting failed module retry", vec![("1", "600")]),
    ] {
        let matched = events
            .iter()
            .filter(|e| e.get("message").is_some_and(|m| m == message))
            .collect::<Vec<_>>();
        assert_eq!(matched.len(), expected.len(), "{message}: {events:?}");
        for (event, (attempt, delay)) in matched.into_iter().zip(expected) {
            assert_eq!(event["attempt"], attempt);
            assert_eq!(event["delay_secs"], delay);
            assert!(event["module_id"].contains("slow-retry-child"));
        }
    }
    module.stop().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn exhausted_budget_retries_after_ten_then_twenty_minutes_with_a_fresh_budget() {
    let module = failed_module().await;
    for (delay, generation) in [(600, 4), (1200, 6), (2400, 8), (3600, 10), (3600, 12)] {
        tokio::time::advance(Duration::from_secs(delay - 1)).await;
        tokio::task::yield_now().await;
        assert_eq!(module.status().unwrap().spawn_generation, generation - 2);
        assert_eq!(module.state().unwrap(), ModuleState::Failed);
        tokio::time::advance(Duration::from_secs(1)).await;
        wait_failed(&module, generation).await;
    }
    module.stop().await.unwrap();
}
