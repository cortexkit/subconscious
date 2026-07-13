use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::{self, Command},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use serde_json::{json, Value};
use subc_control::{
    ClientControlRequest, ClientControlResponse, ConsumerIdentity, SupervisorEntry,
    SupervisorRescanResult,
};
use subc_core::{
    bootstrap::{run_with_config, run_with_daemon_config_path, BootstrapConfig},
    read_frame, write_frame, Frame,
};
use subc_protocol::{BindIdentity, ErrorBody, Flags, FrameType, Priority, RouteTarget};
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
    task::JoinHandle,
    time::{sleep, timeout, Instant},
};

mod common;
use common::connect_authed_client;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
const READ_TIMEOUT: Duration = Duration::from_secs(2);
const START_TIMEOUT: Duration = Duration::from_secs(10);
const STATE_TIMEOUT: Duration = Duration::from_secs(10);

struct RunningDaemon {
    connection_file_path: PathBuf,
    config_path: PathBuf,
    temp_dir: PathBuf,
    task: JoinHandle<Result<(), subc_core::bootstrap::BootstrapError>>,
}

impl RunningDaemon {
    async fn start(name: &str, config_doc: Option<String>) -> Self {
        let temp_dir = unique_temp_dir(name);
        fs::create_dir_all(&temp_dir).unwrap();
        let connection_file_path = temp_dir.join("subc-conn.json");
        let config_path = temp_dir.join("config").join("cortexkit").join("subc.jsonc");
        if let Some(config_doc) = config_doc {
            fs::create_dir_all(config_path.parent().unwrap()).unwrap();
            fs::write(&config_path, config_doc).unwrap();
        }

        let config = BootstrapConfig::new(&connection_file_path, 0)
            .with_daemon_config_path(&config_path)
            .unwrap();
        let task = tokio::spawn(run_with_config(config));
        let mut client = wait_for_client(&connection_file_path, START_TIMEOUT).await;
        let _ =
            control_rpc_on_stream(&mut client, 1, ClientControlRequest::ServerDescribe {}).await;

        Self {
            connection_file_path,
            config_path,
            temp_dir,
            task,
        }
    }
}

impl Drop for RunningDaemon {
    fn drop(&mut self) {
        self.task.abort();
        let _ = fs::remove_dir_all(&self.temp_dir);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn config_spawns_supervises_registers_and_routes_stub_module() {
    let module_id = "daemon-aft";
    let daemon = RunningDaemon::start(
        "daemon-config-spawn",
        Some(config_doc([stub_module(module_id, true, [])])),
    )
    .await;

    let entry = wait_for_supervisor_entry(
        &daemon.connection_file_path,
        module_id,
        |entry| entry.state == "running" && entry.enabled && entry.live,
        STATE_TIMEOUT,
    )
    .await;
    assert_eq!(entry.module_id, module_id);
    wait_for_catalog_module(&daemon.connection_file_path, module_id, STATE_TIMEOUT).await;

    let mut client = wait_for_client(&daemon.connection_file_path, START_TIMEOUT).await;
    let route_channel = open_route(&mut client, module_id, 100).await;
    write_frame(
        &mut client,
        &data_request(
            route_channel,
            101,
            br#"{ "name": "echo", "arguments": { "value": 1 } }"#,
        ),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();

    let response = read_frame_timeout(&mut client).await;
    assert_eq!(response.header.ty, FrameType::Response);
    assert_eq!(response.header.channel, route_channel);
    assert_eq!(response.header.corr, 101);
    let body: Value = serde_json::from_slice(&response.body).unwrap();
    let text = body["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("fake-aft tool echo called"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabled_configured_module_is_listed_then_enable_spawns_and_registers() {
    let module_id = "disabled-aft";
    let daemon = RunningDaemon::start(
        "daemon-config-disabled",
        Some(config_doc([stub_module(module_id, false, [])])),
    )
    .await;

    let entry = wait_for_supervisor_entry(
        &daemon.connection_file_path,
        module_id,
        |entry| entry.state == "disabled" && !entry.enabled && !entry.live,
        STATE_TIMEOUT,
    )
    .await;
    assert_eq!(entry.module_id, module_id);
    assert_catalog_modules(&daemon.connection_file_path, Some(module_id), 10, 0).await;

    let mut client = wait_for_client(&daemon.connection_file_path, START_TIMEOUT).await;
    match control_rpc_on_stream(
        &mut client,
        11,
        ClientControlRequest::SupervisorSetEnabled {
            module_id: module_id.to_string(),
            enabled: true,
        },
    )
    .await
    {
        ClientControlResponse::SupervisorAck {
            module_id: actual,
            applied,
        } => {
            assert_eq!(actual, module_id);
            assert!(applied);
        }
        other => panic!("unexpected supervisor.set_enabled response: {other:?}"),
    }

    wait_for_supervisor_entry(
        &daemon.connection_file_path,
        module_id,
        |entry| entry.state == "running" && entry.enabled && entry.live,
        STATE_TIMEOUT,
    )
    .await;
    wait_for_catalog_module(&daemon.connection_file_path, module_id, STATE_TIMEOUT).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_spawn_is_visible_and_good_modules_still_start() {
    let good_module_id = "good-aft";
    let bad_module_id = "missing-aft";
    let missing_program = unique_temp_dir("missing-program").join("definitely-not-a-module");
    let daemon = RunningDaemon::start(
        "daemon-config-failed-spawn",
        Some(config_doc([
            stub_module(good_module_id, true, []),
            module_doc(bad_module_id, &missing_program, true, BTreeMap::new()),
        ])),
    )
    .await;

    wait_for_supervisor_entry(
        &daemon.connection_file_path,
        bad_module_id,
        |entry| entry.state == "failed" && entry.enabled && !entry.live,
        STATE_TIMEOUT,
    )
    .await;
    wait_for_supervisor_entry(
        &daemon.connection_file_path,
        good_module_id,
        |entry| entry.state == "running" && entry.enabled && entry.live,
        STATE_TIMEOUT,
    )
    .await;

    let mut client = wait_for_client(&daemon.connection_file_path, START_TIMEOUT).await;
    let route_channel = open_route(&mut client, good_module_id, 20).await;
    write_frame(&mut client, &data_request(route_channel, 21, b"ping"))
        .await
        .unwrap();
    client.flush().await.unwrap();
    let response = read_frame_timeout(&mut client).await;
    assert_eq!(response.header.ty, FrameType::Response);
    assert_eq!(response.header.channel, route_channel);
    assert_eq!(response.header.corr, 21);
    assert_eq!(response.body, b"ping");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rescan_adds_reserved_module_and_attests_its_launch_nonce() {
    let existing_id = "rescan-existing";
    let added_id = "rescan-added";
    let existing = stub_module(existing_id, true, []);
    let daemon =
        RunningDaemon::start("daemon-rescan-add", Some(config_doc([existing.clone()]))).await;
    wait_for_catalog_module(&daemon.connection_file_path, existing_id, STATE_TIMEOUT).await;

    let mut added = stub_module(added_id, true, []);
    added["reserved"] = json!(true);
    fs::write(&daemon.config_path, config_doc([existing, added])).unwrap();
    let result = supervisor_rescan(&daemon.connection_file_path, 300).await;
    assert_eq!(result.added, [added_id]);
    assert!(result.removed.is_empty());
    assert!(result.changed_pending_reload.is_empty());
    assert_eq!(result.unchanged, 1);

    wait_for_catalog_module(&daemon.connection_file_path, added_id, STATE_TIMEOUT).await;
    let mut added_client = wait_for_client(&daemon.connection_file_path, START_TIMEOUT).await;
    let added_route = open_route(&mut added_client, added_id, 301).await;
    let nonce = call_tool(&mut added_client, added_route, 302, "_test.launch_nonce").await;
    assert!(
        !nonce.is_empty(),
        "rescan-added module must receive a launch nonce"
    );

    let mut consumer_client = wait_for_client(&daemon.connection_file_path, START_TIMEOUT).await;
    let attested_route = open_route_with_consumer_identity(
        &mut consumer_client,
        existing_id,
        303,
        ConsumerIdentity {
            module_id: added_id.to_string(),
            launch_nonce: nonce,
        },
    )
    .await
    .expect("launch nonce from rescan-added module should attest its consumer identity");
    assert!(attested_route > 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rescan_removes_module_and_leaves_other_open_route_undisturbed() {
    let kept_id = "rescan-kept";
    let removed_id = "rescan-removed";
    let kept = stub_module(kept_id, true, []);
    let removed = stub_module(removed_id, true, []);
    let daemon = RunningDaemon::start(
        "daemon-rescan-remove",
        Some(config_doc([kept.clone(), removed])),
    )
    .await;
    wait_for_catalog_module(&daemon.connection_file_path, kept_id, STATE_TIMEOUT).await;
    wait_for_catalog_module(&daemon.connection_file_path, removed_id, STATE_TIMEOUT).await;

    let mut kept_client = wait_for_client(&daemon.connection_file_path, START_TIMEOUT).await;
    let kept_route = open_route(&mut kept_client, kept_id, 400).await;
    let mut removed_client = wait_for_client(&daemon.connection_file_path, START_TIMEOUT).await;
    let removed_route = open_route(&mut removed_client, removed_id, 401).await;

    fs::write(&daemon.config_path, config_doc([kept])).unwrap();
    let result = supervisor_rescan(&daemon.connection_file_path, 402).await;
    assert_eq!(result.removed, [removed_id]);
    assert_eq!(result.unchanged, 1);
    wait_for_supervisor_absent(&daemon.connection_file_path, removed_id, STATE_TIMEOUT).await;
    wait_for_catalog_absent(&daemon.connection_file_path, removed_id, STATE_TIMEOUT).await;

    let goodbye = read_frame_timeout(&mut removed_client).await;
    assert_eq!(goodbye.header.ty, FrameType::Goodbye);
    assert_eq!(goodbye.header.channel, removed_route);

    write_frame(
        &mut kept_client,
        &data_request(kept_route, 403, b"still-open"),
    )
    .await
    .unwrap();
    kept_client.flush().await.unwrap();
    let response = read_frame_timeout(&mut kept_client).await;
    assert_eq!(response.header.ty, FrameType::Response);
    assert_eq!(response.body, b"still-open");

    let mut unknown_client = wait_for_client(&daemon.connection_file_path, START_TIMEOUT).await;
    let error = open_route_with_identity(&mut unknown_client, removed_id, 404, None)
        .await
        .unwrap_err();
    assert_eq!(error.code, "unknown_module");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rescan_changed_spec_is_pending_until_reload_uses_it() {
    let module_id = "rescan-changed";
    let original = stub_module(
        module_id,
        true,
        [("FAKE_AFT_TOOLCALL_RESULT", "before-rescan")],
    );
    let daemon = RunningDaemon::start("daemon-rescan-changed", Some(config_doc([original]))).await;
    wait_for_catalog_module(&daemon.connection_file_path, module_id, STATE_TIMEOUT).await;
    let mut old_client = wait_for_client(&daemon.connection_file_path, START_TIMEOUT).await;
    let old_route = open_route(&mut old_client, module_id, 501).await;
    let before_pid = call_tool(&mut old_client, old_route, 502, "_test.pid").await;

    let changed = stub_module(
        module_id,
        true,
        [("FAKE_AFT_TOOLCALL_RESULT", "after-reload")],
    );
    fs::write(&daemon.config_path, config_doc([changed])).unwrap();
    let result = supervisor_rescan(&daemon.connection_file_path, 503).await;
    assert_eq!(result.changed_pending_reload, [module_id]);
    assert_eq!(
        call_tool(&mut old_client, old_route, 504, "_test.pid").await,
        before_pid
    );
    assert_eq!(
        call_tool(&mut old_client, old_route, 505, "echo").await,
        "before-rescan"
    );

    let mut control = wait_for_client(&daemon.connection_file_path, START_TIMEOUT).await;
    let response = control_rpc_on_stream(
        &mut control,
        506,
        ClientControlRequest::SupervisorReload {
            module_id: module_id.to_string(),
        },
    )
    .await;
    assert!(matches!(
        response,
        ClientControlResponse::SupervisorAck { applied: true, .. }
    ));

    let mut new_client = wait_for_client(&daemon.connection_file_path, START_TIMEOUT).await;
    let new_route = open_route(&mut new_client, module_id, 507).await;
    assert_ne!(
        call_tool(&mut new_client, new_route, 508, "_test.pid").await,
        before_pid
    );
    assert_eq!(
        call_tool(&mut new_client, new_route, 509, "echo").await,
        "after-reload"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rescan_add_does_not_interrupt_existing_in_flight_request() {
    let existing_id = "rescan-in-flight";
    let added_id = "rescan-in-flight-added";
    let existing = stub_module(existing_id, true, [("FAKE_AFT_TOOLCALL_DELAY_MS", "250")]);
    let daemon = RunningDaemon::start(
        "daemon-rescan-in-flight",
        Some(config_doc([existing.clone()])),
    )
    .await;
    wait_for_catalog_module(&daemon.connection_file_path, existing_id, STATE_TIMEOUT).await;

    let mut existing_client = wait_for_client(&daemon.connection_file_path, START_TIMEOUT).await;
    let existing_route = open_route(&mut existing_client, existing_id, 600).await;
    write_frame(
        &mut existing_client,
        &data_request(
            existing_route,
            601,
            br#"{"name":"echo","arguments":{"value":"in-flight"}}"#,
        ),
    )
    .await
    .unwrap();
    existing_client.flush().await.unwrap();

    fs::write(
        &daemon.config_path,
        config_doc([existing, stub_module(added_id, true, [])]),
    )
    .unwrap();
    let result = supervisor_rescan(&daemon.connection_file_path, 602).await;
    assert_eq!(result.added, [added_id]);
    assert_eq!(result.unchanged, 1);

    let response = read_frame_timeout(&mut existing_client).await;
    assert_eq!(response.header.ty, FrameType::Response);
    assert_eq!(response.header.channel, existing_route);
    assert_eq!(response.header.corr, 601);
    let body: Value = serde_json::from_slice(&response.body).unwrap();
    assert!(body["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("in-flight"));

    assert_eq!(
        call_tool(&mut existing_client, existing_route, 603, "echo").await,
        "fake-aft tool echo called with {}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rescan_invalid_config_is_rejected_without_mutating_modules() {
    let module_id = "rescan-fail-loud";
    let original = stub_module(module_id, true, []);
    let daemon = RunningDaemon::start(
        "daemon-rescan-fail-loud",
        Some(config_doc([original.clone()])),
    )
    .await;
    wait_for_catalog_module(&daemon.connection_file_path, module_id, STATE_TIMEOUT).await;
    let mut module_client = wait_for_client(&daemon.connection_file_path, START_TIMEOUT).await;
    let module_route = open_route(&mut module_client, module_id, 700).await;
    let before_pid = call_tool(&mut module_client, module_route, 701, "_test.pid").await;

    fs::write(&daemon.config_path, "{ invalid jsonc").unwrap();
    let corrupt_error = supervisor_rescan_error(&daemon.connection_file_path, 702).await;
    assert_eq!(corrupt_error.code, "invalid_daemon_config");
    let after_corrupt = supervisor_modules(&daemon.connection_file_path, 703).await;
    assert_eq!(after_corrupt.len(), 1);
    assert_eq!(
        call_tool(&mut module_client, module_route, 704, "_test.pid").await,
        before_pid
    );

    let mut owner = original;
    owner["reserved"] = json!(true);
    owner["reserved_prefixes"] = json!(["owned:"]);
    let overlapping = stub_module("owned:child", true, []);
    fs::write(&daemon.config_path, config_doc([owner, overlapping])).unwrap();
    let overlap_error = supervisor_rescan_error(&daemon.connection_file_path, 705).await;
    assert_eq!(overlap_error.code, "invalid_daemon_config");
    let after_overlap = supervisor_modules(&daemon.connection_file_path, 706).await;
    assert_eq!(after_overlap.len(), 1);
    assert_eq!(after_overlap[0].module_id, module_id);
    assert_eq!(
        call_tool(&mut module_client, module_route, 707, "_test.pid").await,
        before_pid
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rescan_applies_enabled_flips_without_daemon_restart() {
    let module_id = "rescan-enabled";
    let daemon = RunningDaemon::start(
        "daemon-rescan-enabled",
        Some(config_doc([stub_module(module_id, true, [])])),
    )
    .await;
    wait_for_catalog_module(&daemon.connection_file_path, module_id, STATE_TIMEOUT).await;
    let mut before_client = wait_for_client(&daemon.connection_file_path, START_TIMEOUT).await;
    let before_route = open_route(&mut before_client, module_id, 800).await;
    let before_pid = call_tool(&mut before_client, before_route, 801, "_test.pid").await;

    fs::write(
        &daemon.config_path,
        config_doc([stub_module(module_id, false, [])]),
    )
    .unwrap();
    supervisor_rescan(&daemon.connection_file_path, 802).await;
    wait_for_supervisor_entry(
        &daemon.connection_file_path,
        module_id,
        |entry| entry.state == "disabled" && !entry.enabled && !entry.live,
        STATE_TIMEOUT,
    )
    .await;
    wait_for_catalog_absent(&daemon.connection_file_path, module_id, STATE_TIMEOUT).await;

    fs::write(
        &daemon.config_path,
        config_doc([stub_module(module_id, true, [])]),
    )
    .unwrap();
    supervisor_rescan(&daemon.connection_file_path, 803).await;
    wait_for_catalog_module(&daemon.connection_file_path, module_id, STATE_TIMEOUT).await;
    let mut enabled_client = wait_for_client(&daemon.connection_file_path, START_TIMEOUT).await;
    let enabled_route = open_route(&mut enabled_client, module_id, 804).await;
    assert_ne!(
        call_tool(&mut enabled_client, enabled_route, 805, "_test.pid").await,
        before_pid
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_rescans_queue_and_reconcile_once() {
    let existing_id = "rescan-concurrent-existing";
    let added_id = "rescan-concurrent-added";
    let existing = stub_module(existing_id, true, []);
    let daemon = RunningDaemon::start(
        "daemon-rescan-concurrent",
        Some(config_doc([existing.clone()])),
    )
    .await;
    wait_for_catalog_module(&daemon.connection_file_path, existing_id, STATE_TIMEOUT).await;
    fs::write(
        &daemon.config_path,
        config_doc([existing, stub_module(added_id, true, [])]),
    )
    .unwrap();

    let (first, second) = tokio::join!(
        supervisor_rescan(&daemon.connection_file_path, 900),
        supervisor_rescan(&daemon.connection_file_path, 901),
    );
    let reports = [first, second];
    assert_eq!(
        reports
            .iter()
            .filter(|report| report.added == [added_id])
            .count(),
        1
    );
    assert_eq!(
        reports
            .iter()
            .filter(|report| report.added.is_empty() && report.unchanged == 2)
            .count(),
        1
    );
    wait_for_catalog_module(&daemon.connection_file_path, added_id, STATE_TIMEOUT).await;
    let modules = supervisor_modules(&daemon.connection_file_path, 902).await;
    assert_eq!(modules.len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ck_module_rescan_prints_reconcile_table() {
    let existing_id = "ck-rescan-existing";
    let added_id = "ck-rescan-added";
    let existing = stub_module(existing_id, true, []);
    let daemon =
        RunningDaemon::start("daemon-ck-rescan", Some(config_doc([existing.clone()]))).await;
    wait_for_catalog_module(&daemon.connection_file_path, existing_id, STATE_TIMEOUT).await;
    fs::write(
        &daemon.config_path,
        config_doc([existing, stub_module(added_id, true, [])]),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_ck"))
        .args(["module", "rescan", "--subc"])
        .arg(&daemon.connection_file_path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "ck stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("change"));
    assert!(stdout.contains("changed-pending-reload"));
    assert!(stdout.contains(added_id));
    assert!(stdout.contains("unchanged"));
    wait_for_catalog_module(&daemon.connection_file_path, added_id, STATE_TIMEOUT).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn absent_config_starts_bare_daemon_with_no_supervised_modules() {
    let daemon = RunningDaemon::start("daemon-config-absent", None).await;
    assert!(!daemon.config_path.exists());
    let modules = supervisor_modules(&daemon.connection_file_path, 30).await;
    assert!(modules.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn present_invalid_config_fails_loud_before_daemon_starts() {
    let temp_dir = unique_temp_dir("daemon-config-invalid");
    fs::create_dir_all(&temp_dir).unwrap();
    let connection_file_path = temp_dir.join("subc-conn.json");
    let config_path = temp_dir.join("subc.jsonc");
    fs::write(
        &config_path,
        r#"{ "version": 1, "modules": { "aft": "wrong" } }"#,
    )
    .unwrap();

    let err =
        run_with_daemon_config_path(BootstrapConfig::new(&connection_file_path, 0), &config_path)
            .await
            .unwrap_err();
    let message = err.to_string();
    assert!(message.contains(&config_path.display().to_string()));
    assert!(message.contains("invalid daemon config"));
    assert!(!connection_file_path.exists());

    let _ = fs::remove_dir_all(temp_dir);
}

fn config_doc<const N: usize>(modules: [Value; N]) -> String {
    let mut module_map = serde_json::Map::new();
    for module in modules {
        let module_id = module["module_id"].as_str().unwrap().to_string();
        let mut module = module.as_object().unwrap().clone();
        module.remove("module_id");
        module_map.insert(module_id, Value::Object(module));
    }
    serde_json::to_string_pretty(&json!({
        "version": 1,
        "modules": module_map,
    }))
    .unwrap()
}

fn stub_module<const N: usize>(
    module_id: &str,
    enabled: bool,
    extra_env: [(&str, &str); N],
) -> Value {
    let mut env = BTreeMap::from([("FAKE_AFT_MODULE_ID".to_string(), module_id.to_string())]);
    for (key, value) in extra_env {
        env.insert(key.to_string(), value.to_string());
    }
    module_doc(
        module_id,
        Path::new(env!("CARGO_BIN_EXE_fake-aft-stub")),
        enabled,
        env,
    )
}

fn module_doc(
    module_id: &str,
    program: &Path,
    enabled: bool,
    env: BTreeMap<String, String>,
) -> Value {
    json!({
        "module_id": module_id,
        "program": program.to_string_lossy(),
        "args": [],
        "env": env,
        "enabled": enabled,
    })
}

async fn wait_for_client(path: &Path, wait: Duration) -> TcpStream {
    let deadline = Instant::now() + wait;
    loop {
        match connect_authed_client(path).await {
            Ok(client) => return client,
            Err(err) if Instant::now() < deadline => {
                let _ = err;
                sleep(Duration::from_millis(20)).await;
            }
            Err(err) => panic!("daemon did not accept authenticated client within {wait:?}: {err}"),
        }
    }
}

async fn wait_for_supervisor_entry(
    path: &Path,
    module_id: &str,
    predicate: impl Fn(&SupervisorEntry) -> bool,
    wait: Duration,
) -> SupervisorEntry {
    let deadline = Instant::now() + wait;
    let mut corr = 1_000;
    loop {
        let modules = supervisor_modules(path, corr).await;
        if let Some(entry) = modules
            .into_iter()
            .find(|entry| entry.module_id == module_id && predicate(entry))
        {
            return entry;
        }
        if Instant::now() >= deadline {
            let modules = supervisor_modules(path, corr + 10_000).await;
            panic!("module {module_id} did not reach expected supervisor state within {wait:?}; modules: {modules:?}");
        }
        corr += 1;
        sleep(Duration::from_millis(20)).await;
    }
}

async fn supervisor_modules(path: &Path, corr: u64) -> Vec<SupervisorEntry> {
    let mut client = wait_for_client(path, START_TIMEOUT).await;
    match control_rpc_on_stream(&mut client, corr, ClientControlRequest::SupervisorList {}).await {
        ClientControlResponse::SupervisorList { modules, .. } => modules,
        other => panic!("unexpected supervisor.list response: {other:?}"),
    }
}

async fn wait_for_catalog_module(path: &Path, module_id: &str, wait: Duration) {
    let deadline = Instant::now() + wait;
    let mut corr = 2_000;
    loop {
        if catalog_modules(path, Some(module_id), corr).await.len() == 1 {
            return;
        }
        if Instant::now() >= deadline {
            panic!("module {module_id} did not register in catalog within {wait:?}");
        }
        corr += 1;
        sleep(Duration::from_millis(20)).await;
    }
}

async fn assert_catalog_modules(path: &Path, module_id: Option<&str>, corr: u64, expected: usize) {
    let modules = catalog_modules(path, module_id, corr).await;
    assert_eq!(modules.len(), expected, "catalog modules: {modules:?}");
}

async fn catalog_modules(
    path: &Path,
    module_id: Option<&str>,
    corr: u64,
) -> Vec<subc_control::CatalogEntry> {
    let mut client = wait_for_client(path, START_TIMEOUT).await;
    match control_rpc_on_stream(
        &mut client,
        corr,
        ClientControlRequest::CatalogList {
            module_id: module_id.map(ToOwned::to_owned),
        },
    )
    .await
    {
        ClientControlResponse::CatalogList { modules, .. } => modules,
        other => panic!("unexpected catalog.list response: {other:?}"),
    }
}

async fn supervisor_rescan(path: &Path, corr: u64) -> SupervisorRescanResult {
    let mut client = wait_for_client(path, START_TIMEOUT).await;
    match control_rpc_on_stream(&mut client, corr, ClientControlRequest::SupervisorRescan {}).await
    {
        ClientControlResponse::SupervisorRescan { result } => result,
        other => panic!("unexpected supervisor.rescan response: {other:?}"),
    }
}

async fn supervisor_rescan_error(path: &Path, corr: u64) -> ErrorBody {
    let mut client = wait_for_client(path, START_TIMEOUT).await;
    control_rpc_result_on_stream(&mut client, corr, ClientControlRequest::SupervisorRescan {})
        .await
        .expect_err("supervisor.rescan should return a typed error")
}

async fn wait_for_supervisor_absent(path: &Path, module_id: &str, wait: Duration) {
    let deadline = Instant::now() + wait;
    let mut corr = 3_000;
    loop {
        if supervisor_modules(path, corr)
            .await
            .iter()
            .all(|entry| entry.module_id != module_id)
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "module {module_id} remained in supervisor.list after {wait:?}"
        );
        corr += 1;
        sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_for_catalog_absent(path: &Path, module_id: &str, wait: Duration) {
    let deadline = Instant::now() + wait;
    let mut corr = 4_000;
    loop {
        if catalog_modules(path, Some(module_id), corr)
            .await
            .is_empty()
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "module {module_id} remained in catalog after {wait:?}"
        );
        corr += 1;
        sleep(Duration::from_millis(20)).await;
    }
}

async fn open_route<S>(stream: &mut S, module_id: &str, corr: u64) -> u16
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    open_route_with_identity(stream, module_id, corr, None)
        .await
        .unwrap_or_else(|error| panic!("route.open returned error: {error:?}"))
}

async fn open_route_with_consumer_identity<S>(
    stream: &mut S,
    module_id: &str,
    corr: u64,
    consumer_identity: ConsumerIdentity,
) -> Result<u16, ErrorBody>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    open_route_with_identity(stream, module_id, corr, Some(consumer_identity)).await
}

async fn open_route_with_identity<S>(
    stream: &mut S,
    module_id: &str,
    corr: u64,
    consumer_identity: Option<ConsumerIdentity>,
) -> Result<u16, ErrorBody>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let project_root = unique_temp_dir("daemon-config-project");
    fs::create_dir_all(&project_root).unwrap();
    match control_rpc_result_on_stream(
        stream,
        corr,
        ClientControlRequest::RouteOpen {
            target: RouteTarget::ToolProvider {
                module_id: module_id.to_string(),
            },
            identity: BindIdentity {
                project_root,
                harness: "daemon-config-test".to_string(),
                session: format!("session-{corr}"),
            },
            consumer_identity,
            consumer_capabilities: None,
        },
    )
    .await?
    {
        ClientControlResponse::RouteOpen {
            route_channel,
            // WIRE-WAVE2: retain this epoch in the route test handle.
            route_epoch: _route_epoch,
        } => Ok(route_channel),
        other => panic!("unexpected route.open response: {other:?}"),
    }
}

async fn call_tool<S>(stream: &mut S, route_channel: u16, corr: u64, name: &str) -> String
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let body = serde_json::to_vec(&json!({ "name": name, "arguments": {} })).unwrap();
    write_frame(stream, &data_request(route_channel, corr, &body))
        .await
        .unwrap();
    stream.flush().await.unwrap();
    let response = read_frame_timeout(stream).await;
    assert_eq!(response.header.ty, FrameType::Response);
    assert_eq!(response.header.channel, route_channel);
    assert_eq!(response.header.corr, corr);
    let body: Value = serde_json::from_slice(&response.body).unwrap();
    body["content"][0]["text"]
        .as_str()
        .expect("stub tool response should contain text")
        .to_string()
}

async fn control_rpc_on_stream<S>(
    stream: &mut S,
    corr: u64,
    request: ClientControlRequest,
) -> ClientControlResponse
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    control_rpc_result_on_stream(stream, corr, request)
        .await
        .unwrap_or_else(|error| panic!("control RPC returned error: {error:?}"))
}

async fn control_rpc_result_on_stream<S>(
    stream: &mut S,
    corr: u64,
    request: ClientControlRequest,
) -> Result<ClientControlResponse, ErrorBody>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    write_frame(stream, &control_request_frame(corr, request))
        .await
        .unwrap();
    stream.flush().await.unwrap();
    let frame = read_frame_timeout(stream).await;
    assert_eq!(frame.header.channel, 0);
    assert_eq!(frame.header.corr, corr);
    match frame.header.ty {
        FrameType::Response => Ok(serde_json::from_slice(&frame.body).unwrap()),
        FrameType::Error => Err(serde_json::from_slice(&frame.body).unwrap()),
        ty => panic!("unexpected control RPC frame type: {ty:?}"),
    }
}

fn control_request_frame(corr: u64, request: ClientControlRequest) -> Frame {
    let body = serde_json::to_vec(&request).unwrap();
    Frame::build(
        FrameType::Request,
        Flags::new(false, Priority::Passive, false),
        0, // WIRE-WAVE2: thread the binding epoch.
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
        channel, // WIRE-WAVE2: thread the binding epoch.
        0,
        corr,
        body.to_vec(),
    )
    .unwrap()
}

async fn read_frame_timeout<S>(stream: &mut S) -> Frame
where
    S: AsyncRead + Unpin,
{
    timeout(READ_TIMEOUT, async {
        read_frame(stream)
            .await
            .unwrap()
            .expect("connection should stay open")
    })
    .await
    .expect("timed out waiting for frame")
}

fn unique_temp_dir(name: &str) -> PathBuf {
    let nonce = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("subc-core-{name}-{}-{nonce}", process::id()))
}
