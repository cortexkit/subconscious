const NO_TERMINAL: &str = "cancelled module request has no terminal";

async fn reported_stalled_cancels(
    router: &Router,
    client: &RouteCtx,
    replies: &mut mpsc::Receiver<OutboundFrame>,
) -> serde_json::Value {
    router
        .route_for_connection(client, request(0, 99, br#"{"op":"server.describe"}"#))
        .await
        .unwrap();
    let reply = replies.try_recv().unwrap();
    let body: serde_json::Value = serde_json::from_slice(&reply.body).unwrap();
    body["counters"]["stalled_cancels"].clone()
}

#[tokio::test(start_paused = true)]
async fn unanswered_subscription_cancel_warns_once_and_keeps_its_credit() {
    let (output, _guard) = log_capture(tracing::Level::WARN);
    let (router, forwarding, client, mut replies, module, _, mut requests, route) =
        dynamic_route_fixture_with_busy(true, true);
    let binding = module_route_for_test(&router, &module, &route);
    let _monitor = forwarding.spawn_stalled_cancel_monitor();
    let _ = replies.try_recv().unwrap();
    let mut subscribe = route_frame(
        FrameType::Request,
        route.client_channel,
        route.client_epoch,
        701,
    );
    subscribe.header.flags.0 |= subc_protocol::FLAG_SUBSCRIPTION;
    let cancel = route_frame(
        FrameType::Cancel,
        route.client_channel,
        route.client_epoch,
        701,
    );
    router
        .route_for_connection(&client, subscribe)
        .await
        .unwrap();
    assert!(requests.try_recv().unwrap().header.flags.is_subscription());
    router
        .route_for_connection(&client, cancel.clone())
        .await
        .unwrap();
    assert_eq!(requests.try_recv().unwrap().header.ty, FrameType::Cancel);
    assert_eq!(binding.flow.in_flight(), 1);

    tokio::time::advance(Duration::from_secs(20)).await;
    router
        .route_for_connection(&client, cancel.clone())
        .await
        .unwrap();
    assert_eq!(requests.try_recv().unwrap().header.ty, FrameType::Cancel);
    tokio::time::advance(Duration::from_secs(9)).await;
    assert!(!captured_logs(&output).contains(NO_TERMINAL));
    assert_eq!(
        reported_stalled_cancels(&router, &client, &mut replies).await,
        0
    );

    tokio::time::advance(Duration::from_millis(1_001)).await;
    tokio::task::yield_now().await;
    let logs = captured_logs(&output);
    let line = logs
        .lines()
        .find(|line| line.contains(NO_TERMINAL))
        .unwrap_or_else(|| panic!("missing warning: {logs}"));
    for field in [
        "WARN".into(),
        "module_id=epoch-router".into(),
        format!("module_channel={}", route.module_channel),
        format!("module_epoch={}", route.module_epoch),
        "corr=701".into(),
        "subscription=true".into(),
        "seconds_since_cancel=30".into(),
    ] {
        assert!(line.contains(&field), "missing {field}: {line}");
    }
    assert_eq!(
        reported_stalled_cancels(&router, &client, &mut replies).await,
        1
    );
    assert_eq!(binding.flow.in_flight(), 1);

    router.route_for_connection(&client, cancel).await.unwrap();
    assert_eq!(requests.try_recv().unwrap().header.ty, FrameType::Cancel);
    tokio::time::advance(Duration::from_secs(60)).await;
    tokio::task::yield_now().await;
    let logs = captured_logs(&output);
    assert_eq!(logs.matches(NO_TERMINAL).count(), 1, "{logs}");
    assert_eq!(
        reported_stalled_cancels(&router, &client, &mut replies).await,
        1
    );
    assert_eq!(binding.flow.in_flight(), 1);

    router
        .route_for_connection(
            &module,
            route_frame(
                FrameType::StreamEnd,
                route.module_channel,
                route.module_epoch,
                701,
            ),
        )
        .await
        .unwrap();
    assert_eq!(replies.try_recv().unwrap().header.ty, FrameType::StreamEnd);
    assert_eq!(binding.flow.in_flight(), 0);
    assert_eq!(
        reported_stalled_cancels(&router, &client, &mut replies).await,
        0
    );
}

#[tokio::test(start_paused = true)]
async fn terminal_within_cancel_grace_keeps_credit_until_reply_and_never_reports() {
    let (output, _guard) = log_capture(tracing::Level::WARN);
    let (router, forwarding, client, mut replies, module, _, mut requests, route) =
        dynamic_route_fixture(true);
    let binding = module_route_for_test(&router, &module, &route);
    let _monitor = forwarding.spawn_stalled_cancel_monitor();
    let _ = replies.try_recv().unwrap();
    for ty in [FrameType::Request, FrameType::Cancel] {
        router
            .route_for_connection(
                &client,
                route_frame(ty, route.client_channel, route.client_epoch, 701),
            )
            .await
            .unwrap();
        assert_eq!(requests.try_recv().unwrap().header.ty, ty);
    }
    tokio::time::advance(Duration::from_secs(29)).await;
    assert_eq!(binding.flow.in_flight(), 1);
    router
        .route_for_connection(
            &module,
            route_frame(
                FrameType::Response,
                route.module_channel,
                route.module_epoch,
                701,
            ),
        )
        .await
        .unwrap();
    assert_eq!(replies.try_recv().unwrap().header.ty, FrameType::Response);
    assert_eq!(binding.flow.in_flight(), 0);

    // Reusing the correlation after the terminal must not inherit the old deadline.
    router
        .route_for_connection(
            &client,
            route_frame(
                FrameType::Request,
                route.client_channel,
                route.client_epoch,
                701,
            ),
        )
        .await
        .unwrap();
    assert_eq!(requests.try_recv().unwrap().header.ty, FrameType::Request);
    tokio::time::advance(Duration::from_secs(60)).await;
    tokio::task::yield_now().await;
    assert!(!captured_logs(&output).contains(NO_TERMINAL));
    assert_eq!(
        reported_stalled_cancels(&router, &client, &mut replies).await,
        0
    );
    assert_eq!(binding.flow.in_flight(), 1);
}

#[tokio::test(start_paused = true)]
async fn refused_stream_cancel_is_reported_until_route_closes() {
    let (output, _guard) = log_capture(tracing::Level::WARN);
    let (router, forwarding, _, mut replies, module, route, mut requests) = audit_route(
        "oversize-provider",
        Concurrency::Serial,
        crate::server::CONNECTION_EGRESS_BYTE_BUDGET,
    )
    .await;
    let binding = module_route_for_test(&router, &module, &route);
    let client = RouteCtx {
        connection_id: binding.client_connection_id,
        egress: binding.client_sink.clone(),
    };
    let _monitor = forwarding.spawn_stalled_cancel_monitor();
    binding.flow.acquire_tagged(401, false).await.unwrap();
    router
        .route_for_connection(&module, oversized_stream_chunk(&route, 401))
        .await
        .unwrap();
    assert_eq!(replies.try_recv().unwrap().header.ty, FrameType::Error);
    assert_eq!(requests.try_recv().unwrap().header.ty, FrameType::Cancel);
    tokio::time::advance(Duration::from_secs(31)).await;
    tokio::task::yield_now().await;
    let logs = captured_logs(&output);
    assert_eq!(logs.matches(NO_TERMINAL).count(), 1, "{logs}");
    assert!(logs.contains("subscription=false"), "{logs}");
    assert_eq!(
        reported_stalled_cancels(&router, &client, &mut replies).await,
        1
    );
    assert_eq!(binding.flow.in_flight(), 1);

    router
        .route_for_connection(
            &client,
            route_frame(
                FrameType::Goodbye,
                route.client_channel,
                route.client_epoch,
                0,
            ),
        )
        .await
        .unwrap();
    assert_eq!(
        reported_stalled_cancels(&router, &client, &mut replies).await,
        0
    );
}
