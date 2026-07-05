#![forbid(unsafe_code)]

use std::{
    collections::BTreeMap,
    fs, io,
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};

use serde_json::{json, Value};
use subc_client_rs::{
    async_trait, serve_with_handle, CallError, CallOptions, CatalogUpdateError, CloseRouteOptions,
    ConsumerOptions, HandlerOutcome, ModuleHandle, ModuleHandler, RequestCtx, RetryBackoff,
    SubcConsumer, SubcModuleError, SubscribeOptions,
};
use subc_control::{ClientControlRequest, ClientControlResponse};
use subc_protocol::{
    manifest::{
        Bindings, Concurrency, ExecutionMode, IdentityBinding, IdentityScope, ModuleManifest,
        ProviderRole, StorageBinding, StorageKind, StorageScope, Tool, TrustTier,
    },
    session::HealthStatus,
    BindIdentity, ErrorBody, Flags, Frame, FrameType, Priority, RouteTarget, PROTOCOL_VERSION,
};
use subc_transport::{authenticate_client, read_frame, write_frame};
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
    time::{sleep, timeout, Instant},
};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

const MODULE_ID: &str = "subc-client-rs-echo";
const READ_TIMEOUT: Duration = Duration::from_secs(3);
const START_TIMEOUT: Duration = Duration::from_secs(10);
const EVENT_TIMEOUT: Duration = Duration::from_secs(5);
const AUTH_DEADLINE: Duration = Duration::from_secs(2);
const CONSUMER_MODULE_A: &str = "subc-client-rs-consumer-a";
const CONSUMER_MODULE_B: &str = "subc-client-rs-consumer-b";

struct LiveDaemon {
    child: Child,
    runtime_dir: PathBuf,
    config_dir: PathBuf,
    connection_file: PathBuf,
}

struct ProviderProcess {
    child: Child,
}

impl Drop for ProviderProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for LiveDaemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.runtime_dir);
        let _ = fs::remove_dir_all(&self.config_dir);
    }
}

impl LiveDaemon {
    fn kill_and_wait(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_file(&self.connection_file);
    }

    fn restart(&mut self, daemon_bin: &Path) {
        self.child = spawn_daemon_child(daemon_bin, &self.runtime_dir, &self.config_dir);
    }
}

struct EchoModuleHandler;

#[async_trait]
impl ModuleHandler for EchoModuleHandler {
    async fn handle(&self, _ctx: RequestCtx, body: Vec<u8>) -> HandlerOutcome {
        HandlerOutcome::Response(body)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clean_subc_client_rs_serves_through_real_daemon() {
    let workspace = workspace_root();
    let daemon_bin = ensure_binary(
        &workspace,
        binary_path(&workspace, "subc-core"),
        &["build", "-p", "subc-core", "--bins"],
    );
    let module_bin = ensure_binary(
        &workspace,
        example_path(&workspace, "echo-module"),
        &["build", "-p", "subc-client-rs", "--example", "echo-module"],
    );

    let temp_dir = unique_temp_dir("subc-client-rs-real-daemon");
    let runtime_dir = temp_dir.join("runtime");
    let config_dir = temp_dir.join("config");
    let events_path = temp_dir.join("events.jsonl");
    fs::create_dir_all(&runtime_dir).unwrap();
    fs::create_dir_all(config_dir.join("cortexkit")).unwrap();
    fs::write(
        config_dir.join("cortexkit").join("subc.jsonc"),
        config_doc(&module_bin, &events_path),
    )
    .unwrap();

    let mut daemon = spawn_daemon(&daemon_bin, &runtime_dir, &config_dir);
    wait_for_connection_file(&daemon.connection_file, START_TIMEOUT).await;
    wait_for_catalog_module(&daemon.connection_file, MODULE_ID, START_TIMEOUT).await;

    let mut client = connect_authed_client(&daemon.connection_file)
        .await
        .unwrap();
    let health = control_rpc_on_stream(
        &mut client,
        99,
        serde_json::to_value(ClientControlRequest::SupervisorHealthProbe {
            module_id: MODULE_ID.to_string(),
        })
        .unwrap(),
    )
    .await;
    let health: ClientControlResponse = serde_json::from_value(health).unwrap();
    match health {
        ClientControlResponse::SupervisorHealthProbe {
            module_id,
            status,
            detail,
            metrics,
        } => {
            assert_eq!(module_id, MODULE_ID);
            assert_eq!(status, HealthStatus::Ok);
            assert_eq!(detail, None);
            assert_eq!(metrics, None);
        }
        other => panic!("unexpected health response: {other:?}"),
    }

    let route_channel = open_route(&mut client, MODULE_ID, 100).await;
    wait_for_event(&events_path, EVENT_TIMEOUT, |event| {
        event["kind"] == "bind" && event["route_channel"].as_u64() == Some(u64::from(route_channel))
    })
    .await;

    write_frame(
        &mut client,
        &data_request(route_channel, 101, br#"{"kind":"unary","value":42}"#),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();
    let unary = read_frame_timeout(&mut client).await;
    assert_eq!(unary.header.ty, FrameType::Response);
    assert_eq!(unary.header.channel, route_channel);
    assert_eq!(unary.header.corr, 101);
    let unary_body: Value = serde_json::from_slice(&unary.body).unwrap();
    assert_eq!(unary_body["ok"], true);
    assert_eq!(unary_body["echo"]["value"], 42);

    write_frame(
        &mut client,
        &data_request(route_channel, 102, br#"{"kind":"error"}"#),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();
    let error = read_frame_timeout(&mut client).await;
    assert_eq!(error.header.ty, FrameType::Error);
    assert_eq!(error.header.channel, route_channel);
    assert_eq!(error.header.corr, 102);
    let error_body: ErrorBody = serde_json::from_slice(&error.body).unwrap();
    assert_eq!(error_body.code, "example_error");

    write_frame(
        &mut client,
        &data_request(route_channel, 103, br#"{"kind":"stream"}"#),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();
    let stream_data = read_frame_timeout(&mut client).await;
    assert_eq!(stream_data.header.ty, FrameType::StreamData);
    assert_eq!(stream_data.header.channel, route_channel);
    assert_eq!(stream_data.header.corr, 103);
    assert_eq!(stream_data.body, b"stream-event");
    let stream_end = read_frame_timeout(&mut client).await;
    assert_eq!(stream_end.header.ty, FrameType::StreamEnd);
    assert_eq!(stream_end.header.channel, route_channel);
    assert_eq!(stream_end.header.corr, 103);

    write_frame(
        &mut client,
        &data_request(route_channel, 104, br#"{"kind":"cancel"}"#),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();
    wait_for_event(&events_path, EVENT_TIMEOUT, |event| {
        event["kind"] == "cancel_waiting" && event["corr"].as_u64() == Some(104)
    })
    .await;
    write_frame(&mut client, &cancel_frame(route_channel, 104))
        .await
        .unwrap();
    client.flush().await.unwrap();
    wait_for_event(&events_path, EVENT_TIMEOUT, |event| {
        event["kind"] == "cancelled" && event["corr"].as_u64() == Some(104)
    })
    .await;
    let cancelled = read_frame_timeout(&mut client).await;
    assert_eq!(cancelled.header.ty, FrameType::Error);
    let cancelled_body: ErrorBody = serde_json::from_slice(&cancelled.body).unwrap();
    assert_eq!(cancelled_body.code, "cancelled");

    write_frame(&mut client, &goodbye_frame(route_channel, 105))
        .await
        .unwrap();
    client.flush().await.unwrap();
    wait_for_event(&events_path, EVENT_TIMEOUT, |event| {
        event["kind"] == "route_gone"
            && event["route_channel"].as_u64() == Some(u64::from(route_channel))
    })
    .await;

    let _ = daemon.child.kill();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn module_handle_catalog_update_refreshes_catalog_without_dropping_open_routes() {
    let workspace = workspace_root();
    let daemon_bin = ensure_binary(
        &workspace,
        binary_path(&workspace, "subc-core"),
        &["build", "-p", "subc-core", "--bins"],
    );

    let temp_dir = unique_temp_dir("subc-client-rs-module-handle-update");
    let runtime_dir = temp_dir.join("runtime");
    let config_dir = temp_dir.join("config");
    fs::create_dir_all(&runtime_dir).unwrap();
    write_empty_config(&config_dir);

    let mut daemon = spawn_daemon(&daemon_bin, &runtime_dir, &config_dir);
    wait_for_connection_file(&daemon.connection_file, START_TIMEOUT).await;
    let module_id = "subc-client-rs-handle-catalog-update";
    let (handle, serve_task) = spawn_inline_module(
        &daemon.connection_file,
        inline_module_manifest(module_id, &["a", "b"]),
    )
    .await;
    wait_for_catalog_module(&daemon.connection_file, module_id, START_TIMEOUT).await;

    let initial_modules = catalog_modules(&daemon.connection_file, Some(module_id), 10_001).await;
    assert_eq!(module_tool_names(&initial_modules[0]), vec!["a", "b"]);

    let mut client = connect_authed_client(&daemon.connection_file)
        .await
        .unwrap();
    let route_channel = open_route(&mut client, module_id, 10_002).await;

    handle
        .catalog_update(vec![tool_provider_role(&["a", "c"])])
        .await
        .unwrap();

    let updated_modules = catalog_modules(&daemon.connection_file, Some(module_id), 10_003).await;
    assert_eq!(module_tool_names(&updated_modules[0]), vec!["a", "c"]);

    write_frame(
        &mut client,
        &data_request(route_channel, 10_004, b"after-update"),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();
    let response = read_frame_timeout(&mut client).await;
    assert_eq!(response.header.ty, FrameType::Response);
    assert_eq!(response.header.channel, route_channel);
    assert_eq!(response.header.corr, 10_004);
    assert_eq!(response.body, b"after-update");

    daemon.kill_and_wait();
    assert!(serve_task.await.unwrap().is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn module_handle_catalog_update_surfaces_frozen_field_rejections() {
    let workspace = workspace_root();
    let daemon_bin = ensure_binary(
        &workspace,
        binary_path(&workspace, "subc-core"),
        &["build", "-p", "subc-core", "--bins"],
    );

    let temp_dir = unique_temp_dir("subc-client-rs-module-handle-frozen");
    let runtime_dir = temp_dir.join("runtime");
    let config_dir = temp_dir.join("config");
    fs::create_dir_all(&runtime_dir).unwrap();
    write_empty_config(&config_dir);

    let mut daemon = spawn_daemon(&daemon_bin, &runtime_dir, &config_dir);
    wait_for_connection_file(&daemon.connection_file, START_TIMEOUT).await;
    let module_id = "subc-client-rs-handle-frozen-field";
    let (handle, serve_task) = spawn_inline_module(
        &daemon.connection_file,
        inline_module_manifest(module_id, &["a"]),
    )
    .await;
    wait_for_catalog_module(&daemon.connection_file, module_id, START_TIMEOUT).await;

    let error = handle.catalog_update(Vec::new()).await.unwrap_err();
    assert!(matches!(error, CatalogUpdateError::FrozenField(_)));

    daemon.kill_and_wait();
    assert!(serve_task.await.unwrap().is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn module_handle_catalog_update_fails_fast_after_connection_death() {
    let workspace = workspace_root();
    let daemon_bin = ensure_binary(
        &workspace,
        binary_path(&workspace, "subc-core"),
        &["build", "-p", "subc-core", "--bins"],
    );

    let temp_dir = unique_temp_dir("subc-client-rs-module-handle-death");
    let runtime_dir = temp_dir.join("runtime");
    let config_dir = temp_dir.join("config");
    fs::create_dir_all(&runtime_dir).unwrap();
    write_empty_config(&config_dir);

    let mut daemon = spawn_daemon(&daemon_bin, &runtime_dir, &config_dir);
    wait_for_connection_file(&daemon.connection_file, START_TIMEOUT).await;
    let module_id = "subc-client-rs-handle-connection-death";
    let (handle, serve_task) = spawn_inline_module(
        &daemon.connection_file,
        inline_module_manifest(module_id, &["a"]),
    )
    .await;
    wait_for_catalog_module(&daemon.connection_file, module_id, START_TIMEOUT).await;

    daemon.kill_and_wait();
    assert!(serve_task.await.unwrap().is_ok());

    let result = timeout(
        Duration::from_secs(1),
        handle.catalog_update(vec![tool_provider_role(&["a"])]),
    )
    .await
    .unwrap();
    assert!(matches!(result, Err(CatalogUpdateError::ConnectionClosed)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subc_consumer_reports_outcome_unknown_mid_call_then_reopens_after_restart() {
    let workspace = workspace_root();
    let daemon_bin = ensure_binary(
        &workspace,
        binary_path(&workspace, "subc-core"),
        &["build", "-p", "subc-core", "--bins"],
    );
    let module_bin = ensure_binary(
        &workspace,
        example_path(&workspace, "echo-module"),
        &["build", "-p", "subc-client-rs", "--example", "echo-module"],
    );
    let temp_dir = unique_temp_dir("subc-client-rs-consumer-midcall");
    let runtime_dir = temp_dir.join("runtime");
    let config_dir = temp_dir.join("config");
    fs::create_dir_all(&runtime_dir).unwrap();
    write_empty_config(&config_dir);

    let mut daemon = spawn_daemon(&daemon_bin, &runtime_dir, &config_dir);
    wait_for_connection_file(&daemon.connection_file, START_TIMEOUT).await;
    let events_path = temp_dir.join("provider-a.jsonl");
    let provider = spawn_provider(
        &module_bin,
        &daemon.connection_file,
        CONSUMER_MODULE_A,
        &events_path,
    );
    wait_for_catalog_module(&daemon.connection_file, CONSUMER_MODULE_A, START_TIMEOUT).await;

    let consumer = Arc::new(
        SubcConsumer::connect(&daemon.connection_file, fast_consumer_options())
            .await
            .unwrap(),
    );
    let identity = consumer_identity("midcall");
    let first = consumer
        .call(
            tool_target(CONSUMER_MODULE_A),
            identity.clone(),
            br#"{"kind":"unary","value":1}"#.to_vec(),
            fast_call_options(),
        )
        .await
        .unwrap();
    assert_eq!(json_body(&first)["echo"]["value"], 1);

    let in_flight = {
        let consumer = Arc::clone(&consumer);
        let identity = identity.clone();
        tokio::spawn(async move {
            consumer
                .call(
                    tool_target(CONSUMER_MODULE_A),
                    identity,
                    br#"{"kind":"sleep","ms":5000}"#.to_vec(),
                    fast_call_options(),
                )
                .await
        })
    };
    wait_for_event(&events_path, EVENT_TIMEOUT, |event| {
        event["kind"] == "sleep_started"
    })
    .await;
    daemon.kill_and_wait();
    let mid_call = in_flight.await.unwrap();
    assert!(
        matches!(mid_call, Err(CallError::OutcomeUnknown(_))),
        "accepted mid-call must surface OutcomeUnknown, got {mid_call:?}"
    );
    assert_eq!(
        read_events(&events_path)
            .into_iter()
            .filter(|event| event["kind"] == "sleep_started")
            .count(),
        1,
        "OutcomeUnknown calls must not be auto-retried"
    );
    drop(provider);

    daemon.restart(&daemon_bin);
    wait_for_connection_file(&daemon.connection_file, START_TIMEOUT).await;
    let events_path_restarted = temp_dir.join("provider-a-restarted.jsonl");
    let _provider = spawn_provider(
        &module_bin,
        &daemon.connection_file,
        CONSUMER_MODULE_A,
        &events_path_restarted,
    );
    wait_for_catalog_module(&daemon.connection_file, CONSUMER_MODULE_A, START_TIMEOUT).await;

    let reopened = consumer
        .call(
            tool_target(CONSUMER_MODULE_A),
            identity,
            br#"{"kind":"unary","value":2}"#.to_vec(),
            fast_call_options(),
        )
        .await
        .unwrap();
    assert_eq!(json_body(&reopened)["echo"]["value"], 2);
    assert!(
        consumer.current_epoch() >= 2,
        "consumer epoch should advance after reconnect"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subc_consumer_retries_unknown_module_until_provider_registers_and_bounds_absence() {
    let workspace = workspace_root();
    let daemon_bin = ensure_binary(
        &workspace,
        binary_path(&workspace, "subc-core"),
        &["build", "-p", "subc-core", "--bins"],
    );
    let module_bin = ensure_binary(
        &workspace,
        example_path(&workspace, "echo-module"),
        &["build", "-p", "subc-client-rs", "--example", "echo-module"],
    );
    let temp_dir = unique_temp_dir("subc-client-rs-consumer-race");
    let runtime_dir = temp_dir.join("runtime");
    let config_dir = temp_dir.join("config");
    fs::create_dir_all(&runtime_dir).unwrap();
    write_empty_config(&config_dir);

    let mut daemon = spawn_daemon(&daemon_bin, &runtime_dir, &config_dir);
    wait_for_connection_file(&daemon.connection_file, START_TIMEOUT).await;
    let initial_events = temp_dir.join("initial-provider.jsonl");
    let initial_provider = spawn_provider(
        &module_bin,
        &daemon.connection_file,
        CONSUMER_MODULE_A,
        &initial_events,
    );
    wait_for_catalog_module(&daemon.connection_file, CONSUMER_MODULE_A, START_TIMEOUT).await;

    let consumer = Arc::new(
        SubcConsumer::connect(&daemon.connection_file, fast_consumer_options())
            .await
            .unwrap(),
    );
    let identity = consumer_identity("race");
    let warm = consumer
        .call(
            tool_target(CONSUMER_MODULE_A),
            identity.clone(),
            br#"{"kind":"unary","value":"warm"}"#.to_vec(),
            fast_call_options(),
        )
        .await
        .unwrap();
    assert_eq!(json_body(&warm)["echo"]["value"], "warm");

    daemon.kill_and_wait();
    drop(initial_provider);
    daemon.restart(&daemon_bin);
    wait_for_connection_file(&daemon.connection_file, START_TIMEOUT).await;

    let racing_call = {
        let consumer = Arc::clone(&consumer);
        let identity = identity.clone();
        tokio::spawn(async move {
            consumer
                .call(
                    tool_target(CONSUMER_MODULE_A),
                    identity,
                    br#"{"kind":"unary","value":"after-register"}"#.to_vec(),
                    fast_call_options(),
                )
                .await
        })
    };
    sleep(Duration::from_millis(250)).await;
    let restarted_events = temp_dir.join("restarted-provider.jsonl");
    let _provider = spawn_provider(
        &module_bin,
        &daemon.connection_file,
        CONSUMER_MODULE_A,
        &restarted_events,
    );
    wait_for_catalog_module(&daemon.connection_file, CONSUMER_MODULE_A, START_TIMEOUT).await;
    let raced = racing_call.await.unwrap().unwrap();
    assert_eq!(json_body(&raced)["echo"]["value"], "after-register");

    let absent = consumer
        .call(
            tool_target("subc-client-rs-never-registers"),
            identity,
            br#"{"kind":"unary","value":"missing"}"#.to_vec(),
            bounded_absence_options(),
        )
        .await;
    assert!(
        matches!(absent, Err(CallError::NotSent(_))),
        "bounded target absence should terminate as NotSent, got {absent:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subc_consumer_multiplexes_targets_and_classifies_reconnect_in_flight() {
    let workspace = workspace_root();
    let daemon_bin = ensure_binary(
        &workspace,
        binary_path(&workspace, "subc-core"),
        &["build", "-p", "subc-core", "--bins"],
    );
    let module_bin = ensure_binary(
        &workspace,
        example_path(&workspace, "echo-module"),
        &["build", "-p", "subc-client-rs", "--example", "echo-module"],
    );
    let temp_dir = unique_temp_dir("subc-client-rs-consumer-mux");
    let runtime_dir = temp_dir.join("runtime");
    let config_dir = temp_dir.join("config");
    fs::create_dir_all(&runtime_dir).unwrap();
    write_empty_config(&config_dir);
    let mut daemon = spawn_daemon(&daemon_bin, &runtime_dir, &config_dir);
    wait_for_connection_file(&daemon.connection_file, START_TIMEOUT).await;
    let events_a = temp_dir.join("a.jsonl");
    let events_b = temp_dir.join("b.jsonl");
    let provider_a = spawn_provider(
        &module_bin,
        &daemon.connection_file,
        CONSUMER_MODULE_A,
        &events_a,
    );
    let provider_b = spawn_provider(
        &module_bin,
        &daemon.connection_file,
        CONSUMER_MODULE_B,
        &events_b,
    );
    wait_for_catalog_module(&daemon.connection_file, CONSUMER_MODULE_A, START_TIMEOUT).await;
    wait_for_catalog_module(&daemon.connection_file, CONSUMER_MODULE_B, START_TIMEOUT).await;

    let consumer = Arc::new(
        SubcConsumer::connect(&daemon.connection_file, fast_consumer_options())
            .await
            .unwrap(),
    );
    let identity = consumer_identity("multiplex");
    let mut handles = Vec::new();
    for i in 0..20u64 {
        let consumer = Arc::clone(&consumer);
        let identity = identity.clone();
        let module_id = if i % 2 == 0 {
            CONSUMER_MODULE_A
        } else {
            CONSUMER_MODULE_B
        };
        handles.push(tokio::spawn(async move {
            let body = serde_json::to_vec(&json!({ "kind": "unary", "value": i })).unwrap();
            let bytes = consumer
                .call(tool_target(module_id), identity, body, fast_call_options())
                .await
                .unwrap();
            (i, json_body(&bytes))
        }));
    }
    for handle in handles {
        let (i, response) = handle.await.unwrap();
        assert_eq!(response["echo"]["value"], i);
    }

    let mut sleepy = Vec::new();
    for module_id in [
        CONSUMER_MODULE_A,
        CONSUMER_MODULE_B,
        CONSUMER_MODULE_A,
        CONSUMER_MODULE_B,
    ] {
        let consumer = Arc::clone(&consumer);
        let identity = identity.clone();
        sleepy.push(tokio::spawn(async move {
            consumer
                .call(
                    tool_target(module_id),
                    identity,
                    br#"{"kind":"sleep","ms":5000}"#.to_vec(),
                    fast_call_options(),
                )
                .await
        }));
    }
    wait_for_event(&events_a, EVENT_TIMEOUT, |event| {
        event["kind"] == "sleep_started"
    })
    .await;
    wait_for_event(&events_b, EVENT_TIMEOUT, |event| {
        event["kind"] == "sleep_started"
    })
    .await;
    daemon.kill_and_wait();
    for handle in sleepy {
        let result = handle.await.unwrap();
        assert!(
            matches!(result, Err(CallError::OutcomeUnknown(_))),
            "accepted multiplexed sleep should be OutcomeUnknown, got {result:?}"
        );
    }
    drop(provider_a);
    drop(provider_b);

    daemon.restart(&daemon_bin);
    wait_for_connection_file(&daemon.connection_file, START_TIMEOUT).await;
    let _provider_a = spawn_provider(
        &module_bin,
        &daemon.connection_file,
        CONSUMER_MODULE_A,
        &temp_dir.join("a-restarted.jsonl"),
    );
    let _provider_b = spawn_provider(
        &module_bin,
        &daemon.connection_file,
        CONSUMER_MODULE_B,
        &temp_dir.join("b-restarted.jsonl"),
    );
    wait_for_catalog_module(&daemon.connection_file, CONSUMER_MODULE_A, START_TIMEOUT).await;
    wait_for_catalog_module(&daemon.connection_file, CONSUMER_MODULE_B, START_TIMEOUT).await;
    let after = consumer
        .call(
            tool_target(CONSUMER_MODULE_B),
            identity,
            br#"{"kind":"unary","value":"after"}"#.to_vec(),
            fast_call_options(),
        )
        .await
        .unwrap();
    assert_eq!(json_body(&after)["echo"]["value"], "after");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subc_consumer_close_route_releases_the_route_and_reopens_fresh() {
    let workspace = workspace_root();
    let daemon_bin = ensure_binary(
        &workspace,
        binary_path(&workspace, "subc-core"),
        &["build", "-p", "subc-core", "--bins"],
    );
    let module_bin = ensure_binary(
        &workspace,
        example_path(&workspace, "echo-module"),
        &["build", "-p", "subc-client-rs", "--example", "echo-module"],
    );
    let temp_dir = unique_temp_dir("subc-client-rs-close-route");
    let runtime_dir = temp_dir.join("runtime");
    let config_dir = temp_dir.join("config");
    fs::create_dir_all(&runtime_dir).unwrap();
    write_empty_config(&config_dir);

    let mut daemon = spawn_daemon(&daemon_bin, &runtime_dir, &config_dir);
    wait_for_connection_file(&daemon.connection_file, START_TIMEOUT).await;
    let events_path = temp_dir.join("provider.jsonl");
    let _provider = spawn_provider(
        &module_bin,
        &daemon.connection_file,
        CONSUMER_MODULE_A,
        &events_path,
    );
    wait_for_catalog_module(&daemon.connection_file, CONSUMER_MODULE_A, START_TIMEOUT).await;

    let consumer = Arc::new(
        SubcConsumer::connect(&daemon.connection_file, fast_consumer_options())
            .await
            .unwrap(),
    );
    let identity = consumer_identity("close-route");

    // Open a route via a call (the echo-module logs a `bind` event with route_channel).
    let warm = consumer
        .call(
            tool_target(CONSUMER_MODULE_A),
            identity.clone(),
            br#"{"kind":"unary","value":"warm"}"#.to_vec(),
            fast_call_options(),
        )
        .await
        .unwrap();
    assert_eq!(json_body(&warm)["echo"]["value"], "warm");
    let bind = wait_for_event(&events_path, EVENT_TIMEOUT, |event| event["kind"] == "bind").await;
    let first_channel = bind["route_channel"].as_u64().unwrap();

    // close_route: the module must observe a route-gone GOODBYE for that channel.
    consumer
        .close_route(
            tool_target(CONSUMER_MODULE_A),
            identity.clone(),
            CloseRouteOptions::default(),
        )
        .await;
    wait_for_event(&events_path, EVENT_TIMEOUT, |event| {
        event["kind"] == "route_gone" && event["route_channel"].as_u64() == Some(first_channel)
    })
    .await;

    // A later call for the SAME key opens a FRESH route (not a tombstone): the module
    // logs a NEW bind on a different route_channel and the call succeeds.
    let after = consumer
        .call(
            tool_target(CONSUMER_MODULE_A),
            identity,
            br#"{"kind":"unary","value":"after"}"#.to_vec(),
            fast_call_options(),
        )
        .await
        .unwrap();
    assert_eq!(json_body(&after)["echo"]["value"], "after");
    wait_for_event(&events_path, EVENT_TIMEOUT, |event| {
        event["kind"] == "bind" && event["route_channel"].as_u64() != Some(first_channel)
    })
    .await;

    // close_route is idempotent: closing an already-closed / never-opened route is a no-op.
    consumer
        .close_route(
            tool_target("subc-client-rs-never-opened"),
            consumer_identity("close-route-absent"),
            CloseRouteOptions::default(),
        )
        .await;

    daemon.kill_and_wait();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subc_consumer_subscribe_streaming_contract() {
    let workspace = workspace_root();
    let daemon_bin = ensure_binary(
        &workspace,
        binary_path(&workspace, "subc-core"),
        &["build", "-p", "subc-core", "--bins"],
    );
    let module_bin = ensure_binary(
        &workspace,
        example_path(&workspace, "echo-module"),
        &["build", "-p", "subc-client-rs", "--example", "echo-module"],
    );
    let temp_dir = unique_temp_dir("subc-client-rs-subscribe");
    let runtime_dir = temp_dir.join("runtime");
    let config_dir = temp_dir.join("config");
    fs::create_dir_all(&runtime_dir).unwrap();
    write_empty_config(&config_dir);

    let mut daemon = spawn_daemon(&daemon_bin, &runtime_dir, &config_dir);
    wait_for_connection_file(&daemon.connection_file, START_TIMEOUT).await;
    let events_path = temp_dir.join("provider.jsonl");
    let provider = spawn_provider(
        &module_bin,
        &daemon.connection_file,
        CONSUMER_MODULE_A,
        &events_path,
    );
    wait_for_catalog_module(&daemon.connection_file, CONSUMER_MODULE_A, START_TIMEOUT).await;

    let consumer = SubcConsumer::connect(&daemon.connection_file, fast_consumer_options())
        .await
        .unwrap();
    let identity = consumer_identity("subscribe");

    let mut stream = consumer
        .subscribe(
            tool_target(CONSUMER_MODULE_A),
            identity.clone(),
            br#"{"kind":"stream_many","count":3}"#.to_vec(),
            fast_subscribe_options(),
        )
        .await
        .unwrap();
    let mut events = Vec::new();
    for _ in 0..3 {
        events.push(
            timeout(EVENT_TIMEOUT, stream.events().recv())
                .await
                .expect("stream event should arrive")
                .expect("stream event channel should remain open"),
        );
    }
    assert_eq!(
        events,
        vec![
            b"stream-event-0".to_vec(),
            b"stream-event-1".to_vec(),
            b"stream-event-2".to_vec(),
        ]
    );
    assert!(
        timeout(EVENT_TIMEOUT, stream.closed())
            .await
            .expect("stream should close")
            .is_ok(),
        "StreamEnd should resolve subscription.closed() successfully"
    );

    let mut error_stream = consumer
        .subscribe(
            tool_target(CONSUMER_MODULE_A),
            identity.clone(),
            br#"{"kind":"error"}"#.to_vec(),
            fast_subscribe_options(),
        )
        .await
        .unwrap();
    match timeout(EVENT_TIMEOUT, error_stream.closed())
        .await
        .expect("error terminal should close")
    {
        Err(CallError::Module(body)) => assert_eq!(body.code, "example_error"),
        other => panic!("Error terminal should reject closed() with ErrorBody, got {other:?}"),
    }

    let mut cancellable = consumer
        .subscribe(
            tool_target(CONSUMER_MODULE_A),
            identity.clone(),
            br#"{"kind":"cancel","tag":"unsubscribe"}"#.to_vec(),
            fast_subscribe_options(),
        )
        .await
        .unwrap();
    wait_for_event(&events_path, EVENT_TIMEOUT, |event| {
        event["kind"] == "cancel_waiting" && event["tag"] == "unsubscribe"
    })
    .await;
    cancellable.unsubscribe();
    assert!(
        timeout(EVENT_TIMEOUT, cancellable.closed())
            .await
            .expect("unsubscribe should settle promptly")
            .is_ok(),
        "unsubscribe should settle closed() as a local clean close"
    );
    wait_for_event(&events_path, EVENT_TIMEOUT, |event| {
        event["kind"] == "cancelled" && event["tag"] == "unsubscribe"
    })
    .await;

    let mut overflow_opts = fast_subscribe_options();
    overflow_opts.event_buffer = 1;
    let mut overflow = consumer
        .subscribe(
            tool_target(CONSUMER_MODULE_A),
            identity.clone(),
            br#"{"kind":"stream_many","count":4}"#.to_vec(),
            overflow_opts,
        )
        .await
        .unwrap();
    let overflow_closed = timeout(EVENT_TIMEOUT, overflow.closed())
        .await
        .expect("overflow should close the subscription");
    assert!(
        matches!(overflow_closed, Err(CallError::SubscriptionBackpressure(_))),
        "full event channel should close with SubscriptionBackpressure, got {overflow_closed:?}"
    );

    let mut route_gone = consumer
        .subscribe(
            tool_target(CONSUMER_MODULE_A),
            identity.clone(),
            br#"{"kind":"cancel","tag":"route-goodbye"}"#.to_vec(),
            fast_subscribe_options(),
        )
        .await
        .unwrap();
    wait_for_event(&events_path, EVENT_TIMEOUT, |event| {
        event["kind"] == "cancel_waiting" && event["tag"] == "route-goodbye"
    })
    .await;
    drop(provider);
    let route_gone_closed = timeout(EVENT_TIMEOUT, route_gone.closed())
        .await
        .expect("provider route teardown should close the subscription");
    assert!(
        matches!(route_gone_closed, Err(CallError::OutcomeUnknown(_))),
        "route GOODBYE should reject closed(), got {route_gone_closed:?}"
    );

    let restarted_events = temp_dir.join("provider-restarted.jsonl");
    let _restarted_provider = spawn_provider(
        &module_bin,
        &daemon.connection_file,
        CONSUMER_MODULE_A,
        &restarted_events,
    );
    wait_for_catalog_module(&daemon.connection_file, CONSUMER_MODULE_A, START_TIMEOUT).await;
    let after = consumer
        .call(
            tool_target(CONSUMER_MODULE_A),
            identity,
            br#"{"kind":"unary","value":"after-route-goodbye"}"#.to_vec(),
            fast_call_options(),
        )
        .await
        .unwrap();
    assert_eq!(json_body(&after)["echo"]["value"], "after-route-goodbye");

    daemon.kill_and_wait();
}

async fn spawn_inline_module(
    connection_file: &Path,
    manifest: ModuleManifest,
) -> (
    ModuleHandle,
    tokio::task::JoinHandle<Result<(), SubcModuleError>>,
) {
    let (handle, serve_future) = serve_with_handle(connection_file, manifest, EchoModuleHandler)
        .await
        .unwrap();
    (handle, tokio::spawn(serve_future))
}

fn inline_module_manifest(module_id: &str, tool_names: &[&str]) -> ModuleManifest {
    ModuleManifest {
        module_id: module_id.to_string(),
        module_version: env!("CARGO_PKG_VERSION").to_string(),
        protocol_ver: PROTOCOL_VERSION,
        trust_tier: TrustTier::FirstParty,
        provides: vec![tool_provider_role(tool_names)],
        consumes: Vec::new(),
        scheduled_tasks: Vec::new(),
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
    }
}

fn tool_provider_role(tool_names: &[&str]) -> ProviderRole {
    ProviderRole::ToolProvider {
        tools: tool_names
            .iter()
            .map(|name| Tool {
                name: (*name).to_string(),
                description: None,
                execution_mode: ExecutionMode::Pure,
                schema: json!({ "type": "object" }),
            })
            .collect(),
        identity_scope: vec![IdentityScope::Project, IdentityScope::Session],
        concurrency: Concurrency::ModuleManaged,
        emits_push: false,
        sub_supervises: false,
    }
}

fn module_tool_names(module: &Value) -> Vec<&str> {
    module["roles"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|role| role["tools"].as_array().unwrap().iter())
        .map(|tool| tool["name"].as_str().unwrap())
        .collect()
}

fn spawn_daemon(daemon_bin: &Path, runtime_dir: &Path, config_dir: &Path) -> LiveDaemon {
    let child = spawn_daemon_child(daemon_bin, runtime_dir, config_dir);
    LiveDaemon {
        child,
        runtime_dir: runtime_dir.to_path_buf(),
        config_dir: config_dir.to_path_buf(),
        connection_file: runtime_dir.join("subc-connection.json"),
    }
}

fn spawn_daemon_child(daemon_bin: &Path, runtime_dir: &Path, config_dir: &Path) -> Child {
    Command::new(daemon_bin)
        .env("XDG_RUNTIME_DIR", runtime_dir)
        .env("XDG_CONFIG_HOME", config_dir)
        .env("SUBC_PORT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("failed to spawn {}: {error}", daemon_bin.display()))
}

fn spawn_provider(
    module_bin: &Path,
    connection_file: &Path,
    module_id: &str,
    events_path: &Path,
) -> ProviderProcess {
    let child = Command::new(module_bin)
        .arg("--subc")
        .arg(connection_file)
        .env(subc_protocol::SUBC_MODULE_ID_ENV, module_id)
        .env("SUBC_MODULE_ECHO_EVENTS", events_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("failed to spawn {}: {error}", module_bin.display()));
    ProviderProcess { child }
}

fn config_doc(module_bin: &Path, events_path: &Path) -> String {
    let env = BTreeMap::from([(
        "SUBC_MODULE_ECHO_EVENTS".to_string(),
        events_path.to_string_lossy().into_owned(),
    )]);
    serde_json::to_string_pretty(&json!({
        "version": 1,
        "modules": {
            MODULE_ID: {
                "program": module_bin.to_string_lossy(),
                "args": [],
                "env": env,
                "enabled": true,
            }
        }
    }))
    .unwrap()
}

fn write_empty_config(config_dir: &Path) {
    fs::create_dir_all(config_dir.join("cortexkit")).unwrap();
    fs::write(
        config_dir.join("cortexkit").join("subc.jsonc"),
        serde_json::to_string_pretty(&json!({ "version": 1, "modules": {} })).unwrap(),
    )
    .unwrap();
}

fn fast_consumer_options() -> ConsumerOptions {
    ConsumerOptions {
        handshake_timeout: AUTH_DEADLINE,
        reconnect_backoff: RetryBackoff {
            base: Duration::from_millis(50),
            cap: Duration::from_millis(250),
            max_attempts: 20,
        },
        restored_debounce: Duration::from_millis(10),
    }
}

fn fast_call_options() -> CallOptions {
    CallOptions {
        timeout: Duration::from_secs(8),
        route_retry: RetryBackoff {
            base: Duration::from_millis(50),
            cap: Duration::from_millis(250),
            max_attempts: 20,
        },
        route_retry_deadline: Duration::from_secs(5),
        ..CallOptions::default()
    }
}

fn bounded_absence_options() -> CallOptions {
    CallOptions {
        timeout: Duration::from_secs(2),
        route_retry: RetryBackoff {
            base: Duration::from_millis(25),
            cap: Duration::from_millis(50),
            max_attempts: 4,
        },
        route_retry_deadline: Duration::from_millis(300),
        ..CallOptions::default()
    }
}

fn fast_subscribe_options() -> SubscribeOptions {
    SubscribeOptions {
        route_retry: RetryBackoff {
            base: Duration::from_millis(50),
            cap: Duration::from_millis(250),
            max_attempts: 20,
        },
        route_retry_deadline: Duration::from_secs(5),
        route_open_timeout: Duration::from_secs(8),
        ..SubscribeOptions::default()
    }
}

fn consumer_identity(session: &str) -> BindIdentity {
    let project_root = unique_temp_dir("subc-client-rs-consumer-project");
    fs::create_dir_all(&project_root).unwrap();
    BindIdentity {
        project_root,
        harness: "subc-client-rs-consumer-test".to_string(),
        session: session.to_string(),
    }
}

fn tool_target(module_id: &str) -> RouteTarget {
    RouteTarget::ToolProvider {
        module_id: module_id.to_string(),
    }
}

fn json_body(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).unwrap()
}

async fn wait_for_connection_file(path: &Path, wait: Duration) {
    let deadline = Instant::now() + wait;
    loop {
        if path.exists() {
            return;
        }
        if Instant::now() >= deadline {
            panic!(
                "daemon did not write connection file {} within {wait:?}",
                path.display()
            );
        }
        sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_for_catalog_module(path: &Path, module_id: &str, wait: Duration) {
    let deadline = Instant::now() + wait;
    let mut corr = 1_000;
    loop {
        if catalog_modules(path, Some(module_id), corr).await.len() == 1 {
            return;
        }
        if Instant::now() >= deadline {
            panic!("module {module_id} did not register in catalog within {wait:?}");
        }
        corr += 1;
        sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_for_event(path: &Path, wait: Duration, predicate: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + wait;
    loop {
        for event in read_events(path) {
            if predicate(&event) {
                return event;
            }
        }
        if Instant::now() >= deadline {
            panic!(
                "event did not appear within {wait:?}; events: {:?}",
                read_events(path)
            );
        }
        sleep(Duration::from_millis(20)).await;
    }
}

fn read_events(path: &Path) -> Vec<Value> {
    let Ok(raw) = fs::read_to_string(path) else {
        return Vec::new();
    };
    raw.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect()
}

async fn catalog_modules(path: &Path, module_id: Option<&str>, corr: u64) -> Vec<Value> {
    let mut client = connect_authed_client(path).await.unwrap();
    let response = control_rpc_on_stream(
        &mut client,
        corr,
        json!({
            "op": "catalog.list",
            "module_id": module_id,
        }),
    )
    .await;
    assert_eq!(response["op"], "catalog.list");
    response["modules"].as_array().cloned().unwrap_or_default()
}

async fn open_route<S>(stream: &mut S, module_id: &str, corr: u64) -> u16
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let project_root = unique_temp_dir("subc-client-rs-project");
    fs::create_dir_all(&project_root).unwrap();
    let response = control_rpc_on_stream(
        stream,
        corr,
        json!({
            "op": "route.open",
            "target": RouteTarget::ToolProvider {
                module_id: module_id.to_string(),
            },
            "identity": BindIdentity {
                project_root,
                harness: "subc-client-rs-test".to_string(),
                session: "clean-api".to_string(),
            },
        }),
    )
    .await;
    assert_eq!(response["op"], "route.open");
    response["route_channel"]
        .as_u64()
        .expect("route.open must return route_channel") as u16
}

async fn control_rpc_on_stream<S>(stream: &mut S, corr: u64, request: Value) -> Value
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let body = serde_json::to_vec(&request).unwrap();
    write_frame(stream, &control_request_frame(corr, body))
        .await
        .unwrap();
    stream.flush().await.unwrap();
    let response = read_frame_timeout(stream).await;
    assert_eq!(response.header.ty, FrameType::Response);
    assert_eq!(response.header.channel, 0);
    assert_eq!(response.header.corr, corr);
    serde_json::from_slice(&response.body).unwrap()
}

async fn connect_authed_client(path: &Path) -> io::Result<TcpStream> {
    let conn = subc_transport::read(path).map_err(io::Error::other)?;
    let endpoint = conn.endpoints.first().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "connection file has no endpoint",
        )
    })?;
    let ip: IpAddr = endpoint
        .host
        .parse()
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    let mut stream = TcpStream::connect(SocketAddr::new(ip, endpoint.port)).await?;
    authenticate_client(&mut stream, &conn, AUTH_DEADLINE)
        .await
        .map_err(io::Error::other)?;
    Ok(stream)
}

async fn read_frame_timeout<S>(stream: &mut S) -> Frame
where
    S: AsyncRead + Unpin,
{
    timeout(READ_TIMEOUT, read_frame(stream))
        .await
        .expect("timed out reading frame")
        .expect("frame read failed")
        .expect("connection closed")
}

fn control_request_frame(corr: u64, body: Vec<u8>) -> Frame {
    Frame::build(
        FrameType::Request,
        Flags::new(false, Priority::Interactive, false),
        0,
        corr,
        body,
    )
    .unwrap()
}

fn data_request(channel: u16, corr: u64, body: &[u8]) -> Frame {
    Frame::build(
        FrameType::Request,
        Flags::new(false, Priority::Interactive, false),
        channel,
        corr,
        body.to_vec(),
    )
    .unwrap()
}

fn cancel_frame(channel: u16, corr: u64) -> Frame {
    Frame::build(
        FrameType::Cancel,
        Flags::new(false, Priority::Interactive, false),
        channel,
        corr,
        Vec::new(),
    )
    .unwrap()
}

fn goodbye_frame(channel: u16, corr: u64) -> Frame {
    Frame::build(
        FrameType::Goodbye,
        Flags::new(false, Priority::Interactive, false),
        channel,
        corr,
        Vec::new(),
    )
    .unwrap()
}

fn ensure_binary(workspace: &Path, path: PathBuf, cargo_args: &[&str]) -> PathBuf {
    static BUILD_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let _guard = BUILD_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let output = Command::new("cargo")
        .args(cargo_args)
        .current_dir(workspace)
        .output()
        .unwrap_or_else(|error| panic!("failed to run cargo {cargo_args:?}: {error}"));
    if !output.status.success() {
        panic!(
            "cargo {cargo_args:?} failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert!(path.exists(), "expected binary at {}", path.display());
    path
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .unwrap()
        .to_path_buf()
}

fn binary_path(workspace: &Path, name: &str) -> PathBuf {
    workspace
        .join("target")
        .join("debug")
        .join(format!("{name}{}", std::env::consts::EXE_SUFFIX))
}

fn example_path(workspace: &Path, name: &str) -> PathBuf {
    workspace
        .join("target")
        .join("debug")
        .join("examples")
        .join(format!("{name}{}", std::env::consts::EXE_SUFFIX))
}

fn unique_temp_dir(name: &str) -> PathBuf {
    let nonce = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("{name}-{}-{nonce}", std::process::id()))
}
