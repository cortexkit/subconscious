//! Opt-in module configuration delivery. See [`ModuleHandler`] for lifecycle hooks.
//!
//! Only an absent `config.gate` capability permits the existing file reader.
//! Refusals retain the last applied view; no approved view is represented without
//! inventing empty configuration bytes. Anchor-pending reports retry on the next
//! `config.changed` hint. Review and apply hooks never occupy the frame reader.
//!
//! # A small module adopting the gate
//!
//! The hooks below are the entire adoption; an existing `handle` is unchanged.
//! The declaration's user path is provided by the module's startup configuration.
//! The review function uses only supplied bytes and the supplied old view.
//!
//! ```no_run
//! use async_trait::async_trait;
//! use serde_json::json;
//! use std::sync::Mutex;
//! use subc_client_rs::{ModuleHandler, RequestCtx, HandlerOutcome, config::*};
//!
//! struct Counter { user_file: String, value: Mutex<serde_json::Value> }
//! fn schema() -> serde_json::Value {
//!     json!({"$schema":"https://json-schema.org/draft/2020-12/schema",
//!       "type":"object","unevaluatedProperties":false,"properties":{"count":{"type":"integer",
//!       "description":"Counter value","default":0,"x-ck-apply":"live",
//!       "x-ck-layers":["user"]}}})
//! }
//! fn review(request: ConfigReviewRequest) -> Result<ConfigReviewReply, String> {
//!     let schema = Schema::new(schema()).map_err(|e| format!("{e:?}"))?;
//!     let layers = request.layers.iter().filter_map(|l| l.bytes.as_ref().map(|b| (l,b)))
//!         .map(|(l,b)| Ok(LayerValue::new(l.layer.known_layer().ok_or("unknown layer")?,
//!             serde_json::from_slice(b).map_err(|_| "invalid JSON")?)))
//!         .collect::<Result<Vec<_>, &str>>().map_err(str::to_owned)?;
//!     let tuple = schema.review_tuple(&layers, &ValidationContext::new("counter", None))
//!         .map_err(|_| "invalid counter configuration")?;
//!     let old = request.base.as_ref().map(|v| v.effective.clone()).unwrap_or(json!({"count":null}));
//!     let old_source = request.base.as_ref().and_then(|v| v.sources.get("/count")).copied().unwrap_or(Layer::Default);
//!     if old["count"] == tuple.effective["count"] && old_source == tuple.sources["/count"] {
//!         return Ok(ConfigReviewReply::new(vec![], vec![]));
//!     }
//!     let mut field = FieldChange::new("counter", "/count", RecordedValue::new(old["count"].clone()),
//!         RecordedValue::new(tuple.effective["count"].clone()), ApplyKind::Live);
//!     field.old_source = old_source;
//!     field.new_source = tuple.sources["/count"];
//!     Ok(ConfigReviewReply::new(vec![field], vec![]))
//! }
//! #[async_trait]
//! impl ModuleHandler for Counter {
//!     async fn handle(&self, _: RequestCtx, body: Vec<u8>) -> HandlerOutcome {
//!         HandlerOutcome::Response(body)
//!     }
//!     fn config_declaration(&self) -> Option<ConfigDeclareRequest> {
//!         Some(ConfigDeclareRequest::new(schema(), &self.user_file, vec![]))
//!     }
//!     async fn config_review(&self, request: ConfigReviewRequest) -> ConfigReviewReply {
//!         review(request).unwrap_or_else(|reason| ConfigReviewReply::new(vec![],
//!             vec![ConfigReviewRefusal::new("value_invalid", vec!["/count".into()], vec![reason])]))
//!     }
//!     async fn config_apply(&self, view: ConfigView) -> Result<(), Vec<String>> {
//!         *self.value.lock().unwrap() = view.effective; Ok(())
//!     }
//!     async fn config_files(&self) {
//!         // This path is used only with an older daemon, never on a refusal.
//!         if let Ok(bytes) = std::fs::read(&self.user_file) {
//!             if let Ok(value) = serde_json::from_slice(&bytes) { *self.value.lock().unwrap() = value; }
//!         }
//!     }
//!     async fn on_config_state(&self, state: ConfigState) { eprintln!("{}", state.as_str()); }
//! }
//! # let _ = Counter { user_file: "counter.json".into(), value: Mutex::new(json!({"count":0})) };
//! ```

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt,
    sync::{Arc, OnceLock},
};

use serde::{de::DeserializeOwned, Serialize};
use subc_protocol::{ErrorBody, Frame, FrameType};
use tokio::sync::{oneshot, Notify};

use crate::{ModuleHandle, ModuleHandler, RequestDispatcher, CATALOG_UPDATE_TIMEOUT};
pub use subc_config::{ApplyKind, LayerValue, RecordedValue, Schema, ValidationContext};
pub use subc_protocol::config::*;

/// The bytes available after a refusal. Absence is not permission to read files.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub enum ConfigAvailability {
    NoApprovedConfig,
    LastView(ConfigView),
}

/// Wire status or refusal, with the module's retained view where applicable.
/// `ConfigGated` with `NoApprovedConfig` carries the original refusal but no bytes.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub enum ConfigState {
    Unstamped(ConfigView),
    Pending(ConfigView),
    Active(ConfigView),
    Failed {
        view: ConfigAvailability,
        reasons: Vec<String>,
    },
    ConfigGated {
        view: ConfigAvailability,
        refusal: ErrorBody,
    },
    NotDeliveryTarget {
        view: ConfigAvailability,
        refusal: ErrorBody,
    },
    AnchorPending {
        view: ConfigAvailability,
        refusal: ErrorBody,
    },
}

impl ConfigState {
    /// The daemon's status or refusal spelling, without configuration values.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Unstamped(_) => "unstamped",
            Self::Pending(_) => "pending",
            Self::Active(_) => "active",
            Self::Failed { .. } => "failed",
            Self::ConfigGated { .. } => "config_gated",
            Self::NotDeliveryTarget { .. } => "not_delivery_target",
            Self::AnchorPending { .. } => "anchor_pending",
        }
    }
}

/// Non-status errors never authorize a fallback. Refusals preserve extension data.
#[non_exhaustive]
#[derive(Debug)]
pub enum ConfigError {
    Refused(ErrorBody),
    Protocol(String),
    Timeout,
    ConnectionClosed,
}
impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused(body) => write!(f, "{}: {}", body.code, body.message),
            Self::Protocol(message) => f.write_str(message),
            Self::Timeout => f.write_str("config request timed out"),
            Self::ConnectionClosed => f.write_str("config connection closed"),
        }
    }
}
impl Error for ConfigError {}

#[derive(Default)]
pub(crate) struct Session {
    pub(crate) declaration: OnceLock<ConfigDeclareRequest>,
    changed: Notify,
    projects: std::sync::Mutex<BTreeSet<Option<String>>>,
    pub(crate) stop: tokio_util::sync::CancellationToken,
}

pub(crate) fn start<H: ModuleHandler>(
    handle: ModuleHandle,
    handler: Arc<H>,
    dispatcher: &RequestDispatcher,
) {
    let Some(declaration) = handle.shared.config.declaration.get().cloned() else {
        return;
    };
    let session_handle = handle.clone();
    dispatcher.spawn(async move {
        tokio::select! {
            () = session_handle.shared.config.stop.cancelled() => {},
            () = run(handle, handler, declaration) => {},
        }
    });
}

async fn run<H: ModuleHandler>(
    handle: ModuleHandle,
    handler: Arc<H>,
    declaration: ConfigDeclareRequest,
) {
    if !handle.daemon_supports(CONFIG_GATE_CAPABILITY) {
        handler.config_files().await;
        return;
    }
    let mut deliveries = BTreeMap::new();
    let delivery = deliveries.entry(None).or_insert_with(Delivery::default);
    // A declaration refusal still consumes the one declaration on this
    // connection. Future hints fetch again, never declare a second time.
    match call(&handle, &declaration, CONFIG_DECLARE_OP).await {
        Ok(_) => delivery.refresh(&handle, &*handler, None).await,
        Err(error) => delivery.error(&*handler, error).await,
    }
    loop {
        handle.shared.config.changed.notified().await;
        let projects = std::mem::take(
            &mut *handle
                .shared
                .config
                .projects
                .lock()
                .unwrap_or_else(|p| p.into_inner()),
        );
        for project in projects {
            deliveries
                .entry(project.clone())
                .or_insert_with(Delivery::default)
                .refresh(&handle, &*handler, project)
                .await;
        }
    }
}

pub(crate) fn changed(frame: &Frame, handle: &ModuleHandle) -> bool {
    if !is_op(frame, CONFIG_CHANGED_OP) {
        return false;
    }
    if handle.shared.config.declaration.get().is_some()
        && handle.daemon_supports(CONFIG_GATE_CAPABILITY)
    {
        match serde_json::from_slice::<ConfigChanged>(&frame.body) {
            Ok(hint) => {
                handle
                    .shared
                    .config
                    .projects
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert(hint.project_id);
                handle.shared.config.changed.notify_one();
            }
            Err(error) => tracing::warn!(%error, "ignoring malformed config.changed"),
        }
    }
    true
}

fn is_op(frame: &Frame, op: &str) -> bool {
    serde_json::from_slice::<serde_json::Value>(&frame.body)
        .ok()
        .and_then(|value| {
            value
                .get("op")
                .and_then(|value| value.as_str())
                .map(|value| value == op)
        })
        == Some(true)
}

pub(crate) fn review<H: ModuleHandler>(
    frame: &Frame,
    egress: &tokio::sync::mpsc::Sender<Frame>,
    handler: Arc<H>,
    dispatcher: &RequestDispatcher,
    handle: &ModuleHandle,
) -> bool {
    if !is_op(frame, CONFIG_REVIEW_OP) {
        return false;
    }
    let frame = frame.clone();
    let egress = egress.clone();
    let opted_in = handle.shared.config.declaration.get().is_some()
        && handle.daemon_supports(CONFIG_GATE_CAPABILITY);
    let failure = dispatcher.clone();
    dispatcher.spawn(async move {
        let result = async {
            let (ty, body) = if !opted_in {
                (
                    FrameType::Error,
                    serde_json::to_vec(&ErrorBody::new(
                        "unsupported_op",
                        "module did not opt in to config.review",
                    )),
                )
            } else {
                match serde_json::from_slice::<ConfigReviewRequest>(&frame.body) {
                    Ok(request) => (
                        FrameType::Response,
                        serde_json::to_vec(&handler.config_review(request).await),
                    ),
                    Err(_) => (
                        FrameType::Error,
                        serde_json::to_vec(&ErrorBody::new(
                            "invalid_request",
                            "invalid config.review snapshots",
                        )),
                    ),
                }
            };
            let reply = Frame::build_with_version(
                frame.header.ver,
                ty,
                crate::control_flags(),
                0,
                0,
                frame.header.corr,
                body.map_err(crate::SubcModuleError::Json)?,
            )
            .map_err(crate::SubcModuleError::FrameBuild)?;
            crate::send_outbound(&egress, reply).await
        }
        .await;
        if let Err(error) = result {
            failure.fail(error);
        }
    });
    true
}

#[derive(Default)]
struct Delivery {
    view: Option<ConfigView>,
    report: Option<ConfigAppliedRequest>,
}
impl Delivery {
    fn available(&self) -> ConfigAvailability {
        self.view
            .clone()
            .map(ConfigAvailability::LastView)
            .unwrap_or(ConfigAvailability::NoApprovedConfig)
    }

    async fn error<H: ModuleHandler>(&self, handler: &H, error: ConfigError) {
        if let ConfigError::Refused(refusal) = error {
            let view = self.available();
            let state = match refusal.code.as_str() {
                "config_gated" => ConfigState::ConfigGated { view, refusal },
                "not_delivery_target" => ConfigState::NotDeliveryTarget { view, refusal },
                "anchor_pending" => ConfigState::AnchorPending { view, refusal },
                _ => {
                    handler.on_config_error(ConfigError::Refused(refusal)).await;
                    return;
                }
            };
            handler.on_config_state(state).await;
        } else {
            handler.on_config_error(error).await;
        }
    }

    async fn report<H: ModuleHandler>(&mut self, handle: &ModuleHandle, handler: &H) -> bool {
        let Some(report) = self.report.as_ref() else {
            return true;
        };
        match call(handle, report, CONFIG_APPLIED_OP).await {
            Ok(_) => {
                if report.result.is_ack() {
                    if let Some(view) = self.view.as_mut() {
                        view.status = ConfigStatus::Active;
                        handler
                            .on_config_state(ConfigState::Active(view.clone()))
                            .await;
                    }
                }
                self.report = None;
                true
            }
            Err(error) => {
                // Only anchor_pending explicitly permits replaying the report.
                // During a module swap, a not-yet-promoted or replaced connection
                // is not the delivery target. It must wait for config.changed to
                // announce promotion, then fetch the newly assigned attempt.
                if !matches!(&error, ConfigError::Refused(body) if body.code == "anchor_pending") {
                    self.report = None;
                }
                self.error(handler, error).await;
                false
            }
        }
    }

    async fn refresh<H: ModuleHandler>(
        &mut self,
        handle: &ModuleHandle,
        handler: &H,
        project: Option<String>,
    ) {
        if !self.report(handle, handler).await {
            return;
        }
        let mut request = ConfigGetRequest::new();
        request.project_id = project.clone();
        let reply = match call(handle, &request, CONFIG_GET_OP)
            .await
            .and_then(decode::<ConfigGetReply>)
        {
            Ok(reply) => reply,
            Err(error) => {
                self.error(handler, error).await;
                return;
            }
        };
        let Some(view) = reply.pending.as_ref().or(reply.active.as_ref()) else {
            self.error(
                handler,
                ConfigError::Protocol("config.get returned no view".into()),
            )
            .await;
            return;
        };
        if view.project_id != project
            || reply
                .active
                .as_ref()
                .is_some_and(|active| active.project_id != project)
        {
            self.error(
                handler,
                ConfigError::Protocol("config.get returned another project".into()),
            )
            .await;
            return;
        }
        // A reconnect may see both an approved recovery view and a delivery.
        // Establish that recovery view before attempting the new configuration,
        // so a rejected first delivery does not leave a fresh process empty.
        if self.view.is_none() && reply.pending.is_some() {
            if let Some(active) = &reply.active {
                if active.status != ConfigStatus::Active
                    || active
                        .layers
                        .iter()
                        .any(|layer| layer.layer.known_layer().is_none())
                {
                    self.error(
                        handler,
                        ConfigError::Protocol("invalid recovery view".into()),
                    )
                    .await;
                    return;
                }
                match handler.config_apply(active.clone()).await {
                    Ok(()) => {
                        self.view = Some(active.clone());
                        handler
                            .on_config_state(ConfigState::Active(active.clone()))
                            .await;
                    }
                    Err(reasons) => {
                        handler
                            .on_config_state(ConfigState::Failed {
                                view: self.available(),
                                reasons,
                            })
                            .await;
                        return;
                    }
                }
            }
        }
        if !matches!(
            view.status,
            ConfigStatus::Active | ConfigStatus::Pending | ConfigStatus::Unstamped
        ) || view
            .layers
            .iter()
            .any(|layer| layer.layer.known_layer().is_none())
        {
            self.error(
                handler,
                ConfigError::Protocol("config.get returned an unknown status or layer".into()),
            )
            .await;
            return;
        }
        let attempt = if reply.pending.is_some() {
            match view.attempt {
                Some(attempt) => Some(attempt),
                None => {
                    self.error(
                        handler,
                        ConfigError::Protocol("pending view has no delivery attempt".into()),
                    )
                    .await;
                    return;
                }
            }
        } else {
            None
        };
        match handler.config_apply(view.clone()).await {
            Ok(()) => {
                self.view = Some(view.clone());
                handler
                    .on_config_state(match view.status {
                        ConfigStatus::Unstamped => ConfigState::Unstamped(view.clone()),
                        ConfigStatus::Pending => ConfigState::Pending(view.clone()),
                        _ => ConfigState::Active(view.clone()),
                    })
                    .await;
                if let Some(attempt) = attempt {
                    let mut report = ConfigAppliedRequest::new(
                        view.generation,
                        attempt,
                        ConfigApplyResult::Ack,
                        vec![],
                    );
                    report.project_id = view.project_id.clone();
                    self.report = Some(report);
                }
            }
            Err(reasons) => {
                handler
                    .on_config_state(ConfigState::Failed {
                        view: self.available(),
                        reasons: reasons.clone(),
                    })
                    .await;
                if let Some(attempt) = attempt {
                    let mut report = ConfigAppliedRequest::new(
                        view.generation,
                        attempt,
                        ConfigApplyResult::Nack,
                        reasons,
                    );
                    report.project_id = view.project_id.clone();
                    self.report = Some(report);
                }
            }
        }
        self.report(handle, handler).await;
    }
}

fn decode<T: DeserializeOwned>(body: serde_json::Value) -> Result<T, ConfigError> {
    serde_json::from_value(body).map_err(|error| ConfigError::Protocol(error.to_string()))
}

async fn call(
    handle: &ModuleHandle,
    request: &impl Serialize,
    op: &str,
) -> Result<serde_json::Value, ConfigError> {
    let body =
        serde_json::to_vec(request).map_err(|error| ConfigError::Protocol(error.to_string()))?;
    let (corr, writer, rx) = {
        let mut inner = handle.shared.lock_inner();
        let writer = inner.writer.clone().ok_or(ConfigError::ConnectionClosed)?;
        let corr =
            crate::next_module_control_corr(&mut inner).ok_or(ConfigError::ConnectionClosed)?;
        let (tx, rx) = oneshot::channel();
        inner.pending_config_calls.insert(corr, tx);
        (corr, writer, rx)
    };
    let _guard = Waiter {
        handle: handle.clone(),
        corr,
    };
    let frame = Frame::build_with_version(
        handle.shared.negotiated_ver,
        FrameType::Request,
        crate::control_flags(),
        0,
        0,
        corr,
        body,
    )
    .map_err(|error| ConfigError::Protocol(error.to_string()))?;
    let reply = tokio::time::timeout(CATALOG_UPDATE_TIMEOUT, async {
        writer
            .send(frame)
            .await
            .map_err(|_| ConfigError::ConnectionClosed)?;
        rx.await.map_err(|_| ConfigError::ConnectionClosed)
    })
    .await
    .map_err(|_| ConfigError::Timeout)??;
    if reply.header.ty == FrameType::Error {
        return Err(ConfigError::Refused(
            serde_json::from_slice(&reply.body)
                .map_err(|error| ConfigError::Protocol(error.to_string()))?,
        ));
    }
    let body: serde_json::Value = serde_json::from_slice(&reply.body)
        .map_err(|error| ConfigError::Protocol(error.to_string()))?;
    if body.get("op").and_then(|op| op.as_str()) != Some(op) {
        return Err(ConfigError::Protocol(format!("unexpected reply to {op}")));
    }
    Ok(body)
}
struct Waiter {
    handle: ModuleHandle,
    corr: u64,
}
impl Drop for Waiter {
    fn drop(&mut self) {
        self.handle
            .shared
            .lock_inner()
            .pending_config_calls
            .remove(&self.corr);
    }
}

#[cfg(test)]
mod tests;
