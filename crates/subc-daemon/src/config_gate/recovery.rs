//! Recovery retains running views, but never imports rejected store authority.

use super::*;

impl RuntimeOwner {
    pub(crate) fn authority_exit_retry_generation(&self) -> Result<Option<u64>> {
        Ok(self.lock()?.authority_exit)
    }
    pub(crate) fn pending_authority_exit(&self) -> Result<Option<u64>> {
        let mut state = self.lock()?;
        if state.authority_exit.is_some() && state.store.delivery_ready().is_ok() {
            Ok(state.authority_exit)
        } else {
            Ok(None)
        }
    }
    pub(crate) fn is_recovering(&self) -> bool {
        self.admission
            .read()
            .map(|cache| cache.is_recovering())
            .unwrap_or(true)
    }

    /// Called off the async runtime before control dispatch; no snapshot or
    /// proposal is needed to discover that the store has changed.
    pub(crate) fn check_recovery(&self) -> Result<bool> {
        let mut state = self.lock()?;
        let _ = state.store.check();
        Ok(state.recovery_exit_hold
            || matches!(
                state.store.loaded_admission_state().0,
                StoreState::StoreMissing | StoreState::StoreUnverified
            ))
    }

    pub(crate) fn authority_exit_configuration(
        &self,
        generation: u64,
    ) -> Result<
        Option<(
            crate::daemon_config::DaemonConfig,
            crate::daemon_config::ActivityConfig,
            bool,
        )>,
    > {
        let mut state = self.lock()?;
        if state.authority_exit != Some(generation) {
            return Ok(None);
        }
        let body = state.store.body()?;
        let layer = body
            .generations
            .get(&generation)
            .and_then(|entry| entry.delivery.as_ref())
            .and_then(|snapshot| snapshot.daemon.as_ref())
            .ok_or_else(|| {
                GateError::new("config_unavailable", "approved exit has no daemon snapshot")
            })?;
        let bytes = layer.bytes.as_deref().ok_or_else(GateError::stale)?;
        let doc = std::str::from_utf8(bytes).map_err(|_| GateError::stale())?;
        let path = layer.path.as_deref().ok_or_else(GateError::stale)?;
        crate::daemon_config::parse_with_activity(doc, std::path::Path::new(path))
            .map(|(config, activity)| Some((config, activity, body.stamped)))
            .map_err(|error| GateError::new("invalid_daemon_config", error.to_string()))
    }

    /// Release admission only after all committed-exit reconciliation succeeds.
    pub(crate) fn complete_authority_exit(&self, generation: u64) -> Result<()> {
        let mut state = self.lock()?;
        if state.authority_exit != Some(generation) {
            return Err(GateError::stale());
        }
        // Actors created under the recovery hold may not have been observed by
        // the coordinator yet. Queue their ordinary admission release here.
        let modules =
            crate::config_gate::GateSnapshot::from_store(&state.store.body()?.approved)?.modules();
        state.admission_releases.extend(modules);
        state.authority_exit = None;
        state.recovery_exit_hold = false;
        let unstamped = state.store.loaded_admission_state().0 == StoreState::Unstamped;
        drop(state);
        if unstamped {
            self.retire_unstamped_bindings();
        }
        Ok(())
    }

    pub(super) fn retire_unstamped_bindings(&self) {
        // The store mutex must already be released: image checks hold the
        // program-launch mutex while recording refusals, and retirement takes
        // that same program-launch mutex to release retained Windows handles.
        for retire in self
            .binding_retirements
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
        {
            retire();
        }
    }

    pub(crate) fn take_unstamp_hint(&self, connection: ConnectionId) -> Result<bool> {
        let mut state = self.lock()?;
        Ok(state.authority_exit.is_none()
            && state.store.loaded_admission_state().0 == StoreState::Unstamped
            && state.unstamp_hints.remove(&connection))
    }
}

pub(super) fn active_view(
    state: &OwnerState,
    module: &str,
    declaration: &ConfigDeclareRequest,
    request: &subc_protocol::config::ConfigGetRequest,
) -> Result<subc_protocol::config::ConfigGetReply> {
    let body = state.store.loaded_admission_state().1;
    let generation = body
        .versions
        .iter()
        .find(|(tuple, _)| tuple.module == module && tuple.project.is_none())
        .and_then(|(_, versions)| versions.active)
        .ok_or_else(|| GateError::new("config_gated", "no in-memory active view in recovery"))?;
    if request.attempt.is_some() || request.generation.is_some_and(|id| id != generation) {
        return Err(GateError::new(
            "config_gated",
            "recovery serves only the in-memory active view",
        ));
    }
    let delivery = body
        .generations
        .get(&generation)
        .and_then(|entry| entry.delivery.as_ref())
        .ok_or_else(|| GateError::new("config_gated", "active delivery is unavailable"))?;
    if delivery.declarations.get(module) != Some(declaration) {
        return Err(GateError::new(
            "config_gated",
            "active declaration does not match",
        ));
    }
    let mut view = delivery
        .modules
        .get(module)
        .cloned()
        .ok_or_else(|| GateError::new("config_gated", "active view is unavailable"))?;
    view.generation = generation;
    view.status = ConfigStatus::Active;
    view.attempt = None;
    Ok(subc_protocol::config::ConfigGetReply::new(Some(view), None))
}

pub(super) fn changed_file_views(
    owner: &RuntimeOwner,
    state: &mut OwnerState,
    current: &CapturedConfiguration,
) -> Vec<ConnectionId> {
    let connections: Vec<_> = state
        .active_declarations
        .iter()
        .map(|(module, connection)| (module.clone(), *connection))
        .collect();
    connections
        .into_iter()
        .filter_map(|(module, connection)| {
            let next = &current.modules.get(&module)?.view;
            let old = owner
                .module_get_locked(
                    state,
                    connection,
                    &module,
                    subc_protocol::config::ConfigGetRequest::new(),
                    false,
                )
                .ok();
            let old = old.and_then(|reply| reply.active.or(reply.pending));
            (old.is_none_or(|old| old.layers != next.layers)).then_some(connection)
        })
        .collect()
}
