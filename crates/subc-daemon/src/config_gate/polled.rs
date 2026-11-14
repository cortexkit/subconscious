//! Polling clones a verified memory projection; only owner work refreshes it.
use super::*;

#[derive(Clone, Default)]
pub(super) struct Projection {
    pub status: subc_control::config::ConfigStatus,
    pub phone: Option<(ConnectionId, crate::daemon_config::ConfiguredModule)>,
    pub config_declarations: BTreeMap<String, ConfigDeclareRequest>,
    pub approved: super::super::GateSnapshot,
    pub channel_cutover_pending: bool,
}

impl RuntimeOwner {
    pub(crate) fn stamp_channel_cutover_pending(&self) -> bool {
        self.published
            .read()
            .map(|view| view.projection.channel_cutover_pending)
            .unwrap_or(true)
    }
    #[cfg(test)]
    pub(crate) fn polled_status(&self) -> Result<subc_control::config::ConfigStatus> {
        let snapshot = self.polled_snapshot()?;
        self.polled_status_from(snapshot.as_ref())
    }

    pub(crate) fn polled_status_from(
        &self,
        snapshot: &Published,
    ) -> Result<subc_control::config::ConfigStatus> {
        if self.inner.is_poisoned() {
            return Err(GateError::new(
                "config_unavailable",
                "configuration owner lock is poisoned",
            ));
        }
        let view = &snapshot.projection;
        let mut status = view.status.clone();
        if matches!(status.state.as_str(), "stamped" | "unstamped") && !status.anchor_pending {
            let candidate = self.capturer.capture_polled(&view.config_declarations)?;
            if status.state == "stamped" {
                let changed = candidate
                    .changed_keys(&view.approved)
                    .iter()
                    .any(|key| !key.is_program());
                // A config-only read cannot disprove a recorded program change.
                status.pending_proposals = status.pending_proposals.max(u64::from(changed));
            } else {
                status_inputs::report_snapshot(&candidate, &mut status);
            }
        }
        Ok(status)
    }

    pub(crate) fn registration_declaration(&self, module: &str) -> Option<String> {
        self.polled_snapshot()
            .ok()?
            .registration_declaration(module)
    }

    /// A census reads one bounded regular file only when no approved store is in
    /// force. The caller runs this method on a blocking worker once per request.
    pub(crate) fn registration_declarations(&self) -> Result<BTreeMap<String, String>> {
        let snapshot = self.polled_snapshot()?;
        if let Some(declarations) = snapshot.admission.registration_declarations() {
            return Ok(declarations);
        }
        if !matches!(snapshot.approved_daemon, ApprovedDaemonView::NotStamped) {
            return Err(GateError::new(
                "config_unavailable",
                "approved declarations are unavailable",
            ));
        }
        #[cfg(test)]
        if let Some(probe) = self.census_probe.lock().unwrap().clone() {
            probe();
        }
        let candidate = self.capturer.capture_polled(&BTreeMap::new())?;
        if candidate
            .inputs
            .get(&super::super::InputKey::Daemon)
            .is_none_or(|input| !matches!(input.state, subc_config::InputState::Present { .. }))
        {
            return Err(GateError::new(
                "config_unavailable",
                "daemon configuration is unavailable",
            ));
        }
        Ok(candidate
            .modules()
            .into_iter()
            .map(|module| {
                let declaration =
                    super::super::registration::declaration(&candidate, &module).into();
                (module, declaration)
            })
            .collect())
    }

    pub(crate) fn polled_phone_entry(
        &self,
        connection: ConnectionId,
    ) -> Option<crate::daemon_config::ConfiguredModule> {
        let snapshot = self.polled_snapshot().ok()?;
        self.polled_phone_entry_from(snapshot.as_ref(), connection)
    }

    pub(crate) fn polled_phone_entry_from(
        &self,
        snapshot: &Published,
        connection: ConnectionId,
    ) -> Option<crate::daemon_config::ConfiguredModule> {
        if self.phone_is_withdrawing(connection) {
            return None;
        }
        snapshot
            .projection
            .phone
            .as_ref()
            .filter(|(id, _)| *id == connection)
            .map(|(_, entry)| entry.clone())
    }

    pub(super) fn status_projection(
        &self,
        state: &mut OwnerState,
        mut snapshot: Option<super::super::GateSnapshot>,
        admission: &admission::AdmissionCache,
    ) -> Projection {
        let store = state.store.loaded_status();
        let mut status = subc_control::config::ConfigStatus::default();
        status.state = match store.state {
            StoreState::Stamped => "stamped",
            StoreState::Unstamped => "unstamped",
            StoreState::StoreMissing => "store_missing",
            StoreState::StoreUnverified => "store_unverified",
        }
        .into();
        status.integrity = match store.integrity {
            crate::config_store::IntegrityMode::DigestOnly => "digest_only",
            crate::config_store::IntegrityMode::Sealed => "sealed",
        }
        .into();
        status.revision = store.revision;
        status.base_revision = store.base_revision;
        status.anchor_pending = store.anchor_pending;
        status.ungated_inputs = self.startup_overrides.clone();
        status.warnings = store.warnings;
        if state.recovery_exit_hold {
            // Actor admission can remain held after a restamp has replaced and
            // verified the store. That pending work does not revoke its trust.
            if store.state != StoreState::Stamped {
                status.state = "store_unverified".into();
            }
            status
                .warnings
                .push("approved recovery exit is waiting for reconciliation".into());
        }
        if state.stamp_channel_cutover {
            // The stamped snapshot already governs admission. Keep the warning
            // until processes launched with nonce-based registration have stopped
            // and their replacements have started or been parked by admission.
            status
                .warnings
                .push("approved stamp is waiting for registration channel cutover".into());
        }
        #[cfg(feature = "test-support")]
        if state.store.uses_config_gate_test_seam() {
            status
                .development_switches
                .push("SUBC_TEST_CONFIG_GATE".into());
        }
        if cfg!(feature = "dev-desktop-approval") {
            status
                .development_switches
                .push("dev-desktop-approval".into());
        }
        if cfg!(feature = "dev-placement") {
            status.development_switches.push("dev-placement".into());
        }
        if matches!(store.state, StoreState::Stamped | StoreState::Unstamped) {
            let (_, body) = state.store.loaded_admission_state();
            if body.stamped {
                snapshot = super::super::GateSnapshot::from_store(&body.approved).ok();
            }
            for entry in body.generations.values().rev().take_while(|entry| {
                matches!(
                    entry.kind,
                    crate::config_store::CommitKind::Development
                        | crate::config_store::CommitKind::DeclarationApproval
                )
            }) {
                if entry.kind != crate::config_store::CommitKind::Development {
                    continue;
                }
                status.warnings.push(format!(
                    "development generation {}: development placement (no approval)",
                    entry.generation.id
                ));
            }
            status.approval_channels = body
                .generations
                .iter()
                .map(|(id, entry)| {
                    subc_control::config::ConfigApprovalRecord::new(
                        *id,
                        entry.generation.approval_channel,
                    )
                })
                .chain(body.pruned.iter().map(|(id, entry)| {
                    subc_control::config::ConfigApprovalRecord::new(*id, entry.approval_channel)
                }))
                .collect();
            status
                .approval_channels
                .sort_by_key(|record| record.generation);
            status.pending_proposals = u64::from(body.proposal.is_some());
            for (tuple, versions) in &body.versions {
                if let Some(generation) = versions.pending {
                    let entry = body.generations.get(&generation);
                    let transition = entry.and_then(|entry| {
                        super::super::apply::Transition::for_module(
                            &entry.generation.changeset,
                            &tuple.module,
                        )
                        .ok()
                    });
                    status.approved_but_inactive.push(serde_json::json!({
                        "module_id": tuple.module, "project_id": tuple.project, "generation": generation,
                        "digest": entry.map(|entry| &entry.generation.plan_digest),
                        "waiting_for": match transition {
                            Some(super::super::apply::Transition::DaemonRestart) => "daemon_restart",
                            Some(super::super::apply::Transition::NextSupervision) => "next_supervision",
                            _ => "delivery",
                        }
                    }));
                }
            }
        }
        let approved = snapshot.clone().unwrap_or_default();
        if let Some(snapshot) = snapshot {
            status_inputs::report_snapshot(&snapshot, &mut status);
        }
        status.gated_modules = admission.held_modules();
        let phone = (store.state == StoreState::Stamped
            && !store.anchor_pending
            && !state.recovery_exit_hold)
            .then(|| state.active_declarations.get("callosum").copied())
            .flatten()
            .and_then(|connection| {
                self.phone_entry_from_state(state, connection)
                    .ok()
                    .flatten()
                    .map(|entry| (connection, entry))
            });
        let (_, body) = state.store.loaded_admission_state();
        let mut config_declarations = if body.stamped {
            body.generations
                .values()
                .next_back()
                .and_then(|entry| entry.delivery.as_ref())
                .map(|delivery| delivery.declarations.clone())
                .unwrap_or_default()
        } else {
            BTreeMap::new()
        };
        for (module, connection) in &state.active_declarations {
            if let Some((_, declaration)) = state.declarations.get(connection) {
                config_declarations.insert(module.clone(), declaration.clone());
            }
        }
        if store.state == StoreState::Stamped {
            // Include modules whose declarations or configuration views are
            // refused with config_gated, without refreshing the store or
            // allocating a delivery attempt.
            let declared: Vec<_> = state
                .declarations
                .iter()
                .map(|(connection, (module, _))| (*connection, module.clone()))
                .collect();
            for (connection, module) in declared {
                if self
                    .module_get_locked(
                        state,
                        connection,
                        &module,
                        subc_protocol::config::ConfigGetRequest::new(),
                        false,
                    )
                    .is_err_and(|error| error.code == "config_gated")
                {
                    status.gated_modules.push(module);
                }
            }
            status.gated_modules.sort();
            status.gated_modules.dedup();
        }
        Projection {
            status,
            phone,
            config_declarations,
            approved,
            channel_cutover_pending: state.stamp_channel_cutover,
        }
    }
}
