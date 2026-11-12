//! Polling clones a verified memory projection; only owner work refreshes it.
use super::*;

#[derive(Clone, Default)]
pub(super) struct Projection {
    pub status: subc_control::config::ConfigStatus,
    pub declarations: BTreeMap<String, String>,
    pub phone: Option<(ConnectionId, crate::daemon_config::ConfiguredModule)>,
    pub config_declarations: BTreeMap<String, ConfigDeclareRequest>,
    pub approved: super::super::GateSnapshot,
    pub channel_cutover_pending: bool,
}

impl RuntimeOwner {
    pub(crate) fn stamp_channel_cutover_pending(&self) -> bool {
        self.projection
            .read()
            .map(|view| view.channel_cutover_pending)
            .unwrap_or(true)
    }
    pub(crate) fn polled_status(&self) -> Result<subc_control::config::ConfigStatus> {
        let view = self
            .projection
            .read()
            .map(|view| view.clone())
            .map_err(|_| {
                GateError::new("config_unavailable", "status projection is unavailable")
            })?;
        let mut status = view.status;
        if matches!(status.state.as_str(), "stamped" | "unstamped") && !status.anchor_pending {
            let candidate = self.capturer.capture_polled(&view.config_declarations)?;
            if status.state == "stamped" {
                status.pending_proposals = u64::from(
                    candidate
                        .changed_keys(&view.approved)
                        .iter()
                        .any(|key| !key.is_program()),
                );
            } else {
                let coverage = std::mem::take(&mut status.interpreter_only_covered);
                status_inputs::report_snapshot(&candidate, &mut status);
                status.interpreter_only_covered = coverage;
            }
        }
        Ok(status)
    }

    pub(crate) fn registration_declaration(&self, module: &str) -> Option<String> {
        let view = self.projection.read().ok()?;
        if view.status.state == "unstamped" && !view.channel_cutover_pending {
            drop(view);
            let document = std::fs::read_to_string(&self.capturer.daemon_path).ok()?;
            let (configuration, _) =
                crate::daemon_config::parse_with_activity(&document, &self.capturer.daemon_path)
                    .ok()?;
            let entry = configuration
                .modules
                .into_iter()
                .find(|entry| entry.module_id == module)?;
            return Some(
                if entry.protocol == subc_control::ModuleProtocol::None {
                    "not_applicable"
                } else if entry.registration_channel {
                    "declared"
                } else {
                    "undeclared"
                }
                .into(),
            );
        }
        view.declarations.get(module).cloned()
    }

    pub(crate) fn polled_phone_entry(
        &self,
        connection: ConnectionId,
    ) -> Option<crate::daemon_config::ConfiguredModule> {
        if self.phone_is_withdrawing(connection) {
            return None;
        }
        self.projection
            .read()
            .ok()?
            .phone
            .as_ref()
            .filter(|(id, _)| *id == connection)
            .map(|(_, entry)| entry.clone())
    }

    pub(super) fn status_projection(
        &self,
        state: &mut OwnerState,
        mut snapshot: Option<super::super::GateSnapshot>,
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
            status.state = "store_unverified".into();
            status
                .warnings
                .push("approved recovery exit is waiting for reconciliation".into());
        } else if state.stamp_channel_cutover {
            // Durable publication precedes stopping old nonce-authorized children.
            // Do not report the gate enabled while those children still run.
            status.state = "unstamped".into();
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
            for entry in body
                .generations
                .values()
                .rev()
                .take_while(|entry| entry.kind == crate::config_store::CommitKind::Development)
            {
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
        let mut declarations = BTreeMap::new();
        let approved = snapshot.clone().unwrap_or_default();
        if let Some(snapshot) = snapshot {
            status_inputs::report_snapshot(&snapshot, &mut status);
            for module in snapshot.modules() {
                declarations.insert(
                    module.clone(),
                    super::super::registration::declaration(&snapshot, &module).into(),
                );
            }
            for (key, input) in &snapshot.inputs {
                if let (
                    super::super::InputKey::Program { module, .. },
                    subc_config::RecordedValue::Value { value },
                ) = (key, &input.value)
                {
                    if value.get("coverage").and_then(serde_json::Value::as_str)
                        == Some("interpreter only covered")
                    {
                        status
                            .warnings
                            .push(format!("{module}: interpreter only covered"));
                    }
                }
            }
        }
        status.gated_modules = admission::AdmissionCache::from_state(state).held_modules();
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
            declarations,
            phone,
            config_declarations,
            approved,
            channel_cutover_pending: state.stamp_channel_cutover,
        }
    }
}
