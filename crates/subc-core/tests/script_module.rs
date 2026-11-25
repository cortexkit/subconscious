#![forbid(unsafe_code)]

//! Run with the prerequisites and exact command in `script_module/README.md`.

mod common;

use serde_json::{json, Value};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};
use subc_protocol::{Flags, Frame, FrameType, Priority};
use subc_transport::{read_frame, write_frame};
use tokio::io::{AsyncRead, AsyncWrite};

const MODULE: &str = "script-module-e2e";
const LIMIT: Duration = Duration::from_secs(30);

trait Stream: AsyncRead + AsyncWrite + Unpin {}
impl<T: AsyncRead + AsyncWrite + Unpin> Stream for T {}

struct Rig {
    child: Option<Child>,
    data: common::kit::ShortRoot,
    root: cortexkit_test_support::ScratchDir,
}

impl Drop for Rig {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            #[cfg(unix)]
            if let Some(pid) = rustix::process::Pid::from_raw(child.id() as i32) {
                let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                while child.try_wait().ok().flatten().is_none()
                    && std::time::Instant::now() < deadline
                {
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
            let _ = child.kill();
            let _ = child.wait();
        }
        if std::thread::panicking() {
            for name in ["daemon.err", "daemon.out", "operator-events"] {
                eprintln!(
                    "{name}: {}",
                    fs::read_to_string(self.root.join(name)).unwrap_or_default()
                );
            }
            let logs = self.data.join("cortexkit/run/logs");
            for entry in fs::read_dir(logs).into_iter().flatten().flatten() {
                eprintln!(
                    "{}: {}",
                    entry.path().display(),
                    fs::read_to_string(entry.path()).unwrap_or_default()
                );
            }
        }
    }
}

fn checked(command: &mut Command) {
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("{command:?}: {error}"));
    assert!(
        output.status.success(),
        "{command:?}: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn copy_tree(source: &Path, destination: &Path) {
    fs::create_dir_all(destination).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let target = destination.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

impl Rig {
    fn new() -> Self {
        let root = cortexkit_test_support::ScratchDir::new("script-module");
        for name in [
            "home",
            "runtime",
            "config/cortexkit",
            "data",
            "authority",
            "install",
            "empty-path",
        ] {
            fs::create_dir_all(root.join(name)).unwrap();
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(root.join("runtime"), fs::Permissions::from_mode(0o700)).unwrap();
        }
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap();
        let runner = workspace
            .join("clients/script-runner/dist")
            .join(executable("ck-script-runner-bun"));
        assert!(
            runner.is_file(),
            "build the compiled runner first: {}",
            runner.display()
        );
        fs::copy(
            runner,
            root.join("install")
                .join(executable("ck-script-runner-bun")),
        )
        .unwrap();
        fs::copy(
            env!("CARGO_BIN_EXE_ck-subc"),
            root.join("install").join(executable("ckdev-script-daemon")),
        )
        .unwrap();

        // Copy the package, rather than aliasing its source entry, so Bun must
        // resolve the SDK's real exports and ck-script-runner condition.
        let package = root.join("node_modules/@cortexkit/subc-client");
        let sdk = workspace.join("clients/subc-client");
        copy_tree(&sdk.join("dist"), &package.join("dist"));
        copy_tree(&sdk.join("src"), &package.join("src"));
        fs::copy(sdk.join("package.json"), package.join("package.json")).unwrap();
        fs::write(
            root.join("module.ts"),
            include_str!("script_module/module.ts"),
        )
        .unwrap();
        checked(Command::new("bun").current_dir(root.path()).args([
            "build",
            "--target=bun",
            "--format=esm",
            "--conditions=ck-script-runner",
            "--outfile=module.js",
            "module.ts",
        ]));
        checked(
            Command::new(
                root.join("install")
                    .join(executable("ck-script-runner-bun")),
            )
            .arg("--check")
            .arg(root.join("module.js")),
        );
        fs::write(root.join("operator"), "approve\n".repeat(8)).unwrap();
        let script_path = root.join("module.js");
        #[cfg(unix)]
        let script_path = fs::canonicalize(script_path).unwrap();
        fs::write(
            root.join("config/cortexkit/subc.jsonc"),
            serde_json::to_vec(&json!({
                "version": 1, "port": 0, "modules": { MODULE: {
                    "script": {"runtime": "bun", "path": script_path},
                    "enabled": true, "registration_channel": true,
                    "env": {"SCRIPT_MODULE_EVENTS": root.join("module-events")}
                }}
            }))
            .unwrap(),
        )
        .unwrap();
        Self {
            child: None,
            data: common::kit::ShortRoot::create_short_socket_root(),
            root,
        }
    }

    async fn start(&mut self) {
        // /tmp is a symlink on macOS. Resolve Unix paths before configuring the
        // launch, but retain ordinary drive paths on Windows.
        #[cfg(unix)]
        let physical_root = fs::canonicalize(self.root.path()).unwrap();
        #[cfg(not(unix))]
        let physical_root = self.root.path().to_owned();
        let mut command = cortexkit_test_support::dev_command(
            physical_root
                .join("install")
                .join(executable("ckdev-script-daemon")),
        );
        command
            .arg("--config-store-root")
            .arg(self.root.join("authority"))
            .env("HOME", self.root.join("home"))
            .env("USERPROFILE", self.root.join("home"))
            .env("LOCALAPPDATA", self.data.path())
            .env("APPDATA", self.root.join("config"))
            .env("XDG_DATA_HOME", self.data.path())
            .env("XDG_RUNTIME_DIR", self.root.join("runtime"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("SUBC_CONNECTION_FILE", self.connection())
            .env("SUBC_PORT", "0")
            .env("SUBC_CGROUP_PLACEMENT", "disabled")
            .env("SUBC_TEST_CONFIG_GATE", "1")
            .env("SUBC_TEST_OPERATOR_SCRIPT", self.root.join("operator"))
            .env(
                "SUBC_TEST_OPERATOR_EVENTS",
                self.root.join("operator-events"),
            )
            // A successful launch cannot have found an interpreter on PATH.
            .env("PATH", self.root.join("empty-path"))
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .env_remove("SUBC_MODULE_ID")
            .env_remove("SUBC_LAUNCH_NONCE")
            .env_remove("SUBC_LAUNCH_NONCE_FD")
            .stdin(Stdio::null())
            .stdout(fs::File::create(self.root.join("daemon.out")).unwrap())
            .stderr(fs::File::create(self.root.join("daemon.err")).unwrap());
        self.child = Some(command.spawn().unwrap());
        tokio::time::timeout(LIMIT, async {
            while !self.connection().exists() {
                assert!(
                    self.child.as_mut().unwrap().try_wait().unwrap().is_none(),
                    "daemon exited before publishing connection file"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("daemon connection file");
    }

    fn connection(&self) -> PathBuf {
        self.root.join("runtime/subc-connection.json")
    }

    async fn client(&self) -> Box<dyn Stream> {
        let published =
            subc_transport::connection_file::read_for_client_with_local_endpoint(self.connection())
                .unwrap();
        let mut stream: Box<dyn Stream> = match published
            .local_endpoint
            .expect("local endpoint for approval")
        {
            #[cfg(unix)]
            subc_transport::LocalEndpoint::Unix { path } => {
                Box::new(tokio::net::UnixStream::connect(path).await.unwrap())
            }
            #[cfg(windows)]
            subc_transport::LocalEndpoint::Pipe { name } => Box::new(
                tokio::net::windows::named_pipe::ClientOptions::new()
                    .open(name)
                    .unwrap(),
            ),
            endpoint => panic!("unsupported local endpoint: {endpoint:?}"),
        };
        subc_transport::authenticate_client_with_role(
            &mut stream,
            &published.info,
            LIMIT,
            "desktop-app",
        )
        .await
        .unwrap();
        stream
    }

    fn launches(&self) -> String {
        fs::read_to_string(self.root.join("module-events")).unwrap_or_default()
    }
}

fn executable(name: &str) -> String {
    format!("{name}{}", std::env::consts::EXE_SUFFIX)
}

async fn request(
    stream: &mut Box<dyn Stream>,
    channel: u16,
    epoch: u32,
    body: Value,
) -> (FrameType, Value) {
    let frame = Frame::build(
        FrameType::Request,
        Flags::new(false, Priority::Interactive, false),
        channel,
        epoch,
        17,
        serde_json::to_vec(&body).unwrap(),
    )
    .unwrap();
    write_frame(stream, &frame).await.unwrap();
    loop {
        let response = tokio::time::timeout(LIMIT, read_frame(stream))
            .await
            .unwrap()
            .unwrap()
            .expect("response before EOF");
        if response.header.corr == 17 {
            return (
                response.header.ty,
                serde_json::from_slice(&response.body).unwrap(),
            );
        }
    }
}

async fn rpc(stream: &mut Box<dyn Stream>, body: Value) -> Value {
    let (kind, response) = request(stream, 0, 0, body).await;
    assert_eq!(kind, FrameType::Response, "{response}");
    response
}

async fn entry(stream: &mut Box<dyn Stream>) -> Value {
    let response = rpc(stream, json!({"op":"supervisor.list"})).await;
    response["modules"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["module_id"] == MODULE)
        .expect("configured script entry")
        .clone()
}

async fn wait_entry(stream: &mut Box<dyn Stream>, ready: impl Fn(&Value) -> bool) -> Value {
    tokio::time::timeout(LIMIT, async {
        loop {
            let current = entry(stream).await;
            if ready(&current) {
                return current;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("supervisor state transition")
}

async fn observed_process(client: &mut Box<dyn Stream>) -> Value {
    let provenance = rpc(
        client,
        json!({"op":"supervisor.provenance","module_id":MODULE}),
    )
    .await;
    provenance["modules"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["module_id"] == MODULE)
        .unwrap()["daemon_observed"]
        .clone()
}

async fn round_trip(rig: &Rig, client: &mut Box<dyn Stream>) -> u64 {
    wait_entry(client, |entry| {
        entry["state"] == "running" && entry["live"] == true
    })
    .await;
    let catalog = tokio::time::timeout(LIMIT, async {
        loop {
            let catalog = rpc(client, json!({"op":"catalog.list"})).await;
            if catalog["modules"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["module_id"] == MODULE)
            {
                return catalog;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("script registration in catalog");
    let module = catalog["modules"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["module_id"] == MODULE)
        .expect("script in catalog");
    assert!(
        module["roles"]
            .as_array()
            .unwrap()
            .iter()
            .any(|role| role["role"] == "management_surface"
                && role["operations"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|op| op["name"] == "echo")),
        "{module}"
    );
    let route = rpc(client, json!({"op":"route.open", "target":{"kind":"management_surface","module_id":MODULE}, "identity":{"project_root":rig.root.path(),"harness":"script-e2e","session":"round-trip"}})).await;
    let body = json!({"method":"echo","params":{"text":"real daemon","number":42}});
    let (kind, answer) = request(
        client,
        route["route_channel"].as_u64().unwrap() as u16,
        route["route_epoch"].as_u64().unwrap() as u32,
        body.clone(),
    )
    .await;
    assert_eq!(kind, FrameType::Response, "{answer}");
    assert_eq!(answer["answer"], "from the compiled script runner");
    assert_eq!(answer["request"], body);
    let observed = observed_process(client).await;
    let pid = observed["pid"]
        .as_u64()
        .expect("daemon observes a live PID");
    assert_eq!(answer["pid"], pid, "the runner is the supervised process");
    let actual = fs::canonicalize(answer["executable"].as_str().unwrap()).unwrap();
    let expected = fs::canonicalize(
        rig.root
            .join("install")
            .join(executable("ck-script-runner-bun")),
    )
    .unwrap();
    assert_eq!(actual, expected, "not bun from PATH");
    answer["pid"].as_u64().unwrap()
}

async fn set_enabled(client: &mut Box<dyn Stream>, enabled: bool) -> (FrameType, Value) {
    request(
        client,
        0,
        0,
        json!({"op":"supervisor.set_enabled", "module_id":MODULE,"enabled":enabled}),
    )
    .await
}

#[tokio::test]
#[ignore = "requires Bun, the compiled script runner, SDK dist and dev-desktop-approval; see script_module/README.md"]
async fn script_module_real_daemon() {
    let mut rig = Rig::new();
    rig.start().await;
    let mut client = rig.client().await;
    let initial = rpc(&mut client, json!({"op":"config.status"})).await;
    assert_eq!(initial["status"]["state"], "unstamped", "{initial}");
    let first_pid = round_trip(&rig, &mut client).await;

    let plan = rpc(&mut client, json!({"op":"config.stamp"})).await;
    let applied = rpc(
        &mut client,
        json!({"op":"config.apply","digest":plan["view"]["digest"]}),
    )
    .await;
    assert_eq!(applied["generation"], 1, "{applied}");
    let stamped = rpc(&mut client, json!({"op":"config.status"})).await;
    assert_eq!(stamped["status"]["state"], "stamped", "{stamped}");
    let second_pid = round_trip(&rig, &mut client).await;
    assert_ne!(
        first_pid, second_pid,
        "stamped launch must be a new process"
    );
    assert_eq!(rig.launches().lines().count(), 2);

    assert_eq!(set_enabled(&mut client, false).await.0, FrameType::Response);
    wait_entry(&mut client, |entry| {
        entry["live"] == false && entry["state"] == "disabled"
    })
    .await;
    assert!(observed_process(&mut client).await["pid"].is_null());
    let stopped = entry(&mut client).await;
    let launches = rig.launches();
    fs::write(
        rig.root.join("module.js"),
        format!(
            "import {{ appendFileSync }} from 'node:fs'; appendFileSync({}, 'TAMPERED');\n",
            serde_json::to_string(&rig.root.join("module-events")).unwrap()
        ),
    )
    .unwrap();
    let (kind, refused) = set_enabled(&mut client, true).await;
    assert_eq!(kind, FrameType::Error, "{refused}");
    assert_eq!(refused["code"], "config_gated", "{refused}");
    assert!(
        refused["message"]
            .as_str()
            .unwrap()
            .contains("program_refused"),
        "{refused}"
    );
    assert!(
        refused["message"]
            .as_str()
            .unwrap()
            .contains("covered program identity is not approved"),
        "{refused}"
    );
    let held = wait_entry(&mut client, |entry| {
        entry["state"] == "stopped" && entry["live"] == false
    })
    .await;
    assert_eq!(held["script"]["needs_review"], true, "{held}");
    assert_eq!(held["live"], false, "{held}");
    assert!(observed_process(&mut client).await["pid"].is_null());
    assert_eq!(
        held["spawn_generation"], stopped["spawn_generation"],
        "no runner was spawned"
    );
    assert_eq!(
        rig.launches(),
        launches,
        "neither original nor replacement script ran"
    );
    let catalog = rpc(&mut client, json!({"op":"catalog.list"})).await;
    assert!(
        !catalog["modules"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["module_id"] == MODULE),
        "{catalog}"
    );

    #[cfg(unix)]
    {
        drop(client);
        drop(rig);
        let mut linked = Rig::new();
        let physical_root = fs::canonicalize(linked.root.path()).unwrap();
        std::os::unix::fs::symlink(&physical_root, linked.root.join("linked")).unwrap();
        let config_path = linked.root.join("config/cortexkit/subc.jsonc");
        let mut config: Value = serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
        config["modules"][MODULE]["script"]["path"] = json!(physical_root.join("linked/module.js"));
        fs::write(config_path, serde_json::to_vec(&config).unwrap()).unwrap();
        linked.start().await;
        let mut client = linked.client().await;
        let refused = wait_entry(&mut client, |entry| {
            entry["state"] == "failed" && entry["live"] == false
        })
        .await;
        assert_eq!(refused["live"], false, "{refused}");
        assert!(observed_process(&mut client).await["pid"].is_null());
        assert_eq!(refused["spawn_generation"], 0, "{refused}");
        assert!(
            linked.launches().is_empty(),
            "a symlinked script folder must not execute"
        );
        let logs = fs::read_dir(linked.data.join("cortexkit/run/logs"))
            .unwrap()
            .map(|entry| fs::read_to_string(entry.unwrap().path()).unwrap_or_default())
            .collect::<String>();
        assert!(
            logs.contains("program_refused: script could not be read"),
            "{logs}"
        );
    }
}
