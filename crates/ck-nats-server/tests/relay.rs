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
    HubState(u64, Option<Value>),
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

async fn push_state(stream: &mut TcpStream, corr: u64, generation: u64, hub: bool) {
    let state = if hub {
        json!({"generation":generation,"hub_listener":{"port":7422,"cert_der_b64":"public-certificate"}})
    } else {
        json!({"generation":generation,"remotes":remotes()})
    };
    send(
        stream,
        FrameType::StreamData,
        41,
        corr,
        json!({"result":state}),
    )
    .await;
}

async fn push_hub_state(
    stream: &mut TcpStream,
    corr: u64,
    generation: u64,
    listener: Option<Value>,
) {
    let mut state = json!({"generation":generation});
    if let Some(listener) = listener {
        state["hub_listener"] = listener;
    }
    send(
        stream,
        FrameType::StreamData,
        41,
        corr,
        json!({"result":state}),
    )
    .await;
}

impl Fixture {
    async fn start() -> Self {
        Self::start_as(false).await
    }

    async fn start_as(hub: bool) -> Self {
        Self::start_mode(hub, false).await
    }

    async fn start_mode(hub: bool, lifecycle: bool) -> Self {
        let module_id = if hub { "nats-hub" } else { "nats-server" };
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
            json!({"events":root.path().join("engine.jsonl"),"lifecycle":lifecycle}).to_string(),
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
            .env("SUBC_MODULE_ID", module_id)
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
        assert_eq!(body.manifest.module_id, module_id);
        assert_eq!(body.manifest.registration_channel, Some(true));
        assert!(body.manifest.provides.is_empty());
        let ack = ModuleHelloAckBody {
            resume_token: None,
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
            fake_bus(stream, receive, call_tx, bus_opens, hub, lifecycle).await;
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

    async fn lifecycle_events(&self, predicate: impl Fn(&[Value]) -> bool) -> Vec<Value> {
        timeout(LIMIT, async {
            loop {
                let text =
                    fs::read_to_string(self.root.path().join("engine.jsonl")).unwrap_or_default();
                let events: Vec<Value> = text
                    .lines()
                    .filter_map(|line| serde_json::from_str(line).ok())
                    .collect();
                if predicate(&events) {
                    return events;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("engine lifecycle event did not arrive")
    }

    async fn health(&mut self) -> Value {
        send(
            &mut self.primary,
            FrameType::Request,
            0,
            501,
            json!({"op":"health.check"}),
        )
        .await;
        timeout(LIMIT, async {
            loop {
                let frame = read_frame(&mut self.primary).await.unwrap().unwrap();
                if frame.header.ty == FrameType::Ping {
                    send(
                        &mut self.primary,
                        FrameType::Pong,
                        0,
                        frame.header.corr,
                        Value::Null,
                    )
                    .await;
                } else if frame.header.corr == 501 {
                    assert_eq!(frame.header.ty, FrameType::Response);
                    return serde_json::from_slice(&frame.body).unwrap();
                }
            }
        })
        .await
        .unwrap()
    }

    async fn wait_health(&mut self, detail: &str) -> Value {
        timeout(LIMIT, async {
            loop {
                let report = self.health().await;
                if report["detail"] == detail {
                    assert_eq!(report["status"], "ok");
                    return report;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("health did not reach the expected state")
    }
}

async fn fake_bus(
    mut stream: TcpStream,
    mut commands: mpsc::Receiver<BusCommand>,
    calls: mpsc::Sender<Value>,
    opens: Arc<AtomicUsize>,
    hub: bool,
    lifecycle: bool,
) {
    let watch_method = if hub {
        "ckbus.hub_server_watch"
    } else {
        "ckbus.leaf_watch"
    };
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
                Some(BusCommand::State(generation)) => push_state(&mut stream,leaf_corr.unwrap(),generation,hub).await,
                Some(BusCommand::HubState(generation, listener)) => push_hub_state(&mut stream,leaf_corr.unwrap(),generation,listener).await,
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
                        let operations = if available {json!([{"name":watch_method,"kind":"query"},{"name":if hub { "ckbus.hub_tls_sign" } else { "ckbus.leaf_sign" },"kind":"query"}])} else { json!([]) };
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
                            method if method == watch_method => {
                                assert!(frame.header.flags.is_subscription());
                                leaf_corr = Some(frame.header.corr);
                                if lifecycle {
                                    push_hub_state(&mut stream,frame.header.corr,7,None).await;
                                } else {
                                    push_state(&mut stream,frame.header.corr,7,hub).await;
                                }
                                continue;
                            }
                            "ckbus.leaf_sign" => {
                                assert!(!hub);
                                calls.send(body.clone()).await.unwrap();
                                match body["params"]["remote"].as_str().unwrap() {
                                    "hub" => (FrameType::Response,json!({"result":{"jwt":"public.jwt","signature_b64":"A".repeat(86)}})),
                                    "denied" => (FrameType::Error,json!({"code":"vault_unavailable","message":"must not substitute this message for the code"})),
                                    "timeout" => continue,
                                    other => panic!("unexpected signer remote: {other}"),
                                }
                            }
                            "ckbus.hub_tls_sign" => {
                                assert!(hub);
                                assert_eq!(body["params"]["scheme"], "ed25519");
                                calls.send(body.clone()).await.unwrap();
                                match body["params"]["message_b64"].as_str().unwrap() {
                                    "aGVsbG8" => (FrameType::Response,json!({"result":{"signature_b64":"A".repeat(86)}})),
                                    "ZGVuaWVk" => (FrameType::Error,json!({"code":"vault_unavailable","message":"not the reason"})),
                                    "dGltZW91dA" => continue,
                                    other => panic!("unexpected TLS message: {other}"),
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
async fn wrapper_hub_idle_health_before_listener() {
    let mut fixture = Fixture::start_mode(true, true).await;
    fixture
        .lifecycle_events(|events| {
            events
                .iter()
                .any(|e| e["type"] == "state" && e["generation"] == 0)
        })
        .await;
    fixture.wait_health("Not this computer's hub; idle").await;
    fixture.commands.send(BusCommand::Enable).await.unwrap();
    let events = fixture
        .lifecycle_events(|events| {
            events
                .iter()
                .any(|e| e["type"] == "state" && e["generation"] == 7)
        })
        .await;
    assert_eq!(events.iter().filter(|e| e["startup"] == true).count(), 1);
    assert!(events
        .iter()
        .filter(|e| e["type"] == "state")
        .all(|e| e.get("hub_listener").is_none()));
    fixture.wait_health("Not this computer's hub; idle").await;
    fixture.finish().await;
}

async fn active_hub_fixture() -> Fixture {
    let mut fixture = Fixture::start_mode(true, true).await;
    fixture.commands.send(BusCommand::Enable).await.unwrap();
    fixture
        .lifecycle_events(|events| {
            events
                .iter()
                .any(|e| e["type"] == "state" && e["generation"] == 7)
        })
        .await;
    fixture
        .commands
        .send(BusCommand::HubState(
            8,
            Some(json!({"port":7422,"cert_der_b64":"certificate-a"})),
        ))
        .await
        .unwrap();
    fixture.wait_health("Engine is ready.").await;
    fixture
}

fn assert_clean_restart(events: &[Value], generation: u64) -> Value {
    let new_state = events
        .iter()
        .position(|e| e["type"] == "state" && e["generation"] == generation)
        .unwrap();
    let new_pid = events[new_state]["process_id"].clone();
    let startup = events
        .iter()
        .position(|e| e["startup"] == true && e["process_id"] == new_pid)
        .unwrap();
    assert!(events[startup]["hub"] == true);
    assert!(
        events[..startup]
            .iter()
            .any(|e| e["type"] == "shutdown" && e["process_id"] != new_pid),
        "old engine must receive shutdown before replacement starts"
    );
    assert!(startup < new_state);
    events[new_state].clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrapper_hub_withdrawal_restarts_back_to_idle() {
    let mut fixture = active_hub_fixture().await;
    fixture
        .commands
        .send(BusCommand::HubState(9, None))
        .await
        .unwrap();
    let events = fixture
        .lifecycle_events(|events| {
            events
                .iter()
                .any(|e| e["type"] == "state" && e["generation"] == 9)
        })
        .await;
    let state = assert_clean_restart(&events, 9);
    assert!(state.get("hub_listener").is_none());
    assert_eq!(state["remotes"], json!([]));
    assert_eq!(events.iter().filter(|e| e["startup"] == true).count(), 2);
    assert!(
        fixture.child.try_wait().unwrap().is_none(),
        "wrapper must survive listener withdrawal"
    );
    fixture.wait_health("Not this computer's hub; idle").await;
    assert_eq!(fixture.opens.load(Ordering::SeqCst), 1);
    fixture.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrapper_hub_changed_certificate_or_port_restarts_with_new_listener() {
    let mut fixture = active_hub_fixture().await;
    let listener = json!({"port":7422,"cert_der_b64":"certificate-b"});
    fixture
        .commands
        .send(BusCommand::HubState(9, Some(listener.clone())))
        .await
        .unwrap();
    let events = fixture
        .lifecycle_events(|events| {
            events
                .iter()
                .any(|e| e["type"] == "state" && e["generation"] == 9)
        })
        .await;
    assert_eq!(assert_clean_restart(&events, 9)["hub_listener"], listener);
    fixture.wait_health("Engine is ready.").await;
    fixture
        .commands
        .send(BusCommand::HubState(10, Some(listener)))
        .await
        .unwrap();
    let events = fixture
        .lifecycle_events(|events| {
            events
                .iter()
                .any(|e| e["type"] == "state" && e["generation"] == 10)
        })
        .await;
    assert_eq!(
        events.iter().filter(|e| e["startup"] == true).count(),
        2,
        "unchanged listener must not restart"
    );
    let moved = json!({"port":7423,"cert_der_b64":"certificate-b"});
    fixture
        .commands
        .send(BusCommand::HubState(11, Some(moved.clone())))
        .await
        .unwrap();
    let events = fixture
        .lifecycle_events(|events| {
            events
                .iter()
                .any(|e| e["type"] == "state" && e["generation"] == 11)
        })
        .await;
    assert_eq!(assert_clean_restart(&events, 11)["hub_listener"], moved);
    assert_eq!(events.iter().filter(|e| e["startup"] == true).count(), 3);
    fixture.wait_health("Engine is ready.").await;
    assert_eq!(fixture.opens.load(Ordering::SeqCst), 1);
    fixture.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrapper_hub_watch_and_tls_sign_relay() {
    let mut fixture = Fixture::start_as(true).await;
    for id in [1, 2] {
        assert_eq!(
            fixture.event("tls_sign_failed", id).await,
            json!({"v":1,"type":"tls_sign_failed","id":id,"signer":"ck-bus","reason":"ck-bus unavailable"})
        );
    }
    fixture.commands.send(BusCommand::Enable).await.unwrap();
    assert_eq!(
        timeout(LIMIT, fixture.calls.recv()).await.unwrap().unwrap(),
        json!({"method":"ckbus.hub_tls_sign","params":{"message_b64":"aGVsbG8","scheme":"ed25519"}})
    );
    assert_eq!(
        fixture.event("tls_sign_ok", 3).await,
        json!({"v":1,"type":"tls_sign_ok","id":3,"signature_b64":"A".repeat(86)})
    );
    let events = fs::read_to_string(fixture.root.path().join("engine.jsonl")).unwrap();
    let states: Vec<Value> = events
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|value| value["type"] == "state")
        .collect();
    assert_eq!(states, json!([
        {"v":1,"type":"state","generation":0,"remotes":[]},
        {"v":1,"type":"state","generation":7,"remotes":[],"hub_listener":{"port":7422,"cert_der_b64":"public-certificate"}}
    ]).as_array().unwrap().clone());
    assert!(events
        .lines()
        .any(|line| serde_json::from_str::<Value>(line).unwrap()["hub"] == true));
    fixture.commands.send(BusCommand::State(8)).await.unwrap();
    assert_eq!(
        fixture.event("tls_sign_failed", 4).await["reason"],
        "vault_unavailable"
    );
    assert_eq!(
        fixture.calls.recv().await.unwrap()["params"]["message_b64"],
        "ZGVuaWVk"
    );
    let started = Instant::now();
    fixture.commands.send(BusCommand::State(9)).await.unwrap();
    assert_eq!(
        fixture.calls.recv().await.unwrap()["params"]["message_b64"],
        "dGltZW91dA"
    );
    assert_eq!(
        fixture.event("tls_sign_failed", 5).await["signer"],
        "ck-bus"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "TLS relay must answer before the engine deadline"
    );
    assert_eq!(fixture.opens.load(Ordering::SeqCst), 1);
    fixture.commands.send(BusCommand::Remove).await.unwrap();
    fixture.finish().await;
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
