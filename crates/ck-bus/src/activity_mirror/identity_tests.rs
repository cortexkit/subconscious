//! Launch a ckbus reader through the daemon, then inspect its forwarded activity
//! requests for the module id and launch proof the daemon supplied. Identity-free
//! local reads (Direct) are admitted today too, so success alone cannot prove that
//! the mirror read as the supervised principal reserved:ckbus.

use super::tests::{module_frame, until};
use cortexkit_test_support::ScratchDir;
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};
use subc_protocol::{Frame, FrameType};
use subc_transport::{
    authenticate_client, authenticate_server, read_frame, write_frame, ConnectionInfo,
};
use tokio::net::{TcpListener, TcpStream};

const DROP_ACTIVITY_IDENTITY: bool = false;

#[derive(Debug)]
struct ReadIdentity {
    op: String,
    module_id: Option<String>,
    has_nonce: bool,
}

fn forwarded(frame: Frame, observed: &Mutex<Vec<ReadIdentity>>) -> Frame {
    if frame.header.ty != FrameType::Request {
        return frame;
    }
    let Ok(mut body) = serde_json::from_slice::<Value>(&frame.body) else {
        return frame;
    };
    let op = body["op"].as_str().unwrap_or_default().to_string();
    if !matches!(op.as_str(), "activity.query" | "activity.subscribe") {
        return frame;
    }
    if DROP_ACTIVITY_IDENTITY {
        body.as_object_mut().unwrap().remove("consumer_identity");
    }
    // Capture the forwarded body, not the original, so an identity stripped in
    // transit cannot pass the test just because the SDK initially supplied it.
    observed.lock().unwrap().push(ReadIdentity {
        op,
        module_id: body["consumer_identity"]["module_id"]
            .as_str()
            .map(str::to_owned),
        has_nonce: body["consumer_identity"]["launch_nonce"]
            .as_str()
            .is_some_and(|nonce| !nonce.is_empty()),
    });
    Frame::build(
        frame.header.ty,
        frame.header.flags,
        frame.header.channel,
        frame.header.epoch,
        frame.header.corr,
        serde_json::to_vec(&body).unwrap(),
    )
    .unwrap()
}

async fn proxy(
    listener: TcpListener,
    info: ConnectionInfo,
    observed: Arc<Mutex<Vec<ReadIdentity>>>,
) {
    let (mut reader, _) = listener.accept().await.unwrap();
    authenticate_server(
        &mut reader,
        &info.key,
        &info.daemon_id,
        &info.daemon_ver,
        Duration::from_secs(5),
    )
    .await
    .unwrap();
    let endpoint = &info.endpoints[0];
    let mut daemon = TcpStream::connect((endpoint.host.as_str(), endpoint.port))
        .await
        .unwrap();
    authenticate_client(&mut daemon, &info, Duration::from_secs(5))
        .await
        .unwrap();
    let (mut client_read, mut client_write) = reader.split();
    let (mut daemon_read, mut daemon_write) = daemon.split();
    let to_daemon = async {
        while let Some(frame) = next_or_closed(read_frame(&mut client_read).await) {
            if !written_or_closed(
                write_frame(&mut daemon_write, &forwarded(frame, &observed)).await,
            ) {
                break;
            }
        }
    };
    let to_client = async {
        while let Some(frame) = next_or_closed(read_frame(&mut daemon_read).await) {
            if !written_or_closed(write_frame(&mut client_write, &frame).await) {
                break;
            }
        }
    };
    tokio::select! { _ = to_daemon => {}, _ = to_client => {} }
}

/// The supervised reader exits once it has written its result, and on Linux
/// a socket closed with unread bytes is reset rather than shut down cleanly.
/// For this relay a reset or broken pipe ends the connection like EOF; any
/// other frame error is still a test failure.
fn closed_by_peer(error: &subc_transport::FrameIoError) -> bool {
    matches!(
        error,
        subc_transport::FrameIoError::Io(io)
            if matches!(
                io.kind(),
                std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::BrokenPipe
            )
    )
}

fn next_or_closed(read: Result<Option<Frame>, subc_transport::FrameIoError>) -> Option<Frame> {
    match read {
        Ok(frame) => frame,
        Err(error) if closed_by_peer(&error) => None,
        Err(error) => panic!("relay read failed: {error:?}"),
    }
}

fn written_or_closed(write: Result<(), subc_transport::FrameIoError>) -> bool {
    match write {
        Ok(()) => true,
        Err(error) if closed_by_peer(&error) => false,
        Err(error) => panic!("relay write failed: {error:?}"),
    }
}

fn artifact(args: &[&str], name: &str) -> PathBuf {
    let output = std::process::Command::new(env!("CARGO"))
        .args(args)
        .args([
            "--locked",
            "--offline",
            "--jobs",
            "2",
            "--message-format=json",
        ])
        .current_dir(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."))
        .output()
        .expect("build supervised reader fixture");
    assert!(
        output.status.success(),
        "fixture build failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find_map(|row| {
            (row["reason"] == "compiler-artifact" && row["target"]["name"] == name)
                .then(|| row["executable"].as_str().map(PathBuf::from))
                .flatten()
        })
        .expect("Cargo must name the requested fixture executable")
}
fn reader_fixture() -> &'static std::path::Path {
    static EXE: OnceLock<PathBuf> = OnceLock::new();
    EXE.get_or_init(|| {
        cortexkit_test_support::ckdev_binary(artifact(
            &["build", "-p", "ck-bus", "--example", "activity-reader"],
            "activity-reader",
        ))
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supervised_activity_mirror_reads_as_reserved_ckbus() {
    let fixture = reader_fixture();
    let root = ScratchDir::new("ckbus-activity-attestation");
    let connection = root.join("connection.json");
    let proxy_connection = root.join("proxy.json");
    let result = root.join("result.json");
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_port = listener.local_addr().unwrap().port();
    let config_path = root.join("subc.jsonc");
    std::fs::write(&config_path, serde_json::to_vec(&json!({
        "version": 1, "modules": { "ckbus": {
            "program": fixture, "args": [], "enabled": true, "reserved": true,
            "protocol": "subc", "restart": {"max_restarts": 0},
            "env": {
                "XDG_DATA_HOME": root.join("data"), "XDG_RUNTIME_DIR": root.join("run"), "XDG_CONFIG_HOME": root.join("config"),
                "CKBUS_ACTIVITY_CONNECTION": proxy_connection, "CKBUS_ACTIVITY_RESULT": result,
            }
        }}
    })).unwrap()).unwrap();
    let config = subc_daemon::bootstrap::BootstrapConfig::new(&connection, 0)
        .with_terminal_journal_path(root.join("terminals.jsonl"))
        .with_capture_logs_dir(root.join("logs"))
        .with_daemon_config_path(&config_path)
        .unwrap();
    #[cfg(target_os = "macos")]
    let config = config.with_privacy_trampoline(cortexkit_test_support::ckdev_binary(artifact(
        &["build", "-p", "subc-core", "--bin", "ck-subc"],
        "ck-subc",
    )));
    let daemon = tokio::spawn(subc_daemon::bootstrap::run_with_config(config));
    until(|| connection.exists()).await;
    let info = subc_transport::read(&connection).unwrap();
    let mut proxy_info = info.clone();
    proxy_info.endpoints = vec![subc_transport::Endpoint {
        host: "127.0.0.1".into(),
        port: proxy_port,
    }];
    let observed = Arc::new(Mutex::new(vec![]));
    let transport = tokio::spawn(proxy(listener, info.clone(), observed.clone()));
    subc_transport::write_atomic(&proxy_connection, &proxy_info).unwrap();
    until(|| {
        observed
            .lock()
            .unwrap()
            .iter()
            .any(|read| read.op == "activity.subscribe")
    })
    .await;
    {
        let reads = observed.lock().unwrap();
        for op in ["activity.query", "activity.subscribe"] {
            let read = reads
                .iter()
                .find(|read| read.op == op)
                .expect("mirror must issue both activity reads");
            assert_eq!(
                read.module_id.as_deref(),
                Some("ckbus"),
                "{op} must arrive with ck-bus's identity, not Direct"
            );
            assert!(read.has_nonce, "{op} must carry the daemon-issued nonce");
        }
    }
    let endpoint = &info.endpoints[0];
    let mut worker = TcpStream::connect((endpoint.host.as_str(), endpoint.port))
        .await
        .unwrap();
    authenticate_client(&mut worker, &info, Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(module_frame(&mut worker, FrameType::Hello, 1, json!({
        "manifest": subc_protocol::manifest::ModuleManifest::builder("worker", "0.1.0").build(),
        "protocol_ver": subc_protocol::PROTOCOL_VERSION
    })).await.header.ty, FrameType::HelloAck);
    let recorded = module_frame(&mut worker, FrameType::Request, 2, json!({"op":"activity.record", "kind":"worker.settled", "summary":"attested reader received this"})).await;
    assert_eq!(recorded.header.ty, FrameType::Response);
    until(|| result.exists()).await;
    let received: Value = serde_json::from_slice(&std::fs::read(&result).unwrap()).unwrap();
    assert_eq!(
        received["seq"],
        serde_json::from_slice::<Value>(&recorded.body).unwrap()["seq"]
    );
    assert_eq!(received["summary"], "attested reader received this");
    // The daemon accepts the identity it minted for this supervised reader, but
    // the same module id with an invented nonce must not claim that principal.
    let mut unverified = TcpStream::connect((endpoint.host.as_str(), endpoint.port))
        .await
        .unwrap();
    authenticate_client(&mut unverified, &info, Duration::from_secs(5))
        .await
        .unwrap();
    let refused = module_frame(&mut unverified, FrameType::Request, 1, json!({
        "op":"activity.query", "limit":1, "consumer_identity":{"module_id":"ckbus", "launch_nonce":"wrong-nonce"}
    })).await;
    assert_eq!(refused.header.ty, FrameType::Error);
    assert_eq!(
        serde_json::from_slice::<Value>(&refused.body).unwrap()["code"],
        "activity_read_forbidden"
    );
    transport.await.unwrap();
    daemon.abort();
    assert!(daemon.await.unwrap_err().is_cancelled());
}
