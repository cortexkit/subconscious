#![cfg(unix)]

use cortexkit_test_support::{ckdev_binary, dev_command, ScratchDir};
use serde_json::{json, Value};
use std::{
    fs,
    path::PathBuf,
    process::{Child, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use subc_client_rs::{HandlerOutcome, ModuleHandler, RequestCtx, RouteCloseReason};
use subc_protocol::{manifest::ModuleManifest, Flags, Frame, FrameType, Priority};
use subc_transport::{
    authenticate_client,
    frame_io::{read_frame, write_frame},
};
use tokio::{
    net::TcpStream,
    time::{sleep, timeout},
};

struct Daemon {
    child: Child,
    root: ScratchDir,
    connection: PathBuf,
}

impl Daemon {
    async fn start(window_ms: Option<u64>) -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = ScratchDir::new(&format!("shutdown-intent-{stamp}"));
        for dir in ["data", "runtime", "config/cortexkit"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        fs::write(
            root.join("config/cortexkit/subc.jsonc"),
            r#"{"version":1,"modules":{}}"#,
        )
        .unwrap();
        let mut command = dev_command(ckdev_binary(env!("CARGO_BIN_EXE_ck-subc-under-test")));
        command
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_RUNTIME_DIR", root.join("runtime"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("SUBC_PORT", "0")
            .env("SUBC_CGROUP_PLACEMENT", "disabled")
            .env_remove("SUBC_TEST_SHUTDOWN_INTENT_WINDOW_MS")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(ms) = window_ms {
            command.env("SUBC_TEST_SHUTDOWN_INTENT_WINDOW_MS", ms.to_string());
        }
        let child = command.spawn().unwrap();
        let connection = root
            .join("runtime")
            .join(subc_transport::CONNECTION_FILE_NAME);
        let mut daemon = Self {
            child,
            root,
            connection,
        };
        timeout(Duration::from_secs(10), async {
            while !daemon.connection.exists() {
                assert!(
                    daemon.child.try_wait().unwrap().is_none(),
                    "daemon exited during startup"
                );
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("daemon did not publish its connection file");
        daemon
    }

    async fn request(&self, body: Value) -> Frame {
        timeout(Duration::from_secs(3), async {
            let info = subc_transport::read_for_client(&self.connection).unwrap();
            let endpoint = &info.endpoints[0];
            let address = std::net::SocketAddr::new(endpoint.host.parse().unwrap(), endpoint.port);
            let mut stream = TcpStream::connect(address).await.unwrap();
            authenticate_client(&mut stream, &info, Duration::from_secs(2))
                .await
                .unwrap();
            let frame = Frame::build(
                FrameType::Request,
                Flags::new(false, Priority::Interactive, false),
                0,
                0,
                1,
                serde_json::to_vec(&body).unwrap(),
            )
            .unwrap();
            write_frame(&mut stream, &frame).await.unwrap();
            read_frame(&mut stream).await.unwrap().unwrap()
        })
        .await
        .unwrap()
    }

    async fn stop(&mut self) {
        rustix::process::kill_process(
            rustix::process::Pid::from_raw(self.child.id() as i32).unwrap(),
            rustix::process::Signal::TERM,
        )
        .unwrap();
        timeout(Duration::from_secs(5), async {
            loop {
                if let Some(status) = self.child.try_wait().unwrap() {
                    assert!(status.success(), "graceful shutdown failed: {status}");
                    break;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("daemon did not shut down");
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Observer(Arc<Mutex<Vec<(RouteCloseReason, SystemTime)>>>);

#[async_trait::async_trait]
impl ModuleHandler for Observer {
    async fn handle(&self, _ctx: RequestCtx, _body: Vec<u8>) -> HandlerOutcome {
        HandlerOutcome::Response(Vec::new())
    }

    async fn on_draining(&self, reason: RouteCloseReason, deadline: SystemTime) {
        self.0.lock().unwrap().push((reason, deadline));
    }
}

async fn shutdown_notice(announce: bool, window_ms: Option<u64>, expected: RouteCloseReason) {
    let mut daemon = Daemon::start(window_ms).await;
    let events = Arc::new(Mutex::new(Vec::new()));
    let (_handle, serving) = subc_client_rs::serve_with_handle(
        &daemon.connection,
        ModuleManifest::builder("shutdown-observer", "0.1.0").build(),
        Observer(Arc::clone(&events)),
    )
    .await
    .unwrap();
    let serving = tokio::spawn(serving);
    if announce {
        let before = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let reply = daemon
            .request(json!({"op":"daemon.shutdown_intent", "intent":"restart"}))
            .await;
        assert_eq!(reply.header.ty, FrameType::Response);
        let body: Value = serde_json::from_slice(&reply.body).unwrap();
        assert_eq!(body["intent"], "restart");
        let expiry = body["expires_at_ms"].as_u64().unwrap();
        assert!(expiry >= before + window_ms.unwrap_or(120_000));
        assert!(
            expiry
                <= SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64
                    + window_ms.unwrap_or(120_000)
        );
    }
    if let Some(ms) = window_ms {
        sleep(Duration::from_millis(ms + 20)).await;
    }
    let stopped_at = SystemTime::now();
    daemon.stop().await;
    timeout(Duration::from_secs(2), serving)
        .await
        .expect("module connection did not end")
        .unwrap()
        .unwrap();
    let events = events.lock().unwrap();
    assert_eq!(
        events.len(),
        1,
        "on_draining must run exactly once before connection end"
    );
    assert_eq!(events[0].0, expected);
    assert!(
        events[0].1 > stopped_at,
        "notice must carry the shutdown deadline"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn announced_daemon_restart_calls_on_draining_before_connection_end() {
    shutdown_notice(true, None, RouteCloseReason::Restart).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unannounced_daemon_shutdown_calls_on_draining_with_disable() {
    shutdown_notice(false, None, RouteCloseReason::Disable).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_restart_intent_calls_on_draining_with_disable() {
    shutdown_notice(true, Some(20), RouteCloseReason::Disable).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_intent_refuses_unsupported_and_missing_intents() {
    let mut daemon = Daemon::start(None).await;
    for body in [
        json!({"op":"daemon.shutdown_intent", "intent":"stop"}),
        json!({"op":"daemon.shutdown_intent"}),
        json!({"op":"daemon.shutdown_intent", "intent":7}),
    ] {
        let reply = daemon.request(body).await;
        assert_eq!(reply.header.ty, FrameType::Error);
        let body: Value = serde_json::from_slice(&reply.body).unwrap();
        assert_eq!(body["code"], "invalid_params");
        assert!(body["message"].as_str().unwrap().contains("restart"));
    }
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hidden_announce_restart_command_prints_reply() {
    let mut daemon = Daemon::start(None).await;
    let output = dev_command(ckdev_binary(env!("CARGO_BIN_EXE_ck-under-test")))
        .env("XDG_DATA_HOME", daemon.root.join("client-data"))
        .env("XDG_RUNTIME_DIR", daemon.root.join("client-runtime"))
        .env("XDG_CONFIG_HOME", daemon.root.join("client-config"))
        .arg("--subc")
        .arg(&daemon.connection)
        .args(["daemon", "announce-restart"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let reply: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(reply["intent"], "restart");
    assert!(reply["expires_at_ms"].as_u64().is_some());
    daemon.stop().await;
}
