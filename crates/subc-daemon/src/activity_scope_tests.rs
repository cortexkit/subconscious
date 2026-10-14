use super::*;
use subc_control::{ActivityEntry, ACTIVITY_ACTOR_UNKNOWN, ACTIVITY_MALFORMED};

fn event(module: &str, on: Value) -> Frame {
    Frame::build(FrameType::Request, control_flags(), 0, 0, 500,
        serde_json::to_vec(&serde_json::json!({"op":"activity.record", "kind":format!("{module}.settled"), "summary":"Worker settled", "on":on})).unwrap()).unwrap()
}

async fn record_on(rig: &Rig, module: &str, on: Value) -> Frame {
    let ctx = if module == OWNER {
        &rig.owner
    } else {
        &rig.modules.get(module).unwrap().0
    };
    rig.handler
        .handle_control_frame(ctx, event(module, on))
        .await
        .unwrap()
        .remove(0)
}

fn citation(bound: &Bound) -> Value {
    let (channel, epoch) = route_bind_channel(&bound.bind);
    serde_json::json!({"route":{"channel":channel,"epoch":epoch}})
}

#[tokio::test]
async fn activity_scope_and_route_stamps_never_echo_bind_proof() {
    const PROOF: &str = "BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc";
    const HASH: &str = "4bb06f8e4e3a7715d201d573d0aa423762e55dabd61a2c02278fa56cc6d294e0";
    let scratch = ScratchDir::new("activity-bind-proof");
    let mut rig = rig().await;
    rig.handler = rig.handler.with_activity_ledger(scratch.path(), "daemon".into());
    rig.sync(vec![session(1).with_bind_proof_sha256(Some(HASH.into()))]).await;
    let bound = rig.bound(Some(AFT), PLEXUS,
        Some(rig.selector("s", Some(1)).with_bind_proof(Some(PROOF.into())))).await;
    assert_eq!(record_on(&rig, PLEXUS, citation(&bound)).await.header.ty, FrameType::Response);
    assert_eq!(record_on(&rig, OWNER, serde_json::json!({"scope":{"ref":"s","epoch":1}})).await.header.ty, FrameType::Response);
    let ledger = std::fs::read_to_string(scratch.path().join("activity.jsonl")).unwrap();
    assert_eq!(ledger.lines().count(), 2);
    assert!(!ledger.contains(PROOF));
    assert!(!ledger.contains(HASH));
}

#[tokio::test]
async fn activity_route_stamps_verified_opener_project_and_scope_attributes() {
    let scratch = ScratchDir::new("activity-route");
    let mut rig = rig_with_scope_capabilities(true, true).await;
    rig.handler = rig
        .handler
        .with_activity_ledger(scratch.path(), "daemon".into());
    let mut scope = session(1);
    scope.attributes.flow_id = Some("flow-1".into());
    rig.sync(vec![scope]).await;
    let bound = rig
        .bound(Some(AFT), PLEXUS, Some(rig.selector("s", Some(1))))
        .await;
    let response = record_on(&rig, PLEXUS, citation(&bound)).await;
    assert_eq!(response.header.ty, FrameType::Response);
    let entry: ActivityEntry = serde_json::from_str(
        std::fs::read_to_string(scratch.path().join("activity.jsonl"))
            .unwrap()
            .trim(),
    )
    .unwrap();
    assert_eq!(entry.seq, 1);
    assert_eq!(
        entry.actor.principal,
        Principal::Reserved {
            module_id: AFT.into()
        }
    );
    assert_eq!(
        entry.actor.scope_owner,
        Some(Principal::Reserved {
            module_id: OWNER.into()
        })
    );
    assert_eq!(entry.actor.agent_id.as_deref(), Some("agent-1"));
    assert!(entry.actor.run_id.is_none());
    assert_eq!(entry.actor.flow_id.as_deref(), Some("flow-1"));
    assert!(entry.actor.project_root.as_ref().unwrap().is_absolute());
    let uncited = record_on(&rig, PLEXUS, Value::Null).await;
    assert_eq!(uncited.header.ty, FrameType::Response);
    let last: ActivityEntry = serde_json::from_str(
        std::fs::read_to_string(scratch.path().join("activity.jsonl"))
            .unwrap()
            .lines()
            .last()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        last.actor.principal,
        Principal::Reserved {
            module_id: PLEXUS.into()
        }
    );
    assert!(last.actor.scope_owner.is_none());
    let mut run = session(2);
    run.attributes.delegates = false;
    run.attributes.run_id = Some("run-1".into());
    rig.sync(vec![run]).await;
    while rig.modules.get_mut(PLEXUS).unwrap().1.try_recv().is_ok() {}
    let bound = rig
        .bound(Some(AFT), PLEXUS, Some(rig.selector("s", Some(2))))
        .await;
    assert_eq!(
        record_on(&rig, PLEXUS, citation(&bound)).await.header.ty,
        FrameType::Response
    );
    let last: ActivityEntry = serde_json::from_str(
        std::fs::read_to_string(scratch.path().join("activity.jsonl"))
            .unwrap()
            .lines()
            .last()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(last.actor.run_id.as_deref(), Some("run-1"));
    assert!(last.actor.flow_id.is_none());
}

#[tokio::test]
async fn activity_foreign_route_and_stale_epoch_refuse_actor_unknown() {
    let scratch = ScratchDir::new("activity-route-refusals");
    let mut rig = rig().await;
    rig.handler = rig
        .handler
        .with_activity_ledger(scratch.path(), "daemon".into());
    rig.sync(vec![session(1)]).await;
    let bound = rig
        .bound(Some(AFT), PLEXUS, Some(rig.selector("s", Some(1))))
        .await;
    assert_eq!(
        parse_error(&record_on(&rig, OTHER, citation(&bound)).await)["code"],
        ACTIVITY_ACTOR_UNKNOWN
    );
    let mut stale = citation(&bound);
    stale["route"]["epoch"] = serde_json::json!(route_bind_channel(&bound.bind).1 + 1);
    assert_eq!(
        parse_error(&record_on(&rig, PLEXUS, stale).await)["code"],
        ACTIVITY_ACTOR_UNKNOWN
    );
}

#[tokio::test]
async fn activity_changed_bound_tag_refuses_actor_unknown_before_route_drain() {
    let scratch = ScratchDir::new("activity-tag");
    let mut rig = rig().await;
    rig.handler = rig
        .handler
        .with_activity_ledger(scratch.path(), "daemon".into());
    rig.sync(vec![session(1)]).await;
    let bound = rig
        .bound(Some(AFT), PLEXUS, Some(rig.selector("s", Some(1))))
        .await;
    // Change the scope table without publishing drains: attribution must check
    // the current tag, not rely on asynchronous route removal having finished.
    let mut changed = session(1);
    changed.attributes.agent_id = Some("agent-2".into());
    let module = OWNER.to_string();
    rig.handler
        .scopes
        .write()
        .unwrap()
        .sync(&module, rig.owner.connection_id, |_| true, 2, vec![changed])
        .unwrap();
    assert!(rig.live(&bound));
    assert_eq!(
        parse_error(&record_on(&rig, PLEXUS, citation(&bound)).await)["code"],
        ACTIVITY_ACTOR_UNKNOWN
    );
}

#[tokio::test]
async fn activity_scope_requires_owner_synced_live_current_epoch_and_deadline() {
    let scratch = ScratchDir::new("activity-scope");
    let mut rig = rig().await;
    rig.handler = rig
        .handler
        .with_activity_ledger(scratch.path(), "daemon".into())
        .with_wall_clock(|| 100);
    let on = serde_json::json!({"scope":{"ref":"s","epoch":1}});
    assert_eq!(
        parse_error(&record_on(&rig, OWNER, on.clone()).await)["code"],
        ACTIVITY_ACTOR_UNKNOWN
    );
    rig.sync(vec![session(1)]).await;
    assert_eq!(
        record_on(&rig, OWNER, on.clone()).await.header.ty,
        FrameType::Response
    );
    assert_eq!(
        parse_error(&record_on(&rig, PLEXUS, on.clone()).await)["code"],
        ACTIVITY_ACTOR_UNKNOWN
    );
    assert_eq!(
        parse_error(
            &record_on(
                &rig,
                OWNER,
                serde_json::json!({"scope":{"ref":"s","epoch":2}})
            )
            .await
        )["code"],
        ACTIVITY_ACTOR_UNKNOWN
    );
    assert_eq!(
        parse_error(
            &record_on(
                &rig,
                OWNER,
                serde_json::json!({"scope":{"ref":"missing","epoch":1}})
            )
            .await
        )["code"],
        ACTIVITY_ACTOR_UNKNOWN
    );
    let mut expired = session(2);
    expired.expires_at_ms = Some(101);
    rig.sync(vec![expired]).await;
    rig.handler = rig.handler.with_wall_clock(|| 101);
    assert_eq!(
        parse_error(
            &record_on(
                &rig,
                OWNER,
                serde_json::json!({"scope":{"ref":"s","epoch":2}})
            )
            .await
        )["code"],
        ACTIVITY_ACTOR_UNKNOWN
    );
}

#[tokio::test]
async fn activity_dispatch_rejects_malformed_details_subject_and_actor() {
    let mut rig = rig().await;
    let scratch = ScratchDir::new("activity-shape");
    rig.handler = rig
        .handler
        .with_activity_ledger(scratch.path(), "daemon".into());
    for field in [
        serde_json::json!({"details":[]}),
        serde_json::json!({"details":null}),
        serde_json::json!({"subject":{"type":"repo"}}),
        serde_json::json!({"subject":{"type":"repo","id":"repo-1","unknown":1}}),
        serde_json::json!({"actor":{"principal":"direct"}}),
        serde_json::json!({"summary":""}),
    ] {
        let mut body: Value = serde_json::from_slice(&event(PLEXUS, Value::Null).body).unwrap();
        body.as_object_mut()
            .unwrap()
            .extend(field.as_object().unwrap().clone());
        let frame = Frame::build(
            FrameType::Request,
            control_flags(),
            0,
            0,
            500,
            serde_json::to_vec(&body).unwrap(),
        )
        .unwrap();
        let replies = rig
            .handler
            .handle_control_frame(&rig.modules.get(PLEXUS).unwrap().0, frame)
            .await
            .unwrap();
        assert_eq!(
            parse_error(&replies[0])["code"],
            ACTIVITY_MALFORMED,
            "{body}"
        );
    }
}

#[tokio::test]
async fn activity_module_reconnect_does_not_refill_and_actor_refusal_costs_nothing() {
    use crate::activity::{ActivityLedger, WriterHooks};
    use std::sync::atomic::{AtomicU64, Ordering};
    let scratch = ScratchDir::new("activity-reconnect");
    let mut rig = rig().await;
    let now = Arc::new(AtomicU64::new(0));
    let clock = now.clone();
    rig.handler.activity = Some(ActivityLedger::start(
        scratch.path().join("activity.jsonl"),
        "rate".into(),
        2,
        WriterHooks {
            clock: Some(Arc::new(move || clock.load(Ordering::SeqCst))),
            ..Default::default()
        },
    ));
    for _ in 0..59 {
        assert_eq!(
            record_on(&rig, PLEXUS, Value::Null).await.header.ty,
            FrameType::Response
        );
    }
    assert_eq!(
        parse_error(
            &record_on(
                &rig,
                PLEXUS,
                serde_json::json!({"scope":{"ref":"missing","epoch":1}})
            )
            .await
        )["code"],
        ACTIVITY_ACTOR_UNKNOWN
    );
    assert_eq!(
        record_on(&rig, PLEXUS, Value::Null).await.header.ty,
        FrameType::Response
    );
    assert_eq!(
        parse_error(&record_on(&rig, PLEXUS, Value::Null).await)["code"],
        subc_control::ACTIVITY_RATE_LIMITED
    );
    rig.handler
        .cleanup_connection(rig.modules[PLEXUS].0.connection_id)
        .unwrap();
    let (ctx, mut rx) = wide_ctx(66);
    hello_via_sink(
        &rig.handler,
        &ctx,
        &mut rx,
        hello_frame(PLEXUS, PROTOCOL_VERSION, 1),
    )
    .await;
    rig.modules.insert(PLEXUS.into(), (ctx, rx));
    assert_eq!(
        parse_error(&record_on(&rig, PLEXUS, Value::Null).await)["code"],
        subc_control::ACTIVITY_RATE_LIMITED
    );
    now.store(100, Ordering::SeqCst);
    assert_eq!(
        record_on(&rig, PLEXUS, Value::Null).await.header.ty,
        FrameType::Response
    );
}

fn isolated_child(case: &str, test: &str) -> bool {
    if std::env::var("SUBC_ACTIVITY_CHILD_CASE").as_deref() == Ok(case) {
        let root = PathBuf::from(std::env::var_os("SUBC_ACTIVITY_CHILD_ROOT").unwrap());
        for (name, suffix) in [
            ("XDG_DATA_HOME", "data"),
            ("XDG_RUNTIME_DIR", "runtime"),
            ("XDG_CONFIG_HOME", "config"),
        ] {
            assert_eq!(
                PathBuf::from(std::env::var_os(name).unwrap()),
                root.join(suffix),
                "{name} must be set on the child Command"
            );
        }
        return false;
    }
    let scratch = ScratchDir::new("activity-daemon-child");
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"])
        .env("SUBC_ACTIVITY_CHILD_CASE", case)
        .env("SUBC_ACTIVITY_CHILD_ROOT", scratch.path())
        .env("XDG_DATA_HOME", scratch.path().join("data"))
        .env("XDG_RUNTIME_DIR", scratch.path().join("runtime"))
        .env("XDG_CONFIG_HOME", scratch.path().join("config"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "child failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("1 passed"),
        "child must execute the named test"
    );
    true
}

async fn receive(stream: &mut tokio::net::TcpStream) -> Frame {
    tokio::time::timeout(Duration::from_secs(5), crate::read_frame(stream))
        .await
        .expect("daemon reader/reply remains responsive")
        .unwrap()
        .unwrap()
}

async fn connect(
    address: std::net::SocketAddr,
    info: &subc_transport::ConnectionInfo,
) -> tokio::net::TcpStream {
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    subc_transport::authenticate_client(&mut stream, info, Duration::from_secs(5))
        .await
        .unwrap();
    stream
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn activity_real_daemon_stores_only_verified_route_actor() {
    if isolated_child(
        "stamp",
        "control::tests::scopes::activity::activity_real_daemon_stores_only_verified_route_actor",
    ) {
        return;
    }
    let scratch = ScratchDir::new("activity-real-daemon");
    let mut rig = rig_with_scope_capabilities(true, true).await;
    rig.handler = rig
        .handler
        .with_activity_ledger(scratch.path(), "wire-daemon".into());
    let mut scope = session(1);
    scope.attributes.flow_id = Some("flow-verified".into());
    rig.sync(vec![scope]).await;
    rig.handler
        .cleanup_connection(rig.modules[PLEXUS].0.connection_id)
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let info = subc_transport::ConnectionInfo {
        schema: subc_transport::SCHEMA_VERSION,
        wire_version: Some(PROTOCOL_VERSION),
        endpoints: vec![subc_transport::Endpoint {
            host: address.ip().to_string(),
            port: address.port(),
        }],
        key: subc_transport::generate_key().unwrap(),
        daemon_id: subc_transport::generate_daemon_id().unwrap(),
        pid: std::process::id(),
        daemon_ver: "activity-test".into(),
    };
    let router = Arc::new(Router::with_control_handler(Arc::new(rig.handler.clone())));
    let preexisting_connections: Vec<_> = (0..3).map(|_| router.begin_connection()).collect();
    let serving = tokio::spawn(crate::server::serve_listener(
        listener,
        router,
        crate::server::ServerAuth::new(info.key.clone(), info.daemon_id, &info.daemon_ver),
    ));
    let mut module = connect(address, &info).await;
    let mut hello = hello_frame(PLEXUS, PROTOCOL_VERSION, 1);
    let mut body: Value = serde_json::from_slice(&hello.body).unwrap();
    body["manifest"]["capabilities"] =
        serde_json::json!({"provides":["flow-scopes/v1", "agent-run-scopes/v1"]});
    hello = Frame::build(
        FrameType::Hello,
        control_flags(),
        0,
        0,
        1,
        serde_json::to_vec(&body).unwrap(),
    )
    .unwrap();
    crate::write_frame(&mut module, &hello).await.unwrap();
    let ack = parse_ack(&receive(&mut module).await);
    assert!(ack
        .subc_capabilities
        .iter()
        .any(|cap| cap == subc_control::ACTIVITY_LEDGER_CAPABILITY));
    assert!(ack.subc_ops.iter().any(|op| op == ops::ACTIVITY_RECORD));
    let mut client = connect(address, &info).await;
    let (_, _, open) = rig.open_frame(Some(AFT), PLEXUS, Some(rig.selector("s", Some(1))));
    let mut body: Value = serde_json::from_slice(&open.body).unwrap();
    let project = scratch.path().join("project");
    std::fs::create_dir(&project).unwrap();
    body["identity"]["project_root"] = serde_json::json!(project);
    let open = Frame::build(
        FrameType::Request,
        control_flags(),
        0,
        0,
        2,
        serde_json::to_vec(&body).unwrap(),
    )
    .unwrap();
    crate::write_frame(&mut client, &open).await.unwrap();
    let bind = receive(&mut module).await;
    let (channel, epoch) = route_bind_channel(&bind);
    crate::write_frame(&mut module, &route_bind_ack(bind.header.corr))
        .await
        .unwrap();
    assert_eq!(receive(&mut client).await.header.ty, FrameType::Response);
    let record = event(
        PLEXUS,
        serde_json::json!({"route":{"channel":channel,"epoch":epoch}}),
    );
    let sent: Value = serde_json::from_slice(&record.body).unwrap();
    assert!(sent.get("actor").is_none());
    assert!(sent.get("agent_id").is_none());
    crate::write_frame(&mut module, &record).await.unwrap();
    let response = receive(&mut module).await;
    assert_eq!(
        response.header.ty,
        FrameType::Response,
        "{}",
        String::from_utf8_lossy(&response.body)
    );
    let stored: ActivityEntry = serde_json::from_str(
        std::fs::read_to_string(scratch.path().join("activity.jsonl"))
            .unwrap()
            .trim(),
    )
    .unwrap();
    assert_eq!(
        stored.seq,
        serde_json::from_slice::<Value>(&response.body).unwrap()["seq"]
            .as_u64()
            .unwrap()
    );
    assert_eq!(
        stored.actor.principal,
        Principal::Reserved {
            module_id: AFT.into()
        }
    );
    assert_eq!(
        stored.actor.scope_owner,
        Some(Principal::Reserved {
            module_id: OWNER.into()
        })
    );
    assert_eq!(stored.actor.agent_id.as_deref(), Some("agent-1"));
    assert!(stored.actor.run_id.is_none());
    assert_eq!(stored.actor.flow_id.as_deref(), Some("flow-verified"));
    // Windows canonicalizes to the verbatim `\\?\` form, while the route records
    // the plain spelling; compare the roots with that prefix removed.
    let plain = |path: std::path::PathBuf| {
        let text = path.to_string_lossy().into_owned();
        std::path::PathBuf::from(text.strip_prefix(r"\\?\").unwrap_or(&text))
    };
    assert_eq!(
        stored.actor.project_root.map(plain),
        Some(plain(std::fs::canonicalize(project).unwrap()))
    );
    let describe = Frame::build(
        FrameType::Request,
        control_flags(),
        0,
        0,
        3,
        br#"{"op":"server.describe"}"#.to_vec(),
    )
    .unwrap();
    crate::write_frame(&mut client, &describe).await.unwrap();
    let response: Value = serde_json::from_slice(&receive(&mut client).await.body).unwrap();
    assert!(response["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .any(|cap| cap == subc_control::ACTIVITY_LEDGER_CAPABILITY));
    assert!(response["subc_ops"]
        .as_array()
        .unwrap()
        .iter()
        .any(|op| op == ops::ACTIVITY_RECORD));
    drop(module);
    drop(client);
    serving.abort();
    drop(preexisting_connections);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn activity_real_daemon_failures_leave_reader_routing_supervision_and_shutdown_live() {
    if isolated_child("faults", "control::tests::scopes::activity::activity_real_daemon_failures_leave_reader_routing_supervision_and_shutdown_live") { return; }
    use crate::activity::{ActivityLedger, WriterHooks};
    for mode in ["open", "write", "full"] {
        let scratch = ScratchDir::new("activity-real-fault");
        let mut rig = rig().await;
        let pause = Arc::new((std::sync::Mutex::new(true), std::sync::Condvar::new()));
        let ledger = ActivityLedger::start(
            scratch.path().join("activity.jsonl"),
            "fault".into(),
            1,
            WriterHooks {
                open_failure: mode == "open",
                write_failure: mode == "write",
                pause: (mode == "full").then(|| pause.clone()),
                ..Default::default()
            },
        );
        rig.handler.activity = Some(ledger.clone());
        rig.handler
            .cleanup_connection(rig.modules[PLEXUS].0.connection_id)
            .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let info = subc_transport::ConnectionInfo {
            schema: subc_transport::SCHEMA_VERSION,
            wire_version: Some(PROTOCOL_VERSION),
            endpoints: vec![],
            key: subc_transport::generate_key().unwrap(),
            daemon_id: subc_transport::generate_daemon_id().unwrap(),
            pid: std::process::id(),
            daemon_ver: "activity-fault-test".into(),
        };
        let router = Arc::new(Router::with_control_handler(Arc::new(rig.handler.clone())));
        let preexisting_connections: Vec<_> = (0..3).map(|_| router.begin_connection()).collect();
        let serving = tokio::spawn(crate::server::serve_listener(
            listener,
            router,
            crate::server::ServerAuth::new(info.key.clone(), info.daemon_id, &info.daemon_ver),
        ));
        let mut module = connect(address, &info).await;
        crate::write_frame(&mut module, &hello_frame(PLEXUS, PROTOCOL_VERSION, 1))
            .await
            .unwrap();
        assert_eq!(receive(&mut module).await.header.ty, FrameType::HelloAck);
        crate::write_frame(&mut module, &event(PLEXUS, Value::Null))
            .await
            .unwrap();
        if mode == "full" {
            tokio::time::timeout(Duration::from_secs(5), async {
                while ledger.remaining_capacity() != 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            crate::write_frame(&mut module, &event(PLEXUS, Value::Null))
                .await
                .unwrap();
        }
        assert_eq!(
            parse_error(&receive(&mut module).await)["code"],
            subc_control::ACTIVITY_UNAVAILABLE,
            "{mode}"
        );
        let ping = Frame::build(FrameType::Ping, control_flags(), 0, 0, 510, Vec::new()).unwrap();
        crate::write_frame(&mut module, &ping).await.unwrap();
        assert_eq!(
            receive(&mut module).await.header.ty,
            FrameType::Pong,
            "{mode}"
        );
        let mut client = connect(address, &info).await;
        let list = Frame::build(
            FrameType::Request,
            control_flags(),
            0,
            0,
            511,
            br#"{"op":"supervisor.list"}"#.to_vec(),
        )
        .unwrap();
        crate::write_frame(&mut client, &list).await.unwrap();
        assert_eq!(
            receive(&mut client).await.header.ty,
            FrameType::Response,
            "{mode}"
        );
        let (_, _, open) = rig.open_frame(None, PLEXUS, None);
        let mut body: Value = serde_json::from_slice(&open.body).unwrap();
        body["identity"]["project_root"] = serde_json::json!(scratch.path());
        let open = Frame::build(
            FrameType::Request,
            control_flags(),
            0,
            0,
            512,
            serde_json::to_vec(&body).unwrap(),
        )
        .unwrap();
        crate::write_frame(&mut client, &open).await.unwrap();
        let bind = receive(&mut module).await;
        crate::write_frame(&mut module, &route_bind_ack(bind.header.corr))
            .await
            .unwrap();
        let opened = receive(&mut client).await;
        let (channel, epoch) = published_route(&opened);
        let routed = Frame::build(
            FrameType::Request,
            control_flags(),
            channel,
            epoch,
            513,
            b"opaque request".to_vec(),
        )
        .unwrap();
        crate::write_frame(&mut client, &routed).await.unwrap();
        let delivered = receive(&mut module).await;
        assert_eq!(delivered.body, b"opaque request", "{mode}");
        // Connection teardown must not wait for the paused disk writer's reply.
        drop(module);
        drop(client);
        tokio::time::timeout(Duration::from_secs(5), async {
            while rig.forwarding.active_binding_count().unwrap() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        serving.abort();
        drop(preexisting_connections);
        *pause.0.lock().unwrap() = false;
        pause.1.notify_all();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn activity_real_daemon_restart_head_and_query_do_not_block_reader() {
    if isolated_child("read-io", "control::tests::scopes::activity::activity_real_daemon_restart_head_and_query_do_not_block_reader") { return; }
    let scratch = ScratchDir::new("activity-real-read");
    let mut rig = rig().await;
    rig.handler = rig
        .handler
        .with_activity_ledger(scratch.path(), "old".into());
    assert_eq!(
        record_on(&rig, PLEXUS, Value::Null).await.header.ty,
        FrameType::Response
    );
    rig.handler = rig
        .handler
        .with_activity_ledger(scratch.path(), "new".into());
    let ledger = rig.handler.activity.as_ref().unwrap().clone();
    rig.handler
        .cleanup_connection(rig.modules[PLEXUS].0.connection_id)
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let info = subc_transport::ConnectionInfo {
        schema: subc_transport::SCHEMA_VERSION,
        wire_version: Some(PROTOCOL_VERSION),
        endpoints: vec![],
        key: subc_transport::generate_key().unwrap(),
        daemon_id: subc_transport::generate_daemon_id().unwrap(),
        pid: std::process::id(),
        daemon_ver: "activity-read-test".into(),
    };
    let router = Arc::new(Router::with_control_handler(Arc::new(rig.handler.clone())));
    let preexisting_connections: Vec<_> = (0..3).map(|_| router.begin_connection()).collect();
    let serving = tokio::spawn(crate::server::serve_listener(
        listener,
        router,
        crate::server::ServerAuth::new(info.key.clone(), info.daemon_id, &info.daemon_ver),
    ));
    let mut module = connect(address, &info).await;
    crate::write_frame(&mut module, &hello_frame(PLEXUS, PROTOCOL_VERSION, 1))
        .await
        .unwrap();
    assert_eq!(receive(&mut module).await.header.ty, FrameType::HelloAck);
    let mut client = connect(address, &info).await;
    let request = |body: Value, corr| {
        Frame::build(
            FrameType::Request,
            control_flags(),
            0,
            0,
            corr,
            serde_json::to_vec(&body).unwrap(),
        )
        .unwrap()
    };
    crate::write_frame(
        &mut client,
        &request(serde_json::json!({"op":"activity.query","limit":100}), 500),
    )
    .await
    .unwrap();
    let reply = receive(&mut client).await;
    let page: subc_control::ActivityQueryResponse = serde_json::from_slice(&reply.body).unwrap();
    assert_eq!(
        page.entries
            .iter()
            .map(|entry| entry.seq)
            .collect::<Vec<_>>(),
        [1]
    );
    assert_eq!(
        page.head,
        subc_control::ActivityCursor {
            daemon_incarnation: "new".into(),
            seq: 1
        }
    );
    crate::write_frame(
        &mut client,
        &request(
            serde_json::json!({"op":"activity.subscribe","since":page.head}),
            501,
        ),
    )
    .await
    .unwrap();
    crate::write_frame(&mut module, &event(PLEXUS, Value::Null))
        .await
        .unwrap();
    assert_eq!(receive(&mut module).await.header.ty, FrameType::Response);
    let live = receive(&mut client).await;
    assert_eq!(live.header.ty, FrameType::StreamData);
    assert_eq!(live.header.corr, 501);
    assert_eq!(
        serde_json::from_slice::<subc_control::ActivitySubscribeResponse>(&live.body)
            .unwrap()
            .entry
            .seq,
        2
    );
    let entered = Arc::new(std::sync::Barrier::new(2));
    let release = Arc::new(std::sync::Barrier::new(2));
    let worker_entered = entered.clone();
    let worker_release = release.clone();
    let blocker = std::thread::spawn(move || {
        ledger.with_writer_lock(|| {
            worker_entered.wait();
            worker_release.wait();
        })
    });
    entered.wait();
    crate::write_frame(
        &mut client,
        &request(serde_json::json!({"op":"activity.query","limit":100}), 502),
    )
    .await
    .unwrap();
    crate::write_frame(
        &mut client,
        &Frame::build(FrameType::Ping, control_flags(), 0, 0, 503, Vec::new()).unwrap(),
    )
    .await
    .unwrap();
    let pong = tokio::time::timeout(Duration::from_secs(1), crate::read_frame(&mut client)).await;
    release.wait();
    blocker.join().unwrap();
    let pong = pong
        .expect("query must leave the channel-0 reader free to answer ping")
        .unwrap()
        .unwrap();
    assert_eq!((pong.header.ty, pong.header.corr), (FrameType::Pong, 503));
    assert_eq!(receive(&mut client).await.header.corr, 502);
    let cancel = Frame::build(FrameType::Cancel, control_flags(), 0, 0, 501, Vec::new()).unwrap();
    crate::write_frame(&mut client, &cancel).await.unwrap();
    assert_eq!(receive(&mut client).await.header.ty, FrameType::StreamEnd);
    serving.abort();
    drop(preexisting_connections);
}
