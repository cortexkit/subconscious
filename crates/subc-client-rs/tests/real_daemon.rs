#![forbid(unsafe_code)]

use std::{
    collections::BTreeMap,
    fs, io,
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use serde_json::{json, Value};
use subc_protocol::{BindIdentity, ErrorBody, Flags, Frame, FrameType, Priority, RouteTarget};
use subc_transport::{authenticate_client, read_frame, write_frame};
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
    time::{sleep, timeout, Instant},
};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

const MODULE_ID: &str = "subc-client-rs-echo";
const READ_TIMEOUT: Duration = Duration::from_secs(3);
const START_TIMEOUT: Duration = Duration::from_secs(10);
const EVENT_TIMEOUT: Duration = Duration::from_secs(5);
const AUTH_DEADLINE: Duration = Duration::from_secs(2);

struct LiveDaemon {
    child: Child,
    runtime_dir: PathBuf,
    config_dir: PathBuf,
    connection_file: PathBuf,
}

impl Drop for LiveDaemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.runtime_dir);
        let _ = fs::remove_dir_all(&self.config_dir);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clean_subc_client_rs_serves_through_real_daemon() {
    let workspace = workspace_root();
    let daemon_bin = ensure_binary(
        &workspace,
        binary_path(&workspace, "subc-core"),
        &["build", "-p", "subc-core", "--bins"],
    );
    let module_bin = ensure_binary(
        &workspace,
        example_path(&workspace, "echo-module"),
        &["build", "-p", "subc-client-rs", "--example", "echo-module"],
    );

    let temp_dir = unique_temp_dir("subc-client-rs-real-daemon");
    let runtime_dir = temp_dir.join("runtime");
    let config_dir = temp_dir.join("config");
    let events_path = temp_dir.join("events.jsonl");
    fs::create_dir_all(&runtime_dir).unwrap();
    fs::create_dir_all(config_dir.join("cortexkit")).unwrap();
    fs::write(
        config_dir.join("cortexkit").join("subc.jsonc"),
        config_doc(&module_bin, &events_path),
    )
    .unwrap();

    let mut daemon = spawn_daemon(&daemon_bin, &runtime_dir, &config_dir);
    wait_for_connection_file(&daemon.connection_file, START_TIMEOUT).await;
    wait_for_catalog_module(&daemon.connection_file, MODULE_ID, START_TIMEOUT).await;

    let mut client = connect_authed_client(&daemon.connection_file)
        .await
        .unwrap();
    let route_channel = open_route(&mut client, MODULE_ID, 100).await;
    wait_for_event(&events_path, EVENT_TIMEOUT, |event| {
        event["kind"] == "bind" && event["route_channel"].as_u64() == Some(u64::from(route_channel))
    })
    .await;

    write_frame(
        &mut client,
        &data_request(route_channel, 101, br#"{"kind":"unary","value":42}"#),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();
    let unary = read_frame_timeout(&mut client).await;
    assert_eq!(unary.header.ty, FrameType::Response);
    assert_eq!(unary.header.channel, route_channel);
    assert_eq!(unary.header.corr, 101);
    let unary_body: Value = serde_json::from_slice(&unary.body).unwrap();
    assert_eq!(unary_body["ok"], true);
    assert_eq!(unary_body["echo"]["value"], 42);

    write_frame(
        &mut client,
        &data_request(route_channel, 102, br#"{"kind":"error"}"#),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();
    let error = read_frame_timeout(&mut client).await;
    assert_eq!(error.header.ty, FrameType::Error);
    assert_eq!(error.header.channel, route_channel);
    assert_eq!(error.header.corr, 102);
    let error_body: ErrorBody = serde_json::from_slice(&error.body).unwrap();
    assert_eq!(error_body.code, "example_error");

    write_frame(
        &mut client,
        &data_request(route_channel, 103, br#"{"kind":"stream"}"#),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();
    let stream_data = read_frame_timeout(&mut client).await;
    assert_eq!(stream_data.header.ty, FrameType::StreamData);
    assert_eq!(stream_data.header.channel, route_channel);
    assert_eq!(stream_data.header.corr, 103);
    assert_eq!(stream_data.body, b"stream-event");
    let stream_end = read_frame_timeout(&mut client).await;
    assert_eq!(stream_end.header.ty, FrameType::StreamEnd);
    assert_eq!(stream_end.header.channel, route_channel);
    assert_eq!(stream_end.header.corr, 103);

    write_frame(
        &mut client,
        &data_request(route_channel, 104, br#"{"kind":"cancel"}"#),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();
    wait_for_event(&events_path, EVENT_TIMEOUT, |event| {
        event["kind"] == "cancel_waiting" && event["corr"].as_u64() == Some(104)
    })
    .await;
    write_frame(&mut client, &cancel_frame(route_channel, 104))
        .await
        .unwrap();
    client.flush().await.unwrap();
    wait_for_event(&events_path, EVENT_TIMEOUT, |event| {
        event["kind"] == "cancelled" && event["corr"].as_u64() == Some(104)
    })
    .await;
    let cancelled = read_frame_timeout(&mut client).await;
    assert_eq!(cancelled.header.ty, FrameType::Error);
    let cancelled_body: ErrorBody = serde_json::from_slice(&cancelled.body).unwrap();
    assert_eq!(cancelled_body.code, "cancelled");

    write_frame(&mut client, &goodbye_frame(route_channel, 105))
        .await
        .unwrap();
    client.flush().await.unwrap();
    wait_for_event(&events_path, EVENT_TIMEOUT, |event| {
        event["kind"] == "route_gone"
            && event["route_channel"].as_u64() == Some(u64::from(route_channel))
    })
    .await;

    let _ = daemon.child.kill();
}

fn spawn_daemon(daemon_bin: &Path, runtime_dir: &Path, config_dir: &Path) -> LiveDaemon {
    let child = Command::new(daemon_bin)
        .env("XDG_RUNTIME_DIR", runtime_dir)
        .env("XDG_CONFIG_HOME", config_dir)
        .env("SUBC_PORT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("failed to spawn {}: {error}", daemon_bin.display()));
    LiveDaemon {
        child,
        runtime_dir: runtime_dir.to_path_buf(),
        config_dir: config_dir.to_path_buf(),
        connection_file: runtime_dir.join("subc-connection.json"),
    }
}

fn config_doc(module_bin: &Path, events_path: &Path) -> String {
    let env = BTreeMap::from([(
        "SUBC_MODULE_ECHO_EVENTS".to_string(),
        events_path.to_string_lossy().into_owned(),
    )]);
    serde_json::to_string_pretty(&json!({
        "version": 1,
        "modules": {
            MODULE_ID: {
                "program": module_bin.to_string_lossy(),
                "args": [],
                "env": env,
                "enabled": true,
            }
        }
    }))
    .unwrap()
}

async fn wait_for_connection_file(path: &Path, wait: Duration) {
    let deadline = Instant::now() + wait;
    loop {
        if path.exists() {
            return;
        }
        if Instant::now() >= deadline {
            panic!(
                "daemon did not write connection file {} within {wait:?}",
                path.display()
            );
        }
        sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_for_catalog_module(path: &Path, module_id: &str, wait: Duration) {
    let deadline = Instant::now() + wait;
    let mut corr = 1_000;
    loop {
        if catalog_modules(path, Some(module_id), corr).await.len() == 1 {
            return;
        }
        if Instant::now() >= deadline {
            panic!("module {module_id} did not register in catalog within {wait:?}");
        }
        corr += 1;
        sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_for_event(path: &Path, wait: Duration, predicate: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + wait;
    loop {
        for event in read_events(path) {
            if predicate(&event) {
                return event;
            }
        }
        if Instant::now() >= deadline {
            panic!(
                "event did not appear within {wait:?}; events: {:?}",
                read_events(path)
            );
        }
        sleep(Duration::from_millis(20)).await;
    }
}

fn read_events(path: &Path) -> Vec<Value> {
    let Ok(raw) = fs::read_to_string(path) else {
        return Vec::new();
    };
    raw.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect()
}

async fn catalog_modules(path: &Path, module_id: Option<&str>, corr: u64) -> Vec<Value> {
    let mut client = connect_authed_client(path).await.unwrap();
    let response = control_rpc_on_stream(
        &mut client,
        corr,
        json!({
            "op": "catalog.list",
            "module_id": module_id,
        }),
    )
    .await;
    assert_eq!(response["op"], "catalog.list");
    response["modules"].as_array().cloned().unwrap_or_default()
}

async fn open_route<S>(stream: &mut S, module_id: &str, corr: u64) -> u16
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let project_root = unique_temp_dir("subc-client-rs-project");
    fs::create_dir_all(&project_root).unwrap();
    let response = control_rpc_on_stream(
        stream,
        corr,
        json!({
            "op": "route.open",
            "target": RouteTarget::ToolProvider {
                module_id: module_id.to_string(),
            },
            "identity": BindIdentity {
                project_root,
                harness: "subc-client-rs-test".to_string(),
                session: "clean-api".to_string(),
            },
        }),
    )
    .await;
    assert_eq!(response["op"], "route.open");
    response["route_channel"]
        .as_u64()
        .expect("route.open must return route_channel") as u16
}

async fn control_rpc_on_stream<S>(stream: &mut S, corr: u64, request: Value) -> Value
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let body = serde_json::to_vec(&request).unwrap();
    write_frame(stream, &control_request_frame(corr, body))
        .await
        .unwrap();
    stream.flush().await.unwrap();
    let response = read_frame_timeout(stream).await;
    assert_eq!(response.header.ty, FrameType::Response);
    assert_eq!(response.header.channel, 0);
    assert_eq!(response.header.corr, corr);
    serde_json::from_slice(&response.body).unwrap()
}

async fn connect_authed_client(path: &Path) -> io::Result<TcpStream> {
    let conn = subc_transport::read(path).map_err(io::Error::other)?;
    let endpoint = conn.endpoints.first().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "connection file has no endpoint",
        )
    })?;
    let ip: IpAddr = endpoint
        .host
        .parse()
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    let mut stream = TcpStream::connect(SocketAddr::new(ip, endpoint.port)).await?;
    authenticate_client(&mut stream, &conn, AUTH_DEADLINE)
        .await
        .map_err(io::Error::other)?;
    Ok(stream)
}

async fn read_frame_timeout<S>(stream: &mut S) -> Frame
where
    S: AsyncRead + Unpin,
{
    timeout(READ_TIMEOUT, read_frame(stream))
        .await
        .expect("timed out reading frame")
        .expect("frame read failed")
        .expect("connection closed")
}

fn control_request_frame(corr: u64, body: Vec<u8>) -> Frame {
    Frame::build(
        FrameType::Request,
        Flags::new(false, Priority::Interactive, false),
        0,
        corr,
        body,
    )
    .unwrap()
}

fn data_request(channel: u16, corr: u64, body: &[u8]) -> Frame {
    Frame::build(
        FrameType::Request,
        Flags::new(false, Priority::Interactive, false),
        channel,
        corr,
        body.to_vec(),
    )
    .unwrap()
}

fn cancel_frame(channel: u16, corr: u64) -> Frame {
    Frame::build(
        FrameType::Cancel,
        Flags::new(false, Priority::Interactive, false),
        channel,
        corr,
        Vec::new(),
    )
    .unwrap()
}

fn goodbye_frame(channel: u16, corr: u64) -> Frame {
    Frame::build(
        FrameType::Goodbye,
        Flags::new(false, Priority::Interactive, false),
        channel,
        corr,
        Vec::new(),
    )
    .unwrap()
}

fn ensure_binary(workspace: &Path, path: PathBuf, cargo_args: &[&str]) -> PathBuf {
    if path.exists() {
        return path;
    }
    let output = Command::new("cargo")
        .args(cargo_args)
        .current_dir(workspace)
        .output()
        .unwrap_or_else(|error| panic!("failed to run cargo {cargo_args:?}: {error}"));
    if !output.status.success() {
        panic!(
            "cargo {cargo_args:?} failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert!(path.exists(), "expected binary at {}", path.display());
    path
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .unwrap()
        .to_path_buf()
}

fn binary_path(workspace: &Path, name: &str) -> PathBuf {
    workspace
        .join("target")
        .join("debug")
        .join(format!("{name}{}", std::env::consts::EXE_SUFFIX))
}

fn example_path(workspace: &Path, name: &str) -> PathBuf {
    workspace
        .join("target")
        .join("debug")
        .join("examples")
        .join(format!("{name}{}", std::env::consts::EXE_SUFFIX))
}

fn unique_temp_dir(name: &str) -> PathBuf {
    let nonce = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("{name}-{}-{nonce}", std::process::id()))
}
