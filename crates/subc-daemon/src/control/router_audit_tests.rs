// Regression tests for control-plane defects found by a read-only audit of
// route admission, bind relay and activity subscriptions. Each test states the
// behaviour that should hold.

const AUDIT_DENIED_CAPABILITY: &str = "credentials-provider/v1";

/// An attested opener that must never reach a capability opens a route to a
/// target that does not claim it yet. While the bind is pending, the target
/// claims the capability with `catalog.update` and then accepts the bind. The
/// accepted bind must not become a live route to the now-denied provider.
#[tokio::test]
async fn bind_refused_when_target_gains_denied_capability_before_ack() {
    let registry = Arc::new(Registry::default());
    let forwarding = Arc::new(ForwardingTable::default());
    let supervisor = SupervisorHandle::new();
    supervisor.set_spawn_nonce("opener", "opener-nonce".to_string());
    let handler = ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding))
        .with_supervisor(supervisor);
    let (target_ctx, mut target_rx) = route_ctx(ConnectionId::new(1_710));
    let (opener_ctx, mut opener_rx) = route_ctx(ConnectionId::new(1_711));
    register_capability_manifest(
        &handler,
        &target_ctx,
        &mut target_rx,
        capability_manifest("target", &[], &[]),
        1,
    )
    .await;
    register_capability_manifest(
        &handler,
        &opener_ctx,
        &mut opener_rx,
        capability_manifest("opener", &[], &[AUDIT_DENIED_CAPABILITY]),
        2,
    )
    .await;

    let (client_ctx, mut client_rx) = route_ctx(ConnectionId::new(1_712));
    let open = tokio::spawn({
        let handler = handler.clone();
        async move {
            handler
                .handle_control_frame(
                    &client_ctx,
                    route_open_frame_with_admission_facts(
                        3,
                        "target",
                        unique_project_root("audit-deny-before-ack"),
                        Some(ConsumerIdentity {
                            module_id: "opener".to_string(),
                            launch_nonce: "opener-nonce".to_string(),
                        }),
                        None,
                    ),
                )
                .await
                .expect("route.open returns frames")
        }
    });
    let bind = tokio::time::timeout(Duration::from_secs(1), target_rx.recv())
        .await
        .expect("the open passes admission and relays route.bind")
        .expect("target control receiver stays open");
    assert!(matches!(
        serde_json::from_slice::<ModuleControlRequest>(&bind.body),
        Ok(ModuleControlRequest::RouteBind { .. })
    ));

    // The bind is pending: the target now claims the denied capability.
    let replies = handler
        .handle_control_frame(
            &target_ctx,
            catalog_update_with_capabilities_frame(
                4,
                CapabilityDeclarations {
                    provides: vec![AUDIT_DENIED_CAPABILITY.to_string()],
                    requires: Vec::new(),
                    must_never_reach: Vec::new(),
                },
            ),
        )
        .await
        .expect("claim catalog.update succeeds");
    assert!(matches!(
        serde_json::from_slice::<ModuleControlResponseToModule>(&replies[0].body),
        Ok(ModuleControlResponseToModule::CatalogUpdate {})
    ));

    // Then it accepts the bind.
    handler
        .handle_control_frame(&target_ctx, route_bind_ack(bind.header.corr))
        .await
        .expect("route.bind ACK is handled");
    let open_replies = open.await.expect("route.open task joins");

    let live = forwarding.active_binding_count().unwrap();
    let mut client_frames = Vec::new();
    while let Ok(outbound) = client_rx.try_recv() {
        client_frames.push(serde_json::from_slice::<Value>(&outbound.frame.body).ok());
    }
    assert_eq!(
        live, 0,
        "the bind committed after the target claimed '{AUDIT_DENIED_CAPABILITY}', which the \
         attested opener must never reach; open replies {open_replies:?}, client frames \
         {client_frames:?}"
    );
    // Refused the way admission refuses a deny edge, and the target is told to
    // drop the binding it made.
    assert_eq!(open_replies.len(), 1, "{open_replies:?}");
    assert_eq!(
        parse_error(&open_replies[0])["code"],
        "capability_forbidden"
    );
    assert!(client_frames.is_empty(), "{client_frames:?}");
    let goodbye = target_rx
        .try_recv()
        .expect("the target is sent a GOODBYE for the refused bind")
        .frame;
    assert_eq!(goodbye.header.ty, FrameType::Goodbye);
    assert_ne!(goodbye.header.channel, 0);
}

/// A provider whose daemon egress queue is full (its reader has stalled) must
/// not hold `route.open` past the relay deadline: the open is answered once
/// the deadline passes, even though the route.bind never left the daemon.
#[tokio::test(start_paused = true)]
async fn route_open_is_answered_by_its_relay_deadline_while_module_egress_is_full() {
    const RELAY_TIMEOUT: Duration = Duration::from_millis(500);
    let handler = ControlHandler::with_forwarding(
        Arc::new(Registry::default()),
        Arc::new(ForwardingTable::default()),
    )
    .with_route_bind_relay_timeout(RELAY_TIMEOUT);
    let (module_ctx, mut module_rx) = route_ctx(ConnectionId::new(1_720));
    hello_via_sink(
        &handler,
        &module_ctx,
        &mut module_rx,
        hello_frame("stalled-provider", PROTOCOL_VERSION, 1),
    )
    .await;
    // The module stops reading: its egress queue fills and stays full, while
    // its receiver stays alive.
    let mut queued = 0u64;
    while module_ctx
        .egress
        .try_send(
            Frame::build(
                FrameType::Push,
                control_flags(),
                0,
                0,
                0,
                b"queued-before-the-open".to_vec(),
            )
            .unwrap(),
        )
        .is_ok()
    {
        queued += 1;
        assert!(queued < 10_000, "the module egress queue never filled");
    }

    let (client_ctx, _client_rx) = route_ctx(ConnectionId::new(1_721));
    let started = tokio::time::Instant::now();
    let open = tokio::spawn({
        let handler = handler.clone();
        async move {
            handler
                .handle_control_frame(
                    &client_ctx,
                    route_open_frame(2, "stalled-provider", unique_project_root("audit-egress")),
                )
                .await
        }
    });
    let answered = tokio::time::timeout(Duration::from_secs(30), open).await;
    let waited = started.elapsed();
    let replies = answered
        .unwrap_or_else(|_| {
            panic!(
                "route.open was still waiting to enqueue its route.bind {waited:?} after it \
                 started, past its {RELAY_TIMEOUT:?} relay deadline"
            )
        })
        .expect("route.open task joins")
        .expect("route.open returns frames");
    assert!(
        waited <= RELAY_TIMEOUT + Duration::from_millis(100),
        "route.open answered after {waited:?}, past its {RELAY_TIMEOUT:?} relay deadline"
    );
    assert_eq!(replies.len(), 1, "{replies:?}");
    assert_eq!(replies[0].header.ty, FrameType::Error);
    assert_eq!(parse_error(&replies[0])["code"], "module_timeout");
    // The expired bind was never queued, so the module never sees it.
    while let Ok(outbound) = module_rx.try_recv() {
        assert_eq!(
            outbound.frame.header.ty,
            FrameType::Push,
            "only the earlier traffic"
        );
    }
}

/// The other egress wait of an open: reserving room in the opener's own
/// queue for the route.open reply. A client that stopped reading must not hold
/// the open, or the target's relay slot, past the relay deadline either.
#[tokio::test(start_paused = true)]
async fn route_open_is_answered_by_its_relay_deadline_while_client_egress_is_full() {
    const RELAY_TIMEOUT: Duration = Duration::from_millis(500);
    let handler = ControlHandler::with_forwarding(
        Arc::new(Registry::default()),
        Arc::new(ForwardingTable::default()),
    )
    .with_route_bind_relay_timeout(RELAY_TIMEOUT);
    let (module_ctx, mut module_rx) = route_ctx(ConnectionId::new(1_722));
    hello_via_sink(
        &handler,
        &module_ctx,
        &mut module_rx,
        hello_frame("waiting-provider", PROTOCOL_VERSION, 1),
    )
    .await;
    let (client_ctx, _client_rx) = route_ctx(ConnectionId::new(1_723));
    while client_ctx
        .egress
        .try_send(Frame::build(FrameType::Push, control_flags(), 0, 0, 0, Vec::new()).unwrap())
        .is_ok()
    {}

    let started = tokio::time::Instant::now();
    let open = tokio::spawn({
        let handler = handler.clone();
        async move {
            handler
                .handle_control_frame(
                    &client_ctx,
                    route_open_frame(3, "waiting-provider", unique_project_root("audit-client")),
                )
                .await
        }
    });
    let replies = tokio::time::timeout(Duration::from_secs(30), open)
        .await
        .expect("route.open is answered at its relay deadline")
        .expect("route.open task joins")
        .expect("route.open returns frames");
    assert!(started.elapsed() <= RELAY_TIMEOUT + Duration::from_millis(100));
    assert_eq!(parse_error(&replies[0])["code"], "module_timeout");
    assert!(module_rx.try_recv().is_err(), "no route.bind was relayed");
}

fn audit_activity_frame(body: Value, corr: u64) -> Frame {
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

/// A client that starts following the activity ledger and cancels at once
/// must never receive activity on the abandoned correlation. The connection
/// reader hands `activity.subscribe` to a task and handles the next frame,
/// `CANCEL`, itself; this test does the same.
#[tokio::test]
async fn activity_cancel_sent_before_the_subscription_installs_ends_the_subscription() {
    let scratch = ScratchDir::new("audit-activity-cancel");
    let handler = ControlHandler::with_forwarding(
        Arc::new(Registry::default()),
        Arc::new(ForwardingTable::default()),
    )
    .with_supervisor(SupervisorHandle::new())
    .with_activity_ledger(scratch.path(), "audit".into());
    let (ctx, mut rx) = route_ctx(ConnectionId::new(1_730));

    let subscribe_frame = audit_activity_frame(serde_json::json!({"op": "activity.subscribe"}), 2);
    // The reader reserves the correlation before it hands the request off.
    let reservation = handler.reserve_activity_subscription(ctx.connection_id, &subscribe_frame);
    assert!(reservation.is_some(), "activity.subscribe is reserved");
    let subscribe = tokio::spawn({
        let handler = handler.clone();
        let ctx = ctx.clone();
        async move {
            let _reservation = reservation;
            handler.handle_control_frame(&ctx, subscribe_frame).await
        }
    });
    // The reader's next frame: CANCEL on the same correlation, handled inline
    // while the subscribe task has not run yet.
    let cancel_replies = handler
        .handle_control_frame(
            &ctx,
            Frame::build(FrameType::Cancel, control_flags(), 0, 0, 2, Vec::new()).unwrap(),
        )
        .await
        .unwrap();
    let cancel_codes = cancel_replies
        .iter()
        .map(|reply| parse_error(reply)["code"].clone())
        .collect::<Vec<_>>();
    let subscribe_replies = subscribe.await.unwrap().unwrap();

    handler
        .activity
        .as_ref()
        .unwrap()
        .record(
            "worker".into(),
            serde_json::from_value(serde_json::json!({
                "op": "activity.record",
                "kind": "worker.settled",
                "summary": "Worker settled"
            }))
            .unwrap(),
            subc_control::ActivityActor {
                principal: Principal::Direct,
                scope_owner: None,
                agent_id: None,
                run_id: None,
                flow_id: None,
                project_root: None,
                workdir: None,
                project_id: None,
            },
            7,
        )
        .await
        .unwrap();

    let mut delivered = Vec::new();
    while let Ok(Some(outbound)) = tokio::time::timeout(Duration::from_millis(500), rx.recv()).await
    {
        delivered.push((outbound.header.ty, outbound.header.corr));
    }
    assert!(
        !delivered
            .iter()
            .any(|(ty, corr)| *ty == FrameType::StreamData && *corr == 2),
        "the client cancelled correlation 2 (cancel replied {cancel_codes:?}; subscribe replied \
         {subscribe_replies:?}), yet a live feed installed afterwards and sent it {delivered:?}"
    );
    // The cancel won: it is acknowledged like any other, and the stream is
    // settled exactly once.
    assert!(cancel_codes.is_empty(), "{cancel_codes:?}");
    assert!(subscribe_replies.is_empty(), "{subscribe_replies:?}");
    assert_eq!(delivered, vec![(FrameType::StreamEnd, 2)]);
}

/// Only a capability-fenced bind settles under the registry lock. An ack for
/// an open with no attested opener must not take it, so registry readers are
/// not serialized behind forwarding during open bursts.
#[tokio::test]
async fn an_unfenced_bind_ack_settles_without_the_registry_lock() {
    let registry = Arc::new(Registry::default());
    let forwarding = Arc::new(ForwardingTable::default());
    let handler = ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding));
    let (target_ctx, mut target_rx) = route_ctx(ConnectionId::new(1_740));
    hello_via_sink(
        &handler,
        &target_ctx,
        &mut target_rx,
        hello_frame("unfenced-target", PROTOCOL_VERSION, 1),
    )
    .await;
    let (client_ctx, mut client_rx) = route_ctx(ConnectionId::new(1_741));
    let open = tokio::spawn({
        let handler = handler.clone();
        async move {
            handler
                .handle_control_frame(
                    &client_ctx,
                    route_open_frame(2, "unfenced-target", unique_project_root("audit-unfenced")),
                )
                .await
        }
    });
    let bind = tokio::time::timeout(Duration::from_secs(1), target_rx.recv())
        .await
        .expect("the open relays route.bind")
        .unwrap();
    let lock_attempts = registry.observe_next_lock_attempt();
    handler
        .handle_control_frame(&target_ctx, route_bind_ack(bind.header.corr))
        .await
        .unwrap();
    assert!(
        lock_attempts.try_recv().is_err(),
        "settling an unfenced bind took the registry lock"
    );
    assert!(open.await.unwrap().unwrap().is_empty());
    assert!(client_rx.recv().await.is_some(), "the route.open reply");
    assert_eq!(forwarding.active_binding_count().unwrap(), 1);
}

/// A CANCEL that claims a reservation ends the stream with STREAM_END, so a
/// subscribe refused afterwards (here: no activity ledger) must not send a
/// second terminal, its ERROR, on the same correlation.
#[tokio::test]
async fn a_cancelled_subscription_refused_later_ends_its_stream_once() {
    let handler = ControlHandler::with_forwarding(
        Arc::new(Registry::default()),
        Arc::new(ForwardingTable::default()),
    );
    let (ctx, mut rx) = route_ctx(ConnectionId::new(1_750));
    let subscribe_frame = audit_activity_frame(serde_json::json!({"op": "activity.subscribe"}), 2);
    let reservation = handler.reserve_activity_subscription(ctx.connection_id, &subscribe_frame);
    let subscribe = tokio::spawn({
        let handler = handler.clone();
        let ctx = ctx.clone();
        async move {
            let _reservation = reservation;
            handler.handle_control_frame(&ctx, subscribe_frame).await
        }
    });
    let cancel_replies = handler
        .handle_control_frame(
            &ctx,
            Frame::build(FrameType::Cancel, control_flags(), 0, 0, 2, Vec::new()).unwrap(),
        )
        .await
        .unwrap();
    assert!(
        cancel_replies.is_empty(),
        "the CANCEL claims the reservation"
    );
    let subscribe_replies = subscribe.await.unwrap().unwrap();
    assert!(
        subscribe_replies.is_empty(),
        "the refusal would be a second terminal after STREAM_END: {:?}",
        subscribe_replies
            .iter()
            .map(|reply| parse_error(reply)["code"].clone())
            .collect::<Vec<_>>()
    );
    let end = tokio::time::timeout(Duration::from_secs(1), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!((end.header.ty, end.header.corr), (FrameType::StreamEnd, 2));
}
