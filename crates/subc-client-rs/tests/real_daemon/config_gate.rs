//! The spawned daemon covers backward-compatible module file reading.
//! Assigned-generation transitions use the protocol goldens in the unit tests.

use super::*;
use subc_client_rs::config::*;

#[test]
fn config_sdk_real_daemon_file_fallback() {
    if std::env::var_os("CONFIG_SDK_ROOT").is_some() {
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(isolated_entry());
        return;
    }
    let daemon = ensure_binary(
        &workspace_root(),
        &[
            "build",
            "--locked",
            "-p",
            "subc-core",
            "--bin",
            "ck-subc",
            "--message-format=json",
        ],
    );
    let temp = create_short_socket_root();
    let root = &temp.0;
    let connection = root.join("runtime/subc-connection.json");
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "config_gate::config_sdk_real_daemon_file_fallback",
            "--nocapture",
        ])
        .env_remove(subc_protocol::SUBC_MODULE_ID_ENV)
        .env_remove(subc_protocol::SUBC_LAUNCH_NONCE_ENV)
        .env_remove(subc_client_rs::launch_nonce::LAUNCH_NONCE_FD_ENV)
        .env("HOME", root.join("home"))
        .env("USERPROFILE", root.join("home"))
        .env("LOCALAPPDATA", root.join("data"))
        .env("XDG_RUNTIME_DIR", root.join("runtime"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("SUBC_CONNECTION_FILE", &connection)
        .env("CONFIG_SDK_ROOT", root)
        .env("CONFIG_SDK_DAEMON", daemon)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("1 passed"),
        "isolated test was not executed: {stdout}"
    );
}

struct Counter {
    file: PathBuf,
    files: tokio::sync::mpsc::UnboundedSender<()>,
    applied: Arc<Mutex<Option<Value>>>,
}

#[async_trait]
impl ModuleHandler for Counter {
    async fn handle(&self, _: RequestCtx, body: Vec<u8>) -> HandlerOutcome {
        HandlerOutcome::Response(body)
    }
    fn config_declaration(&self) -> Option<ConfigDeclareRequest> {
        Some(ConfigDeclareRequest::new(
            json!({
                "$schema":"https://json-schema.org/draft/2020-12/schema",
                "type":"object","properties":{"count":{"type":"integer",
                    "description":"Counter value","default":0,
                    "x-ck-apply":"live","x-ck-layers":["user"]}}
            }),
            self.file.to_str().unwrap(),
            vec![],
        ))
    }
    async fn config_review(&self, _: ConfigReviewRequest) -> ConfigReviewReply {
        ConfigReviewReply::new(vec![], vec![])
    }
    async fn config_apply(&self, _: ConfigView) -> Result<(), Vec<String>> {
        panic!("a daemon without config.gate must not supply a view");
    }
    async fn on_config_state(&self, state: ConfigState) {
        panic!("unexpected config state on a daemon without config.gate: {state:?}");
    }
    async fn config_files(&self) {
        let bytes = fs::read(&self.file).unwrap();
        *self.applied.lock().unwrap() = Some(serde_json::from_slice(&bytes).unwrap());
        self.files.send(()).unwrap();
    }
    async fn on_config_error(&self, error: ConfigError) {
        panic!("unexpected real daemon config error: {error}");
    }
}

async fn isolated_entry() {
    let root = PathBuf::from(std::env::var_os("CONFIG_SDK_ROOT").expect("isolated root"));
    let runtime = root.join("runtime");
    let config = root.join("config");
    fs::create_dir_all(&runtime).unwrap();
    fs::create_dir_all(root.join("home")).unwrap();
    write_empty_config(&config);
    let file = root.join("counter.json");
    let bytes = b"{\"count\":7}\r\n";
    fs::write(&file, bytes).unwrap();
    let source = PathBuf::from(std::env::var_os("CONFIG_SDK_DAEMON").unwrap());
    let daemon_bin = cortexkit_test_support::ckdev_binary(&source);
    let connection_file = runtime.join("subc-connection.json");
    let child = cortexkit_test_support::dev_command(&daemon_bin)
        .env("SUBC_CONNECTION_FILE", &connection_file)
        .env("SUBC_PORT", "0")
        .env("SUBC_CGROUP_PLACEMENT", "disabled")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut daemon = LiveDaemon {
        child,
        runtime_dir: runtime,
        config_dir: config,
        connection_file,
    };
    wait_for_connection_file(&daemon.connection_file, START_TIMEOUT).await;
    let (files, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let applied = Arc::new(Mutex::new(None));
    let handler = Counter {
        file,
        files,
        applied: applied.clone(),
    };
    let (handle, serving) = serve_with_handle(
        &daemon.connection_file,
        ModuleManifest::builder("counter", "1.0.0").build(),
        handler,
    )
    .await
    .unwrap();
    assert!(!handle.daemon_supports(CONFIG_GATE_CAPABILITY));
    let serving = tokio::spawn(serving);
    timeout(Duration::from_secs(8), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(*applied.lock().unwrap(), Some(json!({"count":7})));
    daemon.kill_and_wait();
    timeout(Duration::from_secs(8), serving)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}
