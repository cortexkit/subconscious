use super::*;
use std::{
    io::{Read, Seek, SeekFrom, Write},
    os::unix::fs::MetadataExt,
};
use subc_client_rs::{config::*, ModuleStopHandle, ServeOptions};
use subc_transport::{authenticate_client_with_role, connection_file};
use tokio::{
    io::AsyncReadExt,
    net::{TcpListener, UnixStream},
    sync::mpsc,
};

const COUNTER: &str = "project-counter";
const PROBE: &str = "project-fallback-probe";
const PROJECT: &str = "layered";
const FALLBACK: &str = "layerless";
const REGISTRY_TEST: &str = "project_config::registry_child";
const COUNTER_TEST: &str = "project_config::counter_child";

fn start_project_daemon(root: &Path) -> LiveDaemon {
    let source = ensure_binary(
        &workspace_root(),
        &[
            "build",
            "--locked",
            "-p",
            "subc-core",
            "--bin",
            "ck-subc",
            "--features",
            "subc-core/test-support,subc-daemon/test-support,subc-daemon/dev-desktop-approval",
            "--message-format=json",
        ],
    );
    let runtime_dir = root.join("runtime");
    let config_dir = root.join("config");
    for dir in [
        &runtime_dir,
        &root.join("home"),
        &root.join("data"),
        &root.join("config/cortexkit"),
        &root.join("layered/.cortexkit"),
        &root.join("layerless"),
    ] {
        fs::create_dir_all(dir).unwrap();
    }
    fs::write(root.join("operator"), "approve\n".repeat(8)).unwrap();
    for module in [COUNTER, PROBE] {
        fs::write(user_file(root, module), b"{\"count\":7}").unwrap();
    }
    let supervised = |test| {
        json!({
            "program": std::env::current_exe().unwrap(),
            "args": ["--ignored", "--exact", test, "--nocapture", "--"],
            "env": {"SDK_PROJECT_TEST_ROOT": root},
            "enabled": true, "registration_channel": true
        })
    };
    fs::write(
        config_dir.join("cortexkit/subc.jsonc"),
        json!({
            "version": 1, "port": 0, "modules": {
                "entorhinal": supervised(REGISTRY_TEST), (COUNTER): supervised(COUNTER_TEST)
            }
        })
        .to_string(),
    )
    .unwrap();

    let binary = root.join("ckdev-subc");
    fs::copy(source, &binary).unwrap();
    #[cfg(target_os = "macos")]
    assert!(Command::new("/usr/bin/codesign")
        .args(["--force", "--sign", "-"])
        .arg(&binary)
        .status()
        .unwrap()
        .success());

    let connection_file = runtime_dir.join("subc-connection.json");
    let mut command = Command::new(&binary);
    command
        .args(["--config-store-root"])
        .arg(root.join("store"))
        .env("SUBC_CONNECTION_FILE", &connection_file)
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_RUNTIME_DIR", &runtime_dir)
        .env("XDG_CONFIG_HOME", &config_dir)
        .env("HOME", root.join("home"))
        .env("SUBC_PORT", "0")
        .env("SUBC_CGROUP_PLACEMENT", "disabled")
        .env("SUBC_TEST_CONFIG_GATE", "1")
        .env("SUBC_TEST_OPERATOR_SCRIPT", root.join("operator"))
        .env("SUBC_TEST_OPERATOR_EVENTS", root.join("operator-events"))
        .env_remove("DBUS_SESSION_BUS_ADDRESS")
        .stdin(Stdio::null())
        .stderr(Stdio::inherit());
    let child = clean_module_env(&mut command).spawn().unwrap();
    LiveDaemon {
        child,
        runtime_dir,
        config_dir,
        connection_file,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn config_sdk_real_daemon_project_delivery_restart_and_fallback() {
    let temp = create_short_socket_root();
    let root = temp.0.canonicalize().unwrap();
    let daemon = start_project_daemon(&root);
    let _log_tails = ProjectLogsOnPanic(&root);
    wait_for_connection_file(&daemon.connection_file, START_TIMEOUT).await;
    wait_for_catalog_module(&daemon.connection_file, "entorhinal", START_TIMEOUT).await;

    let (proxy, wire) = wire_tap(&root, &daemon.connection_file).await;
    let mut probe = start_fallback_probe(&root, &proxy);
    probe.wait_success().await;
    let requests = timeout(READ_TIMEOUT, wire).await.unwrap().unwrap().unwrap();
    assert!(
        requests
            .iter()
            .filter(|body| body["op"] == "config.get" && body["project_id"] == FALLBACK)
            .count()
            >= 2
    );
    assert!(!requests.iter().any(|body| body["op"] == "config.applied"));

    let mut operator = connect_operator(&daemon.connection_file).await;
    let events = root.join("counter-events.jsonl");
    let legacy = wait_for_event(&events, READ_TIMEOUT, |e| {
        e["project_id"].is_null() && e["status"] == "unstamped"
    })
    .await;
    let status = control_rpc_on_stream(&mut operator, 7, json!({"op":"config.status"})).await;
    assert!(
        status["status"]["development_switches"]
            .as_array()
            .unwrap()
            .contains(&json!("SUBC_TEST_CONFIG_GATE")),
        "{status}"
    );
    let user_generation = approve(&mut operator, 1, "config.stamp", &root).await;
    wait_for_stamp_cutover(&mut operator).await;
    let user = wait_for_event(&events, READ_TIMEOUT, |e| {
        e["pid"] != legacy["pid"]
            && e["project_id"].is_null()
            && e["status"] == "active"
            && e["generation"] == user_generation
    })
    .await;
    let initial_pid = &user["pid"];
    fs::write(
        root.join("layered/.cortexkit/project-counter.jsonc"),
        b"{\"count\":13}",
    )
    .unwrap();
    let project_generation = approve(&mut operator, 3, "config.plan", &root).await;
    assert!(project_generation > user_generation);

    // Neither process requests PROJECT. The SDK can only fetch a named project
    // on these fresh connections after the daemon pushes config.changed for it.
    let pending = wait_for_event(&events, READ_TIMEOUT, |e| {
        e["pid"] == *initial_pid && e["project_id"] == PROJECT && e["status"] == "applying"
    })
    .await;
    assert_eq!(pending["generation"], project_generation);
    assert_eq!(pending["effective"], json!({"count": 13}));
    assert!(pending["attempt"].as_u64().is_some());
    assert_project_outcome(
        &root,
        project_generation,
        None,
        Some(project_generation),
        "pending",
    );
    fs::write(root.join("finish-project-apply"), b"ack").unwrap();
    let active = wait_for_event(&events, READ_TIMEOUT, |e| {
        e["pid"] == *initial_pid && e["project_id"] == PROJECT && e["status"] == "active"
    })
    .await;
    assert_eq!(active["attempt"], pending["attempt"]);
    assert_eq!(active["generation"], project_generation);
    assert_project_outcome(
        &root,
        project_generation,
        Some(project_generation),
        None,
        "active",
    );
    let show = control_rpc_on_stream(
        &mut operator,
        5,
        json!({
            "op":"config.show", "module_id":COUNTER, "project_id":PROJECT
        }),
    )
    .await;
    assert_eq!(show["project_id"], PROJECT);
    assert_eq!(show["effective"], json!({"count": 13}));

    control_rpc_on_stream(
        &mut operator,
        6,
        json!({
            "op":"supervisor.restart", "module_id":COUNTER
        }),
    )
    .await;
    let restarted = wait_for_event(&events, READ_TIMEOUT, |e| {
        e["pid"] != *initial_pid && e["project_id"] == PROJECT && e["status"] == "active"
    })
    .await;
    assert_eq!(restarted["generation"], project_generation);
    assert_eq!(restarted["effective"], json!({"count": 13}));
    assert_eq!(restarted["attempt"], Value::Null);
    let fallback = wait_for_event(&events, READ_TIMEOUT, |e| {
        e["pid"] == restarted["pid"] && e["project_id"] == FALLBACK && e["status"] == "active"
    })
    .await;
    assert_eq!(fallback["generation"], user_generation);
    assert_eq!(fallback["effective"], json!({"count": 7}));
    assert_eq!(fallback["attempt"], Value::Null);
}

fn user_file(root: &Path, module: &str) -> PathBuf {
    root.join(format!("config/cortexkit/{module}.jsonc"))
}

fn assert_project_outcome(
    root: &Path,
    generation: u64,
    active: Option<u64>,
    pending: Option<u64>,
    state: &str,
) {
    // Operator views omit per-project outcomes and attempt IDs. The isolated
    // approval store records the durable result under the module/project tuple.
    let store: Value =
        serde_json::from_slice(&fs::read(root.join("store/approved.json")).unwrap()).unwrap();
    let body = &store["body"];
    let tuple = body["versions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row[0]["module"] == COUNTER && row[0]["project"] == PROJECT)
        .expect("project tuple missing from approval store");
    assert_eq!(tuple[1]["active"], json!(active));
    assert_eq!(tuple[1]["pending"], json!(pending));
    let entry = &body["generations"][generation.to_string()];
    assert_eq!(entry["project_outcomes"][COUNTER][PROJECT]["state"], state);
}

fn clean_module_env(command: &mut Command) -> &mut Command {
    command
        .env_remove(subc_protocol::SUBC_MODULE_ID_ENV)
        .env_remove(subc_protocol::SUBC_LAUNCH_NONCE_ENV)
        .env_remove(subc_client_rs::launch_nonce::LAUNCH_NONCE_FD_ENV)
}

async fn connect_operator(path: &Path) -> UnixStream {
    let published = connection_file::read_with_local_endpoint(path).unwrap();
    let Some(subc_transport::LocalEndpoint::Unix { path }) = published.local_endpoint else {
        panic!("missing daemon local socket");
    };
    let mut stream = UnixStream::connect(path).await.unwrap();
    authenticate_client_with_role(&mut stream, &published.info, AUTH_DEADLINE, "desktop-app")
        .await
        .unwrap();
    stream
}

async fn wait_for_stamp_cutover(stream: &mut UnixStream) {
    // Approving the initial config.stamp makes the supervisor restart both
    // fixtures so they register over their approved channels. The old counter
    // can report Active before the replacements register.
    timeout(START_TIMEOUT, async {
        let mut corr = 1_000;
        loop {
            let provenance =
                control_rpc_on_stream(stream, corr, json!({"op":"supervisor.provenance"})).await;
            let live = |id| {
                provenance["modules"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|row| row["module_id"] == id && row["registration"] == "channel")
            };
            if live("entorhinal") && live(COUNTER) {
                return;
            }
            corr += 1;
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("registry and counter did not register through their post-stamp channels")
}

async fn approve(stream: &mut UnixStream, corr: u64, op: &str, root: &Path) -> u64 {
    let plan = control_rpc_on_stream(stream, corr, json!({"op":op})).await;
    assert!(plan["view"]["refusals"].is_null(), "{plan}");
    write_frame(
        stream,
        &control_request_frame(
            corr + 1,
            serde_json::to_vec(&json!({
                "op":"config.apply", "digest":plan["view"]["digest"]
            }))
            .unwrap(),
        ),
    )
    .await
    .unwrap();
    let frame = read_frame_timeout(stream).await;
    assert_eq!(frame.header.corr, corr + 1);
    let result: Value = serde_json::from_slice(&frame.body).unwrap();
    assert_eq!(
        frame.header.ty,
        FrameType::Response,
        "{op}: {result}; operator events: {:?}",
        fs::read_to_string(root.join("operator-events"))
    );
    result["generation"]
        .as_u64()
        .unwrap_or_else(|| panic!("{result}"))
}

struct ProjectLogsOnPanic<'a>(&'a Path);

impl Drop for ProjectLogsOnPanic<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("{}", project_log_tails(self.0));
        }
    }
}

fn project_log_tails(root: &Path) -> String {
    let logs = root.join("data/cortexkit/run/logs");
    let daemon_log = fs::read_dir(&logs)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("subc."))
        .map(|entry| entry.path())
        .max();
    let daemon_tail = daemon_log
        .as_deref()
        .map_or_else(|| format!("no subc log in {}", logs.display()), log_tail);
    let registry_tail = log_tail(&logs.join("entorhinal.stderr.log"));
    let counter_tail = log_tail(&logs.join(format!("{COUNTER}.stderr.log")));
    format!("daemon log tail:\n{daemon_tail}\nregistry fixture output tail:\n{registry_tail}\ncounter fixture output tail:\n{counter_tail}")
}

fn log_tail(path: &Path) -> String {
    const TAIL_BYTES: u64 = 16 * 1024;
    let read_tail = || -> io::Result<Vec<u8>> {
        let mut file = fs::File::open(path)?;
        let start = file.metadata()?.len().saturating_sub(TAIL_BYTES);
        file.seek(SeekFrom::Start(start))?;
        let mut bytes = Vec::new();
        file.take(TAIL_BYTES).read_to_end(&mut bytes)?;
        Ok(bytes)
    };
    match read_tail() {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(error) => format!("{}: {error}", path.display()),
    }
}

struct CounterProcess(Child);

impl CounterProcess {
    async fn wait_success(&mut self) {
        timeout(READ_TIMEOUT, async {
            loop {
                if let Some(status) = self.0.try_wait().unwrap() {
                    assert!(status.success(), "counter subprocess failed: {status}");
                    return;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("counter subprocess did not complete");
    }
}

impl Drop for CounterProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start_fallback_probe(root: &Path, connection: &Path) -> CounterProcess {
    let mut command = Command::new(std::env::current_exe().unwrap());
    CounterProcess(
        clean_module_env(&mut command)
            .args(["--ignored", "--exact", COUNTER_TEST, "--nocapture"])
            .env("SDK_PROJECT_TEST_ROOT", root)
            .env("SDK_PROJECT_TEST_CONNECTION", connection)
            .spawn()
            .unwrap(),
    )
}

#[tokio::test]
#[ignore = "launched by the daemon with the registry principal"]
async fn registry_child() {
    let root = PathBuf::from(std::env::var_os("SDK_PROJECT_TEST_ROOT").unwrap());
    let mut manifest = ModuleManifest::builder("entorhinal", "1.0.0").build();
    manifest.ready = Some(true);
    ServeOptions::default()
        .serve_with(
            &root.join("runtime/subc-connection.json"),
            manifest,
            ProjectRegistry(root.clone()),
        )
        .await
        .unwrap();
}

struct ProjectRegistry(PathBuf);

#[async_trait]
impl ModuleHandler for ProjectRegistry {
    async fn handle(&self, _: RequestCtx, _: Vec<u8>) -> HandlerOutcome {
        unreachable!("registry serves only channel-0 enumeration")
    }

    fn control_ops(&self) -> Vec<String> {
        vec!["projects.enumerate".into()]
    }

    async fn on_control_request(
        &self,
        op: &str,
        body: Value,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<Value, ErrorBody> {
        assert_eq!(op, "projects.enumerate");
        assert_eq!(body, json!({"op": op}));
        let projects = [PROJECT, FALLBACK].map(|id| {
            let root = self.0.join(id);
            let metadata = root.metadata().unwrap();
            json!({"projectId": id, "name": id, "roots": [&root], "implicit": false,
            "refKind": "local", "workspaceId": null, "rootRecords": [{
                "root": root, "remotes": [], "incarnation": {"kind": "token", "value": id},
                "registrationEpoch": null, "identity": "bound",
                "approval": {"state": "approved", "journalSeq": 1},
                "fileId": format!("unix:{}:{}", metadata.dev(), metadata.ino())
            }]})
        });
        Ok(
            json!({"op": op, "result": {"projects":projects, "workspaces":[],
            "generation":1,"incarnation":"sdk-project-fixture"}}),
        )
    }
}

#[tokio::test]
#[ignore = "launched by the daemon and by the fallback probe"]
async fn counter_child() {
    let root = PathBuf::from(std::env::var_os("SDK_PROJECT_TEST_ROOT").unwrap());
    let proxy = std::env::var_os("SDK_PROJECT_TEST_CONNECTION").map(PathBuf::from);
    let module = if proxy.is_some() { PROBE } else { COUNTER };
    let connection = proxy.unwrap_or_else(|| root.join("runtime/subc-connection.json"));
    let (states, mut rx) = mpsc::unbounded_channel();
    let stop = ModuleStopHandle::new();
    let (handle, serving) = ServeOptions::default()
        .with_stop_handle(stop.clone())
        .serve_with_handle(
            &connection,
            ModuleManifest::builder(module, "1.0.0").build(),
            Counter {
                module,
                root,
                states,
            },
        )
        .await
        .unwrap()
        .unwrap();
    assert!(handle.daemon_supports(CONFIG_GATE_CAPABILITY));
    let serving = tokio::spawn(serving);
    if module == PROBE {
        let bare = next_view(&mut rx, None, ConfigStatus::Unstamped).await;
        // Waiting for a second get proves the first refresh finished its report
        // path before shutdown; stopping from its callback could race reporting.
        for _ in 0..2 {
            assert!(handle.refresh_project_config(FALLBACK));
            let fallback = next_view(&mut rx, Some(FALLBACK), ConfigStatus::Unstamped).await;
            assert_eq!(fallback.effective, bare.effective);
            assert_eq!(fallback.effective, json!({"count": 7}));
            assert_eq!(fallback.generation, 0);
            assert_eq!(fallback.attempt, None);
        }
        stop.stop();
        timeout(READ_TIMEOUT, serving)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    } else {
        next_view(&mut rx, Some(PROJECT), ConfigStatus::Active).await;
        assert!(handle.refresh_project_config(FALLBACK));
        serving.await.unwrap().unwrap();
    }
}

struct Counter {
    module: &'static str,
    root: PathBuf,
    states: mpsc::UnboundedSender<Result<ConfigState, ConfigError>>,
}

impl Counter {
    fn record(&self, status: &str, view: &ConfigView) {
        let event = json!({"pid": std::process::id(), "status": status,
            "project_id": view.project_id, "generation": view.generation,
            "attempt": view.attempt, "effective": view.effective});
        fs::File::options()
            .append(true)
            .create(true)
            .open(self.root.join("counter-events.jsonl"))
            .unwrap()
            .write_all(format!("{event}\n").as_bytes())
            .unwrap();
    }
}

#[async_trait]
impl ModuleHandler for Counter {
    async fn handle(&self, _: RequestCtx, _: Vec<u8>) -> HandlerOutcome {
        unreachable!("counter is only exercised through configuration")
    }

    fn config_declaration(&self) -> Option<ConfigDeclareRequest> {
        Some(ConfigDeclareRequest::new(
            json!({
                "$schema":"https://json-schema.org/draft/2020-12/schema", "type":"object",
                "unevaluatedProperties":false, "properties":{"count":{"type":"integer", "description":"Counter value",
                    "default":0, "x-ck-apply":"live", "x-ck-layers":["user", "project"]}}
            }),
            user_file(&self.root, self.module).to_str().unwrap(),
            vec![],
        ))
    }

    async fn config_review(&self, request: ConfigReviewRequest) -> ConfigReviewReply {
        // The counter has one scalar setting; the highest populated layer wins.
        let (layer, value) = request
            .layers
            .iter()
            .rev()
            .find_map(|layer| {
                let value: Value = serde_json::from_slice(layer.bytes.as_ref()?).unwrap();
                Some((layer, value.get("count")?.clone()))
            })
            .unwrap();
        let old = request
            .base
            .as_ref()
            .map(|view| view.effective["count"].clone())
            .unwrap_or(Value::Null);
        let old_source = request
            .base
            .as_ref()
            .and_then(|view| view.sources.get("/count").copied())
            .unwrap_or(subc_config::Layer::Default);
        let new_source = layer.layer.known_layer().unwrap();
        if old == value && old_source == new_source {
            return ConfigReviewReply::new(vec![], vec![]);
        }
        let mut field = FieldChange::new(
            self.module,
            "/count",
            RecordedValue::new(old),
            RecordedValue::new(value),
            ApplyKind::Live,
        );
        field.old_source = old_source;
        field.new_source = new_source;
        field.project_id.clone_from(&layer.project_id);
        ConfigReviewReply::new(vec![field], vec![])
    }

    async fn config_apply(&self, view: ConfigView) -> Result<(), Vec<String>> {
        if view.project_id.as_deref() == Some(PROJECT) && view.status == ConfigStatus::Pending {
            self.record("applying", &view);
            wait_for_file(&self.root.join("finish-project-apply")).await;
        }
        Ok(())
    }

    async fn on_config_state(&self, state: ConfigState) {
        if self.module == COUNTER {
            match &state {
                ConfigState::Unstamped(view) | ConfigState::Active(view) => {
                    self.record(state.as_str(), view)
                }
                _ => {}
            }
        }
        let _ = self.states.send(Ok(state));
    }

    async fn on_config_error(&self, error: ConfigError) {
        let _ = self.states.send(Err(error));
    }
}

async fn next_view(
    rx: &mut mpsc::UnboundedReceiver<Result<ConfigState, ConfigError>>,
    project: Option<&str>,
    status: ConfigStatus,
) -> ConfigView {
    timeout(READ_TIMEOUT, async {
        loop {
            let state = rx.recv().await.expect("SDK state channel closed").unwrap();
            match state {
                ConfigState::Active(view)
                | ConfigState::Pending(view)
                | ConfigState::Unstamped(view) => {
                    if view.project_id.as_deref() == project && view.status == status {
                        return view;
                    }
                }
                other => panic!("unexpected SDK state: {other:?}"),
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no {status:?} view for {project:?}"))
}

async fn wire_tap(
    root: &Path,
    connection: &Path,
) -> (PathBuf, tokio::task::JoinHandle<io::Result<Vec<Value>>>) {
    let mut info = connection_file::read_for_client(connection).unwrap();
    let endpoint = info.endpoints.first().unwrap().clone();
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    info.endpoints.truncate(1);
    info.endpoints[0].host = "127.0.0.1".into();
    info.endpoints[0].port = listener.local_addr().unwrap().port();
    let path = root.join("tap-connection.json");
    connection_file::write_atomic(&path, &info).unwrap();
    let task = tokio::spawn(async move {
        let (mut module, _) = listener.accept().await?;
        let mut daemon = TcpStream::connect((endpoint.host.as_str(), endpoint.port)).await?;
        module.set_nodelay(true)?;
        daemon.set_nodelay(true)?;
        let (mr, mw) = module.split();
        let (dr, dw) = daemon.split();
        let mut requests = Vec::new();
        tokio::try_join!(
            relay(mr, dw, 2, Some(&mut requests)),
            relay(dr, mw, 1, None)
        )?;
        Ok(requests)
    });
    (path, task)
}

async fn relay(
    mut reader: impl AsyncRead + Unpin,
    mut writer: impl AsyncWrite + Unpin,
    auth_messages: usize,
    mut requests: Option<&mut Vec<Value>>,
) -> io::Result<()> {
    // Pass authentication through untouched: two client messages and one server proof.
    for _ in 0..auth_messages {
        let mut length = [0; 4];
        reader.read_exact(&mut length).await?;
        let size = u32::from_le_bytes(length);
        assert!(size <= subc_transport::MAX_AUTH_MESSAGE_LEN);
        let mut message = vec![0; size as usize];
        reader.read_exact(&mut message).await?;
        writer.write_all(&length).await?;
        writer.write_all(&message).await?;
    }
    while let Some(frame) = read_frame(&mut reader).await.map_err(io::Error::other)? {
        if frame.header.channel == 0 && frame.header.ty == FrameType::Request {
            if let Some(requests) = requests.as_deref_mut() {
                requests.push(serde_json::from_slice(&frame.body).map_err(io::Error::other)?);
            }
        }
        write_frame(&mut writer, &frame)
            .await
            .map_err(io::Error::other)?;
    }
    writer.shutdown().await
}
