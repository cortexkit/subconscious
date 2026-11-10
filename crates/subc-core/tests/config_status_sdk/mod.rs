use serde_json::{json, Value};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    time::Duration,
};
use subc_protocol::{Frame, FrameType};
use subc_transport::{read_frame, write_frame};

mod rust;
mod wire;
pub use rust::rust_module;

pub struct Process(pub Child);
impl Drop for Process {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            if let Some(pid) = rustix::process::Pid::from_raw(self.0.id() as i32) {
                let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
            }
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while self.0.try_wait().ok().flatten().is_none() && std::time::Instant::now() < deadline
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

pub struct Rig {
    pub dir: cortexkit_test_support::ScratchDir,
    pub daemon: Option<Process>,
}
impl Drop for Rig {
    fn drop(&mut self) {
        if std::thread::panicking() {
            fn logs(path: &Path) {
                for entry in fs::read_dir(path).into_iter().flatten().flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        logs(&path);
                    } else if path.file_name().is_some_and(|name| {
                        name == "module-events"
                            || name.to_string_lossy().contains(".log")
                            || name.to_string_lossy().ends_with(".err")
                            || name.to_string_lossy().ends_with(".out")
                    }) {
                        let text = fs::read_to_string(&path).unwrap_or_default();
                        eprintln!(
                            "{}: {}",
                            path.display(),
                            &text[text.len().saturating_sub(16000)..]
                        );
                    }
                }
            }
            logs(self.dir.path());
        }
        self.daemon.take();
    }
}
impl Rig {
    pub fn new() -> Self {
        let dir = cortexkit_test_support::ScratchDir::new("config-sdk");
        for name in ["home", "runtime", "config/cortexkit", "data", "authority"] {
            fs::create_dir_all(dir.join(name)).unwrap();
        }
        fs::write(dir.join("operator"), "approve\n".repeat(64)).unwrap();
        fs::write(
            dir.join("config/cortexkit/counter.jsonc"),
            b"{\"count\":7}\r\n",
        )
        .unwrap();
        Self { dir, daemon: None }
    }
    pub fn command(&self, binary: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut cmd = Command::new(binary);
        cmd.env("HOME", self.dir.join("home"))
            .env("XDG_RUNTIME_DIR", self.dir.join("runtime"))
            .env("XDG_CONFIG_HOME", self.dir.join("config"))
            .env("XDG_DATA_HOME", self.dir.join("data"))
            .env(
                "SUBC_CONNECTION_FILE",
                self.dir.join("runtime/subc-connection.json"),
            )
            .env("SUBC_CGROUP_PLACEMENT", "disabled")
            .env_remove("SUBC_PORT")
            .env("SUBC_TEST_CONFIG_GATE", "1")
            .env("SUBC_TEST_OPERATOR_SCRIPT", self.dir.join("operator"))
            .env(
                "SUBC_TEST_OPERATOR_EVENTS",
                self.dir.join("operator-events"),
            )
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .env_remove(subc_protocol::SUBC_MODULE_ID_ENV)
            .env_remove(subc_protocol::SUBC_LAUNCH_NONCE_ENV)
            .env_remove(subc_client_rs::launch_nonce::LAUNCH_NONCE_FD_ENV);
        cmd
    }
    pub async fn start(&mut self, source: &Path, ckdev: bool, switch: bool) {
        let _ = fs::remove_file(self.dir.join("runtime/subc-connection.json"));
        let binary = self.dir.join(if ckdev {
            "ckdev-sdk-daemon"
        } else {
            "ordinary-daemon"
        });
        if !binary.exists() {
            fs::copy(source, &binary).unwrap();
        }
        let mut cmd = self.command(binary);
        cmd.arg("--config-store-root")
            .arg(self.dir.join("authority"));
        if !switch {
            cmd.env_remove("SUBC_TEST_CONFIG_GATE");
        }
        cmd.stdout(Stdio::from(
            fs::File::create(self.dir.join("daemon.out")).unwrap(),
        ))
        .stderr(Stdio::from(
            fs::File::create(self.dir.join("daemon.err")).unwrap(),
        ));
        self.daemon = Some(Process(cmd.spawn().unwrap()));
        wait_for(|| self.dir.join("runtime/subc-connection.json").exists()).await;
    }
    pub fn config(&self, language: &str) {
        let executable = std::env::current_exe().unwrap();
        let (program, args) = if language == "typescript" {
            (
                PathBuf::from(std::env::var_os("CONFIG_SDK_BUN").unwrap_or_else(|| "bun".into())),
                vec![Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/config_status_sdk/module.ts")
                    .to_string_lossy()
                    .into_owned()],
            )
        } else {
            (
                executable,
                vec![
                    "--exact".into(),
                    "config_status_sdk_module".into(),
                    "--nocapture".into(),
                    "--".into(),
                ],
            )
        };
        fs::write(self.dir.join("config/cortexkit/subc.jsonc"), serde_json::to_vec(&json!({
            "version":1, "port":0, "modules":{
                "counter":{"program":program,"args":args,"env":{"CONFIG_SDK_MODULE_ROOT":self.dir.path()},"enabled":true,"overlap":"safe","health":{"cadence_ms":100}},
                "legacy":{"program":"/bin/sh","args":["-c","sleep 600"],"protocol":"none","enabled":false}
            }
        })).unwrap()).unwrap();
    }
    pub fn module(&self, language: &str, id: &str) -> Process {
        fs::write(
            self.dir.join(format!("config/cortexkit/{id}.jsonc")),
            b"{\"count\":901}",
        )
        .unwrap();
        let mut cmd = if language == "typescript" {
            let mut cmd = self.command(std::env::var_os("CONFIG_SDK_BUN").unwrap());
            cmd.arg(
                Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/config_status_sdk/module.ts"),
            );
            cmd
        } else {
            let mut cmd = self.command(std::env::current_exe().unwrap());
            cmd.args(["--exact", "config_status_sdk_module", "--nocapture"]);
            cmd
        };
        cmd.env("CONFIG_SDK_MODULE_ROOT", self.dir.path())
            .env("CONFIG_SDK_MODULE_ID", id)
            .stdout(Stdio::from(
                fs::File::create(self.dir.join(format!("{id}.out"))).unwrap(),
            ))
            .stderr(Stdio::from(
                fs::File::create(self.dir.join(format!("{id}.err"))).unwrap(),
            ));
        Process(cmd.spawn().unwrap())
    }
    pub fn cli_status(&self) -> Value {
        let output = self
            .command(std::env::var_os("CONFIG_SDK_CK").expect("build ck first"))
            .args(["config", "status"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
    pub async fn client(&self) -> tokio::net::UnixStream {
        let published = subc_transport::connection_file::read_with_local_endpoint(
            self.dir.join("runtime/subc-connection.json"),
        )
        .unwrap();
        let Some(subc_transport::LocalEndpoint::Unix { path }) = published.local_endpoint else {
            panic!("missing local socket");
        };
        let mut stream = tokio::net::UnixStream::connect(path).await.unwrap();
        subc_transport::authenticate_client_with_role(
            &mut stream,
            &published.info,
            Duration::from_secs(3),
            "desktop-app",
        )
        .await
        .unwrap();
        stream
    }
    pub fn events(&self) -> Vec<Value> {
        fs::read_to_string(self.dir.join("module-events"))
            .unwrap_or_default()
            .split_inclusive('\n')
            .filter(|line| line.ends_with('\n'))
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
    pub async fn event(&self, state: &str, count: i64) -> Value {
        wait_for(|| {
            self.events()
                .iter()
                .any(|v| v["state"] == state && v["count"] == count)
        })
        .await;
        self.events()
            .into_iter()
            .rev()
            .find(|v| v["state"] == state && v["count"] == count)
            .unwrap()
    }
}

pub async fn wait_for(mut ready: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(20), async {
        while !ready() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("isolated daemon condition did not become ready");
}

pub async fn send<S: tokio::io::AsyncWrite + Unpin>(
    stream: &mut S,
    ty: FrameType,
    corr: u64,
    body: Value,
) {
    let frame = Frame::build(
        ty,
        subc_protocol::Flags::new(false, subc_protocol::Priority::Interactive, false),
        0,
        0,
        corr,
        serde_json::to_vec(&body).unwrap(),
    )
    .unwrap();
    write_frame(stream, &frame).await.unwrap();
}
pub async fn receive<S: tokio::io::AsyncRead + Unpin>(stream: &mut S) -> Frame {
    tokio::time::timeout(Duration::from_secs(10), read_frame(stream))
        .await
        .unwrap()
        .unwrap()
        .unwrap()
}
pub async fn rpc<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
    stream: &mut S,
    body: Value,
) -> Value {
    send(stream, FrameType::Request, 2, body).await;
    serde_json::from_slice(&receive(stream).await.body).unwrap()
}

pub fn isolated(test: &str, language: &str) {
    if std::env::var_os("CONFIG_SDK_ISOLATED").is_some() {
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(acceptance(language));
        return;
    }
    let rig = Rig::new();
    let result = rig
        .command(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture", "--include-ignored"])
        .env("CONFIG_SDK_ISOLATED", "1")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&result.stdout);
    assert!(
        result.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(stdout.contains("1 passed"), "{stdout}");
}

pub async fn seam_guards() {
    let dev = PathBuf::from(
        std::env::var_os("CONFIG_SDK_DEV_DAEMON").expect("build the development daemon first"),
    );
    let release = PathBuf::from(
        std::env::var_os("CONFIG_SDK_DEFAULT_DAEMON")
            .expect("build the default-feature daemon first"),
    );
    assert!(
        !fs::read(&release)
            .unwrap()
            .windows(b"SUBC_TEST_CONFIG_GATE".len())
            .any(|bytes| bytes == b"SUBC_TEST_CONFIG_GATE"),
        "default-feature daemon contains the test switch"
    );
    for (source, name, switch, sealed) in [
        (&dev, true, true, true),
        (&dev, false, true, false),
        (&dev, true, false, false),
        (&release, true, true, false),
    ] {
        let mut rig = Rig::new();
        fs::write(
            rig.dir.join("config/cortexkit/subc.jsonc"),
            b"{\"version\":1,\"port\":0,\"modules\":{}}",
        )
        .unwrap();
        rig.start(source, name, switch).await;
        let mut client = rig.client().await;
        let plan = rpc(&mut client, json!({"op":"config.stamp"})).await;
        let reply = rpc(
            &mut client,
            json!({"op":"config.apply","digest":plan["view"]["digest"]}),
        )
        .await;
        if sealed {
            assert!(reply.get("generation").is_some(), "{reply}");
        } else {
            assert_eq!(reply["code"], "approval_surface_unavailable", "{reply}");
        }
        let status = rpc(&mut client, json!({"op":"config.status"})).await;
        assert_eq!(
            status["status"]["integrity"],
            if sealed { "sealed" } else { "digest_only" },
            "{status}"
        );
        assert_eq!(rig.dir.join("authority/test-key").exists(), sealed);
        if !sealed {
            let warnings = status["status"]["warnings"].to_string();
            assert!(
                warnings.contains("stopped-daemon") && warnings.contains("data-home relocation"),
                "{status}"
            );
            assert!(
                status["status"]["missing_approval_surfaces"]
                    .to_string()
                    .contains("OS prompt"),
                "{status}"
            );
        }
    }
    let mut recovery = Rig::new();
    fs::write(
        recovery.dir.join("config/cortexkit/subc.jsonc"),
        b"{\"version\":1,\"port\":0,\"modules\":{}}",
    )
    .unwrap();
    recovery.start(&dev, true, true).await;
    let mut desktop = recovery.client().await;
    apply(&mut desktop, "config.stamp").await;
    drop(desktop);
    recovery.daemon.take();
    recovery.start(&release, false, true).await;
    let mut client = recovery.client().await;
    let status = rpc(&mut client, json!({"op":"config.status"})).await;
    assert_eq!(status["status"]["state"], "store_unverified", "{status}");
    let missing = status["status"]["missing_approval_surfaces"]
        .as_array()
        .unwrap();
    assert!(
        missing.iter().any(|surface| surface
            .as_str()
            .is_some_and(|name| name.contains("desktop"))),
        "{status}"
    );
    assert!(
        missing.contains(&json!("phone (unavailable in recovery)")),
        "{status}"
    );
}

pub async fn approved_status() {
    let mut rig = Rig::new();
    let source = PathBuf::from(std::env::var_os("CONFIG_SDK_DEV_DAEMON").unwrap());
    let path = rig.dir.join("config/cortexkit/subc.jsonc");
    let mut doc = json!({"version":1,"port":0,"modules":{"managed":{
        "program":"/bin/sleep","args":["600"],"protocol":"none","enabled":false
    }}});
    fs::write(&path, serde_json::to_vec(&doc).unwrap()).unwrap();
    rig.start(&source, true, true).await;
    let initial_pid =
        subc_transport::connection_file::read(rig.dir.join("runtime/subc-connection.json"))
            .unwrap()
            .pid;
    let mut client = rig.client().await;
    apply(&mut client, "config.stamp").await;
    let reservation = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = reservation.local_addr().unwrap().port();
    doc["port"] = json!(port);
    doc["modules"]["managed"]["enabled"] = json!(true);
    doc["modules"]["managed"]["restart"] = json!({"max_restarts":9});
    fs::write(&path, serde_json::to_vec(&doc).unwrap()).unwrap();
    let applied = apply(&mut client, "config.plan").await;
    assert_eq!(applied["restart_required"], true, "{applied}");
    let inactive = rig.cli_status();
    assert!(
        inactive["status"]["approved_but_inactive"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["entry"] == "port" && row["waiting_for"] == "daemon_restart"),
        "{inactive}"
    );
    let before = running_managed(&mut client).await;
    assert_eq!(before["configured_enabled"], true, "{before}");
    assert_eq!(before["max_restarts"], 9, "{before}");
    fs::write(&path, b"{\"version\":1,\"port\":1,\"modules\":{}}").unwrap();
    let retained = running_managed(&mut client).await;
    assert_eq!(retained["configured_enabled"], true);
    assert_eq!(retained["spawn_generation"], before["spawn_generation"]);
    drop(client);
    rig.daemon.take();
    drop(reservation);
    rig.start(&source, true, true).await;
    let info = subc_transport::connection_file::read(rig.dir.join("runtime/subc-connection.json"))
        .unwrap();
    assert_ne!(info.pid, initial_pid);
    assert!(info.endpoints.iter().all(|endpoint| endpoint.port == port));
    let mut client = rig.client().await;
    let restarted = running_managed(&mut client).await;
    assert_eq!(restarted["configured_enabled"], true);
    assert_eq!(restarted["max_restarts"], 9);
    let active = rig.cli_status();
    assert!(
        !active["status"]["approved_but_inactive"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row.get("entry").is_some()),
        "{active}"
    );
    assert_eq!(active["status"]["pending_proposals"], 1, "{active}");
}

async fn running_managed(client: &mut tokio::net::UnixStream) -> Value {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let list = rpc(client, json!({"op":"supervisor.list"})).await;
            let module = list["modules"]
                .as_array()
                .unwrap()
                .iter()
                .find(|module| module["module_id"] == "managed")
                .cloned();
            if let Some(module) =
                module.filter(|module| module["state"] == "running" && module["live"] == true)
            {
                return module;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap()
}

pub async fn published_sample(language: &str) {
    let mut rig = Rig::new();
    let rust = if language == "rust" {
        PathBuf::from(
            std::env::var_os("CONFIG_SDK_RUST_SAMPLE")
                .expect("build the extracted Rustdoc sample first"),
        )
    } else {
        PathBuf::new()
    };
    if language == "rust" {
        let budget = rig
            .command(&rust)
            .env_remove("CONFIG_SDK_MODULE_ROOT")
            .output()
            .unwrap();
        assert!(
            budget.status.success(),
            "{}",
            String::from_utf8_lossy(&budget.stderr)
        );
        let budget: Value = serde_json::from_slice(&budget.stdout).unwrap();
        println!(
            "Rust published sample adoption: {} lines",
            budget["adoption_lines"]
        );
        assert!(budget["adoption_lines"].as_u64().unwrap() <= 60, "{budget}");
    }
    let field = if language == "rust" { "count" } else { "limit" };
    let file = rig.dir.join("config/cortexkit/counter.jsonc");
    fs::write(&file, json!({(field):7}).to_string()).unwrap();
    let (program, args) = if language == "rust" {
        (rust, vec![])
    } else {
        (
            PathBuf::from(std::env::var_os("CONFIG_SDK_BUN").unwrap()),
            vec![Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../clients/subc-client/tests/fixtures/config-sample.ts")
                .to_string_lossy()
                .into_owned()],
        )
    };
    fs::write(rig.dir.join("config/cortexkit/subc.jsonc"), json!({"version":1,"port":0,"modules":{"counter":{
            "program":program,"args":args,"env":{"CONFIG_SDK_MODULE_ROOT":rig.dir.path(),"SAMPLE_FILE":file,"SAMPLE_EVENTS":rig.dir.join("module-events")}
        }}}).to_string()).unwrap();
    rig.start(
        &PathBuf::from(std::env::var_os("CONFIG_SDK_DEV_DAEMON").unwrap()),
        true,
        true,
    )
    .await;
    let _wire = wire::Proxy::start(rig.dir.path()).await;
    wait_for(|| {
        rig.events()
            .iter()
            .any(|event| sample_state(event) == Some("unstamped") && event["current"][field] == 7)
    })
    .await;
    let mut client = rig.client().await;
    let stamp = apply(&mut client, "config.stamp").await;
    wait_for(|| {
        rig.events().iter().any(|event| {
            sample_state(event) == Some("active") && sample_generation(event) == stamp["generation"]
        })
    })
    .await;
    fs::write(&file, json!({(field):11}).to_string()).unwrap();
    let next = apply(&mut client, "config.plan").await;
    wait_for(|| {
        rig.events().iter().any(|event| {
            sample_state(event) == Some("active")
                && sample_generation(event) == next["generation"]
                && event["current"][field] == 11
        })
    })
    .await;
    let unchanged = apply(&mut client, "config.stamp").await;
    wait_for(|| {
        rig.events().iter().any(|event| {
            sample_state(event) == Some("active")
                && sample_generation(event) == unchanged["generation"]
                && event["current"][field] == 11
        })
    })
    .await;
}

fn sample_state(event: &Value) -> Option<&str> {
    event["state"]
        .as_str()
        .or_else(|| event["state"]["state"].as_str())
}
fn sample_generation(event: &Value) -> Value {
    event
        .get("generation")
        .cloned()
        .unwrap_or_else(|| event["state"]["view"]["generation"].clone())
}

async fn apply(client: &mut tokio::net::UnixStream, operation: &str) -> Value {
    let plan = rpc(client, json!({"op":operation})).await;
    assert!(plan.get("view").is_some(), "{plan}");
    let result = rpc(
        client,
        json!({"op":"config.apply","digest":plan["view"]["digest"]}),
    )
    .await;
    assert!(result.get("generation").is_some(), "{result}");
    result
}

async fn acceptance(language: &str) {
    let mut rig = Rig::new();
    rig.config(language);
    let dev = PathBuf::from(std::env::var_os("CONFIG_SDK_DEV_DAEMON").unwrap());
    rig.start(&dev, true, true).await;
    let wire = wire::Proxy::start(rig.dir.path()).await;
    let initial = rig.event("unstamped", 7).await;
    assert_eq!(
        initial["view"]["layers"][0]["bytes"],
        "eyJjb3VudCI6N30NCg=="
    );
    let sent: Vec<_> = wire
        .frames()
        .into_iter()
        .filter(|frame| frame["direction"] == "module")
        .collect();
    assert_eq!(
        sent.iter()
            .filter(|frame| frame["body"]["op"] == "config.declare")
            .count(),
        1
    );
    assert!(
        sent.iter()
            .position(|frame| frame["body"]["op"] == "config.declare")
            .unwrap()
            < sent
                .iter()
                .position(|frame| frame["body"]["op"] == "config.get")
                .unwrap()
    );
    assert!(!sent
        .iter()
        .any(|frame| frame["body"]["op"] == "config.applied"));
    project_refusal(&rig).await;
    let mut client = rig.client().await;
    let status = rpc(&mut client, json!({"op":"config.status"})).await;
    assert_eq!(status["status"]["state"], "unstamped", "{status}");
    assert!(
        status["status"]["ungated_modules"]
            .as_array()
            .unwrap()
            .contains(&json!("legacy")),
        "{status}"
    );
    assert!(
        status["status"]["interpreter_only_covered"]
            .as_array()
            .unwrap()
            .contains(&json!("legacy")),
        "{status}"
    );
    assert!(
        status["status"]["development_switches"]
            .as_array()
            .unwrap()
            .contains(&json!("SUBC_TEST_CONFIG_GATE")),
        "{status}"
    );
    assert_eq!(
        rig.cli_status()["status"]["ungated_inputs"],
        status["status"]["ungated_inputs"]
    );
    assert!(
        status["status"]["development_switches"]
            .as_array()
            .unwrap()
            .contains(&json!("dev-desktop-approval")),
        "{status}"
    );
    assert!(
        status["status"]["ungated_inputs"]
            .as_array()
            .unwrap()
            .contains(&json!("SUBC_TEST_OPERATOR_SCRIPT")),
        "{status}"
    );
    let stamped = apply(&mut client, "config.stamp").await;
    assert_eq!(stamped["generation"], 1);
    rig.event("active", 7).await;
    assert!(wire
        .frames()
        .iter()
        .any(|v| v["body"]["op"] == "config.review"));
    let mut late = rig.module(language, "late");
    wait_for(|| {
        rig.events()
            .iter()
            .any(|v| v["module"] == "late" && v["state"] == "config_gated")
    })
    .await;
    let first = rig
        .events()
        .into_iter()
        .find(|v| v["module"] == "late" && v["state"] == "config_gated")
        .unwrap();
    assert_eq!(first["availability"], "no_approved_config", "{first}");
    assert!(first["view"].is_null(), "{first}");
    assert!(late.0.try_wait().unwrap().is_none());
    let gated = rig.cli_status();
    assert!(
        gated["status"]["gated_modules"]
            .as_array()
            .unwrap()
            .contains(&json!("late")),
        "{gated}"
    );
    drop(late);

    fs::write(
        rig.dir.join("hold-11"),
        b"hold apply after fetching pending",
    )
    .unwrap();
    fs::write(
        rig.dir.join("config/cortexkit/counter.jsonc"),
        b"{\"count\":11}",
    )
    .unwrap();
    let proposal = rpc(&mut client, json!({"op":"config.status"})).await;
    assert_eq!(proposal["status"]["pending_proposals"], 1, "{proposal}");
    let next = apply(&mut client, "config.plan").await;
    rig.event("applying", 11).await;
    fs::write(
        rig.dir.join("observe-health"),
        b"observe health while config apply is paused",
    )
    .unwrap();
    wait_for(|| rig.events().iter().any(|event| event["state"] == "health")).await;
    fs::remove_file(rig.dir.join("observe-health")).unwrap();
    fs::write(
        rig.dir.join("config/cortexkit/counter.jsonc"),
        b"{\"count\":13}",
    )
    .unwrap();
    let concurrent_review = rpc(&mut client, json!({"op":"config.plan"})).await;
    assert!(
        concurrent_review.get("view").is_some(),
        "{concurrent_review}"
    );
    rig.event("review", 13).await;
    fs::write(
        rig.dir.join("config/cortexkit/counter.jsonc"),
        b"{\"count\":11}",
    )
    .unwrap();
    let get = wire
        .frames()
        .into_iter()
        .rev()
        .find(|v| v["body"]["op"] == "config.get" && v["body"].get("pending").is_some())
        .unwrap();
    assert_eq!(get["body"]["active"]["generation"], stamped["generation"]);
    assert_eq!(get["body"]["pending"]["generation"], next["generation"]);
    assert_eq!(get["body"]["active"]["effective"]["count"], 7);
    assert_eq!(get["body"]["pending"]["effective"]["count"], 11);
    let pending = rpc(&mut client, json!({"op":"config.status"})).await;
    assert!(
        !pending["status"]["approved_but_inactive"]
            .as_array()
            .unwrap()
            .is_empty(),
        "{pending}"
    );
    assert_eq!(
        rig.cli_status()["status"]["approved_but_inactive"],
        pending["status"]["approved_but_inactive"]
    );
    fs::remove_file(rig.dir.join("hold-11")).unwrap();
    rig.event("active", 11).await;

    fs::write(
        rig.dir.join("config/cortexkit/counter.jsonc"),
        b"{\"count\":-1}",
    )
    .unwrap();
    let bad = apply(&mut client, "config.plan").await;
    let failed = rig.event("failed", 11).await;
    wait_for(|| {
        wire.frames().iter().any(|v| {
            v["body"]["op"] == "config.applied"
                && v["body"]["generation"] == bad["generation"]
                && v["body"]["result"] == "nack"
        })
    })
    .await;
    wait_for(|| {
        rig.events()
            .iter()
            .any(|v| v["state"] == "active" && v["count"] == 11 && v["pid"] != failed["pid"])
    })
    .await;

    fs::write(
        rig.dir.join("hold-12"),
        b"hold report before anchor failure",
    )
    .unwrap();
    fs::write(
        rig.dir.join("config/cortexkit/counter.jsonc"),
        b"{\"count\":12}",
    )
    .unwrap();
    let anchor_generation = apply(&mut client, "config.plan").await;
    rig.event("applying", 12).await;
    fs::write(rig.dir.join("authority/fail-anchor"), b"deny update").unwrap();
    fs::remove_file(rig.dir.join("hold-12")).unwrap();
    rig.event("anchor_pending", 12).await;
    let anchor = rpc(&mut client, json!({"op":"config.status"})).await;
    assert_eq!(anchor["status"]["anchor_pending"], true, "{anchor}");
    assert_eq!(rig.cli_status()["status"]["anchor_pending"], true);
    timer_retry_without_hint(language, &wire, &anchor_generation["generation"]).await;
    fs::remove_file(rig.dir.join("authority/fail-anchor")).unwrap();
    rig.event("active", 12).await;
    let reports: Vec<_> = wire
        .frames()
        .into_iter()
        .filter(|v| {
            v["direction"] == "module"
                && v["body"]["op"] == "config.applied"
                && v["body"]["generation"] == anchor_generation["generation"]
        })
        .collect();
    assert!(
        reports.len() >= 2,
        "anchor recovery must replay the original ACK"
    );
    assert!(reports
        .iter()
        .all(|report| report["body"] == reports[0]["body"]));

    fs::write(
        rig.dir.join("hold-21"),
        b"hold incumbent and candidate config apply",
    )
    .unwrap();
    fs::write(
        rig.dir.join("hold-health"),
        b"keep candidate from promoting before its first report",
    )
    .unwrap();
    fs::write(
        rig.dir.join("config/cortexkit/counter.jsonc"),
        b"{\"count\":21}",
    )
    .unwrap();
    apply(&mut client, "config.plan").await;
    rig.event("applying", 21).await;
    let mut swap_client = rig.client().await;
    let swap = tokio::spawn(async move {
        rpc(
            &mut swap_client,
            json!({"op":"supervisor.swap","module_id":"counter","ready_timeout_ms":20000}),
        )
        .await
    });
    wait_for(|| {
        swap.is_finished()
            || rig
                .events()
                .iter()
                .filter(|v| v["state"] == "applying" && v["count"] == 21)
                .count()
                >= 2
    })
    .await;
    assert!(
        !swap.is_finished(),
        "swap ended before candidate applied: {:?}",
        swap.await
    );
    fs::remove_file(rig.dir.join("hold-21")).unwrap();
    let candidate = rig.event("not_delivery_target", 21).await;
    assert_eq!(candidate["availability"], "last_view");
    fs::remove_file(rig.dir.join("hold-health")).unwrap();
    let swapped = swap.await.unwrap();
    assert!(swapped.get("code").is_none(), "{swapped}");
    wait_for(|| {
        rig.events()
            .iter()
            .any(|v| v["state"] == "active" && v["count"] == 21 && v["pid"] == candidate["pid"])
    })
    .await;

    fs::write(
        rig.dir.join("config/cortexkit/counter.jsonc"),
        b"{\"count\":19}",
    )
    .unwrap();
    apply(&mut client, "config.unstamp").await;
    rig.event("unstamped", 19).await;
    let unstamp = rpc(&mut client, json!({"op":"config.status"})).await;
    assert_eq!(unstamp["status"]["state"], "unstamped");
    assert_eq!(unstamp["status"]["pending_proposals"], 0);
    assert_eq!(rig.event("unstamped", 19).await["pid"], candidate["pid"]);
    apply(&mut client, "config.stamp").await;
    rig.event("active", 19).await;
    fs::write(
        rig.dir.join("hold-23"),
        b"hold applied report until recovery",
    )
    .unwrap();
    fs::write(
        rig.dir.join("config/cortexkit/counter.jsonc"),
        b"{\"count\":23}",
    )
    .unwrap();
    apply(&mut client, "config.plan").await;
    rig.event("applying", 23).await;
    fs::write(
        rig.dir.join("hold-declare-refused-declare"),
        b"hold declaration after HELLO until recovery",
    )
    .unwrap();
    let mut refused = rig.module(language, "refused-declare");
    wait_for(|| {
        wire.frames().iter().any(|v| {
            v["body"]["op"] == "config.declare"
                && v["body"]["user_file"]
                    .as_str()
                    .is_some_and(|path| path.ends_with("refused-declare.jsonc"))
        })
    })
    .await;
    let store = rig
        .dir
        .join("authority")
        .join(subc_daemon::config_store::STORE_FILE);
    let mut bytes = fs::read(&store).unwrap();
    bytes.push(b' ');
    fs::write(&store, bytes).unwrap();
    let recovery = rig.cli_status();
    let missing = recovery["status"]["missing_approval_surfaces"]
        .as_array()
        .unwrap();
    assert!(
        missing.contains(&json!("phone (unavailable in recovery)")),
        "{recovery}"
    );
    assert_eq!(
        recovery["status"]["state"], "store_unverified",
        "{recovery}"
    );
    fs::remove_file(rig.dir.join("hold-23")).unwrap();
    let retained = rig.event("config_gated", 23).await;
    assert_eq!(retained["availability"], "last_view");
    fs::remove_file(rig.dir.join("hold-declare-refused-declare")).unwrap();
    wait_for(|| {
        rig.events()
            .iter()
            .any(|v| v["module"] == "refused-declare" && v["state"] == "config_gated")
    })
    .await;
    assert!(refused.0.try_wait().unwrap().is_none());
    drop(refused);
    assert!(
        !rig.events().iter().any(|v| v["state"] == "error"),
        "{:?}",
        rig.events()
    );
    drop(wire);
    drop(rig);
    nack_anchor_acceptance(language, &dev).await;
}

async fn nack_anchor_acceptance(language: &str, dev: &Path) {
    let mut rig = Rig::new();
    rig.config(language);
    rig.start(dev, true, true).await;
    let wire = wire::Proxy::start(rig.dir.path()).await;
    let initial = rig.event("unstamped", 7).await;
    let mut client = rig.client().await;
    fs::write(
        rig.dir.join("config/cortexkit/counter.jsonc"),
        b"{\"count\":-2}",
    )
    .unwrap();
    fs::write(
        rig.dir.join("hold--2"),
        b"hold stamp NACK before anchor denial",
    )
    .unwrap();
    let stamp = apply(&mut client, "config.stamp").await;
    rig.event("applying", -2).await;
    fs::write(rig.dir.join("authority/fail-anchor"), b"deny NACK anchor").unwrap();
    fs::remove_file(rig.dir.join("hold--2")).unwrap();
    let pending = rig.event("anchor_pending", 7).await;
    assert_eq!(pending["availability"], "last_view");
    assert_eq!(pending["pid"], initial["pid"]);
    assert_eq!(rig.cli_status()["status"]["anchor_pending"], true);
    timer_retry_without_hint(language, &wire, &stamp["generation"]).await;
    fs::remove_file(rig.dir.join("authority/fail-anchor")).unwrap();
    let gated = rig.event("config_gated", 7).await;
    assert_eq!(gated["availability"], "last_view");
    assert_eq!(gated["refusal"]["code"], "config_gated");
    assert_eq!(gated["pid"], initial["pid"]);
    let reports: Vec<_> = wire
        .frames()
        .into_iter()
        .filter(|v| {
            v["direction"] == "module"
                && v["body"]["op"] == "config.applied"
                && v["body"]["generation"] == stamp["generation"]
        })
        .collect();
    assert!(
        reports.len() >= 2,
        "anchor recovery must replay the original NACK"
    );
    assert_eq!(reports[0]["body"]["result"], "nack");
    assert!(reports
        .iter()
        .all(|report| report["body"] == reports[0]["body"]));
    assert!(rig.cli_status()["status"]["gated_modules"]
        .as_array()
        .unwrap()
        .contains(&json!("counter")));
    assert!(
        !rig.events().iter().any(|v| v["state"] == "error"),
        "{:?}",
        rig.events()
    );
    fs::write(
        rig.dir.join("hold-25"),
        b"apply stays paused throughout drain and stop",
    )
    .unwrap();
    fs::write(
        rig.dir.join("config/cortexkit/counter.jsonc"),
        b"{\"count\":25}",
    )
    .unwrap();
    let stopped_generation = apply(&mut client, "config.plan").await;
    rig.event("applying", 25).await;
    let stopped = rpc(
        &mut client,
        json!({"op":"supervisor.set_enabled","module_id":"counter","enabled":false}),
    )
    .await;
    assert!(stopped.get("code").is_none(), "{stopped}");
    wait_for(|| {
        rig.events()
            .iter()
            .any(|event| event["state"] == "draining")
    })
    .await;
    wait_for(|| {
        rig.events()
            .iter()
            .any(|event| event["state"] == "connection_end")
    })
    .await;
    assert!(rig.dir.join("hold-25").exists());
    assert!(!wire
        .frames()
        .iter()
        .any(|frame| frame["direction"] == "module"
            && frame["body"]["op"] == "config.applied"
            && frame["body"]["generation"] == stopped_generation["generation"]));
}

async fn timer_retry_without_hint(language: &str, wire: &wire::Proxy, generation: &Value) {
    if language != "typescript" {
        return;
    }
    let reports = || {
        wire.frames()
            .iter()
            .filter(|frame| {
                frame["direction"] == "module"
                    && frame["body"]["op"] == "config.applied"
                    && &frame["body"]["generation"] == generation
            })
            .count()
    };
    let hints = || {
        wire.frames()
            .iter()
            .filter(|frame| {
                frame["direction"] == "daemon" && frame["body"]["op"] == "config.changed"
            })
            .count()
    };
    let previous_reports = reports();
    let previous_hints = hints();
    wait_for(|| reports() > previous_reports).await;
    assert_eq!(
        hints(),
        previous_hints,
        "the TypeScript timer must retry while the anchor is still denied, without a new hint"
    );
}

async fn project_refusal(rig: &Rig) {
    let info = subc_transport::connection_file::read(rig.dir.join("runtime/subc-connection.json"))
        .unwrap();
    let endpoint = &info.endpoints[0];
    let mut module = tokio::net::TcpStream::connect((endpoint.host.as_str(), endpoint.port))
        .await
        .unwrap();
    subc_transport::authenticate_client(&mut module, &info, Duration::from_secs(3))
        .await
        .unwrap();
    send(&mut module, FrameType::Hello, 1, json!({
        "manifest":subc_protocol::manifest::ModuleManifest::builder("project-probe", "1.0.0").build(),
        "protocol_ver":subc_protocol::PROTOCOL_VERSION,"control_ops":["config.review"]
    })).await;
    assert_eq!(receive(&mut module).await.header.ty, FrameType::HelloAck);
    let declared = rpc(&mut module, json!({"op":"config.declare","schema":{
        "$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","unevaluatedProperties":false
    },"user_file":rig.dir.join("config/cortexkit/project-probe.jsonc"),"project_dirs":[]})).await;
    assert_eq!(declared["op"], "config.declare", "{declared}");
    let refused = rpc(
        &mut module,
        json!({"op":"config.get","project_id":"unavailable-project"}),
    )
    .await;
    assert_eq!(refused["code"], "project_registry_unavailable", "{refused}");
}
