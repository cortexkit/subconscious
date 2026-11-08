//! Supervisor-owned observations for configuration delivery, independent of
//! module claims in `config.applied`.

use super::*;
use crate::config_gate::runtime::{DeliveryObserver, DeliverySpawn, RuntimeOwner};

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
    ) -> RegistrationGuard<'a> {
        RegistrationGuard {
            runtime: self.runtime,
            spawn: spawn.cloned(),
            active: !matches!(role, SpawnRole::SwapCandidate),
            committed: false,
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

pub(super) struct RegistrationGuard<'a> {
    runtime: &'a ConfigDelivery,
    spawn: Option<DeliverySpawn>,
    active: bool,
    committed: bool,
}
impl RegistrationGuard<'_> {
    pub(super) fn commit(mut self) {
        self.committed = true;
    }
}
impl Drop for RegistrationGuard<'_> {
    fn drop(&mut self) {
        if !self.committed {
            if let Some(spawn) = &self.spawn {
                self.runtime.0.delivery_abandon_spawn(spawn, self.active);
            }
        }
    }
}

impl ConfigDelivery {
    pub(super) async fn unfenced(&self, module: &str) {
        if let Ok(Some(mut changed)) = self.0.delivery_exit_fence(module) {
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
        if let Err(error) = self.0.registered(registration.connection_id, spawn) {
            warn!(module_id=module, %error, "configuration registration observation failed");
        }
        self.0.notify_delivery_changed();
    }

    pub(super) async fn healthy(
        &self,
        module: &str,
        forwarding: Option<&ForwardingTable>,
        registry: &Registry,
    ) {
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
        let generation = self
            .0
            .delivery_current_attempt(registration.connection_id)
            .ok()
            .flatten()
            .map(|(generation, _)| generation)
            .or_else(|| {
                self.0
                    .delivery_child_generation(registration.connection_id)
                    .ok()
                    .flatten()
            });
        if let Some(generation) = generation {
            if let Err(error) = self.0.ready(registration.connection_id, module, generation) {
                warn!(module_id=module, %error, "configuration health observation failed");
            }
        }
        self.0.notify_delivery_changed();
    }
}
