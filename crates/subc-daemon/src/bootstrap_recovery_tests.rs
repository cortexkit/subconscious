//! Spawned fixtures exercise discovery, dispatch and exit publication together.

use super::*;
use crate::config_gate::{ApprovalFacts, ApprovalReply, PlanKind, PresenceDecision};
use crate::config_store::testing::TestSupport;
use cortexkit_test_support::ScratchDir;
use serde_json::{json, Value};
use std::process::{Child, Command, Stdio};
use subc_protocol::{Flags, FrameType, Priority};
use subc_transport::{authenticate_client, read_frame, write_frame};
use tokio::io::{AsyncRead, AsyncWrite};

const FIXTURE: &str = "bootstrap::recovery_tests::config_recovery_fixture";
static DAEMONS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_some() {
            return;
        }
        if let Some(pid) = rustix::process::Pid::from_raw(self.0.id() as i32) {
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
        }
        let until = std::time::Instant::now() + Duration::from_secs(5);
        while self.0.try_wait().ok().flatten().is_none() && std::time::Instant::now() < until {
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Rig {
    dir: ScratchDir,
    exe: PathBuf,
    sealed: bool,
}
impl Rig {
    fn new(sealed: bool) -> Self {
        let dir = ScratchDir::new("recovery");
        let exe = dir.join("ckdev-recovery");
        std::fs::copy(std::env::current_exe().unwrap(), &exe).unwrap();
        for name in [
            "home",
            "data",
            "runtime",
            "config",
            "store",
            "config/cortexkit",
        ] {
            std::fs::create_dir_all(dir.join(name)).unwrap();
        }
        std::fs::write(dir.join("subc.jsonc"), serde_json::to_vec(&json!({
            "version":1, "port":1, "privileged_operator_prompt":"enforce",
            "modules":{"held-fixture":{"program":"/bin/sleep", "args":["3600"], "protocol":"none"},
                "reserved-fixture":{"program":"/bin/sleep", "args":["3600"], "enabled":false,
                    "reserved":true, "reserved_prefixes":["recovery-fixture:"]}}
        })).unwrap()).unwrap();
        std::fs::write(dir.join("config/cortexkit/a.jsonc"), b"{}").unwrap();
        std::fs::write(dir.join("operator"), "approve\napprove\napprove\napprove\n").unwrap();
        Self { dir, exe, sealed }
    }
    fn command(&self, role: &str) -> Command {
        let mut cmd = Command::new(&self.exe);
        cmd.args(["--ignored", "--exact", FIXTURE, "--nocapture"])
            .env("RECOVERY_ROLE", role)
            .env("RECOVERY_ROOT", self.dir.path())
            .env("RECOVERY_SEALED", if self.sealed { "1" } else { "0" })
            .env("HOME", self.dir.join("home"))
            .env("XDG_DATA_HOME", self.dir.join("data"))
            .env("XDG_RUNTIME_DIR", self.dir.join("runtime"))
            .env("XDG_CONFIG_HOME", self.dir.join("config"))
            .env("SUBC_CONNECTION_FILE", self.dir.join("connection.json"))
            .env("SUBC_PORT", "0")
            .stdout(Stdio::from(
                std::fs::File::create(self.dir.join(format!("{role}.log"))).unwrap(),
            ))
            .stderr(Stdio::from(
                std::fs::File::create(self.dir.join(format!("{role}.err"))).unwrap(),
            ));
        cmd
    }
    fn seed(&self) {
        let result = self.command("seed").status().unwrap();
        assert!(
            result.success(),
            "{} {}",
            std::fs::read_to_string(self.dir.join("seed.log")).unwrap(),
            std::fs::read_to_string(self.dir.join("seed.err")).unwrap()
        );
    }
    fn store(&self) -> PathBuf {
        self.dir.join("store").join(crate::config_store::STORE_FILE)
    }
    fn reject_store(&self, missing: bool) {
        if missing {
            std::fs::remove_file(self.store()).unwrap();
        } else if self.sealed {
            let mut bytes = std::fs::read(self.store()).unwrap();
            bytes.push(b' ');
            std::fs::write(self.store(), bytes).unwrap();
        } else {
            let mut body: Value =
                serde_json::from_slice(&std::fs::read(self.store()).unwrap()).unwrap();
            body["format_version"] = json!(999);
            std::fs::write(self.store(), serde_json::to_vec(&body).unwrap()).unwrap();
        }
    }
    async fn start(&self) -> Process {
        let mut process = Process(self.command("daemon").spawn().unwrap());
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                assert!(
                    process.0.try_wait().unwrap().is_none(),
                    "daemon exited: {} {}",
                    std::fs::read_to_string(self.dir.join("daemon.log")).unwrap(),
                    std::fs::read_to_string(self.dir.join("daemon.err")).unwrap()
                );
                if subc_transport::connection_file::read(self.dir.join("connection.json"))
                    .is_ok_and(|info| info.pid == process.0.id())
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        process
    }
    async fn local(&self) -> tokio::net::UnixStream {
        local(self.dir.path()).await
    }
    async fn tcp(&self) -> tokio::net::TcpStream {
        tcp(self.dir.path()).await
    }
    fn retained(&self) -> Option<Vec<u8>> {
        std::fs::read(self.store()).ok()
    }
}

async fn local(root: &Path) -> tokio::net::UnixStream {
    let published =
        subc_transport::connection_file::read_with_local_endpoint(root.join("connection.json"))
            .unwrap();
    let Some(LocalEndpoint::Unix { path }) = published.local_endpoint else {
        panic!("missing local endpoint")
    };
    let mut stream = tokio::net::UnixStream::connect(path).await.unwrap();
    authenticate_client(&mut stream, &published.info, Duration::from_secs(3))
        .await
        .unwrap();
    stream
}

async fn tcp(root: &Path) -> tokio::net::TcpStream {
    let info = subc_transport::connection_file::read(root.join("connection.json")).unwrap();
    let endpoint = &info.endpoints[0];
    let mut stream = tokio::net::TcpStream::connect((endpoint.host.as_str(), endpoint.port))
        .await
        .unwrap();
    authenticate_client(&mut stream, &info, Duration::from_secs(3))
        .await
        .unwrap();
    stream
}

async fn send<S: AsyncWrite + Unpin>(stream: &mut S, ty: FrameType, corr: u64, body: Value) {
    let frame = subc_protocol::Frame::build(
        ty,
        Flags::new(false, Priority::Interactive, false),
        0,
        0,
        corr,
        serde_json::to_vec(&body).unwrap(),
    )
    .unwrap();
    write_frame(stream, &frame).await.unwrap();
}

async fn receive<S: AsyncRead + Unpin>(stream: &mut S) -> subc_protocol::Frame {
    tokio::time::timeout(Duration::from_secs(10), read_frame(stream))
        .await
        .unwrap()
        .unwrap()
        .unwrap()
}

async fn rpc<S: AsyncRead + AsyncWrite + Unpin>(stream: &mut S, body: Value) -> Value {
    send(stream, FrameType::Request, 50, body).await;
    let frame = receive(stream).await;
    assert_eq!(frame.header.corr, 50);
    serde_json::from_slice(&frame.body).unwrap()
}

async fn apply<S: AsyncRead + AsyncWrite + Unpin>(stream: &mut S, op: &str) -> Value {
    let view = rpc(stream, json!({"op":op})).await;
    assert!(view.get("view").is_some(), "{view}");
    rpc(
        stream,
        json!({"op":"config.apply","digest":view["view"]["digest"]}),
    )
    .await
}

fn open_store(root: crate::config_store::StoreRoot) -> crate::config_store::ConfigStore {
    let sealed = std::env::var("RECOVERY_SEALED").unwrap() == "1";
    let key = sealed.then(|| root.path().join("key"));
    TestSupport::new(root.path().to_path_buf(), key)
        .unwrap()
        .with_key_denial(std::env::var_os("RECOVERY_DENIED").is_some())
        .open()
        .unwrap()
}

struct Admitted;
impl crate::config_gate::ReviewAdmission for Admitted {
    fn check(
        &self,
        _: &crate::registry::ModuleRegistration,
    ) -> std::result::Result<(), crate::config_gate::GateError> {
        Ok(())
    }
}

fn adapt_control(mut handler: ControlHandler) -> ControlHandler {
    handler = handler.with_config_os_prompt_policy_for_tests();
    handler = handler.with_recovery_exit_test_hook(
        |step| {
            let root = PathBuf::from(std::env::var_os("RECOVERY_ROOT").unwrap());
            if step == 1 && root.join("fail-exit-step").exists() {
                let attempts = std::fs::read_to_string(root.join("exit-step-completed"))
                    .ok()
                    .and_then(|value| value.parse::<usize>().ok())
                    .unwrap_or(0)
                    + 1;
                std::fs::write(root.join("exit-step-completed"), attempts.to_string()).unwrap();
                return Err(crate::config_gate::GateError::new(
                    "rescan_failed",
                    "injected failure after first successful reconcile step",
                ));
            }
            if step == 2 {
                std::fs::write(root.join("exit-definitions-reconciled"), b"1").unwrap();
            }
            if step == 0 && root.join("fail-before-exit-complete").exists() {
                std::fs::write(root.join("exit-completion-held"), b"1").unwrap();
                return Err(crate::config_gate::GateError::new(
                    "rescan_failed",
                    "injected failure before admission release",
                ));
            }
            Ok(())
        },
        PathBuf::from(std::env::var_os("RECOVERY_ROOT").unwrap())
            .join("quiet-exit-retry")
            .exists(),
    );
    let app = "test.desktop";
    handler = handler
        .with_test_operator_app_policy(crate::operator_apps::TestOperatorAppPolicy {
            macos_enabled: false,
            windows_enabled: false,
            linux_enabled: true,
            app,
            requirement: "always",
            floor: semver::Version::new(0, 0, 0),
            windows_publisher: None,
            windows_path: None,
            linux_path: Some(std::env::current_exe().unwrap()),
            windows_breakaway: false,
            linux_force_direct: true,
        })
        .with_config_review_admission(Arc::new(Admitted));
    handler.app_open_test_platform = Some(crate::operator_apps::AppOpenPlatform::Linux);
    handler.app_open_test_launch = Some(|| {
        let child = Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", FIXTURE, "--nocapture"])
            .env("RECOVERY_ROLE", "desktop")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| crate::app_open::AppOpenRefusal {
                code: "test_launch_failed",
                message: error.to_string(),
            })?;
        Ok(child.id())
    });
    handler
}

#[test]
#[ignore = "subprocess entry point; only run by the isolated recovery rigs"]
fn config_recovery_fixture() {
    let root = PathBuf::from(std::env::var_os("RECOVERY_ROOT").unwrap());
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        match std::env::var("RECOVERY_ROLE").unwrap().as_str() {
            "seed" => {
                let store = open_store(
                    crate::config_store::StoreRoot::from_service_definition(root.join("store"))
                        .unwrap(),
                );
                let owner = crate::config_gate::runtime::RuntimeOwner::new(
                    "seed".into(),
                    store,
                    crate::config_gate::capture::ConfigCapturer {
                        daemon_path: root.join("subc.jsonc"),
                        config_home: root.join("config"),
                        projects: Arc::new(crate::config_gate::capture::UnavailableProjectRegistry),
                    },
                );
                let prepared = owner.prepare(PlanKind::Stamp).unwrap();
                let plan = owner
                    .finish_plan(prepared, crate::config_gate::runtime::daemon_review)
                    .unwrap();
                let facts = ApprovalFacts {
                    connection: 7,
                    owner_uid: true,
                    desktop_app: true,
                    attested_operator_app: true,
                    os_prompt_available: true,
                    phone: None,
                    caller: crate::caller_identity::CallerIdentity::default(),
                };
                owner
                    .desktop_view_delivered(crate::ConnectionId::new(7), &plan)
                    .unwrap();
                let challenge = owner.begin_approval(&plan.digest, &facts).unwrap();
                owner
                    .finish_approval(
                        challenge.attempt(),
                        &facts,
                        ApprovalReply::Presence(PresenceDecision::InteractiveApproval),
                        1,
                    )
                    .unwrap();
            }
            "daemon" => {
                let mut config = BootstrapConfig::new(root.join("connection.json"), 0)
                    .with_daemon_config_source(root.join("subc.jsonc"), None)
                    .with_config_store_root(root.join("store"))
                    .unwrap()
                    .with_local_listener_socket_dir(root.join("socket"));
                config.defer_file_load = true;
                config.test_store_factory = Some(open_store);
                config.test_control = Some(adapt_control);
                config.operator_script = Some(root.join("operator"));
                config.operator_events = Some(root.join("operator-events"));
                run_with_config(config).await.unwrap();
            }
            "desktop" => {
                let mut stream = local(&root).await;
                let view = rpc(&mut stream, json!({"op":"config.stamp"})).await;
                if std::env::var_os("RECOVERY_STALE").is_some() {
                    std::fs::write(
                        root.join("subc.jsonc"),
                        b"{\"version\":1,\"modules\":{},\"port\":2}",
                    )
                    .unwrap();
                }
                let result = rpc(
                    &mut stream,
                    json!({"op":"config.apply","digest":view["view"]["digest"]}),
                )
                .await;
                std::fs::write(
                    root.join("desktop-result"),
                    serde_json::to_vec(&result).unwrap(),
                )
                .unwrap();
            }
            "module" => module_fixture(&root).await,
            role => panic!("unknown fixture role {role}"),
        }
    });
}

async fn module_fixture(root: &Path) {
    let mut stream = tcp(root).await;
    let hello = subc_protocol::ModuleHelloBody {
        manifest: subc_protocol::manifest::ModuleManifest::builder("a", "1.0.0").build(),
        protocol_ver: subc_protocol::PROTOCOL_VERSION,
        control_ops: Some(vec!["config.review".into()]),
        launch_nonce: None,
    };
    send(
        &mut stream,
        FrameType::Hello,
        1,
        serde_json::to_value(hello).unwrap(),
    )
    .await;
    assert_eq!(receive(&mut stream).await.header.ty, FrameType::HelloAck);
    let declaration = subc_protocol::config::ConfigDeclareRequest::new(
        json!({
            "$schema":"https://json-schema.org/draft/2020-12/schema", "type":"object",
            "unevaluatedProperties":false, "properties":{}, "x-ck-layers":["user"], "x-ck-apply":"live"
        }),
        root.join("config/cortexkit/a.jsonc").to_string_lossy(),
        vec![],
    );
    let declared = rpc(&mut stream, serde_json::to_value(declaration).unwrap()).await;
    assert_eq!(declared["op"], "config.declare", "{declared}");
    std::fs::write(root.join("module-ready"), b"ready").unwrap();
    loop {
        let frame = receive(&mut stream).await;
        let body: Value = serde_json::from_slice(&frame.body).unwrap();
        match (frame.header.ty, body["op"].as_str()) {
            (FrameType::Request, Some("config.review")) => {
                let reply = subc_protocol::config::ConfigReviewReply::new(vec![], vec![]);
                send(
                    &mut stream,
                    FrameType::Response,
                    frame.header.corr,
                    serde_json::to_value(reply).unwrap(),
                )
                .await;
            }
            (FrameType::Push, Some("config.changed")) => {
                let view = rpc(&mut stream, json!({"op":"config.get"})).await;
                let reply: subc_protocol::config::ConfigGetReply =
                    serde_json::from_value(view.clone()).unwrap();
                if let Some(pending) = reply.pending {
                    let ack = subc_protocol::config::ConfigAppliedRequest::new(
                        pending.generation,
                        pending.attempt.unwrap(),
                        subc_protocol::config::ConfigApplyResult::Ack,
                        vec![],
                    );
                    let result = rpc(&mut stream, serde_json::to_value(ack).unwrap()).await;
                    assert_eq!(result["op"], "config.applied", "{result}");
                }
                std::fs::write(root.join("module-view"), serde_json::to_vec(&view).unwrap())
                    .unwrap();
            }
            _ => panic!("unexpected module frame {body}"),
        }
    }
}

#[cfg(feature = "dev-desktop-approval")]
async fn wait_path(root: &Path, name: &str) {
    tokio::time::timeout(Duration::from_secs(15), async {
        while !root.join(name).exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "missing {name}; module={}, daemon={}",
            std::fs::read_to_string(root.join("module.log")).unwrap_or_default(),
            std::fs::read_to_string(root.join("daemon.log")).unwrap_or_default()
        )
    });
}

#[tokio::test]
async fn config_recovery_spawned_defaults_and_os_unstamp_write_a_fresh_store() {
    let _serial = DAEMONS.lock().await;
    for (sealed, missing) in [(true, false), (true, true), (false, false)] {
        let rig = Rig::new(sealed);
        rig.seed();
        rig.reject_store(missing);
        let before = rig.retained();
        let daemon = rig.start().await;
        let prompt_before = std::fs::read(rig.dir.join("operator-events")).ok();
        let info = subc_transport::connection_file::read(rig.dir.join("connection.json")).unwrap();
        assert!(info
            .endpoints
            .iter()
            .all(|endpoint| endpoint.port == DEFAULT_SUBC_PORT));
        let mut client = rig.local().await;
        let status = rpc(&mut client, json!({"op":"config.status"})).await;
        assert_eq!(
            status["status"]["state"],
            if missing {
                "store_missing"
            } else {
                "store_unverified"
            }
        );
        assert!(status.to_string().contains("phone"), "{status}");
        assert_eq!(rig.retained(), before);
        assert_eq!(
            rpc(&mut client, json!({"op":"supervisor.list"})).await["modules"],
            json!([])
        );
        for op in [
            "config.plan",
            "supervisor.set_enabled",
            "route.open",
            "catalog.list",
            "operator.confirm",
            "supervisor.rescan",
            "supervisor.rescan_checked",
            "supervisor.restart",
            "supervisor.reload",
            "supervisor.swap",
        ] {
            let refused = rpc(&mut client, json!({"op":op})).await;
            assert_eq!(refused["code"], "config_gated", "{op}: {refused}");
            assert_eq!(rig.retained(), before, "{op}");
        }
        assert_eq!(
            std::fs::read(rig.dir.join("operator-events")).ok(),
            prompt_before
        );
        let mut impostor = rig.tcp().await;
        send(
            &mut impostor,
            FrameType::Hello,
            77,
            json!({
                "manifest":{"module_id":"unreserved-impostor","module_version":"1.0.0"},
                "protocol_ver":subc_protocol::PROTOCOL_VERSION
            }),
        )
        .await;
        let refused: Value = serde_json::from_slice(&receive(&mut impostor).await.body).unwrap();
        assert_eq!(refused["code"], "config_gated");
        let daemon = if sealed && !missing {
            drop(daemon);
            let restarted = rig.start().await;
            let next =
                subc_transport::connection_file::read(rig.dir.join("connection.json")).unwrap();
            assert_ne!(
                next.key, info.key,
                "recovery must publish a fresh authentication key"
            );
            assert_eq!(rig.retained(), before);
            client = rig.local().await;
            restarted
        } else {
            daemon
        };
        let applied = apply(&mut client, "config.unstamp").await;
        assert_eq!(applied["generation"], 1, "{applied}");
        let status = rpc(&mut client, json!({"op":"config.status"})).await;
        assert_eq!(status["status"]["state"], "unstamped");
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let modules = rpc(&mut client, json!({"op":"supervisor.list"})).await;
                if modules["modules"][0]["live"] == true {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("unstamp must automatically release the enabled held actor");
        let body: Value = serde_json::from_slice(&rig.retained().unwrap()).unwrap();
        assert_eq!(body["revision"], 2);
        assert_eq!(body["body"]["generations"].as_object().unwrap().len(), 1);
        assert!(std::fs::read_to_string(rig.dir.join("operator-events"))
            .unwrap()
            .contains("Config files become authoritative again"));
        drop(daemon);
    }
}

#[cfg(feature = "dev-desktop-approval")]
#[tokio::test]
async fn config_recovery_spawned_unstamp_reconciles_changed_existing_actor_while_held() {
    let _serial = DAEMONS.lock().await;
    let rig = Rig::new(true);
    let daemon = rig.start().await;
    let mut client = rig.local().await;
    assert_eq!(apply(&mut client, "config.stamp").await["generation"], 1);
    let path = rig.dir.join("subc.jsonc");
    let mut doc: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    doc["modules"]["held-fixture"]["args"] = json!(["3599"]);
    std::fs::write(path, serde_json::to_vec(&doc).unwrap()).unwrap();
    rig.reject_store(false);
    assert_eq!(
        rpc(&mut client, json!({"op":"config.status"})).await["status"]["state"],
        "store_unverified"
    );
    let applied = apply(&mut client, "config.unstamp").await;
    assert_eq!(applied["generation"], 1, "{applied}");
    assert_eq!(
        rpc(&mut client, json!({"op":"config.status"})).await["status"]["state"],
        "unstamped"
    );
    drop(daemon);
}

#[cfg(feature = "dev-desktop-approval")]
#[tokio::test]
async fn config_recovery_spawned_partial_exit_failure_keeps_the_hold_and_retries() {
    let _serial = DAEMONS.lock().await;
    let rig = Rig::new(true);
    rig.seed();
    rig.reject_store(false);
    std::fs::write(rig.dir.join("fail-exit-step"), b"fail").unwrap();
    std::fs::write(rig.dir.join("quiet-exit-retry"), b"1").unwrap();
    let daemon = rig.start().await;
    let mut client = rig.local().await;
    let applied = apply(&mut client, "config.stamp").await;
    assert_eq!(applied["code"], "rescan_failed", "{applied}");
    assert!(
        rig.dir.join("exit-step-completed").exists(),
        "the injected error must follow a successful supervision step"
    );
    let modules = rpc(&mut client, json!({"op":"supervisor.list"})).await;
    assert_eq!(modules["modules"].as_array().unwrap().len(), 1, "{modules}");
    assert_eq!(
        rpc(&mut client, json!({"op":"config.plan"})).await["code"],
        "config_gated"
    );
    assert_eq!(
        modules["modules"][0]["live"], false,
        "a partially reconciled exit must not start modules"
    );
    let status = rpc(&mut client, json!({"op":"config.status"})).await;
    assert_eq!(status["status"]["state"], "store_unverified", "{status}");
    // File polling below cannot trigger configuration retries. Suppress state
    // notifications during the pending exit to exercise the daemon's timer alone.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let attempts = std::fs::read_to_string(rig.dir.join("exit-step-completed"))
                .ok()
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(0);
            if attempts >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the coordinator must observe the retained exit before notifications are suppressed");
    std::fs::remove_file(rig.dir.join("fail-exit-step")).unwrap();
    wait_path(&rig.dir, "exit-definitions-reconciled").await;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let status = rpc(&mut client, json!({"op":"config.status"})).await;
            if status["status"]["state"] == "stamped" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the retained exit marker must be retried without another approval");
    let modules = rpc(&mut client, json!({"op":"supervisor.list"})).await;
    assert_eq!(modules["modules"].as_array().unwrap().len(), 2, "{modules}");
    drop(daemon);
}

#[cfg(feature = "dev-desktop-approval")]
#[tokio::test]
async fn config_recovery_spawned_unstamp_stages_enable_until_exit_completion() {
    let _serial = DAEMONS.lock().await;
    for stamp in [false, true] {
        let rig = Rig::new(true);
        let path = rig.dir.join("subc.jsonc");
        let mut doc: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        doc["modules"]["held-fixture"]["enabled"] = json!(false);
        std::fs::write(&path, serde_json::to_vec(&doc).unwrap()).unwrap();
        rig.seed();
        let daemon = if stamp {
            rig.reject_store(false);
            rig.start().await
        } else {
            rig.start().await
        };
        let mut client = rig.local().await;
        let path = rig.dir.join("subc.jsonc");
        let mut doc: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        doc["modules"]["held-fixture"]["enabled"] = json!(true);
        std::fs::write(path, serde_json::to_vec(&doc).unwrap()).unwrap();
        if !stamp {
            rig.reject_store(false);
            assert_eq!(
                rpc(&mut client, json!({"op":"config.status"})).await["status"]["state"],
                "store_unverified"
            );
        }
        std::fs::write(rig.dir.join("fail-before-exit-complete"), b"fail").unwrap();
        let applied = apply(
            &mut client,
            if stamp {
                "config.stamp"
            } else {
                "config.unstamp"
            },
        )
        .await;
        assert_eq!(applied["code"], "rescan_failed", "{applied}");
        let staged = rpc(&mut client, json!({"op":"supervisor.list"})).await;
        let module = staged["modules"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["module_id"] == "held-fixture")
            .unwrap();
        assert_eq!(module["enabled"], true, "{staged}");
        assert_eq!(
            module["live"], false,
            "staging must not spawn before completion: {staged}"
        );
        assert!(rig.dir.join("exit-completion-held").exists());
        std::fs::remove_file(rig.dir.join("fail-before-exit-complete")).unwrap();
        let mut last_observation = Value::Null;
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let status = rpc(&mut client, json!({"op":"config.status"})).await;
                let modules = rpc(&mut client, json!({"op":"supervisor.list"})).await;
                let live =
                    modules["modules"].as_array().unwrap().iter().any(|entry| {
                        entry["module_id"] == "held-fixture" && entry["live"] == true
                    });
                last_observation = json!({"stamp":stamp, "status":status, "modules":modules});
                if live {
                    assert_eq!(
                        status["status"]["state"],
                        if stamp { "stamped" } else { "unstamped" }
                    );
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("coordinator must start staged enabled module after completing exit: {last_observation}"));
        drop(daemon);
    }
}

#[cfg(feature = "dev-desktop-approval")]
#[tokio::test]
async fn config_recovery_spawned_stamped_exit_installs_reserved_capabilities_before_hello() {
    let _serial = DAEMONS.lock().await;
    let rig = Rig::new(true);
    let path = rig.dir.join("subc.jsonc");
    let mut doc: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    doc["reserved_capabilities"] = json!({"recovery-test/v1":"reserved-fixture"});
    std::fs::write(path, serde_json::to_vec(&doc).unwrap()).unwrap();
    rig.seed();
    rig.reject_store(false);
    let daemon = rig.start().await;
    let mut client = rig.local().await;
    let applied = apply(&mut client, "config.stamp").await;
    assert_eq!(applied["generation"], 1, "{applied}");
    let mut claimant = rig.tcp().await;
    let mut manifest =
        subc_protocol::manifest::ModuleManifest::builder("unconfigured-claimant", "1.0.0").build();
    manifest.capabilities = Some(subc_protocol::manifest::CapabilityDeclarations {
        provides: vec!["recovery-test/v1".into()],
        requires: vec![],
        must_never_reach: vec![],
    });
    let hello = subc_protocol::ModuleHelloBody {
        manifest,
        protocol_ver: subc_protocol::PROTOCOL_VERSION,
        control_ops: None,
        launch_nonce: None,
    };
    send(
        &mut claimant,
        FrameType::Hello,
        79,
        serde_json::to_value(hello).unwrap(),
    )
    .await;
    let refused: Value = serde_json::from_slice(&receive(&mut claimant).await.body).unwrap();
    assert_eq!(refused["code"], "reserved_capability", "{refused}");
    drop(daemon);
}

#[cfg(feature = "dev-desktop-approval")]
#[tokio::test]
async fn config_recovery_spawned_restamp_via_app_open_binds_fresh_inputs() {
    let _serial = DAEMONS.lock().await;
    for stale in [false, true] {
        let rig = Rig::new(true);
        rig.seed();
        rig.reject_store(false);
        let before = rig.retained();
        let mut command = rig.command("daemon");
        if stale {
            command.env("RECOVERY_STALE", "1");
        }
        let mut daemon = Process(command.spawn().unwrap());
        tokio::time::timeout(Duration::from_secs(15), async {
            while subc_transport::connection_file::read(rig.dir.join("connection.json")).is_err() {
                assert!(daemon.0.try_wait().unwrap().is_none());
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let mut client = rig.local().await;
        let opened = rpc(&mut client, json!({"op":"app.open","app":"test.desktop"})).await;
        assert_eq!(opened["op"], "app.open", "{opened}");
        wait_path(rig.dir.path(), "desktop-result").await;
        let result: Value =
            serde_json::from_slice(&std::fs::read(rig.dir.join("desktop-result")).unwrap())
                .unwrap();
        let status = rpc(&mut client, json!({"op":"config.status"})).await;
        if stale {
            assert_eq!(result["code"], "plan_stale", "{result}");
            assert_eq!(status["status"]["state"], "store_unverified");
            assert_eq!(rig.retained(), before);
        } else {
            assert_eq!(result["generation"], 1, "{result}");
            assert_eq!(status["status"]["state"], "stamped");
            let body: Value = serde_json::from_slice(&rig.retained().unwrap()).unwrap();
            assert_eq!(body["revision"], 2);
            assert_eq!(body["body"]["generations"].as_object().unwrap().len(), 1);
        }
        drop(daemon);
    }
}

#[tokio::test]
async fn config_recovery_spawned_explicit_root_duplicate_start_retains_the_running_daemon() {
    let _serial = DAEMONS.lock().await;
    let rig = Rig::new(true);
    rig.seed();
    rig.reject_store(false);
    let before = rig.retained();
    let mut daemon = rig.start().await;
    let original = subc_transport::connection_file::read(rig.dir.join("connection.json")).unwrap();
    let mut duplicate = rig.command("daemon");
    duplicate
        .stdout(Stdio::from(
            std::fs::File::create(rig.dir.join("duplicate.log")).unwrap(),
        ))
        .stderr(Stdio::from(
            std::fs::File::create(rig.dir.join("duplicate.err")).unwrap(),
        ));
    let mut duplicate = Process(duplicate.spawn().unwrap());
    let result = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(status) = duplicate.0.try_wait().unwrap() {
                break status;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        result.success(),
        "{}",
        std::fs::read_to_string(rig.dir.join("duplicate.err")).unwrap()
    );
    assert!(daemon.0.try_wait().unwrap().is_none());
    let retained = subc_transport::connection_file::read(rig.dir.join("connection.json")).unwrap();
    assert_eq!(retained.pid, original.pid);
    assert_eq!(retained.key, original.key);
    assert_eq!(rig.retained(), before);
    drop(daemon);
}

#[tokio::test]
async fn config_recovery_spawned_invalid_files_and_denied_key_refuse_without_writes() {
    let _serial = DAEMONS.lock().await;
    for denied in [false, true] {
        let rig = Rig::new(true);
        rig.seed();
        rig.reject_store(false);
        let before = rig.retained();
        if !denied {
            std::fs::write(rig.dir.join("subc.jsonc"), b"invalid subc.jsonc").unwrap();
        }
        let mut command = rig.command("daemon");
        if denied {
            command.env("RECOVERY_DENIED", "1");
        }
        let mut daemon = Process(command.spawn().unwrap());
        tokio::time::timeout(Duration::from_secs(15), async {
            while subc_transport::connection_file::read(rig.dir.join("connection.json")).is_err() {
                assert!(daemon.0.try_wait().unwrap().is_none());
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let mut client = rig.local().await;
        for op in ["config.unstamp", "config.stamp"] {
            if denied {
                #[cfg(not(feature = "dev-desktop-approval"))]
                if op == "config.stamp" {
                    continue;
                }
                let result = apply(&mut client, op).await;
                assert_eq!(result["code"], "store_unverified", "{result}");
            } else {
                let result = rpc(&mut client, json!({"op":op})).await;
                assert_eq!(result["code"], "invalid_daemon_config", "{result}");
            }
            assert_eq!(rig.retained(), before);
        }
        let status = rpc(&mut client, json!({"op":"config.status"})).await;
        assert_eq!(status["status"]["state"], "store_unverified");
        if denied {
            assert!(
                status.to_string().to_lowercase().contains("key"),
                "{status}"
            );
        }
        assert_eq!(rig.retained(), before);
        drop(daemon);
    }
}

#[cfg(feature = "dev-desktop-approval")]
#[tokio::test]
async fn config_recovery_spawned_unstamp_hints_changed_opted_in_module_without_restart() {
    let _serial = DAEMONS.lock().await;
    let rig = Rig::new(false);
    let daemon = rig.start().await;
    let mut module = Process(rig.command("module").spawn().unwrap());
    wait_path(rig.dir.path(), "module-ready").await;
    let mut client = rig.local().await;
    let stamped = apply(&mut client, "config.stamp").await;
    assert_eq!(stamped["generation"], 1, "{stamped}");
    wait_path(rig.dir.path(), "module-view").await;
    let raw = b"{ /* changed file view */ }";
    std::fs::write(rig.dir.join("config/cortexkit/a.jsonc"), raw).unwrap();
    let status = rpc(&mut client, json!({"op":"config.status"})).await;
    assert_eq!(status["status"]["pending_proposals"], 1, "{status}");
    let result = apply(&mut client, "config.unstamp").await;
    assert_eq!(result["generation"], 2, "{result}");
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let view: subc_protocol::config::ConfigGetReply =
                serde_json::from_slice(&std::fs::read(rig.dir.join("module-view")).unwrap())
                    .unwrap();
            if view
                .active
                .is_some_and(|active| active.layers[0].bytes.as_deref() == Some(raw.as_slice()))
            {
                assert!(view.pending.is_none());
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let status = rpc(&mut client, json!({"op":"config.status"})).await;
    assert_eq!(status["status"]["state"], "unstamped");
    assert_eq!(status["status"]["pending_proposals"], 0);
    assert!(
        module.0.try_wait().unwrap().is_none(),
        "the opted-in connection must survive unstamp"
    );
    drop(module);
    drop(daemon);
}
