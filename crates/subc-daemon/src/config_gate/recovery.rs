//! Recovery retains running views, but never imports rejected store authority.

use super::*;

impl OwnerState {
    pub(super) fn queue_anchor_hints(&mut self) {
        self.anchor_hints.extend(self.anchor_waiters.drain());
        if !self.anchor_hints.is_empty() {
            // Wake only for recovered anchor hints, not whenever the configuration
            // owner's mutex is released. Unrelated wakes can disrupt the supervisor's
            // sequence of recording an exited child and allowing its replacement.
            self.anchor_notify.notify_one();
        }
    }
}

impl ApprovedDaemonConfiguration {
    pub(crate) fn identity(&self) -> ApprovalIdentity {
        ApprovalIdentity {
            generation: self.generation,
            approval_digest: self.approval_digest.clone(),
            authority_incarnation: self.authority_incarnation.clone(),
        }
    }
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    pub(crate) fn configuration(
        &self,
    ) -> Result<(
        crate::daemon_config::DaemonConfig,
        crate::daemon_config::ActivityConfig,
    )> {
        let bytes = self.daemon.bytes.as_deref().ok_or_else(GateError::stale)?;
        let doc = std::str::from_utf8(bytes).map_err(|_| GateError::stale())?;
        let path = self.daemon.path.as_deref().ok_or_else(GateError::stale)?;
        crate::daemon_config::parse_with_activity(doc, std::path::Path::new(path))
            .map_err(|error| GateError::new("invalid_daemon_config", error.to_string()))
    }

    pub(crate) fn module_entry(
        &self,
        module: &str,
    ) -> Result<crate::daemon_config::ConfiguredModule> {
        self.configuration()?
            .0
            .modules
            .into_iter()
            .find(|entry| entry.module_id == module)
            .ok_or_else(GateError::stale)
    }

    pub(crate) fn changes_entry(&self, module: &str) -> bool {
        self.changed_entries.contains(module)
    }
}

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
        if self.inner.is_poisoned() {
            return true;
        }
        self.published
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
            ApprovedDaemonConfiguration,
        )>,
    > {
        let mut state = self.lock()?;
        if state.authority_exit != Some(generation) {
            return Ok(None);
        }
        state.store.delivery_ready()?;
        let authority_incarnation = state.authority_incarnation.clone();
        let body = state.store.body()?;
        let layer = body
            .generations
            .get(&generation)
            .and_then(|entry| entry.delivery.as_ref())
            .and_then(|snapshot| snapshot.daemon.as_ref())
            .ok_or_else(|| {
                GateError::new("config_unavailable", "approved exit has no daemon snapshot")
            })?;
        let stamped = body.stamped;
        let mut approved = ApprovedDaemonConfiguration {
            generation,
            approval_digest: body.generations[&generation].generation.plan_digest.clone(),
            authority_incarnation,
            daemon: std::sync::Arc::new(layer.clone()),
            changed_entries: BTreeSet::new(),
        };
        let (config, activity) = approved.configuration()?;
        approved.changed_entries = config
            .modules
            .iter()
            .filter(|module| state.reconciled_entries.get(&module.module_id) != Some(module))
            .map(|module| module.module_id.clone())
            .collect();
        Ok(Some((config, activity, stamped, approved)))
    }

    /// Record each completed entry before waiting on the next module. A retry
    /// must not undo a runtime override made after an earlier successful step.
    pub(crate) fn daemon_entry_reconciled(
        &self,
        approved: &ApprovedDaemonConfiguration,
        module: &str,
    ) -> Result<()> {
        let mut state = self.lock()?;
        if !Self::authority_exit_matches(&state, approved) {
            return Err(GateError::stale());
        }
        let changed = if let Some(entry) = approved
            .configuration()?
            .0
            .modules
            .into_iter()
            .find(|entry| entry.module_id == module)
        {
            state
                .reconciled_entries
                .insert(module.to_owned(), entry.clone())
                != Some(entry)
        } else {
            state.reconciled_entries.remove(module).is_some()
        };
        drop(state);
        if changed {
            self.notify_delivery_changed();
        }
        Ok(())
    }

    /// Release admission only after all committed-exit reconciliation succeeds.
    pub(crate) fn complete_authority_exit(
        &self,
        approved: &ApprovedDaemonConfiguration,
    ) -> Result<()> {
        let mut state = self.lock()?;
        if !Self::authority_exit_matches(&state, approved) {
            return Err(GateError::stale());
        }
        // Actors created under an exit hold may not have been observed by the
        // coordinator yet. Normal approvals must not revive unrelated stopped
        // or failed actors whose daemon entries did not change.
        let unstamped = state.store.loaded_admission_state().0 == StoreState::Unstamped;
        if state.recovery_exit_hold || unstamped {
            let modules =
                crate::config_gate::GateSnapshot::from_store(&state.store.body()?.approved)?
                    .modules();
            state.admission_releases.extend(modules);
        }
        state.authority_exit = None;
        state.stamp_channel_cutover = false;
        state.recovery_exit_hold = false;
        drop(state);
        if unstamped {
            self.retire_unstamped_bindings();
        }
        Ok(())
    }

    fn authority_exit_matches(state: &OwnerState, approved: &ApprovedDaemonConfiguration) -> bool {
        state.authority_exit == Some(approved.generation)
            && std::sync::Arc::ptr_eq(
                &state.authority_incarnation,
                &approved.authority_incarnation,
            )
            && state
                .store
                .loaded_admission_state()
                .1
                .generations
                .get(&approved.generation)
                .is_some_and(|entry| entry.generation.plan_digest == approved.approval_digest)
    }

    /// Test helper that loads the current approved configuration before completing
    /// its exit. Production callers must carry their original approved configuration.
    #[cfg(test)]
    pub(crate) fn complete_authority_exit_for_test(&self, generation: u64) -> Result<()> {
        let approved = self
            .authority_exit_configuration(generation)?
            .ok_or_else(GateError::stale)?
            .3;
        self.complete_authority_exit(&approved)
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

    /// Remember only a refusal returned to an opted-in connection. Recovery may
    /// race the reply, in which case the connection needs its wakeup immediately.
    pub(crate) fn anchor_pending_served(&self, connection: ConnectionId) -> Result<()> {
        let mut state = self.lock()?;
        if state.declarations.contains_key(&connection) {
            if state.store.status().anchor_pending {
                state.anchor_waiters.insert(connection);
            } else {
                let inserted = state.anchor_hints.insert(connection);
                drop(state);
                if inserted {
                    self.notify_delivery_changed();
                }
            }
        }
        Ok(())
    }

    pub(crate) fn take_anchor_hint(&self, connection: ConnectionId) -> Result<Option<u64>> {
        let mut state = self.lock()?;
        if state.store.status().anchor_pending || !state.anchor_hints.remove(&connection) {
            return Ok(None);
        }
        let Some((module, _)) = state.declarations.get(&connection) else {
            return Ok(None);
        };
        let (store_state, body) = state.store.loaded_admission_state();
        if store_state == crate::config_store::StoreState::Unstamped {
            return Ok(Some(0));
        }
        Ok(Some(
            body.versions
                .iter()
                .find(|(tuple, _)| &tuple.module == module && tuple.project.is_none())
                .and_then(|(_, versions)| versions.active.or(versions.pending))
                .unwrap_or(0),
        ))
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
