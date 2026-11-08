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
    #[cfg(test)]
    pub(super) fn pause_preflight_for_test(&self) {
        self.runtime.0.pause_spawn_preflight_for_test();
    }
    pub(super) fn admit(&self, spec: &ModuleSpec) -> Result<(), SuperviseError> {
        match self.runtime.0.delivery_exit_fence(&spec.module_id) {
            Ok(None) => Ok(()),
            Ok(Some(_)) => Err(SuperviseError::ConfigDeliveryDeferred {
                module_id: spec.module_id.clone(),
            }),
            Err(error) => Err(SuperviseError::InvalidSpec {
                reason: error.to_string(),
            }),
        }
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
        at: Instant,
        role: SpawnRole,
    ) -> Result<Option<DeliverySpawn>, SuperviseError> {
        self.runtime
            .0
            .delivery_spawned_in_slot(
                &spec.module_id,
                at,
                self.register_timeout,
                self.health_budget,
                !matches!(role, SpawnRole::SwapCandidate),
            )
            .map_err(|error| SuperviseError::InvalidSpec {
                reason: error.to_string(),
            })
    }

    #[cfg(feature = "test-support")]
    pub(super) fn pause_before_exec(&self, spec: &ModuleSpec) -> Result<(), SuperviseError> {
        self.runtime
            .0
            .pause_spawn_before_exec()
            .map_err(|error| SuperviseError::Spawn {
                program: spec.program.clone(),
                source: io::Error::other(error.to_string()),
                cgroup_path: None,
            })
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
    pub(super) async fn launch_current(&self) -> Result<bool, SuperviseError> {
        let Some(spawn) = &self.spawn else {
            return Ok(true);
        };
        loop {
            match self.runtime.0.delivery_launch_current(spawn, self.active) {
                Ok(Some(current)) => return Ok(current),
                Ok(None) => {
                    self.runtime
                        .0
                        .on_blocking(|owner| owner.delivery_wait_for_owner())
                        .await
                }
                Err(error) => Err(error),
            }
            .map_err(|error| SuperviseError::InvalidSpec {
                reason: error.to_string(),
            })?;
        }
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
    pub(super) async fn registered(
        &self,
        module: &str,
        forwarding: Option<&ForwardingTable>,
        registry: &Registry,
        child: &SupervisedChild,
    ) {
        let Some(spawn) = &child.config_delivery else {
            return;
        };
        let Ok(gate) = self.0.module_gate(module) else {
            return;
        };
        let _guard = gate.lock().await;
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
        let spawn = spawn.clone();
        if let Err(error) = self
            .0
            .on_blocking(move |owner| owner.registered(connection, &spawn))
            .await
        {
            warn!(module_id=module, %error, "configuration registration observation failed");
        }
        self.0.notify_delivery_changed();
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
}
