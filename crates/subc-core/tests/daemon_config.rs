use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use serde_json::{json, Value};
use subc_control::{ClientControlRequest, ClientControlResponse, SupervisorEntry};
use subc_core::{
    bootstrap::{run_with_config, run_with_daemon_config_path, BootstrapConfig},
    read_frame, write_frame, Frame,
};
use subc_protocol::{BindIdentity, Flags, FrameType, Priority, RouteTarget};
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
const START_TIMEOUT: Duration = Duration::from_secs(2);
const STATE_TIMEOUT: Duration = Duration::from_secs(2);

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

async fn open_route<S>(stream: &mut S, module_id: &str, corr: u64) -> u16
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let project_root = unique_temp_dir("daemon-config-project");
    fs::create_dir_all(&project_root).unwrap();
    match control_rpc_on_stream(
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
            consumer_identity: None,
            consumer_capabilities: None,
        },
    )
    .await
    {
        ClientControlResponse::RouteOpen { route_channel } => route_channel,
        other => panic!("unexpected route.open response: {other:?}"),
    }
}

async fn control_rpc_on_stream<S>(
    stream: &mut S,
    corr: u64,
    request: ClientControlRequest,
) -> ClientControlResponse
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
        FrameType::Response => serde_json::from_slice(&frame.body).unwrap(),
        FrameType::Error => panic!(
            "control RPC returned error: {:?}",
            serde_json::from_slice::<Value>(&frame.body).unwrap()
        ),
        ty => panic!("unexpected control RPC frame type: {ty:?}"),
    }
}

fn control_request_frame(corr: u64, request: ClientControlRequest) -> Frame {
    let body = serde_json::to_vec(&request).unwrap();
    Frame::build(
        FrameType::Request,
        Flags::new(false, Priority::Passive, false),
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
