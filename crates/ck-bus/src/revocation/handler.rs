//! The running revocation area: the handler that notices a superseded credential, and
//! the task that watches connections and drives every recorded revocation.
//!
//! A module's `ckbus.credential` request is served by issuance, which overwrites the
//! module's census entry with the new credential. Just before that, this handler reads
//! the entry: when the request is answered with a different key, the key the entry named
//! is superseded and its revocation is recorded before the answer returns. Requests for
//! one module are serialized here, so the entry read belongs to the issue that follows
//! it. A census read that fails refuses the request (`ckbus_census_unavailable`), because
//! issuing over an entry nobody could read would leave the credential it names
//! unrevoked.

use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use cortexkit_bus_naming::AccountNames;
use serde_json::{json, Value};
use subc_client_rs::{
    BindDecision, HandlerOutcome, ModuleHandler, RequestCtx, RouteBindRequest, RouteHandle,
};
use subc_protocol::{
    manifest::ModuleManifest, session::HealthReport, ModuleHelloAckBody, Principal,
};
use tokio::sync::{watch, Notify};

use super::{connections::Connections, log_event, RevocationPlane, Revoker, Target};
use crate::{
    bootstrap::{plane::CensusRecord, Ready},
    credentials::Credentials,
    issuance::{self, census::CensusValue, handler::IssuanceHandler},
};

/// The refusal a credential request gets when the module's census entry cannot be read.
pub const CENSUS_UNAVAILABLE: &str = "ckbus_census_unavailable";

/// Routes are identified by channel and epoch, as in issuance.
type RouteKey = (u16, u32);

fn route_key(handle: &RouteHandle) -> RouteKey {
    (handle.channel, handle.epoch)
}

/// The revocation area's shared state.
pub struct Area {
    revoker: Arc<Revoker>,
    ready: watch::Receiver<Option<Arc<Ready>>>,
    /// Wakes the driving task as soon as a revocation is recorded.
    wake: Notify,
    module_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl Area {
    pub fn new(revoker: Arc<Revoker>, ready: watch::Receiver<Option<Arc<Ready>>>) -> Self {
        Self {
            revoker,
            ready,
            wake: Notify::new(),
            module_locks: Mutex::new(HashMap::new()),
        }
    }

    pub fn revoker(&self) -> &Arc<Revoker> {
        &self.revoker
    }

    fn plane(&self) -> Option<RevocationPlane> {
        let ready = self.ready.borrow().clone()?;
        RevocationPlane::from_ready(&ready)
    }

    fn module_lock(&self, module_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.module_locks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(module_id.to_string())
            .or_default()
            .clone()
    }

    /// Records the revocation of the credential `before` named, when the issue that
    /// followed answered a different one.
    fn supersede(&self, module_id: &str, before: &CensusRecord, answer: &[u8]) {
        let issued = serde_json::from_slice::<Value>(answer)
            .ok()
            .and_then(|reply| {
                reply["result"]["credential_public"]
                    .as_str()
                    .map(str::to_string)
            });
        let Some(issued) = issued else {
            return;
        };
        let value = match CensusValue::parse(&before.value) {
            Ok(value) => value,
            Err(reason) => {
                log_event(
                    "ckbus.revocation.superseded_unknown",
                    json!({
                        "module_id": module_id,
                        "reason": reason,
                        "residual": "the census value read before the issue is damaged, so the \
                                     credential it named is not known and is not revoked",
                    }),
                );
                return;
            }
        };
        if value.credential_public == issued {
            return;
        }
        let target = Target::from_census(module_id, &value);
        if let Err(error) = self.revoker.begin(&target) {
            log_event(
                "ckbus.revocation.unrecorded",
                json!({
                    "module_id": module_id,
                    "user_public": target.user_public,
                    "reason": error.to_string(),
                    "residual": "revoked in this process only; a restart before it completes \
                                 loses it",
                }),
            );
        }
        self.wake.notify_one();
    }

    /// Waits for bootstrap, starts the connection watch, then drives every recorded
    /// revocation once per period and whenever one is recorded. A deferred revocation
    /// keeps its record and is retried on the next pass.
    pub async fn run(self: Arc<Self>, period: Duration) {
        let mut ready = self.ready.clone();
        let plane = loop {
            if let Some(plane) = self.plane() {
                break plane;
            }
            if ready.changed().await.is_err() {
                return;
            }
        };
        let mut watching = false;
        loop {
            if !watching {
                match plane.system.watch_connections(&plane.account_public).await {
                    Ok(events) => {
                        watching = true;
                        let connections = self.revoker.connections().clone();
                        tokio::spawn(async move { connections.follow(events).await });
                        log_event(
                            "ckbus.revocation.watching",
                            json!({ "account_public": plane.account_public }),
                        );
                    }
                    Err(error) => log_event(
                        "ckbus.revocation.watch_unavailable",
                        json!({
                            "reason": error.message,
                            "residual": "no connection is known to kick; the revocation list \
                                         alone closes a revoked user's connections",
                        }),
                    ),
                }
            }
            self.revoker.resume_all(&plane).await;
            tokio::select! {
                _ = self.wake.notified() => {}
                _ = tokio::time::sleep(period) => {}
            }
        }
    }
}

/// Wraps the issuance handler with the census read that finds superseded credentials.
pub struct RevocationHandler<H> {
    inner: H,
    area: Arc<Area>,
    principals: Mutex<HashMap<RouteKey, Option<Principal>>>,
}

impl<H> RevocationHandler<H> {
    pub fn new(inner: H, area: Arc<Area>) -> Self {
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

    /// The revocation area, so the spawn-stream consumer revokes through the same
    /// `Revoker` (one serial step (1), one progress store) as the superseded path.
    pub fn area(&self) -> &Arc<Area> {
        &self.area
    }
}

/// Whether `body` asks for a credential. Every other request passes straight through.
fn is_credential_request(body: &[u8]) -> bool {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|request| request["method"].as_str().map(str::to_string))
        .as_deref()
        == Some(issuance::CREDENTIAL_OP)
}

#[async_trait]
impl<H: ModuleHandler> ModuleHandler for RevocationHandler<H> {
    async fn handle(&self, ctx: RequestCtx, body: Vec<u8>) -> HandlerOutcome {
        let principal = self
            .principals()
            .get(&route_key(&ctx.route_handle()))
            .cloned()
            .flatten();
        // Only an attested credential request can issue, and issuance answers every
        // other case (a Direct caller, bootstrap not finished) with its own refusal.
        let module_id = match principal {
            Some(Principal::Reserved { module_id }) if is_credential_request(&body) => module_id,
            _ => return self.inner.handle(ctx, body).await,
        };
        let (Some(plane), Ok(key)) = (self.area.plane(), AccountNames::census_key(&module_id))
        else {
            return self.inner.handle(ctx, body).await;
        };
        let lock = self.area.module_lock(&module_id);
        let _serialized = lock.lock().await;
        let before = match plane.box_plane.census_get(&plane.names, &key).await {
            Ok(before) => before,
            Err(error) => {
                log_event(
                    "ckbus.revocation.census_unavailable",
                    json!({ "module_id": module_id, "reason": error.message }),
                );
                return HandlerOutcome::Error {
                    code: CENSUS_UNAVAILABLE.to_string(),
                    message: format!(
                        "the census entry for {module_id} could not be read, so a credential \
                         it names could not be revoked once superseded; nothing was issued: {}",
                        error.message
                    ),
                };
            }
        };
        let outcome = self.inner.handle(ctx, body).await;
        if let (Some(before), HandlerOutcome::Response(answer)) = (&before, &outcome) {
            self.area.supersede(&module_id, before, answer);
        }
        outcome
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

    async fn health(&self) -> HealthReport {
        self.inner.health().await
    }

    async fn on_route_gone(&self, handle: &RouteHandle) {
        self.principals().remove(&route_key(handle));
        self.inner.on_route_gone(handle).await;
    }
}

/// What `main.rs` serves once revocation is wired.
pub struct Wired<H> {
    pub manifest: ModuleManifest,
    pub handler: RevocationHandler<H>,
}

/// The one wiring call: wraps the issuance handler and starts the driving task over the
/// store root's progress records and bootstrap's `Ready`. `period` is the sentinel
/// period, the retry interval of every deferral.
pub fn wire<H: ModuleHandler>(
    wired: issuance::handler::Wired<H>,
    credentials: Arc<Credentials>,
    store_root: &Path,
    ready: watch::Receiver<Option<Arc<Ready>>>,
    period: Duration,
) -> Wired<IssuanceHandler<H>> {
    let revoker = Arc::new(Revoker::new(
        credentials,
        store_root,
        Arc::new(Connections::default()),
    ));
    let area = Arc::new(Area::new(revoker, ready));
    tokio::spawn(area.clone().run(period));
    Wired {
        manifest: wired.manifest,
        handler: RevocationHandler::new(wired.handler, area),
    }
}
