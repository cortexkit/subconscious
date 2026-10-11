use super::*;
use cortexkit_test_support::ScratchDir;
use subc_control::{ActivityQueryResponse, ActivitySubscribeResponse, ACTIVITY_READ_FORBIDDEN};

fn read_frame(op: &str, corr: u64, identity: Option<(&str, &str)>) -> Frame {
    let mut body = serde_json::json!({"op":op,"limit":100});
    if let Some((module, nonce)) = identity {
        body["consumer_identity"] = serde_json::json!({"module_id":module,"launch_nonce":nonce});
    }
    Frame::build(
        FrameType::Request,
        control_flags(),
        0,
        0,
        corr,
        serde_json::to_vec(&body).unwrap(),
    )
    .unwrap()
}

fn setup(
    scratch: &ScratchDir,
) -> (
    ControlHandler,
    RouteCtx,
    mpsc::Receiver<crate::router::OutboundFrame>,
) {
    let registry = Arc::new(Registry::default());
    let (ctx, rx) = route_ctx(ConnectionId::new(1));
    let supervisor = SupervisorHandle::new();
    for module in ["callosum", "ckbus", "worker"] {
        supervisor.set_spawn_nonce(module, format!("nonce-{module}"));
    }
    let handler = ControlHandler::with_forwarding(registry, Arc::new(ForwardingTable::default()))
        .with_supervisor(supervisor)
        .with_activity_ledger(scratch.path(), "reads".into());
    (handler, ctx, rx)
}

async fn append(handler: &ControlHandler, at_ms: u64) {
    handler.activity.as_ref().unwrap().record("worker".into(),
        serde_json::from_value(serde_json::json!({"op":"activity.record","kind":"worker.settled","summary":"Worker settled"})).unwrap(),
        subc_control::ActivityActor { principal: Principal::Direct, scope_owner: None, agent_id: None, run_id: None, flow_id: None, project_root: None }, at_ms).await.unwrap();
}

#[tokio::test]
async fn activity_reads_admit_direct_and_only_verified_trusted_principals() {
    let scratch = ScratchDir::new("activity-read-identity");
    let (handler, ctx, mut rx) = setup(&scratch);
    hello_via_sink(
        &handler,
        &ctx,
        &mut rx,
        hello_frame("worker", PROTOCOL_VERSION, 1),
    )
    .await;
    for op in ["activity.query", "activity.subscribe"] {
        for (index, identity) in [
            None,
            Some(("callosum", "nonce-callosum")),
            Some(("ckbus", "nonce-ckbus")),
        ]
        .into_iter()
        .enumerate()
        {
            let corr = 10 + index as u64;
            let replies = handler
                .handle_control_frame(&ctx, read_frame(op, corr, identity))
                .await
                .unwrap();
            if op == "activity.query" {
                assert_eq!(replies.len(), 1);
                assert_eq!(replies[0].header.ty, FrameType::Response);
                let reply: ActivityQueryResponse =
                    serde_json::from_slice(&replies[0].body).unwrap();
                assert_eq!(reply.head.daemon_incarnation, "reads");
                assert_eq!(reply.head.seq, 0);
            } else {
                assert!(replies.is_empty());
            }
        }
        for identity in [
            Some(("worker", "nonce-worker")),
            Some(("callosum", "wrong")),
            Some(("ckbus", "wrong")),
            Some(("unlaunched", "nonce")),
        ] {
            let replies = handler
                .handle_control_frame(&ctx, read_frame(op, 40, identity))
                .await
                .unwrap();
            assert_eq!(replies.len(), 1);
            assert_eq!(replies[0].header.ty, FrameType::Error);
            assert_eq!(parse_error(&replies[0])["code"], ACTIVITY_READ_FORBIDDEN);
        }
    }
    handler.cleanup_connection(ctx.connection_id).unwrap();
}

#[tokio::test]
async fn activity_subscription_streams_replay_live_then_cancel_on_same_correlation() {
    let scratch = ScratchDir::new("activity-read-stream");
    let (handler, ctx, mut rx) = setup(&scratch);
    append(&handler, 7).await;
    let replies = handler
        .handle_control_frame(&ctx, read_frame("activity.query", 1, None))
        .await
        .unwrap();
    let head = serde_json::from_slice::<ActivityQueryResponse>(&replies[0].body)
        .unwrap()
        .head;
    append(&handler, 7).await;
    let mut request = read_frame("activity.subscribe", 2, None);
    request.body =
        serde_json::to_vec(&serde_json::json!({"op":"activity.subscribe","since":head})).unwrap();
    assert!(handler
        .handle_control_frame(&ctx, request)
        .await
        .unwrap()
        .is_empty());
    let replay = rx.recv().await.unwrap().frame;
    assert_eq!(replay.header.ty, FrameType::StreamData);
    assert_eq!(replay.header.corr, 2);
    assert_eq!(
        serde_json::from_slice::<ActivitySubscribeResponse>(&replay.body)
            .unwrap()
            .entry
            .seq,
        2
    );
    append(&handler, 7).await;
    let live = rx.recv().await.unwrap().frame;
    assert_eq!(live.header.ty, FrameType::StreamData);
    assert_eq!(live.header.corr, 2);
    assert_eq!(
        serde_json::from_slice::<ActivitySubscribeResponse>(&live.body)
            .unwrap()
            .entry
            .seq,
        3
    );
    let cancel = Frame::build(FrameType::Cancel, control_flags(), 0, 0, 2, Vec::new()).unwrap();
    assert!(handler
        .handle_control_frame(&ctx, cancel)
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        rx.recv().await.unwrap().frame.header.ty,
        FrameType::StreamEnd
    );
    append(&handler, 7).await;
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn activity_subscription_wire_lag_error_follows_all_queued_entries() {
    let scratch = ScratchDir::new("activity-wire-lag");
    let (mut handler, ctx, mut rx) = setup(&scratch);
    let clock = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let controlled = clock.clone();
    handler.activity = Some(crate::activity::ActivityLedger::start(
        scratch.path().join("activity.jsonl"),
        "reads".into(),
        2,
        crate::activity::WriterHooks {
            clock: Some(Arc::new(move || {
                controlled.load(std::sync::atomic::Ordering::Relaxed)
            })),
            ..Default::default()
        },
    ));
    assert!(handler
        .handle_control_frame(&ctx, read_frame("activity.subscribe", 5, None))
        .await
        .unwrap()
        .is_empty());
    for tick in 1..=300 {
        clock.store(tick * 100, std::sync::atomic::Ordering::Relaxed);
        append(&handler, 7).await;
    }
    let mut last = 0;
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap()
            .frame;
        assert_eq!(frame.header.corr, 5);
        if frame.header.ty == FrameType::Error {
            assert_eq!(
                parse_error(&frame)["code"],
                subc_control::ACTIVITY_SUBSCRIBER_LAGGED
            );
            assert!(last >= 256 && last < 300);
            break;
        }
        assert_eq!(frame.header.ty, FrameType::StreamData);
        let entry = serde_json::from_slice::<ActivitySubscribeResponse>(&frame.body)
            .unwrap()
            .entry;
        assert_eq!(entry.seq, last + 1);
        last = entry.seq;
    }
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn activity_retention_config_reload_applies_only_on_executed_rescan() {
    let scratch = ScratchDir::new("activity-rescan");
    let (handler, ctx, _rx) = setup(&scratch);
    let config_path = scratch.path().join("subc.jsonc");
    std::fs::write(
        &config_path,
        r#"{"version":1,"modules":{},"activity":{"max_age_days":2,"max_total_mb":1}}"#,
    )
    .unwrap();
    let supervisor =
        crate::Supervisor::new_for_test(handler.registry.clone(), crate::RestartPolicy::default())
            .with_handle(handler.supervisor.clone());
    let handler = handler.with_supervisor_rescan(supervisor, &config_path, None);
    append(&handler, 7).await;
    let path = scratch.path().join("activity.jsonl.1");
    std::fs::write(&path, "old-generation\n").unwrap();
    for preview in [true, false] {
        let request = Frame::build(
            FrameType::Request,
            control_flags(),
            0,
            0,
            6,
            serde_json::to_vec(&serde_json::json!({"op":"supervisor.rescan","preview":preview}))
                .unwrap(),
        )
        .unwrap();
        let replies = handler.handle_control_frame(&ctx, request).await.unwrap();
        assert_eq!(replies[0].header.ty, FrameType::Response);
        assert_eq!(path.exists(), preview);
    }
}
