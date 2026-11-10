use super::*;
use crate::daemon_config::{ConfiguredModule, DaemonConfig};

type Differences = Vec<serde_json::Value>;

pub(in crate::control) fn preview_approved_daemon(
    current: &DaemonConfig,
    proposed: &[ConfiguredModule],
) -> SupervisorRescanResult {
    let mut result = SupervisorRescanResult {
        added: vec![],
        removed: vec![],
        changed_pending_reload: vec![],
        enabled_changes: vec![],
        unchanged: 0,
        preview: true,
        restart_required: vec![],
        capability_warnings: vec![],
        module_actions: vec![],
        earlier_pending: vec![],
        expect: None,
    };
    for next in proposed {
        let Some(old) = current
            .modules
            .iter()
            .find(|old| old.module_id == next.module_id)
        else {
            result.added.push(next.module_id.clone());
            continue;
        };
        let launch_changed = old.module_spec() != next.module_spec();
        let enabled_changed = old.enabled != next.enabled;
        if launch_changed {
            result.changed_pending_reload.push(next.module_id.clone());
        }
        if enabled_changed {
            result.enabled_changes.push(next.module_id.clone());
        }
        if !launch_changed && !enabled_changed && old.health == next.health {
            result.unchanged += 1;
        }
    }
    result.removed = current
        .modules
        .iter()
        .filter(|old| !proposed.iter().any(|next| old.module_id == next.module_id))
        .map(|old| old.module_id.clone())
        .collect();
    result
}

fn difference<T: std::fmt::Debug + PartialEq>(
    rows: &mut Differences,
    module: Option<&str>,
    entry: &str,
    in_force: &T,
    approved: &T,
) {
    if in_force != approved {
        // Configuration can contain credentials in launch arguments or env.
        // Diagnostic fingerprints separate the two views without exposing bytes.
        rows.push(serde_json::json!({
            "module_id": module, "entry": entry, "waiting_for": "daemon_restart",
            "in_force": {"digest": subc_config::raw_digest(format!("{in_force:?}").as_bytes())},
            "approved": {"digest": subc_config::raw_digest(format!("{approved:?}").as_bytes())},
        }));
    }
}

fn launch_differences(
    rows: &mut Differences,
    module: &str,
    current: &crate::supervise::ModuleSpec,
    approved: &crate::supervise::ModuleSpec,
) {
    macro_rules! field {
        ($($name:ident),+ $(,)?) => {$(
            difference(rows, Some(module), stringify!($name), &current.$name, &approved.$name);
        )+};
    }
    field!(
        program,
        args,
        env,
        reserved,
        reserved_prefixes,
        protocol,
        overlap,
        carry_containment
    );
}

fn module_differences(
    rows: &mut Differences,
    current: &ConfiguredModule,
    approved: &ConfiguredModule,
) {
    let module = Some(approved.module_id.as_str());
    launch_differences(
        rows,
        &approved.module_id,
        &current.module_spec(),
        &approved.module_spec(),
    );
    difference(rows, module, "enabled", &current.enabled, &approved.enabled);
    difference(rows, module, "health", &current.health, &approved.health);
    difference(rows, module, "restart", &current.restart, &approved.restart);
    difference(
        rows,
        module,
        "drain_timeout_ms",
        &current.drain_timeout_ms,
        &approved.drain_timeout_ms,
    );
    difference(
        rows,
        module,
        "register_timeout_s",
        &current.register_timeout,
        &approved.register_timeout,
    );
    difference(
        rows,
        module,
        "route_bind_relay_timeout_ms",
        &current.route_bind_relay_timeout_ms,
        &approved.route_bind_relay_timeout_ms,
    );
}

fn config_differences(current: &DaemonConfig, approved: &DaemonConfig) -> Differences {
    let mut rows = vec![];
    macro_rules! field {
        ($($name:ident),+ $(,)?) => {$(
            difference(&mut rows, None, stringify!($name), &current.$name, &approved.$name);
        )+};
    }
    field!(
        port,
        storage,
        admission_facts_carrier_module_id,
        admission_facts_targets,
        scope_authority_owners,
        privileged_operator_prompt,
        reserved_capabilities,
        route_bind_relay_timeout_ms
    );
    for next in &approved.modules {
        if let Some(old) = current
            .modules
            .iter()
            .find(|old| old.module_id == next.module_id)
        {
            module_differences(&mut rows, old, next);
        } else {
            difference(&mut rows, Some(&next.module_id), "module", &false, &true);
        }
    }
    for old in &current.modules {
        if !approved
            .modules
            .iter()
            .any(|next| next.module_id == old.module_id)
        {
            difference(&mut rows, Some(&old.module_id), "module", &true, &false);
        }
    }
    rows
}

impl ControlHandler {
    fn inactive_daemon_configuration(
        &self,
        approved: &DaemonConfig,
    ) -> Result<Differences, GateError> {
        let mut rows = vec![];
        difference(
            &mut rows,
            None,
            "reserved_capabilities",
            &self.capability_evaluator.reserved_capabilities(),
            &approved.reserved_capabilities,
        );
        difference(
            &mut rows,
            None,
            "route_bind_relay_timeout_ms",
            &self.route_bind_relay_timeout,
            &approved
                .route_bind_relay_timeout_ms
                .map(Duration::from_millis)
                .unwrap_or(DEFAULT_ROUTE_BIND_RELAY_TIMEOUT),
        );
        if let Some(context) = &self.rescan {
            macro_rules! field {
                ($($label:literal => $current:ident => $next:ident),+ $(,)?) => {$(
                    difference(&mut rows, None, $label, &context.$current, &approved.$next);
                )+};
            }
            field!("port" => configured_port => port, "storage" => storage_config => storage,
                "admission_facts_carrier_module_id" => admission_facts_carrier_module_id => admission_facts_carrier_module_id,
                "admission_facts_targets" => admission_facts_targets => admission_facts_targets,
                "scope_authority_owners" => scope_authority_owners => scope_authority_owners,
                "privileged_operator_prompt" => privileged_operator_prompt => privileged_operator_prompt);
        }
        for next in &approved.modules {
            let Some(module) = self.supervisor.get(&next.module_id) else {
                difference(&mut rows, Some(&next.module_id), "module", &false, &true);
                continue;
            };
            let (spec, health) = module
                .configuration()
                .map_err(|e| GateError::new("config_unavailable", e.to_string()))?;
            let status = module
                .status()
                .map_err(|e| GateError::new("config_unavailable", e.to_string()))?;
            launch_differences(&mut rows, &next.module_id, &spec, &next.module_spec());
            let id = Some(next.module_id.as_str());
            if spec == next.module_spec()
                && status.process_alive
                && status.launch_spec_digest.as_ref().is_some_and(|digest| {
                    *digest != crate::supervise::LaunchSpecDigest::from_spec(&spec)
                })
            {
                rows.push(serde_json::json!({"module_id":id,"entry":"launch_spec","waiting_for":"next_spawn",
                    "in_force":"previous_spawn","approved":"supervisor_configuration"}));
            }
            let configured_enabled = module
                .configured_enabled()
                .map_err(|e| GateError::new("config_unavailable", e.to_string()))?;
            difference(&mut rows, id, "enabled", &configured_enabled, &next.enabled);
            difference(&mut rows, id, "health", &health, &next.health);
            let register_timeout = module
                .configured_register_timeout()
                .map_err(|e| GateError::new("config_unavailable", e.to_string()))?;
            difference(
                &mut rows,
                id,
                "register_timeout_s",
                &register_timeout,
                &next.register_timeout,
            );
            difference(
                &mut rows,
                id,
                "route_bind_relay_timeout_ms",
                &self.route_bind_relay_timeout_for(&next.module_id),
                &next
                    .route_bind_relay_timeout_ms
                    .map(Duration::from_millis)
                    .unwrap_or(DEFAULT_ROUTE_BIND_RELAY_TIMEOUT),
            );
            let restart = crate::supervise::RestartPolicy {
                max_restarts: status.max_restarts,
                window: status.restart_window,
                backoff: status.restart_backoff,
                max_backoff: status.restart_max_backoff,
            };
            difference(&mut rows, id, "restart", &restart, &next.restart);
            difference(
                &mut rows,
                id,
                "drain_timeout_ms",
                &status.drain_timeout,
                &next
                    .drain_timeout_ms
                    .map(Duration::from_millis)
                    .unwrap_or(crate::supervise::DEFAULT_DRAIN_TIMEOUT),
            );
        }
        for module in self.supervisor.list() {
            if !approved
                .modules
                .iter()
                .any(|next| next.module_id == module.module_id())
            {
                difference(&mut rows, Some(module.module_id()), "module", &true, &false);
            }
        }
        Ok(rows)
    }

    pub(super) async fn report_daemon_configuration(
        &self,
        owner: &Arc<crate::config_gate::runtime::RuntimeOwner>,
        status: &mut subc_control::config::ConfigStatus,
    ) -> Result<(), GateError> {
        if status.state != "stamped" || status.anchor_pending {
            return Ok(());
        }
        if let Some(approved) =
            config_work(owner.clone(), |owner| owner.approved_daemon_configuration()).await?
        {
            let mut rows = self.inactive_daemon_configuration(&approved)?;
            for row in &mut rows {
                row["generation"] = serde_json::json!(status
                    .approval_channels
                    .last()
                    .map(|record| record.generation));
            }
            status.approved_but_inactive.extend(rows);
        }
        Ok(())
    }

    pub(super) async fn daemon_restart_required(
        &self,
        owner: &Arc<crate::config_gate::runtime::RuntimeOwner>,
        generation: u64,
    ) -> Result<bool, GateError> {
        let configs = config_work(owner.clone(), move |owner| {
            owner.generation_daemon_configurations(generation)
        })
        .await?;
        let Some((previous, approved)) = configs else {
            return Ok(false);
        };
        let changed = config_differences(&previous, &approved);
        let inactive = self.inactive_daemon_configuration(&approved)?;
        Ok(changed.iter().any(|change| {
            inactive.iter().any(|row| {
                row["waiting_for"] == "daemon_restart"
                    && row["module_id"] == change["module_id"]
                    && row["entry"] == change["entry"]
            })
        }))
    }
}
