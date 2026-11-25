#![forbid(unsafe_code)]
#![cfg(any(target_os = "linux", target_os = "macos"))]

mod common;

use common::{connect_authed_client, copy_executable, kit::ShortRoot};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs, io,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use subc_protocol::{Flags, Frame, FrameType, Priority};
use subc_transport::{read_frame, write_frame};
use tokio::{
    process::{Child, Command},
    time::{sleep, timeout},
};

const WAIT: Duration = Duration::from_secs(15);
// Eight parallel cases on two Linux CPUs with software SHA took up to 19.2 s
// per case. Allow about twice that for waits that span debug-image hashing;
// don't use this budget for a handshake or an already-established route.
const UPGRADE_WAIT: Duration = Duration::from_secs(40);

struct Daemon {
    root: ShortRoot,
    executable: PathBuf,
    process: Child,
    connection: PathBuf,
}

impl Daemon {
    async fn boot(hook: Option<(&str, &str)>) -> Self {
        Self::boot_with_prompt(hook, "log").await
    }

    async fn boot_with_prompt(hook: Option<(&str, &str)>, prompt: &str) -> Self {
        Self::boot_with_module(hook, prompt, "approve", None).await
    }

    async fn boot_with_module(
        hook: Option<(&str, &str)>,
        prompt: &str,
        approval: &str,
        module: Option<&str>,
    ) -> Self {
        let modules: Vec<_> = module
            .into_iter()
            .map(|mode| ("upgrade-child", mode))
            .collect();
        Self::boot_fixture(hook, prompt, approval, &modules, false, false).await
    }

    async fn boot_fixture(
        hook: Option<(&str, &str)>,
        prompt: &str,
        approval: &str,
        modules: &[(&str, &str)],
        real_resume: bool,
        with_store: bool,
    ) -> Self {
        let root = ShortRoot::create_short_socket_root();
        let executable = root.join("ckdev-subc-upgrade-fixture");
        copy_executable(&common::ckdev_subc_under_test(), &executable);
        let connection = root
            .join("runtime")
            .join(subc_transport::CONNECTION_FILE_NAME);
        fs::create_dir(root.join("runtime")).unwrap();
        fs::create_dir_all(root.join("data/cortexkit/run")).unwrap();
        fs::set_permissions(
            root.join("data/cortexkit/run"),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        fs::create_dir_all(root.join("config/cortexkit")).unwrap();
        let modules: serde_json::Map<_, _> = modules.iter().map(|&(id, mode)| {
            (id.to_owned(), json!({
                "program": env!("CARGO_BIN_EXE_channel-module-rs"), "protocol":"subc",
                "enabled":true,"reserved":true,"registration_channel":true,
                "restart":{"max_restarts":0},
                "env":{"CHANNEL_MODULE_ROOT":root.path(),"CHANNEL_MODULE_UPGRADE_FIXTURE":mode},
            }))
        }).collect();
        fs::write(
            root.join("config/cortexkit/subc.jsonc"),
            serde_json::to_vec(
                &json!({"version":1,"privileged_operator_prompt":prompt,"modules":modules}),
            )
            .unwrap(),
        )
        .unwrap();
        let script = root.join("approval-script");
        fs::write(&script, format!("{approval}\n").repeat(8)).unwrap();
        let log = fs::File::create(root.join("daemon.log")).unwrap();
        let mut launch = daemon_command(root.path(), &executable, &connection);
        if with_store {
            with_isolated_store(&mut launch, &root);
        }
        let port = if real_resume {
            std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
                .unwrap()
                .local_addr()
                .unwrap()
                .port()
        } else {
            0
        };
        launch
            .env("SUBC_PORT", port.to_string())
            .env("SUBC_TEST_OPERATOR_SCRIPT", script)
            .env_remove("SUBC_TEST_UPGRADE_TEAM_ID")
            .stdout(log.try_clone().unwrap())
            .stderr(log);
        if !real_resume {
            launch.env("SUBC_TEST_UPGRADE_TARGET_MARKER", root.join("replacement"));
        }
        if let Some((key, value)) = hook {
            if matches!(
                key,
                "SUBC_TEST_UPGRADE_RECEIPT_GATE" | "SUBC_TEST_UPGRADE_FREEZE_GATE"
            ) {
                launch.env(key, root.join(value));
            } else if real_resume && key == "SUBC_TEST_UPGRADE_EXEC_PATH" {
                let alternate = root.join(value);
                copy_executable(&common::ckdev_subc(), &alternate);
                launch.env(key, alternate);
            } else {
                launch.env(key, value);
            }
        }
        let process = launch.spawn().unwrap();
        let mut daemon = Self {
            root,
            executable,
            process,
            connection,
        };
        timeout(UPGRADE_WAIT, async {
            loop {
                if let Some(exit) = daemon.process.try_wait().unwrap() {
                    panic!(
                        "daemon boot failed: {exit}; {}",
                        fs::read_to_string(daemon.root.join("daemon.log")).unwrap()
                    );
                }
                if let Ok((_, reply)) = daemon.call(json!({"op":"server.describe"})).await {
                    if reply["counters"]["daemon_identity"]["executable"].is_string() {
                        break;
                    }
                }
                sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "daemon identity not published: {}",
                fs::read_to_string(daemon.root.join("daemon.log")).unwrap()
            )
        });
        daemon
    }

    fn stage(&self, executable: &Path) -> String {
        let replacement = self.root.join("staged");
        copy_executable(executable, &replacement);
        let mut digest = Sha256::new();
        io::copy(&mut fs::File::open(&replacement).unwrap(), &mut digest).unwrap();
        fs::rename(&replacement, &self.executable).unwrap();
        format!("{:x}", digest.finalize())
    }

    fn stage_compatible(&self) -> String {
        self.stage(&common::ckdev_subc_under_test())
    }

    async fn module_pid(&self, previous: Option<u32>) -> u32 {
        self.named_module_pid("upgrade-child", previous).await
    }

    async fn named_module_pid(&self, id: &str, previous: Option<u32>) -> u32 {
        let marker = self.root.join(format!("{id}.registered"));
        timeout(WAIT, async {
            loop {
                if let Some(pid) = fs::read_to_string(&marker)
                    .ok()
                    .and_then(|pid| pid.parse().ok())
                {
                    if Some(pid) != previous {
                        return pid;
                    }
                }
                sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("module registered with a new pid")
    }

    fn socket(&self) -> PathBuf {
        self.root.join("data/cortexkit/run/subc.sock")
    }

    async fn assert_local_serving(&self) {
        let info = subc_transport::read_for_client(&self.connection).unwrap();
        let mut stream = tokio::net::UnixStream::connect(self.socket())
            .await
            .unwrap();
        subc_transport::authenticate_client(&mut stream, &info, WAIT)
            .await
            .unwrap();
    }

    async fn call(&self, body: Value) -> io::Result<(FrameType, Value)> {
        let wait = if body["op"] == "daemon.upgrade_in_place" {
            UPGRADE_WAIT
        } else {
            WAIT
        };
        let mut stream = self.issue(body).await?;
        let reply = timeout(wait, read_frame(&mut stream))
            .await
            .map_err(io::Error::other)?
            .map_err(io::Error::other)?
            .ok_or_else(|| io::Error::other("connection closed without reply"))?;
        assert_eq!(reply.header.corr, 501);
        Ok((reply.header.ty, serde_json::from_slice(&reply.body)?))
    }

    async fn issue(&self, body: Value) -> io::Result<tokio::net::TcpStream> {
        let mut stream = connect_authed_client(&self.connection).await?;
        let frame = Frame::build(
            FrameType::Request,
            Flags::new(false, Priority::Passive, false),
            0,
            0,
            501,
            serde_json::to_vec(&body)?,
        )
        .unwrap();
        write_frame(&mut stream, &frame)
            .await
            .map_err(io::Error::other)?;
        Ok(stream)
    }

    async fn request_upgrade(&self, checksum: String) -> (FrameType, Value) {
        self.call(json!({"op":"daemon.upgrade_in_place", "executable_sha256": checksum}))
            .await
            .unwrap()
    }

    async fn last_upgrade(&self) -> Value {
        timeout(UPGRADE_WAIT, async {
            loop {
                if let Ok((_, response)) = self.call(json!({"op":"server.describe"})).await {
                    let attempt = &response["counters"]["last_upgrade"];
                    if attempt.is_object() {
                        return attempt.clone();
                    }
                }
                sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("unwound daemon answers describe")
    }

    fn handover(&self) -> PathBuf {
        self.root.join("data/cortexkit/run/daemon-upgrade.handover")
    }
}

fn with_isolated_store(command: &mut Command, root: &ShortRoot) {
    command
        .arg("--config-store-root")
        .arg(root.join("authority"))
        .env("SUBC_TEST_CONFIG_GATE", "1");
}

fn daemon_command(root: &Path, executable: &Path, connection: &Path) -> Command {
    let mut command = Command::new(executable);
    command
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_RUNTIME_DIR", connection.parent().unwrap())
        .env("XDG_DATA_HOME", root.join("data"))
        .env("HOME", root)
        .env("TMPDIR", root)
        .env("SUBC_PORT", "0")
        .env("SUBC_CGROUP_PLACEMENT", "disabled")
        .env("SUBC_CONNECTION_FILE", connection)
        .env_remove("SUBC_TEST_UPGRADE_TARGET_MARKER")
        .kill_on_drop(true)
        .stdin(Stdio::null());
    command
}

#[tokio::test]
async fn cold_boot_publishes_discovery_while_identity_hash_is_held() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let root = ShortRoot::create_short_socket_root();
    let executable = root.join("ckdev-subc");
    copy_executable(&common::ckdev_subc_under_test(), &executable);
    fs::create_dir(root.join("runtime")).unwrap();
    let connection = root.join("runtime/subc-connection.json");
    let gate_path = root.join("bh.sock");
    let gate = tokio::net::UnixListener::bind(&gate_path).unwrap();
    let process = daemon_command(root.path(), &executable, &connection)
        .env("SUBC_TEST_UPGRADE_BOOT_HASH_GATE", &gate_path)
        .spawn()
        .unwrap();
    let daemon = Daemon {
        root,
        executable,
        process,
        connection,
    };
    let (mut held, _) = timeout(WAIT, gate.accept()).await.unwrap().unwrap();
    assert_eq!(timeout(WAIT, held.read_u8()).await.unwrap().unwrap(), 1);

    timeout(WAIT, async {
        while connect_authed_client(&daemon.connection).await.is_err() {
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("discovery must be published while hashing is paused");
    let (kind, describe) = timeout(
        Duration::from_secs(1),
        daemon.call(json!({"op": "server.describe"})),
    )
    .await
    .expect("describe must answer without waiting for the held boot hash")
    .unwrap();
    assert_eq!(kind, FrameType::Response);
    assert!(describe["counters"].get("daemon_identity").is_none());

    let old = daemon.root.join("old-image");
    fs::rename(&daemon.executable, &old).unwrap();
    fs::write(&daemon.executable, b"not the booted image").unwrap();
    held.write_u8(1).await.unwrap();
    let mut expected = Sha256::new();
    io::copy(&mut fs::File::open(old).unwrap(), &mut expected).unwrap();
    let expected = format!("{:x}", expected.finalize());
    timeout(WAIT, async {
        loop {
            let (_, describe) = daemon.call(json!({"op": "server.describe"})).await.unwrap();
            if let Some(id) = describe["counters"].get("daemon_identity") {
                assert_eq!(id["executable_sha256"], expected);
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("describe must report the hash of the original open image");
}

async fn fresh_daemon(daemon: &Daemon, old: [u8; 16]) -> Value {
    timeout(UPGRADE_WAIT, async {
        loop {
            let fresh = subc_transport::read_for_client(&daemon.connection)
                .is_ok_and(|info| info.daemon_id != old);
            if fresh {
                if let Ok((_, describe)) = daemon.call(json!({"op":"server.describe"})).await {
                    if describe["counters"]["daemon_identity"].is_object() {
                        return describe;
                    }
                }
            }
            sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("replacement did not start: {}", daemon_logs(&daemon.root)))
}

fn daemon_logs(root: &ShortRoot) -> String {
    let mut logs = fs::read_to_string(root.join("daemon.log")).unwrap_or_default();
    for entry in fs::read_dir(root.join("data/cortexkit/run/logs"))
        .into_iter()
        .flatten()
        .flatten()
    {
        logs.push_str(&fs::read_to_string(entry.path()).unwrap_or_default());
    }
    logs
}

#[cfg(target_os = "linux")]
fn assert_owned_files_in_scratch(daemon: &Daemon, module_image: Option<&Path>) {
    let pid = daemon.process.id().unwrap();
    let mut paths = Vec::new();
    for entry in fs::read_dir(format!("/proc/{pid}/fd")).unwrap().flatten() {
        if let Ok(target) = fs::read_link(entry.path()) {
            paths.push(target);
        }
    }
    assert_scratch_paths(daemon, &paths, module_image);
}

#[cfg(target_os = "macos")]
fn assert_owned_files_in_scratch(daemon: &Daemon, module_image: Option<&Path>) {
    let pid = daemon.process.id().unwrap().to_string();
    let output = std::process::Command::new("/usr/sbin/lsof")
        .args(["-nP", "-p", &pid, "-Fnf"])
        .output()
        .unwrap();
    assert!(output.status.success(), "lsof failed: {:?}", output.stderr);
    let mut numbered_fd = false;
    let mut paths = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if let Some(fd) = line.strip_prefix('f') {
            numbered_fd = fd.parse::<u32>().is_ok();
        } else if numbered_fd {
            if let Some(path) = line.strip_prefix('n') {
                paths.push(PathBuf::from(path));
            }
        }
    }
    assert_scratch_paths(daemon, &paths, module_image);
}

fn assert_scratch_paths(daemon: &Daemon, paths: &[PathBuf], module_image: Option<&Path>) {
    let root = fs::canonicalize(daemon.root.path()).unwrap();
    // The supervisor can still have the configured executable open to compare
    // it with the running image after the module announces registration.
    let module_image = module_image.map(|path| fs::canonicalize(path).unwrap());
    for target in paths {
        if target.is_absolute() && target != Path::new("/dev/null") {
            let canonical = fs::canonicalize(target).unwrap_or_else(|_| target.clone());
            assert!(
                canonical.starts_with(&root)
                    || target.starts_with(daemon.root.path())
                    || module_image.as_ref() == Some(&canonical),
                "daemon fd outside scratch root: {target:?}"
            );
        }
    }
    let publication = root.join("runtime/subc-connection.json.start-lock");
    assert_eq!(paths.iter().filter(|path| **path == publication).count(), 1);
}

#[tokio::test]
async fn exec_receipt_counts_blocked_refusal_and_ignores_input_after_freeze() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{UnixListener, UnixStream};

    async fn gate(listener: &UnixListener, expected: u8) -> UnixStream {
        let (mut stream, _) = timeout(UPGRADE_WAIT, listener.accept())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stream.read_u8().await.unwrap(), expected);
        stream
    }

    let mut daemon = Daemon::boot(Some(("SUBC_TEST_UPGRADE_FREEZE_GATE", "freeze-gate"))).await;
    let checkpoint = UnixListener::bind(daemon.root.join("freeze-gate")).unwrap();
    let mut first = connect_authed_client(&daemon.connection).await.unwrap();
    let mut second = connect_authed_client(&daemon.connection).await.unwrap();
    let mut late = connect_authed_client(&daemon.connection).await.unwrap();
    let checksum = daemon.stage_compatible();
    let mut issuer = daemon
        .issue(json!({"op":"daemon.upgrade_in_place", "executable_sha256": checksum}))
        .await
        .unwrap();

    let mut ready_to_freeze = gate(&checkpoint, 1).await;
    let flags = Flags::new(false, Priority::Passive, false);
    let route_open = |corr| {
        Frame::build(FrameType::Request, flags, 0, 0, corr, serde_json::to_vec(&json!({
        "op":"route.open",
        "target": subc_protocol::RouteTarget::ToolProvider { module_id: "no-module".into() },
        "identity": subc_protocol::BindIdentity::new("upgrade-fixture", "refusal"),
    })).unwrap()).unwrap()
    };
    write_frame(&mut first, &route_open(301)).await.unwrap();
    let mut first_writer = gate(&checkpoint, 3).await;
    write_frame(&mut second, &route_open(302)).await.unwrap();
    let _blocked_writer = gate(&checkpoint, 3).await;

    ready_to_freeze.write_u8(0).await.unwrap();
    let mut frozen = gate(&checkpoint, 2).await;
    let ping = Frame::build(FrameType::Ping, flags, 0, 0, 303, vec![]).unwrap();
    write_frame(&mut late, &ping).await.unwrap();
    first_writer.write_u8(0).await.unwrap();
    let flushed = timeout(WAIT, read_frame(&mut first))
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(flushed.header.corr, 301);
    assert_eq!(
        serde_json::from_slice::<Value>(&flushed.body).unwrap()["detail"]["reason"],
        "daemon_upgrade"
    );
    frozen.write_u8(0).await.unwrap();

    let receipt = timeout(WAIT, read_frame(&mut issuer))
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(receipt.header.ty, FrameType::Response);
    let receipt: Value = serde_json::from_slice(&receipt.body).unwrap();
    assert_eq!(receipt["refusals_tracked"], 2);
    assert_eq!(receipt["refusals_unconfirmed"], 1);
    assert_eq!(
        receipt["settled_in_time"], false,
        "a timed-out writer leaves accounting incomplete: {receipt}"
    );
    assert!(
        !matches!(
            timeout(WAIT, read_frame(&mut late)).await.unwrap(),
            Ok(Some(_))
        ),
        "input after the freeze must never be answered"
    );
    assert!(timeout(UPGRADE_WAIT, daemon.process.wait())
        .await
        .unwrap()
        .unwrap()
        .success());
}

#[tokio::test]
async fn idle_and_split_frame_readers_acknowledge_cutover_without_more_input() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixListener;

    let mut daemon = Daemon::boot(Some(("SUBC_TEST_UPGRADE_FREEZE_GATE", "freeze-gate"))).await;
    let checkpoint = UnixListener::bind(daemon.root.join("freeze-gate")).unwrap();
    let mut idle = connect_authed_client(&daemon.connection).await.unwrap();
    let mut split = connect_authed_client(&daemon.connection).await.unwrap();
    let ping = Frame::build(
        FrameType::Ping,
        Flags::new(false, Priority::Passive, false),
        0,
        0,
        400,
        vec![],
    )
    .unwrap();
    for connection in [&mut idle, &mut split] {
        write_frame(connection, &ping).await.unwrap();
        assert_eq!(
            timeout(WAIT, read_frame(connection))
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .header
                .ty,
            FrameType::Pong
        );
    }
    let partial = ping.header.encode();
    split.write_all(&partial[..5]).await.unwrap();
    let checksum = daemon.stage_compatible();
    let mut caller = daemon
        .issue(json!({"op":"daemon.upgrade_in_place","executable_sha256":checksum}))
        .await
        .unwrap();
    for expected in [1, 2] {
        let (mut gate, _) = timeout(UPGRADE_WAIT, checkpoint.accept())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(gate.read_u8().await.unwrap(), expected);
        gate.write_u8(0).await.unwrap();
    }
    let received = timeout(WAIT, read_frame(&mut caller))
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let receipt: Value = serde_json::from_slice(&received.body).unwrap();
    assert_eq!(receipt["settled_in_time"], true, "{receipt}");
    assert_eq!(receipt["unacknowledged_connections"], 0);
    assert_eq!(receipt["unacknowledged_tasks"], 0);
    assert!(timeout(UPGRADE_WAIT, daemon.process.wait())
        .await
        .unwrap()
        .unwrap()
        .success());
}

#[tokio::test]
async fn renamed_candidate_execs_same_pid_after_receipt() {
    let mut daemon = Daemon::boot(None).await;
    let pid = daemon.process.id().unwrap();
    let checksum = daemon.stage_compatible();
    let (ty, receipt) = daemon.request_upgrade(checksum.clone()).await;
    assert_eq!(ty, FrameType::Response, "{receipt}");
    assert_eq!(receipt["outcome"], "exec_started");
    assert_eq!(receipt["expected"]["core_version"], "1.2.3");
    assert!(timeout(UPGRADE_WAIT, daemon.process.wait())
        .await
        .unwrap()
        .unwrap()
        .success());
    assert_eq!(
        fs::read_to_string(daemon.root.join("replacement"))
            .unwrap()
            .trim(),
        pid.to_string()
    );
    let handover = fs::read_to_string(daemon.handover()).unwrap();
    let lines: Vec<_> = handover.lines().collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[1].len(), 64);
    let document: Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(document["attempt_id"], receipt["attempt_id"]);
    assert_eq!(document["verified_image"]["sha256"], checksum);
    #[cfg(target_os = "macos")]
    assert_eq!(
        document["verified_image"]["cdhash"].as_str().unwrap().len(),
        40
    );
    assert_eq!(document["locks"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn real_resume_republishes_and_keeps_the_registered_child_reachable() {
    use subc_client_rs::{CallOptions, SubcConsumer};
    use subc_protocol::{BindIdentity, RouteTarget};

    let daemon = Daemon::boot_fixture(
        None,
        "log",
        "approve",
        &[("upgrade-child", "carry")],
        true,
        false,
    )
    .await;
    let daemon_pid = daemon.process.id().unwrap();
    let module_pid = daemon.module_pid(None).await;
    let old = subc_transport::read_for_client(&daemon.connection).unwrap();
    let _old_tcp_client = connect_authed_client(&daemon.connection).await.unwrap();

    let (ty, receipt) = daemon.request_upgrade(daemon.stage_compatible()).await;
    assert_eq!(ty, FrameType::Response, "{receipt}");
    assert_eq!(receipt["carried"], json!(["upgrade-child"]));
    let describe = fresh_daemon(&daemon, old.daemon_id).await;
    assert_eq!(describe["counters"]["daemon_identity"]["pid"], daemon_pid);
    assert_eq!(
        describe["counters"]["daemon_identity"]["core_version"],
        receipt["expected"]["core_version"]
    );
    assert_eq!(
        describe["counters"]["daemon_identity"]["build_git_sha"],
        receipt["expected"]["build_git_sha"]
    );
    let fresh = subc_transport::read_for_client(&daemon.connection).unwrap();
    assert_eq!(fresh.endpoints[0].port, old.endpoints[0].port);
    assert_ne!(fresh.daemon_id, old.daemon_id);
    daemon.assert_local_serving().await;
    assert!(!daemon.handover().exists());
    let outcome = timeout(WAIT, async {
        loop {
            let outcome = daemon.last_upgrade().await;
            if outcome["outcome"] != "pending" {
                break outcome;
            }
            sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("child did not resume: {}", daemon_logs(&daemon.root)));
    assert_eq!(outcome["attempt_id"], receipt["attempt_id"]);
    assert_eq!(outcome["outcome"], "upgraded");
    assert_eq!(
        outcome["modules"],
        json!([{"id":"upgrade-child", "disposition":"resumed"}])
    );
    assert_eq!(daemon.module_pid(None).await, module_pid);
    assert!(subc_os::Process::open(module_pid)
        .unwrap()
        .unwrap()
        .observe()
        .is_some());
    let consumer = SubcConsumer::connect(&daemon.connection, Default::default())
        .await
        .unwrap();
    let reply = consumer
        .call(
            RouteTarget::ToolProvider {
                module_id: "upgrade-child".into(),
            },
            BindIdentity::new("resume-fixture", "session"),
            b"after the exec".to_vec(),
            CallOptions {
                timeout: WAIT,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(reply, b"after the exec");
    assert!(daemon_logs(&daemon.root).contains("resume_accept_ms"));
    assert_owned_files_in_scratch(
        &daemon,
        Some(Path::new(env!("CARGO_BIN_EXE_channel-module-rs"))),
    );
}

#[tokio::test]
async fn invalid_target_image_cold_boots_without_adopting_the_child() {
    let daemon = Daemon::boot_fixture(
        Some(("SUBC_TEST_UPGRADE_EXEC_PATH", "alternate-daemon")),
        "log",
        "approve",
        &[("upgrade-child", "carry")],
        true,
        false,
    )
    .await;
    let old = subc_transport::read_for_client(&daemon.connection).unwrap();
    let child = daemon.module_pid(None).await;
    let (ty, receipt) = daemon.request_upgrade(daemon.stage_compatible()).await;
    assert_eq!(ty, FrameType::Response, "{receipt}");
    assert_eq!(receipt["carried"], json!(["upgrade-child"]));
    let describe = fresh_daemon(&daemon, old.daemon_id).await;
    assert_eq!(
        describe["counters"]["daemon_identity"]["pid"],
        daemon.process.id().unwrap()
    );
    assert!(describe["counters"].get("last_upgrade").is_none());
    assert_ne!(daemon.module_pid(Some(child)).await, child);
    assert!(!daemon.handover().exists());
    assert!(daemon_logs(&daemon.root).contains("running image did not match the upgrade target"));
    let lock = fs::File::open(
        daemon
            .connection
            .with_file_name("subc-connection.json.start-lock"),
    )
    .unwrap();
    assert_eq!(
        rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive),
        Err(rustix::io::Errno::WOULDBLOCK),
    );
    assert_owned_files_in_scratch(
        &daemon,
        Some(Path::new(env!("CARGO_BIN_EXE_channel-module-rs"))),
    );
}

#[tokio::test]
async fn cold_cleanup_requires_the_run_directory_lock() {
    let mut owner = Daemon::boot(None).await;
    fs::write(owner.handover(), b"leftover handover").unwrap();
    let contender_runtime = owner.root.join("other-runtime");
    fs::create_dir(&contender_runtime).unwrap();
    let contender_connection = contender_runtime.join(subc_transport::CONNECTION_FILE_NAME);
    let contender =
        daemon_command(owner.root.path(), &owner.executable, &contender_connection).output();
    let result = timeout(WAIT, contender).await.unwrap().unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("run directory lock"));
    assert_eq!(fs::read(owner.handover()).unwrap(), b"leftover handover");
    assert!(!contender_connection.exists());
    assert_owned_files_in_scratch(&owner, None);

    let pid =
        rustix::process::Pid::from_raw(owner.process.id().unwrap().try_into().unwrap()).unwrap();
    rustix::process::kill_process(pid, rustix::process::Signal::TERM).unwrap();
    assert!(timeout(WAIT, owner.process.wait())
        .await
        .unwrap()
        .unwrap()
        .success());
    let mut next = daemon_command(owner.root.path(), &owner.executable, &contender_connection)
        .spawn()
        .unwrap();
    timeout(WAIT, async {
        while !contender_connection.exists() {
            assert!(
                next.try_wait().unwrap().is_none(),
                "next daemon exited below the run lock"
            );
            sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .unwrap();
    assert!(!owner.handover().exists());
    next.kill().await.unwrap();
}

#[tokio::test]
async fn malformed_resume_exits_before_it_opens_logs_or_writes_discovery() {
    let root = ShortRoot::create_short_socket_root();
    let run = root.join("data/cortexkit/run");
    fs::create_dir_all(&run).unwrap();
    let handover = run.join("daemon-upgrade.handover");
    fs::write(&handover, b"invalid envelope").unwrap();
    fs::set_permissions(&handover, fs::Permissions::from_mode(0o644)).unwrap();
    let connection = root
        .join("runtime")
        .join(subc_transport::CONNECTION_FILE_NAME);
    let output = timeout(
        WAIT,
        daemon_command(root.path(), &common::ckdev_subc_under_test(), &connection)
            .arg("--resume-handover")
            .arg(&handover)
            .args(["--handover-key-fd", "700"])
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("mode 0600"));
    assert_eq!(fs::read(&handover).unwrap(), b"invalid envelope");
    assert_eq!(fs::read_dir(run).unwrap().count(), 1);
    assert!(!connection.parent().unwrap().exists());
}

#[tokio::test]
async fn published_resume_authenticates_and_adopts_others_while_one_module_is_held() {
    const AUTH: Duration = Duration::from_secs(2);
    let daemon = Daemon::boot_fixture(
        Some(("SUBC_TEST_RESUME_PAUSE_MODULE", "upgrade-child")),
        "log",
        "approve",
        &[("upgrade-child", "carry"), ("z-ready-child", "carry")],
        true,
        false,
    )
    .await;
    let path = daemon.handover().with_file_name("rp.sock");
    let gate = tokio::net::UnixListener::bind(&path).unwrap();
    let held_pid = daemon.module_pid(None).await;
    let ready_pid = daemon.named_module_pid("z-ready-child", None).await;
    let old = subc_transport::read_for_client(&daemon.connection).unwrap();
    let (kind, receipt) = daemon.request_upgrade(daemon.stage_compatible()).await;
    assert_eq!(kind, FrameType::Response, "{receipt}");
    assert_eq!(
        receipt["carried"],
        json!(["upgrade-child", "z-ready-child"])
    );
    let (mut blocked, _) = timeout(UPGRADE_WAIT, gate.accept()).await.unwrap().unwrap();
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    assert_eq!(timeout(AUTH, blocked.read_u8()).await.unwrap().unwrap(), 1);
    let fresh = subc_transport::read_for_client(&daemon.connection).unwrap();
    assert_ne!(
        old.daemon_id, fresh.daemon_id,
        "preparation starts only after publication"
    );
    let _client = timeout(AUTH, connect_authed_client(&daemon.connection))
        .await
        .unwrap()
        .unwrap();
    let mut local = tokio::net::UnixStream::connect(daemon.socket())
        .await
        .unwrap();
    subc_transport::authenticate_client(&mut local, &fresh, AUTH)
        .await
        .unwrap();
    timeout(AUTH, async {
        loop {
            let report = daemon.last_upgrade().await;
            assert_eq!(
                report["modules"][0],
                json!({"id":"upgrade-child", "disposition":"pending"})
            );
            if report["modules"][1] == json!({"id":"z-ready-child", "disposition":"resumed"}) {
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the other carried module must not wait for the held install");
    blocked.write_u8(1).await.unwrap();
    timeout(WAIT, async {
        loop {
            if daemon.last_upgrade().await["outcome"] == "upgraded" {
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        daemon.last_upgrade().await["modules"][0]["disposition"],
        "resumed"
    );
    assert_eq!(daemon.module_pid(None).await, held_pid);
    assert_eq!(
        daemon.named_module_pid("z-ready-child", None).await,
        ready_pid
    );
}

#[tokio::test]
async fn authenticated_envelope_failure_after_exec_releases_all_three_locks_for_cold_boot() {
    let mut daemon = Daemon::boot_fixture(
        Some(("SUBC_TEST_UPGRADE_CORRUPT_HMAC", "1")),
        "log",
        "approve",
        &[],
        true,
        true,
    )
    .await;
    let old = subc_transport::read_for_client(&daemon.connection).unwrap();
    let lock_paths = [
        daemon.handover().with_file_name("daemon.lock"),
        daemon
            .connection
            .with_file_name("subc-connection.json.start-lock"),
        daemon.root.join("authority.lock"),
    ];
    fn assert_locked(paths: &[PathBuf]) {
        for path in paths {
            let file = fs::File::open(path).unwrap();
            assert_eq!(
                rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive),
                Err(rustix::io::Errno::WOULDBLOCK),
                "{}",
                path.display()
            );
        }
    }
    assert_locked(&lock_paths);
    let (kind, receipt) = daemon.request_upgrade(daemon.stage_compatible()).await;
    assert_eq!(kind, FrameType::Response, "{receipt}");
    let status = timeout(UPGRADE_WAIT, daemon.process.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(!status.success());
    assert!(daemon_logs(&daemon.root).contains("invalid handover authentication tag"));
    assert_eq!(
        fs::metadata(daemon.handover()).unwrap().mode() & 0o777,
        0o600
    );
    let raw = fs::read_to_string(daemon.handover()).unwrap();
    let document: Value = serde_json::from_str(raw.lines().next().unwrap()).unwrap();
    assert_eq!(document["locks"].as_array().unwrap().len(), 3);
    assert_eq!(
        subc_transport::read_for_client(&daemon.connection)
            .unwrap()
            .daemon_id,
        old.daemon_id
    );

    let mut next = daemon_command(daemon.root.path(), &daemon.executable, &daemon.connection);
    with_isolated_store(&mut next, &daemon.root);
    daemon.process = next.spawn().unwrap();
    let _ = fresh_daemon(&daemon, old.daemon_id).await;
    assert!(!daemon.handover().exists());
    assert_locked(&lock_paths);
    daemon.assert_local_serving().await;
}

#[tokio::test]
async fn incompatible_and_mismatched_targets_do_not_change_the_daemon() {
    let daemon = Daemon::boot(None).await;
    // The shipping binary now advertises format 2. Keep exercising an older
    // build-info response that cannot read the handover the daemon would write.
    let old = daemon.root.join("old-daemon");
    fs::write(
        &old,
        b"#!/bin/sh\nprintf '%s\\n' '{\"core_version\":\"0.0.1\",\"handover_versions\":[]}'\n",
    )
    .unwrap();
    fs::set_permissions(&old, fs::Permissions::from_mode(0o700)).unwrap();
    let checksum = daemon.stage(&old);
    let (ty, wrong) = daemon.request_upgrade("0".repeat(64)).await;
    assert_eq!(ty, FrameType::Error);
    assert_eq!(wrong["code"], "upgrade_executable_mismatch");
    let (ty, incompatible) = daemon.request_upgrade(checksum).await;
    assert_eq!(ty, FrameType::Error);
    assert_eq!(incompatible["code"], "upgrade_target_incompatible");
    assert!(!daemon.handover().exists());
    let (_, response) = daemon.call(json!({"op":"server.describe"})).await.unwrap();
    assert_eq!(
        response["counters"]["daemon_identity"]["pid"],
        daemon.process.id().unwrap()
    );
    assert!(response["counters"]["last_upgrade"].is_null());
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn macos_rejects_a_candidate_without_the_required_signing_team() {
    let daemon = Daemon::boot(Some(("SUBC_TEST_UPGRADE_TEAM_ID", "TESTTEAM01"))).await;
    let (kind, error) = daemon.request_upgrade(daemon.stage_compatible()).await;
    assert_eq!(kind, FrameType::Error);
    assert_eq!(error["code"], "upgrade_signature_mismatch", "{error}");
    assert!(!daemon.handover().exists());
    daemon.assert_local_serving().await;
}

#[tokio::test]
async fn handover_write_and_exec_failure_resume_the_original_listeners() {
    for (key, value, stage) in [
        (
            "SUBC_TEST_UPGRADE_HANDOVER_WRITE_FAILURE",
            "1",
            "handover_write",
        ),
        (
            "SUBC_TEST_UPGRADE_EXEC_PATH",
            "/nonexistent/subc/fixture",
            "exec",
        ),
    ] {
        let daemon = Daemon::boot(Some((key, value))).await;
        let socket_inode = fs::metadata(daemon.socket()).unwrap().ino();
        let (ty, receipt) = daemon.request_upgrade(daemon.stage_compatible()).await;
        assert_eq!(ty, FrameType::Response, "{receipt}");
        let failure = daemon.last_upgrade().await;
        assert_eq!(failure["outcome"], "failed");
        assert_eq!(failure["stage"], stage);
        assert_eq!(failure["attempt_id"], receipt["attempt_id"]);
        assert!(!daemon.handover().exists());
        assert_eq!(fs::metadata(daemon.socket()).unwrap().ino(), socket_inode);
        daemon.assert_local_serving().await;
    }
}

async fn boot_with_receipt_gate() -> (Daemon, tokio::net::UnixListener) {
    let daemon = Daemon::boot(Some(("SUBC_TEST_UPGRADE_RECEIPT_GATE", "receipt-gate"))).await;
    let gate = tokio::net::UnixListener::bind(daemon.root.join("receipt-gate")).unwrap();
    (daemon, gate)
}

async fn receipt_checkpoint(
    gate: &tokio::net::UnixListener,
    expected: u8,
) -> tokio::net::UnixStream {
    use tokio::io::AsyncReadExt;

    timeout(UPGRADE_WAIT, async {
        let (mut stream, _) = gate.accept().await.unwrap();
        assert_eq!(stream.read_u8().await.unwrap(), expected);
        stream
    })
    .await
    .unwrap_or_else(|_| panic!("upgrade did not reach receipt checkpoint {expected}"))
}

#[cfg(target_os = "linux")]
fn stage_racing_placement(daemon: &Daemon) -> PathBuf {
    let next = daemon.root.join("racing-placement");
    copy_executable(&common::ckdev_subc(), &next);
    next
}

#[tokio::test]
async fn stalled_receipt_and_caller_disconnect_do_not_cancel_exec() {
    use tokio::io::AsyncWriteExt;

    let (mut daemon, gate) = boot_with_receipt_gate().await;
    let checksum = daemon.stage_compatible();
    #[cfg(target_os = "linux")]
    let next = stage_racing_placement(&daemon);
    let caller = daemon
        .issue(json!({"op":"daemon.upgrade_in_place", "executable_sha256": checksum}))
        .await
        .unwrap();
    let mut receipt = receipt_checkpoint(&gate, 1).await;
    assert!(daemon.socket().exists());
    // Replacing the path before the final hash leaves the verified descriptor
    // valid; replacing it during the hash would change its sampled metadata.
    #[cfg(target_os = "linux")]
    fs::rename(next, &daemon.executable).unwrap();
    drop(caller);
    receipt.write_u8(0).await.unwrap();
    receipt_checkpoint(&gate, 2)
        .await
        .write_u8(0)
        .await
        .unwrap();
    assert!(timeout(UPGRADE_WAIT, daemon.process.wait())
        .await
        .unwrap_or_else(|_| panic!("replacement did not exit: {}", daemon_logs(&daemon.root)))
        .unwrap()
        .success());
    assert!(daemon.root.join("replacement").is_file());
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn candidate_path_replaced_during_final_hash_refuses_exec() {
    use tokio::io::AsyncWriteExt;

    let (daemon, gate) = boot_with_receipt_gate().await;
    let checksum = daemon.stage_compatible();
    let next = stage_racing_placement(&daemon);
    let _caller = daemon
        .issue(json!({"op":"daemon.upgrade_in_place", "executable_sha256": checksum}))
        .await
        .unwrap();
    receipt_checkpoint(&gate, 1)
        .await
        .write_u8(0)
        .await
        .unwrap();
    let mut rehash = receipt_checkpoint(&gate, 2).await;
    fs::rename(next, &daemon.executable).unwrap();
    rehash.write_u8(0).await.unwrap();

    let failed = daemon.last_upgrade().await;
    assert_eq!(failed["outcome"], "failed", "{failed}");
    assert_eq!(failed["stage"], "rehash", "{failed}");
    assert!(daemon_logs(&daemon.root).contains("candidate changed while hashing"));
    assert!(!daemon.handover().exists());
    assert!(!daemon.root.join("replacement").exists());
    daemon.assert_local_serving().await;
}

#[tokio::test]
async fn restart_intent_refuses_upgrade_without_creating_a_handover() {
    let daemon = Daemon::boot(None).await;
    let checksum = daemon.stage_compatible();
    assert_eq!(
        daemon
            .call(json!({"op":"daemon.shutdown_intent","intent":"restart"}))
            .await
            .unwrap()
            .0,
        FrameType::Response
    );
    let (ty, reply) = daemon.request_upgrade(checksum).await;
    assert_eq!(ty, FrameType::Error);
    assert_eq!(reply["code"], "upgrade_busy");
    assert_eq!(reply["detail"]["reason"], "shutdown");
    assert!(!daemon.handover().exists());
}

#[tokio::test]
async fn upgrade_without_approval_is_refused_before_any_child_stops() {
    let daemon = Daemon::boot_with_module(None, "log", "decline", Some("restart")).await;
    let previous = daemon.module_pid(None).await;
    let (kind, error) = daemon.request_upgrade(daemon.stage_compatible()).await;
    assert_eq!(kind, FrameType::Error);
    assert_eq!(error["code"], "operator_declined", "{error}");
    assert_eq!(daemon.module_pid(None).await, previous);
    assert!(subc_os::Process::open(previous).unwrap().is_some());
    assert!(!daemon.handover().exists());
}

#[tokio::test]
async fn failed_notice_flush_aborts_without_claiming_announcement() {
    let daemon = Daemon::boot_with_module(
        Some(("SUBC_TEST_UPGRADE_NOTICE_FAILURE", "upgrade-child")),
        "log",
        "approve",
        Some("carry"),
    )
    .await;
    let previous = daemon.module_pid(None).await;
    let (kind, error) = daemon.request_upgrade(daemon.stage_compatible()).await;
    assert_eq!(kind, FrameType::Error);
    assert_eq!(error["code"], "upgrade_failed", "{error}");
    assert_eq!(error["detail"]["stage"], "reader_park");
    assert_eq!(error["detail"]["announced"], json!([]));
    assert_eq!(error["detail"]["restarted"], json!(["upgrade-child"]));
    assert_ne!(daemon.module_pid(Some(previous)).await, previous);
    assert!(!daemon.handover().exists());
    daemon.assert_local_serving().await;
}

#[tokio::test]
async fn retry_after_unwind_gets_a_fresh_attempt_and_execs() {
    let mut daemon = Daemon::boot(Some(("SUBC_TEST_UPGRADE_HANDOVER_WRITE_FAILURE", "1"))).await;
    let checksum = daemon.stage_compatible();
    let (kind, first) = daemon.request_upgrade(checksum.clone()).await;
    assert_eq!(kind, FrameType::Response, "{first}");
    assert_eq!(
        daemon.last_upgrade().await["attempt_id"],
        first["attempt_id"]
    );
    let (kind, second) = daemon.request_upgrade(checksum).await;
    assert_eq!(kind, FrameType::Response, "{second}");
    assert_ne!(first["attempt_id"], second["attempt_id"]);
    assert_eq!(second["refusals_tracked"], 0);
    assert_eq!(second["refusals_unconfirmed"], 0);
    assert!(timeout(WAIT, daemon.process.wait())
        .await
        .unwrap()
        .unwrap()
        .success());
}

#[tokio::test]
async fn registered_module_cannot_request_upgrade_in_either_prompt_mode() {
    for prompt in ["log", "enforce"] {
        let daemon = Daemon::boot_with_prompt(None, prompt).await;
        let mut module = connect_authed_client(&daemon.connection).await.unwrap();
        let flags = Flags::new(false, Priority::Passive, false);
        let body = subc_protocol::ModuleHelloBody {
            manifest: subc_protocol::manifest::ModuleManifest::builder(
                "upgrade-refusal-fixture",
                "1.0.0",
            )
            .build(),
            protocol_ver: subc_protocol::PROTOCOL_VERSION,
            control_ops: None,
            module_capabilities: None,
            launch_nonce: None,
            resume_token: None,
        };
        let hello = Frame::build(
            FrameType::Hello,
            flags,
            0,
            0,
            1,
            serde_json::to_vec(&body).unwrap(),
        )
        .unwrap();
        write_frame(&mut module, &hello).await.unwrap();
        let ack = timeout(WAIT, read_frame(&mut module))
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(ack.header.ty, FrameType::HelloAck);
        let request = Frame::build(
            FrameType::Request,
            flags,
            0,
            0,
            2,
            serde_json::to_vec(&json!({
                "op":"daemon.upgrade_in_place", "executable_sha256": "0".repeat(64),
            }))
            .unwrap(),
        )
        .unwrap();
        write_frame(&mut module, &request).await.unwrap();
        let refusal = timeout(Duration::from_secs(2), read_frame(&mut module))
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let body: Value = serde_json::from_slice(&refusal.body).unwrap();
        assert_eq!(refusal.header.ty, FrameType::Error);
        assert_eq!(refusal.header.corr, 2);
        assert_eq!(body["code"], "unsupported_control_frame", "{prompt}");
    }
}
