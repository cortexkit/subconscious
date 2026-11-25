use super::*;
use crate::supervise::resume_hello::{self, HelloToken, PreparedAck, ResumeAttempt};

/// Module-originated ops the daemon answers but does not advertise in
/// `HELLO_ACK`. Empty today; an op is served from here while the feature it
/// belongs to is incomplete, so no module is told it works before it does.
pub(super) const MODULE_TO_SUBC_UNADVERTISED_OPS: &[&str] = &[];

const MODULE_BASELINE_CONTROL_OPS: &[&str] = &["route.bind", "route.status"];

impl ControlHandler {
    /// A HELLO waits on `hold` twice between reserving its registry entry and
    /// installing its forwarding endpoint: once to announce it is parked, once
    /// to be released. Build the barrier for two parties, the test being the
    /// other one, and run the HELLO on a thread of its own.
    #[cfg(test)]
    pub(crate) fn with_hello_registry_to_forwarding_hold(
        mut self,
        hold: Arc<std::sync::Barrier>,
    ) -> Self {
        self.hello_registry_to_forwarding_hold = Some(hold);
        self
    }

    #[cfg(test)]
    fn hold_between_registry_and_forwarding(&self) {
        if let Some(hold) = &self.hello_registry_to_forwarding_hold {
            hold.wait();
            hold.wait();
        }
    }

    /// Park after the ACK is queued and forwarding is installed, until a test
    /// has read the ACK and issued a registry lookup from another connection.
    #[cfg(test)]
    fn hold_between_ack_and_publication(&self) {
        if let Some(hold) = &self.hello_ack_to_publication_hold {
            hold.wait();
            hold.wait();
        }
    }

    #[cfg(all(test, unix))]
    pub(crate) fn with_hello_before_publication_hold(
        mut self,
        hold: Arc<std::sync::Barrier>,
    ) -> Self {
        self.hello_before_publication_hold = Some(hold);
        self
    }

    #[cfg(all(test, unix))]
    pub(crate) fn with_hello_after_launch_check_hold(
        mut self,
        hold: Arc<std::sync::Barrier>,
    ) -> Self {
        self.hello_after_launch_check_hold = Some(hold);
        self
    }

    #[cfg(all(test, unix))]
    pub(crate) fn with_hello_inside_publication_hold(
        mut self,
        hold: Arc<std::sync::Barrier>,
    ) -> Self {
        self.hello_inside_publication_hold = Some(hold);
        self
    }
}

impl ControlHandler {
    // HELLO publication owns the spawn transition guard. claim.mint is allowed
    // only after HELLO acceptance, so a failed first HELLO has no bearer tokens
    // or claim-bound consumers to revoke. Remove only its unpublished registry
    // entry without re-entering primary-registration teardown.
    fn rollback_unpublished_hello(
        &self,
        connection: ConnectionId,
    ) -> Result<Vec<crate::registry::ModuleRegistration>, RegistryError> {
        self.registry.deregister_connection_with_reason(
            connection,
            RegistrationEndReason::RegistrationFailed,
        )
    }
}

impl ControlHandler {
    #[cfg(test)]
    pub(super) async fn pause_hello_ingress_for_test(&self) {
        let pause = self
            .hello_ingress_pause
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        if let Some((reached, release)) = pause {
            let _ = reached.send(());
            let _ = release.await;
        }
    }
}

impl ControlHandler {
    #[cfg(test)]
    pub(super) fn handle_hello(
        &self,
        connection_id: ConnectionId,
        egress: Option<crate::router::FrameSink>,
        frame: Frame,
    ) -> Result<Vec<Frame>, RouterError> {
        let binding = self.claims.binding(connection_id);
        let channel_spawn = self.supervisor.registration_spawn(connection_id);
        self.handle_hello_checked(connection_id, egress, frame, false, binding, channel_spawn)
    }

    pub(super) fn handle_hello_checked(
        &self,
        connection_id: ConnectionId,
        sink: Option<crate::FrameSink>,
        frame: Frame,
        wait_for_gate: bool,
        prefix_binding: Option<claims::ClaimBinding>,
        channel_spawn: Option<Arc<crate::supervise::RegistrationSpawn>>,
    ) -> Result<Vec<Frame>, RouterError> {
        let hello = serde_json::from_slice::<serde_json::Value>(&frame.body);
        let Some(value) = hello
            .as_ref()
            .ok()
            .filter(|value| resume_hello::has_resume_token(value))
        else {
            return self.register_hello(
                connection_id,
                sink,
                frame,
                wait_for_gate,
                prefix_binding,
                channel_spawn,
                hello,
                None,
            );
        };
        let refusal = || {
            control_error_frame(&frame, resume_hello::REFUSED, resume_hello::REFUSAL_MESSAGE)
                .map(|error| vec![error])
        };
        if prefix_binding.is_some() || channel_spawn.is_some() {
            return refusal();
        }
        let Some(resume) = self
            .resume_peer(connection_id)
            .and_then(|pid| self.supervisor.pending_adoptions.verify(value, pid).ok())
        else {
            return refusal();
        };
        let reply = self.register_hello(
            connection_id,
            sink,
            frame.clone(),
            wait_for_gate,
            None,
            None,
            hello,
            Some(resume),
        );
        match reply {
            Ok(replies)
                if replies
                    .iter()
                    .all(|reply| reply.header.ty != FrameType::Error) =>
            {
                Ok(replies)
            }
            _ => refusal(),
        }
    }

    fn resume_peer(&self, connection: ConnectionId) -> Option<u32> {
        if !cfg!(any(target_os = "macos", target_os = "linux"))
            || !self
                .local_connections
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .contains(&connection)
        {
            return None;
        }
        self.captured_peers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&connection)
            .and_then(|capture| capture.observed_pid())
    }

    pub(super) async fn wait_for_resume_preparation(
        &self,
        connection: ConnectionId,
        hello: &serde_json::Value,
    ) {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        if let Some(pid) = self.resume_peer(connection) {
            self.supervisor
                .pending_adoptions
                .wait_preparing(hello, pid)
                .await;
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        let _ = (connection, hello);
    }

    #[allow(clippy::too_many_arguments)]
    fn register_hello(
        &self,
        connection_id: ConnectionId,
        sink: Option<crate::FrameSink>,
        frame: Frame,
        wait_for_gate: bool,
        prefix_binding: Option<claims::ClaimBinding>,
        channel_spawn: Option<Arc<crate::supervise::RegistrationSpawn>>,
        hello_value: Result<serde_json::Value, serde_json::Error>,
        resume: Option<ResumeAttempt>,
    ) -> Result<Vec<Frame>, RouterError> {
        let resuming = resume.is_some();
        let check_spawn = |spawn: &crate::supervise::RegistrationSpawn| {
            if resuming {
                spawn.verdict()
            } else {
                spawn.check()
            }
        };
        let primary_sink = channel_spawn.as_ref().and_then(|_| sink.clone());
        #[cfg(test)]
        if prefix_binding.is_some() {
            self.claims.pause_prefix_hello_for_test(5);
            match self.claims.take_prefix_hello_failure_for_test() {
                Some("error") => {
                    return Err(RouterError::backend(
                        0,
                        frame.header.corr,
                        "injected HELLO failure",
                    ))
                }
                Some("panic") => panic!("injected HELLO task panic"),
                _ => {}
            }
        }
        if let Some(binding) = &prefix_binding {
            let value = serde_json::from_slice::<serde_json::Value>(&frame.body).ok();
            let module = value
                .as_ref()
                .and_then(|value| value["manifest"]["module_id"].as_str());
            if !binding.admit_prefix_hello(module)
                || !self.prefix_binding_live(binding, connection_id)
                || !module.is_some_and(|module| {
                    self.supervisor
                        .prefix_claim_authorized(&binding.registration.module_id, module)
                })
                || binding.registration.check().is_err()
            {
                return Ok(vec![control_error_frame(
                    &frame,
                    "claim_prefix_refused",
                    "HELLO must name the prefix claim's exact module id once",
                )?]);
            }
        }
        struct RefuseUnregisteredChannel {
            spawn: Option<Arc<crate::supervise::RegistrationSpawn>>,
            sink: Option<crate::FrameSink>,
        }
        impl Drop for RefuseUnregisteredChannel {
            fn drop(&mut self) {
                if let Some(spawn) = &self.spawn {
                    if !spawn.registered() {
                        if let Some(sink) = &self.sink {
                            sink.terminate_ingress();
                        }
                        spawn.retire_registration_channel();
                    }
                }
            }
        }
        let _channel_attempt = RefuseUnregisteredChannel {
            spawn: channel_spawn.clone(),
            sink: sink.clone(),
        };
        if let Some(spawn) = &channel_spawn {
            if let Err(code) = spawn.admit_hello(
                hello_value
                    .as_ref()
                    .ok()
                    .and_then(|value| value["manifest"]["module_id"].as_str()),
            ) {
                return Ok(vec![control_error_frame(
                    &frame,
                    code,
                    "registration channel HELLO refused",
                )?]);
            }
        }
        // A resume uses the adopted launch's checks, without the inherited
        // channel's retire-on-refusal lifetime.
        let channel_spawn =
            channel_spawn.or_else(|| resume.as_ref().map(|resume| resume.spawn.clone()));
        debug!(
            connection_id = connection_id.get(),
            corr = frame.header.corr,
            "handling HELLO"
        );
        // A module connection has one identity for its entire lifetime. A second
        // registration would leave the old registry owner behind while replacing
        // its forwarding endpoint and launch nonce.
        if self
            .registry
            .get_module_by_connection(connection_id)
            .map_err(|err| RouterError::backend(0, frame.header.corr, err.to_string()))?
            .is_some()
        {
            return Ok(vec![control_error_frame(
                &frame,
                "invalid_hello",
                "connection is already registered as a module",
            )?]);
        }
        let hello_value = match hello_value {
            Ok(value) => value,
            Err(err) => {
                return Ok(vec![control_error_frame(
                    &frame,
                    "invalid_hello",
                    format!("malformed HELLO body: {err}"),
                )?])
            }
        };
        if let Err(detail) = catalog::validate_hello_declarations(&hello_value) {
            return Ok(vec![control_error_frame(
                &frame,
                error_codes::INVALID_PROVIDER_DECLARATIONS,
                detail,
            )?]);
        }
        if let Some(module) = hello_value
            .get("manifest")
            .and_then(|value| value.get("module_id"))
            .and_then(|value| value.as_str())
        {
            let recovering = self
                .config_runtime
                .as_ref()
                .is_some_and(|owner| owner.refuses_control_in_recovery());
            // Defence in depth: RuntimeOwner::module_is_gated in
            // config_gate/admission.rs already gates every id during recovery.
            // A registration channel still never uses the nonce fallback.
            if recovering
                && channel_spawn.as_ref().is_none_or(|spawn| {
                    !self.supervisor.has_live_verified_channel_binding(spawn)
                        || check_spawn(spawn).is_err()
                })
                && !(channel_spawn.is_none()
                    && hello_value
                        .get("launch_nonce")
                        .and_then(|value| value.as_str())
                        .is_some_and(|nonce| {
                            self.supervisor.spawned_consumer_authorized(module, nonce)
                                && self
                                    .supervisor
                                    .has_live_verified_program_binding(module, nonce)
                        }))
            {
                return Ok(vec![control_error_frame(
                    &frame,
                    "config_gated",
                    "recovery admits only previously bound supervised launches",
                )?]);
            }
            if self
                .config_runtime
                .as_ref()
                .is_some_and(|owner| owner.module_is_gated(module))
                && (channel_spawn
                    .as_ref()
                    .is_none_or(|spawn| !self.supervisor.has_live_verified_channel_binding(spawn))
                    || self.config_runtime.as_ref().is_some_and(|owner| {
                        !recovering && owner.check_module_hold(module).is_err()
                    }))
            {
                return Ok(vec![control_error_frame(
                    &frame,
                    "reserved_module",
                    "a gated identity belongs only to its supervised child",
                )?]);
            }
        }
        if channel_spawn.is_none() {
            if let (Some(module), Some(nonce)) = (
                hello_value
                    .get("manifest")
                    .and_then(|value| value.get("module_id"))
                    .and_then(|value| value.as_str()),
                hello_value
                    .get("launch_nonce")
                    .and_then(|value| value.as_str()),
            ) {
                let binding = self.supervisor.has_program_binding_record(module, nonce);
                let required = self
                    .config_runtime
                    .as_ref()
                    .is_some_and(|owner| owner.module_is_gated(module));
                if let Some(error) = (binding || required)
                    .then(|| {
                        self.supervisor
                            .check_current_program_binding(module, nonce)
                            .err()
                    })
                    .flatten()
                {
                    let code = if error.code == "program_refused" {
                        "program_refused"
                    } else {
                        "config_delivery_deferred"
                    };
                    self.request_refusal_close(
                        connection_id,
                        sink.as_ref(),
                        CloseReason::new(code, error.detail.clone()),
                    );
                    return Ok(vec![control_error_frame(&frame, code, error.detail)?]);
                }
            }
        }
        if let Err(err) = validate_hello_display_name(&hello_value) {
            return Ok(vec![control_error_frame(
                &frame,
                "invalid_manifest",
                err.to_string(),
            )?]);
        }
        if let Err(err) = validate_hello_capability_grammar(&hello_value) {
            return Ok(vec![control_error_frame(
                &frame,
                "invalid_capability_grammar",
                err.to_string(),
            )?]);
        }
        if let Err(err) = validate_hello_self_signal_declarations(&hello_value) {
            return Ok(vec![control_error_frame(
                &frame,
                "invalid_manifest",
                err.to_string(),
            )?]);
        }
        if let Err(err) = validate_hello_event_declarations(&hello_value) {
            return Ok(vec![control_error_frame(
                &frame,
                "invalid_event_declaration",
                err.to_string(),
            )?]);
        }
        if let Err(err) = validate_hello_rich_ui_descriptor(&hello_value) {
            return Ok(vec![control_error_frame(
                &frame,
                "invalid_manifest",
                err.to_string(),
            )?]);
        }
        if let Some(provenance) = hello_value
            .get("manifest")
            .and_then(|manifest| manifest.get("provenance"))
        {
            if let Err(err) = serde_json::from_value::<ManifestProvenance>(provenance.clone()) {
                return Ok(vec![control_error_frame(
                    &frame,
                    "invalid_manifest",
                    format!("malformed manifest provenance: {err}"),
                )?]);
            }
        }
        if let Err(reason) = validate_hello_role_declarations(&hello_value) {
            let module_id = hello_value["manifest"]["module_id"]
                .as_str()
                .unwrap_or("<unknown>");
            warn!(module_id, %reason, "dropping whole role_declarations map from HELLO");
        }
        if let Err(reason) = validate_hello_operation_deferral(&hello_value) {
            let module_id = hello_value
                .get("manifest")
                .and_then(|m| m.get("module_id"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown");
            warn!(module_id, %reason, "dropping operation_deferral from HELLO");
        }
        if let Err(reason) = validate_hello_flow_operations(&hello_value) {
            let module_id = hello_value["manifest"]["module_id"]
                .as_str()
                .unwrap_or("<unknown>");
            warn!(module_id, %reason, "dropping flow_operations from HELLO");
        }
        if let Err(reason) = validate_hello_confirm_operations(&hello_value) {
            let module_id = hello_value["manifest"]["module_id"]
                .as_str()
                .unwrap_or("<unknown>");
            warn!(module_id, %reason, "dropping confirm_operations from HELLO");
        }
        let hello = match <ModuleHelloBody as serde::Deserialize>::deserialize(&hello_value) {
            Ok(hello) => hello,
            Err(err) => {
                return Ok(vec![control_error_frame(
                    &frame,
                    "invalid_hello",
                    format!("malformed HELLO body: {err}"),
                )?])
            }
        };

        if hello.protocol_ver != hello.manifest.protocol_ver {
            return Ok(vec![control_error_frame(
                &frame,
                "invalid_manifest",
                format!(
                    "HELLO protocol_ver {} does not match manifest protocol_ver {}",
                    hello.protocol_ver, hello.manifest.protocol_ver
                ),
            )?]);
        }

        if hello.manifest.module_id.trim().is_empty() {
            return Ok(vec![control_error_frame(
                &frame,
                "invalid_manifest",
                "manifest module_id must not be empty",
            )?]);
        }

        let negotiated_ver = match negotiate_version(hello.protocol_ver) {
            Ok(negotiated_ver) => negotiated_ver,
            Err(message) => {
                return Ok(vec![control_error_frame(
                    &frame,
                    "version_unsupported",
                    message,
                )?])
            }
        };

        // Registration and forwarding publication use the same module gate as
        // stamp, unstamp and spawning. Recheck after taking it; parsing may
        // precede those transitions. A program-refusal hold blocks future
        // spawns, but does not take this gate or revoke a HELLO whose supervised
        // child's image has already passed the check.
        let _publication_gate = if let Some(owner) = &self.config_runtime {
            let gate = owner
                .module_gate(&hello.manifest.module_id)
                .map_err(|error| RouterError::backend(0, frame.header.corr, error.to_string()))?;
            Some(if wait_for_gate {
                gate.blocking_lock_owned()
            } else {
                match gate.try_lock_owned() {
                    Ok(guard) => guard,
                    Err(_) => {
                        return Ok(vec![control_error_frame(
                            &frame,
                            "config_delivery_deferred",
                            "module admission is busy",
                        )?])
                    }
                }
            })
        } else {
            None
        };
        #[cfg(test)]
        if let Some(hold) = &self.hello_before_publication_hold {
            hold.wait();
            hold.wait();
        }
        let module = &hello.manifest.module_id;
        if !resuming
            && !self.supervisor.adoption_hello_allowed(
                module,
                channel_spawn.as_ref(),
                hello.launch_nonce.as_deref(),
            )
        {
            return Ok(vec![control_error_frame(
                &frame,
                "reserved_module",
                "module is pending adoption",
            )?]);
        }
        let required = self
            .config_runtime
            .as_ref()
            .is_some_and(|owner| owner.module_is_gated(module));
        let recovering = self
            .config_runtime
            .as_ref()
            .is_some_and(|owner| owner.is_recovering());
        // Both entry paths (handle_control -> handle_hello and
        // handle_control_frame_timed -> spawn_blocking) enter this same
        // handle_hello_checked function. Its fresh, fail-closed store check
        // precedes parsing and this publication-gate check; this cached read
        // also observes any recovery transition published during that work.
        // The nonce-connection-only fallback is defence in depth alongside
        // RuntimeOwner::module_is_gated's all-id recovery gate.
        if recovering
            && channel_spawn.as_ref().is_none_or(|spawn| {
                !self.supervisor.has_live_verified_channel_binding(spawn)
                    || check_spawn(spawn).is_err()
            })
            && !(channel_spawn.is_none()
                && hello.launch_nonce.as_deref().is_some_and(|nonce| {
                    self.supervisor.spawned_consumer_authorized(module, nonce)
                        && self
                            .supervisor
                            .has_live_verified_program_binding(module, nonce)
                }))
        {
            return Ok(vec![control_error_frame(
                &frame,
                "config_gated",
                "recovery admits only previously bound supervised launches",
            )?]);
        }
        if required
            && (channel_spawn
                .as_ref()
                .is_none_or(|spawn| !self.supervisor.has_live_verified_channel_binding(spawn))
                || self
                    .config_runtime
                    .as_ref()
                    .is_some_and(|owner| !recovering && owner.check_module_hold(module).is_err()))
        {
            return Ok(vec![control_error_frame(
                &frame,
                "reserved_module",
                "gated admission requires the current supervised child's registration channel and verified image binding",
            )?]);
        }
        if let Some(spawn) = &channel_spawn {
            if let Err(code) = check_spawn(spawn) {
                return Ok(vec![control_error_frame(
                    &frame,
                    code,
                    "registration launch is no longer live",
                )?]);
            }
        } else if let Some(nonce) = hello.launch_nonce.as_deref() {
            if required || self.supervisor.has_program_binding_record(module, nonce) {
                if let Err(error) = self.supervisor.check_current_program_binding(module, nonce) {
                    let code = if error.code == "program_refused" {
                        "program_refused"
                    } else {
                        "config_delivery_deferred"
                    };
                    self.request_refusal_close(
                        connection_id,
                        sink.as_ref(),
                        CloseReason::new(code, error.detail.clone()),
                    );
                    return Ok(vec![control_error_frame(&frame, code, error.detail)?]);
                }
            }
        }

        // Swap gate, ahead of the reserved gate on purpose. While a blue/green
        // swap is open for this id, the only HELLO admitted as a second process
        // is the one carrying the candidate's launch nonce (the swap token), and
        // it registers into the candidate slot rather than being refused as a
        // duplicate. Run after the reserved gate, a reserved module's candidate
        // would be refused `reserved_module` for presenting a nonce that gate
        // does not know. See `SupervisorHandle::swap_hello_admission`.
        let swap_admission = match &channel_spawn {
            Some(spawn) if spawn.is_candidate() => SwapHelloAdmission::Candidate,
            Some(_) => SwapHelloAdmission::NotSwapping,
            None if prefix_binding.is_some() => SwapHelloAdmission::NotSwapping,
            None => self
                .supervisor
                .swap_hello_admission(&hello.manifest.module_id, hello.launch_nonce.as_deref()),
        };
        if swap_admission == SwapHelloAdmission::ProtocolNone {
            warn!(
                module_id = %hello.manifest.module_id,
                connection_id = connection_id.get(),
                "HELLO refused: the module_id belongs to a protocol:none module, which never registers"
            );
            // The code stays `swap_token_invalid` for existing callers; only the
            // explanation names the actual reason.
            return Ok(vec![control_error_frame(
                &frame,
                "swap_token_invalid",
                format!(
                    "module_id '{}' is a protocol:none module; a protocol:none module may not register",
                    hello.manifest.module_id
                ),
            )?]);
        }
        if swap_admission == SwapHelloAdmission::ChannelRequired {
            warn!(
                module_id = %hello.manifest.module_id,
                connection_id = connection_id.get(),
                "HELLO refused: the module_id is gated and the HELLO did not arrive over its registration channel"
            );
            // The code stays `swap_token_invalid` for existing callers; only the
            // explanation names the actual reason.
            return Ok(vec![control_error_frame(
                &frame,
                "swap_token_invalid",
                format!(
                    "module_id '{}' is gated; it must register over its supervised registration channel",
                    hello.manifest.module_id
                ),
            )?]);
        }
        if swap_admission == SwapHelloAdmission::Refused {
            warn!(
                module_id = %hello.manifest.module_id,
                connection_id = connection_id.get(),
                "HELLO refused: a swap is open for this module_id and the launch nonce is not one the supervisor minted for it"
            );
            return Ok(vec![control_error_frame(
                &frame,
                "swap_token_invalid",
                format!(
                    "module_id '{}' is being swapped; HELLO without the swap candidate's launch nonce is rejected",
                    hello.manifest.module_id
                ),
            )?]);
        }
        let swap_candidate = swap_admission == SwapHelloAdmission::Candidate;

        // A swap candidate proves its identity with the candidate nonce above.
        // Other registrations receive this catalog fact only when reserved-module
        // policy required and accepted a nonce; unreserved registrations require
        // no nonce check.
        let verification = if swap_candidate && channel_spawn.is_none() {
            ReservedHelloVerification {
                nonce_verified: true,
                prefix_owner: None,
            }
        } else {
            let verification = if let Some(binding) = &prefix_binding {
                if !self.supervisor.prefix_claim_authorized(
                    &binding.registration.module_id,
                    &hello.manifest.module_id,
                ) {
                    return Ok(vec![control_error_frame(
                        &frame,
                        "claim_prefix_refused",
                        "prefix ownership changed",
                    )?]);
                }
                Ok(ReservedHelloVerification {
                    nonce_verified: true,
                    prefix_owner: Some(binding.registration.module_id.clone()),
                })
            } else if channel_spawn.is_some() {
                self.supervisor
                    .channel_hello_verification(&hello.manifest.module_id)
            } else {
                self.supervisor.reserved_hello_verification(
                    &hello.manifest.module_id,
                    hello.launch_nonce.as_deref(),
                )
            };
            match verification {
                Ok(verified) => verified,
                Err(rejection) => {
                    let message = match rejection {
                        ReservedHelloRejection::Exact { module_id } => format!(
                            "module_id '{module_id}' is reserved; HELLO without a valid launch nonce is rejected"
                        ),
                        ReservedHelloRejection::Prefix {
                            prefix,
                            owner_module_id,
                        } => format!(
                            "module_id '{}' matches reserved prefix '{prefix}' owned by '{owner_module_id}'; HELLO without the owner launch nonce is rejected",
                            hello.manifest.module_id
                        ),
                    };
                    return Ok(vec![control_error_frame(
                        &frame,
                        "reserved_module",
                        message,
                    )?]);
                }
            }
        };

        let reserved_capability_refusals = self.capability_evaluator.reserved_hello_refusals(
            &hello.manifest.module_id,
            hello.manifest.capabilities.as_ref(),
        );
        if let Some(refusal) = reserved_capability_refusals.first() {
            let capability = refusal.capability.clone();
            let bound_module = refusal.claimants[0].clone();
            log_duplicate_claim_events(reserved_capability_refusals);
            return Ok(vec![control_error_frame(
                &frame,
                "reserved_capability",
                format!(
                    "capability '{}' is reserved for module_id '{}'; claimant '{}' was refused",
                    capability, bound_module, hello.manifest.module_id
                ),
            )?]);
        }

        // A connection that already opened client routes must not also register as
        // a module: cleanup would then release only one side and leak the other.
        if self
            .forwarding
            .connection_has_client_routes(connection_id)
            .map_err(RouterError::Forwarding)?
        {
            return Ok(vec![control_error_frame(
                &frame,
                "invalid_hello",
                "connection has open client routes and cannot also register as a module",
            )?]);
        }

        // Kept for scope sync authority, which goes only to the connection that
        // presented the module's current launch nonce. Recorded before the
        // registration is attempted: a connection whose registration then fails
        // has no registration, so it cannot sync anyway, and cleanup forgets it.
        // A registration channel's authority is the channel itself, never a
        // nonce it also presented, and a prefix registration's is its claim:
        // neither records one, so neither can fall back to nonce authority.
        self.hello_launch_nonces
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .record(
                connection_id,
                if prefix_binding.is_some() || channel_spawn.is_some() {
                    None
                } else {
                    hello.launch_nonce.as_deref()
                },
            );
        let control_ops = effective_module_control_ops(hello.control_ops);
        let prefix_submodule = verification.prefix_owner.is_some();
        let nonce_spawn = (!prefix_submodule && channel_spawn.is_none())
            .then(|| {
                hello
                    .launch_nonce
                    .as_deref()
                    .and_then(|nonce| self.supervisor.nonce_hello_spawn(module, nonce))
            })
            .flatten();
        let token = match channel_spawn
            .as_ref()
            .or(nonce_spawn.as_ref())
            .map(|spawn| {
                HelloToken::new(
                    spawn,
                    connection_id,
                    resume,
                    sink.clone(),
                    self.config_runtime.as_deref(),
                    &hello_value,
                    nonce_spawn.as_ref().and(hello.launch_nonce.as_deref()),
                )
            })
            .transpose()
        {
            Ok(token) => token.flatten(),
            Err(code) => {
                return Ok(vec![control_error_frame(
                    &frame,
                    code,
                    "module token registration refused",
                )?])
            }
        };
        // Encoding precedes insertion; minting waits for duplicate, publication,
        // and ACK egress checks below.
        let hello_ack = PreparedAck::prepare(
            |token| self.build_hello_ack_with_token(&frame, negotiated_ver, module, token),
            token,
        )?;
        #[cfg(test)]
        struct PublicationFinished(Arc<std::sync::Barrier>);
        #[cfg(test)]
        impl Drop for PublicationFinished {
            fn drop(&mut self) {
                self.0.wait();
            }
        }
        #[cfg(test)]
        let _publication_finished = self.hello_after_launch_check_hold.as_ref().map(|hold| {
            hold.wait();
            hold.wait();
            PublicationFinished(hold.clone())
        });
        let publication_spawn = channel_spawn
            .as_ref()
            .or_else(|| prefix_binding.as_ref().map(|binding| &binding.registration));
        #[cfg(test)]
        if prefix_binding.is_some() {
            self.claims.pause_prefix_hello_for_test(7);
        }
        let _registration_publication = match publication_spawn {
            Some(spawn) => Some(match spawn.begin_publication() {
                Ok(guard) => guard,
                Err(code) => {
                    return Ok(vec![control_error_frame(
                        &frame,
                        code,
                        "registration ended before HELLO publication",
                    )?])
                }
            }),
            None => None,
        };
        let (_prefix_live, _prefix_ownership) = if let Some(binding) = &prefix_binding {
            let live = self.claims.lock_prefix_binding(binding, connection_id);
            let ownership = self.supervisor.prefix_claim_publication(
                &binding.registration.module_id,
                &hello.manifest.module_id,
            );
            if live.is_none()
                || ownership.is_none()
                || !self.claim_primary_registered(binding.spawn, &binding.registration)
                || binding.prefix_module_id() != Some(hello.manifest.module_id.as_str())
            {
                return Ok(vec![control_error_frame(
                    &frame,
                    "claim_prefix_refused",
                    "prefix authority ended before publication",
                )?]);
            }
            (live, ownership)
        } else {
            (None, None)
        };
        if let Some(spawn) = publication_spawn {
            self.forwarding
                .bind_spawn(connection_id, spawn)
                .map_err(RouterError::Forwarding)?;
        }
        // The spawn whose own registration this HELLO is, for `target` on
        // the route.open reply: its channel, or the launch its nonce was
        // minted for. A prefix child's HELLO names no spawn of its own. It is
        // only looked up here. The connection is bound to it and its
        // requested declarations are frozen once the registry has accepted
        // this HELLO (`admit_hello_authority`), so a HELLO refused as a
        // duplicate, or for any other reason, leaves the spawn untouched: its
        // closing connection is then no registration of the spawn and ends
        // none of the spawn's routes.
        let hello_authority = match (&channel_spawn, &prefix_binding) {
            (Some(spawn), _) => Some(spawn.authority.clone()),
            (None, None) => nonce_spawn.as_ref().map(|spawn| spawn.authority.clone()),
            (None, Some(_)) => None,
        }
        .map(|record| HelloAuthority {
            record,
            requested: hello
                .manifest
                .provider_declarations
                .clone()
                .unwrap_or_default(),
            froze: Default::default(),
        });
        #[cfg(test)]
        if prefix_binding.is_some() {
            self.claims.pause_prefix_hello_for_test(8);
        }
        if swap_candidate {
            let module_id = hello.manifest.module_id.clone();
            let mut differing_declarations = Default::default();
            let reply = self.register_swap_candidate(
                connection_id,
                sink,
                &frame,
                hello.manifest,
                negotiated_ver,
                control_ops,
                hello_ack,
                hello_authority.as_ref(),
                &mut differing_declarations,
                #[cfg(windows)]
                hello.launch_nonce.as_deref(),
            )?;
            if let Some(spawn) = &channel_spawn {
                if self
                    .registry
                    .get_module_by_connection(connection_id)
                    .ok()
                    .flatten()
                    .is_some()
                {
                    if let Some(sink) = primary_sink {
                        spawn.bind_primary_sink(sink);
                    }
                    spawn.hello_accepted();
                }
            } else if self
                .registry
                .get_module_by_connection(connection_id)
                .ok()
                .flatten()
                .is_some()
            {
                self.supervisor
                    .retire_unused_registration_channel(&module_id, true);
            }
            if self
                .registry
                .get_module_by_connection(connection_id)
                .ok()
                .flatten()
                .is_some()
            {
                self.file_development_widening(
                    hello_authority.map(|authority| authority.record),
                    differing_declarations,
                );
            }
            return Ok(reply);
        }
        // Registered unpublished: the module id is claimed, so a duplicate HELLO
        // is refused, but `catalog.list` does not show the module and the
        // catalog generation does not move until it is published below, once
        // its forwarding endpoint exists. Listing it now would advertise a
        // module that `route.open` still refuses for having no forwarding
        // connection.
        let registration = match self
            .registry
            .register_unpublished_verified_with_control_ops(
                hello.manifest,
                negotiated_ver,
                connection_id,
                control_ops,
                verification.nonce_verified,
            ) {
            Ok(registration) => registration,
            Err(RegistryError::DuplicateModuleId { module_id }) => {
                return Ok(vec![control_error_frame(
                    &frame,
                    "duplicate_module_id",
                    format!(
                        "module_id '{module_id}' is already registered; duplicate HELLO rejected"
                    ),
                )?])
            }
            Err(err @ RegistryError::PathHazardModuleId { .. }) => {
                return Ok(vec![control_error_frame(
                    &frame,
                    "invalid_module_id",
                    err.to_string(),
                )?])
            }
            Err(err) => {
                return Ok(vec![control_error_frame(
                    &frame,
                    "registry_error",
                    err.to_string(),
                )?])
            }
        };
        let differing_declarations =
            match self.admit_hello_authority(connection_id, hello_authority.as_ref()) {
                Ok(differing) => differing,
                Err(err) => {
                    if matches!(
                        self.rollback_unpublished_hello(connection_id),
                        Ok(r) if !r.is_empty()
                    ) {
                        crate::supervise::notify_registration_release();
                    }
                    return Err(RouterError::Forwarding(err));
                }
            };
        #[cfg(test)]
        self.hold_between_registry_and_forwarding();

        let mut reply = Vec::new();
        // Registry readers must wait from before the ACK can be read until
        // publication. Nest forwarding inside the registry lock, never reverse.
        let publication = self
            .registry
            .publish_registration_after(connection_id, || {
                if let Some(sink) = sink {
                    // The forwarding table's module store is also the daemon-to-module
                    // control-RPC lane, so every HELLO gets a live endpoint even when the
                    // manifest has no routable provider role. Non-routable modules still
                    // cannot receive route.bind in production: `handle_route_open` checks
                    // the registry manifest with `target_has_required_role` before the
                    // only production call to `begin_route_bind_relay_for` below that
                    // route.open path. The remaining direct relay callers are unit tests
                    // and benchmark harnesses that construct forwarding state explicitly.
                    //
                    // The HELLO_ACK is queued by the forwarding table itself, before the
                    // endpoint becomes visible, and is NOT returned as a reply. A module
                    // reads HELLO_ACK first and exits on anything else; a reply is only
                    // written after this handler returns, by which time a route.open on
                    // another connection could already have queued a route.bind request
                    // for this module ahead of it.
                    let concurrency = manifest_concurrency(&registration.manifest);
                    self.forwarding.register_module_connection_acked(
                        connection_id,
                        registration.manifest.module_id.clone(),
                        negotiated_ver,
                        concurrency,
                        sink,
                        crate::forwarding::ModuleHelloAck {
                            frame: hello_ack,
                            verified_prefix_owner: verification.prefix_owner,
                        },
                    )?;
                } else {
                    // No sink means no forwarding endpoint, so nothing can be routed
                    // ahead of the ack; it goes out as the reply.
                    reply.push(
                        hello_ack
                            .finish()
                            .map_err(ForwardingError::HelloRegistrationRefused)?,
                    );
                }
                #[cfg(test)]
                self.hold_between_ack_and_publication();
                Ok(())
            })
            .map_err(|err| RouterError::backend(0, frame.header.corr, err.to_string()))?;
        let published = match publication {
            Ok(published) => published,
            Err(err) => {
                // The registry lock has been released, so rollback can take it
                // again. No forwarding endpoint or route was published. Keep
                // forwarding's weak connection-to-spawn association until the
                // connection's read loop ends and removes its remaining routes.
                // Removing this unpublished entry does not advance the catalog
                // generation.
                if matches!(
                    self.rollback_unpublished_hello(connection_id),
                    Ok(r) if !r.is_empty()
                ) {
                    crate::supervise::notify_registration_release();
                }
                self.refuse_hello_authority(connection_id, hello_authority.as_ref());
                return Ok(vec![control_error_frame(
                    &frame,
                    if matches!(err, ForwardingError::ConnectionRoleConflict { .. }) {
                        "invalid_hello"
                    } else {
                        forwarding_error_code(&err)
                    },
                    err.to_string(),
                )?]);
            }
        };
        if !published {
            debug!(
                module_id = %registration.manifest.module_id,
                connection_id = connection_id.get(),
                "registration was removed before it could be published"
            );
            self.refuse_hello_authority(connection_id, hello_authority.as_ref());
            return Ok(reply);
        }

        drop(_prefix_ownership);
        drop(_prefix_live);

        // Exposure over assumption: Concurrency's serde default is pinned to the
        // pre-field behavior (ModuleManaged), so a management surface that is
        // genuinely Serial and just never declared it inherits concurrent
        // delivery silently. Logging which registrations RESOLVED BY DEFAULT
        // turns "no module has been bitten yet" into the checkable claim "no
        // module is exposed" -- one read of the boot log instead of a fleet
        // audit. Detected from the raw HELLO bytes because the serde default
        // deliberately erases the absent/declared distinction from the type.
        if manifest_concurrency_was_defaulted(&frame.body, &registration.manifest) {
            info!(
                module_id = %registration.manifest.module_id,
                "management surface registered with DEFAULTED concurrency=module_managed (manifest predates the field; declare the real lane)"
            );
        }
        if registration.manifest.registration_channel == Some(false)
            && self
                .supervisor
                .module_is_gated(&registration.manifest.module_id)
            && self
                .config_runtime
                .as_ref()
                .and_then(|owner| owner.registration_declaration(&registration.manifest.module_id))
                .as_deref()
                == Some("declared")
        {
            warn!(module_id = %registration.manifest.module_id,
                "HELLO registration_channel is false despite the approved channel declaration; registration remains admitted");
        }

        self.apply_registration_capabilities(&registration);

        self.supervisor
            .record_activity_registration(&registration.manifest.module_id);

        info!(
            module_id = %registration.manifest.module_id,
            module_version = %registration.manifest.module_version,
            negotiated_ver,
            routable_provider = manifest_provides_routable_role(&registration.manifest),
            connection_id = connection_id.get(),
            "module registered"
        );

        if let Some(spawn) = &channel_spawn {
            if let Some(sink) = primary_sink {
                spawn.bind_primary_sink(sink);
            }
            if !resuming {
                spawn.hello_accepted();
            }
        } else if !prefix_submodule {
            self.supervisor
                .retire_unused_registration_channel(&registration.manifest.module_id, false);
        }
        self.file_development_widening(
            hello_authority.map(|authority| authority.record),
            differing_declarations,
        );
        #[cfg(windows)]
        if let Some(nonce) = hello.launch_nonce.as_deref() {
            self.supervisor
                .program_hello_accepted(&registration.manifest.module_id, nonce);
        }

        #[cfg(test)]
        if let Some(hold) = &self.hello_inside_publication_hold {
            hold.wait();
            hold.wait();
        }

        Ok(reply)
    }

    /// Register a HELLO the swap gate admitted into the candidate slot of the
    /// registry and of forwarding, where it is reachable over its own
    /// connection (its `catalog.update` finds it) but by no by-id lookup, so
    /// nothing routes to it until the supervisor cuts over.
    ///
    /// Registry first (unpublished), then forwarding, then publication, the
    /// same order as an ordinary HELLO; a forwarding failure removes the
    /// registry entry again. The swap supervisor treats a published, ready
    /// candidate as registered and promotes it, and promotion needs the
    /// candidate's forwarding endpoint, so the candidate is published only
    /// once that endpoint exists. Unlike an ordinary HELLO, this does not
    /// recompute capability verdicts or run the capability deny census
    /// (`enforce_capability_denies`): both are about the module in the active
    /// slot, which `route.open` can target. A candidate sits in the candidate
    /// slot, so it neither provides capabilities to other modules nor can be
    /// routed to until promotion moves it into the active slot.
    #[allow(clippy::too_many_arguments)]
    fn register_swap_candidate(
        &self,
        connection_id: ConnectionId,
        sink: Option<crate::FrameSink>,
        frame: &Frame,
        manifest: ModuleManifest,
        negotiated_ver: u8,
        control_ops: Vec<String>,
        hello_ack: PreparedAck<'_>,
        hello_authority: Option<&HelloAuthority>,
        differing_declarations: &mut ProviderDeclarations,
        #[cfg(windows)] launch_nonce: Option<&str>,
    ) -> Result<Vec<Frame>, RouterError> {
        let module_id = manifest.module_id.clone();
        // Closing a swap and publishing its candidate must be ordered. The
        // spawn role proves which slot is intended, not that the slot is open.
        self.supervisor
            .with_swap_candidate_admission(&module_id, || {
                let reply = self.publish_swap_candidate(
                    connection_id,
                    sink,
                    frame,
                    manifest,
                    negotiated_ver,
                    control_ops,
                    hello_ack,
                    hello_authority,
                    differing_declarations,
                    #[cfg(windows)]
                    launch_nonce,
                );
                let admitted = self
                    .registry
                    .get_candidate(&module_id)
                    .ok()
                    .flatten()
                    .is_some_and(|candidate| candidate.connection_id == connection_id);
                (reply, admitted)
            })
            .unwrap_or_else(|| {
                Ok(vec![control_error_frame(
                    frame,
                    "swap_token_invalid",
                    "swap ended before candidate HELLO publication",
                )?])
            })
    }

    #[allow(clippy::too_many_arguments)]
    fn publish_swap_candidate(
        &self,
        connection_id: ConnectionId,
        sink: Option<crate::FrameSink>,
        frame: &Frame,
        manifest: ModuleManifest,
        negotiated_ver: u8,
        control_ops: Vec<String>,
        hello_ack: PreparedAck<'_>,
        hello_authority: Option<&HelloAuthority>,
        differing_declarations: &mut ProviderDeclarations,
        #[cfg(windows)] launch_nonce: Option<&str>,
    ) -> Result<Vec<Frame>, RouterError> {
        let module_id = manifest.module_id.clone();
        let registration = match self.registry.register_unpublished_verified_candidate_with_control_ops(
            manifest,
            negotiated_ver,
            connection_id,
            control_ops,
        ) {
            Ok(registration) => registration,
            Err(RegistryError::DuplicateModuleId { module_id }) => {
                return Ok(vec![control_error_frame(
                    frame,
                    "duplicate_module_id",
                    format!(
                        "module_id '{module_id}' already has a swap candidate registered; duplicate HELLO rejected"
                    ),
                )?])
            }
            Err(err @ RegistryError::PathHazardModuleId { .. }) => {
                return Ok(vec![control_error_frame(
                    frame,
                    "invalid_module_id",
                    err.to_string(),
                )?])
            }
            Err(err) => {
                return Ok(vec![control_error_frame(
                    frame,
                    "registry_error",
                    err.to_string(),
                )?])
            }
        };
        *differing_declarations = match self.admit_hello_authority(connection_id, hello_authority) {
            Ok(differing) => differing,
            Err(err) => {
                if matches!(
                    self.rollback_unpublished_hello(connection_id),
                    Ok(r) if !r.is_empty()
                ) {
                    crate::supervise::notify_registration_release();
                }
                return Err(RouterError::Forwarding(err));
            }
        };
        #[cfg(test)]
        self.hold_between_registry_and_forwarding();
        let mut reply = Vec::new();
        let publication = self
            .registry
            .publish_registration_after(connection_id, || {
                if let Some(sink) = sink {
                    // Same ordering as an ordinary HELLO: queue the ACK before
                    // inserting the endpoint, and keep registry readers excluded
                    // until the candidate is published.
                    let concurrency = manifest_concurrency(&registration.manifest);
                    self.forwarding.register_candidate_module_connection_acked(
                        connection_id,
                        module_id.clone(),
                        negotiated_ver,
                        concurrency,
                        sink,
                        hello_ack,
                    )?;
                } else {
                    reply.push(
                        hello_ack
                            .finish()
                            .map_err(ForwardingError::HelloRegistrationRefused)?,
                    );
                }
                #[cfg(test)]
                self.hold_between_ack_and_publication();
                Ok(())
            })
            .map_err(|err| RouterError::backend(0, frame.header.corr, err.to_string()))?;
        let published = match publication {
            Ok(published) => published,
            Err(err) => {
                if matches!(
                    self.rollback_unpublished_hello(connection_id),
                    Ok(r) if !r.is_empty()
                ) {
                    crate::supervise::notify_registration_release();
                }
                self.refuse_hello_authority(connection_id, hello_authority);
                return Ok(vec![control_error_frame(
                    frame,
                    forwarding_error_code(&err),
                    err.to_string(),
                )?]);
            }
        };
        if !published {
            debug!(
                module_id = %module_id,
                connection_id = connection_id.get(),
                "swap candidate registration was removed before it could be published"
            );
            self.refuse_hello_authority(connection_id, hello_authority);
            return Ok(reply);
        }
        info!(
            module_id = %module_id,
            module_version = %registration.manifest.module_version,
            negotiated_ver,
            ready = registration.ready,
            connection_id = connection_id.get(),
            "swap candidate registered; not routable until cutover"
        );
        #[cfg(windows)]
        if let Some(nonce) = launch_nonce {
            self.supervisor.program_hello_accepted(&module_id, nonce);
        }
        Ok(reply)
    }

    /// Bind `connection` to the spawn its HELLO registers as and freeze the
    /// spawn's requested provider declarations. Runs only once the registry
    /// has accepted the HELLO, before its endpoint is published, so a route
    /// reserved on the endpoint always finds the spawn. Returns the requested
    /// entries that differ from the saved approvals, for a widening request.
    fn admit_hello_authority(
        &self,
        connection: ConnectionId,
        authority: Option<&HelloAuthority>,
    ) -> Result<ProviderDeclarations, ForwardingError> {
        let Some(authority) = authority else {
            return Ok(ProviderDeclarations::new());
        };
        let froze = authority
            .record
            .freeze_declarations(authority.requested.clone());
        authority
            .froze
            .store(froze.is_some(), std::sync::atomic::Ordering::Relaxed);
        self.forwarding
            .bind_target_authority(connection, authority.record.clone())
            .inspect_err(|_| self.refuse_hello_authority(connection, Some(authority)))?;
        Ok(froze.unwrap_or_default())
    }

    /// Undo [`Self::admit_hello_authority`] for a HELLO refused after the
    /// registry accepted it, so the refused connection is no registration of
    /// the spawn and the spawn's declarations are left for the HELLO that
    /// does register it to freeze.
    fn refuse_hello_authority(&self, connection: ConnectionId, authority: Option<&HelloAuthority>) {
        let Some(authority) = authority else {
            return;
        };
        if let Err(err) = self.forwarding.unbind_target_authority(connection) {
            warn!(error = %err, "failed to unbind a refused HELLO from its spawn");
        }
        if authority
            .froze
            .swap(false, std::sync::atomic::Ordering::Relaxed)
        {
            authority.record.unfreeze_refused_declarations();
        }
    }

    #[cfg(test)]
    pub(super) fn build_hello_ack(
        &self,
        frame: &Frame,
        negotiated_ver: u8,
        module_id: &str,
    ) -> Result<Frame, RouterError> {
        self.build_hello_ack_with_token(frame, negotiated_ver, module_id, None)
    }

    fn build_hello_ack_with_token(
        &self,
        frame: &Frame,
        negotiated_ver: u8,
        module_id: &str,
        resume_token: Option<&str>,
    ) -> Result<Frame, RouterError> {
        let ack = ModuleHelloAckBody {
            resume_token: resume_token.map(str::to_owned),
            negotiated_ver,
            subc_ops: module_subc_ops(),
            subc_capabilities: self.capabilities_with_settings(),
            storage: self
                .storage_config
                .as_ref()
                .map(|cfg| cfg.descriptor_for(module_id)),
            machine_id: self.machine_id.as_ref().map(|id| id.as_str().to_owned()),
        };
        let body = serde_json::to_vec(&ack).map_err(|err| {
            RouterError::backend(
                0,
                frame.header.corr,
                format!("failed to encode HELLO_ACK: {err}"),
            )
        })?;

        Frame::build_with_version(
            negotiated_ver,
            FrameType::HelloAck,
            control_flags(),
            0,
            0,
            frame.header.corr,
            body,
        )
        .map_err(RouterError::FrameBuild)
    }
}

impl ControlHandler {
    pub(super) fn handle_module_drain_ready(
        &self,
        connection_id: ConnectionId,
        frame: Frame,
    ) -> Result<Vec<Frame>, RouterError> {
        use subc_protocol::session::ModuleDrainReady;

        let body = match serde_json::from_slice::<ModuleDrainReady>(&frame.body) {
            Ok(body) => body,
            Err(err) => {
                return Ok(vec![control_error_frame(
                    &frame,
                    "invalid_control_body",
                    format!("malformed module.drain_ready body: {err}"),
                )?]);
            }
        };
        let ack = control_response_body_frame(&frame, &body, "ModuleDrainReady")?;
        if self
            .forwarding
            .mark_accepting_drain_ready(connection_id, ack)?
        {
            return Ok(Vec::new());
        }
        Ok(vec![control_error_frame(
            &frame,
            error_codes::INVALID_REQUEST,
            "this module connection has no accepting drain",
        )?])
    }

    pub(super) fn handle_module_control_request(
        &self,
        connection_id: ConnectionId,
        frame: Frame,
        request: ModuleControlRequestFromModule,
    ) -> Result<Vec<Frame>, RouterError> {
        match request {
            ModuleControlRequestFromModule::RouteAuthorize {
                principal,
                scope,
                target,
                expected,
            } => self.handle_route_authorize(
                connection_id,
                frame,
                principal,
                scope,
                target,
                expected,
            ),
            ModuleControlRequestFromModule::CatalogUpdate {
                provides,
                capabilities,
                ready,
            } => self.handle_catalog_update(connection_id, frame, provides, capabilities, ready),
            ModuleControlRequestFromModule::CatalogUpdateRoleDeclarations { role_declarations } => {
                self.handle_catalog_update_role_declarations(
                    connection_id,
                    frame,
                    role_declarations,
                )
            }
            ModuleControlRequestFromModule::LiveRoots {} => {
                let registered = self
                    .registry
                    .get_module_by_connection(connection_id)
                    .map_err(|err| RouterError::backend(0, frame.header.corr, err.to_string()))?;
                let Some(registration) = registered else {
                    return Ok(vec![control_error_frame(&frame, "not_registered", "supervisor.live_roots requires an active module registration owned by this connection")?]);
                };
                let response = self
                    .forwarding
                    .live_roots(&registration.manifest.module_id)
                    .map_err(RouterError::Forwarding)?;
                Ok(vec![control_response_body_frame(
                    &frame,
                    &response,
                    "ModuleControlResponseToModule::LiveRoots",
                )?])
            }
            ModuleControlRequestFromModule::AgentCommands {
                started,
                finished,
                known,
            } => self.handle_agent_commands(connection_id, frame, started, finished, known),
            ModuleControlRequestFromModule::ScopeSync { generation, scopes } => {
                self.handle_scope_sync(connection_id, frame, generation, scopes)
            }
            ModuleControlRequestFromModule::ScopeApply {
                generation,
                upsert,
                end,
            } => self.handle_scope_change(connection_id, frame, generation, upsert, Some(end)),
            ModuleControlRequestFromModule::ScopeDescribe { owner, scope_ref } => {
                self.handle_scope_describe(connection_id, frame, owner, scope_ref)
            }
            _ => Ok(vec![control_error_frame(
                &frame,
                "unsupported_control_frame",
                "unsupported module control operation",
            )?]),
        }
    }
}

impl ControlHandler {
    pub(super) fn guard_module_control_op(
        &self,
        frame: &Frame,
        module_id: &str,
        op: &str,
    ) -> Result<Option<Frame>, RouterError> {
        if self.module_grants_op(module_id, op, frame.header.corr)? {
            return Ok(None);
        }

        Ok(Some(control_error_frame(
            frame,
            "op_not_allowed",
            format!("module_id '{module_id}' did not grant control op '{op}'"),
        )?))
    }

    fn module_grants_op(&self, module_id: &str, op: &str, corr: u64) -> Result<bool, RouterError> {
        self.registry
            .grants_control_op(module_id, op)
            .map_err(|err| RouterError::backend(0, corr, err.to_string()))
    }

    pub(super) fn handle_status_update(
        &self,
        endpoint: ModuleEndpointId,
        frame: Frame,
    ) -> Result<Vec<Frame>, RouterError> {
        let update = match serde_json::from_slice::<ModuleControlPush>(&frame.body) {
            Ok(update) => update,
            Err(err) => {
                // Forward-compat: a newer module may push a channel-0 op this subc
                // version doesn't know. The control contract says unknown push ops
                // are IGNORED, never answered with an error. Only a malformed body
                // for an op we DO know is a real error worth surfacing.
                if is_known_module_push_op(&frame.body) {
                    return Ok(vec![control_error_frame(
                        &frame,
                        "invalid_control_body",
                        format!("malformed module control push body: {err}"),
                    )?]);
                }
                return Ok(Vec::new());
            }
        };

        match update {
            ModuleControlPush::RouteStatus {
                route_channel,
                route_epoch,
                status,
            } => {
                self.forwarding
                    .cache_status(endpoint, route_channel, route_epoch, status)
                    .map_err(RouterError::Forwarding)?;
            }
        }
        Ok(Vec::new())
    }

    pub(super) fn handle_route_poll(
        &self,
        ctx: &RouteCtx,
        frame: Frame,
        route_channel: u16,
        route_epoch: u32,
        kind: PollKind,
    ) -> Result<Vec<Frame>, RouterError> {
        let snapshot = self
            .forwarding
            .route_poll_snapshot(ctx.connection_id, route_channel, route_epoch)
            .map_err(RouterError::Forwarding)?;
        let response = match (kind, snapshot) {
            (PollKind::Status, RoutePollSnapshot::Bound { status, .. }) => {
                ClientControlResponse::RoutePoll {
                    route_channel,
                    route_epoch,
                    status,
                    live: None,
                }
            }
            (PollKind::Status, RoutePollSnapshot::Absent) => ClientControlResponse::RoutePoll {
                route_channel,
                route_epoch,
                status: None,
                live: None,
            },
            (PollKind::Liveness, RoutePollSnapshot::Bound { module_id, .. }) => {
                // ABSENCE HERE MEANS "NOT SUPERVISED", NOT "UNKNOWN", and that
                // is what makes reporting `true` correct rather than a
                // confident guess. `process_live` returns None only when the
                // module id has no supervisor snapshot at all -- an
                // externally-started module the daemon did not spawn -- and
                // for those the supervisor has no opinion to offer, ever. It
                // is never None for a supervised module in an unknown state:
                // a supervised module always has a snapshot, and the answer
                // comes from `state == Running && process_alive`.
                //
                // The route is Bound, so the module completed a HELLO on a
                // live connection; "the process this route points at is
                // running" is therefore attested by the binding rather than
                // assumed. Reporting `false` for an unsupervised module would
                // be the actual lie -- it would tell a client its healthy
                // route is dead because the daemon does not manage the
                // process.
                //
                // IF `process_live` EVER GAINS A THIRD CASE -- a supervised
                // module whose liveness is genuinely unknown, e.g. a snapshot
                // that has not been populated yet -- THIS DEFAULT BECOMES
                // WRONG and must split: unsupervised stays true, unknown
                // becomes null so the client can tell the two apart. The
                // response field is already `Option<bool>`, so the wire can
                // carry that distinction today.
                let live = self
                    .process_liveness
                    .as_ref()
                    .and_then(|source| source.process_live(&module_id))
                    .unwrap_or(true);
                ClientControlResponse::RoutePoll {
                    route_channel,
                    route_epoch,
                    status: None,
                    live: Some(live),
                }
            }
            (PollKind::Liveness, RoutePollSnapshot::Absent) => ClientControlResponse::RoutePoll {
                route_channel,
                route_epoch,
                status: None,
                live: Some(false),
            },
        };

        Ok(vec![control_response_body_frame(
            &frame,
            &response,
            "ClientControlResponse::RoutePoll",
        )?])
    }

    pub(crate) fn observe_module_control_completion(
        &self,
        completion: ModuleControlRpcCompletion,
    ) -> bool {
        match completion {
            ModuleControlRpcCompletion::Unknown => false,
            ModuleControlRpcCompletion::Settled => true,
            ModuleControlRpcCompletion::LateHealthAnswer { module_id, latency } => {
                let latency_ms = latency.as_millis().min(u128::from(u64::MAX)) as u64;
                info!(
                    module_id = %module_id,
                    latency_ms,
                    "late health.check answer proves the module is alive"
                );
                match self
                    .supervisor
                    .record_late_health_answer(&module_id, latency_ms)
                {
                    Ok(true) => {}
                    Ok(false) => debug!(
                        module_id = %module_id,
                        latency_ms,
                        "late health.check answer has no active supervisor snapshot"
                    ),
                    Err(err) => warn!(
                        module_id = %module_id,
                        latency_ms,
                        error = %err,
                        "failed to record late health.check answer"
                    ),
                }
                true
            }
        }
    }

    /// Decide whether a failure while settling a relayed `route.bind` belongs to
    /// the module connection whose frame is being handled, or to the client that
    /// relay was opened for.
    ///
    /// This runs on the MODULE connection's frame handler, where returning `Err`
    /// ends that connection -- and a module connection carries every client's
    /// routes to that module, so ending it costs the whole fleet its tools.
    /// `ConnectionClosing` carries the id of the connection that is closing, and
    /// when that id is a CLIENT's, the condition is entirely about that one
    /// client's route.open. A client-scoped condition has no authority over a
    /// shared module connection, so it is logged and the single relay is dropped:
    /// the client is going away, and `complete_pending_relay` already removed the
    /// relay before failing, so there is nothing left to settle. Anything that
    /// relay still reserved is released by that client's own connection teardown,
    /// which is already under way -- that is what "closing" means.
    ///
    /// Every other failure is a statement about THIS connection and stays fatal:
    /// a poisoned forwarding lock, a stale module endpoint, and the module's own
    /// id in `ConnectionClosing` all mean this connection cannot keep serving
    /// frames correctly.
    pub(super) fn refuse_to_end_module_connection_for_a_client(
        &self,
        module_connection_id: ConnectionId,
        corr: u64,
        err: ForwardingError,
    ) -> Result<(), RouterError> {
        if let ForwardingError::ConnectionClosing { connection_id } = err {
            if connection_id != module_connection_id {
                warn!(
                    module_connection_id = module_connection_id.get(),
                    client_connection_id = connection_id.get(),
                    corr,
                    "dropping a route.bind response for a closing client; the module connection keeps serving"
                );
                return Ok(());
            }
        }
        Err(RouterError::Forwarding(err))
    }

    pub(super) fn handle_module_relay_response(
        &self,
        connection_id: ConnectionId,
        frame: Frame,
    ) -> Result<Vec<Frame>, RouterError> {
        let mut secondary_error = None;
        let outcome = match frame.header.ty {
            FrameType::Response => match module_control_response_probe(&frame.body) {
                Ok(probe) if probe.op == "route.bind" => {
                    match serde_json::from_slice::<ModuleControlResponse>(&frame.body) {
                        Ok(ModuleControlResponse::RouteBindAck {}) => {
                            RouteBindRelayOutcome::Accepted
                        }
                        Ok(other) => {
                            let message =
                                format!("route.bind response carried unexpected body: {other:?}");
                            secondary_error = Some(control_error_frame(
                                &frame,
                                "invalid_control_body",
                                message.clone(),
                            )?);
                            RouteBindRelayOutcome::ModuleGone(message)
                        }
                        Err(err) => {
                            let message = format!("malformed route.bind response body: {err}");
                            secondary_error = Some(control_error_frame(
                                &frame,
                                "invalid_control_body",
                                message.clone(),
                            )?);
                            RouteBindRelayOutcome::ModuleGone(message)
                        }
                    }
                }
                Ok(probe) => {
                    let outcome = if matches!(
                        probe.op.as_str(),
                        "config.review" | "consent/v1" | "projects.enumerate"
                    ) {
                        ModuleControlRpcOutcome::RawResponse(frame.body.clone())
                    } else {
                        match serde_json::from_slice::<ModuleControlResponse>(&frame.body) {
                            Ok(response) => ModuleControlRpcOutcome::Response(response),
                            Err(err) => ModuleControlRpcOutcome::MalformedResponse(format!(
                                "malformed {} response body: {err}",
                                probe.op
                            )),
                        }
                    };
                    let completion = self
                        .forwarding
                        .complete_module_control_rpc(
                            connection_id,
                            frame.header.corr,
                            Some(&probe.op),
                            outcome,
                        )
                        .map_err(RouterError::Forwarding)?;
                    if !self.observe_module_control_completion(completion) {
                        debug!(
                            connection_id = connection_id.get(),
                            corr = frame.header.corr,
                            op = %probe.op,
                            "dropping late or unknown module-control RPC response"
                        );
                    }
                    return Ok(Vec::new());
                }
                Err(err) => {
                    if let Some(expected_op) = self
                        .forwarding
                        .pending_module_control_op(connection_id, frame.header.corr)
                        .map_err(RouterError::Forwarding)?
                    {
                        let completion = self
                            .forwarding
                            .complete_module_control_rpc(
                                connection_id,
                                frame.header.corr,
                                None,
                                ModuleControlRpcOutcome::MalformedResponse(format!(
                                    "malformed {expected_op} response body: {err}"
                                )),
                            )
                            .map_err(RouterError::Forwarding)?;
                        if !self.observe_module_control_completion(completion) {
                            debug!(
                                connection_id = connection_id.get(),
                                corr = frame.header.corr,
                                "dropping late malformed module-control RPC response"
                            );
                        }
                        return Ok(Vec::new());
                    }
                    let message = format!("malformed route.bind response body: {err}");
                    secondary_error = Some(control_error_frame(
                        &frame,
                        "invalid_control_body",
                        message.clone(),
                    )?);
                    RouteBindRelayOutcome::ModuleGone(message)
                }
            },
            FrameType::Error => {
                if self
                    .forwarding
                    .pending_module_control_op(connection_id, frame.header.corr)
                    .map_err(RouterError::Forwarding)?
                    .is_some()
                {
                    let outcome = match serde_json::from_slice::<ErrorBody>(&frame.body) {
                        Ok(body) => ModuleControlRpcOutcome::Rejected(body),
                        Err(err) => ModuleControlRpcOutcome::MalformedResponse(format!(
                            "malformed module-control ERROR body: {err}"
                        )),
                    };
                    let completion = self
                        .forwarding
                        .complete_module_control_rpc(
                            connection_id,
                            frame.header.corr,
                            None,
                            outcome,
                        )
                        .map_err(RouterError::Forwarding)?;
                    if !self.observe_module_control_completion(completion) {
                        debug!(
                            connection_id = connection_id.get(),
                            corr = frame.header.corr,
                            "dropping late or unknown module-control RPC error"
                        );
                    }
                    return Ok(Vec::new());
                }
                match serde_json::from_slice::<ErrorBody>(&frame.body) {
                    Ok(body) => RouteBindRelayOutcome::Rejected(body),
                    Err(err) => {
                        let message = format!("malformed route.bind ERROR body: {err}");
                        secondary_error = Some(control_error_frame(
                            &frame,
                            "invalid_control_body",
                            message.clone(),
                        )?);
                        RouteBindRelayOutcome::ModuleGone(message)
                    }
                }
            }
            ty => {
                return Ok(vec![control_error_frame(
                    &frame,
                    "unsupported_control_frame",
                    format!("unsupported module channel-0 frame {ty:?}"),
                )?])
            }
        };

        let settled = self.settle_route_bind_relay(connection_id, frame.header.corr, outcome);
        let completion = match settled {
            Ok(completion) => completion,
            Err(err) => {
                self.refuse_to_end_module_connection_for_a_client(
                    connection_id,
                    frame.header.corr,
                    err,
                )?;
                return Ok(secondary_error.into_iter().collect());
            }
        };
        if let Some(target) = completion.abandoned.as_ref() {
            send_goodbye_target_best_effort(&self.counters, target, "late accepted route.bind");
        }
        if !completion.settled {
            debug!(
                connection_id = connection_id.get(),
                corr = frame.header.corr,
                frame_type = ?frame.header.ty,
                "dropping late or unknown route.bind relay response"
            );
        }
        Ok(secondary_error.into_iter().collect())
    }
}

fn module_subc_ops() -> Vec<String> {
    SUBC_CONTROL_OPS
        .iter()
        .chain(MODULE_TO_SUBC_CONTROL_OPS.iter())
        .map(|op| (*op).to_string())
        .collect()
}

#[cfg(test)]
pub(super) fn module_baseline_control_ops() -> Vec<String> {
    MODULE_BASELINE_CONTROL_OPS
        .iter()
        .map(|op| (*op).to_string())
        .collect()
}

fn effective_module_control_ops(declared: Option<Vec<String>>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut effective = Vec::new();
    for op in MODULE_BASELINE_CONTROL_OPS {
        if seen.insert((*op).to_string()) {
            effective.push((*op).to_string());
        }
    }
    for op in declared.unwrap_or_default() {
        if seen.insert(op.clone()) {
            effective.push(op);
        }
    }
    effective
}

pub(super) fn module_registration_grants_op(control_ops: &[String], op: &str) -> bool {
    MODULE_BASELINE_CONTROL_OPS.contains(&op) || control_ops.iter().any(|granted| granted == op)
}

fn module_control_response_probe(body: &[u8]) -> Result<ControlOpProbe, serde_json::Error> {
    serde_json::from_slice(body).or_else(|original| {
        #[derive(Deserialize)]
        struct ConsentProbe {
            protocol: String,
        }
        match serde_json::from_slice::<ConsentProbe>(body) {
            Ok(probe) if probe.protocol == subc_protocol::consent::CONSENT_V1 => {
                Ok(ControlOpProbe { op: probe.protocol })
            }
            _ => Err(original),
        }
    })
}

/// Channel-0 push ops this subc version understands. A push whose `op` is not in
/// this set is treated as a forward-compat unknown and ignored rather than errored.
const MODULE_PUSH_OPS: &[&str] = &["route.status"];

fn is_known_module_push_op(body: &[u8]) -> bool {
    serde_json::from_slice::<ControlOpProbe>(body)
        .map(|probe| MODULE_PUSH_OPS.contains(&probe.op.as_str()))
        .unwrap_or(false)
}

pub(super) fn is_known_module_request_op(body: &[u8]) -> bool {
    serde_json::from_slice::<ControlOpProbe>(body)
        .map(|probe| is_module_to_subc_op(&probe.op))
        .unwrap_or(false)
}

fn is_module_to_subc_op(op: &str) -> bool {
    MODULE_TO_SUBC_CONTROL_OPS.contains(&op) || MODULE_TO_SUBC_UNADVERTISED_OPS.contains(&op)
}

pub(super) fn module_control_request_op(request: &ModuleControlRequestFromModule) -> &'static str {
    match request {
        ModuleControlRequestFromModule::PolicyRoot(_) => subc_protocol::policy_root::GET_OP,
        ModuleControlRequestFromModule::PolicyRootRotate(_) => {
            subc_protocol::policy_root::ROTATE_OP
        }
        ModuleControlRequestFromModule::CatalogUpdate { .. } => MODULE_TO_SUBC_OP_CATALOG_UPDATE,
        ModuleControlRequestFromModule::CatalogUpdateRoleDeclarations { .. } => {
            MODULE_TO_SUBC_OP_CATALOG_UPDATE_ROLE_DECLARATIONS
        }
        ModuleControlRequestFromModule::LiveRoots {} => "supervisor.live_roots",
        ModuleControlRequestFromModule::ScopeSync { .. } => SCOPE_SYNC_OP,
        ModuleControlRequestFromModule::ScopeApply { .. } => SCOPE_APPLY_OP,
        ModuleControlRequestFromModule::ScopeDescribe { .. } => SCOPE_DESCRIBE_OP,
        ModuleControlRequestFromModule::RouteAuthorize { .. } => {
            subc_protocol::scope::ROUTE_AUTHORIZE_OP
        }
        ModuleControlRequestFromModule::AgentCommands { .. } => "supervisor.agent_commands",
        _ => "unsupported",
    }
}

pub(super) fn parse_module_control_request_from_module(
    body: &[u8],
) -> Result<ModuleControlRequestFromModule, (serde_json::Error, ControlRequestBodyError)> {
    serde_json::from_slice::<ModuleControlRequestFromModule>(body).map_err(|err| {
        let classification = match serde_json::from_slice::<ControlOpProbe>(body) {
            Ok(probe) if is_module_to_subc_op(&probe.op) => ControlRequestBodyError::InvalidBody,
            Ok(_) => ControlRequestBodyError::UnknownOp,
            Err(_) => ControlRequestBodyError::InvalidBody,
        };
        (err, classification)
    })
}

fn manifest_provides_routable_role(manifest: &ModuleManifest) -> bool {
    manifest.provides.iter().any(is_routable_role)
}

/// Returns the routable-provider concurrency subc should enforce for this manifest.
///
/// ToolProvider and ManagementSurface store their delivery concurrency directly.
/// InternalService has no role-specific concurrency field, so it retains the
/// existing ModuleManaged default for backward compatibility.
pub(super) fn manifest_concurrency(manifest: &ModuleManifest) -> Concurrency {
    manifest
        .provides
        .iter()
        .find_map(|provider| match provider {
            ProviderRole::ToolProvider { concurrency, .. }
            | ProviderRole::ManagementSurface { concurrency, .. } => Some(concurrency.clone()),
            ProviderRole::PipelineStage { .. } | ProviderRole::InternalService { .. } => None,
        })
        .unwrap_or(Concurrency::ModuleManaged)
}

/// True when the manifest carries a ManagementSurface role whose concurrency
/// was RESOLVED BY SERDE DEFAULT rather than declared. Reads the raw HELLO
/// bytes because the typed manifest deliberately erases that distinction: the
/// default exists for wire compatibility, and this probe exists so the default
/// stays observable. Any parse irregularity returns false -- the caller only
/// logs, and a malformed body already failed registration upstream.
pub(super) fn manifest_concurrency_was_defaulted(
    raw_hello: &[u8],
    manifest: &ModuleManifest,
) -> bool {
    let has_management_surface = manifest
        .provides
        .iter()
        .any(|provider| matches!(provider, ProviderRole::ManagementSurface { .. }));
    if !has_management_surface {
        return false;
    }
    let Ok(raw) = serde_json::from_slice::<serde_json::Value>(raw_hello) else {
        return false;
    };
    let Some(provides) = raw
        .get("manifest")
        .and_then(|manifest| manifest.get("provides"))
        .and_then(serde_json::Value::as_array)
    else {
        return false;
    };
    // ProviderRole is internally tagged (`tag = "role"`), so the wire shape is
    // flat: {"role": "management_surface", ..., "concurrency": ...} -- verified
    // against the management_surface_manifest_without_concurrency golden, not
    // recalled (the externally-tagged guess was this function's first bug).
    provides.iter().any(|role| {
        role.get("role").and_then(serde_json::Value::as_str) == Some("management_surface")
            && role.get("concurrency").is_none()
    })
}

fn negotiate_version(peer_version: u8) -> Result<u8, String> {
    if peer_version != PROTOCOL_VERSION {
        return Err(format!(
            "protocol_ver {peer_version} is unsupported; this daemon requires exactly {PROTOCOL_VERSION}"
        ));
    }
    Ok(PROTOCOL_VERSION)
}

/// Map a forwarding failure to the wire code a client sees.
///
/// The code is not a label: clients BRANCH on it. Both SDKs decide "retry in
/// place" with `subc_protocol::error_codes::is_retryable_route_open`, so a code
/// chosen here decides whether a caller retries or gives up.
///
/// That makes attribution the load-bearing property, not merely having a code. A
/// permanent fault published as a retryable one produces a fleet-wide retry storm
/// against something that can never recover; a transient fault published as
/// permanent gives up on work that would have succeeded. Both look correct in a
/// log, which is why `retryability_of_forwarding_codes_matches_the_failure` pins
/// the mapping per variant rather than merely asserting that some code exists.
///
/// That fence partitions by RETRYABILITY, which is coarser than identity: swapping
/// two codes on the same side of the boundary passes it. Measured rather than
/// assumed — `NoModuleConnection` re-pointed at `module_reloading` is caught only
/// by `supervision_only_module_health_probe_does_not_enable_route_open_and_cleans_up`,
/// a test named for something else that happens to assert the string.
///
/// That accidental coverage is deliberately left alone rather than promoted to a
/// named test, because it guards a property this function does not promise.
/// Checked at source: every consumer branches on the RETRYABLE SET and none on a
/// specific code within a class, so identity is free to change and only the
/// partition is a contract. Splitting it out would assert a guarantee nothing
/// depends on — and a suite that promises more than the code does is the harder
/// thing to correct later, because the next reader cannot tell which assertions
/// are load-bearing.
///
/// Pin identity here the moment a consumer branches on a specific code.
/// The spawn a HELLO would register as and the provider declarations it
/// requests, carried from the HELLO's checks to the point the registry
/// accepts it.
struct HelloAuthority {
    record: Arc<crate::spawn_authority::SpawnAuthorityRecord>,
    requested: ProviderDeclarations,
    /// Whether admitting this HELLO froze the spawn's requested declarations,
    /// so a refusal after admission undoes that freeze and no other.
    froze: std::sync::atomic::AtomicBool,
}
