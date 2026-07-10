use std::{
    fs,
    ops::Deref,
    path::{Path, PathBuf},
    process::{self, Command, Output},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use serde_json::Value;
use subc_control::{ClientControlRequest, ClientControlResponse, SupervisorEntry};
use subc_core::{
    read_frame, write_frame, Frame, ModuleSpec, RestartPolicy, SupervisedModule, Supervisor,
    SupervisorHandle, SupervisorProcessLiveness,
};
use subc_protocol::{Flags, FrameType, Priority};
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt},
    time::{sleep, timeout, Instant},
};

mod common;
use common::{
    connect_authed_client, start_test_daemon_with_process_liveness_and_supervisor, TestDaemon,
};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
const READ_TIMEOUT: Duration = Duration::from_secs(2);
const SETUP_TIMEOUT: Duration = Duration::from_secs(10);

struct TestServer {
    daemon: TestDaemon,
    process_liveness: Arc<SupervisorProcessLiveness>,
    supervisor_handle: SupervisorHandle,
}

impl TestServer {
    async fn start() -> Self {
        let process_liveness = Arc::new(SupervisorProcessLiveness::new());
        let supervisor_handle = SupervisorHandle::new();
        let daemon = start_test_daemon_with_process_liveness_and_supervisor(
            "ck-cli-server",
            process_liveness.clone(),
            supervisor_handle.clone(),
        )
        .await;
        Self {
            daemon,
            process_liveness,
            supervisor_handle,
        }
    }
}

impl Deref for TestServer {
    type Target = TestDaemon;

    fn deref(&self) -> &Self::Target {
        &self.daemon
    }
}

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(name: &str) -> Self {
        let path = unique_temp_dir(name);
        fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn module_list_json_uses_subc_override_and_shows_stub() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server);
    let module_id = "ck-list-stub";
    let module = spawn_stub(&server, &supervisor, module_id).await;
    let hidden_runtime = TempDir::new("ck-hidden-runtime");
    let hidden_home = TempDir::new("ck-hidden-home");
    let hidden_tmp = TempDir::new("ck-hidden-tmp");

    let output = ck_command()
        .args(["module", "list", "--json", "--subc"])
        .arg(&server.connection_file_path)
        .env("XDG_RUNTIME_DIR", hidden_runtime.path())
        .env("HOME", hidden_home.path())
        .env("TMPDIR", hidden_tmp.path())
        .env("TMP", hidden_tmp.path())
        .env("TEMP", hidden_tmp.path())
        .output()
        .unwrap();
    let value = assert_json_success(output);
    let modules = value["modules"].as_array().unwrap();
    assert!(
        modules
            .iter()
            .any(|module| module["module_id"] == module_id),
        "ck module list --json should include the supervised stub: {value}"
    );

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn module_restart_stop_start_json_drive_supervisor() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server);
    let module_id = "ck-control-stub";
    let module = spawn_stub(&server, &supervisor, module_id).await;

    let restart = assert_json_success(ck_with_subc(
        &server.connection_file_path,
        ["module", "restart", module_id, "--json"],
    ));
    assert_eq!(restart["module_id"], module_id);
    assert_eq!(restart["applied"], true);
    wait_for_supervisor_entry(&server.connection_file_path, module_id, |entry| {
        entry.state == "running" && entry.enabled && entry.live
    })
    .await;

    let stop = assert_json_success(ck_with_subc(
        &server.connection_file_path,
        ["module", "stop", module_id, "--json"],
    ));
    assert_eq!(stop["module_id"], module_id);
    assert_eq!(stop["applied"], true);
    wait_for_supervisor_entry(&server.connection_file_path, module_id, |entry| {
        entry.state == "disabled" && !entry.enabled && !entry.live
    })
    .await;

    let start = assert_json_success(ck_with_subc(
        &server.connection_file_path,
        ["module", "start", module_id, "--json"],
    ));
    assert_eq!(start["module_id"], module_id);
    assert_eq!(start["applied"], true);
    wait_for_supervisor_entry(&server.connection_file_path, module_id, |entry| {
        entry.state == "running" && entry.enabled && entry.live
    })
    .await;

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quota_table_renders_providers_and_used_percent() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server);
    let module_id = "ai-provider-quota";
    let fixture = quota_wire_fixture();
    let module = spawn_quota_stub(&server, &supervisor, module_id, &fixture).await;

    let output = ck_with_subc(&server.connection_file_path, ["quota"]);
    assert_exit(&output, 0);
    let stdout = text(&output.stdout);
    assert!(stdout.contains("anthropic"), "stdout:\n{stdout}");
    assert!(stdout.contains("openai"), "stdout:\n{stdout}");
    assert!(stdout.contains("work"), "account column, stdout:\n{stdout}");
    assert!(
        stdout.contains("    42"),
        "expected usedPercent 42.0 in table, stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("rate limit probe failed for secondary account"),
        "degraded error should appear, stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("Bonus credits"),
        "extraRateWindows title should appear, stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("cookie jar unreadable"),
        "error-only entry should appear, stdout:\n{stdout}"
    );

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quota_filters_by_provider_id() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server);
    let module_id = "ai-provider-quota";
    let fixture = quota_wire_fixture();
    let module = spawn_quota_stub(&server, &supervisor, module_id, &fixture).await;

    let output = ck_with_subc(&server.connection_file_path, ["quota", "anthropic"]);
    assert_exit(&output, 0);
    let stdout = text(&output.stdout);
    assert!(stdout.contains("anthropic"), "stdout:\n{stdout}");
    assert!(
        !stdout.lines().any(|line| line.starts_with("openai")),
        "stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("    42"),
        "expected usedPercent 42.0, stdout:\n{stdout}"
    );

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quota_unknown_provider_lists_valid_ids_and_exits_nonzero() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server);
    let module_id = "ai-provider-quota";
    let fixture = quota_wire_fixture();
    let module = spawn_quota_stub(&server, &supervisor, module_id, &fixture).await;

    let output = ck_with_subc(&server.connection_file_path, ["quota", "unknown-id"]);
    assert_exit(&output, 1);
    let stderr = text(&output.stderr);
    assert!(
        stderr.contains("unknown provider 'unknown-id'"),
        "stderr:\n{stderr}"
    );
    assert!(stderr.contains("anthropic"), "stderr:\n{stderr}");
    assert!(stderr.contains("openai"), "stderr:\n{stderr}");
    assert!(stderr.contains("grok"), "stderr:\n{stderr}");

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quota_json_emits_wrapped_reply_verbatim() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server);
    let module_id = "ai-provider-quota";
    let fixture = quota_wire_fixture();
    let module = spawn_quota_stub(&server, &supervisor, module_id, &fixture).await;

    let output = ck_with_subc(&server.connection_file_path, ["quota", "--json"]);
    let value = assert_json_success(output);
    let result = value["result"].as_array().expect("result array");
    assert_eq!(result.len(), 3);
    assert_eq!(result[0]["provider"], "anthropic");
    assert!(result[0]["usage"]["primary"]["usedPercent"]
        .as_f64()
        .is_some());
    assert_eq!(result[2]["error"].as_str(), Some("cookie jar unreadable"));

    module.stop().await.unwrap();
}

#[test]
fn discovery_failure_lists_tried_paths_and_exits_2() {
    let runtime = TempDir::new("ck-empty-runtime");
    let home = TempDir::new("ck-empty-home");
    let tmp = TempDir::new("ck-empty-tmp");

    let output = ck_command()
        .args(["module", "list", "--json"])
        .env("XDG_RUNTIME_DIR", runtime.path())
        .env("HOME", home.path())
        .env("TMPDIR", tmp.path())
        .env("TMP", tmp.path())
        .env("TEMP", tmp.path())
        .output()
        .unwrap();

    assert_exit(&output, 2);
    assert!(
        output.stdout.is_empty(),
        "discovery failures must not write data to stdout: {}",
        text(&output.stdout)
    );
    let stderr = text(&output.stderr);
    assert_eq!(
        stderr.lines().count(),
        1,
        "stderr should be one line: {stderr}"
    );
    assert!(stderr.contains("no usable subc connection file found"));
    assert!(stderr.contains(
        &runtime
            .path()
            .join("subc-connection.json")
            .display()
            .to_string()
    ));
    assert!(stderr.contains(&home_connection_file(home.path()).display().to_string()));
    assert!(stderr.contains(&tmp.path().display().to_string()));
}

fn ck_command() -> Command {
    Command::new(env!("CARGO_BIN_EXE_ck"))
}

fn ck_with_subc<const N: usize>(connection_file: &Path, args: [&str; N]) -> Output {
    let mut command = ck_command();
    command.args(args).arg("--subc").arg(connection_file);
    command.output().unwrap()
}

fn assert_json_success(output: Output) -> Value {
    assert_exit(&output, 0);
    serde_json::from_slice(&output.stdout).unwrap_or_else(|err| {
        panic!(
            "ck stdout was not JSON: {err}\nstdout:\n{}\nstderr:\n{}",
            text(&output.stdout),
            text(&output.stderr)
        )
    })
}

fn assert_exit(output: &Output, expected: i32) {
    assert_eq!(
        output.status.code(),
        Some(expected),
        "unexpected ck exit status\nstdout:\n{}\nstderr:\n{}",
        text(&output.stdout),
        text(&output.stderr)
    );
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn home_connection_file(home: &Path) -> PathBuf {
    home.join(".local")
        .join("share")
        .join("cortexkit")
        .join("run")
        .join("subc-connection.json")
}

fn supervisor(server: &TestServer) -> Supervisor {
    Supervisor::new(
        Arc::clone(&server.registry),
        RestartPolicy::new(1, Duration::from_millis(10)),
    )
    .with_process_liveness(Arc::clone(&server.process_liveness))
    .with_forwarding(Arc::clone(&server.forwarding))
    .with_handle(server.supervisor_handle.clone())
    .with_drain_timeout(Duration::from_millis(25))
    .with_connection_file_path(server.connection_file_path.clone())
}

async fn spawn_stub(
    server: &TestServer,
    supervisor: &Supervisor,
    module_id: &str,
) -> SupervisedModule {
    let module = supervisor.spawn(stub_spec(module_id)).unwrap();
    wait_for_supervisor_entry(&server.connection_file_path, module_id, |entry| {
        entry.state == "running" && entry.enabled && entry.live
    })
    .await;
    module
}

fn stub_spec(module_id: &str) -> ModuleSpec {
    ModuleSpec {
        module_id: module_id.to_string(),
        program: PathBuf::from(env!("CARGO_BIN_EXE_fake-aft-stub")),
        args: Vec::new(),
        env: vec![("FAKE_AFT_MODULE_ID".to_string(), module_id.to_string())],
        reserved: false,
        reserved_prefixes: Vec::new(),
    }
}

fn quota_wire_fixture() -> Value {
    serde_json::json!([
        {
            "provider": "anthropic",
            "account": "work",
            "source": "vault",
            "usage": {
                "primary": {
                    "usedPercent": 42.0,
                    "resetsAt": "2099-06-15T14:30:00Z",
                    "windowMinutes": 300
                },
                "secondary": {
                    "usedPercent": 17.5,
                    "resetsAt": "2099-06-22T08:00:00+00:00",
                    "windowMinutes": 10080
                },
                "extraRateWindows": [
                    {
                        "title": "Bonus credits",
                        "id": "bonus-1",
                        "window": {
                            "usedPercent": 5.0,
                            "resetsAt": "2099-07-01T00:00:00Z",
                            "windowMinutes": 43200
                        }
                    }
                ]
            }
        },
        {
            "provider": "openai",
            "account": "personal",
            "error": "rate limit probe failed for secondary account",
            "usage": {
                "primary": {
                    "usedPercent": 88.0,
                    "resetsAt": "2099-06-15T18:00:00Z",
                    "windowMinutes": 300
                }
            }
        },
        {
            "provider": "grok",
            "error": "cookie jar unreadable"
        }
    ])
}

async fn spawn_quota_stub(
    server: &TestServer,
    supervisor: &Supervisor,
    module_id: &str,
    fixture: &Value,
) -> SupervisedModule {
    let fixture_json = serde_json::to_string(fixture).unwrap();
    let module = supervisor
        .spawn(ModuleSpec {
            module_id: module_id.to_string(),
            program: PathBuf::from(env!("CARGO_BIN_EXE_fake-aft-stub")),
            args: Vec::new(),
            env: vec![
                ("FAKE_AFT_MODULE_ID".to_string(), module_id.to_string()),
                (
                    "FAKE_AFT_ROLE".to_string(),
                    "management_surface".to_string(),
                ),
                ("FAKE_AFT_USAGE_GET_FIXTURE".to_string(), fixture_json),
            ],
            reserved: false,
            reserved_prefixes: Vec::new(),
        })
        .unwrap();
    wait_for_supervisor_entry(&server.connection_file_path, module_id, |entry| {
        entry.state == "running" && entry.enabled && entry.live
    })
    .await;
    module
}

async fn wait_for_supervisor_entry(
    path: &Path,
    module_id: &str,
    predicate: impl Fn(&SupervisorEntry) -> bool,
) -> SupervisorEntry {
    let deadline = Instant::now() + SETUP_TIMEOUT;
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
            panic!("module {module_id} did not reach expected supervisor state within {SETUP_TIMEOUT:?}; modules: {modules:?}");
        }
        corr += 1;
        sleep(Duration::from_millis(20)).await;
    }
}

async fn supervisor_modules(path: &Path, corr: u64) -> Vec<SupervisorEntry> {
    let mut client = wait_for_client(path).await;
    match control_rpc_on_stream(&mut client, corr, ClientControlRequest::SupervisorList {}).await {
        ClientControlResponse::SupervisorList { modules, .. } => modules,
        other => panic!("unexpected supervisor.list response: {other:?}"),
    }
}

async fn wait_for_client(path: &Path) -> tokio::net::TcpStream {
    let deadline = Instant::now() + SETUP_TIMEOUT;
    loop {
        match connect_authed_client(path).await {
            Ok(client) => return client,
            Err(err) if Instant::now() < deadline => {
                let _ = err;
                sleep(Duration::from_millis(20)).await;
            }
            Err(err) => {
                panic!("daemon did not accept authenticated client within {SETUP_TIMEOUT:?}: {err}")
            }
        }
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
