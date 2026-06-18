use std::{
    fs,
    path::PathBuf,
    process,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use serde_json::json;
use subc_core::{
    read_frame, serve_listener, write_frame, AttachAck, AttachRequest, ControlHandler,
    ForwardingTable, Frame, ModuleSpec, Registry, RestartPolicy, Router, Supervisor,
    SUBC_SOCKET_ENV,
};
use subc_protocol::{Flags, FrameType, Priority};
use tokio::{
    io::AsyncWriteExt,
    net::{UnixListener, UnixStream},
    task::JoinHandle,
    time::{sleep, timeout, Instant},
};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
const READ_TIMEOUT: Duration = Duration::from_secs(2);

struct TestServer {
    registry: Arc<Registry>,
    forwarding: Arc<ForwardingTable>,
    socket_path: PathBuf,
    temp_dir: PathBuf,
    task: JoinHandle<Result<(), subc_core::ServerError>>,
}

impl TestServer {
    fn start() -> Self {
        let temp_dir = unique_temp_dir("forwarding-server");
        fs::create_dir_all(&temp_dir).unwrap();
        let socket_path = temp_dir.join("s.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();

        let registry = Arc::new(Registry::default());
        let control = Arc::new(ControlHandler::new(Arc::clone(&registry)));
        let forwarding = control.forwarding();
        let router = Arc::new(Router::with_control_handler(control));
        let task = tokio::spawn(serve_listener(listener, router));

        Self {
            registry,
            forwarding,
            socket_path,
            temp_dir,
            task,
        }
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.task.abort();
        let _ = fs::remove_dir_all(&self.temp_dir);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn attach_then_forward_request_round_trips_through_stub() {
    let server = TestServer::start();
    let supervisor = Supervisor::new(
        Arc::clone(&server.registry),
        RestartPolicy::new(1, Duration::from_millis(10)),
    )
    .with_drain_timeout(Duration::from_millis(25));
    let module_id = "fake-aft-forwarding";
    let module = supervisor.spawn(stub_spec(&server, module_id)).unwrap();
    wait_for_registration(&server.registry, module_id, Duration::from_secs(1)).await;

    let project = TestProject::new();
    let mut client = UnixStream::connect(&server.socket_path).await.unwrap();
    let attach = AttachRequest {
        project_root: project.path.clone(),
        harness: "opencode".to_string(),
        session: "ses-forwarding".to_string(),
        config: json!({ "spike": true }),
    };
    write_frame(&mut client, &attach_frame(101, attach))
        .await
        .unwrap();
    client.flush().await.unwrap();

    let ack_frame = read_frame_timeout(&mut client).await;
    assert_eq!(ack_frame.header.ty, FrameType::Response);
    assert_eq!(ack_frame.header.channel, 0);
    assert_eq!(ack_frame.header.corr, 101);
    let ack: AttachAck = serde_json::from_slice(&ack_frame.body).unwrap();
    assert!(ack.route_channel > 0);
    assert_eq!(server.forwarding.active_binding_count().unwrap(), 1);
    assert!(server
        .forwarding
        .has_route_channel(ack.route_channel)
        .unwrap());

    let payload = br#"{"jsonrpc":"2.0","id":7,"method":"read","params":{"path":"Cargo.toml"}}"#;
    let request = Frame::build(
        FrameType::Request,
        Flags::new(false, Priority::Interactive, false),
        ack.route_channel,
        202,
        payload.to_vec(),
    )
    .unwrap();
    write_frame(&mut client, &request).await.unwrap();
    client.flush().await.unwrap();

    let response = read_frame_timeout(&mut client).await;
    assert_eq!(response.header.ty, FrameType::Response);
    assert_eq!(response.header.channel, ack.route_channel);
    assert_eq!(response.header.corr, 202);
    assert_eq!(response.body, payload);

    module.stop().await.unwrap();
}

fn attach_frame(corr: u64, attach: AttachRequest) -> Frame {
    let body = serde_json::to_vec(&attach).unwrap();
    Frame::build(
        FrameType::Request,
        Flags::new(false, Priority::Interactive, false),
        0,
        corr,
        body,
    )
    .unwrap()
}

async fn read_frame_timeout(stream: &mut UnixStream) -> Frame {
    timeout(READ_TIMEOUT, async {
        read_frame(stream)
            .await
            .unwrap()
            .expect("connection should stay open")
    })
    .await
    .expect("timed out waiting for frame")
}

fn stub_spec(server: &TestServer, module_id: &str) -> ModuleSpec {
    ModuleSpec {
        module_id: module_id.to_string(),
        program: PathBuf::from(env!("CARGO_BIN_EXE_fake-aft-stub")),
        args: Vec::new(),
        env: vec![
            (
                SUBC_SOCKET_ENV.to_string(),
                server.socket_path.to_string_lossy().into_owned(),
            ),
            ("FAKE_AFT_MODULE_ID".to_string(), module_id.to_string()),
        ],
    }
}

async fn wait_for_registration(registry: &Registry, module_id: &str, wait: Duration) {
    let deadline = Instant::now() + wait;
    loop {
        if registry.get_module(module_id).unwrap().is_some() {
            return;
        }
        if Instant::now() >= deadline {
            panic!("module {module_id} did not register within {wait:?}");
        }
        sleep(Duration::from_millis(10)).await;
    }
}

struct TestProject {
    path: PathBuf,
}

impl TestProject {
    fn new() -> Self {
        let path = unique_temp_dir("forwarding-project");
        fs::create_dir_all(&path).unwrap();
        Self { path }
    }
}

impl Drop for TestProject {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn unique_temp_dir(label: &str) -> PathBuf {
    let nonce = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("sc-{label}-{}-{nonce}", process::id()))
}
