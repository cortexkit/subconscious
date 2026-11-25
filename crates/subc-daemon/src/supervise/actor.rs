use super::*;

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct ActorSelectCheckpoint {
    pub(super) generation: u64,
    pub(super) turn: u64,
    pub(super) registered_connection: Option<ConnectionId>,
    pub(super) next_probe_at: Option<Instant>,
    pub(super) wake_after: Option<Duration>,
}

#[derive(Debug)]
pub(super) enum SupervisorCommand {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    Upgrade {
        key: Arc<()>,
        request: live_upgrade::ActorRequest,
    },
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    HeldAdoptionResolved { restart: bool },
    ApprovedConfiguration {
        approved: crate::config_gate::runtime::ApprovedDaemonConfiguration,
        reply: oneshot::Sender<Result<(), SuperviseError>>,
    },
    ConfigPolicyProbe {
        connection: crate::ConnectionId,
        generation: u64,
        attempt: u64,
    },
    ConfigApplyRestart {
        generation: u64,
        supersedes_swap: bool,
    },
    ConfigExitHold {
        reply: oneshot::Sender<Result<(), SuperviseError>>,
    },
    ConfigDeliveryFailed {
        connection: crate::ConnectionId,
        hold: bool,
    },
    Drain {
        requester: Option<Principal>,
        reply: oneshot::Sender<Result<(), SuperviseError>>,
    },
    Retire {
        requester: Option<Principal>,
        reply: oneshot::Sender<Result<(), SuperviseError>>,
    },
    Restart {
        requester: Option<Principal>,
        /// Operator override for this one restart's drain budget, in ms. `None`
        /// uses the module's configured/default budget; `Some(0)` cuts
        /// immediately (wedge bounce: a stuck request never settles, so
        /// waiting only delays recovery).
        drain_timeout_ms: Option<u64>,
        /// The module's `spawn_generation` when the request was received, before
        /// it waited in the command queue. A queued restart whose module has
        /// since spawned a newer process is already satisfied (see the handler).
        received_at_generation: u64,
        /// When the request entered the command queue, so the handler can log
        /// how long it waited behind the loop's other work.
        queued_at: Instant,
        reply: oneshot::Sender<Result<(), SuperviseError>>,
    },
    Reload {
        requester: Option<Principal>,
        reply: oneshot::Sender<Result<(), SuperviseError>>,
    },
    SetEnabled {
        reset_budget: bool,
        requester: Option<Principal>,
        enabled: bool,
        reply: oneshot::Sender<Result<bool, SuperviseError>>,
    },
    UpdateConfiguration {
        spec: ModuleSpec,
        health: HealthConfig,
        /// Per-module drain override from the new config; `None` re-resolves to
        /// the supervisor-wide default.
        drain_timeout_ms: Option<u64>,
        reply: oneshot::Sender<Result<(), SuperviseError>>,
    },
    Swap {
        requester: Option<Principal>,
        /// How long the candidate may take to register and declare itself
        /// ready. `None` uses [`DEFAULT_SWAP_READY_TIMEOUT`].
        ready_timeout: Option<Duration>,
        /// Answered at cutover or failure; the incumbent's drain follows.
        reply: oneshot::Sender<Result<SwapOutcome, SuperviseError>>,
    },
}

pub(super) async fn supervise_loop(
    mut spec: ModuleSpec,
    mut runtime: SupervisorRuntimeConfig,
    registry: Arc<Registry>,
    process_liveness: Arc<SupervisorProcessLiveness>,
    snapshot: SharedSnapshot,
    mut child: Option<SupervisedChild>,
    mut commands: mpsc::Receiver<SupervisorCommand>,
) {
    let mut health_probe = HealthProbeRuntime::default();
    // Registry writes (including embedded callers) notify this module only.
    // Subscribe before the first refresh; watch retains changes that arrive
    // while commands or probes are running. No polling fallback is needed.
    let mut registration_changes = match registry.subscribe_module_changes(&spec.module_id) {
        Ok(changes) => Some(changes),
        Err(err) => {
            // A poisoned registry cannot accept further writes, so it cannot
            // recover via a timer. Keep exit and command handling alive.
            warn!(module_id = %spec.module_id, error = %err, "health prober could not subscribe to registry");
            None
        }
    };
    // All restart backoffs run here, including health and operator requests.
    // While one is pending the loop serves commands, so disable or drain can
    // cancel the replacement without spawning a process just to stop it.
    let mut pending_respawn: Option<PendingRespawn> = None;
    // Commands a swap handed back to run next (see `swap::SwapEnd`). Served
    // before anything else so a stop that interrupted a swap runs at once.
    let mut requeued: VecDeque<SupervisorCommand> = VecDeque::new();
    let upgrades = runtime
        .supervisor_handle
        .as_ref()
        .map(|handle| handle.upgrade_state.clone())
        .unwrap_or_default();
    let mut upgrade_events = upgrades.subscribe();
    let mut previous_instance_recheck = tokio::time::interval_at(
        Instant::now() + Duration::from_secs(5),
        Duration::from_secs(5),
    );
    previous_instance_recheck.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    'supervision: loop {
        let active_upgrade = upgrade_events.borrow_and_update().clone();
        if let Some(key) = active_upgrade {
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            if let Some(index) = requeued
                .iter()
                .position(|command| matches!(command, SupervisorCommand::Upgrade { .. }))
            {
                let command = requeued.remove(index).expect("located upgrade command");
                live_upgrade::paused_command(
                    command,
                    &key,
                    &spec,
                    &runtime,
                    &registry,
                    &process_liveness,
                    &snapshot,
                    &mut child,
                    &mut requeued,
                )
                .await;
                continue;
            }
            tokio::select! {
                biased;
                _ = upgrade_events.changed() => {},
                command = commands.recv() => {
                    let Some(command) = command else { return; };
                    live_upgrade::paused_command(command, &key, &spec, &runtime, &registry, &process_liveness, &snapshot, &mut child, &mut requeued).await;
                }
            }
            continue;
        }
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        if live_upgrade::drive_pending_child(
            &spec,
            &runtime,
            &registry,
            &process_liveness,
            &snapshot,
            &mut child,
            &mut commands,
            &mut requeued,
        )
        .await
        {
            continue;
        }
        #[cfg(test)]
        {
            lock_snapshot(&snapshot).unwrap().actor_select = None;
        }
        #[cfg(target_os = "macos")]
        if let Some(active) = child.as_mut() {
            active.confirm_privacy_exec().await;
        }
        if let Some(scheduled) = runtime
            .scheduled_respawn
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
        {
            runtime.recovery_is_daemon = scheduled.daemon_initiated;
            pending_respawn = Some(scheduled);
        }
        if pending_respawn.is_some() && (child.is_some() || !respawn_still_pending(&snapshot)) {
            pending_respawn = None;
            cancel_deferred_reload(
                &runtime,
                &spec.module_id,
                "respawn cancelled by a supervisor command",
            );
        }
        if child.is_none()
            && pending_respawn.is_none()
            && lock_snapshot(&snapshot).is_ok_and(|state| state.waiting_for_previous_pid.is_none())
        {
            cancel_deferred_reload(
                &runtime,
                &spec.module_id,
                "respawn cancelled before a replacement was spawned",
            );
            let mut stranded = false;
            let _ = update_snapshot(&snapshot, Some(&spec.module_id), |state| {
                state.respawn_pending = false;
                state.coalesced_restart_pending = false;
                if matches!(
                    state.state,
                    ModuleState::Restarting
                        | ModuleState::Starting
                        | ModuleState::Draining
                        | ModuleState::Unresponsive
                ) {
                    error!(module_id = %spec.module_id, state = ?state.state,
                        "supervision operation ended without a child or pending respawn; marking failed so start can retry");
                    stranded = true;
                }
            });
            if stranded {
                fail_snapshot(
                    &snapshot,
                    Some(&spec.module_id),
                    None,
                    &runtime.failure_context(runtime.recovery_is_daemon),
                );
            }
        }
        if child.is_none()
            && (runtime.child_roster.is_closed()
                || pending_respawn.is_none()
                || !lock_snapshot(&snapshot)
                    .is_ok_and(|state| state.enabled && state.state == ModuleState::Restarting))
        {
            containment::terminate(&runtime.child_roster.containment).await;
        }
        if let Some(command) = requeued.pop_front() {
            if !handle_supervisor_command(
                command,
                &mut spec,
                &mut runtime,
                &registry,
                &process_liveness,
                &snapshot,
                &mut child,
                &mut commands,
                &mut requeued,
            )
            .await
            {
                containment::terminate(&runtime.child_roster.containment).await;
                return;
            }
            if child.is_some() || !respawn_still_pending(&snapshot) {
                pending_respawn = None;
                let _ = update_snapshot(&snapshot, Some(&spec.module_id), |state| {
                    state.respawn_pending = false
                });
            }
            continue;
        }
        if child.is_some() {
            runtime.recovery_is_daemon = true;
            #[cfg(test)]
            {
                lock_snapshot(&snapshot).unwrap().actor_turns += 1;
            }
            health_probe.refresh_registration(&spec, &runtime, &registry, &snapshot);
            let active = child.as_mut().expect("child checked above");
            if active.registered_since_spawn() {
                if let Some(delivery) = &runtime.config_delivery {
                    delivery.queue_registered(
                        &spec.module_id,
                        runtime.forwarding.as_ref(),
                        &registry,
                        active,
                        &upgrades,
                    );
                }
            }
            if runtime.child_roster.is_closed() || !active.awaiting_registration() {
                active.registration_deadline = None;
            }
            let registration_deadline = active.registration_deadline;
            let no_checks = runtime.health.http.is_none()
                && (running_protocol(&spec, &snapshot) == ModuleProtocol::None
                    || (health_probe.registered_connection.is_some() && !health_probe.advertised));
            let uptime_reset = lock_snapshot(&snapshot).ok().and_then(|mut state| {
                state
                    .failed_retry
                    .uptime_reset_at(no_checks, runtime.restart_policy.window)
            });
            let wake_at = match (health_probe.wake_at(), uptime_reset) {
                (probe, Some(reset)) => Some(probe.map_or(reset, |probe| probe.min(reset))),
                (probe, None) => probe,
            };
            let wake_at = match (wake_at, registration_deadline) {
                (Some(wake), Some(register)) => Some(wake.min(register)),
                (wake, register) => wake.or(register),
            };
            #[cfg(test)]
            let wake_after = wake_at.map(|at| at.saturating_duration_since(Instant::now()));
            let probe_sleep = supervisor_sleep(wake_at);
            tokio::pin!(probe_sleep);
            // A `protocol: "none"` child (a plain process that never registers
            // over the subc wire, such as nats-server) with no HTTP health
            // check has nothing to probe, so it waits only for its exit or a
            // supervisor command. A rescan that adds an HTTP check is a command.
            let wire_child = running_protocol(&spec, &snapshot) == ModuleProtocol::Subc;
            #[cfg(test)]
            {
                let registration_pending = wire_child
                    && registration_changes
                        .as_ref()
                        .is_some_and(|changes| changes.has_changed().unwrap());
                let mut state = lock_snapshot(&snapshot).unwrap();
                state.actor_select = (!registration_pending).then_some(ActorSelectCheckpoint {
                    generation: state.spawn_generation,
                    turn: state.actor_turns,
                    registered_connection: health_probe.registered_connection,
                    next_probe_at: health_probe.next_probe_at,
                    wake_after,
                });
            }
            let registration = active
                .awaiting_registration()
                .then(|| active.registration_spawn.clone())
                .flatten();
            #[cfg(all(test, target_os = "linux"))]
            let registration_before_exit = !runtime.child_roster.admission_event_for_test(
                &spec.module_id,
                "observe_registration_before_exit",
                Some(active.pid),
            ) && registration.is_some();
            #[cfg(not(all(test, target_os = "linux")))]
            let registration_before_exit = false;
            #[cfg(all(test, target_os = "linux"))]
            let registration_child_is_running = |child: &mut SupervisedChild| {
                // Model the interval after SIGKILL is accepted but before the
                // child becomes waitable, without delaying the actual kill.
                registration_before_exit || registration_child_is_running(child)
            };
            let registration_failed = async {
                match &registration {
                    Some(spawn) => spawn.wait_registration_failure().await,
                    None => std::future::pending().await,
                }
            };
            let active_child = child.as_mut().expect("child checked above");
            tokio::select! {
                _ = upgrade_events.changed() => continue,
                _ = registration_failed => {
                    let Some(_work) = upgrades.background_work() else { continue; };
                    if !active_child.registered_since_spawn() && registration_child_is_running(active_child) {
                        active_child.registration_deadline = None;
                        active_child.spawn_failure = Some("registration_channel_closed".into());
                        if let Err(error) = stop_unregistered_child(active_child).await {
                            fail_after_wait_error(&spec.module_id, &snapshot, &runtime.terminal_ring,
                                &runtime.spawn_events, format!("registration_channel_closed: {error}"),
                                &runtime.failure_context(true));
                            child = None;
                        }
                    }
                    continue;
                }
                wait_result = active_child.wait(), if !registration_before_exit => {
                    if upgrade_events.has_changed().unwrap_or(false) { continue; }
                    if let Ok(mut state) = lock_snapshot(&snapshot) {
                        state.failed_retry.uptime_reset_at(no_checks, runtime.restart_policy.window);
                    }
                    #[cfg(test)]
                    { lock_snapshot(&snapshot).unwrap().actor_select = None; }
                    // Every arm below that gives up on the CHILD must keep the
                    // supervision task itself alive (child = None, loop
                    // continues into command-serving mode). Returning here
                    // closes the command channel, which makes the module
                    // permanently unrestartable in-band: a clean child exit
                    // of an enabled module once wedged the fleet this way
                    // ('supervisor command channel is closed') and required a
                    // full daemon restart to recover.
                    let exit_report = match wait_result {
                        Ok(status) => classify_reaped_child_exit(&snapshot, active_child, &status),
                        Err(err) => {
                            #[cfg(any(target_os = "linux", target_os = "macos"))]
                            let adopted = active_child.child.verified_adoptee().is_some();
                            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
                            let adopted = false;
                            if !adopted { active_child.drain_stderr(&spec.module_id).await; }
                            fail_after_wait_error(
                                &spec.module_id,
                                &snapshot,
                                &runtime.terminal_ring,
                                &runtime.spawn_events,
                                format!("failed to wait for supervised module: {err}"),
                            &runtime.failure_context(true),
                            );
                            untrack_if_registration_released(
                                &process_liveness,
                                &registry,
                                &spec.module_id,
                                &snapshot,
                            );
                            error!(module_id = %spec.module_id, error = %err, "failed to wait for supervised module");
                            child = None;
                            continue;
                        }
                    };
                    active_child.drain_stderr(&spec.module_id).await;

                    let delivery=runtime.config_delivery.clone();
                    let exited_spawn=active_child.spawned_at;
                    let spawn=active_child.config_delivery.clone();
                    let refusal=active_child.spawn_failure.as_deref().filter(|detail|detail.starts_with("program_refused:") && !active_child.live_program_binding.as_ref().is_some_and(|binding|binding.is_refused())).zip(active_child.admission_base.clone()).map(|(detail,base)|(base,subc_config::raw_digest(detail.as_bytes())));
                    let module_id=spec.module_id.clone();
                    // Publication may own the store mutex for local durable I/O.
                    // Keep commands selectable until the child's exit is recorded;
                    // no respawn disposition is chosen before that observation.
                    let observed=async move {
                        if let Some(delivery)=delivery {
                            delivery.0.on_blocking(move |owner| {
                                if let Some(spawn)=spawn { owner.delivery_child_exited(&spawn)?; }
                                if let Some((base,identity))=refusal {owner.record_program_refusal(&module_id,&base,identity)?;}
                                Ok(())
                            }).await
                        } else {Ok(())}
                    };
                    tokio::pin!(observed);
                    loop {
                        tokio::select! {
                            result=&mut observed=>{
                                if let Err(error)=result {warn!(module_id=%spec.module_id,%error,"configuration exit observation failed");}
                                break;
                            }
                             command=commands.recv()=>{
                                 let Some(command)=command else {containment::terminate(&runtime.child_roster.containment).await;return;};
                                 #[cfg(any(target_os = "linux", target_os = "macos"))]
                                 let Some(command) = live_upgrade::defer_exit_publication_command(command, &mut requeued) else { continue; };
                                 let deferred=||SuperviseError::ConfigDeliveryDeferred {module_id:spec.module_id.clone()};
                                 let command=match command {
                                    SupervisorCommand::Restart {reply,..}|SupervisorCommand::Reload {reply,..}=>{let _=reply.send(Err(deferred()));continue;}
                                    SupervisorCommand::Swap {reply,..}=>{let _=reply.send(Err(deferred()));continue;}
                                    command @ (SupervisorCommand::ConfigApplyRestart {..}|SupervisorCommand::ConfigPolicyProbe {..}|SupervisorCommand::ConfigDeliveryFailed {..})=>{requeued.push_back(command);continue;}
                                    command=>command,
                                };
                                if !handle_supervisor_command(command,&mut spec,&mut runtime,&registry,&process_liveness,&snapshot,&mut child,&mut commands,&mut requeued).await {
                                    containment::terminate(&runtime.child_roster.containment).await;return;
                                }
                                if child.as_ref().is_none_or(|current|current.spawned_at!=exited_spawn) {continue 'supervision;}
                            }
                        }
                    }
                    let Some(_work) = upgrades.background_work() else { continue 'supervision; };
                    let active_child=child.as_mut().expect("exit observation retained its child");
                    // An unavailable image can race the program's own exit.
                    // Only the admission kill is budget-free; another exit must
                    // retain its ordinary crash/clean-exit accounting.
                    let admission_kill = admission_exit::is_admission_kill(active_child.admission_kill_sent, &exit_report);
                    let deferred_spawn=admission_kill && active_child.spawn_failure.as_deref().is_some_and(|detail|detail.starts_with("config_delivery_deferred:"));
                    let refused_program = active_child.live_program_binding.as_ref().is_some_and(|binding| binding.is_refused());
                    let exit_held = !deferred_spawn && (refused_program || runtime.config_delivery.as_ref().is_some_and(|delivery| delivery.0.exit_is_held(&spec.module_id)));
                    let parked = exit_held && (!refused_program || active_child.admission_base.is_some())
                        && try_park_config_attempt(&spec, &runtime, &snapshot, &runtime.failure_context(true),
                            if refused_program { active_child.admission_base.as_ref() } else { None })
                            .unwrap_or_else(|error| { warn!(module_id=%spec.module_id, %error, "exit hold could not be recorded; using ordinary exit accounting"); false });
                    let next = if deferred_spawn {
                        record_terminal_with_detail(&spec.module_id,&runtime.terminal_ring,&runtime.spawn_events,&exit_report,TerminalDisposition::Restarting,active_child.exit_cleanup_detail(active_child.spawn_failure.clone()));
                        let _=update_snapshot(&snapshot,Some(&spec.module_id),|state| {
                            state.state=ModuleState::Restarting;
                            clear_current_process_facts(state);
                            state.last_exit=Some(exit_report.clone());
                        });
                        NextAction::Restart {schedule:None}
                    } else if parked {
                        record_terminal_with_detail(&spec.module_id, &runtime.terminal_ring, &runtime.spawn_events, &exit_report, TerminalDisposition::Stopped, active_child.exit_cleanup_detail(active_child.spawn_failure.clone().or_else(|| Some("config_gated".into()))));
                        NextAction::Stop { registration_released: registry.get_module(&spec.module_id).ok().flatten().is_none() }
                    } else { on_child_exit_with_detail(
                        &spec,
                        runtime.restart_policy,
                        &registry,
                        &snapshot,
                        &runtime.terminal_ring,
                        &runtime.spawn_events,
                        &runtime.child_roster,
                        exit_report,
                        true,
                        active_child.exit_cleanup_detail(None),
                    ).await };
                    runtime.start_requester = None;
                    runtime.pending_activity_restart = false;
                    // The exit is recorded, so a daemon shutdown may stop
                    // waiting for this child (see `SupervisedChild::wait`).
                    let carry = matches!(next, NextAction::Restart { .. })
                        && active_child.spawn_failure.is_none()
                        && !runtime.child_roster.is_closed();
                    if let Err(error) = active_child.finish_exit(carry).await {
                        containment::terminate(&runtime.child_roster.containment).await;
                        fail_after_wait_error(&spec.module_id, &snapshot, &runtime.terminal_ring,
                            &runtime.spawn_events, format!("failed to reap classified exit: {error}"),
                            &runtime.failure_context(true));
                        child = None;
                        continue;
                    }
                    match next {
                        NextAction::Stop { registration_released } => {
                            if registration_released {
                                process_liveness.untrack_if_current(&spec.module_id, &snapshot);
                            }
                            child = None;
                        }
                        NextAction::Restart { schedule } => {
                            let delay = if deferred_spawn {CONFIG_ADMISSION_RETRY_DELAY} else {schedule.map_or(
                                runtime.restart_policy.delay_for_restart(0),
                                |schedule| schedule.delay,
                            )};
                            if let Some(schedule) = schedule {
                                log_crash_respawn(&spec.module_id, schedule);
                            }
                            // The exited child is fully recorded at this point,
                            // so release it and count the backoff down in the
                            // command-serving branch below rather than sleeping
                            // here: commands cannot be received from inside this
                            // select arm, and an operator disable or drain that
                            // arrives during the backoff must cancel the pending
                            // respawn instead of waiting for it to spawn first.
                            child = None;
                            pending_respawn = Some(PendingRespawn { deadline: Instant::now() + delay, kind: RespawnKind::after_exit(schedule), daemon_initiated: true, approval: None });
                             let _ = update_snapshot(&snapshot, Some(&spec.module_id), |state| state.respawn_pending = true);
                        }
                    }
                }
                command = commands.recv() => {
                    #[cfg(test)]
                    { lock_snapshot(&snapshot).unwrap().actor_select = None; }
                    let Some(command) = command else {
                        containment::terminate(&runtime.child_roster.containment).await;
                        return;
                    };
                    if !handle_supervisor_command(
                        command,
                        &mut spec,
                        &mut runtime,
                        &registry,
                        &process_liveness,
                        &snapshot,
                        &mut child,
                        &mut commands,
                        &mut requeued,
                    ).await {
                        containment::terminate(&runtime.child_roster.containment).await;
                        return;
                    }
                }
                _ = async {
                    match registration_changes.as_mut() {
                        Some(changes) => { let _ = changes.changed().await; }
                        None => std::future::pending().await,
                    }
                }, if wire_child => {
                    #[cfg(test)]
                    { lock_snapshot(&snapshot).unwrap().actor_select = None; }
                }
                _ = &mut probe_sleep => {
                    let Some(_work) = upgrades.background_work() else { continue; };
                    #[cfg(test)]
                    { lock_snapshot(&snapshot).unwrap().actor_select = None; }
                    if registration_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                        // Recheck after the wake: HELLO, exit or shutdown may
                        // have become ready in the same executor tick.
                        active_child.registration_deadline = None;
                        if !runtime.child_roster.is_closed()
                            && !active_child.registered_since_spawn()
                            && registration_child_is_running(active_child)
                        {
                            warn!(module_id = %spec.module_id, pid = active_child.pid,
                                wait_secs = runtime.register_timeout.as_secs(),
                                "supervised module registration_timeout: first HELLO deadline expired; stopping child");
                            active_child.spawn_failure = Some("registration_timeout".into());
                            if let Err(error) = stop_unregistered_child(active_child).await {
                                error!(module_id = %spec.module_id, %error, "failed to stop unregistered child");
                                fail_after_wait_error(&spec.module_id, &snapshot, &runtime.terminal_ring,
                                    &runtime.spawn_events, format!("registration_timeout: {error}"),
                                    &runtime.failure_context(true));
                                child = None;
                            }
                            // The ordinary exit handler records the child's exit,
                            // spends crash budget, and schedules recovery.
                            continue;
                        }
                    }
                    if health_probe.due() {
                        run_health_probe_cycle(
                            &spec,
                            &runtime,
                            &registry,
                            &process_liveness,
                            &snapshot,
                            &mut child,
                        ).await;
                        if child.is_none() {
                            runtime.start_requester = None;
                            runtime.pending_activity_restart = false;
                        }
                        if child.is_some() {
                            health_probe.schedule_next(&spec, runtime.health.cadence);
                        }
                    }
                }
            }
        } else if let Some(pending) = pending_respawn.clone() {
            tokio::select! {
                _ = upgrade_events.changed() => continue,
                _ = runtime.child_roster.closed() => {
                    pending_respawn = None;
                    let _ = update_snapshot(&snapshot, Some(&spec.module_id), |state| {
                        state.state = ModuleState::Stopped;
                        state.respawn_pending = false;
                    });
                    containment::terminate(&runtime.child_roster.containment).await;
                }
                _ = async {
                    sleep_until(pending.deadline).await;
                    #[cfg(target_os = "linux")]
                    ordinary_cgroup::await_clear(&runtime.child_roster, &spec.module_id).await;
                    if let Some(delivery) = &runtime.config_delivery {
                        delivery.unfenced(&spec.module_id).await;
                    }
                } => {
                    let Some(_work) = upgrades.background_work() else { continue; };
                    pending_respawn = None;
                    let failure = pending.failure_context(&runtime);
                let _ = update_snapshot(&snapshot, Some(&spec.module_id), |state| state.respawn_pending = false);
                    // A command handled below while the backoff elapsed may
                    // have stopped the module; never respawn past an operator's
                    // disable or drain.
                    if !respawn_still_pending(&snapshot) {
                        continue;
                    }
                    // The daemon began shutting down during the backoff: the
                    // spawn would be refused anyway, and refusing it here
                    // leaves the module stopped instead of reporting a
                    // failed restart.
                    if runtime.child_roster.is_closed() {
                        let _ = update_snapshot(&snapshot, Some(&spec.module_id), |state| {
                            state.state = ModuleState::Stopped;
                        });
                        debug!(module_id = %spec.module_id, "crash respawn cancelled by daemon shutdown");
                        continue;
                    }
                    if let Err(err) = release_dead_registration(
                        &registry,
                        runtime.forwarding.as_deref(),
                        &snapshot,
                        &spec.module_id,
                        &failure,
                    ).await {
                        record_approved_attempt_failure(&spec, &snapshot, &failure, &err);
                        error!(module_id = %spec.module_id, error = %err, "registration did not release before restart");
                        continue;
                    }

                    if matches!(pending.kind, RespawnKind::Reload) {
                        let reply = runtime.deferred_reload_reply.lock().unwrap_or_else(|p| p.into_inner()).take();
                        let result = finish_reload_child(&spec, &runtime, &registry, &process_liveness, &snapshot, &mut child).await;
                        if matches!(result, Err(SuperviseError::ConfigDeliveryDeferred { .. })) {
                            *runtime.deferred_reload_reply.lock().unwrap_or_else(|p| p.into_inner()) = reply;
                            continue;
                        }
                        if result.is_ok() && runtime.pending_activity_restart {
                            runtime.record_activity_transition("subc.module_restarted", &spec.module_id, runtime.start_requester.clone());
                        }
                        runtime.pending_activity_restart = false;
                        if let Some(reply) = reply { let _ = reply.send(result); }
                        continue;
                    }
                    process_liveness.track(spec.module_id.clone(), Arc::clone(&snapshot));
                    match admit_running_child_async(&spec, &runtime, &snapshot, pending.kind).await {
                        Ok(next_child) => {
                            child = Some(next_child);
                            if runtime.pending_activity_restart {
                                runtime.record_activity_transition("subc.module_restarted", &spec.module_id, runtime.start_requester.clone());
                                runtime.pending_activity_restart = false;
                            }
                            debug!(module_id = %spec.module_id, "supervised module restarted after crash");
                        }
                        Err(SuperviseError::ConfigDeliveryDeferred { .. }) => {
                            pending_respawn = Some(PendingRespawn { deadline:Instant::now()+CONFIG_ADMISSION_RETRY_DELAY, ..pending });
                            let _ = update_snapshot(&snapshot, Some(&spec.module_id), |state| state.respawn_pending = true);
                        }
                        Err(SuperviseError::ConfigHeld { .. }) => {
                            let _ = hold_config_respawn(&spec, &runtime, &snapshot, pending.kind, &failure);
                            runtime.pending_activity_restart = false;
                            process_liveness.untrack_if_current(&spec.module_id, &snapshot);
                        }
                        Err(err) => {
                            fail_snapshot(&snapshot, Some(&spec.module_id), None, &failure);
                            record_approved_attempt_failure(&spec, &snapshot, &failure, &err);
                            process_liveness.untrack_if_current(&spec.module_id, &snapshot);
                            error!(module_id = %spec.module_id, error = %err, "failed to restart supervised module");
                        }
                    }
                }
                command = commands.recv() => {
                    let Some(command) = command else {
                        containment::terminate(&runtime.child_roster.containment).await;
                        return;
                    };
                    if !handle_supervisor_command(
                        command,
                        &mut spec,
                        &mut runtime,
                        &registry,
                        &process_liveness,
                        &snapshot,
                        &mut child,
                        &mut commands,
                        &mut requeued,
                    ).await {
                        containment::terminate(&runtime.child_roster.containment).await;
                        return;
                    }
                    // Reconcile the pending respawn with what the command did:
                    // a start may already have spawned a fresh child,
                    // while a disable or drain moved the snapshot out of the
                    // state the respawn was counting down from.
                    if child.is_some() || !respawn_still_pending(&snapshot) {
                        pending_respawn = None;
                let _ = update_snapshot(&snapshot, Some(&spec.module_id), |state| state.respawn_pending = false);
                    }
                }
            }
        } else {
            let slow_retry = lock_snapshot(&snapshot).ok().and_then(|state| {
                (state.enabled && state.state == ModuleState::Failed)
                    .then_some(state.failed_retry.pending)
                    .flatten()
            });
            let command = if lock_snapshot(&snapshot)
                .is_ok_and(|state| state.waiting_for_previous_pid.is_some())
            {
                tokio::select! {
                    _ = upgrade_events.changed() => continue,
                    command = commands.recv() => command,
                    _ = previous_instance_recheck.tick() => {
                        let Some(_work) = upgrades.background_work() else { continue; };
                        if runtime.child_roster.previous_instance_pid(&spec.module_id).is_none() {
                            let enabled = lock_snapshot(&snapshot).is_ok_and(|state| state.enabled);
                            if enabled && !runtime.child_roster.is_closed() {
                                process_liveness.track(spec.module_id.clone(), Arc::clone(&snapshot));
                                match spawn_and_mark_running_async(&spec, &runtime, &snapshot).await {
                                    Ok(next_child) => child = Some(next_child),
                                    Err(SuperviseError::ConfigDeliveryDeferred { .. }) => {
                                        let _ = defer_config_spawn(&spec, &runtime, &snapshot, RespawnKind::Spawn, &runtime.failure_context(true));
                                    }
                                    Err(SuperviseError::ConfigHeld { .. }) => {
                                        let _ = hold_config_spawn(&spec, &runtime, &snapshot, &runtime.failure_context(true));
                                    }
                                    Err(error) => {
                                        fail_snapshot(&snapshot, Some(&spec.module_id), None, &runtime.failure_context(true));
                                        error!(module_id = %spec.module_id, %error, "failed to start module after its previous instance exited");
                                    }
                                }
                            } else {
                                let _ = update_snapshot(&snapshot, Some(&spec.module_id), |state| {
                                    state.waiting_for_previous_pid = None;
                                    state.waiting_for_previous_detail = None;
                                });
                            }
                        }
                        continue;
                    }
                }
            } else {
                tokio::select! {
                    biased;
                    _ = upgrade_events.changed() => continue,
                    command = commands.recv() => command,
                    _ = async {
                        match slow_retry {
                            Some(retry) => sleep_until(retry.deadline).await,
                            None => std::future::pending().await,
                        }
                    } => {
                        let Some(_work) = upgrades.background_work() else { continue; };
                        let retry = slow_retry.expect("only a scheduled slow retry can wake");
                        let _ = update_snapshot(&snapshot, Some(&spec.module_id), |state| {
                            state.failed_retry.pending = None;
                        });
                        if runtime.child_roster.is_closed() {
                            continue;
                        }
                        info!(module_id = %spec.module_id, attempt = retry.attempt,
                            delay_secs = retry.delay.as_secs(), "starting failed module retry");
                        if let Err(error) = set_child_enabled(
                            &spec, &runtime, &registry, &process_liveness, &snapshot, &mut child, true,
                            &runtime.failure_context(true),
                        ).await {
                            error!(module_id = %spec.module_id, %error, "failed module retry could not start");
                        }
                        continue;
                    }
                }
            };
            let Some(command) = command else {
                containment::terminate(&runtime.child_roster.containment).await;
                return;
            };
            if !handle_supervisor_command(
                command,
                &mut spec,
                &mut runtime,
                &registry,
                &process_liveness,
                &snapshot,
                &mut child,
                &mut commands,
                &mut requeued,
            )
            .await
            {
                containment::terminate(&runtime.child_roster.containment).await;
                return;
            }
        }
    }
}

// Preserve the deadline until the future is polled. Starting a relative sleep
// after the executor yields would restart part of the countdown.
pub(super) async fn supervisor_sleep(wake_at: Option<Instant>) {
    match wake_at {
        Some(deadline) => sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

pub(super) enum NextAction {
    Stop {
        registration_released: bool,
    },
    Restart {
        schedule: Option<CrashRestartSchedule>,
    },
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_supervisor_command(
    command: SupervisorCommand,
    spec: &mut ModuleSpec,
    runtime: &mut SupervisorRuntimeConfig,
    registry: &Arc<Registry>,
    process_liveness: &SupervisorProcessLiveness,
    snapshot: &SharedSnapshot,
    child: &mut Option<SupervisedChild>,
    commands: &mut mpsc::Receiver<SupervisorCommand>,
    requeued: &mut VecDeque<SupervisorCommand>,
) -> bool {
    let _work = if let Some(handle) = &runtime.supervisor_handle {
        let Some(work) = handle.upgrade_state.background_work() else {
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            if matches!(command, SupervisorCommand::Upgrade { .. }) {
                requeued.push_back(command);
                return true;
            }
            live_upgrade::refuse_and_defer(command, requeued);
            return true;
        };
        Some(work)
    } else {
        None
    };
    if let SupervisorCommand::SetEnabled {
        enabled: true,
        reset_budget: false,
        reply,
        ..
    } = command
    {
        let parked = lock_snapshot(snapshot).ok().and_then(|state| {
            (state.config_hold_parked
                && state.enabled
                && !state.process_alive
                && child.is_none()
                && state.state == ModuleState::Stopped)
                .then(|| {
                    (
                        state.config_hold_daemon_initiated,
                        state.config_hold_approval.clone(),
                    )
                })
        });
        // Clearing a configuration-delivery block must not restart a child that
        // exited on its own or exhausted its crash budget. Resume only an actor
        // stopped by a configuration hold; otherwise leave its slow retry alone.
        let result = if let Some((daemon_initiated, approval)) = parked {
            runtime.recovery_is_daemon = daemon_initiated;
            let failure = FailureContext {
                approval,
                ..runtime.failure_context(daemon_initiated)
            };
            let result = set_child_enabled_with_budget_policy(
                spec,
                runtime,
                registry,
                process_liveness,
                snapshot,
                child,
                true,
                &failure,
                false,
            )
            .await;
            if let Err(error) = &result {
                record_approved_attempt_failure(spec, snapshot, &failure, error);
            }
            result
        } else {
            Ok(false)
        };
        let _ = reply.send(result);
        return true;
    }
    let blocked = runtime
        .config_delivery
        .as_ref()
        .and_then(|delivery| delivery.0.check_module_hold(&spec.module_id).err())
        .filter(|error| error.code != "config_delivery_deferred")
        .map(|error| {
            config_delivery::gate_hold_error(spec, error, runtime.config_delivery.as_ref())
        })
        .or_else(|| recovery_block_error(spec, &runtime.child_roster))
        .or_else(|| {
            runtime
                .child_roster
                .previous_instance(&spec.module_id)
                .map(|(pid, reason)| previous_instance_error(spec, pid, reason))
        });
    let command = if let Some(error) = blocked {
        match command {
            SupervisorCommand::Restart { reply, .. } | SupervisorCommand::Reload { reply, .. } => {
                let _ = reply.send(Err(error));
                return true;
            }
            SupervisorCommand::Swap { reply, .. } => {
                let _ = reply.send(Err(error));
                return true;
            }
            SupervisorCommand::ConfigApplyRestart { .. } => return true,
            SupervisorCommand::SetEnabled {
                enabled: true,
                requester,
                reply,
                ..
            } => {
                if matches!(error, SuperviseError::ConfigHeld { .. })
                    && lock_snapshot(snapshot).is_ok_and(|state| !state.enabled)
                    && runtime
                        .config_delivery
                        .as_ref()
                        .is_some_and(|delivery| delivery.0.can_stage_unstamp_enable())
                {
                    // Stage only the pending unstamp's desired state. A stamped
                    // unapproved actor stays refused; admission release starts
                    // an unstamped actor only after reconciliation completes.
                    let mut changed = false;
                    let result = update_snapshot(snapshot, Some(&spec.module_id), |state| {
                        changed = !state.enabled;
                        state.enabled = true;
                        if child.is_none() && state.waiting_for_previous_pid.is_none() {
                            state.state = ModuleState::Starting;
                            state.failed_retry.cancel();
                        }
                    })
                    .and_then(|_| {
                        if child.is_none()
                            && lock_snapshot(snapshot)?.waiting_for_previous_pid.is_none()
                        {
                            hold_config_spawn(
                                spec,
                                runtime,
                                snapshot,
                                &runtime.failure_context(false),
                            )?;
                        }
                        Ok(changed)
                    });
                    if result.is_ok() && changed {
                        runtime.start_requester = requester;
                    }
                    let _ = reply.send(result);
                    return true;
                }
                let _ = reply.send(Err(error));
                return true;
            }
            other => other,
        }
    } else {
        command
    };
    // A refused start or restart leaves the Failed actor's existing recovery
    // deadline intact. Cancel only after the command passes admission.
    if matches!(
        command,
        SupervisorCommand::Drain { .. }
            | SupervisorCommand::Retire { .. }
            | SupervisorCommand::SetEnabled { .. }
            | SupervisorCommand::Restart { .. }
    ) {
        let _ = update_snapshot(snapshot, Some(&spec.module_id), |state| {
            state.failed_retry.cancel();
        });
    }
    match command {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        SupervisorCommand::Upgrade { request, .. } => {
            request.refuse_inactive();
            true
        }
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        SupervisorCommand::HeldAdoptionResolved { restart } => {
            live_upgrade::held_resolved(spec, runtime, snapshot, process_liveness, child, restart)
                .await;
            true
        }
        SupervisorCommand::ApprovedConfiguration { approved, reply } => {
            let result = async {
                let next = approved.module_entry(&spec.module_id).map_err(|error| {
                    SuperviseError::InvalidSpec { reason: error.to_string() }
                })?;
                let retry_approved_failure = {
                    let mut state = lock_snapshot(snapshot)?;
                    if let Some((generation, detail)) = &state.approved_start_failure {
                        if *generation == approved.identity() && next.enabled {
                            return Err(SuperviseError::Spawn {
                                program: spec.program.clone(),
                                source: std::io::Error::other(detail.clone()),
                                cgroup_path: None,
                            });
                        }
                    }
                    let retry = state.approved_start_failure.is_some();
                    state.approved_start_failure = None;
                    retry
                };
                stage_supervisor_configuration(
                    spec, runtime, snapshot, next.module_spec(), next.health,
                    next.drain_timeout_ms,
                );
                runtime.restart_policy = next.restart;
                runtime.register_timeout = next.register_timeout;
                if let Some(active) = child.as_mut()
                    .filter(|child| child.registration_deadline.is_some())
                {
                    active.registration_deadline = Some(active.spawned_at + next.register_timeout);
                }
                let enabled = lock_snapshot(snapshot)?.enabled;
                // An approval retries a changed failed entry or its own earlier
                // failed start, not an unrelated crash-budget failure. For an
                // unchanged entry, reconciled means the approved configuration
                // is effective; it does not claim the process has recovered.
                // A terminal gate stop is different: a new approval owns its
                // recovery even when the daemon entry itself did not change.
                let retry_gate_stopped = lock_snapshot(snapshot)?
                    .needs_approved_gate_restart(&approved.identity());
                let retry_failed = enabled && child.is_none()
                    && ((lock_snapshot(snapshot)?.state == ModuleState::Failed
                        && (retry_approved_failure || approved.changes_entry(&spec.module_id)))
                        || retry_gate_stopped);
                if retry_failed {
                    // This approval owns a new start, so the failed attempt's
                    // daemon retry must not survive either result of that start.
                    update_snapshot(snapshot, Some(&spec.module_id), |state| {
                        state.failed_retry.cancel();
                        if retry_gate_stopped {
                            state.stop_reason = None;
                            state.config_exit_failed_approval = None;
                        }
                    })?;
                }
                if enabled != next.enabled || retry_failed {
                    let failure = FailureContext {
                        approval: next.enabled.then(|| approved.identity()),
                        ..runtime.failure_context(false)
                    };
                    let held = next.enabled && runtime.config_delivery.as_ref()
                        .is_some_and(|delivery| delivery.0.has_recorded_admission_hold(&spec.module_id)
                            || delivery.0.check_module_hold(&spec.module_id).is_err_and(|error| error.code == "config_delivery_deferred"));
                    if held {
                        // Preserve module-delivery failure/recovery holds. The
                        // admission release starts the actor when its own hold ends.
                        reset_restart_count(snapshot, &spec.module_id)?;
                        update_snapshot(snapshot, Some(&spec.module_id), |state| {
                            state.enabled = true;
                            if child.is_none() && state.waiting_for_previous_pid.is_none() {
                                state.state = ModuleState::Starting;
                            }
                        })?;
                        if child.is_none() && lock_snapshot(snapshot)?.waiting_for_previous_pid.is_none() {
                            hold_config_spawn(spec, runtime, snapshot, &failure)?;
                        }
                    } else {
                        match set_child_enabled_with_budget_policy(
                            spec, runtime, registry, process_liveness, snapshot, child,
                            next.enabled, &failure, true,
                        ).await {
                            Ok(_) => {},
                            Err(SuperviseError::ConfigHeld { .. }) if next.enabled => {},
                            Err(error) => return Err(error),
                        }
                        if !next.enabled {
                            containment::terminate(&runtime.child_roster.containment).await;
                        }
                    }
                }
                if (child.as_ref().is_some_and(|child| child.registration_spawn.is_some() && child.admission_base.is_none())
                        || (child.is_none() && lock_snapshot(snapshot)?.state == ModuleState::Restarting
                            && runtime.config_delivery.as_ref().is_some_and(|delivery| delivery.0.stamp_channel_cutover_pending())))
                    && runtime.config_delivery.as_ref().is_some_and(|delivery| delivery.0.module_is_gated(&spec.module_id))
                {
                    let failure = FailureContext {
                        approval: Some(approved.identity()),
                        ..runtime.failure_context(false)
                    };
                    // A stamp cannot adopt an unverified legacy process, even
                    // when its approved replacement no longer speaks subc.
                    // Stopping that process spends no crash restart budget.
                    if child.is_some() {
                        restart_child_with_budget_policy(spec, runtime, registry,
                            process_liveness, snapshot, child, runtime.drain_timeout, false).await?;
                    }
                    // The stamp is already committed. A deferred or held launch
                    // belongs to the normal retry/hold loop, not a failed apply.
                    update_snapshot(snapshot, Some(&spec.module_id), |state| state.respawn_pending = false)?;
                    match spawn_and_mark_running_async(spec, runtime, snapshot).await {
                        Ok(replacement) => *child = Some(replacement),
                        Err(SuperviseError::ConfigDeliveryDeferred { .. }) => {
                            defer_config_spawn(spec, runtime, snapshot, RespawnKind::Spawn, &failure)?;
                        }
                        Err(SuperviseError::ConfigHeld { .. }) => hold_config_spawn(spec, runtime, snapshot, &failure)?,
                        Err(error) => {
                            fail_snapshot(snapshot, Some(&spec.module_id), None, &failure);
                            return Err(error);
                        }
                    }
                }
                info!(module_id=%spec.module_id, config_generation=approved.generation(), "approved daemon entry reconciled");
                Ok(())
            }
            .await;
            if let Err(error) = &result {
                let _ = update_snapshot(snapshot, Some(&spec.module_id), |state| {
                    if state.enabled
                        && state.state == ModuleState::Failed
                        && state
                            .approved_start_failure
                            .as_ref()
                            .is_none_or(|(generation, _)| *generation != approved.identity())
                    {
                        state.approved_start_failure =
                            Some((approved.identity(), error.to_string()));
                    }
                });
            }
            let _ = reply.send(result);
            true
        }
        SupervisorCommand::ConfigPolicyProbe {
            connection,
            generation,
            attempt,
        } => {
            let Some(delivery) = &runtime.config_delivery else {
                return true;
            };
            let Ok(gate) = delivery.0.module_gate(&spec.module_id) else {
                return true;
            };
            let guard = gate.lock().await;
            let pending = delivery
                .0
                .on_blocking(move |owner| {
                    owner.policy_probe_pending(connection, generation, attempt)
                })
                .await
                .unwrap_or(false)
                && child.is_some()
                && registry
                    .get_module(&spec.module_id)
                    .ok()
                    .flatten()
                    .is_some_and(|registration| registration.connection_id == connection);
            drop(guard);
            if pending {
                run_health_probe_cycle(spec, runtime, registry, process_liveness, snapshot, child)
                    .await;
            }
            true
        }
        SupervisorCommand::ConfigApplyRestart {
            generation,
            supersedes_swap,
        } => {
            let Some(owner) = runtime
                .config_delivery
                .as_ref()
                .map(|delivery| delivery.0.clone())
            else {
                return true;
            };
            let Ok(gate) = owner.module_gate(&spec.module_id) else {
                return true;
            };
            let _guard = gate.lock().await;
            let spawn = child
                .as_ref()
                .and_then(|child| child.config_delivery.clone());
            let module = spec.module_id.clone();
            let current = owner
                .on_blocking(move |owner| {
                    Ok((supersedes_swap
                        || owner
                            .pending_config_restarts()?
                            .contains(&(module.clone(), generation)))
                        && owner.requires_fresher_spawn(&module, spawn.as_ref())?
                            == Some(generation))
                })
                .await
                .unwrap_or(false);
            if !current
                || !lock_snapshot(snapshot).is_ok_and(|state| {
                    state.enabled && !state.respawn_pending && state.process_alive
                })
            {
                return true;
            }
            drop(_guard);
            info!(module_id=%spec.module_id, config_generation=generation, "approved configuration restart preserves the crash budget");
            if let Err(error) = restart_child_with_budget_policy(
                spec,
                runtime,
                registry,
                process_liveness,
                snapshot,
                child,
                runtime.drain_timeout,
                false,
            )
            .await
            {
                warn!(module_id=%spec.module_id, %error, "approved configuration restart could not be scheduled");
            }
            true
        }
        SupervisorCommand::ConfigExitHold { reply } => {
            // A notification owns no start, backoff, reload reply or previous-
            // instance wait. Only an idle, registered initial start has no work
            // to preserve; its initialized startup context can be parked here.
            let idle_start = child.is_none()
                && lock_snapshot(snapshot).is_ok_and(|state| {
                    state.enabled
                        && state.state == ModuleState::Starting
                        && !state.process_alive
                        && !state.respawn_pending
                        && state.waiting_for_previous_pid.is_none()
                        && state.waiting_for_previous_detail.is_none()
                        && state.failed_retry.pending.is_none()
                })
                && runtime
                    .scheduled_respawn
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .is_none()
                && runtime
                    .deferred_reload_reply
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .is_none();
            let result = if idle_start {
                try_park_config_attempt(
                    spec,
                    runtime,
                    snapshot,
                    &runtime.failure_context(runtime.recovery_is_daemon),
                    None,
                )
                .map(|_| ())
            } else {
                Ok(())
            };
            let _ = reply.send(result);
            true
        }
        SupervisorCommand::ConfigDeliveryFailed { connection, hold } => {
            if registry
                .get_module(&spec.module_id)
                .ok()
                .flatten()
                .is_none_or(|registration| registration.connection_id != connection)
            {
                return true;
            }
            runtime.recovery_is_daemon = true;
            // With a recoverable generation, delivery-failure recovery spends
            // one crash-budget entry: the module rejected its configuration.
            // Approved configuration restarts spend none.
            if hold {
                let notice = begin_forwarding_drain_if_configured(
                    spec,
                    runtime,
                    registry,
                    snapshot,
                    Some(true),
                    RouteCloseReason::Disable,
                )
                .await;
                if let Ok(notice) = notice {
                    let drained = drain_optional_child(
                        &spec.module_id,
                        spec.protocol,
                        notice,
                        registry,
                        runtime.forwarding.as_deref(),
                        snapshot,
                        &runtime.terminal_ring,
                        &runtime.spawn_events,
                        child,
                        runtime.drain_timeout,
                        ModuleState::Failed,
                        Some(true),
                        &runtime.failure_context(true),
                    )
                    .await;
                    if drained.is_ok() {
                        match try_park_config_attempt(
                            spec,
                            runtime,
                            snapshot,
                            &runtime.failure_context(true),
                            None,
                        ) {
                            Ok(true) => {}
                            Ok(false) => {
                                if let Err(error) = budgeted_restart_child(
                                    spec,
                                    runtime,
                                    registry,
                                    process_liveness,
                                    snapshot,
                                    child,
                                    SupervisorHealthStatus::Unknown,
                                    Some("configuration delivery failed after its hold ended"),
                                    unix_ms_now(),
                                    false,
                                )
                                .await
                                {
                                    warn!(module_id=%spec.module_id, %error, "configuration failure restart could not be scheduled");
                                }
                            }
                            Err(error) => {
                                warn!(module_id=%spec.module_id, %error, "configuration failure could not be parked")
                            }
                        }
                    }
                }
            } else if let Err(error) = budgeted_restart_child(
                spec,
                runtime,
                registry,
                process_liveness,
                snapshot,
                child,
                SupervisorHealthStatus::Unknown,
                Some("configuration delivery failed"),
                unix_ms_now(),
                false,
            )
            .await
            {
                warn!(module_id=%spec.module_id, %error, "configuration failure restart could not be scheduled");
            }
            true
        }
        SupervisorCommand::Drain { requester, reply } => {
            // A plain stop runs no forwarding drain, so nothing reaches the
            // module over its connection before the wait: ask by signal.
            let result = drain_optional_child(
                &spec.module_id,
                spec.protocol,
                StopNotice::NotSent,
                registry,
                runtime.forwarding.as_deref(),
                snapshot,
                &runtime.terminal_ring,
                &runtime.spawn_events,
                child,
                runtime.drain_timeout,
                ModuleState::Stopped,
                None,
                &FailureContext::operator(),
            )
            .await;
            containment::terminate(&runtime.child_roster.containment).await;
            let registration_released = result.is_ok();
            if registration_released {
                runtime.record_activity_transition(
                    "subc.module_stopped",
                    &spec.module_id,
                    requester,
                );
            }
            let _ = reply.send(result);
            if registration_released {
                process_liveness.untrack_if_current(&spec.module_id, snapshot);
            }
            !registration_released
                && runtime
                    .supervisor_handle
                    .as_ref()
                    .is_some_and(|handle| handle.adoption_blocks(&spec.module_id))
        }
        SupervisorCommand::Retire { requester, reply } => {
            let result = async {
                let stop_notice = begin_forwarding_drain_if_configured(
                    spec,
                    runtime,
                    registry,
                    snapshot,
                    None,
                    RouteCloseReason::Disable,
                )
                .await?;
                drain_optional_child(
                    &spec.module_id,
                    spec.protocol,
                    stop_notice,
                    registry,
                    runtime.forwarding.as_deref(),
                    snapshot,
                    &runtime.terminal_ring,
                    &runtime.spawn_events,
                    child,
                    runtime.drain_timeout,
                    ModuleState::Stopped,
                    None,
                    &FailureContext::operator(),
                )
                .await
            }
            .await;
            containment::terminate(&runtime.child_roster.containment).await;
            let registration_released = result.is_ok();
            if registration_released {
                runtime.record_activity_transition(
                    "subc.module_stopped",
                    &spec.module_id,
                    requester,
                );
            }
            let _ = reply.send(result);
            if registration_released {
                process_liveness.untrack_if_current(&spec.module_id, snapshot);
            }
            false
        }
        SupervisorCommand::Restart {
            requester,
            drain_timeout_ms,
            received_at_generation,
            queued_at,
            reply,
        } => {
            // Without this line a restart that waited in the queue (behind a
            // health probe cycle or another command) was invisible: the log
            // showed only the drain timing out, minutes after the operator's call.
            info!(
                module_id = %spec.module_id,
                queued_ms = u64::try_from(queued_at.elapsed().as_millis()).unwrap_or(u64::MAX),
                "restart command dequeued"
            );
            // ACK AT INITIATION, not completion. The blocking form deadlocked any
            // caller whose own request lane rides the module being restarted: the
            // caller's in-flight request keeps the drain from quiescing, the drain
            // keeps the restart from completing, and the completion keeps the reply
            // from releasing the caller — so the drain always timed out and cut the
            // initiator with a GOODBYE, even on a healthy module. Replying once the
            // restart is validated lets a self-lane caller settle, which is exactly
            // what makes the drain succeed. Completion is observable via
            // supervisor.list / module status; a post-ack failure lands the module
            // in a visible terminal state below rather than in a reply nobody can
            // receive.
            let validation = match lock_snapshot(snapshot) {
                Ok(state) if !state.enabled => Err(SuperviseError::Disabled {
                    module_id: spec.module_id.clone(),
                }),
                Ok(_) => Ok(()),
                Err(err) => Err(err),
            };
            let initiated = validation.is_ok();
            let _ = reply.send(validation);
            // A restart asks for a fresh process. Commands run one at a time,
            // so a restart queued behind another restart (two operator calls
            // in quick succession) is dequeued the moment the first one has
            // spawned its replacement -- before that process has sent HELLO.
            // Running it would drain and kill the process the first restart
            // just produced, which is the opposite of what both callers asked
            // for. If a process spawned after this request was received is
            // still supervised, the request is already satisfied. Not when the
            // configuration changed since that spawn: then the newer process
            // predates the spec this restart may exist to apply.
            // Generation publication can lag a child that is already serving
            // while its identity is recorded. Only process creation after request
            // receipt satisfies a restart, not publication after receipt.
            let satisfied_by_generation = if initiated
                && child
                    .as_ref()
                    .is_some_and(|child| child.spawned_at > queued_at)
            {
                lock_snapshot(snapshot).ok().and_then(|state| {
                    (!state.configuration_updated_since_spawn).then_some(state.spawn_generation)
                })
            } else {
                None
            };
            let satisfied_by_pending = initiated
                && child.is_none()
                && lock_snapshot(snapshot).ok().is_some_and(|mut state| {
                    // No child exists yet, so this replacement will necessarily
                    // be created after the queued request, unlike a published
                    // generation whose process may already have been serving.
                    let pending = state.respawn_pending && !state.configuration_updated_since_spawn;
                    if pending {
                        state.coalesced_restart_pending = true;
                    }
                    pending
                });
            if satisfied_by_pending {
                debug!(module_id = %spec.module_id, "restart coalesced into the pending replacement");
            } else if let Some(generation) = satisfied_by_generation {
                info!(
                    module_id = %spec.module_id,
                    received_at_generation,
                    "restart already satisfied by generation {generation}; not restarting again"
                );
            } else if initiated {
                // Precedence: this restart's operator override, else the module's
                // configured budget (already resolved into the runtime).
                let drain_timeout = drain_timeout_ms
                    .map(Duration::from_millis)
                    .unwrap_or(runtime.drain_timeout);
                if let Err(err) = restart_child(
                    spec,
                    runtime,
                    registry,
                    process_liveness,
                    snapshot,
                    child,
                    drain_timeout,
                )
                .await
                {
                    warn!(
                        module_id = %spec.module_id,
                        error = %err,
                        "operator restart failed after initiation ack; module state carries the outcome"
                    );
                    let _ = update_snapshot(snapshot, Some(&spec.module_id), |state| {
                        state.state = ModuleState::Failed;
                        clear_current_process_facts(state);
                    });
                } else {
                    runtime.start_requester = requester;
                    runtime.pending_activity_restart = true;
                }
            }
            true
        }
        SupervisorCommand::Reload { requester, reply } => {
            let result =
                reload_child(spec, runtime, registry, process_liveness, snapshot, child).await;
            if result.is_ok() {
                runtime.start_requester = requester;
                runtime.pending_activity_restart = true;
            }
            if result.is_ok()
                && runtime
                    .scheduled_respawn
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .as_ref()
                    .is_some_and(|pending| matches!(pending.kind, RespawnKind::Reload))
            {
                *runtime
                    .deferred_reload_reply
                    .lock()
                    .unwrap_or_else(|p| p.into_inner()) = Some(reply);
            } else {
                let _ = reply.send(result);
            }
            true
        }
        SupervisorCommand::SetEnabled {
            enabled,
            requester,
            reply,
            reset_budget,
        } => {
            let previous_requester = runtime.start_requester.clone();
            if enabled {
                runtime.start_requester = requester.clone();
            }
            let result = set_child_enabled_with_budget_policy(
                spec,
                runtime,
                registry,
                process_liveness,
                snapshot,
                child,
                enabled,
                &runtime.failure_context(false),
                reset_budget,
            )
            .await;
            if !enabled {
                containment::terminate(&runtime.child_roster.containment).await;
            }
            if matches!(result, Ok(true)) {
                runtime.pending_activity_restart = false;
                if enabled {
                    runtime.start_requester = requester;
                } else {
                    runtime.start_requester = None;
                    runtime.record_activity_transition(
                        "subc.module_disabled",
                        &spec.module_id,
                        requester,
                    );
                }
            } else {
                runtime.start_requester = previous_requester;
            }
            let _ = reply.send(result);
            true
        }
        SupervisorCommand::UpdateConfiguration {
            spec: next_spec,
            health,
            drain_timeout_ms,
            reply,
        } => {
            if runtime
                .config_delivery
                .as_ref()
                .is_some_and(|delivery| delivery.0.file_configuration_is_gated(&spec.module_id))
            {
                let _ = reply.send(Err(SuperviseError::ConfigHeld {
                    module_id: spec.module_id.clone(),
                    code: "config_gated".into(),
                    detail: "live file reconfiguration cannot replace stamped authority".into(),
                }));
                return true;
            }
            stage_supervisor_configuration(
                spec,
                runtime,
                snapshot,
                next_spec,
                health,
                drain_timeout_ms,
            );
            let _ = update_snapshot(snapshot, Some(&spec.module_id), |state| {
                state.configuration_updated_since_spawn = true;
            });
            let _ = reply.send(Ok(()));
            true
        }
        SupervisorCommand::Swap {
            requester,
            ready_timeout,
            reply,
        } => {
            if spec.carry_containment
                || containment::current(&runtime.child_roster.containment).is_some()
            {
                let _ = reply.send(Err(SuperviseError::InvalidSpec {
                    reason: "a carried container cannot overlap a swap candidate".to_owned(),
                }));
                return true;
            }
            let mut swap_runtime = runtime.clone();
            swap_runtime.start_requester = requester;
            let before = lock_snapshot(snapshot)
                .ok()
                .map(|state| state.spawn_generation);
            let end = swap::run_swap(
                spec,
                &swap_runtime,
                registry,
                process_liveness,
                snapshot,
                child,
                commands,
                ready_timeout.unwrap_or(DEFAULT_SWAP_READY_TIMEOUT),
                reply,
            )
            .await;
            if before
                != lock_snapshot(snapshot)
                    .ok()
                    .map(|state| state.spawn_generation)
            {
                runtime.start_requester = swap_runtime.start_requester;
            }
            requeued.extend(end.requeue);
            true
        }
    }
}

fn stage_supervisor_configuration(
    spec: &mut ModuleSpec,
    runtime: &mut SupervisorRuntimeConfig,
    snapshot: &SharedSnapshot,
    next_spec: ModuleSpec,
    health: HealthConfig,
    drain_timeout_ms: Option<u64>,
) {
    if let Some(handle) = &runtime.supervisor_handle {
        handle.apply_identity_configuration(&next_spec);
    }
    let launch_changed = *spec != next_spec;
    *spec = next_spec;
    if launch_changed {
        let _ = update_snapshot(snapshot, Some(&spec.module_id), |state| {
            state.configuration_updated_since_spawn = true
        });
    }
    let health_changed = runtime.health != health;
    runtime.health = health;
    // Reset the cadence and old endpoint's failure streak on a live
    // health-policy change rather than waiting for its old deadline.
    if health_changed {
        let _ = update_snapshot(snapshot, Some(&spec.module_id), |state| {
            state.health = ModuleHealthStatus::default();
            state.last_probe_evidence = None;
        });
    }
    runtime.drain_timeout = drain_timeout_ms
        .map(Duration::from_millis)
        .unwrap_or(runtime.default_drain_timeout);
    *runtime
        .effective_drain_timeout
        .lock()
        .unwrap_or_else(|p| p.into_inner()) = runtime.drain_timeout;
}
