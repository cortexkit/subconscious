use subc_control::config::{ConfigControlRequest, ConfigControlResponse};

use super::*;
mod admission;
mod daemon_readers;
pub(super) use daemon_readers::preview_approved_daemon;
mod delivery_worker;
mod phone;
mod recovery;
mod route_policy;

/// Bound each immutable module review so an unanswered request cannot approve a plan.
const CONFIG_REVIEW_TIMEOUT: Duration = Duration::from_secs(30);

/// After a phone answer, lock waits and freshness work have thirty seconds to
/// reach durable publication. Publication already started returns its real result.
const CONFIG_APPROVAL_COMMIT_TIMEOUT: Duration = Duration::from_secs(30);
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
            | "config.place_program"
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
            Self::PlaceProgram { .. } => None,
            _ => None,
        }
    }
}

impl ControlHandler {
    #[cfg(feature = "test-support")]
    pub(crate) fn with_config_os_prompt_policy_for_tests(self) -> Self {
        if let Some(owner) = self.config_runtime.as_ref() {
            owner.set_os_prompt_policy_for_tests();
        }
        self
    }
    fn config_delivery_target_current(
        &self,
        module: &str,
        connection: ConnectionId,
        active: bool,
        correlation: u64,
    ) -> Result<bool, RouterError> {
        if active {
            Ok(self
                .registry
                .get_module(module)
                .map_err(|error| RouterError::backend(0, correlation, error.to_string()))?
                .is_some_and(|target| target.connection_id == connection)
                && self
                    .forwarding
                    .active_module_endpoint(module)
                    .map_err(RouterError::Forwarding)?
                    .is_some_and(|target| target.connection_id == connection))
        } else {
            Ok(self
                .registry
                .get_candidate(module)
                .map_err(|error| RouterError::backend(0, correlation, error.to_string()))?
                .is_some_and(|target| target.connection_id == connection))
        }
    }

    pub(crate) fn defers_config_approval(&self, frame: &Frame) -> bool {
        frame.header.channel == 0
            && frame.header.ty == FrameType::Request
            && matches!(
                serde_json::from_slice::<ConfigControlRequest>(&frame.body),
                Ok(ConfigControlRequest::Apply { .. })
            )
    }

    pub(crate) fn reserve_config_approval(
        &self,
        ctx: &RouteCtx,
        frame: &Frame,
    ) -> std::result::Result<Option<crate::config_gate::runtime::ApprovalRequestGuard>, GateError>
    {
        if self.defers_config_approval(frame) {
            if let Some(owner) = &self.config_runtime {
                return owner
                    .reserve_approval_request(ctx.connection_id, frame.header.corr)
                    .map(Some);
            }
        }
        Ok(None)
    }

    pub(super) fn cancel_config_approval(&self, caller: ConnectionId, correlation: u64) -> bool {
        use crate::config_gate::runtime::CancelApprovalRequest;
        let Some(owner) = &self.config_runtime else {
            return false;
        };
        match owner.cancel_approval_request_outcome(caller, correlation) {
            CancelApprovalRequest::Unknown => false,
            CancelApprovalRequest::Withdrawn | CancelApprovalRequest::AlreadyReported => true,
            CancelApprovalRequest::TooLate => {
                if let Some(activity) = &self.activity {
                    activity.config_cancel_too_late(caller, correlation);
                }
                true
            }
        }
    }

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
                    use crate::config_gate::runtime::delivery::CONFIG_LIVE_ACK_TIMEOUT;
                    let selected = view
                        .pending
                        .as_ref()
                        .and_then(|view| view.attempt.map(|attempt| (view.generation, attempt)));
                    let window = runtime
                        .on_blocking(move |owner| {
                            if active {
                                if let Some((generation, attempt)) = selected {
                                    owner.delivery_write_started(
                                        connection,
                                        generation,
                                        attempt,
                                        tokio::time::Instant::now(),
                                    )?;
                                }
                            }
                            owner.delivery_window(connection)
                        })
                        .await
                        .map_err(|error| {
                            RouterError::backend(0, frame.header.corr, error.to_string())
                        })?;
                    let write_deadline = window
                        .filter(|window| selected == Some((window.generation, window.attempt)))
                        .and_then(|window| window.deadline)
                        .map_or(
                            tokio::time::Instant::now() + CONFIG_LIVE_ACK_TIMEOUT,
                            |deadline| {
                                deadline.min(tokio::time::Instant::now() + CONFIG_LIVE_ACK_TIMEOUT)
                            },
                        );
                    drop(_gate);
                    let flushed =
                        tokio::time::timeout_at(write_deadline, ctx.egress.send_flushed(response))
                            .await;
                    let flushed_at = tokio::time::Instant::now();
                    let _gate = gate.lock().await;
                    let current_window = runtime
                        .on_blocking(move |owner| owner.delivery_window(connection))
                        .await
                        .ok()
                        .flatten();
                    // A slow writer may finish after cutover or supersession.
                    // Its receipt must not advance the replacement's attempt.
                    if !self.config_delivery_target_current(
                        &registration.manifest.module_id,
                        connection,
                        active,
                        frame.header.corr,
                    )? || selected.is_some_and(|selected| {
                        current_window
                            .is_none_or(|window| selected != (window.generation, window.attempt))
                    }) {
                        return Ok(vec![]);
                    }
                    match flushed {
                        Ok(Ok(())) => {}
                        failed => {
                            if active {
                                if let Some((generation, attempt)) = selected {
                                    let module = registration.manifest.module_id.clone();
                                    let _ = config_work(runtime.clone(), move |owner| {
                                        owner.delivery_timeout(
                                            connection, &module, generation, attempt,
                                        )
                                    })
                                    .await;
                                }
                            }
                            return match failed {
                                Ok(Err(error)) => Err(error),
                                _ => Ok(vec![]),
                            };
                        }
                    }
                    let module = registration.manifest.module_id.clone();
                    let delivered = config_work(runtime.clone(), move |owner| {
                        owner.module_view_delivered(connection, &module, &view, flushed_at, active)
                    })
                    .await;
                    if let Err(error) = delivered {
                        if matches!(
                            error.code.as_str(),
                            "delivery_superseded" | "not_delivery_target"
                        ) {
                            return Ok(vec![]);
                        }
                        if error.code == "delivery_expired" {
                            if active {
                                if let Some((generation, attempt)) = selected {
                                    let module = registration.manifest.module_id.clone();
                                    let _ = config_work(runtime.clone(), move |owner| {
                                        owner.delivery_timeout(
                                            connection, &module, generation, attempt,
                                        )
                                    })
                                    .await;
                                }
                            }
                        } else {
                            return Err(RouterError::backend(
                                0,
                                frame.header.corr,
                                error.to_string(),
                            ));
                        }
                    }
                    Ok(vec![])
                } else {
                    Ok(vec![response])
                }
            }
            Err(error) => {
                if error.code == "anchor_pending" {
                    let _ = config_work(runtime.clone(), move |owner| {
                        owner.anchor_pending_served(connection)
                    })
                    .await;
                }
                Ok(vec![config_error_frame(&frame, error)?])
            }
        }
    }
    pub fn with_config_runtime(
        mut self,
        runtime: Arc<crate::config_gate::runtime::RuntimeOwner>,
    ) -> Self {
        self.supervisor.install_binding_retirement(&runtime);
        let _ = runtime.set_activity(self.activity.clone());
        runtime.install_module_route_authority(Arc::new(route_policy::ModuleRouteAuthority(
            self.clone(),
            Arc::downgrade(&runtime),
        )));
        self.config_runtime = Some(runtime);
        self
    }

    /// Only bootstrap's explicit store-root path advertises configuration authority.
    pub(crate) fn with_explicit_config_store(
        self,
        runtime: Arc<crate::config_gate::runtime::RuntimeOwner>,
    ) -> Self {
        let mut handler = self.with_config_runtime(runtime);
        let capability = subc_protocol::config::CONFIG_GATE_CAPABILITY;
        if !handler
            .subc_capabilities
            .iter()
            .any(|present| present == capability)
        {
            let mut capabilities = handler.subc_capabilities.to_vec();
            capabilities.push(capability.to_owned());
            handler.subc_capabilities = capabilities.into();
        }
        handler
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
            // Requesting and receiving the complete immutable review is the
            // development caller's desktop presentation, not app attestation.
            Ok(self
                .local_connections
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .contains(&connection))
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
            owner.desktop_view_delivered_when(connection, &view, || {
                peers
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .contains_key(&connection)
            })
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
        self.config_review_admission_is_default = false;
        self
    }

    pub(super) fn review_admission(&self) -> Arc<dyn crate::config_gate::ReviewAdmission> {
        if self.config_runtime.is_some() && self.config_review_admission_is_default {
            Arc::new(admission::SupervisedReviewAdmission(self.clone()))
        } else {
            self.config_review_admission.clone()
        }
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
            ConfigControlRequest::PlaceProgram {
                module_id,
                path,
                sha256,
            } => {
                let gate = runtime.module_gate(&module_id)?;
                let _guard = gate.lock().await;
                let generation = config_work(runtime, move |owner| {
                    owner.place_program(&module_id, &path, &sha256)
                })
                .await?;
                Ok(ConfigControlResponse::ProgramPlaced { generation })
            }
            ConfigControlRequest::Status {} => {
                let snapshot = runtime.polled_snapshot()?;
                let captured = snapshot.clone();
                let mut status = runtime
                    .on_blocking(move |owner| owner.polled_status_from(&captured))
                    .await?;
                let phone = self
                    .config_phone_surface_checked(false, Some(&snapshot))
                    .await;
                let facts = self.config_approval_facts_with_phone(ctx, phone).await;
                status
                    .missing_approval_surfaces
                    .extend(crate::config_gate::missing_surfaces(
                        &facts,
                        matches!(status.state.as_str(), "store_missing" | "store_unverified"),
                    ));
                if let Err(error) = self.config_desktop_identity(ctx.connection_id) {
                    status.missing_approval_surfaces.push(error.code);
                }
                self.report_daemon_configuration(&snapshot.approved_daemon, &mut status)
                    .await?;
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
                let connection = ctx.connection_id;
                let corr = frame.header.corr;
                #[cfg(all(test, unix))]
                runtime.pause_approval_claim_for_test().await;
                let request_guard = runtime.claim_approval_request(connection, corr)?;
                if self.rescan.is_none() {
                    return Err(GateError::new(
                        "supervisor_reconcile_unavailable",
                        "configuration commit requires the supervisor owner",
                    ));
                }
                let facts = self.config_approval_facts(ctx).await;
                let cancellation = request_guard.request.clone();
                let (challenge, mut attempt_guard) = config_work(runtime.clone(), move |owner| {
                    let challenge =
                        owner.begin_approval_for_request(&digest, &facts, Some(&cancellation))?;
                    // The job owns slot cleanup even if its async caller drops
                    // before this blocking result can be delivered.
                    let guard = phone::ApprovalAttemptGuard::new(&owner, challenge.attempt());
                    owner.track_reserved_approval_request(
                        challenge.attempt(),
                        connection,
                        corr,
                        &cancellation,
                    )?;
                    Ok((challenge, guard))
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
                request_guard.request.ensure_live()?;
                let reply = match &challenge {
                    ApprovalChallenge::Desktop { .. } | ApprovalChallenge::OsPrompt { .. } => {
                        ApprovalReply::Presence(
                            self.confirm_config_gate_presence(ctx, frame, &challenge)
                                .await,
                        )
                    }
                    ApprovalChallenge::Phone {
                        attempt,
                        connection,
                        deadline,
                        request,
                    } => {
                        self.config_phone_rpc(
                            &runtime,
                            *attempt,
                            ConnectionId::new(*connection),
                            *deadline,
                            request,
                        )
                        .await?
                    }
                };
                let attempt = challenge.attempt();
                let window = matches!(challenge, ApprovalChallenge::Phone { .. }).then(|| {
                    Arc::new(crate::config_gate::runtime::ApprovalCommitWindow::new(
                        Instant::now() + CONFIG_APPROVAL_COMMIT_TIMEOUT,
                    ))
                });
                attempt_guard.commit_window = window.clone();
                let checked_phone = self.config_phone_surface().await;
                let locks = async {
                    let operation = self.supervisor.operation_lock().lock_owned().await;
                    let module_gates = runtime.approval_commit_gates(attempt).await?;
                    let current_phone = self.config_phone_surface_snapshot().await;
                    let phone = checked_phone.filter(|phone| Some(phone) == current_phone.as_ref());
                    let facts = self.config_approval_facts_with_phone(ctx, phone).await;
                    Ok::<_, GateError>((operation, module_gates, facts))
                };
                let (operation, module_gates, facts) = if let Some(window) = &window {
                    timeout_at(window.deadline, locks)
                        .await
                        .map_err(|_| GateError::commit_timeout())??
                } else {
                    locks.await?
                };
                let now = now_ms();
                let publish_window = window.clone();
                let publication_request = request_guard.request.clone();
                let exit_runtime = runtime.clone();
                let work = config_work(runtime, move |owner| {
                    let _owned_gates = (operation, module_gates);
                    owner.finish_approval_with_commit_window(
                        attempt,
                        &facts,
                        reply,
                        now,
                        publish_window,
                        publication_request,
                    )
                });
                tokio::pin!(work);
                let generation = if let Some(window) = window {
                    tokio::select! {
                        result = &mut work => result?,
                        _ = tokio::time::sleep_until(window.deadline) => {
                            if window.expire() { return Err(GateError::commit_timeout()); }
                            work.await?
                        }
                    }
                } else {
                    work.await?
                };
                self.reconcile_authority_exit(exit_runtime.clone(), generation)
                    .await?;
                let restart_required = self
                    .daemon_restart_required(&exit_runtime, generation)
                    .await?;
                Ok(ConfigControlResponse::Applied {
                    generation,
                    restart_required,
                })
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
                    let admission = self.review_admission();
                    let reviewer = registration.clone();
                    config_work(runtime.clone(), move |_| admission.check(&reviewer)).await?;
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
        let phone = self.config_phone_surface().await;
        self.config_approval_facts_with_phone(ctx, phone).await
    }

    async fn config_approval_facts_with_phone(
        &self,
        ctx: &RouteCtx,
        phone: Option<crate::config_gate::PhoneSurface>,
    ) -> ApprovalFacts {
        let os_policy = crate::config_gate::os_prompt_policy_present();
        #[cfg(any(test, feature = "test-support"))]
        let os_policy = os_policy
            || self
                .config_runtime
                .as_ref()
                .is_some_and(|owner| owner.os_prompt_policy_for_tests());
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
                os_policy,
            ),
            phone,
            caller: self.resolve_caller(ctx.connection_id).await,
        }
    }

    // The caller holds Callosum's module gate while allocating its RPC or
    // committing approval. This snapshot grants nothing until revalidated there.
    async fn config_phone_surface(&self) -> Option<crate::config_gate::PhoneSurface> {
        self.config_phone_surface_checked(true, None).await
    }

    async fn config_phone_surface_snapshot(&self) -> Option<crate::config_gate::PhoneSurface> {
        self.config_phone_surface_checked(false, None).await
    }

    async fn config_phone_surface_checked(
        &self,
        content: bool,
        publication: Option<&crate::config_gate::runtime::Published>,
    ) -> Option<crate::config_gate::PhoneSurface> {
        if self.config_runtime.as_ref().is_some_and(|owner| {
            publication.map_or_else(|| owner.is_recovering(), |view| view.is_recovering())
        }) {
            return None;
        }
        if !self.operator_app_policy.phone_enabled {
            return None;
        }
        let runtime = self.config_runtime.as_ref()?;
        let (_, registrations) = self.registry.list_modules_with_nonce_verification().ok()?;
        let (registration, verified) = registrations
            .into_iter()
            .find(|(registration, _)| registration.manifest.module_id == "callosum")?;
        if !verified
            || !registration.ready
            || !registration
                .control_ops
                .iter()
                .any(|op| op == subc_protocol::consent::CONSENT_V1)
        {
            return None;
        }
        let _spawn = self
            .supervisor
            .registration_spawn(registration.connection_id)?;
        if !self
            .forwarding
            .active_module_endpoint("callosum")
            .ok()
            .flatten()
            .is_some_and(|endpoint| endpoint.connection_id == registration.connection_id)
        {
            return None;
        }
        // Configuration approval requires a channel-bound phone launch.
        // Enabling configuration gating invalidates legacy launch nonces,
        // including an old registration whose process is still draining.
        let module = self.supervisor.get("callosum")?;
        let status = module.status().ok()?;
        let (spec, _) = module.configuration().ok()?;
        if !self.supervisor.registration_snapshot_is_current_live(
            &self.registry,
            "callosum",
            registration.connection_id,
            None,
        ) || !spec.reserved
            || !status.enabled
            || !status.process_alive
            || !status.registration_active
        {
            return None;
        }
        let connection = registration.connection_id;
        let approved = if content {
            runtime
                .on_blocking(move |owner| owner.approved_phone_entry(connection))
                .await
                .ok()??
        } else {
            if let Some(snapshot) = publication {
                runtime.polled_phone_entry_from(snapshot, connection)?
            } else {
                runtime.polled_phone_entry(connection)?
            }
        };
        if !approved.enabled || approved.module_spec() != spec {
            return None;
        }
        if content {
            let supervisor = self.supervisor.clone();
            let registry = self.registry.clone();
            if !runtime
                .on_blocking(move |_| {
                    Ok(supervisor
                        .registration_is_current_live(&registry, "callosum", connection, None))
                })
                .await
                .ok()?
            {
                return None;
            }
        }
        if !self.supervisor.registration_snapshot_is_current_live(
            &self.registry,
            "callosum",
            registration.connection_id,
            None,
        ) {
            return None;
        }
        Some(crate::config_gate::PhoneSurface {
            module_id: "callosum".into(),
            connection: connection.get(),
            spawn_generation: status.spawn_generation,
            enabled_by_policy: true,
            opted_in: true,
            approved_declaration: true,
            reserved: true,
            channel_registered: true,
            program_admitted: true,
            active_registration: true,
        })
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
    runtime
        .on_blocking(move |owner| {
            let result = work(owner.clone());
            owner.notify_delivery_changed();
            result
        })
        .await
}

pub(super) fn config_error_frame(frame: &Frame, error: GateError) -> Result<Frame, RouterError> {
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
