//! The health and sentinel rows' harness: the sentinel timing ck-bus logs at start, the
//! supervisor's view of `ckbus`, a stopped (hung) broker, and a supervised module that
//! registers WITHOUT advertising `health.check`, the control for the health carrier.
//!
//! The healthless module is this row's own test executable, declared through the
//! per-run config and started by `supervisor.rescan`, as the issuance participant is.
//! It speaks the subc wire directly because the client library always advertises
//! `health.check`: it sends HELLO with its launch nonce and no control ops, then keeps
//! the connection open, answering nothing.
//!
//! Every row file compiles this whole module and uses only part of it.
#![allow(dead_code)]

use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use serde_json::{json, Value};
use subc_control::{ClientControlRequest, ClientControlResponse};
use subc_protocol::{
    manifest::ModuleManifest, Flags, Frame, FrameType, ModuleHelloBody, Priority, PROTOCOL_VERSION,
    SUBC_LAUNCH_NONCE_ENV, SUBC_MODULE_ID_ENV,
};
use subc_transport::{authenticate_client, connection_file, read_frame, write_frame};
use tokio::io::AsyncWriteExt;

use super::{bus, control, signer::run::SignerRun};

pub const HEALTHLESS: &str = "healthless";
/// The row test each row file declares to run as the healthless child.
pub const CHILD_TEST: &str = "healthless_child";
const CHILD_ARGV_ENV: &str = "CKBUS_HEALTHLESS_ARGV";

/// The harness values for the health and sentinel rows (spec: 1000 ms and 200 ms).
pub const PERIOD_MS: u64 = 1_000;
pub const TIMEOUT_MS: u64 = 200;

/// The spec budgets the health and A8 rows at 120 s together. They have eight arms, so
/// each arm gets an equal share and fails with its measured wall time past it.
pub const ARM_BUDGET: Duration = Duration::from_secs(15);

/// Fails the arm when it ran longer than `ARM_BUDGET` since `started`.
pub fn within_budget(started: Instant, arm: &str) {
    let took = started.elapsed();
    eprintln!("arm {arm:?} wall time {took:?} (budget {ARM_BUDGET:?})");
    assert!(
        took <= ARM_BUDGET,
        "arm {arm:?} exceeded its share of the rows' 120 s budget: {took:?}"
    );
}

/// How many ck-bus processes have logged their start in this run.
pub fn started_count(run_root: &Path) -> usize {
    bus::events(run_root, "ckbus.runtime.started").len()
}

/// `supervisor.health_probe` for any module: the status, or the refusal's code.
pub async fn probe(connection_file: &Path, module_id: &str) -> Result<String, String> {
    match control::rpc(
        connection_file,
        ClientControlRequest::SupervisorHealthProbe {
            module_id: module_id.to_string(),
        },
    )
    .await
    {
        control::ControlReply::Response(ClientControlResponse::SupervisorHealthProbe {
            status,
            ..
        }) => Ok(format!("{status:?}")),
        control::ControlReply::Response(other) => {
            panic!("supervisor.health_probe answered another variant: {other:?}")
        }
        control::ControlReply::Error(error) => Err(error.code),
    }
}

/// The `ckbus` environment that sets the sentinel values.
pub fn sentinel_env() -> Vec<(String, String)> {
    vec![
        (
            "CKBUS_SENTINEL_PERIOD_MS".to_string(),
            PERIOD_MS.to_string(),
        ),
        (
            "CKBUS_SENTINEL_TIMEOUT_MS".to_string(),
            TIMEOUT_MS.to_string(),
        ),
    ]
}

/// The sentinel timing one ck-bus process logged at start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Started {
    pub period: Duration,
    pub timeout: Duration,
    pub incarnation: String,
}

impl Started {
    /// `3 * period + timeout`, computed from the logged values.
    pub fn down_bound(&self) -> Duration {
        self.period * 3 + self.timeout
    }
}

/// Parses the `count`th `ckbus.runtime.started` line, waiting for it. Fails when the
/// line or one of its three fields is absent.
pub async fn started(run_root: &Path, count: usize) -> Started {
    let line = bus::wait_event(
        run_root,
        "ckbus.runtime.started",
        count,
        Duration::from_secs(30),
    )
    .await;
    let ms = |name: &str| {
        Duration::from_millis(
            line[name]
                .as_u64()
                .unwrap_or_else(|| panic!("the start line carries {name}: {line}")),
        )
    };
    Started {
        period: ms("sentinel_period_ms"),
        timeout: ms("sentinel_timeout_ms"),
        incarnation: line["process_incarnation"]
            .as_str()
            .unwrap_or_else(|| panic!("the start line carries its incarnation: {line}"))
            .to_string(),
    }
}

/// `supervisor.health_probe` for `ckbus`, waiting until `accept` takes the answer.
pub async fn wait_health(
    connection_file: &Path,
    limit: Duration,
    what: &str,
    accept: impl Fn(&str, Option<&str>, &Value) -> bool,
) -> (String, Option<String>, Value, Instant) {
    let deadline = Instant::now() + limit;
    loop {
        let last = bus::try_health(connection_file).await;
        let at = Instant::now();
        if let Ok((status, detail, metrics)) = &last {
            if accept(status, detail.as_deref(), metrics) {
                return (status.clone(), detail.clone(), metrics.clone(), at);
            }
        }
        assert!(
            Instant::now() < deadline,
            "health never showed {what} within {limit:?}; last probe {last:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Whether a probe answer is up.
pub fn is_up(status: &str, detail: Option<&str>, _metrics: &Value) -> bool {
    status == "Ok" && detail == Some("bus.health.up")
}

/// `ckbus`'s `supervisor.list` entry: its state, whether it is registered, and its
/// restart counters.
pub async fn ckbus_list_entry(connection_file: &Path) -> Value {
    let response =
        control::response(connection_file, ClientControlRequest::SupervisorList {}).await;
    let ClientControlResponse::SupervisorList { modules, .. } = response else {
        panic!("supervisor.list must return its matching response variant");
    };
    let entry = modules
        .iter()
        .find(|module| module.module_id == "ckbus")
        .expect("supervisor.list lists ckbus");
    serde_json::to_value(entry).expect("a list entry encodes")
}

/// `module_id`'s status in the cached `supervisor.health` snapshot.
pub async fn cached_health_status(connection_file: &Path, module_id: &str) -> String {
    let response =
        control::response(connection_file, ClientControlRequest::SupervisorHealth {}).await;
    let ClientControlResponse::SupervisorHealth { modules, .. } = response else {
        panic!("supervisor.health must return its matching response variant");
    };
    let entry = modules
        .iter()
        .find(|module| module.module_id == module_id)
        .unwrap_or_else(|| panic!("supervisor.health lists {module_id}"));
    format!("{:?}", entry.status)
}

/// The pid of the `nats-server` running `server_dir/server.conf`, found by its command
/// line (the harness's server handle keeps its child private).
pub fn nats_server_pid(server_dir: &Path) -> u32 {
    let conf = server_dir.join("server.conf");
    let output = std::process::Command::new("pgrep")
        .arg("-f")
        .arg(conf.display().to_string())
        .output()
        .expect("pgrep runs");
    let text = String::from_utf8_lossy(&output.stdout);
    let pids: Vec<u32> = text
        .lines()
        .filter_map(|line| line.trim().parse().ok())
        .collect();
    assert_eq!(
        pids.len(),
        1,
        "exactly one nats-server runs {}: {text:?}",
        conf.display()
    );
    pids[0]
}

/// Sends `signal` (`STOP` or `CONT`) to `pid`.
pub fn signal(pid: u32, signal: &str) {
    let status = std::process::Command::new("kill")
        .arg(format!("-{signal}"))
        .arg(pid.to_string())
        .status()
        .expect("kill runs");
    assert!(status.success(), "kill -{signal} {pid} failed");
}

/// Declares the healthless module in the run's config and has the daemon start it.
pub async fn register_healthless(run: &SignerRun) {
    let exe = std::env::current_exe().expect("the row's own executable");
    let mut value: Value =
        serde_json::from_slice(&fs::read(&run.config_file).expect("rendered config"))
            .expect("rendered config is JSON");
    value["modules"][HEALTHLESS] = json!({
        "program": "/bin/sh",
        "args": [
            "-c",
            format!("{CHILD_ARGV_ENV}=\"$*\" exec \"$0\" --exact {CHILD_TEST} --nocapture --test-threads=1"),
            exe.display().to_string(),
        ],
        "enabled": true,
        "protocol": "subc",
        "drain_timeout_ms": 2000,
    });
    fs::write(
        &run.config_file,
        serde_json::to_vec_pretty(&value).expect("config encodes"),
    )
    .expect("rendered config writable");
    let reply = control::rpc(
        &run.connection_file,
        ClientControlRequest::SupervisorRescan { preview: false },
    )
    .await;
    if let control::ControlReply::Error(error) = reply {
        panic!(
            "supervisor.rescan refused: {} {}",
            error.code, error.message
        );
    }
    run.wait_for_catalog_id(HEALTHLESS).await;
}

/// The healthless child: runs only when the daemon started this executable as it.
pub fn healthless_child_entry() {
    let Ok(argv) = std::env::var(CHILD_ARGV_ENV) else {
        return;
    };
    let mut words = argv.split_whitespace();
    let mut connection = None;
    while let Some(word) = words.next() {
        if word == "--subc" {
            connection = words.next().map(PathBuf::from);
        } else if let Some(path) = word.strip_prefix("--subc=") {
            connection = Some(PathBuf::from(path));
        }
    }
    let connection = connection.expect("the daemon passes --subc to the module");
    let module_id = std::env::var(SUBC_MODULE_ID_ENV).expect("the daemon names the module");
    let launch_nonce = std::env::var(SUBC_LAUNCH_NONCE_ENV).ok();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("healthless runtime");
    runtime.block_on(async move {
        let file = connection_file::read(&connection).expect("connection file decodes");
        let endpoint = file.endpoints.first().expect("an endpoint");
        let mut stream = tokio::net::TcpStream::connect((endpoint.host.as_str(), endpoint.port))
            .await
            .expect("the daemon accepts the module");
        authenticate_client(&mut stream, &file, Duration::from_secs(2))
            .await
            .expect("the module authenticates");
        let body = serde_json::to_vec(&ModuleHelloBody {
            manifest: ModuleManifest::builder(&module_id, "0.0.0-healthless").build(),
            protocol_ver: PROTOCOL_VERSION,
            // The control: no `health.check` among the advertised control ops.
            control_ops: None,
            launch_nonce,
        })
        .expect("HELLO encodes");
        let hello = Frame::build(
            FrameType::Hello,
            Flags::new(false, Priority::Passive, false),
            0,
            0,
            1,
            body,
        )
        .expect("HELLO frame builds");
        write_frame(&mut stream, &hello)
            .await
            .expect("HELLO writes");
        stream.flush().await.expect("HELLO flushes");
        let ack = read_frame(&mut stream)
            .await
            .expect("HELLO_ACK decodes")
            .expect("the daemon answers HELLO");
        assert_eq!(ack.header.ty, FrameType::HelloAck, "HELLO accepted");
        // Registered; keep the connection until the daemon closes it.
        while let Ok(Some(_frame)) = read_frame(&mut stream).await {}
    });
    std::process::exit(0);
}
