use subc_control::config::{ConfigControlRequest, ConfigControlResponse};

use super::*;

/// Bound each immutable module review so an unanswered request cannot approve a plan.
const CONFIG_REVIEW_TIMEOUT: Duration = Duration::from_secs(30);
use crate::config_gate::{
    ApprovalChallenge, ApprovalFacts, ApprovalReply, GateError, PlanKind, PresenceDecision,
};

pub(super) fn is_config_op(op: &str) -> bool {
    matches!(
        op,
        "config.status"
            | "config.plan"
            | "config.apply"
            | "config.stamp"
            | "config.unstamp"
            | "config.history"
            | "config.rollback"
    )
}

pub(super) fn is_module_config_op(op: &str) -> bool {
    matches!(op, "config.declare" | "config.get" | "config.applied")
}

impl PrivilegedRequest for ConfigControlRequest {
    fn privileged_operation(&self) -> Option<PrivilegedOperation<'_>> {
        // The config gate binds trusted review and presence to its digest. The
        // generic operator prompt would run twice, and its log/shortcut modes
        // cannot substitute for config approval.
        match self {
            Self::Status {} => None,
            Self::Plan {} => None,
            Self::Apply { .. } => None,
            Self::Stamp {} => None,
            Self::Unstamp {} => None,
            Self::History {} => None,
            Self::Rollback { .. } => None,
            _ => None,
        }
    }
}

impl ControlHandler {
    pub(super) async fn handle_module_config_request(
        &self,
        ctx: &RouteCtx,
        frame: Frame,
        op: &str,
    ) -> Result<Vec<Frame>, RouterError> {
        let Some(runtime) = self.config_runtime.clone() else {
            return Ok(vec![control_error_frame(
                &frame,
                "config_gate_inactive",
                "no config store configured",
            )?]);
        };
        let registration = self
            .registry
            .get_module_by_connection(ctx.connection_id)
            .map_err(|error| RouterError::backend(0, frame.header.corr, error.to_string()))?;
        let Some(registration) = registration else {
            return Ok(vec![control_error_frame(
                &frame,
                "not_registered",
                "module config operations require a registered connection",
            )?]);
        };
        let module = registration.manifest.module_id.clone();
        let gate = runtime
            .module_gate(&module)
            .map_err(|e| RouterError::backend(0, frame.header.corr, e.to_string()))?;
        let _gate = gate.lock().await;
        let active = self
            .registry
            .get_module(&module)
            .map_err(|error| RouterError::backend(0, frame.header.corr, error.to_string()))?
            .is_some_and(|r| r.connection_id == ctx.connection_id)
            && self
                .forwarding
                .active_module_endpoint(&module)
                .map_err(RouterError::Forwarding)?
                .is_some_and(|target| target.connection_id == ctx.connection_id);
        let candidate = self
            .registry
            .get_candidate(&module)
            .map_err(|error| RouterError::backend(0, frame.header.corr, error.to_string()))?
            .is_some_and(|r| r.connection_id == ctx.connection_id);
        if !active && !candidate {
            return Ok(vec![control_error_frame(
                &frame,
                "not_delivery_target",
                "connection is no longer the active or candidate delivery target",
            )?]);
        }
        let body = frame.body.clone();
        let connection = ctx.connection_id;
        let mut delivered_view = None;
        let result = match op {
            "config.declare" => {
                let declaration = serde_json::from_slice(&body).map_err(|_| {
                    GateError::new("invalid_control_body", "malformed config.declare request")
                });
                match declaration {
                    Ok(declaration) => {
                        config_work(runtime.clone(), move |owner| {
                            owner.declare(connection, &module, declaration, active)?;
                            Ok(serde_json::json!({"op":"config.declare"}))
                        })
                        .await
                    }
                    Err(error) => Err(error),
                }
            }
            "config.get" => {
                let request = serde_json::from_slice(&body).map_err(|_| {
                    GateError::new("invalid_control_body", "malformed config.get request")
                });
                match request {
                    Ok(request) => {
                        let module_id = module.clone();
                        match config_work(runtime.clone(), move |owner| {
                            owner.module_get(connection, &module_id, request)
                        })
                        .await
                        {
                            Ok(reply) => {
                                let encoded = serde_json::to_value(&reply).map_err(|_| {
                                    GateError::new(
                                        "config_unavailable",
                                        "could not encode config view",
                                    )
                                });
                                delivered_view = Some(reply);
                                encoded
                            }
                            Err(error) => Err(error),
                        }
                    }
                    Err(error) => Err(error),
                }
            }
            "config.applied" => {
                match serde_json::from_slice::<subc_protocol::config::ConfigAppliedRequest>(&body) {
                    Ok(request) => {
                        config_work(runtime.clone(), move |owner| {
                            owner.module_applied(connection, &module, request)?;
                            Ok(serde_json::json!({"op":"config.applied"}))
                        })
                        .await
                    }
                    Err(_) => Err(GateError::new(
                        "invalid_control_body",
                        "malformed config.applied request",
                    )),
                }
            }
            _ => Err(GateError::new(
                "invalid_control_body",
                "unsupported module configuration operation",
            )),
        };
        match result {
            Ok(response) => {
                let response =
                    control_response_body_frame(&frame, &response, "module config response")?;
                if let Some(view) = delivered_view {
                    ctx.egress.send_flushed(response).await?;
                    let module = registration.manifest.module_id;
                    config_work(runtime, move |owner| {
                        owner.module_view_delivered(
                            connection,
                            &module,
                            &view,
                            tokio::time::Instant::now(),
                            active,
                        )
                    })
                    .await
                    .map_err(|error| {
                        RouterError::backend(0, frame.header.corr, error.to_string())
                    })?;
                    Ok(vec![])
                } else {
                    Ok(vec![response])
                }
            }
            Err(error) => Ok(vec![config_error_frame(&frame, error)?]),
        }
    }
    pub fn with_config_runtime(
        mut self,
        runtime: Arc<crate::config_gate::runtime::RuntimeOwner>,
    ) -> Self {
        let _ = runtime.set_activity(self.activity.clone());
        self.config_runtime = Some(runtime);
        self
    }

    /// Development surface identity is supplied by the daemon constructor, not
    /// selected by a connection, config file or startup argument.
    #[cfg(feature = "dev-desktop-approval")]
    pub fn with_dev_desktop_verifier(
        mut self,
        verifier: crate::config_gate::desktop::DevDesktopVerifier,
    ) -> Self {
        self.dev_desktop_verifier = verifier;
        self
    }

    fn config_desktop_identity(
        &self,
        connection: ConnectionId,
    ) -> std::result::Result<bool, GateError> {
        if !self.config_owner_connection(connection) {
            return Ok(false);
        }
        #[cfg(feature = "dev-desktop-approval")]
        {
            let peer = self
                .captured_peers
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .get(&connection)
                .cloned();
            self.dev_desktop_verifier.verify(peer.as_ref())
        }
        #[cfg(not(feature = "dev-desktop-approval"))]
        {
            Ok(self
                .config_desktop_stamps
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .get(&connection)
                .is_some_and(|stamp| stamp.revalidate_with_launches(&self.launch_table)))
        }
    }

    pub(crate) fn config_desktop_review_response(
        &self,
        connection: ConnectionId,
        response: &Frame,
    ) -> Option<subc_control::config::ConfigPlanView> {
        if self.config_runtime.is_none() || response.header.ty != FrameType::Response {
            return None;
        }
        let probe: ControlOpProbe = serde_json::from_slice(&response.body).ok()?;
        if !matches!(
            probe.op.as_str(),
            "config.plan" | "config.stamp" | "config.unstamp" | "config.rollback"
        ) || !matches!(self.config_desktop_identity(connection), Ok(true))
        {
            return None;
        }
        match serde_json::from_slice::<ConfigControlResponse>(&response.body).ok()? {
            ConfigControlResponse::Plan { view }
            | ConfigControlResponse::Stamp { view }
            | ConfigControlResponse::Unstamp { view }
            | ConfigControlResponse::Rollback { view } => Some(view),
            _ => None,
        }
    }

    pub(crate) async fn config_desktop_review_delivered(
        &self,
        connection: ConnectionId,
        view: subc_control::config::ConfigPlanView,
    ) {
        if !matches!(self.config_desktop_identity(connection), Ok(true)) {
            return;
        }
        let Some(runtime) = self.config_runtime.clone() else {
            return;
        };
        let peers = self.captured_peers.clone();
        let _ = config_work(runtime, move |owner| {
            // Lock order is captured peers, then owner. Cleanup removes the
            // peer before dropping the owner's receipt, so a delayed writer
            // acknowledgement cannot resurrect a disconnected connection.
            let peers = peers.lock().unwrap_or_else(|p| p.into_inner());
            if !peers.contains_key(&connection) {
                return Ok(());
            }
            owner.desktop_view_delivered(connection, &view)
        })
        .await;
    }

    pub(crate) async fn deliver_config_desktop_review(
        &self,
        ctx: &RouteCtx,
        response: Frame,
        view: subc_control::config::ConfigPlanView,
    ) -> Result<(), RouterError> {
        ctx.egress.send_flushed(response).await?;
        self.config_desktop_review_delivered(ctx.connection_id, view)
            .await;
        Ok(())
    }

    pub fn with_config_review_admission(
        mut self,
        admission: Arc<dyn crate::config_gate::ReviewAdmission>,
    ) -> Self {
        self.config_review_admission = admission;
        self
    }

    pub(super) async fn handle_config_gate_request(
        &self,
        ctx: &RouteCtx,
        frame: Frame,
    ) -> Result<Vec<Frame>, RouterError> {
        let request = match serde_json::from_slice::<ConfigControlRequest>(&frame.body) {
            Ok(request) => request,
            Err(error) => {
                return Ok(vec![control_error_frame(
                    &frame,
                    "invalid_control_body",
                    format!("malformed config control body: {error}"),
                )?])
            }
        };
        debug_assert!(privileged_operation(&request).is_none());
        let Some(runtime) = self.config_runtime.clone() else {
            return Ok(vec![control_error_frame(
                &frame,
                "config_gate_inactive",
                "no config store configured",
            )?]);
        };
        if !self.config_owner_connection(ctx.connection_id) {
            return Ok(vec![control_error_frame(
                &frame,
                "owner_required",
                "configuration control requires a kernel-observed owner-user connection",
            )?]);
        }
        let result = self
            .execute_config_request(ctx, &frame, runtime, request)
            .await;
        match result {
            Ok(response) => Ok(vec![control_response_body_frame(
                &frame,
                &response,
                "ConfigControlResponse",
            )?]),
            Err(error) => Ok(vec![config_error_frame(&frame, error)?]),
        }
    }

    fn config_owner_connection(&self, connection: ConnectionId) -> bool {
        let peers = self
            .observed_peers
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let Some(peer) = peers.get(&connection) else {
            return false;
        };
        #[cfg(unix)]
        {
            peer.uid == Some(subc_os::effective_uid())
        }
        #[cfg(windows)]
        {
            subc_os::named_pipe::current_user_sid_string()
                .ok()
                .as_ref()
                .is_some_and(|sid| peer.sid.as_ref() == Some(sid))
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = peer;
            false
        }
    }

    async fn execute_config_request(
        &self,
        ctx: &RouteCtx,
        frame: &Frame,
        runtime: Arc<crate::config_gate::runtime::RuntimeOwner>,
        request: ConfigControlRequest,
    ) -> std::result::Result<ConfigControlResponse, GateError> {
        match request {
            ConfigControlRequest::Status {} => {
                let mut status = config_work(runtime, |owner| owner.status()).await?;
                let facts = self.config_approval_facts(ctx).await;
                status
                    .missing_approval_surfaces
                    .extend(crate::config_gate::missing_surfaces(
                        &facts,
                        matches!(status.state.as_str(), "store_missing" | "store_unverified"),
                    ));
                if let Err(error) = self.config_desktop_identity(ctx.connection_id) {
                    status.missing_approval_surfaces.push(error.code);
                }
                Ok(ConfigControlResponse::Status { status })
            }
            ConfigControlRequest::History {} => {
                let (generations, pruned) = config_work(runtime, |owner| owner.history()).await?;
                Ok(ConfigControlResponse::History {
                    generations,
                    pruned,
                })
            }
            ConfigControlRequest::Apply { digest } => {
                if self.rescan.is_none() {
                    return Err(GateError::new(
                        "supervisor_reconcile_unavailable",
                        "configuration commit requires the supervisor owner",
                    ));
                }
                let facts = self.config_approval_facts(ctx).await;
                let challenge = config_work(runtime.clone(), move |owner| {
                    owner.begin_approval(&digest, &facts)
                })
                .await
                .map_err(|error| {
                    if error.code == "approval_surface_unavailable" {
                        self.config_desktop_identity(ctx.connection_id)
                            .err()
                            .unwrap_or(error)
                    } else {
                        error
                    }
                })?;
                let reply = match &challenge {
                    ApprovalChallenge::Desktop { .. } | ApprovalChallenge::OsPrompt { .. } => {
                        ApprovalReply::Presence(
                            self.confirm_config_gate_presence(ctx, frame, &challenge)
                                .await,
                        )
                    }
                    ApprovalChallenge::Phone { .. } => {
                        return Err(GateError::new(
                            "approval_surface_unavailable",
                            "phone approval relay is not connected",
                        ))
                    }
                };
                let operation = self.supervisor.operation_lock();
                let _operation = operation.lock().await;
                let facts = self.config_approval_facts(ctx).await;
                let attempt = challenge.attempt();
                let now = now_ms();
                let generation = config_work(runtime, move |owner| {
                    owner.finish_approval(attempt, &facts, reply, now)
                })
                .await?;
                Ok(ConfigControlResponse::Applied { generation })
            }
            request => {
                let kind = match request {
                    ConfigControlRequest::Plan {} => PlanKind::Change,
                    ConfigControlRequest::Stamp {} => PlanKind::Stamp,
                    ConfigControlRequest::Unstamp {} => PlanKind::Unstamp,
                    ConfigControlRequest::Rollback { generation } => PlanKind::Rollback(generation),
                    _ => {
                        return Err(GateError::new(
                            "unsupported_control_frame",
                            "unknown configuration operation",
                        ))
                    }
                };
                let prepared =
                    config_work(runtime.clone(), move |owner| owner.prepare(kind)).await?;
                let mut fields = Vec::new();
                for (module, captured) in &prepared.candidate.modules {
                    if !prepared.review_modules.contains(module) {
                        continue;
                    }
                    let registration = self
                        .registry
                        .get_module(module)
                        .map_err(|_| {
                            GateError::new("config_unavailable", "registry is unavailable")
                        })?
                        .ok_or_else(|| {
                            GateError::new(
                                "module_review_unavailable",
                                format!("{module}: no active reviewer"),
                            )
                        })?;
                    self.config_review_admission.check(&registration)?;
                    if !registration
                        .control_ops
                        .iter()
                        .any(|op| op == "config.review")
                    {
                        return Err(GateError::new(
                            "module_review_unavailable",
                            format!("{module}: config.review is not opted in"),
                        ));
                    }
                    let base = prepared.bases.get(module);
                    let request = subc_protocol::config::ConfigReviewRequest::new(
                        base.map_or(0, |b| b.generation),
                        captured.view.layers.clone(),
                        base.cloned(),
                    );
                    let bytes = serde_json::to_vec(&request).map_err(|_| {
                        GateError::new("config_unavailable", "could not encode immutable review")
                    })?;
                    let reply = self
                        .config_rpc(
                            &runtime,
                            module,
                            registration.connection_id,
                            "config.review",
                            bytes,
                        )
                        .await?;
                    let reply = serde_json::from_slice(&reply).map_err(|_| {
                        GateError::new(
                            "module_review_invalid",
                            format!("{module}: malformed config.review reply"),
                        )
                    })?;
                    fields.extend(crate::config_gate::review::checked_fields(
                        module,
                        &captured.schema,
                        base,
                        prepared.base_schemas.get(module),
                        &captured.view,
                        reply,
                    )?);
                }
                let view = config_work(runtime, move |owner| {
                    owner.finish_plan(prepared, move |old, new| {
                        let mut changeset = crate::config_gate::runtime::daemon_review(old, new)?;
                        changeset.fields = fields;
                        let mut effects: std::collections::BTreeMap<String,std::collections::BTreeMap<String,Vec<String>>> = Default::default();
                        for field in &changeset.fields {
                            let kind = serde_json::to_value(field.apply).map_err(|_| GateError::new("schema_invalid", "could not encode apply kind"))?.as_str().ok_or_else(|| GateError::new("schema_invalid","apply kind is not a string"))?.to_owned();
                            effects.entry(field.module_id.clone()).or_default().entry(kind).or_default().push(field.path.clone());
                        }
                        if changeset.fields.iter().any(|field| field.apply != subc_config::ApplyKind::Live) {
                            changeset.apply_plan = serde_json::json!({"daemon_effects":changeset.apply_plan,"modules":effects});
                        }
                        Ok(changeset)
                    })
                })
                .await?;
                Ok(match kind {
                    PlanKind::Change => ConfigControlResponse::Plan { view },
                    PlanKind::Stamp => ConfigControlResponse::Stamp { view },
                    PlanKind::Unstamp => ConfigControlResponse::Unstamp { view },
                    PlanKind::Rollback(_) => ConfigControlResponse::Rollback { view },
                })
            }
        }
    }

    async fn config_approval_facts(&self, ctx: &RouteCtx) -> ApprovalFacts {
        let desktop = self
            .config_desktop_identity(ctx.connection_id)
            .unwrap_or(false);
        ApprovalFacts {
            connection: ctx.connection_id.get(),
            owner_uid: self.config_owner_connection(ctx.connection_id),
            desktop_app: desktop,
            attested_operator_app: desktop && !cfg!(feature = "dev-desktop-approval"),
            os_prompt_available: config_os_prompt_available(
                self.forwarding.operator_confirms().config_gate_ready(),
                crate::config_gate::os_prompt_policy_present(),
            ),
            phone: None,
            caller: self.resolve_caller(ctx.connection_id).await,
        }
    }

    async fn config_rpc(
        &self,
        runtime: &Arc<crate::config_gate::runtime::RuntimeOwner>,
        module: &str,
        expected_connection: ConnectionId,
        op: &str,
        bytes: Vec<u8>,
    ) -> std::result::Result<Vec<u8>, GateError> {
        let gate = runtime.module_gate(module)?;
        let pending = {
            let _gate = gate.lock().await;
            let registration = self
                .registry
                .get_module(module)
                .map_err(|_| GateError::new("config_unavailable", "registry unavailable"))?
                .ok_or_else(|| {
                    GateError::new(
                        "module_review_unavailable",
                        format!("{module}: no active target"),
                    )
                })?;
            let pending = self
                .forwarding
                .begin_module_control_rpc_for(module, op, Instant::now() + CONFIG_REVIEW_TIMEOUT)
                .map_err(|_| {
                    GateError::new(
                        "module_review_unavailable",
                        format!("{module}: target unavailable"),
                    )
                })?;
            if pending.endpoint.connection_id != registration.connection_id
                || registration.connection_id != expected_connection
            {
                let _ = self
                    .forwarding
                    .cancel_module_control_rpc(pending.endpoint, pending.corr);
                return Err(GateError::new(
                    "not_delivery_target",
                    "registry and forwarding targets differ",
                ));
            }
            pending
        };
        let PendingModuleControlRpc {
            endpoint,
            module_sink,
            negotiated_ver,
            corr,
            receiver,
        } = pending;
        let mut guard = ModuleControlRpcGuard::new(self.forwarding.clone(), endpoint, corr);
        let deadline = Instant::now() + CONFIG_REVIEW_TIMEOUT;
        let frame = Frame::build_with_version(
            negotiated_ver,
            FrameType::Request,
            control_flags(),
            0,
            0,
            corr,
            bytes,
        )
        .map_err(|_| GateError::new("config_unavailable", "could not encode config request"))?;
        timeout_at(deadline, module_sink.send(frame))
            .await
            .map_err(|_| {
                GateError::new(
                    "config_review_timeout",
                    format!("{module}: config.review exceeded its 30-second deadline; plan remains unapproved"),
                )
            })?
            .map_err(|_| {
                GateError::new(
                    "module_review_unavailable",
                    format!("{module}: target disconnected"),
                )
            })?;
        let outcome = timeout_at(deadline, receiver)
            .await
            .map_err(|_| {
                GateError::new(
                    "config_review_timeout",
                    format!("{module}: config.review exceeded its 30-second deadline; plan remains unapproved"),
                )
            })?
            .map_err(|_| {
                GateError::new(
                    "module_review_unavailable",
                    format!("{module}: target disconnected"),
                )
            })?;
        guard.disarm();
        match outcome {
            ModuleControlRpcOutcome::RawResponse(bytes) => Ok(bytes),
            ModuleControlRpcOutcome::DeadlineElapsed => Err(GateError::new("config_review_timeout",format!("{module}: config.review exceeded its 30-second deadline; plan remains unapproved"))),
            _ => Err(GateError::new(
                "module_review_unavailable",
                format!("{module}: config request was not answered"),
            )),
        }
    }

    /// Execute only the presence part of an engine-selected challenge. The
    /// runtime owner still rechecks the inputs and surface before committing.
    pub async fn confirm_config_gate_presence(
        &self,
        ctx: &RouteCtx,
        frame: &Frame,
        challenge: &ApprovalChallenge,
    ) -> PresenceDecision {
        let Some(text) = challenge.prompt() else {
            return PresenceDecision::Unavailable;
        };
        let caller = self.resolve_caller(ctx.connection_id).await;
        let key = crate::operator_confirm::DeclineKey::for_caller(&caller, ctx.connection_id);
        // Neither privileged_operator_prompt nor operator-app shortcut policy
        // is consulted: every desktop gate review requires OS presence.
        let outcome = self
            .forwarding
            .operator_confirms()
            .approve_config_gate(ctx, frame, text.to_owned(), caller.description(), key)
            .await;
        PresenceDecision::from_operator_outcome(&outcome)
    }
}

fn config_os_prompt_available(provider_ready: bool, policy_present: bool) -> bool {
    provider_ready && policy_present
}

async fn config_work<T: Send + 'static>(
    runtime: Arc<crate::config_gate::runtime::RuntimeOwner>,
    work: impl FnOnce(Arc<crate::config_gate::runtime::RuntimeOwner>) -> std::result::Result<T, GateError>
        + Send
        + 'static,
) -> std::result::Result<T, GateError> {
    tokio::task::spawn_blocking(move || work(runtime))
        .await
        .map_err(|_| GateError::new("config_unavailable", "configuration worker stopped"))?
}

fn config_error_frame(frame: &Frame, error: GateError) -> Result<Frame, RouterError> {
    control_error_body_frame(
        frame,
        ErrorBody {
            code: error.code,
            message: error.detail,
            detail: None,
        },
    )
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod desktop_tests;
