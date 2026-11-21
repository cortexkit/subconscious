use super::*;

#[cfg(test)]
#[derive(Debug, Default)]
pub(super) struct ReloadExitRecordGate {
    pub(super) reached: tokio::sync::Notify,
    pub(super) resume: tokio::sync::Notify,
}

impl SupervisedModule {
    pub async fn restart(&self, drain_timeout_ms: Option<u64>) -> Result<(), SuperviseError> {
        self.restart_requested(drain_timeout_ms, None).await
    }

    pub(crate) async fn restart_requested(
        &self,
        drain_timeout_ms: Option<u64>,
        requester: Option<Principal>,
    ) -> Result<(), SuperviseError> {
        let queued_at = Instant::now();
        let received_at_generation = lock_snapshot(&self.inner.snapshot)?.spawn_generation;
        let (reply_tx, reply_rx) = oneshot::channel();
        self.inner
            .commands
            .send(SupervisorCommand::Restart {
                requester,
                drain_timeout_ms,
                received_at_generation,
                queued_at,
                reply: reply_tx,
            })
            .await
            .map_err(|_| SuperviseError::CommandClosed {
                module_id: self.inner.module_id.clone(),
            })?;
        reply_rx.await.map_err(|_| SuperviseError::CommandClosed {
            module_id: self.inner.module_id.clone(),
        })?
    }
}

impl SupervisedModule {
    pub async fn reload(&self) -> Result<(), SuperviseError> {
        self.reload_requested(None).await
    }

    pub(crate) async fn reload_requested(
        &self,
        requester: Option<Principal>,
    ) -> Result<(), SuperviseError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.inner
            .commands
            .send(SupervisorCommand::Reload {
                requester,
                reply: reply_tx,
            })
            .await
            .map_err(|_| SuperviseError::CommandClosed {
                module_id: self.inner.module_id.clone(),
            })?;
        reply_rx.await.map_err(|_| SuperviseError::CommandClosed {
            module_id: self.inner.module_id.clone(),
        })?
    }

    pub async fn set_enabled(&self, enabled: bool) -> Result<bool, SuperviseError> {
        self.set_enabled_requested(enabled, None).await
    }

    pub(crate) async fn set_enabled_requested(
        &self,
        enabled: bool,
        requester: Option<Principal>,
    ) -> Result<bool, SuperviseError> {
        self.set_enabled_with_budget_policy(enabled, requester, true)
            .await
    }

    pub(crate) async fn release_config_hold(&self) -> Result<bool, SuperviseError> {
        self.set_enabled_with_budget_policy(true, None, false).await
    }
}

impl SupervisedModule {
    async fn set_enabled_with_budget_policy(
        &self,
        enabled: bool,
        requester: Option<Principal>,
        reset_budget: bool,
    ) -> Result<bool, SuperviseError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.inner
            .commands
            .send(SupervisorCommand::SetEnabled {
                requester,
                enabled,
                reset_budget,
                reply: reply_tx,
            })
            .await
            .map_err(|_| SuperviseError::CommandClosed {
                module_id: self.inner.module_id.clone(),
            })?;
        reply_rx.await.map_err(|_| SuperviseError::CommandClosed {
            module_id: self.inner.module_id.clone(),
        })?
    }
}

pub(super) async fn restart_child(
    spec: &ModuleSpec,
    runtime: &SupervisorRuntimeConfig,
    registry: &Registry,
    process_liveness: &SupervisorProcessLiveness,
    snapshot: &SharedSnapshot,
    child: &mut Option<SupervisedChild>,
    drain_timeout: Duration,
) -> Result<(), SuperviseError> {
    restart_child_with_budget_policy(
        spec,
        runtime,
        registry,
        process_liveness,
        snapshot,
        child,
        drain_timeout,
        true,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn restart_child_with_budget_policy(
    spec: &ModuleSpec,
    runtime: &SupervisorRuntimeConfig,
    registry: &Registry,
    process_liveness: &SupervisorProcessLiveness,
    snapshot: &SharedSnapshot,
    child: &mut Option<SupervisedChild>,
    drain_timeout: Duration,
    reset_budget: bool,
) -> Result<(), SuperviseError> {
    // Restart cycles a running module; it must not silently start a disabled one.
    if !lock_snapshot(snapshot)?.enabled {
        return Err(SuperviseError::Disabled {
            module_id: spec.module_id.clone(),
        });
    }
    let stop_notice = begin_forwarding_drain_with_timeout(
        spec,
        runtime,
        registry,
        snapshot,
        None,
        RouteCloseReason::Restart,
        drain_timeout,
    )
    .await?;

    if child.is_some() {
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
            drain_timeout,
            ModuleState::Restarting,
            Some(true),
            &FailureContext::operator(),
        )
        .await?;
    } else {
        update_snapshot(snapshot, Some(&spec.module_id), |state| {
            state.enabled = true;
            state.state = ModuleState::Restarting;
            clear_current_process_facts(state);
        })?;
        release_dead_registration(
            registry,
            runtime.forwarding.as_deref(),
            snapshot,
            &spec.module_id,
            &runtime.failure_context(false),
        )
        .await?;
    }

    if reset_budget {
        reset_restart_count(snapshot, &spec.module_id)?;
    }
    process_liveness.track(spec.module_id.clone(), Arc::clone(snapshot));
    schedule_respawn(
        runtime,
        snapshot,
        &spec.module_id,
        runtime.restart_policy.backoff,
        RespawnKind::Spawn,
        false,
    )
}

pub(super) async fn reload_child(
    spec: &ModuleSpec,
    runtime: &SupervisorRuntimeConfig,
    registry: &Registry,
    process_liveness: &SupervisorProcessLiveness,
    snapshot: &SharedSnapshot,
    child: &mut Option<SupervisedChild>,
) -> Result<(), SuperviseError> {
    // Reload cycles a running module; it must not silently start a disabled one.
    if !lock_snapshot(snapshot)?.enabled {
        return Err(SuperviseError::Disabled {
            module_id: spec.module_id.clone(),
        });
    }
    let stop_notice = begin_forwarding_drain(
        spec,
        runtime,
        registry,
        snapshot,
        Some(true),
        RouteCloseReason::Reload,
    )
    .await?;

    if child.is_some() {
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
            ModuleState::Restarting,
            Some(true),
            &FailureContext::operator(),
        )
        .await?;
    } else {
        update_snapshot(snapshot, Some(&spec.module_id), |state| {
            state.enabled = true;
            state.state = ModuleState::Restarting;
            clear_current_process_facts(state);
        })?;
        release_dead_registration(
            registry,
            runtime.forwarding.as_deref(),
            snapshot,
            &spec.module_id,
            &runtime.failure_context(false),
        )
        .await?;
    }

    reset_restart_count(snapshot, &spec.module_id)?;
    process_liveness.track(spec.module_id.clone(), Arc::clone(snapshot));
    schedule_respawn(
        runtime,
        snapshot,
        &spec.module_id,
        runtime.restart_policy.backoff,
        RespawnKind::Reload,
        false,
    )
}

pub(super) async fn finish_reload_child(
    spec: &ModuleSpec,
    runtime: &SupervisorRuntimeConfig,
    registry: &Registry,
    process_liveness: &SupervisorProcessLiveness,
    snapshot: &SharedSnapshot,
    child: &mut Option<SupervisedChild>,
) -> Result<(), SuperviseError> {
    process_liveness.track(spec.module_id.clone(), Arc::clone(snapshot));
    let next_child = match spawn_and_mark_running_async(spec, runtime, snapshot).await {
        Ok(next_child) => next_child,
        Err(error @ SuperviseError::ConfigDeliveryDeferred { .. }) => {
            defer_config_spawn(
                spec,
                runtime,
                snapshot,
                RespawnKind::Reload,
                &runtime.failure_context(runtime.recovery_is_daemon),
            )?;
            return Err(error);
        }
        Err(error @ SuperviseError::ConfigHeld { .. }) => {
            let failure = runtime.failure_context(runtime.recovery_is_daemon);
            if try_park_config_attempt(spec, runtime, snapshot, &failure, None)? {
                return Err(error);
            }
            defer_config_spawn(spec, runtime, snapshot, RespawnKind::Reload, &failure)?;
            return Err(SuperviseError::ConfigDeliveryDeferred {
                module_id: spec.module_id.clone(),
            });
        }
        Err(err) => {
            return handle_reload_spawn_failure(
                spec,
                runtime,
                process_liveness,
                snapshot,
                child,
                format!("new child failed to spawn: {err}"),
            )
            .await;
        }
    };
    *child = Some(next_child);

    let wait_outcome = {
        let active_child = child.as_mut().expect("new reload child was just stored");
        wait_for_registration_after_reload(
            registry,
            &spec.module_id,
            snapshot,
            active_child,
            REGISTRY_RELEASE_TIMEOUT,
        )
        .await?
    };

    match wait_outcome {
        RegistrationWaitOutcome::Registered => {
            child
                .as_mut()
                .expect("registered reload child")
                .registration_deadline = None;
            debug!(module_id = %spec.module_id, "supervised module reloaded and registered");
            Ok(())
        }
        RegistrationWaitOutcome::Exited(exit_report) => {
            if let Some(active_child) = child.as_mut() {
                active_child.drain_stderr(&spec.module_id).await;
            }
            // Keep the exited child's roster guard until its terminal is written.
            // Shutdown waits on that guard, not on the child Option used for respawn.
            let mut exited_child = child.take().expect("exited reload child is still stored");
            #[cfg(test)]
            if let Some(gate) = &runtime.test_reload_exit_record_gate {
                gate.reached.notify_one();
                gate.resume.notified().await;
            }
            let result = handle_reload_child_registration_failure(
                spec,
                runtime,
                registry,
                process_liveness,
                snapshot,
                child,
                ReloadRegistrationFailure {
                    exit_report: registration_failure_exit_report(exit_report),
                    cleanup_detail: exited_child.exit_cleanup_detail(None),
                    reason: exited_child
                        .spawn_failure
                        .clone()
                        .unwrap_or_else(|| "new child exited before registering".to_string()),
                },
            )
            .await;
            exited_child
                .finish_exit(false)
                .await
                .map_err(|source| SuperviseError::Wait {
                    module_id: spec.module_id.clone(),
                    source,
                })?;
            result
        }
        RegistrationWaitOutcome::TimedOut => {
            let mut timed_out_child = child
                .take()
                .expect("timed-out reload child is still running");
            timed_out_child
                .start_kill()
                .map_err(|source| SuperviseError::Kill {
                    module_id: spec.module_id.clone(),
                    source,
                })?;
            let status = timed_out_child
                .wait()
                .await
                .map_err(|source| SuperviseError::Wait {
                    module_id: spec.module_id.clone(),
                    source,
                })?;
            timed_out_child.drain_stderr(&spec.module_id).await;
            let result = handle_reload_child_registration_failure(
                spec,
                runtime,
                registry,
                process_liveness,
                snapshot,
                child,
                ReloadRegistrationFailure {
                    exit_report: registration_failure_exit_report(classify_reaped_child_exit(
                        snapshot,
                        &timed_out_child,
                        &status,
                    )),
                    cleanup_detail: timed_out_child.exit_cleanup_detail(None),
                    reason: format!(
                        "new child did not register within {:?}",
                        REGISTRY_RELEASE_TIMEOUT
                    ),
                },
            )
            .await;
            timed_out_child
                .finish_exit(false)
                .await
                .map_err(|source| SuperviseError::Wait {
                    module_id: spec.module_id.clone(),
                    source,
                })?;
            result
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn set_child_enabled(
    spec: &ModuleSpec,
    runtime: &SupervisorRuntimeConfig,
    registry: &Registry,
    process_liveness: &SupervisorProcessLiveness,
    snapshot: &SharedSnapshot,
    child: &mut Option<SupervisedChild>,
    enabled: bool,
    failure: &FailureContext,
) -> Result<bool, SuperviseError> {
    set_child_enabled_with_budget_policy(
        spec,
        runtime,
        registry,
        process_liveness,
        snapshot,
        child,
        enabled,
        failure,
        true,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn set_child_enabled_with_budget_policy(
    spec: &ModuleSpec,
    runtime: &SupervisorRuntimeConfig,
    registry: &Registry,
    process_liveness: &SupervisorProcessLiveness,
    snapshot: &SharedSnapshot,
    child: &mut Option<SupervisedChild>,
    enabled: bool,
    failure: &FailureContext,
    reset_budget: bool,
) -> Result<bool, SuperviseError> {
    let (current_enabled, current_state, respawn_pending) = {
        let state = lock_snapshot(snapshot)?;
        (state.enabled, state.state, state.respawn_pending)
    };
    // `start` (enable on an already-enabled module) heals TERMINAL states instead
    // of no-op'ing: a module whose restart budget exhausted (Failed) or that exited
    // clean (Stopped) has no live process and no other in-band recovery — the
    // operator's start is the explicit recovery act and resets the budget. Without
    // this arm the only revival was subc-probe --supervisor-restart in a terminal,
    // which the 2026-07-14 aft outage proved is a trap when the failed module is
    // the one providing every agent's shell.
    let revive_terminal = enabled
        && current_enabled
        && child.is_none()
        && (matches!(current_state, ModuleState::Failed | ModuleState::Stopped)
            || (current_state == ModuleState::Restarting && !respawn_pending));
    if current_enabled == enabled && !revive_terminal {
        return Ok(false);
    }

    if enabled {
        update_snapshot(snapshot, Some(&spec.module_id), |state| {
            state.enabled = true;
            state.state = ModuleState::Starting;
            clear_current_process_facts(state);
        })?;
        #[cfg(test)]
        if runtime.test_seed_stale_facts_before_enable_spawn {
            update_snapshot(snapshot, Some(&spec.module_id), |state| {
                state.process_alive = true;
                state.pid = Some(41);
                state.spawned_at_ms = Some(42);
                state.spawned_from = Some(PathBuf::from("/spawned/module"));
                state.spawned_file_identity = Some(SpawnedFileIdentity {
                    device: 43,
                    inode: 44,
                    #[cfg(windows)]
                    windows_image: None,
                });
            })?;
        }
        release_dead_registration(
            registry,
            runtime.forwarding.as_deref(),
            snapshot,
            &spec.module_id,
            failure,
        )
        .await?;
        if reset_budget {
            reset_restart_count(snapshot, &spec.module_id)?;
        }
        process_liveness.track(spec.module_id.clone(), Arc::clone(snapshot));
        let next_child = match spawn_and_mark_running_async(spec, runtime, snapshot).await {
            Ok(next_child) => next_child,
            Err(SuperviseError::ConfigDeliveryDeferred { .. }) => {
                defer_config_spawn(spec, runtime, snapshot, RespawnKind::Spawn, failure)?;
                return Ok(true);
            }
            Err(error @ SuperviseError::ConfigHeld { .. }) => {
                if let Err(parking_error) = hold_config_spawn(spec, runtime, snapshot, failure) {
                    process_liveness.untrack_if_current(&spec.module_id, snapshot);
                    return Err(parking_error);
                }
                return Err(error);
            }
            Err(err) => {
                fail_snapshot(snapshot, Some(&spec.module_id), None, failure);
                process_liveness.untrack_if_current(&spec.module_id, snapshot);
                return Err(err);
            }
        };
        *child = Some(next_child);
        debug!(module_id = %spec.module_id, "supervised module enabled");
        Ok(true)
    } else {
        update_snapshot(snapshot, Some(&spec.module_id), |state| {
            state.config_hold_parked = false;
            state.config_hold_daemon_initiated = false;
            state.config_hold_approval = None;
        })?;
        let stop_notice = begin_forwarding_drain_if_configured(
            spec,
            runtime,
            registry,
            snapshot,
            Some(false),
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
            ModuleState::Disabled,
            Some(false),
            failure,
        )
        .await?;
        debug!(module_id = %spec.module_id, "supervised module disabled");
        Ok(true)
    }
}
