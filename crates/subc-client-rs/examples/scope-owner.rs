#![forbid(unsafe_code)]

//! A supervised module that owns scopes through the SDK's `ModuleHandle`.
//!
//! Only a module the daemon launched itself may sync scopes, so the
//! `scope.sync` / `scope.describe` helpers can only be exercised end to end
//! from a process the daemon supervises. This one connects, then runs the
//! steps in the JSON file named by `SUBC_SCOPE_OWNER_SCRIPT`, in order, and
//! appends one JSON line per step to `SUBC_SCOPE_OWNER_RESULTS`. A step is
//! either `{"op": "sync", "generation": N, "scopes": [ScopeRecord, ...]}` or
//! `{"op": "describe", "owner": Principal, "ref": "..."}`. After the last step
//! it writes `{"done": true}` and keeps serving, so the supervisor does not
//! restart it and run the steps a second time.

use std::{
    error::Error,
    fs,
    io::Write as _,
    path::{Path, PathBuf},
};

use serde_json::{json, Value};
use subc_client_rs::{
    async_trait, HandlerOutcome, ModuleHandler, RequestCtx, ScopeCallError, ScopeDescribeReply,
    ScopeSyncReply,
};
use subc_protocol::{
    manifest::{Concurrency, ExecutionMode, IdentityScope, ModuleManifest, ProviderRole, Tool},
    scope::ScopeRecord,
    Principal,
};

const DEFAULT_MODULE_ID: &str = "subc-client-rs-scope-owner";
const SCRIPT_ENV: &str = "SUBC_SCOPE_OWNER_SCRIPT";
const RESULTS_ENV: &str = "SUBC_SCOPE_OWNER_RESULTS";

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    // First, before anything could spawn a child that would inherit the
    // still-unread nonce descriptor.
    let _ = subc_client_rs::launch_nonce();
    let module_id = std::env::var(subc_protocol::SUBC_MODULE_ID_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_MODULE_ID.to_string());
    let connection_file = subc_arg().ok_or("missing --subc <connection-file>")?;
    let script: Vec<Value> = match std::env::var_os(SCRIPT_ENV) {
        Some(path) => serde_json::from_slice(&fs::read(path)?)?,
        None => Vec::new(),
    };
    let results_path = std::env::var_os(RESULTS_ENV).map(PathBuf::from);

    let (handle, serve) =
        subc_client_rs::serve_with_handle(&connection_file, manifest(&module_id), IdleHandler)
            .await?;
    let serving = tokio::spawn(serve);

    for (step, request) in script.iter().enumerate() {
        let outcome = match request.get("op").and_then(Value::as_str) {
            Some("sync") => {
                let generation = request["generation"].as_u64().unwrap_or(0);
                let scopes: Vec<ScopeRecord> = serde_json::from_value(request["scopes"].clone())?;
                match handle.scope_sync(generation, scopes).await {
                    Ok(reply) => json!({ "ok": sync_json(&reply) }),
                    Err(error) => error_json(&error),
                }
            }
            Some("describe") => {
                let owner: Principal = serde_json::from_value(request["owner"].clone())?;
                let scope_ref = request["ref"].as_str().unwrap_or_default().to_string();
                match handle.scope_describe(owner, scope_ref).await {
                    Ok(reply) => json!({ "ok": describe_json(&reply) }),
                    Err(error) => error_json(&error),
                }
            }
            other => json!({ "other": format!("unknown step op {other:?}") }),
        };
        record(
            results_path.as_deref(),
            json!({ "step": step, "result": outcome }),
        );
    }
    record(results_path.as_deref(), json!({ "done": true }));

    serving.await??;
    Ok(())
}

/// Report a refusal by the daemon's typed code, read from
/// [`ScopeCallError::Refused`]; every other failure is reported as `other`,
/// so a refusal that lost its code shows up as the wrong kind.
fn error_json(error: &ScopeCallError) -> Value {
    match error {
        ScopeCallError::Refused { code, message } => {
            json!({ "refused": { "code": code, "message": message } })
        }
        other => json!({ "other": format!("{other:?}") }),
    }
}

fn sync_json(reply: &ScopeSyncReply) -> Value {
    json!({
        "generation": reply.generation,
        "results": reply.results,
        "ended": reply.ended,
    })
}

fn describe_json(reply: &ScopeDescribeReply) -> Value {
    json!({
        "status": reply.status,
        "scope_epoch": reply.scope_epoch,
        "daemon_incarnation": reply.daemon_incarnation,
        "owner_synced": reply.owner_synced,
        "owner_configured": reply.owner_configured,
        "scope": reply.scope,
    })
}

fn subc_arg() -> Option<PathBuf> {
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--subc" {
            return args.next().map(PathBuf::from);
        }
        if let Some(raw) = arg.to_str().and_then(|arg| arg.strip_prefix("--subc=")) {
            return Some(PathBuf::from(raw));
        }
    }
    None
}

/// Append `event` as one JSON line. The steps run one at a time, so lines
/// never interleave.
fn record(path: Option<&Path>, event: Value) {
    let Some(path) = path else {
        return;
    };
    let _ = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut file| file.write_all(format!("{event}\n").as_bytes()));
}

struct IdleHandler;

#[async_trait]
impl ModuleHandler for IdleHandler {
    async fn handle(&self, _ctx: RequestCtx, _body: Vec<u8>) -> HandlerOutcome {
        HandlerOutcome::Error {
            code: "not_implemented".to_string(),
            message: "the scope-owner example serves no requests".to_string(),
        }
    }
}

fn manifest(module_id: &str) -> ModuleManifest {
    ModuleManifest::builder(module_id, env!("CARGO_PKG_VERSION"))
        .provides(vec![ProviderRole::ToolProvider {
            tools: vec![Tool {
                name: "noop".to_string(),
                description: None,
                execution_mode: ExecutionMode::Pure,
                schema: json!({"type": "object"}),
            }],
            identity_scope: vec![IdentityScope::Project],
            concurrency: Concurrency::ModuleManaged,
            emits_push: false,
            sub_supervises: false,
        }])
        .build()
}
