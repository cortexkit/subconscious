use super::*;

impl ControlHandler {
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
        let Some(owner) = self.config_runtime.clone() else {
            return Ok(None);
        };
        let recovering = config_work(owner, |owner| owner.check_recovery())
            .await
            .unwrap_or(true);
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
        let Some((config, activity, stamped)) = config_work(owner.clone(), move |owner| {
            owner.authority_exit_configuration(generation)
        })
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
            // Existing module processes receive approved configuration changes
            // through the configuration-delivery worker, not file rescans.
            // Listener changes remain inactive until the next daemon start.
            for (index, module) in config.modules.into_iter().enumerate() {
                if self.supervisor.get(&module.module_id).is_none() {
                    context
                        .supervisor
                        .supervise_configured_requested_async(
                            module.module_spec(),
                            module.enabled,
                            module.health,
                            module.drain_timeout_ms,
                            module.restart,
                            module.register_timeout,
                            Some(Principal::Direct),
                        )
                        .await
                        .map_err(|error| GateError::new("rescan_failed", error.to_string()))?;
                }
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
