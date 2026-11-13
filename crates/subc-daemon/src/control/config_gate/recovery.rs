use super::*;

impl ControlHandler {
    #[cfg(all(test, unix))]
    pub(crate) async fn reconcile_cutover_for_test(
        &self,
        owner: Arc<crate::config_gate::runtime::RuntimeOwner>,
        generation: u64,
    ) -> std::result::Result<(), GateError> {
        self.reconcile_authority_exit(owner, generation).await
    }
    #[cfg(all(test, feature = "test-support", unix))]
    pub(crate) fn with_recovery_exit_retry_pause_for_test(
        mut self,
        pause: fn(u64) -> Option<std::path::PathBuf>,
    ) -> Self {
        self.recovery_exit_test_retry_pause = Some(pause);
        self
    }
    #[cfg(all(test, feature = "test-support", unix))]
    pub(crate) fn with_recovery_exit_test_hook(
        mut self,
        hook: fn(usize) -> std::result::Result<(), GateError>,
        quiet_retry: bool,
    ) -> Self {
        self.recovery_exit_test_after_step = Some(hook);
        self.recovery_exit_test_quiet_retry = quiet_retry;
        self
    }
    pub(in crate::control) async fn recovery_request_refusal(
        &self,
        ctx: &RouteCtx,
        frame: &Frame,
        op: Option<&str>,
    ) -> Result<Option<Frame>, RouterError> {
        // List is allowed even in recovery and is polled continuously. Its
        // cached view must not wait for authority verification or publication.
        if matches!(op, Some("supervisor.list" | "config.status")) {
            return Ok(None);
        }
        let Some(owner) = self.config_runtime.clone() else {
            return Ok(None);
        };
        let recovering = if op == Some("catalog.list") {
            owner.is_recovering()
        } else {
            owner
                .on_blocking(|owner| owner.check_recovery())
                .await
                .unwrap_or(true)
        };
        if !recovering {
            return Ok(None);
        }
        let running_module = self
            .registry
            .get_module_by_connection(ctx.connection_id)
            .map_err(|error| RouterError::backend(0, frame.header.corr, error.to_string()))?
            .is_some();
        let allowed = matches!(
            op,
            Some(
                "server.describe"
                    | "supervisor.list"
                    | "daemon.shutdown_intent"
                    | "config.status"
                    | "config.unstamp"
                    | "config.stamp"
                    | "config.apply"
                    | "app.open"
                    | "activity.query"
                    | "activity.subscribe"
                    | "connection.describe"
            )
        ) || (running_module && matches!(op, Some("activity.record" | "config.get")));
        if allowed {
            return Ok(None);
        }
        control_error_frame(frame, "config_gated", "configuration store is in recovery").map(Some)
    }

    pub(in crate::control) async fn reconcile_authority_exit(
        &self,
        owner: Arc<crate::config_gate::runtime::RuntimeOwner>,
        generation: u64,
    ) -> std::result::Result<(), GateError> {
        let _operation = self.supervisor.operation_lock().lock_owned().await;
        // This snapshot read must not notify the delivery worker. A failed
        // reconciliation would consume its own wakeup and retry without backoff,
        // contending with module spawns for the configuration owner lock.
        let Some((config, activity, stamped, approved)) = owner
            .on_blocking(move |owner| owner.authority_exit_configuration(generation))
            .await?
        else {
            return Ok(());
        };
        let context = self.rescan.as_ref().ok_or_else(|| {
            GateError::new(
                "supervisor_reconcile_unavailable",
                "configuration exit requires the supervisor owner",
            )
        })?;
        // Every reserved id and prefix must be closed before the recovery hold
        // can be released, including disabled modules that have never spawned.
        for module in &config.modules {
            self.supervisor
                .apply_identity_configuration(&module.module_spec());
        }
        let configured = config
            .modules
            .iter()
            .map(|module| (module.module_id.clone(), module.enabled))
            .collect::<Vec<_>>();
        self.capability_evaluator
            .configure(configured, config.reserved_capabilities);
        self.configure_activity(activity)
            .await
            .map_err(|error| GateError::new("activity_config_failed", error))?;
        if stamped {
            // Daemon entries and module-layer activation have separate state.
            // Only the former is reconciled here; listeners wait for startup.
            for module in self.supervisor.list() {
                if config
                    .modules
                    .iter()
                    .any(|next| next.module_id == module.module_id())
                {
                    continue;
                }
                let id = module.module_id().to_owned();
                module
                    .retire_requested(Some(Principal::Direct))
                    .await
                    .map_err(|error| GateError::new("rescan_failed", error.to_string()))?;
                self.supervisor.record_rescan_removal(&id);
                self.supervisor.retire(&id);
                self.route_outages.forget(&id);
                let witness = approved.clone();
                owner
                    .on_blocking(move |owner| owner.daemon_entry_reconciled(&witness, &id))
                    .await?;
            }
            for (index, module) in config.modules.into_iter().enumerate() {
                if let Some(existing) = self.supervisor.get(&module.module_id) {
                    if approved.changes_entry(&module.module_id) {
                        existing
                            .reconcile_approved_configuration(approved.clone())
                            .await
                            .map_err(|error| GateError::new("rescan_failed", error.to_string()))?;
                    }
                } else {
                    let registered = context
                        .supervisor
                        .register_configured_with_health(
                            module.module_spec(),
                            module.enabled,
                            module.health.clone(),
                            module.drain_timeout_ms,
                            module.restart,
                            module.register_timeout,
                        )
                        .map_err(|error| GateError::new("rescan_failed", error.to_string()))?;
                    context
                        .supervisor
                        .start_registered_approved_async(registered, Principal::Direct)
                        .await
                        .map_err(|error| GateError::new("rescan_failed", error.to_string()))?;
                }
                let witness = approved.clone();
                owner
                    .on_blocking(move |owner| {
                        owner.daemon_entry_reconciled(&witness, &module.module_id)
                    })
                    .await?;
                #[cfg(test)]
                if let Some(hook) = self.recovery_exit_test_after_step {
                    hook(index + 1)?;
                }
                #[cfg(not(test))]
                let _ = index;
            }
        } else {
            self.reconcile_supervised_modules(
                &context.supervisor,
                config.modules,
                false,
                Principal::Direct,
            )
            .await
            .map_err(|error| GateError::new("rescan_failed", error))?;
        }
        self.refresh_capability_requirements();
        #[cfg(test)]
        if let Some(hook) = self.recovery_exit_test_after_step {
            hook(0)?;
        }
        if stamped && owner.stamp_channel_cutover_pending() {
            for module in self.supervisor.list() {
                let status = module
                    .status()
                    .map_err(|error| GateError::new("config_unavailable", error.to_string()))?;
                let parked = !status.process_alive
                    && matches!(
                        status.state,
                        crate::supervise::ModuleState::Stopped
                            | crate::supervise::ModuleState::Failed
                            | crate::supervise::ModuleState::Disabled
                    );
                if !(parked || (status.process_alive && !status.legacy_registration_spawn)) {
                    return Ok(());
                }
            }
        }
        // complete_authority_exit releases admission and clears the pending
        // recovery-exit marker. Earlier failure leaves both the hold and marker
        // intact for the configuration-delivery coordinator to retry.
        config_work(owner, move |owner| {
            owner.complete_authority_exit(generation)
        })
        .await?;
        Ok(())
    }
}
