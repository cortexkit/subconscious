#![forbid(unsafe_code)]

use std::{
    collections::BTreeMap,
    env, fs,
    future::Future,
    io::ErrorKind,
    net::Ipv4Addr,
    path::{Path, PathBuf},
    process,
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use rmcp::{
    model::{
        CallToolRequest, CallToolRequestParams, CancelledNotificationParam, ClientCapabilities,
        ClientInfo, ClientRequest, CreateElicitationRequestParams, CreateElicitationResult,
        ElicitationAction, ErrorCode, ErrorData as McpErrorData, GetPromptRequestParams,
        Implementation, JsonObject, ProgressNotificationParam,
    },
    service::{
        MaybeSendFuture, NotificationContext, PeerRequestOptions, RunningService, ServiceError,
    },
    ClientHandler, RoleClient, ServiceExt,
};
use serde_json::{json, Value};
use subc_control::{CatalogEntry, ClientControlRequest, ClientControlResponse, SupervisorEntry};
use subc_core::{
    read_frame, serve_listener, write_frame, ControlHandler, ForwardingTable, Frame,
    ModuleProcessLiveness, ModuleSpec, Registry, RestartPolicy, Router, ServerAuth,
    SupervisedModule, Supervisor, SupervisorHandle, SupervisorProcessLiveness,
};
use subc_protocol::{
    manifest::{
        Bindings, Concurrency, ExecutionMode, IdentityBinding, IdentityScope, ModuleManifest,
        ProviderRole, StorageBinding, StorageKind, StorageScope, Tool as ProviderTool, TrustTier,
    },
    session::{HealthStatus as ControlHealthStatus, ModuleControlRequest, ModuleControlResponse},
    BindIdentity, ErrorBody, Flags, FrameType, ModuleHelloAckBody, ModuleHelloBody, Priority,
    RouteTarget, PROTOCOL_VERSION,
};
use subc_transport::{
    authenticate_client, authenticate_server, generate_daemon_id, generate_key, write_atomic,
    ConnectionInfo, Endpoint, SCHEMA_VERSION,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    process::{Child, ChildStderr, ChildStdin, ChildStdout, Command},
    sync::{mpsc, oneshot, Mutex as TokioMutex},
    task::JoinHandle,
    time::{sleep, timeout, Instant},
};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
const TEST_DAEMON_VER: &str = "test-subc-mcp";
// The module refuses to serve without daemon spawn attestation (SUBC_MODULE_ID +
// SUBC_LAUNCH_NONCE), so the tests spawn it exactly as the daemon would: env
// injected, nonce seeded into the supervisor handle for route.open verification.
const TEST_MCP_MODULE_ID: &str = "subc-mcp";
const TEST_MCP_LAUNCH_NONCE: &str = "test-mcp-launch-nonce";
// 30s, raised from 10s (2026-08-14): each test in the reverse-elicitation
// family spawns a real daemon + shim + module stub, and the default test
// parallelism runs eight such spawns at once. On a loaded host or CI runner the
// 10s budget expired inside wait_for_atomic_at_least while the chain was still
// warming -- 1-4 of the family failing per run, none under filtered single-test
// runs, and a code-identical CI flip from green to red as runner load moved.
// The budget prices the HOST, not the code under test; the waits settle in
// well under a second once the processes exist.
const SETUP_TIMEOUT: Duration = Duration::from_secs(30);
const READ_TIMEOUT: Duration = SETUP_TIMEOUT;
const QUIET_TIMEOUT: Duration = Duration::from_millis(750);
const NO_HANG_TIMEOUT: Duration = Duration::from_secs(2);
const TEST_SHIM_SCHEMA_VERSION: u32 = 1;
const TEST_MAX_SHIM_CONTROL_MESSAGE_LEN: u32 = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WireRoute {
    channel: u16,
    epoch: u32,
}

impl WireRoute {
    fn from_frame(frame: &Frame) -> Self {
        Self {
            channel: frame.header.channel,
            epoch: frame.header.epoch,
        }
    }
}

struct TestDaemon {
    registry: Arc<Registry>,
    forwarding: Arc<ForwardingTable>,
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
    supervisor_handle: SupervisorHandle,
}

impl TestServer {
    async fn start() -> Self {
        let process_liveness = Arc::new(SupervisorProcessLiveness::new());
        let supervisor_handle = SupervisorHandle::new();
        let daemon = start_test_daemon_with_process_liveness_and_supervisor(
            "subc-mcp-phase2b",
            Arc::clone(&process_liveness),
            supervisor_handle.clone(),
        )
        .await;
        // Authorize the attested module's route.opens: the daemon-injected nonce the
        // module echoes as consumer_identity must match a known spawn nonce.
        supervisor_handle.set_spawn_nonce(TEST_MCP_MODULE_ID, TEST_MCP_LAUNCH_NONCE.to_owned());
        Self {
            daemon,
            process_liveness,
            supervisor_handle,
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
    stderr: Option<ChildStderr>,
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
    progress_count: Arc<AtomicUsize>,
    tool_list_changed_count: Arc<AtomicUsize>,
    prompt_list_changed_count: Arc<AtomicUsize>,
}

impl TestMcpClient {
    fn new() -> Self {
        Self {
            progress: Arc::new(TokioMutex::new(Vec::new())),
            progress_count: Arc::new(AtomicUsize::new(0)),
            tool_list_changed_count: Arc::new(AtomicUsize::new(0)),
            prompt_list_changed_count: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Wait until `counter` advances past `baseline`, bounded by READ_TIMEOUT.
    ///
    /// Notifications are level-triggered counters (not edge-triggered `Notify`)
    /// so a notification delivered before this poll begins is never lost — the
    /// race that made the edge-triggered waits flake on Windows under load.
    async fn wait_for_counter(counter: &AtomicUsize, baseline: usize, label: &str) {
        let deadline = Instant::now() + READ_TIMEOUT;
        loop {
            if counter.load(Ordering::SeqCst) > baseline {
                return;
            }
            if Instant::now() >= deadline {
                panic!(
                    "timed out waiting for {label} (count still {}, baseline {baseline})",
                    counter.load(Ordering::SeqCst)
                );
            }
            sleep(Duration::from_millis(20)).await;
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
        let count = Arc::clone(&self.progress_count);
        async move {
            progress.lock().await.push(params);
            count.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn on_tool_list_changed(
        &self,
        _context: NotificationContext<RoleClient>,
    ) -> impl Future<Output = ()> + MaybeSendFuture + '_ {
        self.tool_list_changed_count.fetch_add(1, Ordering::SeqCst);
        std::future::ready(())
    }

    fn on_prompt_list_changed(
        &self,
        _context: NotificationContext<RoleClient>,
    ) -> impl Future<Output = ()> + MaybeSendFuture + '_ {
        self.prompt_list_changed_count
            .fetch_add(1, Ordering::SeqCst);
        std::future::ready(())
    }
}

struct StubProvider<'a> {
    module_id: &'a str,
    env: Vec<(&'a str, &'a str)>,
}

impl<'a> StubProvider<'a> {
    fn new(module_id: &'a str, env: &[(&'a str, &'a str)]) -> Self {
        Self {
            module_id,
            env: env.to_vec(),
        }
    }
}

#[derive(Clone, Copy)]
enum RawProviderBehavior {
    MalformedProgress,
    MalformedResult,
}

struct RawProvider {
    module_id: String,
    tool_name: String,
    route_cancel_count: Arc<AtomicUsize>,
    shutdown_tx: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
}

impl RawProvider {
    async fn start(
        connection_file_path: &Path,
        module_id: &str,
        tool_name: &str,
        behavior: RawProviderBehavior,
    ) -> Self {
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let connection_file_path = connection_file_path.to_path_buf();
        let module_id_owned = module_id.to_owned();
        let tool_name_owned = tool_name.to_owned();
        let task_module_id = module_id_owned.clone();
        let task_tool_name = tool_name_owned.clone();
        let route_cancel_count = Arc::new(AtomicUsize::new(0));
        let task_route_cancel_count = Arc::clone(&route_cancel_count);
        let task = tokio::spawn(async move {
            run_raw_provider(
                &connection_file_path,
                &task_module_id,
                &task_tool_name,
                behavior,
                task_route_cancel_count,
                shutdown_rx,
            )
            .await;
        });
        Self {
            module_id: module_id_owned,
            tool_name: tool_name_owned,
            route_cancel_count,
            shutdown_tx: Some(shutdown_tx),
            task,
        }
    }

    fn exposed_tool_name(&self) -> String {
        format!("{}_{}", self.module_id, self.tool_name)
    }

    fn route_cancel_count(&self) -> &AtomicUsize {
        self.route_cancel_count.as_ref()
    }

    async fn shutdown(mut self) {
        if let Some(shutdown_tx) = self.shutdown_tx.take() {
            let _ = shutdown_tx.send(());
        }
        match timeout(Duration::from_secs(2), &mut self.task).await {
            Ok(joined) => {
                let _ = joined;
            }
            Err(_) => {
                self.task.abort();
                let _ = self.task.await;
            }
        }
    }
}

struct RawProviderHarness {
    _server: TestServer,
    _project: TestProject,
    raw_provider: RawProvider,
    module: Child,
    shim: ShimProcess,
    client: RunningService<RoleClient, TestMcpClient>,
}

impl RawProviderHarness {
    async fn start(label: &str, behavior: RawProviderBehavior) -> Self {
        let server = TestServer::start().await;
        let raw_provider = RawProvider::start(
            &server.daemon.connection_file_path,
            "raw",
            "probe",
            behavior,
        )
        .await;
        wait_for_registration(&server.daemon.registry, "raw", READ_TIMEOUT).await;

        let user_config_home = server.daemon.temp_dir.join(format!("{label}-xdg-config"));
        fs::create_dir_all(&user_config_home).unwrap();

        let module_connection_file = server
            .daemon
            .temp_dir
            .join(format!("{label}-subc-mcp.json"));
        let mut module = spawn_module(
            &server.daemon.connection_file_path,
            &module_connection_file,
            &user_config_home,
        );
        wait_for_module_connection_file(&mut module, &module_connection_file, READ_TIMEOUT).await;

        let project = TestProject::new(label);
        let mut shim = spawn_shim(&module_connection_file, &project.path, &user_config_home);
        let client = shim.serve_mcp_client(TestMcpClient::new()).await;

        Self {
            _server: server,
            _project: project,
            raw_provider,
            module,
            shim,
            client,
        }
    }

    fn tool_name(&self) -> String {
        self.raw_provider.exposed_tool_name()
    }

    async fn shutdown(self) {
        let Self {
            _server,
            _project,
            raw_provider,
            mut module,
            mut shim,
            client,
        } = self;
        let _ = client.cancel().await;
        let _ = timeout(Duration::from_secs(2), shim.child.wait()).await;
        if module.try_wait().unwrap().is_none() {
            let _ = module.start_kill();
            let _ = timeout(Duration::from_secs(2), module.wait()).await;
        }
        raw_provider.shutdown().await;
    }
}

#[derive(Debug)]
enum ScriptedProviderEvent {
    Bound { route: WireRoute },
    RouteRequest(Frame),
    ReverseResponse { corr: u64, body: Value },
    ReverseError { corr: u64, body: Value },
    RouteCancel { corr: u64, claimed: bool },
    RouteGoodbye,
}

enum ScriptedProviderCommand {
    Send(Frame),
    Shutdown,
}

struct ScriptedProvider {
    module_id: String,
    tool_name: String,
    command_tx: mpsc::Sender<ScriptedProviderCommand>,
    events_rx: mpsc::Receiver<ScriptedProviderEvent>,
    task: JoinHandle<()>,
}

impl ScriptedProvider {
    async fn start(connection_file_path: &Path, module_id: &str, tool_name: &str) -> Self {
        let (command_tx, command_rx) = mpsc::channel(32);
        let (events_tx, events_rx) = mpsc::channel(64);
        let connection_file_path = connection_file_path.to_path_buf();
        let module_id_owned = module_id.to_owned();
        let tool_name_owned = tool_name.to_owned();
        let task_module_id = module_id_owned.clone();
        let task_tool_name = tool_name_owned.clone();
        let task = tokio::spawn(async move {
            run_scripted_provider(
                &connection_file_path,
                &task_module_id,
                &task_tool_name,
                command_rx,
                events_tx,
            )
            .await;
        });

        Self {
            module_id: module_id_owned,
            tool_name: tool_name_owned,
            command_tx,
            events_rx,
            task,
        }
    }

    fn exposed_tool_name(&self) -> String {
        format!("{}_{}", self.module_id, self.tool_name)
    }

    async fn shutdown(mut self) {
        let _ = self
            .command_tx
            .send(ScriptedProviderCommand::Shutdown)
            .await;
        match timeout(Duration::from_secs(2), &mut self.task).await {
            Ok(joined) => {
                let _ = joined;
            }
            Err(_) => {
                self.task.abort();
                let _ = self.task.await;
            }
        }
    }

    async fn wait_bound(&mut self) -> WireRoute {
        match self
            .wait_for_event("route bind", |event| {
                matches!(event, ScriptedProviderEvent::Bound { .. })
            })
            .await
        {
            ScriptedProviderEvent::Bound { route } => route,
            other => panic!("expected route bind event, got {other:?}"),
        }
    }

    async fn wait_route_request(&mut self) -> Frame {
        match self
            .wait_for_event("route request", |event| {
                matches!(event, ScriptedProviderEvent::RouteRequest(_))
            })
            .await
        {
            ScriptedProviderEvent::RouteRequest(frame) => frame,
            other => panic!("expected route request event, got {other:?}"),
        }
    }

    async fn wait_reverse_response(&mut self, corr: u64) -> Value {
        match self
            .wait_for_event("reverse response", |event| {
                matches!(event, ScriptedProviderEvent::ReverseResponse { corr: event_corr, .. } if *event_corr == corr)
            })
            .await
        {
            ScriptedProviderEvent::ReverseResponse { body, .. } => body,
            other => panic!("expected reverse response event, got {other:?}"),
        }
    }

    async fn wait_reverse_error(&mut self, corr: u64) -> Value {
        match self
            .wait_for_event("reverse error", |event| {
                matches!(event, ScriptedProviderEvent::ReverseError { corr: event_corr, .. } if *event_corr == corr)
            })
            .await
        {
            ScriptedProviderEvent::ReverseError { body, .. } => body,
            other => panic!("expected reverse error event, got {other:?}"),
        }
    }

    async fn wait_route_cancel(&mut self, corr: u64) -> bool {
        match self
            .wait_for_event("route cancel", |event| {
                matches!(event, ScriptedProviderEvent::RouteCancel { corr: event_corr, .. } if *event_corr == corr)
            })
            .await
        {
            ScriptedProviderEvent::RouteCancel { claimed, .. } => claimed,
            other => panic!("expected route cancel event, got {other:?}"),
        }
    }

    async fn wait_for_event<F>(&mut self, label: &str, mut matches: F) -> ScriptedProviderEvent
    where
        F: FnMut(&ScriptedProviderEvent) -> bool,
    {
        let deadline = Instant::now() + READ_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "timed out waiting for {label}");
            let event = timeout(remaining, self.events_rx.recv())
                .await
                .unwrap_or_else(|_| panic!("timed out waiting for {label}"))
                .expect("scripted provider event stream should stay open");
            if matches(&event) {
                return event;
            }
        }
    }

    async fn send_frame(&self, frame: Frame) {
        self.command_tx
            .send(ScriptedProviderCommand::Send(frame))
            .await
            .expect("scripted provider should accept outbound frame commands");
    }

    async fn send_reverse_request(&self, route: WireRoute, corr: u64, body: Value) {
        let body = serde_json::to_vec(&body).unwrap();
        self.send_frame(data_request(route, corr, &body)).await;
    }

    async fn send_route_response(&self, request: &Frame, body: Value) {
        let body = serde_json::to_vec(&body).unwrap();
        let frame = Frame::build_with_version(
            request.header.ver,
            FrameType::Response,
            request.header.flags,
            request.header.channel,
            request.header.epoch,
            request.header.corr,
            body,
        )
        .unwrap();
        self.send_frame(frame).await;
    }
}

#[derive(Clone)]
struct ReverseTestClient {
    capabilities: ClientCapabilities,
    prompt_count: Arc<AtomicUsize>,
    cancel_count: Arc<AtomicUsize>,
    response_tx: mpsc::Sender<std::result::Result<CreateElicitationResult, McpErrorData>>,
    response_rx:
        Arc<TokioMutex<mpsc::Receiver<std::result::Result<CreateElicitationResult, McpErrorData>>>>,
}

impl ReverseTestClient {
    fn new(capabilities: ClientCapabilities) -> Self {
        let (response_tx, response_rx) = mpsc::channel(16);
        Self {
            capabilities,
            prompt_count: Arc::new(AtomicUsize::new(0)),
            cancel_count: Arc::new(AtomicUsize::new(0)),
            response_tx,
            response_rx: Arc::new(TokioMutex::new(response_rx)),
        }
    }

    async fn respond_elicitation(&self, result: CreateElicitationResult) {
        self.response_tx
            .send(Ok(result))
            .await
            .expect("reverse test client should accept elicitation responses");
    }

    async fn wait_for_prompts(&self, expected: usize) {
        wait_for_atomic_at_least(&self.prompt_count, expected, "elicitation prompts").await;
    }

    async fn wait_for_cancellations(&self, expected: usize) {
        wait_for_atomic_at_least(&self.cancel_count, expected, "MCP request cancellations").await;
    }

    async fn assert_prompt_count_stays(&self, expected: usize) {
        assert_counter_stays(
            &self.prompt_count,
            expected,
            "elicitation prompts",
            QUIET_TIMEOUT,
        )
        .await;
    }
}

impl ClientHandler for ReverseTestClient {
    fn get_info(&self) -> ClientInfo {
        ClientInfo::new(
            self.capabilities.clone(),
            Implementation::new("subc-mcp-reverse-test", "0"),
        )
    }

    fn create_elicitation(
        &self,
        _request: CreateElicitationRequestParams,
        context: rmcp::service::RequestContext<RoleClient>,
    ) -> impl Future<Output = std::result::Result<CreateElicitationResult, McpErrorData>>
           + MaybeSendFuture
           + '_ {
        let prompt_count = Arc::clone(&self.prompt_count);
        let response_rx = Arc::clone(&self.response_rx);
        async move {
            prompt_count.fetch_add(1, Ordering::SeqCst);
            tokio::select! {
                response = async {
                    let mut response_rx = response_rx.lock().await;
                    response_rx.recv().await
                } => {
                    response.unwrap_or_else(|| {
                        Ok(CreateElicitationResult::new(ElicitationAction::Decline))
                    })
                }
                _ = context.ct.cancelled() => Err(McpErrorData::internal_error(
                    "elicitation request was cancelled by the server",
                    None,
                )),
            }
        }
    }

    fn on_cancelled(
        &self,
        _params: CancelledNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) -> impl Future<Output = ()> + MaybeSendFuture + '_ {
        self.cancel_count.fetch_add(1, Ordering::SeqCst);
        std::future::ready(())
    }
}

struct ReverseHarness {
    _server: TestServer,
    _project: TestProject,
    provider: ScriptedProvider,
    module: Child,
    shim: ShimProcess,
    client: Option<RunningService<RoleClient, ReverseTestClient>>,
    client_handler: ReverseTestClient,
}

impl ReverseHarness {
    async fn start(label: &str, capabilities: ClientCapabilities) -> Self {
        Self::start_with_module_env(label, capabilities, &[]).await
    }

    async fn start_with_module_env(
        label: &str,
        capabilities: ClientCapabilities,
        module_env: &[(&str, &str)],
    ) -> Self {
        Self::start_inner(label, capabilities, module_env, true).await
    }

    async fn start_attach_window(label: &str) -> Self {
        Self::start_inner(label, ClientCapabilities::default(), &[], false).await
    }

    async fn start_inner(
        label: &str,
        capabilities: ClientCapabilities,
        module_env: &[(&str, &str)],
        serve_client: bool,
    ) -> Self {
        let server = TestServer::start().await;
        let provider =
            ScriptedProvider::start(&server.daemon.connection_file_path, "raw", "probe").await;
        wait_for_registration(&server.daemon.registry, "raw", READ_TIMEOUT).await;

        let user_config_home = server.daemon.temp_dir.join(format!("{label}-xdg-config"));
        fs::create_dir_all(&user_config_home).unwrap();
        let module_connection_file = server
            .daemon
            .temp_dir
            .join(format!("{label}-subc-mcp.json"));
        let mut module = spawn_module_with_extra_env(
            &server.daemon.connection_file_path,
            &module_connection_file,
            &user_config_home,
            module_env,
        );
        wait_for_module_connection_file(&mut module, &module_connection_file, READ_TIMEOUT).await;

        let project = TestProject::new(label);
        let mut shim = spawn_shim(&module_connection_file, &project.path, &user_config_home);
        let client_handler = ReverseTestClient::new(capabilities);
        let client = if serve_client {
            Some(shim.serve_mcp_client(client_handler.clone()).await)
        } else {
            None
        };

        Self {
            _server: server,
            _project: project,
            provider,
            module,
            shim,
            client,
            client_handler,
        }
    }

    async fn stop_client(&mut self) {
        if let Some(client) = self.client.take() {
            let _ = client.cancel().await;
        }
        let _ = timeout(Duration::from_secs(2), self.shim.child.wait()).await;
    }

    async fn shutdown(mut self) {
        if let Some(client) = self.client.take() {
            let _ = client.cancel().await;
        }
        let _ = timeout(Duration::from_secs(2), self.shim.child.wait()).await;
        if self.module.try_wait().unwrap().is_none() {
            let _ = self.module.start_kill();
            let _ = timeout(Duration::from_secs(2), self.module.wait()).await;
        }
        self.provider.shutdown().await;
    }
}

async fn run_scripted_provider(
    connection_file_path: &Path,
    module_id: &str,
    tool_name: &str,
    mut command_rx: mpsc::Receiver<ScriptedProviderCommand>,
    events_tx: mpsc::Sender<ScriptedProviderEvent>,
) {
    let mut stream = connect_control_client(connection_file_path)
        .await
        .expect("scripted test provider should authenticate to the daemon");
    let hello = ModuleHelloBody {
        manifest: raw_provider_manifest(module_id, tool_name),
        protocol_ver: PROTOCOL_VERSION,
        control_ops: None,
        launch_nonce: None,
    };
    let body = serde_json::to_vec(&hello).unwrap();
    let frame = Frame::build(
        FrameType::Hello,
        Flags::new(false, Priority::Passive, false),
        0,
        0,
        1,
        body,
    )
    .unwrap();
    write_frame(&mut stream, &frame).await.unwrap();
    stream.flush().await.unwrap();

    let ack = read_frame_timeout(&mut stream).await;
    assert_eq!(ack.header.ty, FrameType::HelloAck);
    let _ack: ModuleHelloAckBody = serde_json::from_slice(&ack.body).unwrap();

    let mut route = None;
    loop {
        tokio::select! {
            command = command_rx.recv() => {
                match command {
                    Some(ScriptedProviderCommand::Send(frame)) => {
                        if frame.header.channel != 0 {
                            let installed = route.expect(
                                "route egress must not enter the writer queue before RouteBind ack",
                            );
                            assert_eq!(
                                WireRoute::from_frame(&frame),
                                installed,
                                "route egress must use the handle installed after RouteBind ack",
                            );
                        }
                        write_frame(&mut stream, &frame).await.unwrap();
                        stream.flush().await.unwrap();
                    }
                    Some(ScriptedProviderCommand::Shutdown) | None => return,
                }
            }
            frame = read_frame(&mut stream) => {
                let Some(frame) = frame.unwrap() else {
                    return;
                };
                if frame.header.channel != 0
                    && route != Some(WireRoute::from_frame(&frame))
                {
                    continue;
                }
                match frame.header.ty {
                    FrameType::Request if frame.header.channel == 0 => {
                        let request: ModuleControlRequest = serde_json::from_slice(&frame.body).unwrap();
                        match request {
                            ModuleControlRequest::RouteBind {
                                route_channel,
                                epoch,
                                ..
                            } => {
                                let next_route = WireRoute {
                                    channel: route_channel,
                                    epoch,
                                };
                                let body = serde_json::to_vec(
                                    &ModuleControlResponse::RouteBindAck {},
                                )
                                .unwrap();
                                let response = Frame::build_with_version(
                                    frame.header.ver,
                                    FrameType::Response,
                                    Flags::new(false, Priority::Passive, false),
                                    0,
                                    0,
                                    frame.header.corr,
                                    body,
                                )
                                .unwrap();
                                write_frame(&mut stream, &response).await.unwrap();
                                stream.flush().await.unwrap();
                                route = Some(next_route);
                                let _ = events_tx
                                    .send(ScriptedProviderEvent::Bound { route: next_route })
                                    .await;
                            }
                            ModuleControlRequest::HealthCheck {} => {}
                        }
                    }
                    FrameType::Request if route == Some(WireRoute::from_frame(&frame)) => {
                        let _ = events_tx
                            .send(ScriptedProviderEvent::RouteRequest(frame))
                            .await;
                    }
                    FrameType::Response if route == Some(WireRoute::from_frame(&frame)) => {
                        let body = serde_json::from_slice::<Value>(&frame.body)
                            .unwrap_or(Value::Null);
                        let _ = events_tx
                            .send(ScriptedProviderEvent::ReverseResponse {
                                corr: frame.header.corr,
                                body,
                            })
                            .await;
                    }
                    FrameType::Error if route == Some(WireRoute::from_frame(&frame)) => {
                        let body = serde_json::from_slice::<Value>(&frame.body)
                            .unwrap_or(Value::Null);
                        let _ = events_tx
                            .send(ScriptedProviderEvent::ReverseError {
                                corr: frame.header.corr,
                                body,
                            })
                            .await;
                    }
                    FrameType::Cancel if route == Some(WireRoute::from_frame(&frame)) => {
                        let _ = events_tx
                            .send(ScriptedProviderEvent::RouteCancel {
                                corr: frame.header.corr,
                                claimed: true,
                            })
                            .await;
                    }
                    FrameType::Goodbye if route == Some(WireRoute::from_frame(&frame)) => {
                        let _ = events_tx.send(ScriptedProviderEvent::RouteGoodbye).await;
                        return;
                    }
                    FrameType::Goodbye if frame.header.channel == 0 => return,
                    _ => {}
                }
            }
        }
    }
}

fn elicitation_capabilities() -> ClientCapabilities {
    let mut capabilities = ClientCapabilities::default();
    capabilities.elicitation = Some(Default::default());
    capabilities
}

fn elicitation_body(label: &str) -> Value {
    json!({
        "method": "elicitation/create",
        "params": {
            "mode": "url",
            "message": format!("approve {label}"),
            "url": "https://example.invalid/approve",
            "elicitationId": label,
        }
    })
}

fn accepted_elicitation(content: Value) -> CreateElicitationResult {
    CreateElicitationResult::new(ElicitationAction::Accept).with_content(content)
}

async fn write_raw_mcp_message(writer: &mut ChildStdin, message: &Value) {
    let mut encoded = serde_json::to_vec(message).expect("raw MCP message should encode");
    encoded.push(b'\n');
    writer
        .write_all(&encoded)
        .await
        .expect("raw MCP message should write");
    writer.flush().await.expect("raw MCP message should flush");
}

async fn read_raw_mcp_message(reader: &mut ChildStdout) -> Value {
    let encoded = timeout(READ_TIMEOUT, async {
        let mut encoded = Vec::new();
        loop {
            let mut byte = [0_u8; 1];
            reader.read_exact(&mut byte).await?;
            if byte[0] == b'\n' {
                return Ok::<_, std::io::Error>(encoded);
            }
            encoded.push(byte[0]);
        }
    })
    .await
    .expect("timed out waiting for raw MCP message")
    .expect("raw MCP message should read");
    serde_json::from_slice(&encoded).expect("raw MCP message should decode")
}

async fn wait_for_atomic_at_least(counter: &AtomicUsize, expected: usize, label: &str) {
    let deadline = Instant::now() + READ_TIMEOUT;
    loop {
        let observed = counter.load(Ordering::SeqCst);
        if observed >= expected {
            return;
        }
        // The counts are in the message because a bare timeout cannot be
        // triaged: zero-observed means the chain never warmed, while
        // observed-one-short means it works and lost a race.
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {label}: observed {observed}, expected at least {expected}"
        );
        sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn mcp_reverse_elicitation_declared_host_round_trips() {
    let mut harness =
        ReverseHarness::start("mcp-reverse-roundtrip", elicitation_capabilities()).await;
    let route_channel = harness.provider.wait_bound().await;

    harness
        .provider
        .send_reverse_request(route_channel, 901, elicitation_body("roundtrip"))
        .await;
    harness.client_handler.wait_for_prompts(1).await;
    harness
        .client_handler
        .respond_elicitation(accepted_elicitation(json!({ "approved": true })))
        .await;

    let response = harness.provider.wait_reverse_response(901).await;
    assert_eq!(
        response.get("action"),
        Some(&Value::String("accept".to_owned()))
    );
    assert_eq!(
        response.pointer("/content/approved"),
        Some(&Value::Bool(true))
    );

    harness.shutdown().await;
}

#[tokio::test]
async fn mcp_initialize_publishes_reverse_peer_before_initialized_notification() {
    let mut harness = ReverseHarness::start_attach_window("mcp-reverse-initialize-boundary").await;
    let route = harness.provider.wait_bound().await;

    let initialize = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-11-25",
            "capabilities": { "elicitation": {} },
            "clientInfo": { "name": "subc-mcp-raw-initialize-test", "version": "0" }
        }
    });
    write_raw_mcp_message(
        harness
            .shim
            .stdin
            .as_mut()
            .expect("shim stdin should remain available"),
        &initialize,
    )
    .await;
    let initialize_response = read_raw_mcp_message(
        harness
            .shim
            .stdout
            .as_mut()
            .expect("shim stdout should remain available"),
    )
    .await;
    assert_eq!(initialize_response.get("id"), Some(&json!(1)));
    assert!(initialize_response.get("result").is_some());

    let mut raw_stdin = harness
        .shim
        .stdin
        .take()
        .expect("shim stdin should remain available");
    let mut raw_stdout = harness
        .shim
        .stdout
        .take()
        .expect("shim stdout should remain available");
    let prompt_count = Arc::new(AtomicUsize::new(0));
    let client_prompt_count = Arc::clone(&prompt_count);
    let raw_client = tokio::spawn(async move {
        let prompt = read_raw_mcp_message(&mut raw_stdout).await;
        assert_eq!(prompt.get("method"), Some(&json!("elicitation/create")));
        client_prompt_count.fetch_add(1, Ordering::SeqCst);
        let response = json!({
            "jsonrpc": "2.0",
            "id": prompt.get("id").expect("elicitation request should carry an id"),
            "result": {
                "action": "accept",
                "content": { "initializeBoundary": true }
            }
        });
        write_raw_mcp_message(&mut raw_stdin, &response).await;
        (raw_stdin, raw_stdout)
    });

    harness
        .provider
        .send_reverse_request(route, 907, elicitation_body("initialize-boundary"))
        .await;
    let (raw_stdin, raw_stdout) = tokio::select! {
        result = raw_client => result.expect("raw MCP client should handle elicitation"),
        error = harness.provider.wait_reverse_error(907) => {
            panic!("reverse request was rejected after initialize instead of reaching the host: {error}")
        }
    };
    wait_for_atomic_at_least(&prompt_count, 1, "pre-initialized elicitation prompts").await;
    harness.shim.stdin = Some(raw_stdin);
    harness.shim.stdout = Some(raw_stdout);

    let provider_response = harness.provider.wait_reverse_response(907).await;
    assert_eq!(
        provider_response.pointer("/content/initializeBoundary"),
        Some(&Value::Bool(true))
    );

    write_raw_mcp_message(
        harness
            .shim
            .stdin
            .as_mut()
            .expect("shim stdin should remain available"),
        &json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
    )
    .await;
    drop(harness.shim.stdin.take());
    harness.shutdown().await;
}

#[tokio::test]
async fn mcp_reverse_elicitation_absent_capability_and_attach_window_fail_fast() {
    let mut absent =
        ReverseHarness::start("mcp-reverse-no-cap", ClientCapabilities::default()).await;
    let absent_route = absent.provider.wait_bound().await;
    absent
        .provider
        .send_reverse_request(absent_route, 902, elicitation_body("no-cap"))
        .await;
    let error = absent.provider.wait_reverse_error(902).await;
    assert_eq!(error.get("code"), Some(&json!(-32601)));
    absent.client_handler.assert_prompt_count_stays(0).await;
    absent.shutdown().await;

    let mut window = ReverseHarness::start_attach_window("mcp-reverse-attach-window").await;
    let window_route = window.provider.wait_bound().await;
    sleep(Duration::from_millis(50)).await;
    window
        .provider
        .send_reverse_request(window_route, 903, elicitation_body("before-init"))
        .await;
    let error = window.provider.wait_reverse_error(903).await;
    assert_eq!(error.get("code"), Some(&json!(-32601)));
    window.client_handler.assert_prompt_count_stays(0).await;
    window.shutdown().await;
}

#[tokio::test]
async fn mcp_reverse_elicitation_corr_collision_does_not_poison_forward_call() {
    let mut harness =
        ReverseHarness::start("mcp-reverse-corr-collision", elicitation_capabilities()).await;
    let _route_channel = harness.provider.wait_bound().await;
    let tool_name = harness.provider.exposed_tool_name();
    let peer = harness.client.as_ref().unwrap().peer().clone();
    let call =
        tokio::spawn(async move { peer.call_tool(CallToolRequestParams::new(tool_name)).await });

    let forward_request = harness.provider.wait_route_request().await;
    let colliding_corr = forward_request.header.corr;
    harness
        .provider
        .send_reverse_request(
            WireRoute::from_frame(&forward_request),
            colliding_corr,
            elicitation_body("collision"),
        )
        .await;
    harness.client_handler.wait_for_prompts(1).await;
    harness
        .client_handler
        .respond_elicitation(accepted_elicitation(json!({ "collision": "ok" })))
        .await;
    let reverse_response = harness.provider.wait_reverse_response(colliding_corr).await;
    assert_eq!(
        reverse_response.pointer("/content/collision"),
        Some(&Value::String("ok".to_owned()))
    );

    harness
        .provider
        .send_route_response(
            &forward_request,
            json!({
                "content": [{ "type": "text", "text": "forward-ok" }],
                "isError": false,
            }),
        )
        .await;
    let result = call.await.unwrap().unwrap();
    assert_eq!(result_text(&result), "forward-ok");

    harness.shutdown().await;
}

#[tokio::test]
async fn mcp_reverse_elicitation_duplicate_corr_is_ignored_while_pending() {
    let mut harness =
        ReverseHarness::start("mcp-reverse-duplicate", elicitation_capabilities()).await;
    let route_channel = harness.provider.wait_bound().await;

    harness
        .provider
        .send_reverse_request(route_channel, 904, elicitation_body("duplicate"))
        .await;
    harness.client_handler.wait_for_prompts(1).await;
    harness
        .provider
        .send_reverse_request(route_channel, 904, elicitation_body("duplicate-again"))
        .await;
    harness.client_handler.assert_prompt_count_stays(1).await;
    harness
        .client_handler
        .respond_elicitation(accepted_elicitation(json!({ "deduped": true })))
        .await;
    let response = harness.provider.wait_reverse_response(904).await;
    assert_eq!(
        response.pointer("/content/deduped"),
        Some(&Value::Bool(true))
    );

    harness.shutdown().await;
}

#[tokio::test]
async fn mcp_reverse_elicitation_shim_death_settles_pending_with_error() {
    let mut harness =
        ReverseHarness::start("mcp-reverse-shim-death", elicitation_capabilities()).await;
    let route_channel = harness.provider.wait_bound().await;

    harness
        .provider
        .send_reverse_request(route_channel, 905, elicitation_body("shim-death"))
        .await;
    harness.client_handler.wait_for_prompts(1).await;
    harness.stop_client().await;
    let error = harness.provider.wait_reverse_error(905).await;
    assert_eq!(error.get("code"), Some(&json!(-32603)));

    harness.shutdown().await;
}

#[tokio::test]
async fn mcp_reverse_elicitation_forward_cancel_settles_prompt_and_provider() {
    let mut harness =
        ReverseHarness::start("mcp-reverse-forward-cancel", elicitation_capabilities()).await;
    let _route_channel = harness.provider.wait_bound().await;
    let tool_name = harness.provider.exposed_tool_name();
    let peer = harness.client.as_ref().unwrap().peer().clone();
    let handle = peer
        .send_cancellable_request(
            ClientRequest::CallToolRequest(CallToolRequest::new(CallToolRequestParams::new(
                tool_name,
            ))),
            PeerRequestOptions::no_options(),
        )
        .await
        .unwrap();
    let request_id = handle.id.clone();

    let forward_request = harness.provider.wait_route_request().await;
    harness
        .provider
        .send_reverse_request(
            WireRoute::from_frame(&forward_request),
            906,
            elicitation_body("forward-cancel"),
        )
        .await;
    harness.client_handler.wait_for_prompts(1).await;

    peer.notify_cancelled(CancelledNotificationParam {
        request_id,
        reason: Some("test cancellation".to_owned()),
    })
    .await
    .unwrap();

    let reverse_error = harness.provider.wait_reverse_error(906).await;
    assert_eq!(reverse_error.get("code"), Some(&json!(-32603)));
    assert!(
        harness
            .provider
            .wait_route_cancel(forward_request.header.corr)
            .await
    );
    harness.client_handler.wait_for_cancellations(1).await;
    let err = handle.await_response().await.unwrap_err();
    match err {
        ServiceError::Cancelled { .. } => {}
        ServiceError::McpError(error) if error.message.contains("cancelled") => {}
        other => panic!("expected cancelled call result, got {other:?}"),
    }

    harness.shutdown().await;
}

#[tokio::test]
async fn mcp_reverse_elicitation_pending_cap_errors_ninth_request() {
    let mut harness = ReverseHarness::start("mcp-reverse-cap", elicitation_capabilities()).await;
    let route_channel = harness.provider.wait_bound().await;

    for corr in 1_000..=1_008 {
        harness
            .provider
            .send_reverse_request(
                route_channel,
                corr,
                elicitation_body(&format!("cap-{corr}")),
            )
            .await;
    }

    let error = harness.provider.wait_reverse_error(1_008).await;
    assert_eq!(error.get("code"), Some(&json!(-32603)));

    harness.shutdown().await;
}

#[tokio::test]
async fn mcp_reverse_elicitation_ttl_expires_one_pending_without_disturbing_another() {
    let mut harness = ReverseHarness::start_with_module_env(
        "mcp-reverse-ttl",
        elicitation_capabilities(),
        &[("SUBC_MCP_REVERSE_RELAY_TTL_MS", "300")],
    )
    .await;
    let route_channel = harness.provider.wait_bound().await;

    harness
        .provider
        .send_reverse_request(route_channel, 2_001, elicitation_body("ttl-expired"))
        .await;
    harness.client_handler.wait_for_prompts(1).await;
    sleep(Duration::from_millis(150)).await;
    harness
        .provider
        .send_reverse_request(route_channel, 2_002, elicitation_body("ttl-survivor"))
        .await;
    harness.client_handler.wait_for_prompts(2).await;
    harness.client_handler.wait_for_cancellations(1).await;
    harness
        .client_handler
        .respond_elicitation(accepted_elicitation(json!({ "survived": true })))
        .await;
    let response = harness.provider.wait_reverse_response(2_002).await;
    assert_eq!(
        response.pointer("/content/survived"),
        Some(&Value::Bool(true))
    );

    harness.shutdown().await;
}

struct McpHarness {
    server: TestServer,
    _project: TestProject,
    providers: BTreeMap<String, SupervisedModule>,
    module: Child,
    shim: ShimProcess,
    client: RunningService<RoleClient, TestMcpClient>,
    client_handler: TestMcpClient,
    events_path: PathBuf,
    provider_events: BTreeMap<String, PathBuf>,
}

impl McpHarness {
    async fn start(label: &str, provider_env: &[(&str, &str)]) -> Self {
        Self::start_configured(
            label,
            vec![StubProvider::new("fake-aft", provider_env)],
            None,
            None,
        )
        .await
    }

    async fn start_configured(
        label: &str,
        provider_specs: Vec<StubProvider<'_>>,
        user_config: Option<&str>,
        project_config: Option<&str>,
    ) -> Self {
        let server = TestServer::start().await;
        let mut providers = BTreeMap::new();
        let mut provider_events = BTreeMap::new();

        for provider_spec in provider_specs {
            let events_path = server
                .daemon
                .temp_dir
                .join(format!("{label}-{}-events.jsonl", provider_spec.module_id));
            let provider = supervisor(&server)
                .spawn(stub_spec(
                    provider_spec.module_id,
                    &events_path,
                    &provider_spec.env,
                ))
                .unwrap();
            wait_for_registration(
                &server.daemon.registry,
                provider_spec.module_id,
                READ_TIMEOUT,
            )
            .await;
            provider_events.insert(provider_spec.module_id.to_owned(), events_path);
            providers.insert(provider_spec.module_id.to_owned(), provider);
        }

        let user_config_home = server.daemon.temp_dir.join(format!("{label}-xdg-config"));
        fs::create_dir_all(&user_config_home).unwrap();
        if let Some(user_config) = user_config {
            write_user_mcp_config(&user_config_home, user_config);
        }

        let module_connection_file = server
            .daemon
            .temp_dir
            .join(format!("{label}-subc-mcp.json"));
        let mut module = spawn_module(
            &server.daemon.connection_file_path,
            &module_connection_file,
            &user_config_home,
        );
        wait_for_module_connection_file(&mut module, &module_connection_file, READ_TIMEOUT).await;

        let project = TestProject::new(label);
        if let Some(project_config) = project_config {
            write_project_mcp_config(&project.path, project_config);
        }

        let mut shim = spawn_shim(&module_connection_file, &project.path, &user_config_home);
        let client_handler = TestMcpClient::new();
        let client = shim.serve_mcp_client(client_handler.clone()).await;
        let events_path = provider_events
            .values()
            .next()
            .cloned()
            .expect("test harness should have at least one provider");

        Self {
            server,
            _project: project,
            providers,
            module,
            shim,
            client,
            client_handler,
            events_path,
            provider_events,
        }
    }

    fn provider_events_path(&self, module_id: &str) -> &Path {
        self.provider_events
            .get(module_id)
            .unwrap_or_else(|| panic!("missing provider events path for {module_id}"))
    }

    async fn spawn_provider(&mut self, module_id: &str, provider_env: &[(&str, &str)]) {
        let events_path = self
            .server
            .daemon
            .temp_dir
            .join(format!("dynamic-{module_id}-events.jsonl"));
        let provider = supervisor(&self.server)
            .spawn(stub_spec(module_id, &events_path, provider_env))
            .unwrap();
        wait_for_registration(&self.server.daemon.registry, module_id, READ_TIMEOUT).await;
        self.provider_events
            .insert(module_id.to_owned(), events_path);
        self.providers.insert(module_id.to_owned(), provider);
    }

    async fn shutdown(self) {
        let Self {
            server: _server,
            _project,
            providers,
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
        for provider in providers.into_values() {
            provider.stop().await.unwrap();
        }
    }
}

#[tokio::test]
async fn mcp_initialize_advertises_tools_and_prompts_capabilities() {
    let harness = McpHarness::start("mcp-init", &[]).await;

    let server_info = harness
        .client
        .peer()
        .peer_info()
        .expect("client should store initialize result");
    let tools_capability = server_info
        .capabilities
        .tools
        .as_ref()
        .expect("subc-mcp should advertise the MCP tools capability");
    assert_eq!(tools_capability.list_changed, Some(true));
    let prompts_capability = server_info
        .capabilities
        .prompts
        .as_ref()
        .expect("subc-mcp should advertise the MCP prompts capability");
    assert_eq!(prompts_capability.list_changed, Some(true));
    assert_eq!(server_info.server_info.name, "subc-mcp");

    harness.shutdown().await;
}

#[tokio::test]
async fn mcp_prompts_are_hidden_by_default() {
    let harness = McpHarness::start("mcp-prompts-hidden", &[]).await;

    let result = harness.client.peer().list_prompts(None).await.unwrap();
    assert_eq!(
        serde_json::to_value(result).unwrap(),
        json!({ "prompts": [] })
    );

    harness.shutdown().await;
}

#[tokio::test]
async fn mcp_prompt_policy_refresh_is_proactive_inert_and_tool_neutral() {
    let user_config = r#"{
        "version": 1,
        "refresh": "immediate",
        "prompts": { "defaultEnabled": true }
    }"#;
    let hidden_project_config = r#"{
        "version": 1,
        "prompts": { "defaultEnabled": false }
    }"#;
    let enabled_project_config = r#"{
        "version": 1,
        "prompts": { "defaultEnabled": true }
    }"#;
    let harness = McpHarness::start_configured(
        "mcp-prompts-refresh",
        vec![StubProvider::new("fake-aft", &[])],
        Some(user_config),
        Some(hidden_project_config),
    )
    .await;
    let peer = harness.client.peer();
    let tools_before = serde_json::to_vec(&peer.list_tools(None).await.unwrap()).unwrap();
    let tool_notification_baseline = harness
        .client_handler
        .tool_list_changed_count
        .load(Ordering::SeqCst);
    let prompt_notification_baseline = harness
        .client_handler
        .prompt_list_changed_count
        .load(Ordering::SeqCst);

    assert!(peer.list_prompts(None).await.unwrap().prompts.is_empty());
    write_project_mcp_config(&harness._project.path, enabled_project_config);
    TestMcpClient::wait_for_counter(
        &harness.client_handler.prompt_list_changed_count,
        prompt_notification_baseline,
        "proactive prompt list activation notification without an MCP request",
    )
    .await;
    let activated = peer.list_prompts(None).await.unwrap();
    assert_eq!(
        serde_json::to_value(activated).unwrap(),
        json!({
            "prompts": [
                {
                    "name": "status",
                    "description": "Summarize the current conversation state from Magic Context."
                },
                {
                    "name": "wrapup",
                    "description": "Wrap up this conversation: fold history and keep only the most recent messages.",
                    "arguments": [
                        {
                            "name": "keep",
                            "description": "number of recent messages to keep (5-100, default 20)",
                            "required": false
                        }
                    ]
                }
            ]
        })
    );
    assert_eq!(
        serde_json::to_vec(&peer.list_tools(None).await.unwrap()).unwrap(),
        tools_before
    );

    let after_activation = harness
        .client_handler
        .prompt_list_changed_count
        .load(Ordering::SeqCst);
    assert_eq!(after_activation, prompt_notification_baseline + 1);
    write_project_mcp_config(&harness._project.path, hidden_project_config);
    TestMcpClient::wait_for_counter(
        &harness.client_handler.prompt_list_changed_count,
        after_activation,
        "proactive prompt list re-hide notification without an MCP request",
    )
    .await;
    assert_mcp_error(
        peer.get_prompt(GetPromptRequestParams::new("status"))
            .await
            .unwrap_err(),
        ErrorCode::INVALID_PARAMS,
        "unknown prompt 'status'",
    );
    assert_eq!(
        harness
            .client_handler
            .prompt_list_changed_count
            .load(Ordering::SeqCst),
        after_activation + 1
    );
    assert!(peer.list_prompts(None).await.unwrap().prompts.is_empty());
    assert_eq!(
        serde_json::to_vec(&peer.list_tools(None).await.unwrap()).unwrap(),
        tools_before
    );
    assert_counter_stays(
        &harness.client_handler.tool_list_changed_count,
        tool_notification_baseline,
        "tool list notifications during prompt-only policy refresh",
        QUIET_TIMEOUT,
    )
    .await;

    harness.shutdown().await;
}

#[tokio::test]
async fn mcp_prompts_get_validation_and_unavailable_backend_errors_are_clean() {
    let harness = McpHarness::start_configured(
        "mcp-prompts-get",
        vec![StubProvider::new("fake-aft", &[])],
        Some(r#"{ "version": 1, "prompts": { "defaultEnabled": true } }"#),
        None,
    )
    .await;
    let peer = harness.client.peer();

    assert_mcp_error(
        peer.get_prompt(GetPromptRequestParams::new("missing"))
            .await
            .unwrap_err(),
        ErrorCode::INVALID_PARAMS,
        "unknown prompt 'missing'",
    );

    assert_mcp_error(
        peer.get_prompt(GetPromptRequestParams::new("status"))
            .await
            .unwrap_err(),
        ErrorCode(-32000),
        "prompt backend is temporarily unavailable; try again shortly",
    );

    for keep in [None, Some("-2"), Some("4"), Some("500")] {
        let mut request = GetPromptRequestParams::new("wrapup");
        if let Some(keep) = keep {
            let mut arguments = JsonObject::new();
            arguments.insert("keep".to_owned(), json!(keep));
            request = request.with_arguments(arguments);
        }
        assert_mcp_error(
            peer.get_prompt(request).await.unwrap_err(),
            ErrorCode(-32000),
            "prompt backend is temporarily unavailable; try again shortly",
        );
    }

    for keep in ["abc", "9223372036854775808"] {
        let mut arguments = JsonObject::new();
        arguments.insert("keep".to_owned(), json!(keep));
        assert_mcp_error(
            peer.get_prompt(GetPromptRequestParams::new("wrapup").with_arguments(arguments))
                .await
                .unwrap_err(),
            ErrorCode::INVALID_PARAMS,
            "keep must be an integer",
        );
    }

    let mut arguments = JsonObject::new();
    arguments.insert("recent".to_owned(), json!("20"));
    assert_mcp_error(
        peer.get_prompt(GetPromptRequestParams::new("wrapup").with_arguments(arguments))
            .await
            .unwrap_err(),
        ErrorCode::INVALID_PARAMS,
        "unknown argument 'recent' for prompt 'wrapup'",
    );

    harness.shutdown().await;
}

#[tokio::test]
async fn mcp_tools_list_returns_stub_manifest_tools() {
    let harness = McpHarness::start("mcp-list", &[]).await;

    let tools = harness.client.peer().list_tools(None).await.unwrap();
    assert_eq!(tools.tools.len(), 1);
    let tool = &tools.tools[0];
    assert_eq!(tool.name, "fake-aft_fake_read");
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
        .call_tool(CallToolRequestParams::new("fake-aft_fake_read").with_arguments(args))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(false));
    assert_eq!(result.content.len(), 1);
    assert_eq!(
        result_text(&result),
        "fake-aft tool fake_read called with {\"value\":\"hello\"}"
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
async fn mcp_project_ack_only_tool_stays_visible_and_skips_provider_frames() {
    let user_config = r#"
    {
      "version": 1,
      "providers": {
        "fake-aft": {
          "tools": { "overrides": { "fake_read": { "mode": "forward" } } }
        }
      }
    }
    "#;
    let project_config = r#"
    {
      "version": 1,
      "providers": {
        "fake-aft": {
          "tools": { "overrides": { "fake_read": { "mode": "ack_only" } } }
        }
      }
    }
    "#;
    let harness = McpHarness::start_configured(
        "mcp-ack-only",
        vec![StubProvider::new("fake-aft", &[])],
        Some(user_config),
        Some(project_config),
    )
    .await;

    let tools = harness.client.peer().list_tools(None).await.unwrap();
    assert_eq!(tools.tools.len(), 1);
    assert_eq!(tools.tools[0].name, "fake-aft_fake_read");
    assert_eq!(
        tools.tools[0].input_schema.get("type"),
        Some(&Value::String("object".to_owned()))
    );

    let provider_events_before = stub_events(&harness.events_path).unwrap();
    let mut args = JsonObject::new();
    args.insert("value".to_owned(), json!("ack-only"));
    let result = harness
        .client
        .peer()
        .call_tool(CallToolRequestParams::new("fake-aft_fake_read").with_arguments(args))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(false));
    assert_eq!(result.content.len(), 1);
    assert_eq!(result_text(&result), "Queued for context compaction.");
    assert_no_new_stub_events_within(&harness.events_path, &provider_events_before, QUIET_TIMEOUT)
        .await;

    harness.shutdown().await;
}

#[tokio::test]
async fn mcp_project_mode_cannot_widen_user_ack_only() {
    let user_config = r#"
    {
      "version": 1,
      "providers": {
        "fake-aft": {
          "tools": { "overrides": { "fake_read": { "mode": "ack_only" } } }
        }
      }
    }
    "#;
    let project_config = r#"
    {
      "version": 1,
      "providers": {
        "fake-aft": {
          "tools": { "overrides": { "fake_read": { "mode": "forward" } } }
        }
      }
    }
    "#;
    let harness = McpHarness::start_configured(
        "mcp-ack-only-widen",
        vec![StubProvider::new("fake-aft", &[])],
        Some(user_config),
        Some(project_config),
    )
    .await;

    let provider_events_before = stub_events(&harness.events_path).unwrap();
    let result = harness
        .client
        .peer()
        .call_tool(CallToolRequestParams::new("fake-aft_fake_read"))
        .await
        .unwrap();

    assert_eq!(result_text(&result), "Queued for context compaction.");
    assert_no_new_stub_events_within(&harness.events_path, &provider_events_before, QUIET_TIMEOUT)
        .await;

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
        peer.call_tool(CallToolRequestParams::new("fake-aft_fake_read"))
            .await
    });

    TestMcpClient::wait_for_counter(
        &harness.client_handler.progress_count,
        0,
        "progress notification before tool result",
    )
    .await;
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
                "fake-aft_fake_read",
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

    // The call resolves via one of two valid race outcomes: the client's local
    // cancellation completes first (ServiceError::Cancelled), or the gateway's
    // cancel-error response arrives first (McpError "tool call cancelled by MCP
    // client"). Both mean the call was cancelled; the durable proof that the
    // CANCEL actually reached the provider over the route is the stub event below.
    let err = handle.await_response().await.unwrap_err();
    match &err {
        ServiceError::Cancelled { .. } => {}
        ServiceError::McpError(error) if error.message.contains("cancelled") => {}
        other => panic!(
            "client should observe a cancelled MCP request (Cancelled or a cancelled McpError), got {other:?}"
        ),
    }

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
        .call_tool(CallToolRequestParams::new("fake-aft_fake_read"))
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
        .call_tool(CallToolRequestParams::new("fake-aft_fake_read"))
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

#[tokio::test]
async fn mcp_unknown_tool_returns_invalid_params_without_provider_call() {
    let harness = McpHarness::start("mcp-unknown-tool", &[]).await;
    let err = harness
        .client
        .peer()
        .call_tool(CallToolRequestParams::new("fake-aft_missing"))
        .await
        .unwrap_err();

    match err {
        ServiceError::McpError(error) => {
            assert_eq!(error.code, rmcp::model::ErrorCode::INVALID_PARAMS);
            assert_eq!(error.message, "unknown tool 'fake-aft_missing'");
        }
        other => panic!("expected invalid-params MCP error for unknown tool, got {other:?}"),
    }
    assert_no_stub_event_within(harness.provider_events_path("fake-aft"), QUIET_TIMEOUT, |event| {
        matches!(event.get("kind"), Some(Value::String(kind)) if kind == "tool_call" || kind == "request_received")
    })
    .await;

    harness.shutdown().await;
}

#[tokio::test]
async fn mcp_malformed_progress_frame_resolves_request_to_error_without_hang() {
    let harness = RawProviderHarness::start(
        "mcp-malformed-progress",
        RawProviderBehavior::MalformedProgress,
    )
    .await;
    assert_eq!(
        list_tool_names_on_peer(harness.client.peer()).await,
        vec![harness.tool_name()]
    );

    let result = timeout(
        NO_HANG_TIMEOUT,
        harness
            .client
            .peer()
            .call_tool(CallToolRequestParams::new(harness.tool_name())),
    )
    .await
    .expect("malformed progress request should resolve without hanging")
    .unwrap_err();

    match result {
        ServiceError::McpError(error) => {
            assert_eq!(error.code, rmcp::model::ErrorCode::INTERNAL_ERROR);
            assert!(
                error
                    .message
                    .contains("provider returned malformed progress"),
                "unexpected malformed-progress error: {error:?}"
            );
        }
        other => panic!("expected MCP internal error for malformed progress, got {other:?}"),
    }
    wait_for_atomic_at_least(
        harness.raw_provider.route_cancel_count(),
        1,
        "malformed-progress route cancellation",
    )
    .await;

    harness.shutdown().await;
}

#[tokio::test]
async fn mcp_malformed_terminal_response_resolves_request_to_error_without_hang() {
    let harness =
        RawProviderHarness::start("mcp-malformed-result", RawProviderBehavior::MalformedResult)
            .await;
    assert_eq!(
        list_tool_names_on_peer(harness.client.peer()).await,
        vec![harness.tool_name()]
    );

    let result = timeout(
        NO_HANG_TIMEOUT,
        harness
            .client
            .peer()
            .call_tool(CallToolRequestParams::new(harness.tool_name())),
    )
    .await
    .expect("malformed terminal response should resolve without hanging")
    .unwrap_err();

    match result {
        ServiceError::McpError(error) => {
            assert_eq!(error.code, rmcp::model::ErrorCode::INTERNAL_ERROR);
            assert!(
                error
                    .message
                    .contains("provider returned malformed tool result"),
                "unexpected malformed-terminal error: {error:?}"
            );
        }
        other => {
            panic!("expected MCP internal error for malformed terminal response, got {other:?}")
        }
    }

    harness.shutdown().await;
}

#[tokio::test]
async fn mcp_multi_provider_aggregation_routes_by_namespace_map() {
    let harness = McpHarness::start_configured(
        "mcp-multi-provider",
        vec![
            StubProvider::new("aft", &[("FAKE_AFT_TOOLS", "read,write")]),
            StubProvider::new("mc", &[("FAKE_AFT_TOOLS", "memory")]),
        ],
        None,
        None,
    )
    .await;

    assert_eq!(
        list_tool_names(&harness).await,
        vec!["aft_read", "aft_write", "mc_memory"]
    );

    let mut args = JsonObject::new();
    args.insert("key".to_owned(), json!("alpha"));
    let result = harness
        .client
        .peer()
        .call_tool(CallToolRequestParams::new("mc_memory").with_arguments(args))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(false));

    let mc_event = wait_for_stub_event(harness.provider_events_path("mc"), READ_TIMEOUT, |event| {
        event.get("kind") == Some(&Value::String("tool_call".to_owned()))
    })
    .await;
    assert_eq!(
        mc_event.get("name"),
        Some(&Value::String("memory".to_owned()))
    );
    assert_eq!(mc_event.pointer("/arguments/key"), Some(&json!("alpha")));
    assert!(
        stub_events(harness.provider_events_path("aft"))
            .unwrap()
            .into_iter()
            .all(|event| event.get("kind") != Some(&Value::String("tool_call".to_owned()))),
        "mc_memory should not route to aft"
    );

    harness.shutdown().await;
}

#[tokio::test]
async fn mcp_client_disconnect_goodbyes_all_provider_routes() {
    let mut harness = McpHarness::start_configured(
        "mcp-client-disconnect",
        vec![
            StubProvider::new("aft", &[("FAKE_AFT_TOOLS", "read")]),
            StubProvider::new("mc", &[("FAKE_AFT_TOOLS", "memory")]),
        ],
        None,
        None,
    )
    .await;
    wait_for_binding_count(&harness.server.daemon.forwarding, 2, READ_TIMEOUT).await;

    let aft_attach =
        wait_for_stub_event(harness.provider_events_path("aft"), READ_TIMEOUT, |event| {
            event.get("kind") == Some(&Value::String("attach".to_owned()))
        })
        .await;
    let mc_attach =
        wait_for_stub_event(harness.provider_events_path("mc"), READ_TIMEOUT, |event| {
            event.get("kind") == Some(&Value::String("attach".to_owned()))
        })
        .await;

    let _ = harness.client.close().await.unwrap();

    let aft_channel = aft_attach["route_channel"].as_u64().unwrap();
    let mc_channel = mc_attach["route_channel"].as_u64().unwrap();
    wait_for_stub_event(harness.provider_events_path("aft"), READ_TIMEOUT, |event| {
        event.get("kind") == Some(&Value::String("detach".to_owned()))
            && event.get("route_channel") == Some(&Value::from(aft_channel))
    })
    .await;
    wait_for_stub_event(harness.provider_events_path("mc"), READ_TIMEOUT, |event| {
        event.get("kind") == Some(&Value::String("detach".to_owned()))
            && event.get("route_channel") == Some(&Value::from(mc_channel))
    })
    .await;
    wait_for_binding_count(&harness.server.daemon.forwarding, 0, READ_TIMEOUT).await;

    harness.shutdown().await;
}

#[tokio::test]
async fn mcp_project_config_disables_provider_and_tool() {
    let project_config = r#"
    {
      // project config owns the user-visible exposure plane
      "version": 1,
      "providers": {
        "mc": { "enabled": false },
        "aft": { "tools": { "overrides": { "bash": false } } }
      }
    }
    "#;
    let harness = McpHarness::start_configured(
        "mcp-config-disable",
        vec![
            StubProvider::new("aft", &[("FAKE_AFT_TOOLS", "read,bash,write")]),
            StubProvider::new("mc", &[("FAKE_AFT_TOOLS", "memory")]),
        ],
        None,
        Some(project_config),
    )
    .await;

    assert_eq!(
        list_tool_names(&harness).await,
        vec!["aft_read", "aft_write"]
    );
    harness.shutdown().await;
}

#[tokio::test]
async fn mcp_project_config_allowlist_mode_exposes_only_overrides() {
    let project_config = r#"
    {
      "version": 1,
      "providers": {
        "aft": {
          "tools": {
            "defaultEnabled": false,
            "overrides": { "read": true }
          }
        }
      }
    }
    "#;
    let harness = McpHarness::start_configured(
        "mcp-config-allowlist",
        vec![StubProvider::new(
            "aft",
            &[("FAKE_AFT_TOOLS", "read,bash,write")],
        )],
        None,
        Some(project_config),
    )
    .await;

    assert_eq!(list_tool_names(&harness).await, vec!["aft_read"]);
    harness.shutdown().await;
}

#[tokio::test]
async fn mcp_project_tier_cannot_reenable_user_denied_tools_or_null_delete_denies() {
    let user_config = r#"
    {
      "version": 1,
      "providers": {
        "aft": {
          "tools": { "overrides": { "read": false, "write": false } }
        }
      }
    }
    "#;
    let project_config = r#"
    {
      "version": 1,
      "providers": {
        "aft": {
          "tools": { "overrides": { "read": true, "write": null } }
        }
      }
    }
    "#;
    let harness = McpHarness::start_configured(
        "mcp-config-merge",
        vec![StubProvider::new(
            "aft",
            &[("FAKE_AFT_TOOLS", "read,write")],
        )],
        Some(user_config),
        Some(project_config),
    )
    .await;

    assert_eq!(list_tool_names(&harness).await, Vec::<String>::new());

    harness.shutdown().await;
}

#[tokio::test]
async fn mcp_project_provider_level_null_cannot_reset_user_denies() {
    let user_config = r#"
    {
      "version": 1,
      "providers": {
        "aft": {
          "enabled": false,
          "namespace": "renamed",
          "tools": { "defaultEnabled": false }
        }
      }
    }
    "#;
    let project_config = r#"
    {
      "version": 1,
      "providers": {
        "aft": {
          "enabled": null,
          "namespace": null,
          "tools": null
        }
      }
    }
    "#;
    let harness = McpHarness::start_configured(
        "mcp-provider-null-reset",
        vec![StubProvider::new("aft", &[("FAKE_AFT_TOOLS", "read")])],
        Some(user_config),
        Some(project_config),
    )
    .await;

    assert_eq!(list_tool_names(&harness).await, Vec::<String>::new());
    let err = harness
        .client
        .peer()
        .call_tool(CallToolRequestParams::new("aft_read"))
        .await
        .unwrap_err();
    assert_unknown_tool_error(err, "aft_read");

    harness.shutdown().await;
}

#[tokio::test]
async fn mcp_global_namespace_override_changes_exposed_prefix() {
    let user_config = r#"
    {
      "version": 1,
      "providers": {
        "aft": { "namespace": "tools" }
      }
    }
    "#;
    let harness = McpHarness::start_configured(
        "mcp-namespace",
        vec![StubProvider::new("aft", &[("FAKE_AFT_TOOLS", "read")])],
        Some(user_config),
        None,
    )
    .await;

    assert_eq!(list_tool_names(&harness).await, vec!["tools_read"]);
    let result = harness
        .client
        .peer()
        .call_tool(CallToolRequestParams::new("tools_read"))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(false));
    let event = wait_for_stub_event(harness.provider_events_path("aft"), READ_TIMEOUT, |event| {
        event.get("kind") == Some(&Value::String("tool_call".to_owned()))
    })
    .await;
    assert_eq!(event.get("name"), Some(&Value::String("read".to_owned())));

    harness.shutdown().await;
}

#[tokio::test]
async fn mcp_project_namespace_and_description_overrides_are_dropped() {
    let user_config = r#"
    {
      "version": 1,
      "providers": {
        "aft": {
          "tools": {
            "overrides": {
              "read": { "description": "global read description" }
            }
          }
        }
      }
    }
    "#;
    let project_config = r#"
    {
      "version": 1,
      "providers": {
        "aft": {
          "namespace": "project",
          "tools": {
            "overrides": {
              "read": { "description": "project read description" }
            }
          }
        }
      }
    }
    "#;
    let harness = McpHarness::start_configured(
        "mcp-project-dropped-strings",
        vec![StubProvider::new("aft", &[("FAKE_AFT_TOOLS", "read")])],
        Some(user_config),
        Some(project_config),
    )
    .await;

    let tools = harness.client.peer().list_tools(None).await.unwrap().tools;
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "aft_read");
    assert_eq!(
        tools[0].description.as_deref(),
        Some("global read description")
    );

    harness.shutdown().await;
}

#[tokio::test]
async fn mcp_project_attempts_to_grant_are_dropped_with_warning() {
    let label = "mcp-project-grant-warning";
    let server = TestServer::start().await;
    let events_path = server
        .daemon
        .temp_dir
        .join(format!("{label}-aft-events.jsonl"));
    let provider = supervisor(&server)
        .spawn(stub_spec(
            "aft",
            &events_path,
            &[("FAKE_AFT_TOOLS", "read")],
        ))
        .unwrap();
    wait_for_registration(&server.daemon.registry, "aft", READ_TIMEOUT).await;

    let user_config_home = server.daemon.temp_dir.join(format!("{label}-xdg-config"));
    fs::create_dir_all(&user_config_home).unwrap();
    write_user_mcp_config(
        &user_config_home,
        r#"
        {
          "version": 1,
          "providers": { "aft": { "enabled": false } }
        }
        "#,
    );

    let module_connection_file = server
        .daemon
        .temp_dir
        .join(format!("{label}-subc-mcp.json"));
    let (mut module, mut module_stderr) = spawn_module_with_stderr(
        &server.daemon.connection_file_path,
        &module_connection_file,
        &user_config_home,
    );
    wait_for_module_connection_file(&mut module, &module_connection_file, READ_TIMEOUT).await;

    let project = TestProject::new(label);
    write_project_mcp_config(
        &project.path,
        r#"
        {
          "version": 1,
          "providers": { "aft": { "enabled": true } }
        }
        "#,
    );

    let mut shim = spawn_shim(&module_connection_file, &project.path, &user_config_home);
    let client_handler = TestMcpClient::new();
    let client = shim.serve_mcp_client(client_handler).await;
    assert_eq!(
        list_tool_names_on_peer(client.peer()).await,
        Vec::<String>::new()
    );
    let stderr = wait_for_child_stderr_contains(
        &mut module_stderr,
        "dropping project MCP config field providers.aft.enabled",
        READ_TIMEOUT,
    )
    .await;
    assert!(
        stderr.contains("cannot enable a provider disabled by the user baseline"),
        "warning should explain why the grant was dropped: {stderr}"
    );

    let _ = client.cancel().await;
    let _ = timeout(Duration::from_secs(2), shim.child.wait()).await;
    if module.try_wait().unwrap().is_none() {
        let _ = module.start_kill();
        let _ = timeout(Duration::from_secs(2), module.wait()).await;
    }
    provider.stop().await.unwrap();
}

#[tokio::test]
async fn mcp_zero_tool_provider_gets_no_route() {
    let project_config = r#"
    {
      "version": 1,
      "providers": {
        "aft": { "tools": { "defaultEnabled": false } }
      }
    }
    "#;
    let harness = McpHarness::start_configured(
        "mcp-zero-tool-provider",
        vec![StubProvider::new("aft", &[("FAKE_AFT_TOOLS", "read")])],
        None,
        Some(project_config),
    )
    .await;

    assert_eq!(list_tool_names(&harness).await, Vec::<String>::new());
    wait_for_binding_count(&harness.server.daemon.forwarding, 0, READ_TIMEOUT).await;
    assert_no_stub_event_within(
        harness.provider_events_path("aft"),
        QUIET_TIMEOUT,
        |event| event.get("kind") == Some(&Value::String("attach".to_owned())),
    )
    .await;

    harness.shutdown().await;
}

#[tokio::test]
async fn mcp_facade_default_deny_requires_global_enable() {
    let default_harness = McpHarness::start_configured(
        "mcp-default-deny-absent",
        vec![StubProvider::new(
            "magic-context",
            &[("FAKE_AFT_TOOLS", "read")],
        )],
        None,
        None,
    )
    .await;
    assert_eq!(
        list_tool_names(&default_harness).await,
        Vec::<String>::new()
    );
    wait_for_binding_count(&default_harness.server.daemon.forwarding, 0, READ_TIMEOUT).await;
    default_harness.shutdown().await;

    let global_config = r#"
    {
      "version": 1,
      "providers": { "magic-context": { "enabled": true } }
    }
    "#;
    let global_harness = McpHarness::start_configured(
        "mcp-default-deny-global",
        vec![StubProvider::new(
            "magic-context",
            &[("FAKE_AFT_TOOLS", "read")],
        )],
        Some(global_config),
        None,
    )
    .await;
    assert_eq!(
        list_tool_names(&global_harness).await,
        vec!["magic-context_read"]
    );
    global_harness.shutdown().await;

    let project_config = r#"
    {
      "version": 1,
      "providers": { "magic-context": { "enabled": true } }
    }
    "#;
    let project_harness = McpHarness::start_configured(
        "mcp-default-deny-project",
        vec![StubProvider::new(
            "magic-context",
            &[("FAKE_AFT_TOOLS", "read")],
        )],
        None,
        Some(project_config),
    )
    .await;
    assert_eq!(
        list_tool_names(&project_harness).await,
        Vec::<String>::new()
    );
    wait_for_binding_count(&project_harness.server.daemon.forwarding, 0, READ_TIMEOUT).await;
    project_harness.shutdown().await;
}

// THESE FOUR TESTS ASSERTED THE BEHAVIOUR 27289612 DELIBERATELY REMOVED.
//
// They were written when ONE malformed name aborted the whole surface: an
// invalid namespace, an MCP-illegal tool name, or a collision made
// `desired_session_from_catalog` return Err, the module rejected the attach, and
// the shim exited. That is what `expect_shim_attach_failure` asserts.
//
// It shipped a fleet outage. `plexus` published dotted tool names, and every
// Claude Code session on the machine came up with ZERO subc tools -- no ctx_*,
// no aft -- because one provider's illegal name erased every other provider's.
// The fix made each offending ENTRY skip with a warning while the rest of the
// surface is served.
//
// So the tests now assert the OPPOSITE of the contract, and they are rewritten
// rather than deleted: the interesting property did not disappear, it INVERTED.
// "one bad name is refused" became "one bad name is refused AND its neighbours
// survive", which is a strictly stronger thing to hold, and deleting them would
// have left the outage's own regression test missing.
//
// Each keeps a NEIGHBOUR in the fixture and asserts both halves -- the illegal
// entry absent, the legal one present. Asserting only the absence would pass on
// a build that skipped everything, which is the outage.
#[tokio::test]
async fn mcp_meta_tool_name_collision_skips_the_entry_and_keeps_the_rest() {
    let user_config = r#"
    {
      "version": 1,
      "providers": { "aft": { "namespace": "tools" } }
    }
    "#;
    let harness = McpHarness::start_configured(
        "mcp-meta-reserved-collision",
        vec![StubProvider::new(
            "aft",
            &[("FAKE_AFT_TOOLS", "search,read")],
        )],
        Some(user_config),
        None,
    )
    .await;

    let names: Vec<String> = harness
        .client
        .peer()
        .list_tools(None)
        .await
        .unwrap()
        .tools
        .into_iter()
        .map(|t| t.name.to_string())
        .collect();

    // `tools_search` is a reserved meta-tool name, so the namespaced `search`
    // collides and must be dropped -- but `read` from the same provider survives.
    assert!(
        !names.contains(&"tools_search".to_string()),
        "the reserved-name collision must be skipped, got {names:?}"
    );
    assert!(
        names.contains(&"tools_read".to_string()),
        "the sibling tool must still be served, got {names:?}"
    );

    harness.shutdown().await;
}

#[tokio::test]
async fn mcp_invalid_namespace_skips_the_provider_and_keeps_the_rest() {
    let user_config = r#"
    {
      "version": 1,
      "providers": {
        "aft": { "namespace": "bad namespace" }
      }
    }
    "#;
    let harness = McpHarness::start_configured(
        "mcp-invalid-namespace",
        vec![
            StubProvider::new("aft", &[("FAKE_AFT_TOOLS", "read")]),
            StubProvider::new("mc", &[("FAKE_AFT_TOOLS", "recall")]),
        ],
        Some(user_config),
        None,
    )
    .await;

    let names: Vec<String> = harness
        .client
        .peer()
        .list_tools(None)
        .await
        .unwrap()
        .tools
        .into_iter()
        .map(|t| t.name.to_string())
        .collect();

    // An unusable namespace drops that PROVIDER; the other provider is untouched.
    // This is the exact shape of the outage: one bad config entry must not cost
    // the surface every other module's tools.
    assert!(
        !names.iter().any(|n| n.contains("read")),
        "the provider with an invalid namespace must be skipped, got {names:?}"
    );
    assert!(
        names.contains(&"mc_recall".to_string()),
        "the healthy provider must still be served, got {names:?}"
    );

    harness.shutdown().await;
}

#[tokio::test]
async fn mcp_invalid_tool_name_skips_the_tool_and_keeps_the_rest() {
    let harness = McpHarness::start_configured(
        "mcp-invalid-tool-name",
        vec![StubProvider::new(
            "aft",
            &[("FAKE_AFT_TOOLS", "bad tool,read")],
        )],
        None,
        None,
    )
    .await;

    let names: Vec<String> = harness
        .client
        .peer()
        .list_tools(None)
        .await
        .unwrap()
        .tools
        .into_iter()
        .map(|t| t.name.to_string())
        .collect();

    // The illegal name must be SKIPPED, never sanitised into the surface: a
    // renamed tool would be addressable under a name the module does not answer.
    assert!(
        !names.iter().any(|n| n.contains("bad")),
        "the illegal tool must be skipped, not renamed, got {names:?}"
    );
    assert!(
        names.contains(&"aft_read".to_string()),
        "the legal sibling tool must still be served, got {names:?}"
    );

    harness.shutdown().await;
}

/// Two providers claiming one exposed name: the FIRST claimant is kept.
///
/// Rewritten alongside the other three (see the note above
/// `mcp_meta_tool_name_collision_skips_the_entry_and_keeps_the_rest`) -- this
/// one carries an extra property the others do not. A collision has two
/// survivable resolutions, keep-first and keep-neither, and only keep-first is
/// correct: dropping both would let a newly-installed module silently delete a
/// tool the user already depends on.
///
/// So the assertion is that exactly ONE `dup_read` is served, not merely that
/// the surface is non-empty. A count of two would mean duplicate names on the
/// wire, which is what the collision check exists to prevent.
#[tokio::test]
async fn mcp_namespace_collision_keeps_the_first_claimant() {
    let user_config = r#"
    {
      "version": 1,
      "providers": {
        "aft": { "namespace": "dup" },
        "mc": { "namespace": "dup" }
      }
    }
    "#;
    let harness = McpHarness::start_configured(
        "mcp-collision",
        vec![
            StubProvider::new("aft", &[("FAKE_AFT_TOOLS", "read")]),
            StubProvider::new("mc", &[("FAKE_AFT_TOOLS", "read")]),
        ],
        Some(user_config),
        None,
    )
    .await;

    let names: Vec<String> = harness
        .client
        .peer()
        .list_tools(None)
        .await
        .unwrap()
        .tools
        .into_iter()
        .map(|t| t.name.to_string())
        .collect();

    assert_eq!(
        names.iter().filter(|n| n.as_str() == "dup_read").count(),
        1,
        "exactly one claimant must survive a name collision, got {names:?}"
    );

    harness.shutdown().await;
}

#[tokio::test]
async fn mcp_invalid_config_fails_attach_closed() {
    expect_shim_attach_failure(
        "mcp-invalid-config",
        vec![StubProvider::new("aft", &[("FAKE_AFT_TOOLS", "read")])],
        None,
        Some(r#"{ "providers": { "aft": {} } }"#),
    )
    .await;
}

#[tokio::test]
async fn mcp_provider_goodbye_removes_tools_notifies_and_fails_inflight_call() {
    let harness = McpHarness::start_configured(
        "mcp-provider-goodbye",
        vec![
            StubProvider::new("aft", &[("FAKE_AFT_TOOLS", "read")]),
            StubProvider::new(
                "mc",
                &[
                    ("FAKE_AFT_TOOLS", "memory"),
                    ("FAKE_AFT_TOOLCALL_DELAY_MS", "5000"),
                ],
            ),
        ],
        None,
        None,
    )
    .await;
    assert_eq!(
        list_tool_names(&harness).await,
        vec!["aft_read", "mc_memory"]
    );

    let peer = harness.client.peer().clone();
    let call = tokio::spawn(async move {
        peer.call_tool(CallToolRequestParams::new("mc_memory"))
            .await
    });
    let _event = wait_for_stub_event(harness.provider_events_path("mc"), READ_TIMEOUT, |event| {
        event.get("kind") == Some(&Value::String("request_received".to_owned()))
            && event.pointer("/body_json/name") == Some(&Value::String("memory".to_owned()))
    })
    .await;

    harness.providers.get("mc").unwrap().stop().await.unwrap();
    TestMcpClient::wait_for_counter(
        &harness.client_handler.tool_list_changed_count,
        0,
        "tools/list_changed after provider GOODBYE",
    )
    .await;

    let err = call.await.unwrap().unwrap_err();
    match err {
        ServiceError::McpError(error) => assert!(
            error.message.contains("target_unavailable"),
            "in-flight provider call should fail cleanly, got {error:?}"
        ),
        other => panic!("expected MCP error for provider death, got {other:?}"),
    }
    assert_eq!(list_tool_names(&harness).await, vec!["aft_read"]);

    harness.shutdown().await;
}

#[tokio::test]
async fn mcp_catalog_poller_adds_new_provider_and_notifies() {
    let mut harness = McpHarness::start_configured(
        "mcp-catalog-add",
        vec![StubProvider::new("aft", &[("FAKE_AFT_TOOLS", "read")])],
        None,
        None,
    )
    .await;
    assert_eq!(list_tool_names(&harness).await, vec!["aft_read"]);

    harness
        .spawn_provider("mc", &[("FAKE_AFT_TOOLS", "memory")])
        .await;
    TestMcpClient::wait_for_counter(
        &harness.client_handler.tool_list_changed_count,
        0,
        "tools/list_changed after catalog generation poll",
    )
    .await;
    assert_eq!(
        list_tool_names(&harness).await,
        vec!["aft_read", "mc_memory"]
    );

    harness.shutdown().await;
}

#[tokio::test]
async fn mcp_on_attach_refresh_keeps_mid_session_config_edits_sticky() {
    let harness = McpHarness::start_configured(
        "mcp-on-attach-sticky",
        vec![StubProvider::new(
            "aft",
            &[("FAKE_AFT_TOOLS", "read,write")],
        )],
        None,
        None,
    )
    .await;
    assert_eq!(
        list_tool_names(&harness).await,
        vec!["aft_read", "aft_write"]
    );

    write_project_mcp_config(
        &harness._project.path,
        r#"
        {
          "version": 1,
          "providers": {
            "aft": { "tools": { "overrides": { "write": false } } }
          }
        }
        "#,
    );

    assert_eq!(
        list_tool_names(&harness).await,
        vec!["aft_read", "aft_write"]
    );
    let result = harness
        .client
        .peer()
        .call_tool(CallToolRequestParams::new("aft_write"))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(false));

    harness.shutdown().await;
}

#[tokio::test]
async fn mcp_immediate_policy_removal_does_not_retain_forward_binding() {
    let user_config = r#"
    {
      "version": 1,
      "refresh": "immediate",
      "providers": {
        "aft": {
          "tools": { "overrides": { "read": { "mode": "ack_only" } } }
        }
      }
    }
    "#;
    let harness = McpHarness::start_configured(
        "mcp-forward-removal",
        vec![StubProvider::new(
            "aft",
            &[("FAKE_AFT_TOOLS", "read,write,keep")],
        )],
        Some(user_config),
        None,
    )
    .await;
    assert_eq!(
        list_tool_names(&harness).await,
        vec!["aft_keep", "aft_read", "aft_write"]
    );

    let notification_baseline = harness
        .client_handler
        .tool_list_changed_count
        .load(Ordering::SeqCst);
    let provider_events_before = stub_events(harness.provider_events_path("aft")).unwrap();
    write_project_mcp_config(
        &harness._project.path,
        r#"
        {
          "version": 1,
          "providers": {
            "aft": {
              "tools": { "overrides": { "read": false, "write": false } }
            }
          }
        }
        "#,
    );

    let ack_result = harness
        .client
        .peer()
        .call_tool(CallToolRequestParams::new("aft_read"))
        .await
        .unwrap();
    assert_eq!(ack_result.is_error, Some(false));
    assert_eq!(result_text(&ack_result), "Queued for context compaction.");

    let err = harness
        .client
        .peer()
        .call_tool(CallToolRequestParams::new("aft_write"))
        .await
        .unwrap_err();
    assert_unknown_tool_error(err, "aft_write");
    TestMcpClient::wait_for_counter(
        &harness.client_handler.tool_list_changed_count,
        notification_baseline,
        "tools/list_changed after immediate policy refresh",
    )
    .await;
    assert_eq!(list_tool_names(&harness).await, vec!["aft_keep"]);
    assert_no_new_stub_events_within(
        harness.provider_events_path("aft"),
        &provider_events_before,
        QUIET_TIMEOUT,
    )
    .await;

    harness.shutdown().await;
}

#[tokio::test]
async fn mcp_immediate_policy_removal_keeps_ack_only_tombstone_for_stale_call() {
    let user_config = r#"
    {
      "version": 1,
      "refresh": "immediate",
      "providers": {
        "aft": {
          "tools": { "overrides": { "read": { "mode": "ack_only" } } }
        }
      }
    }
    "#;
    let harness = McpHarness::start_configured(
        "mcp-ack-only-tombstone",
        vec![StubProvider::new(
            "aft",
            &[("FAKE_AFT_TOOLS", "read,write")],
        )],
        Some(user_config),
        None,
    )
    .await;
    assert_eq!(
        list_tool_names(&harness).await,
        vec!["aft_read", "aft_write"]
    );

    let provider_events_before = stub_events(harness.provider_events_path("aft")).unwrap();
    write_project_mcp_config(
        &harness._project.path,
        r#"
        {
          "version": 1,
          "providers": {
            "aft": { "tools": { "overrides": { "read": false } } }
          }
        }
        "#,
    );

    let result = harness
        .client
        .peer()
        .call_tool(CallToolRequestParams::new("aft_read"))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(false));
    assert_eq!(result_text(&result), "Queued for context compaction.");
    assert_eq!(list_tool_names(&harness).await, vec!["aft_write"]);
    assert_no_new_stub_events_within(
        harness.provider_events_path("aft"),
        &provider_events_before,
        QUIET_TIMEOUT,
    )
    .await;

    harness.shutdown().await;
}

#[tokio::test]
async fn mcp_search_mode_exposes_meta_surface_and_private_invocation() {
    let user_config = r#"
    {
      "version": 1,
      "surfaceMode": "search",
      "providers": {
        "aft": { "tools": { "overrides": { "bash": false } } }
      }
    }
    "#;
    let mut harness = McpHarness::start_configured(
        "mcp-search-mode",
        vec![StubProvider::new(
            "aft",
            &[("FAKE_AFT_TOOLS", "write,bash,read")],
        )],
        Some(user_config),
        None,
    )
    .await;

    assert_eq!(
        list_tool_names(&harness).await,
        vec!["tools_invoke", "tools_search"]
    );

    let direct_err = harness
        .client
        .peer()
        .call_tool(CallToolRequestParams::new("aft_read"))
        .await
        .unwrap_err();
    assert_unknown_tool_error(direct_err, "aft_read");
    assert_no_stub_event_within(
        harness.provider_events_path("aft"),
        QUIET_TIMEOUT,
        |event| event.get("kind") == Some(&Value::String("tool_call".to_owned())),
    )
    .await;

    let mut search_args = JsonObject::new();
    search_args.insert("query".to_owned(), json!("aft_"));
    let search_result = harness
        .client
        .peer()
        .call_tool(CallToolRequestParams::new("tools_search").with_arguments(search_args))
        .await
        .unwrap();
    let search_json = result_json(&search_result);
    let names = search_json
        .as_array()
        .expect("tools_search should return an array")
        .iter()
        .map(|entry| entry["name"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(names, vec!["aft_read", "aft_write"]);

    let mut invoke_args = JsonObject::new();
    invoke_args.insert("name".to_owned(), json!("aft_read"));
    invoke_args.insert("arguments".to_owned(), json!({ "value": "via invoke" }));
    let invoke_result = harness
        .client
        .peer()
        .call_tool(CallToolRequestParams::new("tools_invoke").with_arguments(invoke_args))
        .await
        .unwrap();
    assert_eq!(invoke_result.is_error, Some(false));
    let event = wait_for_stub_event(harness.provider_events_path("aft"), READ_TIMEOUT, |event| {
        event.get("kind") == Some(&Value::String("tool_call".to_owned()))
    })
    .await;
    assert_eq!(event.get("name"), Some(&Value::String("read".to_owned())));
    assert_eq!(
        event.pointer("/arguments/value"),
        Some(&json!("via invoke"))
    );

    for missing in ["aft_bash", "aft_missing"] {
        let mut args = JsonObject::new();
        args.insert("name".to_owned(), json!(missing));
        let err = harness
            .client
            .peer()
            .call_tool(CallToolRequestParams::new("tools_invoke").with_arguments(args))
            .await
            .unwrap_err();
        assert_unknown_tool_error(err, missing);
    }

    let before_death = harness
        .client_handler
        .tool_list_changed_count
        .load(Ordering::SeqCst);
    harness.providers.get("aft").unwrap().stop().await.unwrap();
    TestMcpClient::wait_for_counter(
        &harness.client_handler.tool_list_changed_count,
        before_death,
        "tools/list_changed after search-mode provider death",
    )
    .await;
    let mut dead_args = JsonObject::new();
    dead_args.insert("name".to_owned(), json!("aft_read"));
    let dead_err = harness
        .client
        .peer()
        .call_tool(CallToolRequestParams::new("tools_invoke").with_arguments(dead_args))
        .await
        .unwrap_err();
    assert_unknown_tool_error(dead_err, "aft_read");

    let before_rejoin = harness
        .client_handler
        .tool_list_changed_count
        .load(Ordering::SeqCst);
    harness
        .spawn_provider("aft", &[("FAKE_AFT_TOOLS", "read,write,bash")])
        .await;
    TestMcpClient::wait_for_counter(
        &harness.client_handler.tool_list_changed_count,
        before_rejoin,
        "tools/list_changed after search-mode provider rejoin",
    )
    .await;
    assert_eq!(
        list_tool_names(&harness).await,
        vec!["tools_invoke", "tools_search"]
    );
    let mut rejoin_args = JsonObject::new();
    rejoin_args.insert("name".to_owned(), json!("aft_read"));
    let rejoin_result = harness
        .client
        .peer()
        .call_tool(CallToolRequestParams::new("tools_invoke").with_arguments(rejoin_args))
        .await
        .unwrap();
    assert_eq!(rejoin_result.is_error, Some(false));

    harness.shutdown().await;
}

#[tokio::test]
async fn mcp_search_mode_invoke_honors_ack_only_policy() {
    let user_config = r#"
    {
      "version": 1,
      "surfaceMode": "search",
      "providers": {
        "fake-aft": {
          "tools": { "overrides": { "fake_read": { "mode": "ack_only" } } }
        }
      }
    }
    "#;
    let harness = McpHarness::start_configured(
        "mcp-search-ack-only",
        vec![StubProvider::new("fake-aft", &[])],
        Some(user_config),
        None,
    )
    .await;

    let mut search_args = JsonObject::new();
    search_args.insert("query".to_owned(), json!("fake_read"));
    let search_result = harness
        .client
        .peer()
        .call_tool(CallToolRequestParams::new("tools_search").with_arguments(search_args))
        .await
        .unwrap();
    let search_json = result_json(&search_result);
    assert_eq!(search_json[0]["name"], json!("fake-aft_fake_read"));
    assert_eq!(search_json[0]["input_schema"]["type"], json!("object"));

    let provider_events_before = stub_events(&harness.events_path).unwrap();
    let mut invoke_args = JsonObject::new();
    invoke_args.insert("name".to_owned(), json!("fake-aft_fake_read"));
    invoke_args.insert("arguments".to_owned(), json!({ "value": "search ack" }));
    let result = harness
        .client
        .peer()
        .call_tool(CallToolRequestParams::new("tools_invoke").with_arguments(invoke_args))
        .await
        .unwrap();

    assert_eq!(result.is_error, Some(false));
    assert_eq!(result.content.len(), 1);
    assert_eq!(result_text(&result), "Queued for context compaction.");
    assert_no_new_stub_events_within(&harness.events_path, &provider_events_before, QUIET_TIMEOUT)
        .await;

    harness.shutdown().await;
}

#[tokio::test]
async fn mcp_catalog_reconciliation_failure_preserves_previous_snapshot_and_cleans_opened_routes() {
    let label = "mcp-catalog-failure";
    let server = TestServer::start().await;
    let mut providers = BTreeMap::new();
    let mut provider_events = BTreeMap::new();

    let aft_events_path = server
        .daemon
        .temp_dir
        .join(format!("{label}-aft-events.jsonl"));
    let aft = supervisor(&server)
        .spawn(stub_spec(
            "aft",
            &aft_events_path,
            &[("FAKE_AFT_TOOLS", "read")],
        ))
        .unwrap();
    wait_for_registration(&server.daemon.registry, "aft", READ_TIMEOUT).await;
    providers.insert("aft".to_owned(), aft);
    provider_events.insert("aft".to_owned(), aft_events_path.clone());

    let user_config_home = server.daemon.temp_dir.join(format!("{label}-xdg-config"));
    fs::create_dir_all(&user_config_home).unwrap();
    let module_connection_file = server
        .daemon
        .temp_dir
        .join(format!("{label}-subc-mcp.json"));
    let mut module = spawn_module(
        &server.daemon.connection_file_path,
        &module_connection_file,
        &user_config_home,
    );
    wait_for_module_connection_file(&mut module, &module_connection_file, READ_TIMEOUT).await;

    let project = TestProject::new(label);
    let mut shim = spawn_shim(&module_connection_file, &project.path, &user_config_home);
    let _aft_attach = wait_for_stub_event(&aft_events_path, READ_TIMEOUT, |event| {
        event.get("kind") == Some(&Value::String("attach".to_owned()))
    })
    .await;
    wait_for_binding_count(&server.daemon.forwarding, 1, READ_TIMEOUT).await;

    let bee_events_path = server
        .daemon
        .temp_dir
        .join(format!("{label}-bee-events.jsonl"));
    let bee = supervisor(&server)
        .spawn(stub_spec(
            "bee",
            &bee_events_path,
            &[("FAKE_AFT_TOOLS", "memory")],
        ))
        .unwrap();
    wait_for_registration(&server.daemon.registry, "bee", READ_TIMEOUT).await;
    providers.insert("bee".to_owned(), bee);
    provider_events.insert("bee".to_owned(), bee_events_path.clone());

    let zed_events_path = server
        .daemon
        .temp_dir
        .join(format!("{label}-zed-events.jsonl"));
    let zed = supervisor(&server)
        .spawn(stub_spec(
            "zed",
            &zed_events_path,
            &[
                ("FAKE_AFT_TOOLS", "search"),
                ("FAKE_AFT_REJECT_ATTACH", "1"),
            ],
        ))
        .unwrap();
    wait_for_registration(&server.daemon.registry, "zed", READ_TIMEOUT).await;
    providers.insert("zed".to_owned(), zed);
    provider_events.insert("zed".to_owned(), zed_events_path.clone());

    let client_handler = TestMcpClient::new();
    let client = shim.serve_mcp_client(client_handler.clone()).await;
    let harness = McpHarness {
        server,
        _project: project,
        providers,
        module,
        shim,
        client,
        client_handler,
        events_path: aft_events_path.clone(),
        provider_events,
    };

    let bee_attach =
        wait_for_stub_event(harness.provider_events_path("bee"), READ_TIMEOUT, |event| {
            event.get("kind") == Some(&Value::String("attach".to_owned()))
        })
        .await;
    wait_for_stub_event(harness.provider_events_path("zed"), READ_TIMEOUT, |event| {
        event.get("kind") == Some(&Value::String("attach".to_owned()))
            && event.get("reject") == Some(&Value::Bool(true))
    })
    .await;
    let _bee_channel = bee_attach["route_channel"].as_u64().unwrap();
    // The rejecting provider is SKIPPED for this pass; the healthy new
    // provider is adopted. The old contract rolled the whole reconcile back,
    // which meant one module's refusal silently discarded every other
    // module's tools -- the exact shape that once removed the entire Claude
    // Code tool surface because one connector's policy said no. Two bindings:
    // aft (kept) and bee (adopted); zed gets none.
    wait_for_binding_count(&harness.server.daemon.forwarding, 2, READ_TIMEOUT).await;
    TestMcpClient::wait_for_counter(
        &harness.client_handler.tool_list_changed_count,
        0,
        "tools/list_changed after partial catalog reconciliation",
    )
    .await;
    assert_eq!(
        list_tool_names(&harness).await,
        vec!["aft_read", "bee_memory"],
        "the healthy new provider must be adopted and the rejecting one absent"
    );

    let result = harness
        .client
        .peer()
        .call_tool(CallToolRequestParams::new("aft_read"))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(false));
    assert!(
        result_text(&result).contains("fake-aft tool read called"),
        "existing tool should remain callable after partial reconciliation: {result:?}"
    );
    let result = harness
        .client
        .peer()
        .call_tool(CallToolRequestParams::new("bee_memory"))
        .await
        .unwrap();
    assert_eq!(
        result.is_error,
        Some(false),
        "the adopted provider's tool must actually serve: {result:?}"
    );

    harness.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supervised_mcp_module_reports_live_non_routable_and_preserves_provider_route() {
    let server = TestServer::start().await;
    let provider_events_path = server
        .daemon
        .temp_dir
        .join("supervised-mcp-aft-events.jsonl");
    let provider = supervisor(&server)
        .spawn(stub_spec(
            "aft",
            &provider_events_path,
            &[("FAKE_AFT_TOOLS", "read")],
        ))
        .unwrap();
    wait_for_registration(&server.daemon.registry, "aft", SETUP_TIMEOUT).await;

    let xdg_config_home = server.daemon.temp_dir.join("supervised-mcp-xdg-config");
    fs::create_dir_all(&xdg_config_home).unwrap();
    let module_connection_file = server.daemon.temp_dir.join("supervised-mcp-module.json");
    let mcp = supervisor(&server)
        .spawn(mcp_module_spec(
            "mcp",
            &module_connection_file,
            &xdg_config_home,
        ))
        .unwrap();

    let entry = wait_for_supervisor_entry(
        &server.daemon.connection_file_path,
        "mcp",
        |entry| entry.state == "running" && entry.enabled && entry.live,
        SETUP_TIMEOUT,
    )
    .await;
    assert_eq!(entry.module_id, "mcp");

    let catalog = catalog_modules(&server.daemon.connection_file_path, Some("mcp"), 2_000).await;
    assert_eq!(catalog.len(), 1, "mcp should be registered in the catalog");
    assert!(
        catalog[0].roles.is_empty(),
        "mcp supervision registration must not advertise routable roles: {:?}",
        catalog[0].roles
    );

    let mut client =
        wait_for_control_client(&server.daemon.connection_file_path, SETUP_TIMEOUT).await;
    let err = control_error_on_stream(
        &mut client,
        2_001,
        ClientControlRequest::RouteOpen {
            target: RouteTarget::ToolProvider {
                module_id: "mcp".to_string(),
            },
            identity: route_identity("mcp", 2_001),
            consumer_identity: None,
            consumer_capabilities: None,
            admission_facts: None,
        },
    )
    .await;
    assert_eq!(err.code, "target_unavailable");
    assert!(
        err.message
            .contains("does not provide the requested target"),
        "unexpected route.open error for non-routable mcp: {err:?}"
    );

    let route = open_route(&mut client, "aft", 2_002).await;
    write_frame(
        &mut client,
        &data_request(
            route,
            2_003,
            br#"{ "name": "read", "arguments": { "path": "demo" } }"#,
        ),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();
    let response = read_frame_timeout(&mut client).await;
    assert_eq!(response.header.ty, FrameType::Response);
    assert_eq!(
        (response.header.channel, response.header.epoch),
        (route.channel, route.epoch)
    );
    assert_eq!(response.header.corr, 2_003);
    let body: Value = serde_json::from_slice(&response.body).unwrap();
    let text = body["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("fake-aft tool read called"),
        "aft provider should still serve tool calls after mcp registers: {body:?}"
    );

    match control_rpc_on_stream(
        &mut client,
        2_004,
        ClientControlRequest::SupervisorHealthProbe {
            module_id: "mcp".to_string(),
        },
    )
    .await
    {
        ClientControlResponse::SupervisorHealthProbe {
            module_id,
            status,
            metrics,
            ..
        } => {
            assert_eq!(module_id, "mcp");
            assert_eq!(status, ControlHealthStatus::Ok);
            let metrics = metrics.expect("health metrics should be present");
            assert!(
                metrics.get("active_relay_routes").is_some()
                    && metrics.get("pending_reverse_requests").is_some(),
                "expected relay metrics in health report: {metrics:?}"
            );
        }
        other => panic!("unexpected supervisor.health_probe response: {other:?}"),
    }

    mcp.stop().await.unwrap();
    provider.stop().await.unwrap();
}

#[tokio::test]
async fn mcp_shim_rejects_unsupported_hello_ack_schema() {
    let bad_schema = TEST_SHIM_SCHEMA_VERSION + 1;
    let module_server = TestProject::new("mcp-bad-ack-module-server");
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let key = generate_key().unwrap();
    let daemon_id = generate_daemon_id().unwrap();
    let connection_file_path = module_server.path.join("subc-mcp-module.json");
    write_atomic(
        &connection_file_path,
        &ConnectionInfo {
            schema: SCHEMA_VERSION,
            wire_version: None,
            endpoints: vec![Endpoint {
                host: Ipv4Addr::LOCALHOST.to_string(),
                port,
            }],
            key: key.clone(),
            daemon_id,
            pid: process::id(),
            daemon_ver: TEST_DAEMON_VER.to_owned(),
        },
    )
    .unwrap();

    let server_task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        authenticate_server(
            &mut stream,
            &key,
            &daemon_id,
            TEST_DAEMON_VER,
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        let hello =
            read_len_prefixed_json::<_, Value>(&mut stream, TEST_MAX_SHIM_CONTROL_MESSAGE_LEN)
                .await
                .unwrap()
                .expect("shim should send ShimHello before reading ShimHelloAck");
        assert_eq!(
            hello.get("schema"),
            Some(&Value::from(TEST_SHIM_SCHEMA_VERSION))
        );
        write_len_prefixed_json(
            &mut stream,
            &json!({ "schema": bad_schema }),
            TEST_MAX_SHIM_CONTROL_MESSAGE_LEN,
        )
        .await
        .unwrap();
    });

    let project = TestProject::new("mcp-bad-ack-project");
    let xdg_config_home = module_server.path.join("xdg-config");
    fs::create_dir_all(&xdg_config_home).unwrap();
    let mut shim = spawn_shim(&connection_file_path, &project.path, &xdg_config_home);

    let exit = timeout(SETUP_TIMEOUT, shim.child.wait())
        .await
        .expect("shim should exit on unsupported ShimHelloAck schema")
        .expect("waiting for shim exit failed");
    assert!(
        !exit.success(),
        "shim should fail when the module replies with an unsupported ShimHelloAck schema"
    );
    let mut stderr = shim
        .stderr
        .take()
        .expect("shim stderr should be available for schema mismatch assertions");
    let stderr = read_child_stderr(&mut stderr).await;
    assert!(
        stderr.contains(&format!(
            "unsupported ShimHelloAck schema {bad_schema} (expected {TEST_SHIM_SCHEMA_VERSION})"
        )),
        "shim stderr should report the typed schema mismatch, got: {stderr}"
    );
    server_task.await.unwrap();
}

#[tokio::test]
async fn mcp_module_without_spawn_attestation_exits_loud_before_serving() {
    // The facade fronts remote-model callers; its binds must carry the attested
    // reserved principal. Started WITHOUT the daemon-injected env (a manual
    // launch or an injection regression), it must refuse to serve — the
    // alternative is silently binding as the trusted `direct` principal.
    let server = TestServer::start().await;
    let module_connection_file = server.daemon.temp_dir.join("mcp-unattested-module.json");
    let xdg_config_home = server.daemon.temp_dir.join("mcp-unattested-xdg-config");
    fs::create_dir_all(&xdg_config_home).unwrap();

    let mut command = Command::new(env!("CARGO_BIN_EXE_ck-subc-mcp"));
    command
        .arg("module")
        .arg("--subc")
        .arg(&server.daemon.connection_file_path)
        .arg("--connection-file")
        .arg(&module_connection_file)
        .env("XDG_CONFIG_HOME", &xdg_config_home)
        .env_remove(subc_protocol::SUBC_MODULE_ID_ENV)
        .env_remove(subc_protocol::SUBC_LAUNCH_NONCE_ENV)
        .stderr(process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().unwrap();
    let mut stderr = child.stderr.take().expect("module stderr should be piped");

    let exit = timeout(SETUP_TIMEOUT, child.wait())
        .await
        .expect("unattested module must exit promptly (it did not within SETUP_TIMEOUT)")
        .expect("waiting for the module child to exit failed");
    assert!(
        !exit.success(),
        "unattested module must exit with a failure status, got {exit:?}"
    );
    let mut stderr_text = String::new();
    stderr.read_to_string(&mut stderr_text).await.unwrap();
    assert!(
        stderr_text.contains("spawn attestation"),
        "failure must name the missing attestation, got: {stderr_text}"
    );
    assert!(
        !module_connection_file.exists(),
        "unattested module must never publish its connection file (never serves)"
    );
}

#[tokio::test]
async fn mcp_module_rejects_unsupported_shim_hello_schema_without_opening_routes() {
    let bad_schema = TEST_SHIM_SCHEMA_VERSION + 1;
    let server = TestServer::start().await;
    let provider_events_path = server
        .daemon
        .temp_dir
        .join("mcp-bad-shim-hello-aft-events.jsonl");
    let provider = supervisor(&server)
        .spawn(stub_spec(
            "aft",
            &provider_events_path,
            &[("FAKE_AFT_TOOLS", "read")],
        ))
        .unwrap();
    wait_for_registration(&server.daemon.registry, "aft", SETUP_TIMEOUT).await;

    let xdg_config_home = server.daemon.temp_dir.join("mcp-bad-shim-hello-xdg-config");
    fs::create_dir_all(&xdg_config_home).unwrap();
    let module_connection_file = server
        .daemon
        .temp_dir
        .join("mcp-bad-shim-hello-module.json");
    let (mut module, mut module_stderr) = spawn_module_with_stderr(
        &server.daemon.connection_file_path,
        &module_connection_file,
        &xdg_config_home,
    );
    wait_for_module_connection_file(&mut module, &module_connection_file, READ_TIMEOUT).await;

    let project = TestProject::new("mcp-bad-shim-hello-project");
    let mut stream = connect_control_client(&module_connection_file)
        .await
        .unwrap();
    write_len_prefixed_json(
        &mut stream,
        &json!({
            "schema": bad_schema,
            "project_root": project.path,
            "harness": "subc-mcp-test",
            "shim_session_id": "shim-bad-schema"
        }),
        TEST_MAX_SHIM_CONTROL_MESSAGE_LEN,
    )
    .await
    .unwrap();
    let response =
        read_len_prefixed_json::<_, Value>(&mut stream, TEST_MAX_SHIM_CONTROL_MESSAGE_LEN)
            .await
            .unwrap();
    assert!(
        response.is_none(),
        "module should close the shim socket instead of replying to an unsupported ShimHello"
    );
    let stderr = wait_for_child_stderr_contains(
        &mut module_stderr,
        &format!("unsupported ShimHello schema {bad_schema} (expected {TEST_SHIM_SCHEMA_VERSION})"),
        READ_TIMEOUT,
    )
    .await;
    assert!(
        stderr.contains("unsupported ShimHello schema"),
        "module stderr should report the typed schema mismatch, got: {stderr}"
    );
    assert_no_stub_event_within(&provider_events_path, QUIET_TIMEOUT, |event| {
        event.get("kind") == Some(&Value::String("attach".to_owned()))
    })
    .await;
    wait_for_binding_count(&server.daemon.forwarding, 0, READ_TIMEOUT).await;

    if module.try_wait().unwrap().is_none() {
        let _ = module.start_kill();
        let _ = timeout(Duration::from_secs(2), module.wait()).await;
    }
    provider.stop().await.unwrap();
}

async fn list_tool_names(harness: &McpHarness) -> Vec<String> {
    list_tool_names_on_peer(harness.client.peer()).await
}

async fn list_tool_names_on_peer(peer: &rmcp::service::Peer<RoleClient>) -> Vec<String> {
    let mut names = peer
        .list_tools(None)
        .await
        .unwrap()
        .tools
        .into_iter()
        .map(|tool| tool.name.to_string())
        .collect::<Vec<_>>();
    names.sort();
    names
}

async fn expect_shim_attach_failure(
    label: &str,
    provider_specs: Vec<StubProvider<'_>>,
    user_config: Option<&str>,
    project_config: Option<&str>,
) {
    let server = TestServer::start().await;
    let mut providers = Vec::new();
    for provider_spec in provider_specs {
        let events_path = server
            .daemon
            .temp_dir
            .join(format!("{label}-{}-events.jsonl", provider_spec.module_id));
        let provider = supervisor(&server)
            .spawn(stub_spec(
                provider_spec.module_id,
                &events_path,
                &provider_spec.env,
            ))
            .unwrap();
        wait_for_registration(
            &server.daemon.registry,
            provider_spec.module_id,
            READ_TIMEOUT,
        )
        .await;
        providers.push(provider);
    }

    let user_config_home = server.daemon.temp_dir.join(format!("{label}-xdg-config"));
    fs::create_dir_all(&user_config_home).unwrap();
    if let Some(user_config) = user_config {
        write_user_mcp_config(&user_config_home, user_config);
    }

    let module_connection_file = server
        .daemon
        .temp_dir
        .join(format!("{label}-subc-mcp.json"));
    let mut module = spawn_module(
        &server.daemon.connection_file_path,
        &module_connection_file,
        &user_config_home,
    );
    wait_for_module_connection_file(&mut module, &module_connection_file, READ_TIMEOUT).await;

    let project = TestProject::new(label);
    if let Some(project_config) = project_config {
        write_project_mcp_config(&project.path, project_config);
    }

    let mut shim = spawn_shim(&module_connection_file, &project.path, &user_config_home);
    // Fail-closed contract: when the module rejects the attach it drops the shim's
    // transport socket, so the shim's socket->stdout copy hits EOF and the shim
    // process must exit, closing its stdout. A real MCP host observes fail-closed
    // through that child-process / stdio lifecycle, so assert the production signal
    // directly: the shim EXITS, promptly and cleanly.
    //
    // The earlier check instead waited for an in-process rmcp client's serve() to
    // resolve, which hung to the timeout under load: the shim saw the socket EOF in
    // well under a millisecond but did NOT exit, because tokio::io::stdin()'s
    // uncancellable blocking-read thread stranded runtime shutdown until the client
    // dropped the shim's stdin at the timeout. The product fix (explicit
    // process::exit in main) makes the shim exit on socket EOF; this assertion
    // guards that fix.
    let exit = timeout(SETUP_TIMEOUT, shim.child.wait())
        .await
        .expect("shim must exit promptly when attach fails (it did not within SETUP_TIMEOUT)")
        .expect("waiting for the shim child to exit failed");
    assert!(
        exit.success(),
        "shim should exit cleanly on fail-closed attach (socket EOF), got {exit:?}"
    );
    wait_for_binding_count(&server.daemon.forwarding, 0, READ_TIMEOUT).await;

    if module.try_wait().unwrap().is_none() {
        let _ = module.start_kill();
        let _ = timeout(Duration::from_secs(2), module.wait()).await;
    }
    for provider in providers {
        provider.stop().await.unwrap();
    }
}

async fn start_test_daemon_with_process_liveness_and_supervisor(
    name: &str,
    process_liveness: Arc<SupervisorProcessLiveness>,
    supervisor_handle: SupervisorHandle,
) -> TestDaemon {
    let temp_dir = unique_temp_dir(name);
    fs::create_dir_all(&temp_dir).unwrap();
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let connection_file_path = temp_dir.join("subc-conn.json");
    let conn = ConnectionInfo {
        schema: SCHEMA_VERSION,
        wire_version: None,
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
    let control = ControlHandler::new(Arc::clone(&registry))
        .with_process_liveness(process_liveness)
        .with_supervisor(supervisor_handle);
    let forwarding = control.forwarding();
    let router = Arc::new(Router::with_control_handler(Arc::new(control)));
    let auth = ServerAuth::new(conn.key, conn.daemon_id, conn.daemon_ver);
    let task = tokio::spawn(serve_listener(listener, router, auth));

    TestDaemon {
        registry,
        forwarding,
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
    .with_forwarding(Arc::clone(&server.daemon.forwarding))
    .with_handle(server.supervisor_handle.clone())
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
        reserved: false,
        reserved_prefixes: Vec::new(),
    }
}

fn mcp_module_spec(
    module_id: &str,
    module_connection_file: &Path,
    xdg_config_home: &Path,
) -> ModuleSpec {
    ModuleSpec {
        module_id: module_id.to_owned(),
        program: PathBuf::from(env!("CARGO_BIN_EXE_ck-subc-mcp")),
        args: vec![
            "module".to_string(),
            "--connection-file".to_string(),
            module_connection_file.display().to_string(),
        ],
        env: vec![(
            "XDG_CONFIG_HOME".to_string(),
            xdg_config_home.display().to_string(),
        )],
        reserved: false,
        reserved_prefixes: Vec::new(),
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

fn spawn_module(
    subc_connection_file: &Path,
    module_connection_file: &Path,
    xdg_config_home: &Path,
) -> Child {
    // Discard stderr with /dev/null, NOT a dropped pipe handle: the attested
    // module eprintln!s its registration line, and writing to a closed pipe
    // makes eprintln! panic (exit 101) in the child.
    let mut command = module_command(
        subc_connection_file,
        module_connection_file,
        xdg_config_home,
    );
    command.stderr(process::Stdio::null());
    command.spawn().unwrap()
}

fn spawn_module_with_extra_env(
    subc_connection_file: &Path,
    module_connection_file: &Path,
    xdg_config_home: &Path,
    extra_env: &[(&str, &str)],
) -> Child {
    let mut command = module_command(
        subc_connection_file,
        module_connection_file,
        xdg_config_home,
    );
    for (key, value) in extra_env {
        command.env(key, value);
    }
    command.stderr(process::Stdio::null());
    command.spawn().unwrap()
}

fn spawn_module_with_stderr(
    subc_connection_file: &Path,
    module_connection_file: &Path,
    xdg_config_home: &Path,
) -> (Child, ChildStderr) {
    let mut command = module_command(
        subc_connection_file,
        module_connection_file,
        xdg_config_home,
    );
    command.stderr(process::Stdio::piped());
    let mut child = command.spawn().unwrap();
    let stderr = child.stderr.take().expect("module stderr should be piped");
    (child, stderr)
}

fn module_command(
    subc_connection_file: &Path,
    module_connection_file: &Path,
    xdg_config_home: &Path,
) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ck-subc-mcp"));
    command
        .arg("module")
        .arg("--subc")
        .arg(subc_connection_file)
        .arg("--connection-file")
        .arg(module_connection_file)
        .env("XDG_CONFIG_HOME", xdg_config_home)
        .env(subc_protocol::SUBC_MODULE_ID_ENV, TEST_MCP_MODULE_ID)
        .env(subc_protocol::SUBC_LAUNCH_NONCE_ENV, TEST_MCP_LAUNCH_NONCE)
        .kill_on_drop(true);
    command
}

async fn wait_for_module_connection_file(child: &mut Child, path: &Path, wait: Duration) {
    let deadline = Instant::now() + wait;
    loop {
        if subc_transport::read_for_client(path).is_ok() {
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

fn spawn_shim(
    module_connection_file: &Path,
    project_root: &Path,
    xdg_config_home: &Path,
) -> ShimProcess {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ck-subc-mcp"));
    command
        .arg("shim")
        .arg("--module-connection-file")
        .arg(module_connection_file)
        .env("CLAUDE_PROJECT_DIR", project_root)
        .env("XDG_CONFIG_HOME", xdg_config_home)
        .stdin(process::Stdio::piped())
        .stdout(process::Stdio::piped())
        .stderr(process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().unwrap();
    let stdin = child.stdin.take().expect("shim stdin should be piped");
    let stdout = child.stdout.take().expect("shim stdout should be piped");
    let stderr = child.stderr.take().expect("shim stderr should be piped");
    ShimProcess {
        child,
        stdin: Some(stdin),
        stdout: Some(stdout),
        stderr: Some(stderr),
    }
}

fn write_project_mcp_config(project_root: &Path, doc: &str) {
    let dir = project_root.join(".cortexkit");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("mcp.jsonc"), doc).unwrap();
}

fn write_user_mcp_config(xdg_config_home: &Path, doc: &str) {
    let dir = xdg_config_home.join("cortexkit");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("mcp.jsonc"), doc).unwrap();
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

async fn wait_for_supervisor_entry(
    path: &Path,
    module_id: &str,
    predicate: impl Fn(&SupervisorEntry) -> bool,
    wait: Duration,
) -> SupervisorEntry {
    let deadline = Instant::now() + wait;
    let mut corr = 10_000;
    loop {
        let modules = supervisor_modules(path, corr).await;
        if let Some(entry) = modules
            .into_iter()
            .find(|entry| entry.module_id == module_id && predicate(entry))
        {
            return entry;
        }
        if Instant::now() >= deadline {
            let modules = supervisor_modules(path, corr + 100_000).await;
            panic!(
                "module {module_id} did not reach expected supervisor state within {wait:?}; modules: {modules:?}"
            );
        }
        corr += 1;
        sleep(Duration::from_millis(20)).await;
    }
}

async fn supervisor_modules(path: &Path, corr: u64) -> Vec<SupervisorEntry> {
    let mut client = wait_for_control_client(path, SETUP_TIMEOUT).await;
    match control_rpc_on_stream(&mut client, corr, ClientControlRequest::SupervisorList {}).await {
        ClientControlResponse::SupervisorList { modules, .. } => modules,
        other => panic!("unexpected supervisor.list response: {other:?}"),
    }
}

async fn catalog_modules(path: &Path, module_id: Option<&str>, corr: u64) -> Vec<CatalogEntry> {
    let mut client = wait_for_control_client(path, SETUP_TIMEOUT).await;
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

async fn wait_for_control_client(path: &Path, wait: Duration) -> TcpStream {
    let deadline = Instant::now() + wait;
    loop {
        match connect_control_client(path).await {
            Ok(client) => return client,
            Err(err) if Instant::now() < deadline => {
                let _ = err;
                sleep(Duration::from_millis(20)).await;
            }
            Err(err) => panic!("daemon did not accept authenticated client within {wait:?}: {err}"),
        }
    }
}

async fn connect_control_client(path: &Path) -> Result<TcpStream, String> {
    let conn = subc_transport::read_for_client(path).map_err(|source| source.to_string())?;
    let endpoint = conn
        .endpoints
        .first()
        .ok_or_else(|| format!("{} has no endpoints", path.display()))?;
    let mut stream = TcpStream::connect((endpoint.host.as_str(), endpoint.port))
        .await
        .map_err(|source| source.to_string())?;
    authenticate_client(&mut stream, &conn, Duration::from_secs(2))
        .await
        .map_err(|source| source.to_string())?;
    Ok(stream)
}

async fn open_route<S>(stream: &mut S, module_id: &str, corr: u64) -> WireRoute
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    match control_rpc_on_stream(
        stream,
        corr,
        ClientControlRequest::RouteOpen {
            target: RouteTarget::ToolProvider {
                module_id: module_id.to_string(),
            },
            identity: route_identity(module_id, corr),
            consumer_identity: None,
            consumer_capabilities: None,
            admission_facts: None,
        },
    )
    .await
    {
        ClientControlResponse::RouteOpen {
            route_channel,
            route_epoch,
        } => WireRoute {
            channel: route_channel,
            epoch: route_epoch,
        },
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
    let frame = control_frame_on_stream(stream, corr, request).await;
    match frame.header.ty {
        FrameType::Response => serde_json::from_slice(&frame.body).unwrap(),
        FrameType::Error => panic!(
            "control RPC returned error: {:?}",
            serde_json::from_slice::<Value>(&frame.body).unwrap()
        ),
        ty => panic!("unexpected control RPC frame type: {ty:?}"),
    }
}

async fn control_error_on_stream<S>(
    stream: &mut S,
    corr: u64,
    request: ClientControlRequest,
) -> ErrorBody
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let frame = control_frame_on_stream(stream, corr, request).await;
    match frame.header.ty {
        FrameType::Error => serde_json::from_slice(&frame.body).unwrap(),
        FrameType::Response => panic!(
            "control RPC unexpectedly succeeded: {:?}",
            serde_json::from_slice::<ClientControlResponse>(&frame.body).unwrap()
        ),
        ty => panic!("unexpected control RPC frame type: {ty:?}"),
    }
}

async fn control_frame_on_stream<S>(
    stream: &mut S,
    corr: u64,
    request: ClientControlRequest,
) -> Frame
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
    frame
}

fn control_request_frame(corr: u64, request: ClientControlRequest) -> Frame {
    let body = serde_json::to_vec(&request).unwrap();
    Frame::build(
        FrameType::Request,
        Flags::new(false, Priority::Passive, false),
        0,
        0,
        corr,
        body,
    )
    .unwrap()
}

fn data_request(route: WireRoute, corr: u64, body: &[u8]) -> Frame {
    Frame::build(
        FrameType::Request,
        Flags::new(false, Priority::Interactive, false),
        route.channel,
        route.epoch,
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

fn route_identity(label: &str, corr: u64) -> BindIdentity {
    let project_root = unique_temp_dir(&format!("mcp-route-{label}-{corr}"));
    fs::create_dir_all(&project_root).unwrap();
    BindIdentity {
        project_root,
        harness: "subc-mcp-test".to_string(),
        session: format!("session-{corr}"),
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

async fn assert_no_stub_event_within<F>(path: &Path, wait: Duration, matches: F)
where
    F: Fn(&Value) -> bool,
{
    let deadline = Instant::now() + wait;
    loop {
        let events = stub_events(path).unwrap();
        if let Some(event) = events.iter().find(|event| matches(event)) {
            panic!("unexpected stub event within {wait:?}: {event:?}; events: {events:?}");
        }
        if Instant::now() >= deadline {
            return;
        }
        sleep(Duration::from_millis(10)).await;
    }
}

async fn assert_no_new_stub_events_within(path: &Path, baseline: &[Value], wait: Duration) {
    let deadline = Instant::now() + wait;
    loop {
        let events = stub_events(path).unwrap();
        assert_eq!(
            events, baseline,
            "provider received a frame during an ack-only invocation"
        );
        if Instant::now() >= deadline {
            return;
        }
        sleep(Duration::from_millis(10)).await;
    }
}

async fn wait_for_binding_count(forwarding: &ForwardingTable, expected: usize, wait: Duration) {
    let deadline = Instant::now() + wait;
    loop {
        let count = forwarding.active_binding_count().unwrap();
        if count == expected {
            return;
        }
        if Instant::now() >= deadline {
            panic!("forwarding binding count {count} did not become {expected} within {wait:?}");
        }
        sleep(Duration::from_millis(10)).await;
    }
}

async fn assert_counter_stays(counter: &AtomicUsize, baseline: usize, label: &str, wait: Duration) {
    let deadline = Instant::now() + wait;
    loop {
        let current = counter.load(Ordering::SeqCst);
        assert_eq!(
            current, baseline,
            "{label} changed unexpectedly: count is {current}, baseline {baseline}"
        );
        if Instant::now() >= deadline {
            return;
        }
        sleep(Duration::from_millis(10)).await;
    }
}

async fn run_raw_provider(
    connection_file_path: &Path,
    module_id: &str,
    tool_name: &str,
    behavior: RawProviderBehavior,
    route_cancel_count: Arc<AtomicUsize>,
    mut shutdown_rx: oneshot::Receiver<()>,
) {
    let mut stream = connect_control_client(connection_file_path)
        .await
        .expect("raw test provider should authenticate to the daemon");
    let hello = ModuleHelloBody {
        manifest: raw_provider_manifest(module_id, tool_name),
        protocol_ver: PROTOCOL_VERSION,
        control_ops: None,
        launch_nonce: None,
    };
    let body = serde_json::to_vec(&hello).unwrap();
    let frame = Frame::build(
        FrameType::Hello,
        Flags::new(false, Priority::Passive, false),
        0,
        0,
        1,
        body,
    )
    .unwrap();
    write_frame(&mut stream, &frame).await.unwrap();
    stream.flush().await.unwrap();

    let ack = read_frame_timeout(&mut stream).await;
    assert_eq!(ack.header.ty, FrameType::HelloAck);
    let _ack: ModuleHelloAckBody = serde_json::from_slice(&ack.body).unwrap();

    let mut route = None;
    loop {
        tokio::select! {
            _ = &mut shutdown_rx => return,
            frame = read_frame(&mut stream) => {
                let Some(frame) = frame.unwrap() else {
                    return;
                };
                if frame.header.channel != 0
                    && route != Some(WireRoute::from_frame(&frame))
                {
                    continue;
                }
                match frame.header.ty {
                    FrameType::Request if frame.header.channel == 0 => {
                        let request: ModuleControlRequest = serde_json::from_slice(&frame.body).unwrap();
                        match request {
                            ModuleControlRequest::RouteBind {
                                route_channel,
                                epoch,
                                ..
                            } => {
                                let next_route = WireRoute {
                                    channel: route_channel,
                                    epoch,
                                };
                                let body = serde_json::to_vec(
                                    &ModuleControlResponse::RouteBindAck {},
                                )
                                .unwrap();
                                let response = Frame::build_with_version(
                                    frame.header.ver,
                                    FrameType::Response,
                                    Flags::new(false, Priority::Passive, false),
                                    0,
                                    0,
                                    frame.header.corr,
                                    body,
                                )
                                .unwrap();
                                write_frame(&mut stream, &response).await.unwrap();
                                stream.flush().await.unwrap();
                                route = Some(next_route);
                            }
                            ModuleControlRequest::HealthCheck {} => {}
                        }
                    }
                    FrameType::Request if route == Some(WireRoute::from_frame(&frame)) => {
                        match behavior {
                            RawProviderBehavior::MalformedProgress => {
                                let push = Frame::build_with_version(
                                    frame.header.ver,
                                    FrameType::Push,
                                    Flags::new(false, Priority::Passive, true),
                                    frame.header.channel,
                                    frame.header.epoch,
                                    frame.header.corr,
                                    b"{malformed progress".to_vec(),
                                )
                                .unwrap();
                                write_frame(&mut stream, &push).await.unwrap();
                                stream.flush().await.unwrap();
                            }
                            RawProviderBehavior::MalformedResult => {
                                let response = Frame::build_with_version(
                                    frame.header.ver,
                                    FrameType::Response,
                                    frame.header.flags,
                                    frame.header.channel,
                                    frame.header.epoch,
                                    frame.header.corr,
                                    b"{malformed tool result".to_vec(),
                                )
                                .unwrap();
                                write_frame(&mut stream, &response).await.unwrap();
                                stream.flush().await.unwrap();
                            }
                        }
                    }
                    FrameType::Cancel if route == Some(WireRoute::from_frame(&frame)) => {
                        route_cancel_count.fetch_add(1, Ordering::SeqCst);
                    }
                    FrameType::Goodbye if frame.header.channel == 0 => return,
                    _ => {}
                }
            }
        }
    }
}

fn raw_provider_manifest(module_id: &str, tool_name: &str) -> ModuleManifest {
    ModuleManifest {
        module_id: module_id.to_owned(),
        module_version: "0.0.0-raw-test".to_string(),
        protocol_ver: PROTOCOL_VERSION,
        trust_tier: TrustTier::FirstParty,
        provides: vec![ProviderRole::ToolProvider {
            tools: vec![ProviderTool {
                name: tool_name.to_owned(),
                description: None,
                execution_mode: ExecutionMode::Pure,
                schema: json!({"type": "object"}),
            }],
            identity_scope: vec![IdentityScope::Project, IdentityScope::Session],
            concurrency: Concurrency::ModuleManaged,
            emits_push: true,
            sub_supervises: true,
        }],
        consumes: Vec::new(),
        bindings: Bindings {
            storage: StorageBinding {
                kind: StorageKind::Sqlite,
                scope: StorageScope::Project,
                owns_schema: true,
            },
            vault_grants: Vec::new(),
            identity: IdentityBinding {
                requires: vec![IdentityScope::Project],
                optional: vec![IdentityScope::Session],
            },
        },
        capabilities: None,
        self_signals: None,
        provenance: None,
    }
}

async fn read_len_prefixed_json<R, T>(reader: &mut R, max_len: u32) -> Result<Option<T>, String>
where
    R: AsyncRead + Unpin,
    T: serde::de::DeserializeOwned,
{
    let mut len_bytes = [0u8; 4];
    match reader.read_exact(&mut len_bytes).await {
        Ok(_) => {}
        Err(err) if err.kind() == ErrorKind::UnexpectedEof => return Ok(None),
        Err(err) => return Err(format!("failed to read message length prefix: {err}")),
    }
    let len = u32::from_le_bytes(len_bytes);
    if len > max_len {
        return Err(format!(
            "length-prefixed message too large: {len} bytes (max {max_len})"
        ));
    }
    let mut bytes = vec![0u8; len as usize];
    if !bytes.is_empty() {
        reader
            .read_exact(&mut bytes)
            .await
            .map_err(|err| format!("failed to read message body: {err}"))?;
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|err| format!("failed to decode JSON message: {err}"))
}

async fn write_len_prefixed_json<W, T>(
    writer: &mut W,
    value: &T,
    max_len: u32,
) -> Result<(), String>
where
    W: AsyncWrite + Unpin,
    T: serde::Serialize,
{
    let bytes =
        serde_json::to_vec(value).map_err(|err| format!("failed to encode JSON message: {err}"))?;
    let len = u32::try_from(bytes.len()).map_err(|_| {
        format!(
            "length-prefixed message too large for u32: {} bytes",
            bytes.len()
        )
    })?;
    if len > max_len {
        return Err(format!(
            "length-prefixed message too large: {len} bytes (max {max_len})"
        ));
    }
    writer
        .write_all(&len.to_le_bytes())
        .await
        .map_err(|err| format!("failed to write message length prefix: {err}"))?;
    if !bytes.is_empty() {
        writer
            .write_all(&bytes)
            .await
            .map_err(|err| format!("failed to write message body: {err}"))?;
    }
    writer
        .flush()
        .await
        .map_err(|err| format!("failed to flush JSON message: {err}"))
}

async fn read_child_stderr(stderr: &mut ChildStderr) -> String {
    let mut bytes = Vec::new();
    stderr.read_to_end(&mut bytes).await.unwrap();
    String::from_utf8_lossy(&bytes).into_owned()
}

async fn wait_for_child_stderr_contains(
    stderr: &mut ChildStderr,
    needle: &str,
    wait: Duration,
) -> String {
    let deadline = Instant::now() + wait;
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 512];
    loop {
        let text = String::from_utf8_lossy(&bytes).into_owned();
        if text.contains(needle) {
            return text;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            remaining > Duration::ZERO,
            "stderr did not contain '{needle}' within {wait:?}; stderr so far: {text}"
        );
        let read = timeout(remaining, stderr.read(&mut chunk))
            .await
            .unwrap_or_else(|_| {
                panic!("timed out waiting for stderr to contain '{needle}'; stderr so far: {text}")
            })
            .unwrap();
        if read == 0 {
            panic!("stderr closed before containing '{needle}'; stderr so far: {text}");
        }
        bytes.extend_from_slice(&chunk[..read]);
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

fn result_json(result: &rmcp::model::CallToolResult) -> Value {
    serde_json::from_str(result_text(result)).unwrap_or_else(|err| {
        panic!(
            "tool result text should be JSON, got {:?}: {err}",
            result_text(result)
        )
    })
}

fn assert_mcp_error(error: ServiceError, code: ErrorCode, message: &str) {
    match error {
        ServiceError::McpError(error) => {
            assert_eq!(error.code, code);
            assert_eq!(error.message, message);
        }
        other => panic!("expected MCP error {code:?} with message {message:?}, got {other:?}"),
    }
}

fn assert_unknown_tool_error(error: ServiceError, name: &str) {
    match error {
        ServiceError::McpError(error) => {
            assert_eq!(error.code, rmcp::model::ErrorCode::INVALID_PARAMS);
            assert_eq!(error.message, format!("unknown tool '{name}'"));
        }
        other => panic!("expected invalid-params unknown-tool error for {name}, got {other:?}"),
    }
}

fn unique_temp_dir(label: &str) -> PathBuf {
    let nonce = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    env::temp_dir().join(format!("sc-{label}-{}-{nonce}", process::id()))
}
