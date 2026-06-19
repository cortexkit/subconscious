#![forbid(unsafe_code)]

use std::{
    env, fs,
    future::Future,
    io::ErrorKind,
    net::Ipv4Addr,
    path::{Path, PathBuf},
    process,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use rmcp::{
    model::{
        CallToolRequest, CallToolRequestParams, CancelledNotificationParam, ClientRequest,
        JsonObject, ProgressNotificationParam,
    },
    service::{
        MaybeSendFuture, NotificationContext, PeerRequestOptions, RunningService, ServiceError,
    },
    ClientHandler, RoleClient, ServiceExt,
};
use serde_json::{json, Value};
use subc_core::{
    serve_listener, ControlHandler, ModuleProcessLiveness, ModuleSpec, Registry, RestartPolicy,
    Router, ServerAuth, SupervisedModule, Supervisor, SupervisorProcessLiveness,
};
use subc_transport::{
    generate_daemon_id, generate_key, write_atomic, ConnectionInfo, Endpoint, SCHEMA_VERSION,
};
use tokio::{
    net::TcpListener,
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::{Mutex as TokioMutex, Notify},
    task::JoinHandle,
    time::{sleep, timeout, Instant},
};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
const TEST_DAEMON_VER: &str = "test-subc-mcp";
const READ_TIMEOUT: Duration = Duration::from_secs(10);

struct TestDaemon {
    registry: Arc<Registry>,
    connection_file_path: PathBuf,
    temp_dir: PathBuf,
    task: JoinHandle<Result<(), subc_core::ServerError>>,
}

impl Drop for TestDaemon {
    fn drop(&mut self) {
        self.task.abort();
        let _ = fs::remove_dir_all(&self.temp_dir);
    }
}

struct TestServer {
    daemon: TestDaemon,
    process_liveness: Arc<SupervisorProcessLiveness>,
}

impl TestServer {
    async fn start() -> Self {
        let process_liveness = Arc::new(SupervisorProcessLiveness::new());
        let daemon = start_test_daemon_with_process_liveness(
            "subc-mcp-phase2b",
            Arc::clone(&process_liveness),
        )
        .await;
        Self {
            daemon,
            process_liveness,
        }
    }
}

struct TestProject {
    path: PathBuf,
}

impl TestProject {
    fn new(label: &str) -> Self {
        let path = unique_temp_dir(label);
        fs::create_dir_all(&path).unwrap();
        Self { path }
    }
}

impl Drop for TestProject {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

struct ShimProcess {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: Option<ChildStdout>,
}

impl ShimProcess {
    async fn serve_mcp_client<H>(&mut self, handler: H) -> RunningService<RoleClient, H>
    where
        H: ClientHandler,
    {
        let stdout = self.stdout.take().expect("shim stdout should be available");
        let stdin = self.stdin.take().expect("shim stdin should be available");
        handler
            .serve((stdout, stdin))
            .await
            .expect("rmcp client should initialize through subc-mcp shim")
    }
}

#[derive(Clone)]
struct TestMcpClient {
    progress: Arc<TokioMutex<Vec<ProgressNotificationParam>>>,
    progress_notify: Arc<Notify>,
}

impl TestMcpClient {
    fn new() -> Self {
        Self {
            progress: Arc::new(TokioMutex::new(Vec::new())),
            progress_notify: Arc::new(Notify::new()),
        }
    }
}

impl ClientHandler for TestMcpClient {
    fn on_progress(
        &self,
        params: ProgressNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) -> impl Future<Output = ()> + MaybeSendFuture + '_ {
        let progress = Arc::clone(&self.progress);
        let notify = Arc::clone(&self.progress_notify);
        async move {
            progress.lock().await.push(params);
            notify.notify_waiters();
        }
    }
}

struct McpHarness {
    _server: TestServer,
    _project: TestProject,
    provider: SupervisedModule,
    module: Child,
    shim: ShimProcess,
    client: RunningService<RoleClient, TestMcpClient>,
    client_handler: TestMcpClient,
    events_path: PathBuf,
}

impl McpHarness {
    async fn start(label: &str, provider_env: &[(&str, &str)]) -> Self {
        let server = TestServer::start().await;
        let events_path = server
            .daemon
            .temp_dir
            .join(format!("{label}-fake-aft-events.jsonl"));

        let provider = supervisor(&server)
            .spawn(stub_spec("fake-aft", &events_path, provider_env))
            .unwrap();
        wait_for_registration(&server.daemon.registry, "fake-aft", READ_TIMEOUT).await;

        let module_connection_file = server
            .daemon
            .temp_dir
            .join(format!("{label}-subc-mcp.json"));
        let mut module = spawn_module(&server.daemon.connection_file_path, &module_connection_file);
        wait_for_module_connection_file(&mut module, &module_connection_file, READ_TIMEOUT).await;

        let project = TestProject::new(label);
        let mut shim = spawn_shim(&module_connection_file, &project.path);
        let client_handler = TestMcpClient::new();
        let client = shim.serve_mcp_client(client_handler.clone()).await;

        Self {
            _server: server,
            _project: project,
            provider,
            module,
            shim,
            client,
            client_handler,
            events_path,
        }
    }

    async fn shutdown(self) {
        let Self {
            _server,
            _project,
            provider,
            mut module,
            mut shim,
            client,
            ..
        } = self;
        let _ = client.cancel().await;
        let _ = timeout(Duration::from_secs(2), shim.child.wait()).await;
        if module.try_wait().unwrap().is_none() {
            let _ = module.start_kill();
            let _ = timeout(Duration::from_secs(2), module.wait()).await;
        }
        provider.stop().await.unwrap();
    }
}

#[tokio::test]
async fn mcp_initialize_advertises_tools_capability() {
    let harness = McpHarness::start("mcp-init", &[]).await;

    let server_info = harness
        .client
        .peer()
        .peer_info()
        .expect("client should store initialize result");
    assert!(
        server_info.capabilities.tools.is_some(),
        "subc-mcp should advertise the MCP tools capability"
    );
    assert_eq!(server_info.server_info.name, "subc-mcp");

    harness.shutdown().await;
}

#[tokio::test]
async fn mcp_tools_list_returns_stub_manifest_tools() {
    let harness = McpHarness::start("mcp-list", &[]).await;

    let tools = harness.client.peer().list_tools(None).await.unwrap();
    assert_eq!(tools.tools.len(), 1);
    let tool = &tools.tools[0];
    assert_eq!(tool.name, "fake_read");
    assert_eq!(
        tool.input_schema.get("type"),
        Some(&Value::String("object".to_owned()))
    );

    harness.shutdown().await;
}

#[tokio::test]
async fn mcp_tools_call_returns_result_and_stub_receives_route_body() {
    let harness = McpHarness::start("mcp-call", &[]).await;

    let mut args = JsonObject::new();
    args.insert("value".to_owned(), json!("hello"));
    let result = harness
        .client
        .peer()
        .call_tool(CallToolRequestParams::new("fake_read").with_arguments(args))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(false));
    assert!(
        result_text(&result).contains("fake-aft tool fake_read called with"),
        "unexpected tool result: {result:?}"
    );

    let event = wait_for_stub_event(&harness.events_path, READ_TIMEOUT, |event| {
        event.get("kind") == Some(&Value::String("tool_call".to_owned()))
    })
    .await;
    assert_eq!(
        event.get("name"),
        Some(&Value::String("fake_read".to_owned()))
    );
    assert_eq!(event.pointer("/arguments/value"), Some(&json!("hello")));
    assert!(
        event
            .get("progress_token")
            .is_some_and(|token| !token.is_null()),
        "rmcp should supply a progress token and subc-mcp should forward it in the v1 route contract"
    );

    harness.shutdown().await;
}

#[tokio::test]
async fn mcp_tools_call_forwards_progress_notifications() {
    let harness = McpHarness::start(
        "mcp-progress",
        &[
            ("FAKE_AFT_TOOLCALL_PROGRESS", "1"),
            ("FAKE_AFT_TOOLCALL_DELAY_MS", "100"),
        ],
    )
    .await;

    let peer = harness.client.peer().clone();
    let call = tokio::spawn(async move {
        peer.call_tool(CallToolRequestParams::new("fake_read"))
            .await
    });

    timeout(
        READ_TIMEOUT,
        harness.client_handler.progress_notify.notified(),
    )
    .await
    .expect("timed out waiting for progress before tool result");
    let progress = harness.client_handler.progress.lock().await.clone();
    assert!(
        !progress.is_empty(),
        "client should receive progress notifications"
    );
    assert_eq!(progress[0].progress, 1.0);
    assert_eq!(progress[0].total, Some(2.0));

    let result = call.await.unwrap().unwrap();
    assert_eq!(result.is_error, Some(false));

    harness.shutdown().await;
}

#[tokio::test]
async fn mcp_tools_call_cancel_sends_route_cancel() {
    let harness = McpHarness::start("mcp-cancel", &[("FAKE_AFT_TOOLCALL_DELAY_MS", "1000")]).await;

    let peer = harness.client.peer().clone();
    let handle = peer
        .send_cancellable_request(
            ClientRequest::CallToolRequest(CallToolRequest::new(CallToolRequestParams::new(
                "fake_read",
            ))),
            PeerRequestOptions::no_options(),
        )
        .await
        .unwrap();
    let request_id = handle.id.clone();
    peer.notify_cancelled(CancelledNotificationParam {
        request_id,
        reason: Some("test cancellation".to_owned()),
    })
    .await
    .unwrap();

    let err = handle.await_response().await.unwrap_err();
    assert!(
        matches!(err, ServiceError::Cancelled { .. }),
        "client should observe a cancelled MCP request, got {err:?}"
    );

    let cancel = wait_for_stub_event(&harness.events_path, READ_TIMEOUT, |event| {
        event.get("kind") == Some(&Value::String("cancel".to_owned()))
            && event.get("claimed") == Some(&Value::Bool(true))
    })
    .await;
    assert_eq!(
        cancel.get("kind"),
        Some(&Value::String("cancel".to_owned()))
    );

    harness.shutdown().await;
}

#[tokio::test]
async fn mcp_tools_call_splits_tool_errors_from_subc_errors() {
    let tool_error_harness =
        McpHarness::start("mcp-tool-error", &[("FAKE_AFT_TOOLCALL_ERROR", "1")]).await;
    let tool_error = tool_error_harness
        .client
        .peer()
        .call_tool(CallToolRequestParams::new("fake_read"))
        .await
        .unwrap();
    assert_eq!(tool_error.is_error, Some(true));
    assert!(
        result_text(&tool_error).contains("fake-aft tool error"),
        "tool execution failures should be successful MCP responses carrying isError=true"
    );
    tool_error_harness.shutdown().await;

    let subc_error_harness =
        McpHarness::start("mcp-subc-error", &[("FAKE_AFT_TOOLCALL_SUBC_ERROR", "1")]).await;
    let subc_error = subc_error_harness
        .client
        .peer()
        .call_tool(CallToolRequestParams::new("fake_read"))
        .await
        .unwrap_err();
    match subc_error {
        ServiceError::McpError(error) => {
            assert!(
                error.message.contains("target_unavailable"),
                "subc-level failures should surface as JSON-RPC errors, got {error:?}"
            );
        }
        other => panic!("expected JSON-RPC MCP error for subc-level failure, got {other:?}"),
    }
    subc_error_harness.shutdown().await;
}

async fn start_test_daemon_with_process_liveness(
    name: &str,
    process_liveness: Arc<SupervisorProcessLiveness>,
) -> TestDaemon {
    let temp_dir = unique_temp_dir(name);
    fs::create_dir_all(&temp_dir).unwrap();
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let connection_file_path = temp_dir.join("subc-conn.json");
    let conn = ConnectionInfo {
        schema: SCHEMA_VERSION,
        endpoints: vec![Endpoint {
            host: Ipv4Addr::LOCALHOST.to_string(),
            port,
        }],
        key: generate_key().unwrap(),
        daemon_id: generate_daemon_id().unwrap(),
        pid: process::id(),
        daemon_ver: TEST_DAEMON_VER.to_owned(),
    };
    write_atomic(&connection_file_path, &conn).unwrap();

    let registry = Arc::new(Registry::default());
    let process_liveness: Arc<dyn ModuleProcessLiveness> = process_liveness;
    let control = Arc::new(
        ControlHandler::new(Arc::clone(&registry)).with_process_liveness(process_liveness),
    );
    let router = Arc::new(Router::with_control_handler(control));
    let auth = ServerAuth::new(conn.key, conn.daemon_id, conn.daemon_ver);
    let task = tokio::spawn(serve_listener(listener, router, auth));

    TestDaemon {
        registry,
        connection_file_path,
        temp_dir,
        task,
    }
}

fn supervisor(server: &TestServer) -> Supervisor {
    Supervisor::new(
        Arc::clone(&server.daemon.registry),
        RestartPolicy::new(0, Duration::ZERO),
    )
    .with_process_liveness(Arc::clone(&server.process_liveness))
    .with_drain_timeout(Duration::from_millis(25))
    .with_connection_file_path(server.daemon.connection_file_path.clone())
}

fn stub_spec(module_id: &str, events_path: &Path, extra_env: &[(&str, &str)]) -> ModuleSpec {
    let (program, args) = fake_aft_stub_command();
    let mut env = vec![
        ("FAKE_AFT_MODULE_ID".to_owned(), module_id.to_owned()),
        (
            "FAKE_AFT_EVENTS_PATH".to_owned(),
            events_path.display().to_string(),
        ),
    ];
    env.extend(
        extra_env
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned())),
    );
    ModuleSpec {
        module_id: module_id.to_owned(),
        program,
        args,
        env,
    }
}

fn fake_aft_stub_command() -> (PathBuf, Vec<String>) {
    if let Some(path) = option_env!("CARGO_BIN_EXE_fake-aft-stub") {
        return (PathBuf::from(path), Vec::new());
    }

    if let Ok(current_exe) = env::current_exe() {
        if let Some(target_debug) = current_exe.parent().and_then(Path::parent) {
            let candidate = target_debug.join(format!("fake-aft-stub{}", env::consts::EXE_SUFFIX));
            if candidate_is_fresh(&candidate) {
                return (candidate, Vec::new());
            }
        }
    }

    let cargo = env::var_os("CARGO")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("cargo"));
    (
        cargo,
        vec![
            "run".to_owned(),
            "-p".to_owned(),
            "subc-core".to_owned(),
            "--bin".to_owned(),
            "fake-aft-stub".to_owned(),
            "--".to_owned(),
        ],
    )
}

fn candidate_is_fresh(candidate: &Path) -> bool {
    let Ok(candidate_modified) = fs::metadata(candidate).and_then(|metadata| metadata.modified())
    else {
        return false;
    };
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("subc-mcp should live under crates/")
        .join("subc-core/src/bin/fake-aft-stub.rs");
    let Ok(source_modified) = fs::metadata(source).and_then(|metadata| metadata.modified()) else {
        return true;
    };
    candidate_modified >= source_modified
}

fn spawn_module(subc_connection_file: &Path, module_connection_file: &Path) -> Child {
    let mut command = Command::new(env!("CARGO_BIN_EXE_subc-mcp"));
    command
        .arg("module")
        .arg("--subc")
        .arg(subc_connection_file)
        .arg("--connection-file")
        .arg(module_connection_file)
        .kill_on_drop(true);
    command.spawn().unwrap()
}

async fn wait_for_module_connection_file(child: &mut Child, path: &Path, wait: Duration) {
    let deadline = Instant::now() + wait;
    loop {
        if subc_transport::read(path).is_ok() {
            return;
        }
        if let Some(status) = child.try_wait().unwrap() {
            panic!(
                "subc-mcp module exited before publishing {}: {status}",
                path.display()
            );
        }
        assert!(
            Instant::now() < deadline,
            "subc-mcp module did not publish {} within {wait:?}",
            path.display()
        );
        sleep(Duration::from_millis(10)).await;
    }
}

fn spawn_shim(module_connection_file: &Path, project_root: &Path) -> ShimProcess {
    let mut command = Command::new(env!("CARGO_BIN_EXE_subc-mcp"));
    command
        .arg("shim")
        .arg("--module-connection-file")
        .arg(module_connection_file)
        .env("CLAUDE_PROJECT_DIR", project_root)
        .stdin(process::Stdio::piped())
        .stdout(process::Stdio::piped())
        .stderr(process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().unwrap();
    let stdin = child.stdin.take().expect("shim stdin should be piped");
    let stdout = child.stdout.take().expect("shim stdout should be piped");
    ShimProcess {
        child,
        stdin: Some(stdin),
        stdout: Some(stdout),
    }
}

async fn wait_for_registration(registry: &Registry, module_id: &str, wait: Duration) {
    let deadline = Instant::now() + wait;
    loop {
        if registry.get_module(module_id).unwrap().is_some() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "module {module_id} did not register within {wait:?}"
        );
        sleep(Duration::from_millis(10)).await;
    }
}

async fn wait_for_stub_event<F>(path: &Path, wait: Duration, matches: F) -> Value
where
    F: Fn(&Value) -> bool,
{
    let deadline = Instant::now() + wait;
    let mut last_parse_error = None;
    loop {
        match stub_events(path) {
            Ok(events) => {
                for event in events {
                    if matches(&event) {
                        return event;
                    }
                }
            }
            Err(err) => last_parse_error = Some(err),
        }
        if Instant::now() >= deadline {
            let events = stub_events(path)
                .map(|events| format!("{events:?}"))
                .unwrap_or_else(|err| format!("unparseable ({err})"));
            panic!(
                "stub event did not appear within {wait:?}; events: {events}; last parse error: {last_parse_error:?}"
            );
        }
        sleep(Duration::from_millis(10)).await;
    }
}

fn stub_events(path: &Path) -> Result<Vec<Value>, String> {
    match fs::read_to_string(path) {
        Ok(contents) => contents
            .lines()
            .enumerate()
            .filter(|(_, line)| !line.trim().is_empty())
            .map(|(index, line)| {
                serde_json::from_str(line).map_err(|err| {
                    format!(
                        "failed to parse stub event line {} in {}: {err}",
                        index + 1,
                        path.display()
                    )
                })
            })
            .collect(),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(Vec::new()),
        Err(err) => Err(format!(
            "failed to read stub events {}: {err}",
            path.display()
        )),
    }
}

fn result_text(result: &rmcp::model::CallToolResult) -> &str {
    result
        .content
        .first()
        .and_then(|content| content.as_text())
        .map(|text| text.text.as_str())
        .expect("tool result should contain text content")
}

fn unique_temp_dir(label: &str) -> PathBuf {
    let nonce = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    env::temp_dir().join(format!("sc-{label}-{}-{nonce}", process::id()))
}
