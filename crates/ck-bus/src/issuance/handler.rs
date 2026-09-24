//! The subc side of issuance: the two ops advertised on ck-bus's management surface, the
//! principal each route was bound with, and the wiring `main.rs` calls.
//!
//! The daemon stamps a route's principal once, in `route.bind`, never per request, so
//! the handler records it by route in `on_bind` and looks it up for every request on
//! that route. A request on a route with no recorded principal is answered as `Direct`.
//! Every other request, and health, goes to the wrapped handler; only a damaged
//! `epoch_high_water.json` overrides health to down.

use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde_json::{json, Value};
use subc_client_rs::{
    consumer::{ConsumerOptions, SubcConsumer},
    BindDecision, HandlerOutcome, ModuleHandler, RequestCtx, RouteBindRequest, RouteHandle,
};
use subc_protocol::{
    manifest::{
        Concurrency, ManagementOperation, ManagementOperationKind, ModuleManifest,
        ModuleManifestBuilder, ProviderRole,
    },
    session::{HealthReport, HealthStatus},
    ModuleHelloAckBody, Principal,
};
use tokio::sync::{watch, OnceCell};

use super::{code, log_event, Issuance, LiveGenerations, Plane, PlaneSource, Refusal};
use crate::{
    bootstrap::{config::BrokerConfig, Ready},
    credentials::Credentials,
    grants,
};

/// Health `metrics.cause` while a high-water entry is damaged.
pub const HIGH_WATER_DAMAGED_CAUSE: &str = "epoch-high-water-damaged";

/// Routes are identified by channel and epoch: a module has one connection to the
/// daemon, and the epoch distinguishes a reused channel.
type RouteKey = (u16, u32);

fn route_key(handle: &RouteHandle) -> RouteKey {
    (handle.channel, handle.epoch)
}

/// Adds the two issuance ops to the module's manifest as a management surface, which is
/// how a participant's `route.open` reaches ck-bus.
pub fn advertise(builder: ModuleManifestBuilder) -> ModuleManifest {
    let operations = [
        (super::CREDENTIAL_OP, ManagementOperationKind::Mutate),
        (super::NONCE_SIGN_OP, ManagementOperationKind::Query),
    ]
    .into_iter()
    .map(|(name, kind)| ManagementOperation {
        name: name.to_string(),
        kind,
        description: Some("ck-bus participant credential issuance".to_string()),
    })
    .collect();
    builder
        .provides(vec![ProviderRole::ManagementSurface {
            operations,
            config_schema: json!({}),
            observability: vec![],
            identity_scope: vec![],
            concurrency: Concurrency::ModuleManaged,
        }])
        .build()
}

/// Wraps the module's handler with the issuance ops.
pub struct IssuanceHandler<H> {
    inner: H,
    issuance: Arc<Issuance>,
    principals: Mutex<HashMap<RouteKey, Option<Principal>>>,
}

impl<H> IssuanceHandler<H> {
    pub fn new(inner: H, issuance: Arc<Issuance>) -> Self {
        Self {
            inner,
            issuance,
            principals: Mutex::new(HashMap::new()),
        }
    }

    fn principals(&self) -> std::sync::MutexGuard<'_, HashMap<RouteKey, Option<Principal>>> {
        self.principals
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Answers one issuance request. `None` when `body` names an op this area does not
    /// serve, so the wrapped handler answers it.
    pub async fn answer(
        &self,
        principal: Option<&Principal>,
        body: &[u8],
    ) -> Option<HandlerOutcome> {
        let request: Value = serde_json::from_slice(body).ok()?;
        let method = request.get("method").and_then(Value::as_str)?;
        if method != super::CREDENTIAL_OP && method != super::NONCE_SIGN_OP {
            return None;
        }
        let params = request.get("params").cloned().unwrap_or(Value::Null);
        // Recorded so the log shows a claimed id was seen and not used.
        let claimed = params.get("module_id").and_then(Value::as_str);
        let attested = match principal {
            Some(Principal::Reserved { module_id }) => Ok(module_id.clone()),
            other => Err(Refusal::new(
                code::PRINCIPAL_DIRECT,
                format!(
                    "{method} answers only a caller the daemon attested by its launch nonce; \
                     this route arrived as {}",
                    principal_label(other)
                ),
            )),
        };
        let outcome = match attested {
            Err(refusal) => Err(refusal),
            Ok(module_id) if method == super::CREDENTIAL_OP => self
                .issuance
                .issue(&module_id)
                .await
                .map(|answer| answer.to_json()),
            Ok(module_id) => match params
                .get("nonce_b64")
                .and_then(Value::as_str)
                .map(|nonce| STANDARD.decode(nonce))
            {
                Some(Ok(nonce)) => self
                    .issuance
                    .sign_nonce(
                        &module_id,
                        params.get("credential_public").and_then(Value::as_str),
                        &nonce,
                    )
                    .await
                    .map(|signature| json!({ "signature_b64": STANDARD.encode(signature) })),
                _ => Err(Refusal::new(
                    code::BAD_REQUEST,
                    "ckbus.nonce_sign requires params.nonce_b64 in standard base64",
                )),
            },
        };
        log_event(
            "ckbus.issuance.answer",
            json!({
                "op": method,
                "principal": principal_label(principal),
                "claimed_module_id": claimed,
                "outcome": match &outcome {
                    Ok(_) => "answered",
                    Err(refusal) => refusal.code,
                },
            }),
        );
        Some(match outcome {
            Ok(result) => HandlerOutcome::Response(
                serde_json::to_vec(&json!({ "result": result })).expect("an answer encodes"),
            ),
            Err(refusal) => HandlerOutcome::Error {
                code: refusal.code.to_string(),
                message: refusal.message,
            },
        })
    }
}

/// How a principal is named in logs: `reserved:<module id>`, `direct`, `unverified`, or
/// `none` for a route bound without one.
pub fn principal_label(principal: Option<&Principal>) -> String {
    match principal {
        Some(Principal::Reserved { module_id }) => format!("reserved:{module_id}"),
        Some(Principal::Direct) => "direct".to_string(),
        Some(Principal::Unverified) => "unverified".to_string(),
        None => "none".to_string(),
    }
}

#[async_trait]
impl<H: ModuleHandler> ModuleHandler for IssuanceHandler<H> {
    async fn handle(&self, ctx: RequestCtx, body: Vec<u8>) -> HandlerOutcome {
        let principal = self
            .principals()
            .get(&route_key(&ctx.route_handle()))
            .cloned()
            .flatten();
        match self.answer(principal.as_ref(), &body).await {
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

    async fn health(&self) -> HealthReport {
        let Some(damage) = self.issuance.damage() else {
            return self.inner.health().await;
        };
        HealthReport {
            status: HealthStatus::Failing,
            detail: Some("bus.health.down".to_string()),
            metrics: Some(json!({
                "class": "Unavailable",
                "cause": HIGH_WATER_DAMAGED_CAUSE,
                "path": damage.path().display().to_string(),
                "message": damage.to_string(),
            })),
        }
    }

    async fn on_route_gone(&self, handle: &RouteHandle) {
        self.principals().remove(&route_key(handle));
        self.inner.on_route_gone(handle).await;
    }
}

/// The live generation from `supervisor.spawn_snapshot`, over ck-bus's own client
/// connection to the daemon.
pub struct SpawnSnapshotGenerations {
    connection_file: std::path::PathBuf,
    consumer: OnceCell<SubcConsumer>,
}

impl SpawnSnapshotGenerations {
    pub fn new(connection_file: std::path::PathBuf) -> Self {
        Self {
            connection_file,
            consumer: OnceCell::new(),
        }
    }
}

#[async_trait]
impl LiveGenerations for SpawnSnapshotGenerations {
    async fn live_generation(&self, module_id: &str) -> Result<Option<u64>, String> {
        let consumer = self
            .consumer
            .get_or_try_init(|| async {
                SubcConsumer::connect(&self.connection_file, ConsumerOptions::default()).await
            })
            .await
            .map_err(|error| format!("cannot reach the daemon: {error}"))?;
        let snapshot = consumer
            .spawn_snapshot()
            .await
            .map_err(|error| error.to_string())?;
        // The supervisor runs one process per module id; the highest generation is the
        // live one should the snapshot ever list two.
        Ok(snapshot
            .live
            .iter()
            .filter(|spawn| spawn.module_id == module_id)
            .map(|spawn| spawn.spawn_generation)
            .max())
    }
}

/// The plane from bootstrap's published `Ready` and the broker URL in the environment.
pub struct ReadyPlane {
    ready: watch::Receiver<Option<Arc<Ready>>>,
    server_url: Option<String>,
}

impl ReadyPlane {
    pub fn new(ready: watch::Receiver<Option<Arc<Ready>>>) -> Self {
        Self {
            ready,
            server_url: BrokerConfig::from_env().ok().map(|config| config.url),
        }
    }
}

impl PlaneSource for ReadyPlane {
    fn current(&self) -> Option<Plane> {
        let ready = self.ready.borrow().clone()?;
        let names = grants::derive_account(&ready.account.acct).ok()?;
        Some(Plane {
            names,
            account_public: ready.account.account_public.clone(),
            server_url: self.server_url.clone()?,
            box_plane: ready.box_plane.clone(),
        })
    }
}

/// What `main.rs` serves: the manifest with the issuance surface, and the wrapped
/// handler.
pub struct Wired<H> {
    pub manifest: ModuleManifest,
    pub handler: IssuanceHandler<H>,
}

/// The one wiring call: issuance over the daemon connection file from `--subc`, the
/// store root, the credentials area and bootstrap's `Ready`.
pub fn wire<H: ModuleHandler>(
    builder: ModuleManifestBuilder,
    inner: H,
    credentials: Arc<Credentials>,
    store_root: &Path,
    ready: watch::Receiver<Option<Arc<Ready>>>,
) -> Result<Wired<H>, String> {
    let connection_file = crate::credentials::vault::subc_arg(std::env::args_os())
        .ok_or("ck-bus needs --subc <connection file> to read the spawn snapshot")?;
    let issuance = Arc::new(Issuance::new(
        credentials,
        store_root,
        Arc::new(SpawnSnapshotGenerations::new(connection_file)),
        Arc::new(ReadyPlane::new(ready)),
    ));
    Ok(Wired {
        manifest: advertise(builder),
        handler: IssuanceHandler::new(inner, issuance),
    })
}
