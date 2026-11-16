#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    process::Stdio,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use serde_json::{json, Value};
use subc_protocol::{
    Flags, Frame, FrameType, ModuleHelloAckBody, ModuleHelloBody, Priority, PROTOCOL_VERSION,
};
use subc_transport::{
    authenticate_server,
    connection_file::{
        generate_daemon_id, generate_key, write_atomic, ConnectionInfo, Endpoint, SCHEMA_VERSION,
    },
    read_frame, write_frame,
};
use tokio::{
    io::AsyncWriteExt,
    net::{TcpListener, TcpStream},
    process::{Child, Command},
    sync::mpsc,
    task::JoinHandle,
    time::{sleep, timeout, Instant},
};

const LIMIT: Duration = Duration::from_secs(12);

enum BusCommand {
    Enable,
    State(u64),
    Remove,
}

struct Fixture {
    root: tempfile::TempDir,
    child: Child,
    primary: TcpStream,
    commands: mpsc::Sender<BusCommand>,
    calls: mpsc::Receiver<Value>,
    bus: JoinHandle<()>,
    opens: Arc<AtomicUsize>,
}

async fn send(stream: &mut TcpStream, ty: FrameType, channel: u16, corr: u64, body: Value) {
    let body = if ty == FrameType::Goodbye {
        vec![]
    } else {
        serde_json::to_vec(&body).unwrap()
    };
    let frame = Frame::build(
        ty,
        Flags::new(false, Priority::Interactive, false),
        channel,
        if channel == 0 { 0 } else { 1 },
        corr,
        body,
    )
    .unwrap();
    write_frame(stream, &frame).await.unwrap();
    stream.flush().await.unwrap();
}

async fn authenticate(stream: &mut TcpStream, info: &ConnectionInfo) {
    authenticate_server(
        stream,
        &info.key,
        &info.daemon_id,
        &info.daemon_ver,
        Duration::from_secs(2),
    )
    .await
    .unwrap();
}

fn remotes() -> Value {
    json!([{"name":"hub","account":"ACCOUNT","urls":["tls://hub.invalid:7422"],"deny_imports":["private.>"],"deny_exports":[],"pin":{"kind":"spki","sha256_hex":"00".repeat(32)}}])
}

async fn push_state(stream: &mut TcpStream, corr: u64, generation: u64) {
    send(
        stream,
        FrameType::StreamData,
        41,
        corr,
        json!({"result":{"generation":generation,"remotes":remotes()}}),
    )
    .await;
}

impl Fixture {
    async fn start() -> Self {
        let root = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let info = ConnectionInfo {
            schema: SCHEMA_VERSION,
            wire_version: None,
            endpoints: vec![Endpoint {
                host: "127.0.0.1".into(),
                port: listener.local_addr().unwrap().port(),
            }],
            key: generate_key().unwrap(),
            daemon_id: generate_daemon_id().unwrap(),
            pid: std::process::id(),
            daemon_ver: "0.41.0".into(),
        };
        let connection = root.path().join("subc.json");
        write_atomic(&connection, &info).unwrap();
        let script = root.path().join("engine");
        fs::write(&script, include_str!("fixtures/engine.py")).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
        let config = root.path().join("server.conf");
        fs::write(
            &config,
            json!({"events":root.path().join("engine.jsonl")}).to_string(),
        )
        .unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_ck-nats-server"))
            .args(["-c"])
            .arg(config)
            .arg("--engine")
            .arg(script)
            .arg("--subc")
            .arg(connection)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("SUBC_MODULE_ID", "nats-server")
            .env("XDG_DATA_HOME", root.path().join("data"))
            .env("XDG_RUNTIME_DIR", root.path().join("runtime"))
            .env("XDG_CONFIG_HOME", root.path().join("config"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let (mut primary, _) = timeout(LIMIT, listener.accept()).await.unwrap().unwrap();
        authenticate(&mut primary, &info).await;
        let hello = read_frame(&mut primary).await.unwrap().unwrap();
        assert_eq!(hello.header.ty, FrameType::Hello);
        let body: ModuleHelloBody = serde_json::from_slice(&hello.body).unwrap();
        assert_eq!(body.manifest.module_id, "nats-server");
        assert_eq!(body.manifest.registration_channel, Some(true));
        assert!(body.manifest.provides.is_empty());
        let ack = ModuleHelloAckBody {
            negotiated_ver: PROTOCOL_VERSION,
            subc_ops: vec![],
            subc_capabilities: vec![],
            storage: None,
            machine_id: None,
        };
        send(
            &mut primary,
            FrameType::HelloAck,
            0,
            hello.header.corr,
            serde_json::to_value(ack).unwrap(),
        )
        .await;
        let (commands, receive) = mpsc::channel(16);
        let (call_tx, calls) = mpsc::channel(16);
        let opens = Arc::new(AtomicUsize::new(0));
        let bus_opens = opens.clone();
        let bus = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            authenticate(&mut stream, &info).await;
            fake_bus(stream, receive, call_tx, bus_opens).await;
        });
        Self {
            root,
            child,
            primary,
            commands,
            calls,
            bus,
            opens,
        }
    }

    async fn event(&self, kind: &str, id: u64) -> Value {
        timeout(LIMIT, async {
            loop {
                let events =
                    fs::read_to_string(self.root.path().join("engine.jsonl")).unwrap_or_default();
                for line in events.lines() {
                    if let Ok(value) = serde_json::from_str::<Value>(line) {
                        if value["type"] == kind && value["id"] == id {
                            return value;
                        }
                    }
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("engine did not receive {kind} for {id}"))
    }

    async fn enable(&mut self) {
        self.commands.send(BusCommand::Enable).await.unwrap();
        let call = timeout(LIMIT, self.calls.recv()).await.unwrap().unwrap();
        assert_eq!(
            call,
            json!({"method":"ckbus.leaf_sign","params":{"remote":"hub","nonce_b64":"abcdefghijklmn0"}})
        );
        assert_eq!(
            self.event("sign_ok", 3).await,
            json!({"v":1,"type":"sign_ok","id":3,"jwt":"public.jwt","signature_b64":"A".repeat(86)})
        );
    }

    async fn finish(mut self) {
        send(&mut self.primary, FrameType::Goodbye, 0, 0, Value::Null).await;
        assert!(timeout(LIMIT, self.child.wait())
            .await
            .unwrap()
            .unwrap()
            .success());
        timeout(LIMIT, &mut self.bus).await.unwrap().unwrap();
        let events = fs::read_to_string(self.root.path().join("engine.jsonl")).unwrap();
        assert!(events
            .lines()
            .any(|line| serde_json::from_str::<Value>(line).unwrap()["type"] == "shutdown"));
    }
}

async fn fake_bus(
    mut stream: TcpStream,
    mut commands: mpsc::Receiver<BusCommand>,
    calls: mpsc::Sender<Value>,
    opens: Arc<AtomicUsize>,
) {
    let mut available = false;
    let mut catalog_corr = None;
    let mut leaf_corr = None;
    loop {
        tokio::select! {
            command = commands.recv() => match command {
                Some(BusCommand::Enable) => {
                    available = true;
                    if let Some(corr) = catalog_corr { send(&mut stream,FrameType::StreamData,0,corr,json!({"op":"catalog.watch","generation":2})).await; }
                }
                Some(BusCommand::State(generation)) => push_state(&mut stream,leaf_corr.unwrap(),generation).await,
                Some(BusCommand::Remove) => {
                    available = false;
                    send(&mut stream,FrameType::StreamData,0,catalog_corr.unwrap(),json!({"op":"catalog.watch","generation":3})).await;
                }
                None => return,
            },
            frame = read_frame(&mut stream) => {
                let Some(frame) = frame.unwrap() else { return };
                if frame.header.ty == FrameType::Ping {
                    send(&mut stream,FrameType::Pong,0,frame.header.corr,Value::Null).await;
                    continue;
                }
                if frame.header.ty != FrameType::Request { continue; }
                let body: Value = serde_json::from_slice(&frame.body).unwrap();
                let (ty,reply) = match body["op"].as_str() {
                    Some("catalog.watch") => {
                        catalog_corr = Some(frame.header.corr);
                        (FrameType::StreamData,json!({"op":"catalog.watch","generation":1}))
                    }
                    Some("catalog.list") => {
                        let operations = if available {json!([{"name":"ckbus.leaf_watch","kind":"query"},{"name":"ckbus.leaf_sign","kind":"query"}])} else { json!([]) };
                        (FrameType::Response,json!({"op":"catalog.list","generation":1,"modules":[{"module_id":"ckbus","ready":true,"roles":[{"role":"management_surface","operations":operations,"config_schema":{},"observability":[],"identity_scope":[],"concurrency":"module_managed"}],"control_ops":[]}],"subc_ops":[]}))
                    }
                    Some("server.describe") => (FrameType::Response,json!({"op":"server.describe","protocol_ver":1,"subc_ops":[],"capabilities":[],"connected_clients":2})),
                    Some("route.open") => {
                        assert_eq!(opens.fetch_add(1,Ordering::SeqCst),0,"wrapper must reuse one route");
                        assert_eq!(body["target"]["module_id"],"ckbus");
                        (FrameType::Response,json!({"op":"route.open","route_channel":41,"route_epoch":1}))
                    }
                    other => {
                        assert!(other.is_none(),"unexpected control op: {body}");
                        match body["method"].as_str().unwrap() {
                            "ckbus.leaf_watch" => {
                                assert!(frame.header.flags.is_subscription());
                                leaf_corr = Some(frame.header.corr);
                                push_state(&mut stream,frame.header.corr,7).await;
                                continue;
                            }
                            "ckbus.leaf_sign" => {
                                calls.send(body.clone()).await.unwrap();
                                match body["params"]["remote"].as_str().unwrap() {
                                    "hub" => (FrameType::Response,json!({"result":{"jwt":"public.jwt","signature_b64":"A".repeat(86)}})),
                                    "denied" => (FrameType::Error,json!({"code":"vault_unavailable","message":"must not substitute this message for the code"})),
                                    "timeout" => continue,
                                    other => panic!("unexpected signer remote: {other}"),
                                }
                            }
                            other => panic!("unexpected bus method: {other}"),
                        }
                    }
                };
                send(&mut stream,ty,frame.header.channel,frame.header.corr,reply).await;
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrapper_relay_discovers_watch_relays_state_and_signs_then_stops() {
    let mut fixture = Fixture::start().await;
    assert_eq!(
        fixture.event("sign_failed", 1).await,
        json!({"v":1,"type":"sign_failed","id":1,"signer":"ck-bus","reason":"ck-bus unavailable"})
    );
    assert_eq!(
        fixture.event("tls_sign_failed", 2).await,
        json!({"v":1,"type":"tls_sign_failed","id":2,"signer":"ck-bus","reason":"ck-bus unavailable"})
    );
    fixture.enable().await;
    let events = fs::read_to_string(fixture.root.path().join("engine.jsonl")).unwrap();
    let states: Vec<Value> = events
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|value| value["type"] == "state")
        .collect();
    assert_eq!(states,json!([{"v":1,"type":"state","generation":0,"remotes":[]},{"v":1,"type":"state","generation":7,"remotes":remotes()}]).as_array().unwrap().clone());
    fixture.commands.send(BusCommand::State(8)).await.unwrap();
    assert_eq!(
        fixture.event("sign_failed", 4).await["reason"],
        "vault_unavailable"
    );
    assert_eq!(
        fixture.calls.recv().await.unwrap()["params"]["remote"],
        "denied"
    );
    let started = Instant::now();
    fixture.commands.send(BusCommand::State(9)).await.unwrap();
    assert_eq!(
        fixture.calls.recv().await.unwrap()["params"]["remote"],
        "timeout"
    );
    assert_eq!(fixture.event("sign_failed", 5).await["signer"], "ck-bus");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "wrapper must answer before engine's five-second signing deadline"
    );
    assert_eq!(fixture.opens.load(Ordering::SeqCst), 1);
    fixture.commands.send(BusCommand::Remove).await.unwrap();
    fixture.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrapper_exits_nonzero_when_engine_exits_zero_unprompted() {
    let mut fixture = Fixture::start().await;
    fixture.event("sign_failed", 1).await;
    fixture.enable().await;
    fixture.commands.send(BusCommand::State(10)).await.unwrap();
    assert_eq!(
        timeout(LIMIT, fixture.child.wait())
            .await
            .unwrap()
            .unwrap()
            .code(),
        Some(1)
    );
    timeout(LIMIT, &mut fixture.bus).await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrapper_exits_nonzero_on_engine_stdout_eof() {
    let mut fixture = Fixture::start().await;
    fixture.event("sign_failed", 1).await;
    fixture.enable().await;
    fixture.commands.send(BusCommand::State(11)).await.unwrap();
    assert_eq!(
        timeout(LIMIT, fixture.child.wait())
            .await
            .unwrap()
            .unwrap()
            .code(),
        Some(1)
    );
    timeout(LIMIT, &mut fixture.bus).await.unwrap().unwrap();
    let events = fs::read_to_string(fixture.root.path().join("engine.jsonl")).unwrap();
    assert!(events
        .lines()
        .any(|line| serde_json::from_str::<Value>(line).unwrap()["eof"] == true));
}
