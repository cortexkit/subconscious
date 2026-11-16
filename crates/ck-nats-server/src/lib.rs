mod bus;
mod engine;
mod wire;

use std::{
    path::PathBuf,
    sync::{Arc, OnceLock},
    time::SystemTime,
};

use serde_json::{json, Value};
use subc_client_rs::{
    async_trait, ConnectionEnd, HandlerOutcome, HealthReport, HealthStatus, ModuleHandler,
    RequestCtx, RouteCloseReason,
};
use subc_protocol::manifest::ModuleManifest;
use tokio::sync::RwLock;

use engine::Engine;
use wire::{JetStream, Leaf, LeafState, Listener};

#[derive(Default)]
struct Snapshot {
    ready: bool,
    status: Option<(JetStream, Vec<Leaf>, Option<Listener>)>,
    apply_failure: Option<(u64, String)>,
}

fn short(text: &str) -> String {
    text.chars().take(256).collect()
}

impl Snapshot {
    fn health(&self, hub: bool) -> HealthReport {
        let mut report = HealthReport {
            status: HealthStatus::Failing,
            detail: Some("Waiting for engine ready and status.".into()),
            metrics: None,
        };
        let Some((jetstream, leafs, listener)) = &self.status else {
            return report;
        };
        let failing = !self.ready
            || (jetstream.enabled && !jetstream.ok)
            || (hub && !listener.as_ref().is_some_and(|l| l.bound));
        let degraded = self.apply_failure.is_some()
            || leafs
                .iter()
                .any(|l| !matches!(l.state, LeafState::Connected));
        report.status = if failing {
            HealthStatus::Failing
        } else if degraded {
            HealthStatus::Degraded
        } else {
            HealthStatus::Ok
        };
        report.detail = Some(
            if failing {
                "Engine is not ready, JetStream is unavailable, or the hub listener is unbound."
            } else if degraded {
                "A leaf is not connected or engine state was refused."
            } else {
                "Engine is ready."
            }
            .into(),
        );
        let mut metrics = json!({
            "ready": self.ready,
            "jetstream": {"enabled":jetstream.enabled,"ok":jetstream.ok,"error":jetstream.error.as_deref().map(short)},
            "leafs": [],
            "leaf_count": leafs.len(),
            "listener": listener.as_ref().map(|l| json!({"port":l.port,"bound":l.bound,"error":l.error.as_deref().map(short)})),
            "apply_failed": self.apply_failure.as_ref().map(|(generation,error)| json!({"generation":generation,"error":short(error)})),
        });
        // The daemon's health metrics have a 16 KiB ceiling. Keep a bounded
        // summary even if the engine has many remotes or long error messages.
        let mut size = metrics.to_string().len();
        for leaf in leafs {
            let entry = json!({"remote":short(&leaf.remote),"state":leaf.state,"since_ms":leaf.since_ms,"error":leaf.error.as_deref().map(short)});
            size += entry.to_string().len() + 1;
            if size > 12_000 {
                break;
            }
            metrics["leafs"].as_array_mut().unwrap().push(entry);
        }
        report.metrics = Some(metrics);
        report
    }
}

struct Handler {
    engine: Arc<OnceLock<Engine>>,
    snapshot: Arc<RwLock<Snapshot>>,
    hub: bool,
}

#[async_trait]
impl ModuleHandler for Handler {
    async fn handle(&self, _ctx: RequestCtx, _body: Vec<u8>) -> HandlerOutcome {
        HandlerOutcome::Error {
            code: "unsupported_operation".into(),
            message: "This module provides no data routes.".into(),
        }
    }

    async fn health(&self) -> HealthReport {
        if self.engine.get().is_none() {
            return HealthReport {
                status: HealthStatus::Failing,
                detail: Some("Starting the engine.".into()),
                metrics: None,
            };
        }
        self.snapshot.read().await.health(self.hub)
    }

    async fn on_draining(&self, _reason: RouteCloseReason, deadline: SystemTime) {
        let Some(engine) = self.engine.get() else {
            return;
        };
        engine
            .stop(
                deadline
                    .duration_since(SystemTime::now())
                    .unwrap_or_default(),
            )
            .await;
    }

    async fn on_connection_end(&self, _end: ConnectionEnd) {
        if let Some(engine) = self.engine.get() {
            engine.stop(std::time::Duration::from_secs(20)).await;
        }
    }
}

pub async fn run(config: PathBuf, executable: PathBuf, connection: PathBuf) -> Result<(), String> {
    let module_id = std::env::var("SUBC_MODULE_ID").map_err(|_| "SUBC_MODULE_ID is required")?;
    let manifest = ModuleManifest::builder(&module_id, env!("CARGO_PKG_VERSION"))
        .registration_channel(true)
        .build();
    let snapshot = Arc::new(RwLock::new(Snapshot::default()));
    let bus = Arc::new(RwLock::new(None));
    let engine_slot = Arc::new(OnceLock::new());
    let handler = Handler {
        engine: engine_slot.clone(),
        snapshot: snapshot.clone(),
        hub: module_id == "nats-hub",
    };
    // Adoption marks the daemon's registration descriptor close-on-exec before
    // any engine process can inherit the module's serving channel.
    let (_handle, serving) = subc_client_rs::serve_with_handle(&connection, manifest, handler)
        .await
        .map_err(|error| error.to_string())?;
    let (engine, mut running) = Engine::spawn(&executable, &config, snapshot, bus.clone()).await?;
    engine_slot
        .set(engine.clone())
        .map_err(|_| "engine already started")?;
    let relay = tokio::spawn(bus::follow(connection, bus, engine.clone()));
    tokio::pin!(serving);
    let result = tokio::select! {
        result = &mut serving => {
            engine.stop(std::time::Duration::from_secs(20)).await;
            let _ = (&mut running).await;
            result.map_err(|e| e.to_string())
        }
        result = &mut running => {
            if engine.stopping() { serving.await.map_err(|e| e.to_string()) }
            else { Err(format!("engine terminated unexpectedly: {result:?}")) }
        }
    };
    relay.abort();
    let _ = relay.await;
    result
}

fn state_frame(generation: u64, remotes: Vec<Value>) -> Value {
    json!({"type":"state","generation":generation,"remotes":remotes})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status() -> Snapshot {
        Snapshot {
            ready: true,
            status: Some((
                JetStream {
                    enabled: true,
                    ok: true,
                    error: None,
                },
                vec![],
                None,
            )),
            apply_failure: None,
        }
    }

    #[tokio::test]
    async fn wrapper_before_engine_health_fails_and_shutdown_hooks_return() {
        let handler = Handler {
            engine: Arc::new(OnceLock::new()),
            snapshot: Arc::new(RwLock::new(status())),
            hub: false,
        };
        let health = handler.health().await;
        assert_eq!(health.status, HealthStatus::Failing);
        assert_eq!(health.detail.as_deref(), Some("Starting the engine."));
        handler
            .on_draining(RouteCloseReason::Restart, SystemTime::now())
            .await;
        handler.on_connection_end(ConnectionEnd::Goodbye).await;
    }

    #[test]
    fn wrapper_health_requires_ready_status_and_reports_engine_failures() {
        assert_eq!(
            Snapshot::default().health(false).status,
            HealthStatus::Failing
        );
        let mut snapshot = status();
        snapshot.ready = false;
        assert_eq!(snapshot.health(false).status, HealthStatus::Failing);
        snapshot.ready = true;
        assert_eq!(snapshot.health(false).status, HealthStatus::Ok);
        assert_eq!(snapshot.health(true).status, HealthStatus::Failing);
        let (jetstream, _, listener) = snapshot.status.as_mut().unwrap();
        *listener = Some(Listener {
            port: 7422,
            bound: true,
            error: None,
        });
        jetstream.ok = false;
        assert_eq!(snapshot.health(false).status, HealthStatus::Failing);
        snapshot.status.as_mut().unwrap().0.enabled = false;
        assert_eq!(snapshot.health(true).status, HealthStatus::Ok);
        snapshot.status.as_mut().unwrap().1.push(Leaf {
            remote: "r".into(),
            state: LeafState::Connecting,
            url: None,
            since_ms: 1,
            error: None,
        });
        assert_eq!(snapshot.health(false).status, HealthStatus::Degraded);
        snapshot.status.as_mut().unwrap().1[0].state = LeafState::Failed;
        assert_eq!(snapshot.health(false).status, HealthStatus::Degraded);
        snapshot.status.as_mut().unwrap().1[0].state = LeafState::Connected;
        assert_eq!(snapshot.health(false).status, HealthStatus::Ok);
        snapshot.apply_failure = Some((9, "reload refused".into()));
        let report = snapshot.health(false);
        assert_eq!(report.status, HealthStatus::Degraded);
        assert_eq!(
            report.metrics.unwrap()["apply_failed"],
            json!({"generation":9,"error":"reload refused"})
        );
    }

    #[test]
    fn wrapper_health_metrics_are_bounded_and_omit_leaf_urls() {
        let mut snapshot = status();
        snapshot.status.as_mut().unwrap().1 = (0..1000)
            .map(|n| Leaf {
                remote: format!("remote-{n}"),
                state: LeafState::Failed,
                url: Some("tls://private.invalid:7422".into()),
                since_ms: 123,
                error: Some("bad".repeat(1000)),
            })
            .collect();
        let metrics = snapshot.health(false).metrics.unwrap();
        assert!(metrics.to_string().len() < 16_384);
        assert_eq!(metrics["leaf_count"], 1000);
        assert!(metrics["leafs"].as_array().unwrap().len() < 1000);
        assert_eq!(metrics["leafs"][0]["remote"], "remote-0");
        assert!(metrics["leafs"][0].get("url").is_none());
        assert_eq!(metrics["leafs"][0]["error"].as_str().unwrap().len(), 256);
    }
}
