#![forbid(unsafe_code)]

use std::{
    env, fs,
    io::{self, ErrorKind},
    net::Ipv4Addr,
    path::{Path, PathBuf},
    process,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use serde_json::Value;
use subc_core::{
    serve_listener, ControlHandler, ModuleProcessLiveness, ModuleSpec, Registry, RestartPolicy,
    Router, ServerAuth, Supervisor, SupervisorProcessLiveness,
};
use subc_transport::{
    generate_daemon_id, generate_key, write_atomic, ConnectionInfo, Endpoint, SCHEMA_VERSION,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpListener,
    process::{Child, ChildStdin, ChildStdout, Command},
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
            "subc-mcp-phase1",
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
    stdout: ChildStdout,
}

impl ShimProcess {
    fn close_stdin(&mut self) {
        drop(self.stdin.take());
    }
}

#[tokio::test]
async fn phase1_shim_module_round_trips_detaches_and_survives() {
    let server = TestServer::start().await;
    let events_path = server.daemon.temp_dir.join("fake-aft-events.jsonl");

    let provider = supervisor(&server)
        .spawn(stub_spec("fake-aft", &events_path))
        .unwrap();
    wait_for_registration(&server.daemon.registry, "fake-aft", READ_TIMEOUT).await;

    let module_connection_file = server.daemon.temp_dir.join("subc-mcp-connection.json");
    let mut module = spawn_module(&server.daemon.connection_file_path, &module_connection_file);
    wait_for_module_connection_file(&mut module, &module_connection_file, READ_TIMEOUT).await;

    let project = TestProject::new("subc-mcp-project");
    let mut shim = spawn_shim(&module_connection_file, &project.path);
    let response = shim_round_trip(&mut shim, b"phase1-roundtrip").await;
    assert_eq!(
        response, b"phase1-roundtrip",
        "shim stdout should carry the fake-aft-stub echo response"
    );

    let attach = wait_for_stub_event(&events_path, READ_TIMEOUT, |event| {
        event.get("kind") == Some(&Value::String("attach".to_owned()))
    })
    .await;
    let route_channel = attach
        .get("route_channel")
        .and_then(Value::as_u64)
        .expect("attach event should include route_channel");
    assert!(route_channel > 0, "attach should allocate a non-zero route");

    shim.close_stdin();
    let detach = wait_for_stub_event(&events_path, READ_TIMEOUT, |event| {
        event.get("kind") == Some(&Value::String("detach".to_owned()))
            && event.get("route_channel").and_then(Value::as_u64) == Some(route_channel)
    })
    .await;
    assert_eq!(
        detach.get("route_channel").and_then(Value::as_u64),
        Some(route_channel),
        "closing the shim should send per-route GOODBYE and detach the same route"
    );
    wait_for_shim_exit(shim).await;

    assert!(
        module.try_wait().unwrap().is_none(),
        "subc-mcp module should survive the first shim disconnect"
    );

    let mut second_shim = spawn_shim(&module_connection_file, &project.path);
    let second_response = shim_round_trip(&mut second_shim, b"phase1-second-roundtrip").await;
    assert_eq!(
        second_response, b"phase1-second-roundtrip",
        "a second shim should still round-trip through the same live module"
    );
    second_shim.close_stdin();
    wait_for_shim_exit(second_shim).await;

    assert!(
        module.try_wait().unwrap().is_none(),
        "subc-mcp module exited after serving the second shim"
    );

    module.start_kill().unwrap();
    let _ = timeout(READ_TIMEOUT, module.wait()).await;
    provider.stop().await.unwrap();
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

fn stub_spec(module_id: &str, events_path: &Path) -> ModuleSpec {
    let (program, args) = fake_aft_stub_command();
    ModuleSpec {
        module_id: module_id.to_owned(),
        program,
        args,
        env: vec![
            ("FAKE_AFT_MODULE_ID".to_owned(), module_id.to_owned()),
            (
                "FAKE_AFT_EVENTS_PATH".to_owned(),
                events_path.display().to_string(),
            ),
        ],
    }
}

fn fake_aft_stub_command() -> (PathBuf, Vec<String>) {
    if let Some(path) = option_env!("CARGO_BIN_EXE_fake-aft-stub") {
        return (PathBuf::from(path), Vec::new());
    }

    if let Ok(current_exe) = env::current_exe() {
        if let Some(target_debug) = current_exe.parent().and_then(Path::parent) {
            let candidate = target_debug.join(format!("fake-aft-stub{}", env::consts::EXE_SUFFIX));
            if candidate.exists() {
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
        stdout,
    }
}

async fn shim_round_trip(shim: &mut ShimProcess, payload: &[u8]) -> Vec<u8> {
    let stdin = shim.stdin.as_mut().expect("shim stdin should be open");
    write_len_prefixed(stdin, payload).await.unwrap();
    read_len_prefixed_timeout(&mut shim.stdout).await
}

async fn wait_for_shim_exit(mut shim: ShimProcess) {
    let status = timeout(READ_TIMEOUT, shim.child.wait())
        .await
        .expect("shim did not exit after stdin closed")
        .expect("failed to wait for shim exit");
    assert!(status.success(), "shim exited unsuccessfully: {status}");
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

async fn write_len_prefixed<W>(writer: &mut W, bytes: &[u8]) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let len = u32::try_from(bytes.len()).expect("test payload should fit in u32");
    writer.write_all(&len.to_le_bytes()).await?;
    writer.write_all(bytes).await?;
    writer.flush().await
}

async fn read_len_prefixed_timeout<R>(reader: &mut R) -> Vec<u8>
where
    R: AsyncRead + Unpin,
{
    timeout(READ_TIMEOUT, async {
        let mut len_bytes = [0u8; 4];
        reader.read_exact(&mut len_bytes).await?;
        let len = u32::from_le_bytes(len_bytes) as usize;
        let mut bytes = vec![0u8; len];
        if len > 0 {
            reader.read_exact(&mut bytes).await?;
        }
        io::Result::Ok(bytes)
    })
    .await
    .expect("timed out reading shim stdout frame")
    .expect("failed to read shim stdout frame")
}

fn unique_temp_dir(label: &str) -> PathBuf {
    let nonce = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    env::temp_dir().join(format!("sc-{label}-{}-{nonce}", process::id()))
}
