//! The subc side of the hub area: the four hub ops on ck-bus's management surface, the
//! `peer-sharing/v1` share, and the hub's `fed.` entries in health. The watch and the
//! TLS signature answer only the hub instance's wrapper (`reserved:nats-hub`), the
//! identity read only callosum, and the leaf issue only a paired computer's call that
//! callosum forwarded.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use serde_json::{json, Map, Value};
use subc_client_rs::{
    BindDecision, HandlerOutcome, ModuleHandler, RequestCtx, RouteBindRequest, RouteHandle,
};
use subc_protocol::{
    manifest::{
        ManagementOperation, ManagementOperationKind, ModuleManifest, OperationDelivery,
        ProviderRole,
    },
    session::HealthReport,
    ModuleHelloAckBody, Principal,
};

use super::{
    area::{HubArea, Refusal},
    hub_share, issue_code, tls_code, CALLOSUM_MODULE, FED_COMPUTER_HARNESS_PREFIX,
    HUB_IDENTITY_READ_OP, HUB_LEAF_ISSUE_OP, HUB_SERVER_MODULE, HUB_SERVER_WATCH_OP,
    HUB_TLS_SIGN_OP, PEER_SHARING_ROLE,
};
use crate::federation::leaf::{area::log_event, hub::is_hex};

/// Routes are identified by channel and epoch, as in the other areas.
type RouteKey = (u16, u32);

fn route_key(handle: &RouteHandle) -> RouteKey {
    (handle.channel, handle.epoch)
}

/// Who opened a route: the daemon's principal and the opener's harness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caller {
    pub principal: Option<Principal>,
    pub harness: String,
}

/// Adds the four hub ops to ck-bus's management surface (`ckbus.hub_server_watch` as a
/// watch) and declares the `peer-sharing/v1` share holding only `ckbus.hub_leaf_issue`.
pub fn advertise(mut manifest: ModuleManifest) -> ModuleManifest {
    for role in &mut manifest.provides {
        if let ProviderRole::ManagementSurface { operations, .. } = role {
            for (name, description) in [
                (
                    HUB_SERVER_WATCH_OP,
                    "the hub listener for the hub instance, for reserved:nats-hub only",
                ),
                (
                    HUB_TLS_SIGN_OP,
                    "one hub listener TLS signature, for reserved:nats-hub only",
                ),
                (
                    HUB_IDENTITY_READ_OP,
                    "this computer's hub certificate pin and leaf port, for reserved:callosum",
                ),
                (
                    HUB_LEAF_ISSUE_OP,
                    "a paired computer's hub leaf credential, through callosum's re-export",
                ),
            ] {
                operations.push(ManagementOperation {
                    name: name.to_string(),
                    kind: ManagementOperationKind::Query,
                    description: Some(description.to_string()),
                });
            }
        }
    }
    manifest
        .operation_delivery
        .insert(HUB_SERVER_WATCH_OP.to_string(), OperationDelivery::WATCH);
    let mut declaration = Map::new();
    declaration.insert("shares".to_string(), json!([hub_share()]));
    manifest
        .role_declarations
        .insert(PEER_SHARING_ROLE.to_string(), declaration);
    manifest
}

/// The paired computer a forwarded call comes from: callosum's principal with harness
/// `fed-computer:<fingerprint>`. Anything else is `None`.
pub fn forwarded_computer(caller: Option<&Caller>) -> Option<String> {
    let caller = caller?;
    if !reserved(caller.principal.as_ref(), CALLOSUM_MODULE) {
        return None;
    }
    let fingerprint = caller.harness.strip_prefix(FED_COMPUTER_HARNESS_PREFIX)?;
    is_hex(fingerprint, 32).then(|| fingerprint.to_string())
}

fn reserved(principal: Option<&Principal>, module: &str) -> bool {
    matches!(principal, Some(Principal::Reserved { module_id }) if module_id == module)
}

fn error(code: &str, message: impl Into<String>) -> HandlerOutcome {
    HandlerOutcome::Error {
        code: code.to_string(),
        message: message.into(),
    }
}

fn answer_json(result: Value) -> HandlerOutcome {
    HandlerOutcome::Response(
        serde_json::to_vec(&json!({ "result": result })).expect("an answer encodes"),
    )
}

fn refused(refusal: Refusal) -> HandlerOutcome {
    error(refusal.code, refusal.message)
}

/// Answers a hub op, or `None` for any other request.
pub async fn answer(
    area: &HubArea,
    caller: Option<&Caller>,
    ctx: &RequestCtx,
    body: &[u8],
) -> Option<HandlerOutcome> {
    let request: Value = serde_json::from_slice(body).ok()?;
    if request.get("method").and_then(Value::as_str) == Some(HUB_SERVER_WATCH_OP) {
        let principal = caller.and_then(|caller| caller.principal.as_ref());
        if !reserved(principal, HUB_SERVER_MODULE) {
            return refuse(caller, HUB_SERVER_WATCH_OP, "reserved:nats-hub");
        }
        return Some(serve_watch(area, ctx).await);
    }
    query(area, caller, &request).await
}

fn refuse(caller: Option<&Caller>, method: &str, who: &str) -> Option<HandlerOutcome> {
    log_event(
        "ckbus.federation.refused",
        json!({
            "op": method,
            "principal": format!("{:?}", caller.and_then(|caller| caller.principal.as_ref())),
            "harness": caller.map(|caller| caller.harness.clone()),
        }),
    );
    Some(error(
        issue_code::NOT_ADMITTED,
        format!("{method} answers only {who}"),
    ))
}

/// Answers the three hub ops that reply once, or `None` for any other request.
pub async fn query(
    area: &HubArea,
    caller: Option<&Caller>,
    request: &Value,
) -> Option<HandlerOutcome> {
    let method = request.get("method").and_then(Value::as_str)?;
    let principal = caller.and_then(|caller| caller.principal.as_ref());
    let not_admitted = |who: &str| refuse(caller, method, who);
    let params = &request["params"];
    match method {
        HUB_TLS_SIGN_OP => {
            if !reserved(principal, HUB_SERVER_MODULE) {
                return not_admitted("reserved:nats-hub");
            }
            let (Some(message), Some(scheme)) = (
                params.get("message_b64").and_then(Value::as_str),
                params.get("scheme").and_then(Value::as_str),
            ) else {
                return Some(error(
                    tls_code::BAD_REQUEST,
                    "ckbus.hub_tls_sign requires params.message_b64 and params.scheme",
                ));
            };
            let outcome = area.tls_sign(message, scheme).await;
            if let Err(refusal) = &outcome {
                // Logged by code only: a handshake flood must not flood the log with
                // its messages.
                log_event(
                    "ckbus.federation.hub_tls_sign_refused",
                    json!({ "code": refusal.code }),
                );
            }
            Some(outcome.map_or_else(refused, answer_json))
        }
        HUB_IDENTITY_READ_OP => {
            if !reserved(principal, CALLOSUM_MODULE) {
                return not_admitted("reserved:callosum");
            }
            Some(answer_json(area.identity_json()))
        }
        HUB_LEAF_ISSUE_OP => {
            let Some(fingerprint) = forwarded_computer(caller) else {
                return not_admitted("a paired computer through reserved:callosum");
            };
            let Some(leaf_nkey) = params.get("leaf_nkey").and_then(Value::as_str) else {
                return Some(error(
                    issue_code::BAD_REQUEST,
                    "ckbus.hub_leaf_issue requires params.leaf_nkey",
                ));
            };
            let outcome = area.leaf_issue(&fingerprint, leaf_nkey).await;
            if let Err(refusal) = &outcome {
                log_event(
                    "ckbus.federation.hub_leaf_issue_refused",
                    json!({
                        "fingerprint": fingerprint,
                        "code": refusal.code,
                        "detail": refusal.message,
                    }),
                );
            }
            Some(outcome.map_or_else(refused, |jwt| answer_json(json!({ "jwt": jwt }))))
        }
        _ => None,
    }
}

/// Waits for the first listener decision, then sends it and each later state.
async fn serve_watch(area: &HubArea, ctx: &RequestCtx) -> HandlerOutcome {
    let mut states = area.subscribe();
    loop {
        let state = states.borrow_and_update().watch_json();
        if let Some(state) = state {
            let body =
                serde_json::to_vec(&json!({ "result": state })).expect("a hub state encodes");
            if ctx.emit(body).await.is_err() {
                return HandlerOutcome::Streamed;
            }
        }
        tokio::select! {
            _ = ctx.cancelled() => return HandlerOutcome::Streamed,
            changed = states.changed() => if changed.is_err() {
                return HandlerOutcome::Streamed;
            },
        }
    }
}

/// Wraps the module's handler with the hub ops.
pub struct HubHandler<H> {
    inner: H,
    area: Arc<HubArea>,
    callers: Mutex<HashMap<RouteKey, Caller>>,
}

impl<H> HubHandler<H> {
    pub fn new(inner: H, area: Arc<HubArea>) -> Self {
        Self {
            inner,
            area,
            callers: Mutex::new(HashMap::new()),
        }
    }

    fn callers(&self) -> std::sync::MutexGuard<'_, HashMap<RouteKey, Caller>> {
        self.callers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[async_trait]
impl<H: ModuleHandler> ModuleHandler for HubHandler<H> {
    async fn handle(&self, ctx: RequestCtx, body: Vec<u8>) -> HandlerOutcome {
        let caller = self.callers().get(&route_key(&ctx.route_handle())).cloned();
        match answer(&self.area, caller.as_ref(), &ctx, &body).await {
            Some(outcome) => outcome,
            None => self.inner.handle(ctx, body).await,
        }
    }

    async fn on_hello_ack(&self, ack: &ModuleHelloAckBody) {
        self.inner.on_hello_ack(ack).await;
    }

    async fn on_bind(&self, request: &RouteBindRequest) -> BindDecision {
        self.callers().insert(
            route_key(&request.handle),
            Caller {
                principal: request.principal.clone(),
                harness: request.identity.harness.clone(),
            },
        );
        self.inner.on_bind(request).await
    }

    async fn on_bound(&self, handle: &RouteHandle) {
        self.inner.on_bound(handle).await;
    }

    /// The wrapped answer, with the hub's `fed.` entries added to its metrics. Status,
    /// detail and every existing metric are passed through unchanged.
    async fn health(&self) -> HealthReport {
        let mut report = self.inner.health().await;
        let fed = self.area.health_metrics();
        match report.metrics.as_mut() {
            Some(Value::Object(metrics)) => metrics.extend(fed),
            Some(_) => {}
            None => report.metrics = Some(Value::Object(fed)),
        }
        report
    }

    async fn on_route_gone(&self, handle: &RouteHandle) {
        self.callers().remove(&route_key(handle));
        self.inner.on_route_gone(handle).await;
    }
}
