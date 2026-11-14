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
