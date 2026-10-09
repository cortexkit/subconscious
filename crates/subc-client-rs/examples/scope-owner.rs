#![forbid(unsafe_code)]

//! A supervised module that owns scopes through the SDK's `ModuleHandle`.
//!
//! Only a module the daemon launched itself may sync scopes, so the
//! `scope.sync` / `scope.apply` / `scope.describe` helpers are exercised end to end
//! from a process the daemon supervises. This one connects, then runs the
//! steps in the JSON file named by `SUBC_SCOPE_OWNER_SCRIPT`, in order, and
//! appends one JSON line per step to `SUBC_SCOPE_OWNER_RESULTS`. A step is
//! `{"op": "sync", "generation": N, "scopes": [ScopeRecord, ...]}`,
//! `{"op": "apply", "generation": N, "upsert": [ScopeRecord, ...], "end": [ScopeEnd, ...]}`,
//! or `{"op": "describe", "owner": Principal, "ref": "..."}`. Records may use
//! the script-only `expires_in_ms` instead of `expires_at_ms`; the step resolves
//! it against Unix wall time when it runs. Each results line includes the
//! resolved request, so a caller can observe the exact deadline sent. After the last step
//! it writes `{"done": true}` and keeps serving, so the supervisor does not
//! restart it and run the steps a second time.

use std::{
    error::Error,
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde_json::{json, Value};
use subc_client_rs::{
    async_trait, HandlerOutcome, ModuleHandler, RequestCtx, ScopeApplyReply, ScopeCallError,
    ScopeDescribeReply, ScopeSyncReply,
};
use subc_protocol::{
    manifest::{Concurrency, ExecutionMode, IdentityScope, ModuleManifest, ProviderRole, Tool},
    scope::{ScopeEnd, ScopeRecord},
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
        let mut request = request.clone();
        let outcome = match request.get("op").and_then(Value::as_str) {
            Some("sync") => {
                let generation = request["generation"].as_u64().unwrap_or(0);
                resolve_scope_expiries(&mut request["scopes"], wall_now_ms()?)?;
                let scopes: Vec<ScopeRecord> = serde_json::from_value(request["scopes"].clone())?;
                match handle.scope_sync(generation, scopes).await {
                    Ok(reply) => json!({ "ok": sync_json(&reply) }),
                    Err(error) => error_json(&error),
                }
            }
            Some("apply") => {
                let generation = request["generation"].as_u64().unwrap_or(0);
                resolve_scope_expiries(&mut request["upsert"], wall_now_ms()?)?;
                let upsert: Vec<ScopeRecord> = serde_json::from_value(request["upsert"].clone())?;
                let end: Vec<ScopeEnd> = serde_json::from_value(request["end"].clone())?;
                match handle.scope_apply(generation, upsert, end).await {
                    Ok(reply) => json!({ "ok": apply_json(&reply) }),
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
            json!({ "step": step, "request": request, "result": outcome }),
        );
    }
    record(results_path.as_deref(), json!({ "done": true }));

    serving.await??;
    Ok(())
}

fn wall_now_ms() -> Result<u64, Box<dyn Error + Send + Sync>> {
    Ok(u64::try_from(
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis(),
    )?)
}

/// Resolve relative deadlines before decoding the strict wire type: the
/// daemon must receive only the absolute deadline, never `expires_in_ms`.
fn resolve_scope_expiries(
    records: &mut Value,
    now_ms: u64,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    for record in records
        .as_array_mut()
        .ok_or("scope records must be an array")?
    {
        let fields = record
            .as_object_mut()
            .ok_or("scope record must be an object")?;
        if let Some(relative) = fields.remove("expires_in_ms") {
            if fields.contains_key("expires_at_ms") {
                return Err("use only one of expires_in_ms and expires_at_ms".into());
            }
            let relative = relative.as_u64().ok_or("expires_in_ms must be a u64")?;
            let deadline = now_ms
                .checked_add(relative)
                .ok_or("scope deadline overflow")?;
            fields.insert("expires_at_ms".to_string(), json!(deadline));
        }
    }
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

fn apply_json(reply: &ScopeApplyReply) -> Value {
    json!({
        "generation": reply.generation,
        "results": reply.results,
        "end_results": reply.end_results,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_expiry_becomes_an_absolute_wire_deadline() {
        let mut records = json!([
            {"ref": "run", "scope_epoch": 1, "kind": "ephemeral", "expires_in_ms": 15000},
            {"ref": "absolute", "scope_epoch": 2, "kind": "ephemeral", "expires_at_ms": 20000},
            {"ref": "unbounded", "scope_epoch": 1, "kind": "ephemeral"}
        ]);
        resolve_scope_expiries(&mut records, 1000).unwrap();
        assert_eq!(records[0]["expires_at_ms"], 16000);
        assert!(records[0].get("expires_in_ms").is_none());
        assert_eq!(records[1]["expires_at_ms"], 20000);
        assert!(records[2].get("expires_at_ms").is_none());
        let decoded: Vec<ScopeRecord> = serde_json::from_value(records).unwrap();
        assert_eq!(decoded[0].expires_at_ms, Some(16000));
        assert_eq!(decoded[1].expires_at_ms, Some(20000));
        assert_eq!(decoded[2].expires_at_ms, None);
    }

    #[test]
    fn malformed_or_overflowing_relative_expiries_are_script_errors() {
        for record in [
            json!({"expires_in_ms": -1}),
            json!({"expires_in_ms": "15000"}),
            json!({"expires_in_ms": 1, "expires_at_ms": 20000}),
            json!({"expires_in_ms": u64::MAX}),
        ] {
            assert!(resolve_scope_expiries(&mut json!([record]), 1).is_err());
        }
    }
}
