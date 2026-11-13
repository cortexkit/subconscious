//! Real-daemon child-bound registration acceptance. Build helper binaries as in
//! `scripts/test-daemon-lib.sh`, with `subc-daemon/dev-desktop-approval` enabled.
#![cfg(all(unix, feature = "test-support"))]

use cortexkit_test_support::{ckdev_binary, ScratchDir};
use serde_json::{json, Value};
use std::{
    fs,
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::Duration,
};
use subc_daemon::{read_frame, write_frame, Frame};
use subc_protocol::{manifest::ModuleManifest, Flags, FrameType, Priority};
use tokio::{
    net::UnixStream,
    time::{sleep, timeout},
};

struct Rig {
    root: ScratchDir,
    daemon: Child,
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
        if std::thread::panicking() {
            eprintln!(
                "daemon: {}",
                fs::read_to_string(self.root.join("daemon.log")).unwrap_or_default()
            );
            dump_logs(&self.root.join("data/cortexkit/run/logs"));
            for id in ["caller", "target"] {
                eprintln!(
                    "{id}: {}",
                    fs::read_to_string(self.root.join(format!("{id}.events"))).unwrap_or_default()
                );
            }
        }
    }
}

fn dump_logs(path: &std::path::Path) {
    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            dump_logs(&entry.path());
        }
    } else if let Ok(log) = fs::read_to_string(path) {
        eprintln!("{}: {log}", path.display());
    }
}

impl Rig {
    async fn start(gated: bool, typescript: bool) -> Self {
        let root = ScratchDir::new("cbr");
        for dir in ["home", "config/cortexkit", "data", "runtime", "authority"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        fs::write(root.join("operator"), "approve\n".repeat(16)).unwrap();
        let module = |id: &str| {
            let (program, args) = if typescript && id == "caller" {
                (
                    PathBuf::from(
                        std::env::var_os("CHILD_BOUND_BUN")
                            .expect("set CHILD_BOUND_BUN to the absolute bun path"),
                    ),
                    vec![PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                        .join("tests/fixtures/channel_module.ts")],
                )
            } else {
                (env!("CARGO_BIN_EXE_channel-module-rs").into(), vec![])
            };
            fs::write(root.join(format!("config/cortexkit/{id}.jsonc")), "{}").unwrap();
            let mut env = json!({"CHANNEL_MODULE_ROOT":root.path()});
            if !gated {
                env["CHANNEL_MODULE_LEGACY"] = json!("1");
            }
            json!({"program":program,"args":args,"enabled":true,"reserved":true,
                "registration_channel":gated,"env":env})
        };
        fs::write(
            root.join("config/cortexkit/subc.jsonc"),
            json!({
                "version":1,"port":0,"modules":{"caller":module("caller"),"target":module("target")}
            })
            .to_string(),
        )
        .unwrap();
        let mut command = Command::new(ckdev_binary(env!("CARGO_BIN_EXE_ck-subc-under-test")));
        command
            .env("HOME", root.join("home"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_RUNTIME_DIR", root.join("runtime"))
            .env(
                "SUBC_CONNECTION_FILE",
                root.join("runtime/subc-connection.json"),
            )
            .env("SUBC_CGROUP_PLACEMENT", "disabled")
            .env("SUBC_TEST_CONFIG_GATE", "1")
            .env("SUBC_TEST_OPERATOR_SCRIPT", root.join("operator"))
            .env_remove("SUBC_PORT")
            .env_remove("SUBC_MODULE_ID")
            .env_remove("SUBC_LAUNCH_NONCE")
            .env_remove("SUBC_LAUNCH_NONCE_FD")
            .env_remove("SUBC_REGISTRATION_FD")
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .arg("--config-store-root")
            .arg(root.join("authority"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(fs::File::create(root.join("daemon.log")).unwrap());
        let rig = Self {
            daemon: command.spawn().unwrap(),
            root,
        };
        rig.wait(|| {
            subc_transport::connection_file::read_with_local_endpoint(
                rig.root.join("runtime/subc-connection.json"),
            )
            .is_ok_and(|published| published.local_endpoint.is_some())
        })
        .await;
        // Never use default discovery: both the published PID and the endpoint must
        // belong to this test's child, not an already running daemon.
        let published = rig.published();
        assert_eq!(published.info.pid, rig.daemon.id());
        let Some(subc_transport::LocalEndpoint::Unix { path }) = published.local_endpoint else {
            panic!("expected isolated Unix endpoint")
        };
        assert!(
            path.starts_with(rig.root.path()),
            "endpoint escaped isolated root: {path:?}"
        );
        rig.wait(|| {
            ["caller", "target"]
                .iter()
                .all(|id| rig.events(id).iter().any(|e| e["event"] == "config"))
        })
        .await;
        rig
    }

    fn published(&self) -> subc_transport::connection_file::ConnectionInfoWithLocalEndpoint {
        subc_transport::connection_file::read_with_local_endpoint(
            self.root.join("runtime/subc-connection.json"),
        )
        .unwrap()
    }

    async fn client(&self) -> UnixStream {
        let published = self.published();
        assert_eq!(
            published.info.pid,
            self.daemon.id(),
            "refuse a live daemon connection file"
        );
        let Some(subc_transport::LocalEndpoint::Unix { path }) = published.local_endpoint else {
            panic!("missing Unix endpoint")
        };
        assert!(path.starts_with(self.root.path()));
        let mut stream = UnixStream::connect(path).await.unwrap();
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

    async fn wait(&self, ready: impl Fn() -> bool) {
        timeout(Duration::from_secs(30), async {
            while !ready() {
                sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("isolated module reaches expected state");
    }

    fn events(&self, id: &str) -> Vec<Value> {
        fs::read_to_string(self.root.join(format!("{id}.events")))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    async fn stamp(&self) {
        let mut client = self.client().await;
        let plan = rpc(&mut client, json!({"op":"config.stamp"})).await;
        assert!(plan["view"]["digest"].is_string(), "{plan}");
        let applied = rpc(
            &mut client,
            json!({"op":"config.apply","digest":plan["view"]["digest"]}),
        )
        .await;
        assert_eq!(applied["generation"], 1, "{applied}");
        self.wait(|| {
            ["caller", "target"]
                .iter()
                .all(|id| self.events(id).iter().any(|e| e["state"] == "active"))
        })
        .await;
    }

    async fn exercise(&self, typescript: bool) {
        fs::write(self.root.join("go"), "").unwrap();
        self.wait(|| self.root.join("result.json").exists()).await;
        let result: Value =
            serde_json::from_slice(&fs::read(self.root.join("result.json")).unwrap()).unwrap();
        assert_eq!(result["sync"]["Ok"], 1, "{result}");
        assert_eq!(result["apply"]["Ok"], 2, "{result}");
        if !typescript {
            assert_eq!(result["agents"], json!({"Ok":null}), "{result}");
        }
        let expected = if typescript {
            json!({"message":"child-bound roundtrip"})
        } else {
            json!("child-bound roundtrip")
        };
        assert_eq!(result["reply"]["Ok"], expected, "{result}");
        let bind = self
            .events("target")
            .into_iter()
            .find(|e| e["event"] == "bind" && e["identity"]["harness"] == "channel-fixture")
            .expect("target saw fixture bind");
        assert_eq!(
            bind["principal"],
            json!({"kind":"reserved","module_id":"caller"}),
            "{bind}, result: {result}"
        );
    }
}

async fn rpc(stream: &mut UnixStream, body: Value) -> Value {
    let frame = Frame::build(
        FrameType::Request,
        Flags::new(false, Priority::Interactive, false),
        0,
        0,
        17,
        serde_json::to_vec(&body).unwrap(),
    )
    .unwrap();
    write_frame(stream, &frame).await.unwrap();
    timeout(Duration::from_secs(15), async {
        loop {
            let reply = read_frame(stream).await.unwrap().expect("daemon replied");
            if reply.header.corr == 17 {
                return serde_json::from_slice(&reply.body).unwrap();
            }
        }
    })
    .await
    .expect("control reply")
}

async fn gated(typescript: bool) {
    let rig = Rig::start(true, typescript).await;
    // Approving config.stamp activates the configuration gate: configured module
    // IDs then require their daemon-supplied channel. Save the real nonce delivered
    // before approval, so replay refusal checks more than rejecting an invented token.
    let nonce = fs::read_to_string(rig.root.join("caller.nonce")).unwrap();
    rig.stamp().await;
    // Verify the serving launch uses its channel before asking it to open a
    // claim-bound consumer under the stamped gate.
    timeout(Duration::from_secs(30), async {
        loop {
            let provenance = rpc(
                &mut rig.client().await,
                json!({"op":"supervisor.provenance","module_id":"caller"}),
            )
            .await;
            if provenance["modules"][0]["registration"] == "channel" {
                break;
            }
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("supervisor reports channel registration");
    rig.exercise(typescript).await;
    let refused = rpc(
        &mut rig.client().await,
        json!({
            "op":"route.open", "target":{"kind":"tool_provider","module_id":"target"},
            "identity":{"harness":"nonce-replay","session":"session"},
            "consumer_identity":{"module_id":"caller","launch_nonce":nonce},
        }),
    )
    .await;
    assert_eq!(refused["code"], "bad_consumer_identity", "{refused}");
    let hello = json!({"manifest":ModuleManifest::builder("caller", "1.0.0").build(),
        "protocol_ver":subc_protocol::PROTOCOL_VERSION,"launch_nonce":nonce});
    let info = rig.published().info;
    assert_eq!(info.pid, rig.daemon.id());
    let endpoint = &info.endpoints[0];
    assert!(matches!(endpoint.host.as_str(), "127.0.0.1" | "::1"));
    let mut client = tokio::net::TcpStream::connect((endpoint.host.as_str(), endpoint.port))
        .await
        .unwrap();
    subc_transport::authenticate_client(&mut client, &info, Duration::from_secs(3))
        .await
        .unwrap();
    let frame = Frame::build(
        FrameType::Hello,
        Flags::new(false, Priority::Interactive, false),
        0,
        0,
        18,
        serde_json::to_vec(&hello).unwrap(),
    )
    .unwrap();
    write_frame(&mut client, &frame).await.unwrap();
    let reply = timeout(Duration::from_secs(5), read_frame(&mut client))
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(reply.header.ty, FrameType::Error);
    let refused: Value = serde_json::from_slice(&reply.body).unwrap();
    assert_eq!(refused["code"], "reserved_module", "{refused}");
}

#[tokio::test]
async fn child_bound_rust_real_daemon() {
    gated(false).await;
}

/// Build `cargo build --locked -p subc-core -p subc-daemon -p subc-os --bins --features subc-core/test-support,subc-daemon/test-support,subc-daemon/dev-desktop-approval`.
/// Build the bundle-local helper with `bun run --cwd clients/subc-client build:native`.
/// Run `CHILD_BOUND_BUN="$(command -v bun)" cargo test --locked -p subc-core --features test-support,subc-daemon/dev-desktop-approval --test child_bound_registration child_bound_typescript_real_daemon -- --ignored --exact --nocapture`.
#[tokio::test]
#[ignore = "requires bun and the bundle-local registration native helper; see doc comment"]
async fn child_bound_typescript_real_daemon() {
    gated(true).await;
}

#[tokio::test]
async fn child_bound_ungated_nonce_control() {
    let rig = Rig::start(false, false).await;
    rig.exercise(false).await;
    let provenance = rpc(
        &mut rig.client().await,
        json!({"op":"supervisor.provenance","module_id":"caller"}),
    )
    .await;
    assert_eq!(
        provenance["modules"][0]["registration"], "nonce",
        "{provenance}"
    );
}
