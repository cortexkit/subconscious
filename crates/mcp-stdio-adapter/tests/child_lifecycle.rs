use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use mcp_stdio_adapter::{
    adapter::{AdapterHandler, CredentialResolutionError, CredentialResolver, LifecycleSettings},
    constants::BASE_ENV_KEYS,
    registry::{parse_document, ServerRegistry},
};
use serde_json::{json, Value};
use subc_client_rs::{async_trait, HandlerOutcome};

struct RotatingResolver {
    value: Mutex<String>,
    calls: Mutex<u64>,
}

impl RotatingResolver {
    fn new(value: &str) -> Self {
        Self {
            value: Mutex::new(value.to_string()),
            calls: Mutex::new(0),
        }
    }

    fn rotate(&self, value: &str) {
        *self.value.lock().unwrap() = value.to_string();
    }

    fn calls(&self) -> u64 {
        *self.calls.lock().unwrap()
    }
}

#[async_trait]
impl CredentialResolver for RotatingResolver {
    async fn resolve(&self, _handle: &str) -> Result<String, CredentialResolutionError> {
        *self.calls.lock().unwrap() += 1;
        Ok(self.value.lock().unwrap().clone())
    }
}

struct MissingResolver;

#[async_trait]
impl CredentialResolver for MissingResolver {
    async fn resolve(&self, _handle: &str) -> Result<String, CredentialResolutionError> {
        Err(CredentialResolutionError)
    }
}

fn fixture_path() -> String {
    warm_fixture();
    env!("CARGO_BIN_EXE_fake-mcp-child").to_string()
}

/// Pay the macOS first-exec assessment toll on the freshly built fixture
/// binary once, outside any spawn/initialize budget. Cargo mints a new inode
/// for the fixture on every rebuild, and under host load the kernel's
/// first-execution assessment of a new inode can stall for tens of seconds;
/// unwarmed, that toll lands inside spawn_initialize_budget and converts
/// framing/idle-shed tests into initialize_failed flakes. The fixture exits
/// on stdin EOF, so a null-stdin run terminates immediately once assessed.
fn warm_fixture() {
    static WARM: std::sync::Once = std::sync::Once::new();
    WARM.call_once(|| {
        let _ = std::process::Command::new(env!("CARGO_BIN_EXE_fake-mcp-child"))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    });
}

fn test_settings() -> LifecycleSettings {
    LifecycleSettings {
        spawn_initialize_budget: Duration::from_secs(30),
        spawn_attempt_budget: 3,
        spawn_retry_cooldown: Duration::from_secs(60),
        eviction_grace: Duration::ZERO,
        idle_ttl_override: Some(Duration::ZERO),
    }
}

fn registry(servers: Value) -> ServerRegistry {
    parse_document(Path::new("fixture-registry.jsonc"), &servers.to_string())
        .expect("fixture registry parses")
        .0
}

fn server(env: Value) -> Value {
    json!({
        "command": fixture_path(),
        "env": env,
    })
}

async fn call(handler: &AdapterHandler, server: &str, method: &str, params: Value) -> Value {
    let outcome = handler
        .route_outcome(
            &serde_json::to_vec(&json!({
                "server": server,
                "op": method,
                "payload": { "method": method, "params": params },
            }))
            .unwrap(),
        )
        .await;
    let HandlerOutcome::Response(body) = outcome else {
        panic!("fixture call must succeed: {outcome:?}");
    };
    serde_json::from_slice(&body).unwrap()
}

async fn refusal(handler: &AdapterHandler, server: &str, method: &str) -> (String, Value) {
    let outcome = handler
        .route_outcome(
            &serde_json::to_vec(&json!({
                "server": server,
                "op": method,
                "payload": { "method": method },
            }))
            .unwrap(),
        )
        .await;
    let HandlerOutcome::ErrorWithDetail { code, detail, .. } = outcome else {
        panic!("fixture call must be refused: {outcome:?}");
    };
    (code, detail)
}

/// Mirror of the adapter's declared-override semantics: on Windows a declared
/// variable replaces base keys differing only by case (env keys are
/// case-insensitive there); on Unix keys are distinct.
fn declare(expected: &mut BTreeMap<String, String>, key: &str, value: &str) {
    #[cfg(windows)]
    expected.retain(|existing, _| !existing.eq_ignore_ascii_case(key));
    expected.insert(key.to_string(), value.to_string());
}

async fn evict_after_test_ttl() {
    for _ in 0..5 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test]
async fn real_stdio_child_is_lazily_spawned_isolated_and_resolved_again_after_idle_shed() {
    let resolver = Arc::new(RotatingResolver::new("first-secret"));
    let mut env = serde_json::Map::new();
    env.insert("FIXTURE_MODE".to_string(), json!({"value": "normal"}));
    env.insert("PATH".to_string(), json!({"value": "shadowed-path"}));
    env.insert("TAGGED".to_string(), json!({"handle": "vault:fixture"}));
    let handler = AdapterHandler::with_resolver(
        registry(json!({"fixture": server(Value::Object(env))})),
        resolver.clone(),
        test_settings(),
    );

    assert_eq!(handler.metrics().snapshot()["children_live"], 0);
    let first = call(&handler, "fixture", "tools/call", json!({"check": "env"})).await;
    assert_eq!(first["served_from"], "live");
    assert!(first.get("spawn_elapsed_ms").is_some());
    assert_eq!(handler.metrics().snapshot()["spawns_total"], 1);

    let child_environment: BTreeMap<String, String> =
        serde_json::from_value(first["payload"]["environment"].clone()).unwrap();
    let mut expected: BTreeMap<String, String> = BASE_ENV_KEYS
        .iter()
        .filter_map(|key| {
            std::env::var(key)
                .ok()
                .map(|value| ((*key).to_string(), value))
        })
        .collect();
    declare(&mut expected, "FIXTURE_MODE", "normal");
    declare(&mut expected, "PATH", "shadowed-path");
    declare(&mut expected, "TAGGED", "first-secret");
    assert_eq!(child_environment, expected);
    assert!(!child_environment.contains_key("SUBC_MODULE_ID"));
    assert!(!child_environment.contains_key("SUBC_LAUNCH_NONCE"));

    evict_after_test_ttl().await;

    resolver.rotate("second-secret");
    let second = call(
        &handler,
        "fixture",
        "tools/call",
        json!({"check": "rotation"}),
    )
    .await;
    assert_eq!(second["served_from"], "live");
    assert_eq!(second["payload"]["environment"]["TAGGED"], "second-secret");
    assert_ne!(first["payload"]["pid"], second["payload"]["pid"]);
    assert_eq!(handler.metrics().snapshot()["spawns_total"], 2);
    assert_eq!(handler.metrics().snapshot()["idle_evictions_total"], 1);
    assert_eq!(resolver.calls(), 2);

    evict_after_test_ttl().await;
}

#[tokio::test]
async fn spawn_failed_refusal_fence_has_retry_after_ms_for_an_unexecutable_child() {
    let handler = AdapterHandler::with_resolver(
        registry(json!({"missing": {"command": "/definitely/not/a/real/mcp-child"}})),
        Arc::new(MissingResolver),
        test_settings(),
    );

    let (code, detail) = refusal(&handler, "missing", "tools/list").await;

    assert_eq!(code, "spawn_failed");
    assert_eq!(detail["cause"], "exec");
    assert!(detail.get("retry_after_ms").is_some());
    assert_eq!(handler.metrics().snapshot()["spawns_total"], 0);
}

#[tokio::test]
async fn vault_miss_spawn_failed_refusal_fence_names_variable_not_handle_or_secret() {
    let handler = AdapterHandler::with_resolver(
        registry(json!({
            "vaulted": server(json!({"TOKEN": {"handle": "vault:never-echo-this"}}))
        })),
        Arc::new(MissingResolver),
        test_settings(),
    );

    let (code, detail) = refusal(&handler, "vaulted", "tools/list").await;
    let rendered = detail.to_string();

    assert_eq!(code, "spawn_failed");
    assert_eq!(detail["cause"], "credential_resolution");
    assert_eq!(detail["env_var"], "TOKEN");
    assert!(!rendered.contains("never-echo-this"));
    assert_eq!(handler.metrics().snapshot()["spawns_total"], 0);
}

#[tokio::test]
async fn framing_kill_refusal_fence_names_the_ceiling_and_other_server_remains_live() {
    let handler = AdapterHandler::with_resolver(
        registry(json!({
            "oversized": {
                "command": fixture_path(),
                "frame_ceiling_bytes": 64,
                "env": {"FIXTURE_MODE": {"value": "oversized"}}
            },
            "normal": server(json!({"FIXTURE_MODE": {"value": "normal"}})),
        })),
        Arc::new(MissingResolver),
        test_settings(),
    );

    let (code, detail) = refusal(&handler, "oversized", "tools/list").await;

    assert_eq!(code, "child_framing_error");
    assert_eq!(detail["ceiling_bytes"], 64);
    assert!(detail["observed_bytes"].as_u64().unwrap() > 64);
    assert_eq!(handler.metrics().snapshot()["children_live"], 0);
    let normal = call(&handler, "normal", "tools/list", json!({})).await;
    assert_eq!(normal["payload"]["tools"][0]["name"], "fixture");

    evict_after_test_ttl().await;
}
