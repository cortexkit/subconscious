//! The subc side of the leaf area: `ckbus.leaf_watch` and `ckbus.leaf_sign` on ck-bus's
//! management surface, admitted only to the local server's wrapper, and the `fed.`
//! entries added to health.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use serde_json::{json, Value};
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
    area::{log_event, LeafArea},
    sign_code, ADMITTED_MODULE, LEAF_SIGN_OP, LEAF_WATCH_OP,
};

/// Routes are identified by channel and epoch, as in the other areas.
type RouteKey = (u16, u32);

fn route_key(handle: &RouteHandle) -> RouteKey {
    (handle.channel, handle.epoch)
}

/// Adds the two leaf ops to ck-bus's management surface, `ckbus.leaf_watch` as a watch.
pub fn advertise(mut manifest: ModuleManifest) -> ModuleManifest {
    for role in &mut manifest.provides {
        if let ProviderRole::ManagementSurface { operations, .. } = role {
            for (name, description) in [
                (
                    LEAF_WATCH_OP,
                    "leaf remotes for the local server, for reserved:nats-server only",
                ),
                (
                    LEAF_SIGN_OP,
                    "the hub connect signature, for reserved:nats-server only",
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
        .insert(LEAF_WATCH_OP.to_string(), OperationDelivery::WATCH);
    manifest
}

/// Wraps the module's handler with the leaf ops.
pub struct LeafHandler<H> {
    inner: H,
    area: Arc<LeafArea>,
    principals: Mutex<HashMap<RouteKey, Option<Principal>>>,
}

impl<H> LeafHandler<H> {
    pub fn new(inner: H, area: Arc<LeafArea>) -> Self {
        Self {
            inner,
            area,
            principals: Mutex::new(HashMap::new()),
        }
    }

    fn principals(&self) -> std::sync::MutexGuard<'_, HashMap<RouteKey, Option<Principal>>> {
        self.principals
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

pub(super) fn admitted(principal: Option<&Principal>) -> bool {
    matches!(principal, Some(Principal::Reserved { module_id }) if module_id == ADMITTED_MODULE)
}

fn error(code: &str, message: impl Into<String>) -> HandlerOutcome {
    HandlerOutcome::Error {
        code: code.to_string(),
        message: message.into(),
    }
}

/// Answers a leaf op, or `None` for any other request.
pub async fn answer(
    area: &LeafArea,
    principal: Option<&Principal>,
    ctx: &RequestCtx,
    body: &[u8],
) -> Option<HandlerOutcome> {
    let request: Value = serde_json::from_slice(body).ok()?;
    let method = request.get("method").and_then(Value::as_str)?;
    if method != LEAF_WATCH_OP && method != LEAF_SIGN_OP {
        return None;
    }
    if !admitted(principal) {
        log_event(
            "ckbus.federation.refused",
            json!({ "op": method, "principal": format!("{principal:?}") }),
        );
        return Some(error(
            sign_code::NOT_ADMITTED,
            format!("{method} answers only reserved:{ADMITTED_MODULE}"),
        ));
    }
    if method == LEAF_WATCH_OP {
        return Some(serve_watch(area, ctx).await);
    }
    let params = &request["params"];
    let (Some(remote), Some(nonce)) = (
        params.get("remote").and_then(Value::as_str),
        params.get("nonce_b64").and_then(Value::as_str),
    ) else {
        return Some(error(
            sign_code::BAD_REQUEST,
            "ckbus.leaf_sign requires params.remote and params.nonce_b64",
        ));
    };
    let outcome = area.sign(remote, nonce).await;
    log_event(
        "ckbus.federation.leaf_sign",
        json!({
            "remote": remote,
            "outcome": match &outcome { Ok(_) => "signed", Err(refusal) => refusal.code },
            "detail": outcome.as_ref().err().map(|refusal| refusal.message.clone()),
        }),
    );
    Some(match outcome {
        Ok(result) => HandlerOutcome::Response(
            serde_json::to_vec(&json!({ "result": result })).expect("an answer encodes"),
        ),
        Err(refusal) => error(refusal.code, refusal.message),
    })
}

/// Waits for the first complete decision, then sends it and each later state.
async fn serve_watch(area: &LeafArea, ctx: &RequestCtx) -> HandlerOutcome {
    let mut states = area.subscribe();
    loop {
        let state = states.borrow_and_update().watch_json();
        if let Some(state) = state {
            let body =
                serde_json::to_vec(&json!({ "result": state })).expect("a leaf state encodes");
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

#[async_trait]
impl<H: ModuleHandler> ModuleHandler for LeafHandler<H> {
    async fn handle(&self, ctx: RequestCtx, body: Vec<u8>) -> HandlerOutcome {
        let principal = self
            .principals()
            .get(&route_key(&ctx.route_handle()))
            .cloned()
            .flatten();
        match answer(&self.area, principal.as_ref(), &ctx, &body).await {
            Some(outcome) => outcome,
            None => self.inner.handle(ctx, body).await,
        }
    }

    async fn on_hello_ack(&self, ack: &ModuleHelloAckBody) {
        self.inner.on_hello_ack(ack).await;
    }

    async fn on_bind(&self, request: &RouteBindRequest) -> BindDecision {
        self.principals()
            .insert(route_key(&request.handle), request.principal.clone());
        self.inner.on_bind(request).await
    }

    async fn on_bound(&self, handle: &RouteHandle) {
        self.inner.on_bound(handle).await;
    }

    /// The wrapped answer, with the `fed.` entries added to its metrics. Status, detail
    /// and every existing metric are passed through unchanged.
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
        self.principals().remove(&route_key(handle));
        self.inner.on_route_gone(handle).await;
    }
}
