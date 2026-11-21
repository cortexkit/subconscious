use super::*;

/// Observe test request cancellation relative to the connection's cleanup.
#[cfg(test)]
pub(super) struct ActivityRequestPause {
    pub(super) connection: ConnectionId,
    pub(super) started: tokio::sync::Notify,
    pub(super) cancelled: tokio::sync::Notify,
    pub(super) cleanup_started: std::sync::atomic::AtomicBool,
    pub(super) cancelled_after_cleanup: std::sync::atomic::AtomicBool,
}

#[cfg(test)]
struct ActivityRequestCancellation(Arc<ActivityRequestPause>);

#[cfg(test)]
impl Drop for ActivityRequestCancellation {
    fn drop(&mut self) {
        self.0.cancelled_after_cleanup.store(
            self.0
                .cleanup_started
                .load(std::sync::atomic::Ordering::Acquire),
            std::sync::atomic::Ordering::Release,
        );
        self.0.cancelled.notify_one();
    }
}

impl ControlHandler {
    /// Open the ledger on its own writer thread; an unavailable sink does not
    /// prevent the daemon from serving routing or supervisor requests.
    pub fn with_activity_ledger(mut self, run_dir: &Path, incarnation: String) -> Self {
        self.activity = Some(crate::activity::ActivityLedger::open(
            run_dir.join("activity.jsonl"),
            incarnation,
        ));
        self.supervisor.set_activity(self.activity.clone());
        self
    }
}

impl ControlHandler {
    pub(crate) fn with_activity_ledger_config(
        mut self,
        run_dir: &Path,
        incarnation: String,
        config: crate::activity::ActivityConfig,
    ) -> Self {
        self.activity = Some(crate::activity::ActivityLedger::open_config(
            run_dir.join("activity.jsonl"),
            incarnation,
            config,
        ));
        self.supervisor.set_activity(self.activity.clone());
        self
    }

    pub(crate) async fn configure_activity(
        &self,
        config: crate::activity::ActivityConfig,
    ) -> Result<(), &'static str> {
        match &self.activity {
            Some(ledger) => ledger.configure(config).await,
            None => Ok(()),
        }
    }

    pub(super) fn cancel_activity_subscription(
        &self,
        ctx: &RouteCtx,
        frame: &Frame,
    ) -> Result<bool, RouterError> {
        let key = (ctx.connection_id, frame.header.corr);
        let mut streams = self
            .activity_streams
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        // A subscription still being set up is cancelled through the
        // reservation the reader made, under the same stream lock.
        match streams.remove(&key) {
            Some(task) => task.abort(),
            None if self.activity_reservations.cancel(key) => {}
            None => return Ok(false),
        }
        drop(streams);
        let end = Frame::build_with_version(
            response_version(frame),
            FrameType::StreamEnd,
            control_flags(),
            0,
            0,
            frame.header.corr,
            Vec::new(),
        )
        .map_err(|error| RouterError::backend(0, frame.header.corr, error.to_string()))?;
        let sink = ctx.egress.clone();
        tokio::spawn(async move {
            let _ = sink.send(end).await;
        });
        Ok(true)
    }

    pub(super) async fn handle_activity_read(
        &self,
        ctx: &RouteCtx,
        frame: Frame,
    ) -> Result<Vec<Frame>, RouterError> {
        #[cfg(test)]
        {
            let pause = self
                .activity_request_pause
                .lock()
                .unwrap()
                .clone()
                .filter(|pause| pause.connection == ctx.connection_id);
            if let Some(pause) = pause {
                let _cancelled = ActivityRequestCancellation(pause.clone());
                pause.started.notify_one();
                std::future::pending::<()>().await;
            }
        }
        use subc_control::{
            ActivityQueryRequest, ActivitySubscribeRequest, ActivitySubscribeResponse,
            ACTIVITY_MALFORMED, ACTIVITY_READ_FORBIDDEN, ACTIVITY_UNAVAILABLE,
        };
        #[derive(Deserialize)]
        struct Identity {
            #[serde(default)]
            consumer_identity: Option<ConsumerIdentity>,
        }
        // A registration channel or claim-bound connection carries its module in
        // its binding, so no request field can name another principal, and its
        // subscription ends when the bound spawn's authority does.
        let mut bound_spawn = None;
        let legacy = match self.activity_reader(ctx.connection_id) {
            channel_operations::ActivityReader::Module { module_id, spawn } => {
                bound_spawn = Some(spawn);
                Some(Ok(Principal::Reserved { module_id }))
            }
            // Refused outright: falling back to `direct` would admit it.
            channel_operations::ActivityReader::Refused => {
                return Ok(vec![control_error_frame(
                    &frame,
                    ACTIVITY_READ_FORBIDDEN,
                    "helper, relay and retired connections cannot read activity",
                )?]);
            }
            channel_operations::ActivityReader::Legacy => None,
        };
        // A registered module connection already identifies its caller. Ignore
        // consumer_identity there so a module cannot borrow a mirror's identity.
        // Only separate consumer connections use the Direct/nonce trust boundary.
        let principal = match legacy {
            Some(principal) => principal,
            None => match self
                .forwarding
                .module_id_for_connection(ctx.connection_id)
                .map_err(RouterError::Forwarding)?
            {
                Some(module_id) => Ok(Principal::Reserved { module_id }),
                None => {
                    let identity: Identity = match serde_json::from_slice(&frame.body) {
                        Ok(identity) => identity,
                        Err(_) => {
                            return Ok(vec![control_error_frame(
                                &frame,
                                ACTIVITY_MALFORMED,
                                "malformed activity read",
                            )?])
                        }
                    };
                    self.route_open_principal(
                        ctx.connection_id,
                        &frame,
                        identity.consumer_identity,
                    )?
                }
            },
        };
        let admitted = matches!(principal, Ok(Principal::Direct))
            || matches!(&principal, Ok(Principal::Reserved { module_id }) if module_id == "callosum" || module_id == "ckbus");
        if !admitted {
            return Ok(vec![control_error_frame(
                &frame,
                ACTIVITY_READ_FORBIDDEN,
                "activity reads require an operator or trusted mirror principal",
            )?]);
        }
        let Some(ledger) = &self.activity else {
            return Ok(vec![control_error_frame(
                &frame,
                ACTIVITY_UNAVAILABLE,
                "activity ledger unavailable",
            )?]);
        };
        if serde_json::from_slice::<ControlOpProbe>(&frame.body)
            .is_ok_and(|probe| probe.op == ops::ACTIVITY_QUERY)
        {
            let result = match serde_json::from_slice::<ActivityQueryRequest>(&frame.body) {
                Ok(request) => ledger.query(request).await,
                Err(_) => Err(ACTIVITY_MALFORMED),
            };
            return match result {
                Ok(reply) => Ok(vec![control_response_body_frame(
                    &frame,
                    &reply,
                    ops::ACTIVITY_QUERY,
                )?]),
                Err(code) => Ok(vec![control_error_frame(
                    &frame,
                    code,
                    "activity query refused",
                )?]),
            };
        }
        let request: ActivitySubscribeRequest = match serde_json::from_slice(&frame.body) {
            Ok(request) => request,
            Err(_) => {
                return Ok(vec![control_error_frame(
                    &frame,
                    ACTIVITY_MALFORMED,
                    "malformed activity subscription",
                )?])
            }
        };
        let mut subscription = match ledger.subscribe(request.since).await {
            Ok(subscription) => subscription,
            Err(code) => {
                return Ok(vec![control_error_frame(
                    &frame,
                    code,
                    "activity subscription refused",
                )?])
            }
        };
        let key = (ctx.connection_id, frame.header.corr);
        let sink = ctx.egress.clone();
        let mut streams = self
            .activity_streams
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        streams.retain(|_, task| !task.is_finished());
        if self.activity_reservations.cancelled(key) {
            return Ok(Vec::new()); // Its CANCEL already sent STREAM_END.
        }
        if streams.get(&key).is_some_and(|task| !task.is_finished()) {
            return Ok(vec![control_error_frame(
                &frame,
                "duplicate_subscription",
                "activity subscription correlation id already in use",
            )?]);
        }
        let stream = async move {
            loop {
                let entry = if let Some(entry) = subscription.replay.pop_front() {
                    Some(entry)
                } else {
                    subscription.receiver.recv().await
                };
                let Some(entry) = entry else {
                    break;
                };
                let Ok(body) = serde_json::to_vec(&ActivitySubscribeResponse { entry }) else {
                    return;
                };
                let Ok(next) = Frame::build_with_version(
                    response_version(&frame),
                    FrameType::StreamData,
                    control_flags(),
                    0,
                    0,
                    frame.header.corr,
                    body,
                ) else {
                    return;
                };
                if sink.send(next).await.is_err() {
                    return;
                }
            }
            // Dropping a full subscriber's sender drains its accepted entries
            // before the terminal error, preserving their order on the wire.
            if let Some(code) = subscription.terminal_code() {
                if let Ok(end) =
                    control_error_frame(&frame, code, "activity subscriber fell behind")
                {
                    let _ = sink.send(end).await;
                }
            }
        };
        // The spawn's retirement revokes this connection's authority. End the
        // subscription then, rather than when the connection's close is
        // eventually processed or the next entry fails to send.
        let task = tokio::spawn(async move {
            match bound_spawn {
                Some(spawn) => {
                    tokio::select! {
                        () = stream => {}
                        () = spawn.wait_closed() => {}
                    }
                }
                None => stream.await,
            }
        });
        streams.insert(key, task.abort_handle());
        Ok(Vec::new())
    }

    pub(super) async fn handle_activity_record(
        &self,
        ctx: &RouteCtx,
        frame: Frame,
    ) -> Result<Vec<Frame>, RouterError> {
        use subc_control::{
            ActivityRecordRequest, ActivityRecordResponse, ACTIVITY_ACTOR_UNKNOWN,
            ACTIVITY_MALFORMED, ACTIVITY_UNAVAILABLE,
        };
        let result = async {
            let module = self
                .forwarding
                .module_id_for_connection(ctx.connection_id)
                .ok()
                .flatten()
                .ok_or(ACTIVITY_ACTOR_UNKNOWN)?;
            let request: ActivityRecordRequest =
                serde_json::from_slice(&frame.body).map_err(|_| ACTIVITY_MALFORMED)?;
            crate::activity::validate(&module, &request)?;
            let actor = self.activity_actor(ctx.connection_id, &module, request.on.as_ref())?;
            let ledger = self.activity.as_ref().ok_or(ACTIVITY_UNAVAILABLE)?;
            let at_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            ledger.record(module, request, actor, at_ms).await
        }
        .await;
        match result {
            Ok(entry) => Ok(vec![control_response_body_frame(
                &frame,
                &ActivityRecordResponse { seq: entry.seq },
                "activity.record",
            )?]),
            Err(code) => Ok(vec![control_error_frame(
                &frame,
                code,
                "activity recording refused",
            )?]),
        }
    }

    fn activity_actor(
        &self,
        connection: ConnectionId,
        module: &str,
        on: Option<&subc_control::ActivityOn>,
    ) -> Result<subc_control::ActivityActor, &'static str> {
        use crate::forwarding::{DataRoute, DataRouteState};
        use subc_control::{ActivityActor, ActivityOn, ACTIVITY_ACTOR_UNKNOWN};
        let principal = |id: &str| Principal::Reserved {
            module_id: id.to_owned(),
        };
        let mut actor = ActivityActor {
            principal: principal(module),
            scope_owner: None,
            agent_id: None,
            run_id: None,
            flow_id: None,
            project_root: None,
            workdir: None,
            project_id: None,
        };
        let scope = match on {
            None => return Ok(actor),
            Some(ActivityOn::Scope(scope)) => {
                let selector = subc_protocol::scope::ScopeSelector::new(
                    principal(module),
                    scope.scope_ref.clone(),
                    Some(scope.epoch),
                );
                self.scopes
                    .read()
                    .map_err(|_| ACTIVITY_ACTOR_UNKNOWN)?
                    .attribute(&principal(module), module, &selector, true)
                    .map_err(|_| ACTIVITY_ACTOR_UNKNOWN)?
            }
            Some(ActivityOn::Route(route)) => {
                let DataRoute::Module(DataRouteState::Bound(binding)) = self
                    .forwarding
                    .lookup_data_route(connection, route.channel, route.epoch)
                    .map_err(|_| ACTIVITY_ACTOR_UNKNOWN)?
                else {
                    return Err(ACTIVITY_ACTOR_UNKNOWN);
                };
                if binding.module_id != module {
                    return Err(ACTIVITY_ACTOR_UNKNOWN);
                }
                actor.principal = binding.principal.clone();
                actor.project_root = binding
                    .project_root
                    .as_ref()
                    .map(|root| root.as_path().to_path_buf());
                let Some(bound) = &binding.scope else {
                    return Ok(actor);
                };
                let owner = principal(&bound.owner);
                let selector = subc_protocol::scope::ScopeSelector::new(
                    owner.clone(),
                    bound.scope_ref.clone(),
                    Some(bound.tag.scope_epoch),
                );
                let admitted = self
                    .scopes
                    .read()
                    .map_err(|_| ACTIVITY_ACTOR_UNKNOWN)?
                    .attribute(&owner, module, &selector, true)
                    .map_err(|_| ACTIVITY_ACTOR_UNKNOWN)?;
                if admitted.tag != bound.tag {
                    return Err(ACTIVITY_ACTOR_UNKNOWN);
                }
                admitted
            }
        };
        actor.scope_owner = Some(scope.stamp.owner);
        actor.agent_id = scope.stamp.attributes.agent_id;
        actor.run_id = scope.stamp.attributes.run_id;
        actor.flow_id = scope.stamp.attributes.flow_id;
        actor.workdir = scope.stamp.workdir.map(std::path::PathBuf::from);
        actor.project_id = scope.stamp.project_id;
        Ok(actor)
    }
}
