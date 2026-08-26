#![forbid(unsafe_code)]

use std::{
    error::Error,
    fs,
    io::Write as _,
    path::{Path, PathBuf},
};

use serde_json::{json, Value};
use subc_client_rs::{async_trait, BindDecision, HandlerOutcome, ModuleHandler, RequestCtx};
use subc_protocol::{
    manifest::{
        Bindings, Concurrency, ExecutionMode, IdentityBinding, IdentityScope, ProviderRole,
        StorageBinding, StorageKind, StorageScope, Tool, TrustTier,
    },
    ModuleHelloAckBody, PROTOCOL_VERSION,
};
use tokio::time::{sleep, Duration};

const DEFAULT_MODULE_ID: &str = "subc-client-rs-echo";
const EVENTS_ENV: &str = "SUBC_MODULE_ECHO_EVENTS";

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    let module_id = std::env::var(subc_protocol::SUBC_MODULE_ID_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_MODULE_ID.to_string());
    let events_path = std::env::var_os(EVENTS_ENV).map(PathBuf::from);
    subc_client_rs::serve(manifest(&module_id), EchoHandler { events_path }).await?;
    Ok(())
}

struct EchoHandler {
    events_path: Option<PathBuf>,
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
}

impl EchoHandler {
    fn record(&self, event: Value) {
        let Some(path) = self.events_path.as_ref() else {
            return;
        };
        let _ = append_json_line(path, event);
    }
}

fn append_json_line(path: &Path, event: Value) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{event}")
}

fn manifest(module_id: &str) -> subc_protocol::manifest::ModuleManifest {
    subc_protocol::manifest::ModuleManifest {
        module_id: module_id.to_string(),
        module_version: env!("CARGO_PKG_VERSION").to_string(),
        protocol_ver: PROTOCOL_VERSION,
        trust_tier: TrustTier::FirstParty,
        provides: vec![ProviderRole::ToolProvider {
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
        }],
        consumes: Vec::new(),
        bindings: Bindings {
            storage: StorageBinding {
                kind: StorageKind::Sqlite,
                scope: StorageScope::Project,
                owns_schema: false,
            },
            vault_grants: Vec::new(),
            identity: IdentityBinding {
                requires: vec![IdentityScope::Project],
                optional: vec![IdentityScope::Session],
            },
        },
        capabilities: None,
        provenance: None,
    }
}
