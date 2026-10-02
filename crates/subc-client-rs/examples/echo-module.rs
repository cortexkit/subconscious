#![forbid(unsafe_code)]

use std::{
    error::Error,
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{SystemTime, UNIX_EPOCH},
};

use serde_json::{json, Value};
use subc_client_rs::{
    async_trait, BindDecision, ConnectionEnd, HandlerOutcome, HealthReport, ModuleHandler,
    RequestCtx, RouteCloseReason,
};
use subc_protocol::{
    manifest::{
        Concurrency, ExecutionMode, IdentityScope, ProviderRole, SelfSignalDeclaration,
        SelfSignalEffect, SelfSignalKind, SignalAnchor, Tool,
    },
    ModuleHelloAckBody,
};
use tokio::time::{sleep, Duration};

const DEFAULT_MODULE_ID: &str = "subc-client-rs-echo";
const EVENTS_ENV: &str = "SUBC_MODULE_ECHO_EVENTS";
/// When set, the module declares provenance, which the SDK completes with
/// where the launch nonce was read from.
const PROVENANCE_ENV: &str = "SUBC_MODULE_ECHO_PROVENANCE";
/// When set to a number of milliseconds, the module declares a `Busy` gauge
/// that holds a daemon drain open: it reads 1 from startup until that long
/// after the drain notice arrives, then 0.
const BUSY_AFTER_DRAIN_ENV: &str = "SUBC_MODULE_ECHO_BUSY_AFTER_DRAIN_MS";
/// The health gauge named by the `Busy` self-signal.
const BUSY_GAUGE: &str = "drain_work";

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    let nonce_env_present = std::env::var_os(subc_protocol::SUBC_LAUNCH_NONCE_ENV).is_some();
    let nonce_fd_env_present =
        std::env::var_os(subc_client_rs::launch_nonce::LAUNCH_NONCE_FD_ENV).is_some();
    // First, before anything could spawn a child that would inherit the
    // still-unread nonce descriptor.
    let launch_nonce = subc_client_rs::launch_nonce();
    let module_id = std::env::var(subc_protocol::SUBC_MODULE_ID_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_MODULE_ID.to_string());
    let events_path = std::env::var_os(EVENTS_ENV).map(PathBuf::from);
    if let Some(path) = &events_path {
        let source = match &launch_nonce {
            Ok(Some(nonce)) => nonce.source().as_str().to_string(),
            Ok(None) => "none".to_string(),
            Err(error) => format!("error: {error}"),
        };
        let _ = append_json_line(
            path,
            json!({"kind": "launch_nonce", "source": source,
            "env_present": nonce_env_present, "fd_env_present": nonce_fd_env_present}),
        );
    }
    let busy_after_drain = std::env::var(BUSY_AFTER_DRAIN_ENV)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(Duration::from_millis);
    let mut manifest = manifest(&module_id, busy_after_drain.is_some());
    if std::env::var_os(PROVENANCE_ENV).is_some() {
        manifest.provenance = Some(subc_client_rs::build_provenance(None, None, None)?);
    }
    subc_client_rs::serve(
        manifest,
        EchoHandler {
            events_path,
            busy_after_drain,
            busy: Arc::new(AtomicU64::new(1)),
        },
    )
    .await?;
    Ok(())
}

struct EchoHandler {
    events_path: Option<PathBuf>,
    /// Set when the module declares its `Busy` gauge: how long after the drain
    /// notice the gauge keeps reading 1.
    busy_after_drain: Option<Duration>,
    /// The `Busy` gauge's value.
    busy: Arc<AtomicU64>,
}

#[async_trait]
impl ModuleHandler for EchoHandler {
    async fn handle(&self, ctx: RequestCtx, body: Vec<u8>) -> HandlerOutcome {
        let request = serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null);
        match request.get("kind").and_then(Value::as_str) {
            Some("error") => HandlerOutcome::Error {
                code: "example_error".to_string(),
                message: "clean example error".to_string(),
            },
            Some("stream") => match ctx.emit(b"stream-event".to_vec()).await {
                Ok(()) => HandlerOutcome::Streamed,
                Err(error) => HandlerOutcome::Error {
                    code: "emit_failed".to_string(),
                    message: error.to_string(),
                },
            },
            Some("stream_many") => {
                let count = request.get("count").and_then(Value::as_u64).unwrap_or(3);
                for index in 0..count {
                    if let Err(error) = ctx.emit(format!("stream-event-{index}").into_bytes()).await
                    {
                        return HandlerOutcome::Error {
                            code: "emit_failed".to_string(),
                            message: error.to_string(),
                        };
                    }
                }
                HandlerOutcome::Streamed
            }
            Some("sleep") => {
                let ms = request.get("ms").and_then(Value::as_u64).unwrap_or(100);
                self.record(json!({
                    "kind": "sleep_started",
                    "channel": ctx.route_handle().channel,
                    "corr": ctx.corr(),
                    "ms": ms,
                }));
                sleep(Duration::from_millis(ms)).await;
                match serde_json::to_vec(&json!({ "ok": true, "slept_ms": ms })) {
                    Ok(response) => HandlerOutcome::Response(response),
                    Err(error) => HandlerOutcome::Error {
                        code: "encode_failed".to_string(),
                        message: error.to_string(),
                    },
                }
            }
            Some("cancel") => {
                let tag = request.get("tag").cloned().unwrap_or(Value::Null);
                self.record(json!({
                    "kind": "cancel_waiting",
                    "channel": ctx.route_handle().channel,
                    "corr": ctx.corr(),
                    "tag": tag.clone(),
                }));
                ctx.cancelled().await;
                self.record(json!({
                    "kind": "cancelled",
                    "channel": ctx.route_handle().channel,
                    "corr": ctx.corr(),
                    "tag": tag,
                }));
                HandlerOutcome::Error {
                    code: "cancelled".to_string(),
                    message: "handler observed cancellation".to_string(),
                }
            }
            _ => match serde_json::to_vec(&json!({ "ok": true, "echo": request })) {
                Ok(response) => HandlerOutcome::Response(response),
                Err(error) => HandlerOutcome::Error {
                    code: "encode_failed".to_string(),
                    message: error.to_string(),
                },
            },
        }
    }

    async fn on_hello_ack(&self, ack: &ModuleHelloAckBody) {
        self.record(json!({
            "kind": "hello_ack",
            "negotiated_ver": ack.negotiated_ver,
        }));
    }

    async fn on_bind(&self, req: &subc_client_rs::RouteBindRequest) -> BindDecision {
        self.record(json!({
            "kind": "bind",
            "route_channel": req.handle.channel,
            "route_epoch": req.handle.epoch,
            "target": &req.target,
            "identity": &req.identity,
        }));
        BindDecision::accept()
    }

    async fn on_route_gone(&self, handle: &subc_client_rs::RouteHandle) {
        self.record(json!({
            "kind": "route_gone",
            "route_channel": handle.channel,
            "route_epoch": handle.epoch,
        }));
    }

    async fn health(&self) -> HealthReport {
        if self.busy_after_drain.is_none() {
            return HealthReport {
                detail: Some("no health implementation; inherited default".to_string()),
                ..HealthReport::ok()
            };
        }
        let busy = self.busy.load(Ordering::SeqCst);
        self.record(json!({ "kind": "health", BUSY_GAUGE: busy }));
        HealthReport {
            metrics: Some(json!({ BUSY_GAUGE: busy })),
            ..HealthReport::ok()
        }
    }

    async fn on_draining(&self, reason: RouteCloseReason, deadline: SystemTime) {
        self.record(json!({
            "kind": "draining",
            "reason": format!("{reason:?}"),
            "deadline_ms": unix_ms(deadline),
        }));
        // Simulate in-flight work that keeps running for a while after the
        // module stops taking new work; the busy gauge covers it.
        if let Some(delay) = self.busy_after_drain {
            sleep(delay).await;
            self.busy.store(0, Ordering::SeqCst);
            self.record(json!({ "kind": "busy_done" }));
        }
    }

    async fn on_connection_end(&self, end: ConnectionEnd) {
        self.record(json!({ "kind": "connection_end", "end": end.as_str() }));
    }
}

impl EchoHandler {
    /// Append `event`, stamped with this process's id (so a test can tell the
    /// process that drained from the one that replaced it) and the wall-clock
    /// time in Unix milliseconds.
    fn record(&self, mut event: Value) {
        let Some(path) = self.events_path.as_ref() else {
            return;
        };
        if let Some(fields) = event.as_object_mut() {
            fields.insert("pid".to_string(), json!(std::process::id()));
            fields.insert("at_ms".to_string(), json!(unix_ms(SystemTime::now())));
        }
        let _ = append_json_line(path, event);
    }
}

fn unix_ms(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map(|since| u64::try_from(since.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// One `write_all`, never `writeln!`: this module serves requests concurrently,
/// and `writeln!` emits a `write` syscall per JSON fragment, so two writers
/// interleave mid-line and a line-parsing reader drops both records silently.
/// Measured at 1576 of 1600 events lost with 8 concurrent appenders.
fn append_json_line(path: &Path, event: Value) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(format!("{event}\n").as_bytes())
}

fn manifest(module_id: &str, declare_busy: bool) -> subc_protocol::manifest::ModuleManifest {
    // How a module keeps a drain open while its own work finishes: a `Busy`
    // self-signal anchored to health gauges. The daemon probes `health` during
    // the drain and waits until every named gauge reads 0.
    let self_signals = declare_busy.then(|| {
        vec![SelfSignalDeclaration {
            name: "drain_work".to_string(),
            kind: SelfSignalKind::Busy,
            effect: SelfSignalEffect::Observe,
            anchored_to: SignalAnchor::HealthGauges {
                gauges: vec![BUSY_GAUGE.to_string()],
            },
            cadence: None,
            domain: None,
            note: None,
        }]
    });
    subc_protocol::manifest::ModuleManifest::builder(module_id, env!("CARGO_PKG_VERSION"))
        .self_signals(self_signals)
        .provides(vec![ProviderRole::ToolProvider {
            tools: vec![Tool {
                name: "echo".to_string(),
                description: None,
                execution_mode: ExecutionMode::Pure,
                schema: json!({"type": "object"}),
            }],
            identity_scope: vec![IdentityScope::Project, IdentityScope::Session],
            concurrency: Concurrency::ModuleManaged,
            emits_push: true,
            sub_supervises: true,
        }])
        .build()
}
