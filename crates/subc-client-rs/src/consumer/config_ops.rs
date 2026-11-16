//! Typed channel-0 settings calls. Use these only when `server.describe`
//! advertises `config.settings/v1`, over [`SubcConsumer::connect_local`]: the
//! daemon requires a kernel-observed local owner, not just the connection key.
//!
//! Daemon refusals are [`CallError::Module`], with codes available through
//! [`CallError::code`]. Reads retry transport failures within the call deadline;
//! planning, discarding and applying are sent once, because an unknown outcome
//! may already have created, removed or applied the plan.

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use subc_control::config::{ConfigControlRequest, ConfigControlResponse, ConfigWatchResponse};
pub use subc_control::config::{
    ConfigEdit, ConfigEditAction, ConfigModule, ConfigPlanRefusal, ConfigPlanView,
    ConfigSettingsLayer, ConfigThrough, ConfigWatchEvent, Digest,
};
use tokio::{sync::Semaphore, time::Instant};

use super::{
    is_retryable_catalog_transport_error, AdmissionClass, CallError, Priority, RoutePermit,
    SubcConsumer, Subscription, SubscriptionEnd, SubscriptionSend, TerminalFrame,
};

// The CLI reserves the same budget for the daemon's operator response, beyond
// the ordinary control round trip, so a person can review a stamped apply.
const OPERATOR_RESPONSE_BUDGET: Duration = Duration::from_secs(290);
const CONFIG_EVENT_BUFFER: usize = 256;

/// Settings module rows and whether the install has an approval stamp.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigModulesResponse {
    pub modules: Vec<ConfigModule>,
    pub stamped: bool,
}

/// The admitted module schema, including its settings annotations.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigSchemaResponse {
    pub module_id: String,
    pub schema_digest: Digest,
    pub schema: serde_json::Value,
}

/// Effective settings and the layer documents that supplied them.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigShowResponse {
    pub module_id: String,
    pub project_id: Option<String>,
    pub through: Option<ConfigThrough>,
    pub stamped: bool,
    pub base_generation: u64,
    pub effective: serde_json::Value,
    pub sources: BTreeMap<String, subc_config::Layer>,
    pub layers: Vec<ConfigSettingsLayer>,
    pub pending_plan: Option<Digest>,
}

/// A completed apply; a stamped generation may still await activation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigApplyResponse {
    pub generation: u64,
    pub restart_required: bool,
}

impl SubcConsumer {
    async fn config_request(
        &self,
        request: ConfigControlRequest,
        retry: bool,
    ) -> Result<ConfigControlResponse, CallError> {
        let op = request.op();
        let timeout = if matches!(request, ConfigControlRequest::Apply { .. }) {
            self.shared
                .opts
                .call_timeout
                .saturating_add(OPERATOR_RESPONSE_BUDGET)
        } else {
            self.shared.opts.call_timeout
        };
        let deadline = Instant::now() + timeout;
        let body = serde_json::to_vec(&request)
            .map_err(|err| CallError::not_sent(format!("failed to encode {op}: {err}")))?;
        let mut attempt = 0usize;
        loop {
            attempt = attempt.saturating_add(1);
            match self
                .shared
                .control_call(body.clone(), deadline, false, None)
                .await
            {
                Ok(TerminalFrame::Response { body, .. }) => {
                    let response: ConfigControlResponse =
                        serde_json::from_slice(&body).map_err(|err| {
                            CallError::invalid_response(format!(
                                "failed to decode {op} response: {err}"
                            ))
                        })?;
                    return Ok(response);
                }
                Ok(TerminalFrame::Error { body, .. }) => return Err(CallError::Module(body)),
                Ok(TerminalFrame::StreamEnd) => {
                    return Err(CallError::invalid_response(format!(
                        "{op} returned StreamEnd"
                    )))
                }
                Err(err)
                    if retry
                        && is_retryable_catalog_transport_error(&err)
                        && Instant::now() < deadline =>
                {
                    let delay = self.shared.jittered(
                        self.shared
                            .opts
                            .reconnect_backoff
                            .delay_after_attempt(attempt),
                    );
                    self.shared.sleep_until_retry(deadline, delay).await?;
                }
                Err(err) => return Err(err),
            }
        }
    }

    /// List gated and ungated modules without prompting (`config.modules`).
    /// Status strings are open; display unknown statuses as raw text. Like all
    /// settings calls, this can refuse with `owner_required` on a nonlocal connection.
    pub async fn config_modules(&self) -> Result<ConfigModulesResponse, CallError> {
        match self
            .config_request(ConfigControlRequest::Modules {}, true)
            .await?
        {
            ConfigControlResponse::Modules { modules, stamped } => {
                Ok(ConfigModulesResponse { modules, stamped })
            }
            _ => Err(unexpected("config.modules")),
        }
    }

    /// Read an admitted schema (`config.schema`), never prompting. Expect
    /// `unknown_module` for an unknown id or `module_ungated` for one without a schema.
    pub async fn config_schema(
        &self,
        module_id: impl Into<String>,
    ) -> Result<ConfigSchemaResponse, CallError> {
        match self
            .config_request(
                ConfigControlRequest::Schema {
                    module_id: module_id.into(),
                },
                true,
            )
            .await?
        {
            ConfigControlResponse::Schema {
                module_id,
                schema_digest,
                schema,
            } => Ok(ConfigSchemaResponse {
                module_id,
                schema_digest,
                schema,
            }),
            _ => Err(unexpected("config.schema")),
        }
    }

    /// Read approved settings, or current file settings at generation zero when
    /// unstamped (`config.show`). `through` limits the merged layers. Expect
    /// `unknown_module`, `module_ungated`, or `unknown_project` for unavailable selectors.
    pub async fn config_show(
        &self,
        module_id: impl Into<String>,
        project_id: Option<String>,
        through: Option<ConfigThrough>,
    ) -> Result<ConfigShowResponse, CallError> {
        match self
            .config_request(
                ConfigControlRequest::Show {
                    module_id: module_id.into(),
                    project_id,
                    through,
                },
                true,
            )
            .await?
        {
            ConfigControlResponse::Show {
                module_id,
                project_id,
                through,
                stamped,
                base_generation,
                effective,
                sources,
                layers,
                pending_plan,
            } => Ok(ConfigShowResponse {
                module_id,
                project_id,
                through,
                stamped,
                base_generation,
                effective,
                sources,
                layers,
                pending_plan,
            }),
            _ => Err(unexpected("config.show")),
        }
    }

    /// Prepare a reviewable plan without prompting (`config.plan`). Pass both
    /// arguments as `None` to plan file drift (requires a stamp, otherwise
    /// `config_gated`), or supply edits and each touched
    /// module's `config.show` base generation. Missing bases are `invalid_params`;
    /// moved bases are `config_changed_since_review`. Field refusals (including
    /// `layer_not_served` for workspace/project edits) are in `view.refusals` and
    /// block the whole plan. Sent once, never retried.
    pub async fn config_plan(
        &self,
        base_generations: Option<BTreeMap<String, u64>>,
        edits: Option<Vec<ConfigEdit>>,
    ) -> Result<ConfigPlanView, CallError> {
        match self
            .config_request(
                ConfigControlRequest::Plan {
                    base_generations,
                    edits,
                },
                false,
            )
            .await?
        {
            ConfigControlResponse::Plan { view } => Ok(view),
            _ => Err(unexpected("config.plan")),
        }
    }

    /// Retrieve the pending review view by its full digest (`config.plan_get`).
    /// `unknown_plan` means it was applied, discarded, expired or superseded.
    pub async fn config_plan_get(&self, digest: Digest) -> Result<ConfigPlanView, CallError> {
        match self
            .config_request(ConfigControlRequest::PlanGet { digest }, true)
            .await?
        {
            ConfigControlResponse::PlanGet { view } => Ok(view),
            _ => Err(unexpected("config.plan_get")),
        }
    }

    /// Drop a pending plan without applying or prompting (`config.plan_discard`).
    /// Expect `unknown_plan` if gone or `plan_in_review` while being approved.
    /// Sent once, never retried.
    pub async fn config_plan_discard(&self, digest: Digest) -> Result<(), CallError> {
        match self
            .config_request(ConfigControlRequest::PlanDiscard { digest }, false)
            .await?
        {
            ConfigControlResponse::PlanDiscard {} => Ok(()),
            _ => Err(unexpected("config.plan_discard")),
        }
    }

    /// Apply exactly the reviewed digest (`config.apply`). Unstamped installs
    /// apply immediately; stamped installs wait for trusted operator approval.
    /// The timeout adds the daemon's 290-second `OPERATOR_RESPONSE_BUDGET` to the
    /// consumer's control-call timeout, as `ck config apply` does. Expect
    /// `plan_unknown`, `plan_refused`, `config_changed_since_review`, or approval
    /// refusal codes. Sent once: after a timeout or connection loss, re-read
    /// settings before deciding what to do, because the apply may have committed.
    pub async fn config_apply(&self, digest: Digest) -> Result<ConfigApplyResponse, CallError> {
        match self
            .config_request(ConfigControlRequest::Apply { digest }, false)
            .await?
        {
            ConfigControlResponse::Applied {
                generation,
                restart_required,
            } => Ok(ConfigApplyResponse {
                generation,
                restart_required,
            }),
            _ => Err(unexpected("config.apply")),
        }
    }

    /// Watch module settings rows (`config.watch`), optionally filtered by id.
    /// Every watch starts with a resync; any later resync replaces all state.
    /// Re-read settings/schema on changes. A reconnect ends the subscription;
    /// open another watch for a fresh resync. Daemon refusals arrive through
    /// [`ConfigSubscription::next`] and retain their code; inspect
    /// [`ConfigSubscription::ended`] for the complete terminal reason.
    pub async fn config_watch(
        &self,
        modules: Option<Vec<String>>,
    ) -> Result<ConfigSubscription, CallError> {
        let deadline = Instant::now() + self.shared.opts.call_timeout;
        let body = serde_json::to_vec(&ConfigControlRequest::Watch { modules })
            .map_err(|err| CallError::not_sent(format!("failed to encode config.watch: {err}")))?;
        loop {
            self.shared.ensure_connected_for_call(deadline).await?;
            // Keep a permit alive until this watch ends, as route subscriptions do.
            let permit = Arc::new(Semaphore::new(1))
                .acquire_owned()
                .await
                .map_err(|_| CallError::not_sent("config subscription permit closed"))?;
            match self
                .shared
                .send_subscription(SubscriptionSend {
                    expected_handle: None,
                    channel: 0,
                    epoch: 0,
                    body: body.clone(),
                    priority: Priority::Interactive,
                    admission_class: AdmissionClass::Normal,
                    event_buffer: CONFIG_EVENT_BUFFER,
                    deadline,
                    permit: RoutePermit::legacy(permit),
                })
                .await
            {
                Ok(inner) => {
                    return Ok(ConfigSubscription {
                        inner,
                        finished: false,
                    })
                }
                Err(err) if err.is_not_sent() && Instant::now() < deadline => continue,
                Err(err) => return Err(err),
            }
        }
    }
}

fn unexpected(op: &str) -> CallError {
    CallError::invalid_response(format!("{op} returned an unexpected control response"))
}

/// A held settings watch. Dropping it sends a best-effort cancellation.
pub struct ConfigSubscription {
    inner: Subscription,
    finished: bool,
}

impl ConfigSubscription {
    /// Yield queued events before reporting a terminal error. `Ok(None)` means
    /// a clean daemon end or caller cancellation; use [`Self::ended`] to tell
    /// them apart. A malformed event cancels the watch and returns a decode error.
    pub async fn next(&mut self) -> Result<Option<ConfigWatchEvent>, CallError> {
        if self.finished {
            return Ok(None);
        }
        if let Some(body) = self.inner.events().recv().await {
            return serde_json::from_slice::<ConfigWatchResponse>(&body)
                .map(|response| Some(response.event))
                .map_err(|err| {
                    self.finished = true;
                    let _ = self.inner.unsubscribe();
                    CallError::invalid_response(format!(
                        "failed to decode config.watch event: {err}"
                    ))
                });
        }
        let result = self.inner.closed().await;
        self.finished = true;
        result.map(|()| None)
    }

    /// Wait for the terminal outcome, independently of draining events. A daemon
    /// refusal/closure retains its code, message and detail in `Error`; a lost
    /// connection is `ConnectionLost`, and local unsubscribe is `Cancelled`.
    /// Clean daemon endings are `StreamEnd` or `Response` (with its body).
    pub async fn ended(&mut self) -> SubscriptionEnd {
        self.inner.ended().await
    }

    /// Cancel the held request; [`Self::ended`] reports `Cancelled` promptly.
    pub fn unsubscribe(&self) -> Result<(), CallError> {
        self.inner.unsubscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::super::{dispatch_frame, ConsumerOptions, Shared, WriteCommand};
    use super::*;
    use serde_json::{json, Value};
    use subc_protocol::{ErrorBody, Flags, Frame, FrameType};
    use tokio::sync::mpsc;

    fn consumer() -> (SubcConsumer, mpsc::Receiver<WriteCommand>) {
        let shared = Arc::new(Shared::new(
            "unused-config-test-connection".into(),
            ConsumerOptions::default(),
        ));
        let (writer, rx) = mpsc::channel(32);
        shared.lock_inner().writer = Some(writer);
        shared.lock_inner().daemon_ver = "0.50.0".into();
        (SubcConsumer { shared }, rx)
    }

    fn frame(kind: FrameType, corr: u64, body: Value) -> Frame {
        Frame::build(
            kind,
            Flags::new(false, Priority::Interactive, false),
            0,
            0,
            corr,
            if kind == FrameType::StreamEnd {
                vec![]
            } else {
                serde_json::to_vec(&body).unwrap()
            },
        )
        .unwrap()
    }

    async fn request(rx: &mut mpsc::Receiver<WriteCommand>, expected: Value) -> Frame {
        let command = rx.recv().await.unwrap();
        assert_eq!(command.frame.header.channel, 0);
        assert_eq!(command.frame.header.ty, FrameType::Request);
        assert_eq!(
            serde_json::from_slice::<Value>(&command.frame.body).unwrap(),
            expected
        );
        command.frame
    }

    async fn respond(shared: &Arc<Shared>, request: &Frame, reply: Value) {
        assert!(
            dispatch_frame(
                shared,
                1,
                frame(FrameType::Response, request.header.corr, reply)
            )
            .await
        );
    }

    fn plan() -> ConfigPlanView {
        ConfigPlanView::new(subc_config::Plan::new(
            0,
            0,
            "test-daemon",
            Default::default(),
            subc_config::Changeset::new(),
        ))
        .unwrap()
    }

    #[tokio::test]
    async fn config_reads_send_selectors_and_decode_typed_responses() {
        let (consumer, mut rx) = consumer();
        let shared = consumer.shared.clone();
        let call = tokio::spawn(async move {
            let modules = consumer.config_modules().await.unwrap();
            assert!(!modules.stamped);
            assert_eq!(modules.modules[0].status, "future_status");
            let schema = consumer.config_schema("demo").await.unwrap();
            assert_eq!(schema.module_id, "demo");
            assert_eq!(schema.schema, json!({"type":"object"}));
            let show = consumer
                .config_show(
                    "demo",
                    Some("project-a".into()),
                    Some(ConfigThrough::Default),
                )
                .await
                .unwrap();
            assert_eq!(show.project_id.as_deref(), Some("project-a"));
            assert_eq!(show.through, Some(ConfigThrough::Default));
            assert_eq!(show.base_generation, 7);
            assert_eq!(show.effective, json!({"limit":1}));
            assert_eq!(show.sources["/limit"], subc_config::Layer::Default);
            assert_eq!(show.layers[0].document, json!({"limit":1}));
            assert!(show.pending_plan.is_some());
        });
        let req = request(&mut rx, json!({"op":"config.modules"})).await;
        respond(&shared, &req, json!({"op":"config.modules","stamped":false,"modules":[{"module_id":"demo","gated":true,"status":"future_status"}]})).await;
        let req = request(&mut rx, json!({"op":"config.schema","module_id":"demo"})).await;
        respond(&shared, &req, json!({"op":"config.schema","module_id":"demo","schema_digest":subc_config::raw_digest(b"schema"),"schema":{"type":"object"}})).await;
        let req = request(&mut rx, json!({"op":"config.show","module_id":"demo","project_id":"project-a","through":"default"})).await;
        respond(&shared, &req, json!({"op":"config.show","module_id":"demo","project_id":"project-a","through":"default","stamped":true,"base_generation":7,"effective":{"limit":1},"sources":{"/limit":"default"},"layers":[{"layer":"default","document":{"limit":1}}],"pending_plan":subc_config::raw_digest(b"pending")})).await;
        call.await.unwrap();
    }

    #[tokio::test]
    async fn config_plans_send_edits_bases_and_digests_and_preserve_refusals() {
        let (consumer, mut rx) = consumer();
        let shared = consumer.shared.clone();
        let mut view = plan();
        view.refusals.push(ConfigPlanRefusal {
            module_id: "demo".into(),
            path: "/limit".into(),
            code: "value_invalid".into(),
            message: "invalid limit".into(),
        });
        let expected = view.clone();
        let call = tokio::spawn(async move {
            let edits = vec![ConfigEdit {
                module_id: "demo".into(),
                layer: "user".into(),
                workspace_id: None,
                project_id: None,
                path: "/limit".into(),
                action: ConfigEditAction::Set { set: json!(4) },
            }];
            let planned = consumer
                .config_plan(Some(BTreeMap::from([("demo".into(), 7)])), Some(edits))
                .await
                .unwrap();
            assert_eq!(planned, expected);
            assert_eq!(
                consumer
                    .config_plan_get(planned.digest.clone())
                    .await
                    .unwrap(),
                planned
            );
            consumer.config_plan_discard(planned.digest).await.unwrap();
            consumer.config_plan(None, None).await.unwrap();
        });
        let req = request(&mut rx, json!({"op":"config.plan","base_generations":{"demo":7},"edits":[{"module_id":"demo","layer":"user","path":"/limit","set":4}]})).await;
        respond(&shared, &req, json!({"op":"config.plan","view":view})).await;
        let req = request(
            &mut rx,
            json!({"op":"config.plan_get","digest":view.digest}),
        )
        .await;
        respond(&shared, &req, json!({"op":"config.plan_get","view":view})).await;
        let req = request(
            &mut rx,
            json!({"op":"config.plan_discard","digest":view.digest}),
        )
        .await;
        respond(&shared, &req, json!({"op":"config.plan_discard"})).await;
        let req = request(&mut rx, json!({"op":"config.plan"})).await;
        respond(&shared, &req, json!({"op":"config.plan","view":plan()})).await;
        call.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn config_apply_waits_for_the_operator_response_budget() {
        let (consumer, mut rx) = consumer();
        let shared = consumer.shared.clone();
        let digest = subc_config::raw_digest(b"apply");
        let expected_digest = digest.clone();
        let call = tokio::spawn(async move { consumer.config_apply(digest).await });
        let req = request(
            &mut rx,
            json!({"op":"config.apply","digest":expected_digest}),
        )
        .await;
        // Even a reply near the end of the daemon's review budget must be accepted.
        tokio::time::advance(Duration::from_secs(319)).await;
        assert!(!call.is_finished());
        respond(
            &shared,
            &req,
            json!({"op":"config.apply","generation":8,"restart_required":true}),
        )
        .await;
        assert_eq!(
            call.await.unwrap().unwrap(),
            ConfigApplyResponse {
                generation: 8,
                restart_required: true
            }
        );
    }

    #[tokio::test]
    async fn config_refusals_keep_codes_and_mutations_are_not_retried_after_loss() {
        for op in ["config.plan", "config.plan_discard", "config.apply"] {
            let (consumer, mut rx) = consumer();
            let shared = consumer.shared.clone();
            let call = tokio::spawn(async move {
                match op {
                    "config.plan" => consumer.config_plan(None, None).await.map(|_| ()),
                    "config.plan_discard" => {
                        consumer
                            .config_plan_discard(subc_config::raw_digest(b"plan"))
                            .await
                    }
                    _ => consumer
                        .config_apply(subc_config::raw_digest(b"plan"))
                        .await
                        .map(|_| ()),
                }
            });
            rx.recv().await.unwrap();
            shared.handle_generation_drop(1, "test connection lost".into());
            assert!(call.await.unwrap().is_err());
            assert!(rx.try_recv().is_err());
        }
        let (consumer, mut rx) = consumer();
        let shared = consumer.shared.clone();
        let call = tokio::spawn(async move { consumer.config_schema("missing").await });
        let req = request(&mut rx, json!({"op":"config.schema","module_id":"missing"})).await;
        let body = ErrorBody::new("unknown_module", "no such module");
        dispatch_frame(
            &shared,
            1,
            frame(FrameType::Error, req.header.corr, json!(body)),
        )
        .await;
        assert_eq!(
            call.await.unwrap().unwrap_err().code(),
            Some("unknown_module")
        );
    }

    #[tokio::test]
    async fn config_reads_retry_a_lost_reply_with_the_same_request() {
        let (consumer, mut rx) = consumer();
        let shared = consumer.shared.clone();
        let call = tokio::spawn(async move { consumer.config_modules().await });
        let first = rx.recv().await.unwrap();
        let key = first.pending.unwrap();
        assert!(shared.lock_inner().pending.remove(&key).is_some());
        let second = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.frame.body, second.frame.body);
        assert_ne!(first.frame.header.corr, second.frame.header.corr);
        respond(
            &shared,
            &second.frame,
            json!({"op":"config.modules","modules":[],"stamped":false}),
        )
        .await;
        assert!(call.await.unwrap().unwrap().modules.is_empty());
    }

    #[tokio::test]
    async fn config_reads_reject_mismatched_malformed_and_stream_end_replies() {
        for (kind, reply) in [
            (FrameType::Response, json!({"op":"config.plan_discard"})),
            (
                FrameType::Response,
                json!({"op":"config.modules","modules":"bad"}),
            ),
            (FrameType::StreamEnd, Value::Null),
        ] {
            let (consumer, mut rx) = consumer();
            let shared = consumer.shared.clone();
            let call = tokio::spawn(async move { consumer.config_modules().await });
            let req = request(&mut rx, json!({"op":"config.modules"})).await;
            dispatch_frame(&shared, 1, frame(kind, req.header.corr, reply)).await;
            assert!(call.await.unwrap().is_err());
        }
    }

    #[tokio::test]
    async fn config_watch_yields_resync_change_and_daemon_closure_reason() {
        let (consumer, mut rx) = consumer();
        let mut watch = consumer
            .config_watch(Some(vec!["demo".into()]))
            .await
            .unwrap();
        let req = request(&mut rx, json!({"op":"config.watch","modules":["demo"]})).await;
        let events = [
            json!({"op":"config.watch","event":"resync","modules":[],"stamped":false}),
            json!({"op":"config.watch","event":"change","module_id":"demo","status":"pending_activation","approved_generation":8}),
            json!({"op":"config.watch","event":"resync","modules":[],"stamped":true}),
        ];
        for event in &events {
            dispatch_frame(
                &consumer.shared,
                1,
                frame(FrameType::StreamData, req.header.corr, event.clone()),
            )
            .await;
        }
        let error = ErrorBody::new(
            "config_store_unavailable",
            "the settings store is unavailable",
        );
        dispatch_frame(
            &consumer.shared,
            1,
            frame(FrameType::Error, req.header.corr, json!(error)),
        )
        .await;
        assert_eq!(watch.ended().await, SubscriptionEnd::Error(error.clone()));
        for event in events {
            let expected: ConfigWatchResponse = serde_json::from_value(event).unwrap();
            assert_eq!(watch.next().await.unwrap(), Some(expected.event));
        }
        assert_eq!(
            watch.next().await.unwrap_err().code(),
            Some(error.code.as_str())
        );
        assert_eq!(watch.next().await.unwrap(), None);
        assert_eq!(watch.ended().await, SubscriptionEnd::Error(error));
    }

    #[tokio::test]
    async fn config_watch_distinguishes_connection_loss_cancellation_and_clean_end() {
        let (consumer, mut rx) = consumer();
        let mut watch = consumer.config_watch(None).await.unwrap();
        request(&mut rx, json!({"op":"config.watch"})).await;
        consumer
            .shared
            .handle_generation_drop(1, "test connection lost".into());
        assert_eq!(watch.ended().await, SubscriptionEnd::ConnectionLost);
        assert!(watch.next().await.is_err());

        let (consumer, mut rx) = self::consumer();
        let mut watch = consumer.config_watch(None).await.unwrap();
        let req = request(&mut rx, json!({"op":"config.watch"})).await;
        watch.unsubscribe().unwrap();
        assert_eq!(watch.ended().await, SubscriptionEnd::Cancelled);
        assert_eq!(watch.next().await.unwrap(), None);
        let cancel = rx.recv().await.unwrap().frame;
        assert_eq!(cancel.header.ty, FrameType::Cancel);
        assert_eq!(cancel.header.channel, 0);
        assert_eq!(cancel.header.corr, req.header.corr);

        let mut watch = consumer.config_watch(None).await.unwrap();
        let req = request(&mut rx, json!({"op":"config.watch"})).await;
        dispatch_frame(
            &consumer.shared,
            1,
            frame(FrameType::StreamEnd, req.header.corr, Value::Null),
        )
        .await;
        assert_eq!(watch.ended().await, SubscriptionEnd::StreamEnd);
        assert_eq!(watch.next().await.unwrap(), None);

        let watch = consumer.config_watch(None).await.unwrap();
        let req = request(&mut rx, json!({"op":"config.watch"})).await;
        drop(watch);
        let cancel = rx.recv().await.unwrap().frame;
        assert_eq!(cancel.header.ty, FrameType::Cancel);
        assert_eq!(cancel.header.corr, req.header.corr);
    }

    #[tokio::test]
    async fn config_watch_rejects_malformed_events_and_cancels() {
        let (consumer, mut rx) = consumer();
        let mut watch = consumer.config_watch(None).await.unwrap();
        let req = request(&mut rx, json!({"op":"config.watch"})).await;
        dispatch_frame(
            &consumer.shared,
            1,
            frame(
                FrameType::StreamData,
                req.header.corr,
                json!({"op":"another.watch","event":"resync","modules":[],"stamped":false}),
            ),
        )
        .await;
        assert!(watch.next().await.is_err());
        assert_eq!(watch.ended().await, SubscriptionEnd::Cancelled);
        assert_eq!(rx.recv().await.unwrap().frame.header.ty, FrameType::Cancel);
    }
}
