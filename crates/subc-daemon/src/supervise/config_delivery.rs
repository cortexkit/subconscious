//! Supervisor-owned observations for configuration delivery, independent of
//! module claims in `config.applied`.

use super::*;
use crate::config_gate::runtime::{
    AppliedDisposition, DeliveryObserver, DeliverySpawn, RuntimeOwner,
};

#[derive(Clone)]
pub(super) struct ConfigDelivery(pub(super) Arc<RuntimeOwner>);

impl std::fmt::Debug for ConfigDelivery {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ConfigDelivery")
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy)]
pub(super) struct SpawnDelivery<'a> {
    runtime: &'a ConfigDelivery,
    register_timeout: Duration,
    health_budget: Duration,
}

impl SupervisorRuntimeConfig {
    pub(super) fn spawn_delivery(&self) -> Option<SpawnDelivery<'_>> {
        self.config_delivery.as_ref().map(|runtime| SpawnDelivery {
            runtime,
            register_timeout: self.register_timeout,
            health_budget: self.health.deadline,
        })
    }
}

impl<'a> SpawnDelivery<'a> {
    pub(super) fn record_refusal(
        &self,
        spec: &ModuleSpec,
        base: &subc_config::Digest,
        detail: &str,
    ) -> Result<bool, SuperviseError> {
        self.runtime
            .0
            .record_program_refusal(
                &spec.module_id,
                base,
                subc_config::raw_digest(detail.as_bytes()),
            )
            .map_err(|error| gate_hold_error(spec, error, Some(self.runtime)))
    }
    pub(super) fn runtime(&self) -> ConfigDelivery {
        self.runtime.clone()
    }
    pub(super) fn prepare(
        &self,
        spec: &ModuleSpec,
    ) -> Result<crate::config_gate::runtime::SpawnAdmission, SuperviseError> {
        self.runtime
            .0
            .prepare_spawn(spec)
            .map_err(|error| gate_hold_error(spec, error, Some(self.runtime)))
    }
    pub(super) fn check(
        &self,
        spec: &ModuleSpec,
        admission: &crate::config_gate::runtime::SpawnAdmission,
    ) -> Result<(), SuperviseError> {
        self.runtime
            .0
            .check_spawn(&spec.module_id, admission)
            .map_err(|error| gate_hold_error(spec, error, Some(self.runtime)))
    }
    pub(super) fn module_gate(
        &self,
        spec: &ModuleSpec,
    ) -> Result<Arc<tokio::sync::Mutex<()>>, SuperviseError> {
        self.runtime
            .0
            .module_gate(&spec.module_id)
            .map_err(|error| gate_hold_error(spec, error, Some(self.runtime)))
    }
    #[cfg(test)]
    pub(super) fn pause_preflight_for_test(&self) {
        self.runtime.0.pause_spawn_preflight_for_test();
    }
    pub(super) fn registration_guard(
        self,
        spawn: Option<&DeliverySpawn>,
        role: SpawnRole,
    ) -> RegistrationGuard {
        RegistrationGuard {
            runtime: self.runtime.clone(),
            spawn: spawn.cloned(),
            active: !matches!(role, SpawnRole::SwapCandidate),
            committed: false,
            deferred_cleanup: false,
        }
    }
    pub(super) fn spawned(
        &self,
        spec: &ModuleSpec,
        admission: &crate::config_gate::runtime::SpawnAdmission,
        at: Instant,
        role: SpawnRole,
        authority: Option<Arc<crate::spawn_authority::SpawnAuthorityRecord>>,
    ) -> Result<Option<DeliverySpawn>, SuperviseError> {
        self.runtime
            .0
            .delivery_spawned_for_admission_in_slot(
                &spec.module_id,
                admission,
                at,
                self.register_timeout,
                self.health_budget,
                !matches!(role, SpawnRole::SwapCandidate),
                authority,
            )
            .map_err(|error| gate_hold_error(spec, error, Some(self.runtime)))
    }
}

pub(super) fn gate_hold_error(
    spec: &ModuleSpec,
    error: crate::config_gate::GateError,
    delivery: Option<&ConfigDelivery>,
) -> SuperviseError {
    if error.code == "config_delivery_deferred" {
        SuperviseError::ConfigDeliveryDeferred {
            module_id: spec.module_id.clone(),
        }
    } else if matches!(
        error.code.as_str(),
        "config_gated" | "program_refused" | subc_control::REGISTRATION_CHANNEL_UNSUPPORTED
    ) && delivery
        .is_some_and(|delivery| delivery.0.has_recorded_admission_hold(&spec.module_id))
    {
        SuperviseError::ConfigHeld {
            module_id: spec.module_id.clone(),
            code: error.code,
            detail: error.detail,
        }
    } else {
        SuperviseError::Spawn {
            program: spec.program.clone(),
            source: std::io::Error::other(error),
            cgroup_path: None,
        }
    }
}

pub(super) struct RegistrationGuard {
    runtime: ConfigDelivery,
    spawn: Option<DeliverySpawn>,
    active: bool,
    committed: bool,
    deferred_cleanup: bool,
}
impl RegistrationGuard {
    pub(super) fn defer_cleanup(&mut self) {
        self.deferred_cleanup = true;
    }
    pub(super) fn launch_current(&self) -> Result<bool, SuperviseError> {
        let Some(spawn) = &self.spawn else {
            return Ok(true);
        };
        launch_current_result(
            spawn.module(),
            self.runtime.0.delivery_launch_current(spawn, self.active),
        )
    }
    pub(super) fn commit(mut self) {
        self.committed = true;
    }
}
impl Drop for RegistrationGuard {
    fn drop(&mut self) {
        if !self.committed {
            if let Some(spawn) = &self.spawn {
                if self.deferred_cleanup {
                    self.runtime
                        .0
                        .delivery_queue_abandoned_spawn(spawn.clone(), self.active);
                } else {
                    self.runtime.0.delivery_abandon_spawn(spawn, self.active);
                }
            }
        }
    }
}

impl ConfigDelivery {
    pub(super) async fn unfenced(&self, module: &str) {
        if let Ok(gate) = self.0.module_gate(module) {
            // This future is selected alongside actor commands. Waiting for
            // publication here never parks the actor's command handler.
            drop(gate.lock().await);
        }
        let module = module.to_owned();
        if let Ok(Some(mut changed)) = self
            .0
            .on_blocking(move |owner| owner.delivery_exit_fence(&module))
            .await
        {
            while *changed.borrow_and_update() {
                if changed.changed().await.is_err() {
                    break;
                }
            }
        }
    }
    pub(super) fn queue_registered(
        &self,
        module: &str,
        forwarding: Option<&Arc<ForwardingTable>>,
        registry: &Arc<Registry>,
        child: &mut SupervisedChild,
    ) {
        let Some(spawn) = &child.config_delivery else {
            return;
        };
        let Ok(gate) = self.0.module_gate(module) else {
            return;
        };
        let Some(registration) = registry.get_module(module).ok().flatten() else {
            return;
        };
        if forwarding
            .and_then(|forwarding| forwarding.active_module_endpoint(module).ok().flatten())
            .is_none_or(|endpoint| endpoint.connection_id != registration.connection_id)
        {
            return;
        }
        let connection = registration.connection_id;
        if child.registered_config_connection == Some(connection) {
            return;
        }
        child.registered_config_connection = Some(connection);
        let spawn = spawn.clone();
        let owner = self.0.clone();
        let registry = registry.clone();
        let forwarding = forwarding.cloned();
        let module = module.to_owned();
        tokio::spawn(async move {
            let _guard = gate.lock().await;
            if registry
                .get_module(&module)
                .ok()
                .flatten()
                .is_none_or(|current| current.connection_id != connection)
                || forwarding
                    .as_ref()
                    .and_then(|forwarding| {
                        forwarding.active_module_endpoint(&module).ok().flatten()
                    })
                    .is_none_or(|endpoint| endpoint.connection_id != connection)
            {
                return;
            }
            if let Err(error) = owner
                .on_blocking(move |owner| owner.registered(connection, &spawn))
                .await
            {
                warn!(module_id=%module,%error,"configuration registration observation failed");
            }
            owner.notify_delivery_changed();
        });
    }

    pub(super) async fn health_probe(
        &self,
        module: &str,
        forwarding: Option<&ForwardingTable>,
        registry: &Registry,
    ) -> Option<crate::config_gate::runtime::DeliveryProbe> {
        let registration = registry.get_module(module).ok().flatten()?;
        if forwarding
            .and_then(|forwarding| forwarding.active_module_endpoint(module).ok().flatten())
            .is_none_or(|endpoint| endpoint.connection_id != registration.connection_id)
        {
            return None;
        }
        let module = module.to_owned();
        self.0
            .on_blocking(move |owner| owner.health_probe(registration.connection_id, &module))
            .await
            .ok()
            .flatten()
    }

    pub(super) async fn health_result(
        &self,
        module: &str,
        forwarding: Option<&ForwardingTable>,
        registry: &Registry,
        probe: crate::config_gate::runtime::DeliveryProbe,
        healthy: bool,
    ) -> bool {
        // Even an obsolete policy probe belongs to delivery, not to a second
        // health-triggered restart after its attempt has already failed.
        let owned_failure = !healthy && probe.owns_failure();
        let Ok(gate) = self.0.module_gate(module) else {
            return false;
        };
        let _guard = gate.lock().await;
        let active = registry
            .get_module(module)
            .ok()
            .flatten()
            .is_some_and(|registration| registration.connection_id == probe.connection())
            && forwarding
                .and_then(|table| table.active_module_endpoint(module).ok().flatten())
                .is_some_and(|endpoint| endpoint.connection_id == probe.connection());
        if !active {
            return owned_failure;
        }
        let failed = match self
            .0
            .on_blocking(move |owner| owner.health_result(probe, healthy))
            .await
        {
            Ok(Some(
                AppliedDisposition::Failed { .. } | AppliedDisposition::CatchupFailed { .. },
            )) => true,
            Ok(_) => false,
            Err(error) => {
                warn!(module_id=module, %error, "configuration health observation failed");
                !healthy && error.code == "anchor_pending"
            }
        };
        self.0.notify_delivery_changed();
        failed || owned_failure
    }

    /// A health probe answered: `ready` when it reported ready. `probed` is
    /// the connection whose registration answered. Counts toward `spawn`'s
    /// first-party mark only when that registration is the spawn's own,
    /// whatever registration the module has by the time the answer is read.
    pub(super) async fn mark_readiness(
        &self,
        module: &str,
        forwarding: Option<&ForwardingTable>,
        probed: Option<ConnectionId>,
        spawn: &DeliverySpawn,
        ready: bool,
    ) {
        let Some(record) = spawn.authority() else {
            return;
        };
        let own = forwarding
            .zip(probed)
            .and_then(|(forwarding, probed)| {
                forwarding
                    .target_authority_for_connection(probed)
                    .ok()
                    .flatten()
            })
            .is_some_and(|answered| Arc::ptr_eq(&answered, record));
        if !own {
            return;
        }
        let spawn = spawn.clone();
        let result = self
            .0
            .on_blocking(move |owner| {
                if ready {
                    owner.mark_ready(&spawn)
                } else {
                    owner.mark_not_ready(&spawn)
                }
            })
            .await;
        if let Err(error) = result {
            warn!(module_id = module, %error, "first-party readiness observation failed");
        }
    }
}

fn launch_current_result(
    module: &str,
    result: std::result::Result<Option<bool>, crate::config_gate::GateError>,
) -> Result<bool, SuperviseError> {
    match result {
        Ok(Some(current)) => Ok(current),
        Ok(None) => Err(SuperviseError::ConfigDeliveryDeferred {
            module_id: module.to_owned(),
        }),
        Err(_) => Err(SuperviseError::ConfigDeliveryDeferred {
            module_id: module.to_owned(),
        }),
    }
}

impl SupervisedModule {
    pub(crate) fn check_approved_start_failure(
        &self,
        generation: &crate::config_gate::runtime::ApprovalIdentity,
    ) -> Result<bool, SuperviseError> {
        let state = lock_snapshot(&self.inner.snapshot)?;
        if let Some((failed_generation, detail)) = &state.approved_start_failure {
            if failed_generation == generation {
                return Err(SuperviseError::ReloadFailed {
                    module_id: self.inner.module_id.clone(),
                    reason: detail.clone(),
                });
            }
        }
        Ok(state.approved_start_failure.is_some())
    }

    pub(crate) fn has_approved_start_failure(
        &self,
        identity: &crate::config_gate::runtime::ApprovalIdentity,
    ) -> bool {
        lock_snapshot(&self.inner.snapshot).is_ok_and(|state| {
            state
                .approved_start_failure
                .as_ref()
                .is_some_and(|(failed, _)| failed == identity)
        })
    }

    pub(crate) fn needs_approved_gate_restart(
        &self,
        approval: &crate::config_gate::runtime::ApprovalIdentity,
    ) -> bool {
        lock_snapshot(&self.inner.snapshot)
            .is_ok_and(|state| state.needs_approved_gate_restart(approval))
    }

    #[cfg(all(test, unix))]
    pub(crate) fn config_hold_origin_for_test(
        &self,
    ) -> (bool, Option<crate::config_gate::runtime::ApprovalIdentity>) {
        let state = lock_snapshot(&self.inner.snapshot).unwrap();
        (
            state.config_hold_daemon_initiated,
            state.config_hold_approval.clone(),
        )
    }

    pub(crate) async fn config_policy_probe(
        &self,
        connection: crate::ConnectionId,
        generation: u64,
        attempt: u64,
    ) -> Result<(), SuperviseError> {
        self.inner
            .commands
            .send(SupervisorCommand::ConfigPolicyProbe {
                connection,
                generation,
                attempt,
            })
            .await
            .map_err(|_| SuperviseError::CommandClosed {
                module_id: self.inner.module_id.clone(),
            })
    }
    pub(crate) async fn config_exit_hold(&self) -> Result<(), SuperviseError> {
        let (reply, result) = oneshot::channel();
        self.inner
            .commands
            .send(SupervisorCommand::ConfigExitHold { reply })
            .await
            .map_err(|_| SuperviseError::CommandClosed {
                module_id: self.module_id().to_string(),
            })?;
        result.await.map_err(|_| SuperviseError::CommandClosed {
            module_id: self.module_id().to_string(),
        })?
    }
    pub(crate) async fn config_delivery_failed(
        &self,
        connection: crate::ConnectionId,
        hold: bool,
    ) -> Result<(), SuperviseError> {
        self.inner
            .commands
            .send(SupervisorCommand::ConfigDeliveryFailed { connection, hold })
            .await
            .map_err(|_| SuperviseError::CommandClosed {
                module_id: self.module_id().to_string(),
            })
    }

    pub(crate) async fn config_apply_restart(&self, generation: u64) -> Result<(), SuperviseError> {
        self.inner
            .commands
            .send(SupervisorCommand::ConfigApplyRestart {
                generation,
                supersedes_swap: false,
            })
            .await
            .map_err(|_| SuperviseError::CommandClosed {
                module_id: self.inner.module_id.clone(),
            })
    }
}

impl SupervisedModule {
    pub(crate) async fn reconcile_approved_configuration(
        &self,
        approved: crate::config_gate::runtime::ApprovedDaemonConfiguration,
    ) -> Result<(), SuperviseError> {
        let entry = approved
            .module_entry(&self.inner.module_id)
            .map_err(|error| SuperviseError::InvalidSpec {
                reason: error.to_string(),
            })?;
        validate_spec(&entry.module_spec())?;
        let (reply, received) = oneshot::channel();
        self.inner
            .commands
            .send(SupervisorCommand::ApprovedConfiguration { approved, reply })
            .await
            .map_err(|_| SuperviseError::CommandClosed {
                module_id: self.inner.module_id.clone(),
            })?;
        received.await.map_err(|_| SuperviseError::CommandClosed {
            module_id: self.inner.module_id.clone(),
        })??;
        let mut configuration =
            self.inner
                .configuration
                .lock()
                .map_err(|_| SuperviseError::StatePoisoned {
                    module_id: Some(self.inner.module_id.clone()),
                })?;
        configuration.spec = entry.module_spec();
        configuration.enabled = entry.enabled;
        configuration.health = entry.health;
        configuration.register_timeout = entry.register_timeout;
        configuration.restart_policy = entry.restart;
        Ok(())
    }
}

pub(super) fn cancel_deferred_reload(
    runtime: &SupervisorRuntimeConfig,
    module_id: &str,
    reason: &str,
) {
    if let Some(reply) = runtime
        .deferred_reload_reply
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .take()
    {
        let _ = reply.send(Err(SuperviseError::ReloadFailed {
            module_id: module_id.to_string(),
            reason: reason.to_string(),
        }));
    }
}

fn park_config_spawn(
    spec: &ModuleSpec,
    runtime: &SupervisorRuntimeConfig,
    snapshot: &SharedSnapshot,
    failure: &FailureContext,
    admission_refused: Option<String>,
) -> Result<(), SuperviseError> {
    *runtime
        .scheduled_respawn
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    if let Some(reply) = runtime
        .deferred_reload_reply
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take()
    {
        let _ = reply.send(Err(SuperviseError::ConfigHeld {
            module_id: spec.module_id.clone(),
            code: "config_gated".into(),
            detail: "module is held by configuration admission".into(),
        }));
    }
    update_snapshot(snapshot, Some(&spec.module_id), |state| {
        state.admission_refused = admission_refused;
        state.config_hold_daemon_initiated = failure.daemon_initiated;
        state.config_hold_approval = failure.approval.clone();
        state.config_hold_parked = true;
        state.state = ModuleState::Stopped;
        state.respawn_pending = false;
        state.failed_retry.pending = None;
        // Parking ends this attempt and discards its failure timestamp. If a
        // later release fails, its retry delay starts at that new failure.
        state.failed_retry.failed_at = None;
        clear_current_process_facts(state);
    })
}

pub(super) fn try_park_config_attempt(
    spec: &ModuleSpec,
    runtime: &SupervisorRuntimeConfig,
    snapshot: &SharedSnapshot,
    failure: &FailureContext,
    program_base: Option<&subc_config::Digest>,
) -> Result<bool, SuperviseError> {
    let result = (|| {
        if !lock_snapshot(snapshot)?.enabled {
            return Ok(false);
        }
        #[cfg(test)]
        if let Some(delivery) = &runtime.config_delivery {
            delivery.0.pause_before_config_park_for_test();
        }
        let Some(delivery) = &runtime.config_delivery else {
            return Ok(false);
        };
        let parked = delivery
            .0
            .with_recorded_admission_hold(&spec.module_id, program_base, |refusal| {
                park_config_spawn(spec, runtime, snapshot, failure, refusal)
            })
            .map_err(|error| config_delivery::gate_hold_error(spec, error, Some(delivery)))?;
        if let Some(result) = parked {
            result?;
            Ok(true)
        } else {
            Ok(false)
        }
    })();
    if let Err(error) = &result {
        // A failed park still belongs to the attempt that tried to park. Mark it
        // Failed here, before the monitor can use a daemon/operator origin saved
        // for a different attempt to decide whether to schedule a slow retry.
        fail_snapshot(snapshot, Some(&spec.module_id), None, failure);
        record_approved_attempt_failure(spec, snapshot, failure, error);
    }
    result
}

pub(super) fn record_approved_attempt_failure(
    spec: &ModuleSpec,
    snapshot: &SharedSnapshot,
    failure: &FailureContext,
    error: &SuperviseError,
) {
    let Some(approval) = &failure.approval else {
        return;
    };
    let _ = update_snapshot(snapshot, Some(&spec.module_id), |state| {
        if state.enabled
            && state.state == ModuleState::Failed
            && state
                .approved_start_failure
                .as_ref()
                .is_none_or(|(identity, _)| identity != approval)
        {
            state.approved_start_failure = Some((approval.clone(), error.to_string()));
        }
    });
}

pub(super) fn hold_config_spawn(
    spec: &ModuleSpec,
    runtime: &SupervisorRuntimeConfig,
    snapshot: &SharedSnapshot,
    failure: &FailureContext,
) -> Result<(), SuperviseError> {
    hold_config_respawn(spec, runtime, snapshot, RespawnKind::Spawn, failure)
}

pub(super) fn hold_config_respawn(
    spec: &ModuleSpec,
    runtime: &SupervisorRuntimeConfig,
    snapshot: &SharedSnapshot,
    kind: RespawnKind,
    failure: &FailureContext,
) -> Result<(), SuperviseError> {
    if !lock_snapshot(snapshot)?.enabled {
        return Ok(());
    }
    if !try_park_config_attempt(spec, runtime, snapshot, failure, None)? {
        // This caller consumed an actual start attempt. If its hold ended before
        // parking, retain that attempt instead of waiting for a lost release.
        defer_config_spawn(spec, runtime, snapshot, kind, failure)?;
    }
    Ok(())
}

pub(super) const CONFIG_ADMISSION_RETRY_DELAY: Duration = Duration::from_millis(100);

pub(super) fn defer_config_spawn(
    spec: &ModuleSpec,
    runtime: &SupervisorRuntimeConfig,
    snapshot: &SharedSnapshot,
    kind: RespawnKind,
    failure: &FailureContext,
) -> Result<(), SuperviseError> {
    update_snapshot(snapshot, Some(&spec.module_id), |state| {
        state.state = ModuleState::Restarting;
        state.respawn_pending = true;
        clear_current_process_facts(state);
    })?;
    *runtime
        .scheduled_respawn
        .lock()
        .unwrap_or_else(|p| p.into_inner()) = Some(PendingRespawn {
        deadline: Instant::now() + CONFIG_ADMISSION_RETRY_DELAY,
        kind,
        daemon_initiated: failure.daemon_initiated,
        approval: failure.approval.clone(),
    });
    Ok(())
}

#[cfg(test)]
mod launch_current_tests {
    use super::*;
    #[test]
    fn config_admission_post_exec_poisoned_launch_recheck_defers_instead_of_failing_spec() {
        let result = launch_current_result(
            "a",
            Err(crate::config_gate::GateError::new(
                "config_unavailable",
                "configuration owner lock is poisoned",
            )),
        );
        assert!(
            matches!(result, Err(SuperviseError::ConfigDeliveryDeferred { .. })),
            "{result:?}"
        );
    }
}
