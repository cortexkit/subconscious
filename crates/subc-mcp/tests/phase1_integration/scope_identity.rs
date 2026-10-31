use super::*;
use subc_protocol::{manifest::CapabilityDeclarations, scope::ScopeStatus};

#[path = "attribution_proxy.rs"]
mod attribution_proxy;
use attribution_proxy::{AttributionMode, AttributionProxy};

struct RootsClient {
    roots: Option<Vec<PathBuf>>,
    non_file: bool,
    fail_roots: bool,
    /// How many times the gateway asked this host for its roots.
    roots_asked: Arc<AtomicUsize>,
}

enum RootsMode {
    Absent,
    Empty,
    Folder,
    NonFile,
    Error,
}

impl ClientHandler for RootsClient {
    fn get_info(&self) -> ClientConfig {
        let mut info = host_client_config(ClientConfig::default());
        if self.roots.is_some() {
            info.capabilities.roots = Some(Default::default());
        }
        info
    }

    // rmcp 3 deprecates roots (dropped in 2026-07-28); the gateway still asks
    // for them on the 2025-11-25 sessions these tests open.
    #[allow(deprecated)]
    async fn list_roots(
        &self,
        _context: rmcp::service::RequestContext<RoleClient>,
    ) -> Result<rmcp::model::ListRootsResult, McpErrorData> {
        self.roots_asked.fetch_add(1, Ordering::SeqCst);
        if self.fail_roots {
            return Err(McpErrorData::internal_error(
                "roots service is unavailable",
                None,
            ));
        }
        if self.non_file {
            return Ok(serde_json::from_value(
                json!({"roots":[{"uri":"vscode-remote://ssh-remote/workspace"}]}),
            )
            .unwrap());
        }
        Ok(
            serde_json::from_value(json!({"roots": self.roots.iter().flatten().map(|path| {
            let path = path.to_string_lossy().replace('\\', "/");
            let prefix = if cfg!(windows) { "file:///" } else { "file://" };
            json!({"uri": format!("{prefix}{path}")})
        }).collect::<Vec<_>>()}))
            .unwrap(),
        )
    }
}

struct IdentityProvider {
    binds: mpsc::Receiver<(Value, Vec<u8>)>,
    requests: Arc<TokioMutex<Vec<Value>>>,
    task: JoinHandle<()>,
}

impl Drop for IdentityProvider {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl IdentityProvider {
    async fn start(server: &TestServer, id: &str, scoped: bool, resolve: Option<Value>) -> Self {
        let mut stream = connect_control_client(&server.daemon.connection_file_path)
            .await
            .unwrap();
        let mut manifest = raw_provider_manifest(id, "probe");
        if resolve.is_some() {
            manifest.provides = vec![serde_json::from_value(json!({
                "role": "management_surface", "operations": [{"name":"resolve", "kind":"query"}],
                "config_schema": {}, "observability": [], "identity_scope": []
            }))
            .unwrap()];
        }
        if scoped {
            manifest.capabilities = Some(CapabilityDeclarations {
                provides: vec![
                    "scope-workdir/v1".into(),
                    "mcp-conversation-binding/v1".into(),
                ],
                requires: vec![],
                must_never_reach: vec![],
            });
        }
        let hello = ModuleHelloBody {
            manifest,
            protocol_ver: PROTOCOL_VERSION,
            control_ops: None,
            launch_nonce: None,
        };
        let frame = Frame::build(
            FrameType::Hello,
            Flags::new(false, Priority::Passive, false),
            0,
            0,
            1,
            serde_json::to_vec(&hello).unwrap(),
        )
        .unwrap();
        write_frame(&mut stream, &frame).await.unwrap();
        assert_eq!(
            read_frame_timeout(&mut stream).await.header.ty,
            FrameType::HelloAck
        );
        let (tx, binds) = mpsc::channel(16);
        let requests = Arc::new(TokioMutex::new(Vec::new()));
        let seen = requests.clone();
        let task = tokio::spawn(async move {
            while let Some(frame) = read_frame(&mut stream).await.unwrap() {
                if frame.header.ty != FrameType::Request {
                    continue;
                }
                let request: Value = serde_json::from_slice(&frame.body).unwrap();
                let body = if frame.header.channel == 0 {
                    tx.send((request, frame.body.clone())).await.unwrap();
                    serde_json::to_value(ModuleControlResponse::RouteBindAck {}).unwrap()
                } else {
                    seen.lock().await.push(request);
                    resolve.clone().unwrap_or(json!({"content":[]}))
                };
                let response = Frame::build(
                    FrameType::Response,
                    Flags::new(false, Priority::Passive, false),
                    frame.header.channel,
                    frame.header.epoch,
                    frame.header.corr,
                    serde_json::to_vec(&body).unwrap(),
                )
                .unwrap();
                write_frame(&mut stream, &response).await.unwrap();
            }
        });
        Self {
            binds,
            requests,
            task,
        }
    }

    async fn bound(&mut self) -> Value {
        self.bound_raw().await.0
    }

    async fn bound_raw(&mut self) -> (Value, Vec<u8>) {
        timeout(READ_TIMEOUT, self.binds.recv())
            .await
            .unwrap()
            .unwrap()
    }
}

struct IdentityHarness {
    server: TestServer,
    module: SupervisedModule,
    shim: ShimProcess,
    client: RunningService<RoleClient, RootsClient>,
    provider: IdentityProvider,
    entorhinal: IdentityProvider,
    project: TestProject,
    root: PathBuf,
    proxy: AttributionProxy,
    roots_asked: Arc<AtomicUsize>,
}

impl IdentityHarness {
    async fn start(scoped: bool, roots: RootsMode, resolve: Value, token: bool) -> Self {
        Self::start_with_policy(scoped, roots, resolve, token, true, AttributionMode::Normal).await
    }

    async fn start_with_policy(
        scoped: bool,
        roots: RootsMode,
        resolve: Value,
        token: bool,
        reserved: bool,
        mode: AttributionMode,
    ) -> Self {
        Self::start_full(scoped, roots, resolve, token, reserved, mode, false).await
    }

    /// `modern` serves the host as an MCP 2026-07-28 client, which sends no
    /// `initialize`.
    #[allow(clippy::too_many_arguments)]
    async fn start_full(
        scoped: bool,
        roots: RootsMode,
        resolve: Value,
        token: bool,
        reserved: bool,
        mode: AttributionMode,
        modern: bool,
    ) -> Self {
        let server = TestServer::start().await;
        let proxy = AttributionProxy::start(&server, mode).await;
        let provider = IdentityProvider::start(&server, "scope-probe", scoped, None).await;
        let entorhinal = IdentityProvider::start(&server, "entorhinal", false, Some(resolve)).await;
        let home = server.daemon.temp_dir.join("home");
        fs::create_dir_all(&home).unwrap();
        let connection = server.daemon.temp_dir.join("mcp.json");
        let mut spec = mcp_module_spec(TEST_MCP_MODULE_ID, &connection, &home);
        spec.reserved = reserved;
        let module = supervisor(&server)
            .with_connection_file_path(proxy.connection_file.clone())
            .spawn(spec)
            .unwrap();
        timeout(READ_TIMEOUT, async {
            while !connection.exists() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let project = TestProject::new("scope-identity");
        let root = project.path.join("other-worktree");
        fs::create_dir_all(root.join("nested")).unwrap();
        // A worktree's .git is a file, unlike the main checkout's directory.
        fs::write(root.join(".git"), "gitdir: /unused-test-gitdir\n").unwrap();
        let mut shim = if token {
            spawn_shim(&connection, &project.path, &home)
        } else {
            let mut command = Command::from(cortexkit_test_support::dev_command(ckdev_subc_mcp()));
            command
                .arg("shim")
                .arg("--module-connection-file")
                .arg(&connection)
                .env("CLAUDE_PROJECT_DIR", project.path.as_ref())
                .env_remove("CK_INSTANCE_TOKEN")
                .env("XDG_CONFIG_HOME", &home)
                .env("XDG_DATA_HOME", &home)
                .env("XDG_RUNTIME_DIR", &home)
                .env_remove(subc_protocol::SUBC_MODULE_ID_ENV)
                .stdin(process::Stdio::piped())
                .stdout(process::Stdio::piped())
                .stderr(process::Stdio::null())
                .kill_on_drop(true);
            let mut child = command.spawn().unwrap();
            ShimProcess {
                stdin: child.stdin.take(),
                stdout: child.stdout.take(),
                child,
            }
        };
        let roots_asked = Arc::new(AtomicUsize::new(0));
        let handler = RootsClient {
            non_file: matches!(roots, RootsMode::NonFile),
            fail_roots: matches!(roots, RootsMode::Error),
            roots: match roots {
                RootsMode::Absent => None,
                RootsMode::Empty | RootsMode::NonFile | RootsMode::Error => Some(vec![]),
                RootsMode::Folder => Some(vec![root.join("nested")]),
            },
            roots_asked: Arc::clone(&roots_asked),
        };
        let client = if modern {
            shim.serve_mcp_client_2026(handler).await
        } else {
            shim.serve_mcp_client(handler).await
        };
        Self {
            server,
            module,
            shim,
            client,
            provider,
            entorhinal,
            project,
            root,
            proxy,
            roots_asked,
        }
    }

    async fn shutdown(mut self) {
        self.client.cancel().await.unwrap();
        let _ = timeout(READ_TIMEOUT, self.shim.child.wait()).await.unwrap();
        self.module.stop().await.unwrap();
    }
}

/// A folder as the daemon stores it and the gateway sends it: canonical, and
/// on Windows in the plain spelling. `std::fs::canonicalize` returns the
/// verbatim `\\?\` form there, which neither ever sends
/// (docs/path-canonicalization.md).
fn daemon_canonical(path: &Path) -> PathBuf {
    subc_daemon::ProjectRootId::from_path_allowing_missing(path)
        .unwrap()
        .as_path()
        .to_path_buf()
}

fn explicit() -> Value {
    json!({"result":{"projectId":"project-test", "via":"root"}})
}

#[tokio::test]
async fn scope_identity_roots_stamp_has_workdir_and_project() {
    let mut h = IdentityHarness::start(true, RootsMode::Folder, explicit(), false).await;
    let bind = h.provider.bound().await;
    assert_eq!(
        bind.pointer("/scope/workdir"),
        Some(&json!(daemon_canonical(&h.root)))
    );
    assert_eq!(
        bind.pointer("/scope/project_id"),
        Some(&json!("project-test"))
    );
    assert_eq!(
        bind.pointer("/identity/project_root"),
        Some(&json!(daemon_canonical(&h.root)))
    );
    assert_eq!(
        *h.entorhinal.requests.lock().await,
        vec![json!({"method":"resolve","params":{"canonicalRoot":daemon_canonical(&h.root)}})]
    );
    h.shutdown().await;
}

#[tokio::test]
async fn scope_identity_no_roots_uses_fallback() {
    let mut h = IdentityHarness::start(true, RootsMode::Absent, explicit(), false).await;
    let bind = h.provider.bound().await;
    assert_eq!(
        bind.pointer("/scope/workdir"),
        Some(&json!(daemon_canonical(h.project.path.as_ref())))
    );
    h.shutdown().await;
}

#[tokio::test]
async fn scope_identity_entorhinal_refusal_leaves_project_unset() {
    let mut h = IdentityHarness::start(
        true,
        RootsMode::Folder,
        json!({"code":"refused","message":"not allowed","detail":null}),
        false,
    )
    .await;
    let bind = h.provider.bound().await;
    assert_eq!(
        bind.pointer("/scope/workdir"),
        Some(&json!(daemon_canonical(&h.root)))
    );
    assert!(bind.pointer("/scope/project_id").is_none());
    h.shutdown().await;
}

#[tokio::test]
async fn scope_identity_legacy_provider_gets_host_folder_from_scope_without_stamp_workdir() {
    let mut h = IdentityHarness::start(false, RootsMode::Folder, explicit(), false).await;
    let bind = h.provider.bound().await;
    assert_eq!(
        bind["identity"]["project_root"],
        json!(daemon_canonical(&h.root))
    );
    assert_eq!(bind["identity"]["project_id"], "project-test");
    assert_eq!(
        bind["scope"]["owner"],
        json!({"kind": "reserved", "module_id": "subc-mcp"})
    );
    assert!(bind["scope"].get("workdir").is_none());
    assert!(bind["scope"].get("project_id").is_none());
    h.shutdown().await;
}

#[tokio::test]
async fn scope_identity_record_ends_with_session() {
    let mut h = IdentityHarness::start(true, RootsMode::Folder, explicit(), false).await;
    let bind = h.provider.bound().await;
    let scope = bind.get("scope").expect("standalone session scope");
    let mut stream = scope_observer(&h.server).await;
    let request = json!({"op":"scope.describe", "owner":scope["owner"], "ref":scope["ref"]});
    let live = describe_scope(&mut stream, 10, &request).await;
    assert_eq!(live["status"], json!(ScopeStatus::Live));
    h.client.cancel().await.unwrap();
    let _ = timeout(READ_TIMEOUT, h.shim.child.wait()).await.unwrap();
    timeout(READ_TIMEOUT, async {
        loop {
            let ended = describe_scope(&mut stream, 11, &request).await;
            if ended["status"] == json!(ScopeStatus::Ended) {
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("scope must end while its owner module is still running");
    h.module.stop().await.unwrap();
}

async fn describe_scope(stream: &mut TcpStream, corr: u64, body: &Value) -> Value {
    let frame = Frame::build(
        FrameType::Request,
        Flags::new(false, Priority::Passive, false),
        0,
        0,
        corr,
        serde_json::to_vec(body).unwrap(),
    )
    .unwrap();
    write_frame(stream, &frame).await.unwrap();
    let response = read_frame_timeout(stream).await;
    assert_eq!(
        response.header.ty,
        FrameType::Response,
        "{}",
        String::from_utf8_lossy(&response.body)
    );
    serde_json::from_slice(&response.body).unwrap()
}

async fn scope_observer(server: &TestServer) -> TcpStream {
    let mut stream = connect_control_client(&server.daemon.connection_file_path)
        .await
        .unwrap();
    let hello = ModuleHelloBody {
        manifest: ModuleManifest::builder("scope-observer", "0.0.0").build(),
        protocol_ver: PROTOCOL_VERSION,
        control_ops: None,
        launch_nonce: None,
    };
    let frame = Frame::build(
        FrameType::Hello,
        Flags::new(false, Priority::Passive, false),
        0,
        0,
        1,
        serde_json::to_vec(&hello).unwrap(),
    )
    .unwrap();
    write_frame(&mut stream, &frame).await.unwrap();
    assert_eq!(
        read_frame_timeout(&mut stream).await.header.ty,
        FrameType::HelloAck
    );
    stream
}

#[tokio::test]
async fn scope_identity_gateway_down_never_creates_local_record() {
    let mut h = IdentityHarness::start(true, RootsMode::Folder, explicit(), true).await;
    let bind = h.provider.bound().await;
    assert!(bind.get("scope").is_none());
    assert_eq!(bind["identity"]["project_root"], "/");
    assert!(h.entorhinal.requests.lock().await.is_empty());
    let request: ClientRequest = serde_json::from_value(json!({
        "method":"tools/call", "params":{"name":"scope-probe_probe", "arguments":{}, "_meta":{"claudecode/toolUseId":"tool-without-gateway"}}
    })).unwrap();
    let response = h.client.peer().send_request(request).await.unwrap();
    let response = serde_json::to_value(response).unwrap();
    assert_eq!(
        response["isError"], true,
        "gateway failure must refuse without fallback"
    );
    assert!(h.provider.requests.lock().await.is_empty());
    let mut observer = scope_observer(&h.server).await;
    let state = describe_scope(&mut observer, 2, &json!({"op":"scope.describe", "owner":{"kind":"reserved","module_id":"subc-mcp"},"ref":"any-ref"})).await;
    assert_eq!(
        state["owner_synced"], false,
        "no sync means no local scope.apply could create a record"
    );
    h.shutdown().await;
}

#[tokio::test]
async fn scope_identity_implicit_project_is_not_attribution() {
    let mut h = IdentityHarness::start(
        true,
        RootsMode::Folder,
        json!({"result":{"projectId":"pj-implicit1-derived", "via":"implicit"}}),
        false,
    )
    .await;
    let bind = h.provider.bound().await;
    assert_eq!(
        bind.pointer("/scope/workdir"),
        Some(&json!(daemon_canonical(&h.root)))
    );
    assert!(bind.pointer("/scope/project_id").is_none());
    h.shutdown().await;
}

#[tokio::test]
async fn scope_identity_empty_roots_uses_fallback() {
    let mut h = IdentityHarness::start(true, RootsMode::Empty, explicit(), false).await;
    let bind = h.provider.bound().await;
    assert_eq!(
        bind.pointer("/scope/workdir"),
        Some(&json!(daemon_canonical(h.project.path.as_ref())))
    );
    h.shutdown().await;
}

async fn assert_session_tools_work(h: &IdentityHarness) {
    let tools = timeout(READ_TIMEOUT, h.client.peer().list_tools(None))
        .await
        .unwrap()
        .expect("attribution must not fail tools/list");
    assert!(tools
        .tools
        .iter()
        .any(|tool| tool.name == "scope-probe_probe"));
    let result = h
        .client
        .peer()
        .call_tool(CallToolRequestParams::new("scope-probe_probe"))
        .await
        .expect("attribution must not fail tools/call");
    assert_ne!(result.is_error, Some(true));
    assert_eq!(h.provider.requests.lock().await.len(), 1);
}

#[tokio::test]
async fn scope_fallback_stale_launch_nonce_keeps_unscoped_tools() {
    let mut h = IdentityHarness::start_with_policy(
        true,
        RootsMode::Folder,
        explicit(),
        false,
        false,
        AttributionMode::StaleScopeNonce,
    )
    .await;
    assert_session_tools_work(&h).await;
    let bind = h.provider.bound().await;
    assert!(bind.get("scope").is_none());
    assert_eq!(bind["identity"]["project_root"], "/");
    assert!(
        h.proxy.scope_requests.load(Ordering::SeqCst) > 0,
        "the daemon's scope authority refusal must execute"
    );
    assert!(
        h.proxy
            .errors
            .lock()
            .await
            .iter()
            .any(|error| error["code"] == "scope_sync_not_authority"),
        "must reach the daemon's current-launch authority check"
    );
    h.shutdown().await;
}

#[tokio::test]
async fn scope_fallback_unreserved_supervised_module_creates_record() {
    let mut h = IdentityHarness::start_with_policy(
        true,
        RootsMode::Folder,
        explicit(),
        false,
        false,
        AttributionMode::Normal,
    )
    .await;
    assert_session_tools_work(&h).await;
    let bind = h.provider.bound().await;
    assert_eq!(
        bind.pointer("/scope/workdir"),
        Some(&json!(daemon_canonical(&h.root)))
    );
    assert_eq!(
        bind.pointer("/scope/project_id"),
        Some(&json!("project-test"))
    );
    h.shutdown().await;
}

#[tokio::test]
async fn scope_fallback_old_daemon_gets_no_record_attempt() {
    let mut h = IdentityHarness::start_with_policy(
        true,
        RootsMode::Folder,
        explicit(),
        false,
        true,
        AttributionMode::NoWorkdir,
    )
    .await;
    assert_session_tools_work(&h).await;
    let bind = h.provider.bound().await;
    assert!(bind.get("scope").is_none());
    assert_eq!(bind["identity"]["project_root"], "/");
    assert!(
        h.proxy.describes.load(Ordering::SeqCst) > 0,
        "capability discovery must use server.describe"
    );
    assert_eq!(
        h.proxy.scope_requests.load(Ordering::SeqCst),
        0,
        "old daemon must receive neither sync nor apply"
    );
    assert!(h.entorhinal.requests.lock().await.is_empty());
    h.shutdown().await;
}

#[tokio::test]
async fn scope_fallback_refusal_keeps_tools_and_catalog_lifecycle() {
    let mut h = IdentityHarness::start_with_policy(
        true,
        RootsMode::Folder,
        explicit(),
        false,
        true,
        AttributionMode::RefuseScopeApply,
    )
    .await;
    assert_session_tools_work(&h).await;
    let bind = h.provider.bound().await;
    assert!(bind.get("scope").is_none());
    assert_eq!(bind["identity"]["project_root"], "/");
    assert!(
        h.proxy.scope_requests.load(Ordering::SeqCst) >= 2,
        "the scope refusal must actually execute"
    );
    let _late = IdentityProvider::start(&h.server, "late-probe", false, None).await;
    timeout(READ_TIMEOUT, async {
        loop {
            let tools = h
                .client
                .peer()
                .list_tools(None)
                .await
                .expect("catalog refresh after refusal");
            if tools
                .tools
                .iter()
                .any(|tool| tool.name == "late-probe_probe")
            {
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("scope failure must not stop the lifecycle or catalog poller");
    h.shutdown().await;
}

#[tokio::test]
async fn scope_fallback_non_file_root_uses_shim_root() {
    let mut h = IdentityHarness::start(true, RootsMode::NonFile, explicit(), false).await;
    assert_session_tools_work(&h).await;
    let bind = h.provider.bound().await;
    assert_eq!(
        bind.pointer("/scope/workdir"),
        Some(&json!(daemon_canonical(h.project.path.as_ref())))
    );
    assert_eq!(
        *h.entorhinal.requests.lock().await,
        vec![
            json!({"method":"resolve", "params":{"canonicalRoot":daemon_canonical(h.project.path.as_ref())}})
        ]
    );
    h.shutdown().await;
}

#[tokio::test]
async fn scope_fallback_roots_error_uses_shim_root() {
    let mut h = IdentityHarness::start(true, RootsMode::Error, explicit(), false).await;
    assert_session_tools_work(&h).await;
    let bind = h.provider.bound().await;
    assert_eq!(
        bind.pointer("/scope/workdir"),
        Some(&json!(daemon_canonical(h.project.path.as_ref())))
    );
    h.shutdown().await;
}

#[tokio::test]
async fn scope_identity_2026_host_starts_its_session_once_without_initialize() {
    // A 2026-07-28 host sends no `initialize`, so the session's start-up
    // (scope record, project resolve, provider routes, catalog lifecycle) has
    // to run on its first request, and only once across many.
    let mut h = IdentityHarness::start_full(
        true,
        RootsMode::Folder,
        explicit(),
        false,
        true,
        AttributionMode::Normal,
        true,
    )
    .await;
    let peer = h.client.peer().clone();
    let (first, second, prompts) = tokio::join!(
        peer.list_tools(None),
        peer.list_tools(None),
        peer.list_prompts(None)
    );
    for tools in [first.unwrap(), second.unwrap()] {
        assert!(tools
            .tools
            .iter()
            .any(|tool| tool.name == "scope-probe_probe"));
    }
    prompts.unwrap();
    for _ in 0..2 {
        let result = h
            .client
            .call_tool(CallToolRequestParams::new("scope-probe_probe"))
            .await
            .expect("tools/call on a 2026-07-28 session");
        assert_ne!(result.is_error, Some(true));
    }
    assert_eq!(h.provider.requests.lock().await.len(), 2);

    // roots/list is a server-to-client request such a host refuses, so the
    // scope's workdir is the shim's project root even though the host
    // declares roots.
    let bind = h.provider.bound().await;
    assert_eq!(
        bind.pointer("/scope/workdir"),
        Some(&json!(daemon_canonical(h.project.path.as_ref())))
    );
    assert_eq!(h.roots_asked.load(Ordering::SeqCst), 0);

    // One route bind, one project resolve, one scope registration
    // (scope.sync, then scope.apply), however many requests came in.
    assert!(
        timeout(QUIET_TIMEOUT, h.provider.binds.recv())
            .await
            .is_err(),
        "the provider route must be bound once"
    );
    assert_eq!(
        *h.entorhinal.requests.lock().await,
        vec![
            json!({"method":"resolve", "params":{"canonicalRoot":daemon_canonical(h.project.path.as_ref())}})
        ]
    );
    assert_eq!(h.proxy.scope_requests.load(Ordering::SeqCst), 2);

    // The catalog lifecycle started too: a provider that registers later
    // shows up in tools/list.
    let _late = IdentityProvider::start(&h.server, "late-probe", false, None).await;
    timeout(READ_TIMEOUT, async {
        loop {
            let tools = peer.list_tools(None).await.expect("tools/list");
            if tools
                .tools
                .iter()
                .any(|tool| tool.name == "late-probe_probe")
            {
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the first request must start the catalog lifecycle");
    assert_eq!(h.proxy.scope_requests.load(Ordering::SeqCst), 2);
    h.shutdown().await;
}
