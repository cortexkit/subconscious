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
use tokio_util::sync::CancellationToken;

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

/// Waits until the slot's eviction timer has actually fired. The re-armed
/// timer needs a few scheduler rounds to observe the zero test TTL, so a fixed
/// yield count is not enough under load.
async fn evict_after_test_ttl(handler: &AdapterHandler, expected_evictions: u64) {
    for _ in 0..400 {
        let evictions = handler.metrics().snapshot()["idle_evictions_total"]
            .as_u64()
            .unwrap();
        if evictions >= expected_evictions {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("idle eviction count did not reach {expected_evictions}");
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

    evict_after_test_ttl(&handler, 1).await;

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

    evict_after_test_ttl(&handler, 2).await;
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
async fn wedged_child_call_ends_at_deadline_and_later_calls_are_not_queued_behind_it() {
    let handler = AdapterHandler::with_resolver(
        registry(json!({"wedged": {
            "command": fixture_path(),
            "deadline_ms": 400,
            "env": {"FIXTURE_MODE": {"value": "hang"}}
        }})),
        Arc::new(MissingResolver),
        test_settings(),
    );

    // The first call wedges inside the child; the second starts while the
    // first is still waiting, so it can only complete if the wedged wait ends
    // and releases the per-server lane.
    let (first, second) = tokio::join!(
        tokio::time::timeout(
            Duration::from_secs(20),
            refusal(&handler, "wedged", "tools/call")
        ),
        async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            tokio::time::timeout(
                Duration::from_secs(20),
                refusal(&handler, "wedged", "tools/call"),
            )
            .await
        }
    );

    let (code, _detail) = first.expect("first call must end at the configured deadline, not hang");
    assert_eq!(code, "child_unresponsive");
    let (code, _detail) = second.expect("second call must not queue behind the wedged session");
    assert_eq!(code, "child_unresponsive");
    assert_eq!(handler.metrics().snapshot()["children_live"], 0);
}

#[tokio::test]
async fn cancelled_call_stops_waiting_on_a_wedged_child() {
    let handler = AdapterHandler::with_resolver(
        registry(json!({"wedged": {
            "command": fixture_path(),
            "deadline_ms": 60000,
            "env": {"FIXTURE_MODE": {"value": "hang"}}
        }})),
        Arc::new(MissingResolver),
        test_settings(),
    );
    let metrics = Arc::clone(handler.metrics());
    let token = CancellationToken::new();
    let cancel = token.clone();
    let call = tokio::spawn(async move {
        handler
            .route_outcome_with_cancellation(
                &serde_json::to_vec(&json!({
                    "server": "wedged",
                    "op": "tools/call",
                    "payload": { "method": "tools/call" },
                }))
                .unwrap(),
                cancel,
            )
            .await
    });

    tokio::time::sleep(Duration::from_millis(200)).await;
    token.cancel();
    let outcome = tokio::time::timeout(Duration::from_secs(5), call)
        .await
        .expect("a cancelled call must stop waiting on the child")
        .expect("call task must not panic");
    let HandlerOutcome::ErrorWithDetail { code, .. } = outcome else {
        panic!("cancelled call must end as a refusal: {outcome:?}");
    };
    assert_eq!(code, "child_unresponsive");
    assert_eq!(metrics.snapshot()["children_live"], 0);
}

#[tokio::test]
async fn idle_eviction_keeps_one_timer_task_per_server_across_calls() {
    let handler = AdapterHandler::with_resolver(
        registry(json!({
            "one": server(json!({"FIXTURE_MODE": {"value": "normal"}})),
            "two": server(json!({"FIXTURE_MODE": {"value": "normal"}})),
        })),
        Arc::new(MissingResolver),
        LifecycleSettings {
            // Long production TTL: the timers stay asleep for the whole test,
            // so every live timer is observable in the metric.
            idle_ttl_override: None,
            ..test_settings()
        },
    );

    for _ in 0..3 {
        call(&handler, "one", "tools/call", json!({})).await;
    }
    assert_eq!(handler.metrics().snapshot()["eviction_timers_live"], 1);

    call(&handler, "two", "tools/call", json!({})).await;
    assert_eq!(handler.metrics().snapshot()["eviction_timers_live"], 2);
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

    evict_after_test_ttl(&handler, 1).await;
}

fn mode_handler(mode: &str) -> AdapterHandler {
    AdapterHandler::with_resolver(
        registry(json!({"fixture": server(json!({"FIXTURE_MODE": {"value": mode}}))})),
        Arc::new(MissingResolver),
        test_settings(),
    )
}

#[tokio::test]
async fn initialize_error_is_refused_before_tool_dispatch() {
    let home = subc_test_support::TestTempDir::new("initialize-error-frames");
    let events = home.join("frames.jsonl");
    let handler = AdapterHandler::with_resolver(
        registry(
            json!({"fixture":server(json!({"FIXTURE_MODE":{"value":"initialize-error"},
            "FIXTURE_EVENTS_PATH":{"value":events.to_string_lossy()}}))}),
        ),
        Arc::new(MissingResolver),
        test_settings(),
    );
    let (code, _) = refusal(&handler, "fixture", "tools/call").await;
    assert_eq!(code, "initialize_failed");
    assert_eq!(handler.metrics().snapshot()["children_live"], 0);
    let frames: Vec<Value> = std::fs::read_to_string(events)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        frames.len(),
        1,
        "initialize refusal must not write initialized or a tool request"
    );
    assert_eq!(frames[0]["method"], "initialize");
}

#[tokio::test]
async fn tools_list_cache_preserves_cursor_pages_in_both_orders() {
    for second_first in [false, true] {
        let handler = mode_handler("paginated");
        let params = if second_first {
            json!({"cursor":"p2"})
        } else {
            json!({})
        };
        let first = call(&handler, "fixture", "tools/list", params.clone()).await;
        assert_eq!(
            first["payload"]["tools"][0]["name"],
            if second_first { "second" } else { "first" }
        );
        let other_params = if second_first {
            json!({})
        } else {
            json!({"cursor":"p2"})
        };
        let other = call(&handler, "fixture", "tools/list", other_params).await;
        assert_eq!(
            other["payload"]["tools"][0]["name"],
            if second_first { "first" } else { "second" }
        );
        let cached = call(&handler, "fixture", "tools/list", params).await;
        assert_eq!(cached["served_from"], "cache");
        assert_eq!(cached["payload"], first["payload"]);
    }
}

#[tokio::test]
async fn server_request_with_call_id_is_not_a_response() {
    let handler = mode_handler("same-id-ping");
    let result = call(&handler, "fixture", "tools/call", json!({"name":"fixture"})).await;
    assert_eq!(result["payload"]["echo"]["name"], "fixture");
    evict_after_test_ttl(&handler, 1).await;
}

#[tokio::test]
async fn discovery_deadline_is_not_retried_and_reports_child_unresponsive() {
    let handler = AdapterHandler::with_resolver(
        registry(
            json!({"fixture": {"command":fixture_path(), "deadline_ms":200,
            "env":{"FIXTURE_MODE":{"value":"hang"}}}}),
        ),
        Arc::new(MissingResolver),
        test_settings(),
    );
    let (code, _) = refusal(&handler, "fixture", "tools/list").await;
    assert_eq!(code, "child_unresponsive");
    assert_eq!(
        handler.metrics().snapshot()["spawns_total"],
        1,
        "deadline exhaustion cannot buy another full deadline"
    );
}

#[tokio::test]
async fn spawn_elapsed_excludes_slow_tool_execution() {
    let handler = mode_handler("slow");
    let start = std::time::Instant::now();
    let reply = call(&handler, "fixture", "tools/call", json!({})).await;
    let total = start.elapsed().as_millis() as u64;
    let spawn = reply["spawn_elapsed_ms"].as_u64().unwrap();
    assert!(
        total.saturating_sub(spawn) >= 180,
        "tool latency belongs outside spawn cost: total={total}, spawn={spawn}"
    );
    evict_after_test_ttl(&handler, 1).await;
}

#[tokio::test]
async fn early_child_exits_exhaust_spawn_budget() {
    let handler = AdapterHandler::with_resolver(
        registry(json!({"fixture":server(json!({"FIXTURE_MODE":{"value":"early-exit"}}))})),
        Arc::new(MissingResolver),
        LifecycleSettings {
            idle_ttl_override: None,
            ..test_settings()
        },
    );
    for _ in 0..3 {
        assert_eq!(
            refusal(&handler, "fixture", "tools/call").await.0,
            "call_outcome_unknown"
        );
    }
    let (code, detail) = refusal(&handler, "fixture", "tools/call").await;
    assert_eq!(code, "spawn_failed");
    assert_eq!(detail["cause"], "early_exit");
    assert!(detail["retry_after_ms"].as_u64().unwrap() > 0);
    assert_eq!(handler.metrics().snapshot()["spawns_total"], 3);
}

#[tokio::test]
async fn idle_children_are_evicted_at_global_capacity() {
    let servers: serde_json::Map<String, Value> = (0..9)
        .map(|i| (format!("s{i}"), server(json!({}))))
        .collect();
    let handler = AdapterHandler::with_resolver(
        registry(Value::Object(servers)),
        Arc::new(MissingResolver),
        LifecycleSettings {
            idle_ttl_override: None,
            ..test_settings()
        },
    );
    for i in 0..9 {
        call(&handler, &format!("s{i}"), "tools/call", json!({})).await;
        assert!(
            handler.metrics().snapshot()["children_live"]
                .as_u64()
                .unwrap()
                <= 8
        );
    }
    assert_eq!(handler.metrics().snapshot()["idle_evictions_total"], 1);
    assert_eq!(handler.metrics().snapshot()["spawns_total"], 9);
}

#[cfg(unix)]
#[tokio::test]
async fn teardown_kills_grandchild_ignoring_sigterm() {
    let handler = mode_handler("tree");
    let reply = call(&handler, "fixture", "tools/call", json!({})).await;
    let pid =
        rustix::process::Pid::from_raw(reply["payload"]["grandchild_pid"].as_u64().unwrap() as i32)
            .unwrap();
    evict_after_test_ttl(&handler, 1).await;
    let gone = tokio::time::timeout(Duration::from_secs(2), async {
        while rustix::process::test_kill_process(pid).is_ok() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .is_ok();
    // A failed regression must not leave its intentionally uncontained helper behind.
    if !gone {
        let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
    }
    assert!(
        gone,
        "session teardown must kill descendants even when they ignore SIGTERM"
    );
}

#[tokio::test]
async fn busy_children_refuse_at_global_capacity() {
    let servers: serde_json::Map<String, Value> = (0..9)
        .map(|i| {
            (
                format!("s{i}"),
                server(json!({"FIXTURE_MODE":{"value":"hang"}})),
            )
        })
        .collect();
    let handler = Arc::new(AdapterHandler::with_resolver(
        registry(Value::Object(servers)),
        Arc::new(MissingResolver),
        LifecycleSettings {
            idle_ttl_override: None,
            ..test_settings()
        },
    ));
    let token = CancellationToken::new();
    let mut calls = Vec::new();
    for i in 0..8 {
        let handler = Arc::clone(&handler);
        let token = token.clone();
        calls.push(tokio::spawn(async move { handler.route_outcome_with_cancellation(
            &serde_json::to_vec(&json!({"server":format!("s{i}"), "op":"tools/call", "payload":{"method":"tools/call"}})).unwrap(), token,
        ).await }));
    }
    tokio::time::timeout(Duration::from_secs(30), async {
        while handler.metrics().snapshot()["children_live"]
            .as_u64()
            .unwrap()
            < 8
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let ninth_handler = Arc::clone(&handler);
    let ninth_token = token.clone();
    let mut ninth_call = tokio::spawn(async move {
        ninth_handler
            .route_outcome_with_cancellation(
                br#"{"server":"s8","op":"tools/call","payload":{"method":"tools/call"}}"#,
                ninth_token,
            )
            .await
    });
    let ninth = tokio::time::timeout(Duration::from_secs(2), &mut ninth_call).await;
    token.cancel();
    if ninth.is_err() {
        ninth_call.await.unwrap();
    }
    for call in calls {
        call.await.unwrap();
    }
    let outcome = ninth
        .expect("busy capacity must refuse instead of spawning")
        .unwrap();
    let HandlerOutcome::ErrorWithDetail { code, .. } = outcome else {
        panic!("capacity must refuse: {outcome:?}");
    };
    assert_eq!(code, "child_capacity");
    assert_eq!(handler.metrics().snapshot()["children_live"], 0);
    assert_eq!(handler.metrics().snapshot()["spawns_total"], 8);
}
