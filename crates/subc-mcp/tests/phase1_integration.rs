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
        CallToolRequest, CallToolRequestParams, CancelledNotificationParam, ClientRequest,
        JsonObject, ProgressNotificationParam,
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
    session::ConfigTier, BindIdentity, ErrorBody, Flags, FrameType, Priority, RouteTarget,
};
use subc_transport::{
    authenticate_client, generate_daemon_id, generate_key, write_atomic, ConnectionInfo, Endpoint,
    SCHEMA_VERSION,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::Mutex as TokioMutex,
    task::JoinHandle,
    time::{sleep, timeout, Instant},
};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
const TEST_DAEMON_VER: &str = "test-subc-mcp";
const SETUP_TIMEOUT: Duration = Duration::from_secs(10);
const READ_TIMEOUT: Duration = SETUP_TIMEOUT;

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
}

impl TestMcpClient {
    fn new() -> Self {
        Self {
            progress: Arc::new(TokioMutex::new(Vec::new())),
            progress_count: Arc::new(AtomicUsize::new(0)),
            tool_list_changed_count: Arc::new(AtomicUsize::new(0)),
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
async fn mcp_initialize_advertises_tools_capability() {
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
    assert_eq!(server_info.server_info.name, "subc-mcp");

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
async fn mcp_two_tier_config_project_overrides_user_and_null_deletes_override() {
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

    assert_eq!(
        list_tool_names(&harness).await,
        vec!["aft_read", "aft_write"]
    );
    let attach = wait_for_stub_event(harness.provider_events_path("aft"), READ_TIMEOUT, |event| {
        event.get("kind") == Some(&Value::String("attach".to_owned()))
    })
    .await;
    let tiers = attach
        .get("config")
        .and_then(Value::as_array)
        .expect("route.bind should receive raw config tiers");
    assert_eq!(tiers.len(), 2);
    assert_eq!(
        tiers[0].get("tier"),
        Some(&Value::String("user".to_owned()))
    );
    assert_eq!(
        tiers[1].get("tier"),
        Some(&Value::String("project".to_owned()))
    );

    harness.shutdown().await;
}

#[tokio::test]
async fn mcp_namespace_override_changes_exposed_prefix() {
    let project_config = r#"
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
        None,
        Some(project_config),
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
async fn mcp_namespace_collision_fails_attach_closed() {
    let project_config = r#"
    {
      "version": 1,
      "providers": {
        "aft": { "namespace": "dup" },
        "mc": { "namespace": "dup" }
      }
    }
    "#;
    expect_shim_attach_failure(
        "mcp-collision",
        vec![
            StubProvider::new("aft", &[("FAKE_AFT_TOOLS", "read")]),
            StubProvider::new("mc", &[("FAKE_AFT_TOOLS", "read")]),
        ],
        None,
        Some(project_config),
    )
    .await;
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
            config: route_config(),
        },
    )
    .await;
    assert_eq!(err.code, "target_unavailable");
    assert!(
        err.message
            .contains("does not provide the requested target"),
        "unexpected route.open error for non-routable mcp: {err:?}"
    );

    let route_channel = open_route(&mut client, "aft", 2_002).await;
    write_frame(
        &mut client,
        &data_request(
            route_channel,
            2_003,
            br#"{ "name": "read", "arguments": { "path": "demo" } }"#,
        ),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();
    let response = read_frame_timeout(&mut client).await;
    assert_eq!(response.header.ty, FrameType::Response);
    assert_eq!(response.header.channel, route_channel);
    assert_eq!(response.header.corr, 2_003);
    let body: Value = serde_json::from_slice(&response.body).unwrap();
    let text = body["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("fake-aft tool read called"),
        "aft provider should still serve tool calls after mcp registers: {body:?}"
    );

    mcp.stop().await.unwrap();
    provider.stop().await.unwrap();
}

async fn list_tool_names(harness: &McpHarness) -> Vec<String> {
    let mut names = harness
        .client
        .peer()
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
    }
}

fn mcp_module_spec(
    module_id: &str,
    module_connection_file: &Path,
    xdg_config_home: &Path,
) -> ModuleSpec {
    ModuleSpec {
        module_id: module_id.to_owned(),
        program: PathBuf::from(env!("CARGO_BIN_EXE_subc-mcp")),
        args: vec![
            "module".to_string(),
            "--connection-file".to_string(),
            module_connection_file.display().to_string(),
        ],
        env: vec![(
            "XDG_CONFIG_HOME".to_string(),
            xdg_config_home.display().to_string(),
        )],
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
    let mut command = Command::new(env!("CARGO_BIN_EXE_subc-mcp"));
    command
        .arg("module")
        .arg("--subc")
        .arg(subc_connection_file)
        .arg("--connection-file")
        .arg(module_connection_file)
        .env("XDG_CONFIG_HOME", xdg_config_home)
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

fn spawn_shim(
    module_connection_file: &Path,
    project_root: &Path,
    xdg_config_home: &Path,
) -> ShimProcess {
    let mut command = Command::new(env!("CARGO_BIN_EXE_subc-mcp"));
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
    ShimProcess {
        child,
        stdin: Some(stdin),
        stdout: Some(stdout),
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
    let conn = subc_transport::read(path).map_err(|source| source.to_string())?;
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

async fn open_route<S>(stream: &mut S, module_id: &str, corr: u64) -> u16
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
            config: route_config(),
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

fn route_identity(label: &str, corr: u64) -> BindIdentity {
    let project_root = unique_temp_dir(&format!("mcp-route-{label}-{corr}"));
    fs::create_dir_all(&project_root).unwrap();
    BindIdentity {
        project_root,
        harness: "subc-mcp-test".to_string(),
        session: format!("session-{corr}"),
    }
}

fn route_config() -> Vec<ConfigTier> {
    vec![ConfigTier {
        tier: "project".to_string(),
        source: "subc-mcp-test".to_string(),
        doc: "{}".to_string(),
    }]
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
