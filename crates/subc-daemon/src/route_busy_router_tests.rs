struct BusyRouterRig {
    router: Arc<Router>,
    client: RouteCtx,
    client_rx: mpsc::Receiver<OutboundFrame>,
    module: RouteCtx,
    module_rx: mpsc::Receiver<OutboundFrame>,
    pending: crate::forwarding::PendingRouteBindRelay,
    _module_egress_rx: mpsc::Receiver<OutboundFrame>,
}

fn busy_router_rig(opted_in: bool) -> BusyRouterRig {
    let (router, _forwarding, client, client_rx, module, module_egress_rx, module_rx, pending) =
        dynamic_route_fixture_with_busy(true, opted_in);
    BusyRouterRig {
        router: Arc::new(router),
        client,
        client_rx,
        module,
        module_rx,
        pending,
        _module_egress_rx: module_egress_rx,
    }
}

fn busy_router_request(rig: &BusyRouterRig, corr: u64, watch: bool) -> Frame {
    let mut frame = route_frame(
        FrameType::Request,
        rig.pending.client_channel,
        rig.pending.client_epoch,
        corr,
    );
    if watch {
        frame.header.flags.0 |= subc_protocol::FLAG_SUBSCRIPTION;
    }
    frame
}

async fn busy_router_receive(rx: &mut mpsc::Receiver<OutboundFrame>) -> Frame {
    tokio::time::timeout(Duration::from_secs(1), rx.recv())
        .await
        .expect("expected frame was not delivered promptly")
        .expect("connection closed")
        .frame
}

#[tokio::test]
async fn route_busy_refusal_has_detail_origin_and_returns_ok_without_server_warning() {
    let logs = Arc::new(std::sync::Mutex::new(Vec::new()));
    let writer = BusyLogWriter(Arc::clone(&logs));
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    let _subscriber = tracing::subscriber::set_default(subscriber);
    let mut rig = busy_router_rig(true);
    busy_router_receive(&mut rig.client_rx).await;
    for corr in 1..=32 {
        rig.router
            .route_for_connection(&rig.client, busy_router_request(&rig, corr, false))
            .await
            .unwrap();
        assert_eq!(
            busy_router_receive(&mut rig.module_rx).await.header.corr,
            corr
        );
    }
    for corr in 33..=34 {
        tokio::time::timeout(
            Duration::from_millis(100),
            rig.router
                .route_for_connection(&rig.client, busy_router_request(&rig, corr, false)),
        )
        .await
        .expect("full opted-in route must not wait")
        .expect("busy refusal is handled, not a server routing error");
        let error = busy_router_receive(&mut rig.client_rx).await;
        assert_eq!(
            (error.header.channel, error.header.epoch, error.header.corr),
            (rig.pending.client_channel, rig.pending.client_epoch, corr)
        );
        assert!(error.header.flags.is_daemon_origin());
        let body: ErrorBody = serde_json::from_slice(&error.body).unwrap();
        assert_eq!(body.code, error_codes::ROUTE_BUSY);
        assert_eq!(
            body.detail.unwrap(),
            serde_json::json!({"reason":"no_credit", "request_in_flight":32, "watch_in_flight":0, "request_window":32, "watch_window":24})
        );
    }
    assert!(
        rig.module_rx.try_recv().is_err(),
        "refused requests were never forwarded"
    );
    let logs = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
    assert_eq!(
        logs.matches("request refused: route_busy").count(),
        1,
        "busy WARN is rate-limited per route: {logs}"
    );
    assert!(
        !logs.contains("routing failure recovered with ERROR frame"),
        "{logs}"
    );
    assert_eq!(
        rig.router.counters.snapshot()["route_busy_by_module_principal"]["epoch-router"]["direct"],
        2
    );
    rig.router
        .route_for_connection(
            &rig.client,
            route_frame(
                FrameType::Cancel,
                rig.pending.client_channel,
                rig.pending.client_epoch,
                33,
            ),
        )
        .await
        .unwrap();
    assert_eq!(
        busy_router_receive(&mut rig.module_rx).await.header.ty,
        FrameType::Cancel
    );
}

#[tokio::test]
async fn route_busy_watch_cancel_keeps_credit_until_terminal_and_releases_once() {
    let mut rig = busy_router_rig(true);
    busy_router_receive(&mut rig.client_rx).await;
    for corr in 1..=24 {
        rig.router
            .route_for_connection(&rig.client, busy_router_request(&rig, corr, true))
            .await
            .unwrap();
        busy_router_receive(&mut rig.module_rx).await;
    }
    rig.router
        .route_for_connection(
            &rig.client,
            route_frame(
                FrameType::Cancel,
                rig.pending.client_channel,
                rig.pending.client_epoch,
                1,
            ),
        )
        .await
        .unwrap();
    assert_eq!(
        busy_router_receive(&mut rig.module_rx).await.header.ty,
        FrameType::Cancel
    );
    rig.router
        .route_for_connection(&rig.client, busy_router_request(&rig, 25, true))
        .await
        .unwrap();
    let refused: ErrorBody =
        serde_json::from_slice(&busy_router_receive(&mut rig.client_rx).await.body).unwrap();
    assert_eq!(refused.detail.unwrap()["watch_in_flight"], 24);
    let terminal = route_frame(
        FrameType::Error,
        rig.pending.module_channel,
        rig.pending.module_epoch,
        1,
    );
    rig.router
        .route_for_connection(&rig.module, terminal.clone())
        .await
        .unwrap();
    busy_router_receive(&mut rig.client_rx).await;
    rig.router
        .route_for_connection(&rig.client, busy_router_request(&rig, 26, true))
        .await
        .unwrap();
    assert_eq!(
        busy_router_receive(&mut rig.module_rx).await.header.corr,
        26
    );
    rig.router
        .route_for_connection(&rig.module, terminal)
        .await
        .unwrap();
    busy_router_receive(&mut rig.client_rx).await;
    rig.router
        .route_for_connection(&rig.client, busy_router_request(&rig, 27, true))
        .await
        .unwrap();
    let refused: ErrorBody =
        serde_json::from_slice(&busy_router_receive(&mut rig.client_rx).await.body).unwrap();
    assert_eq!(refused.detail.unwrap()["watch_in_flight"], 24);
    rig.router
        .route_for_connection(&rig.client, busy_router_request(&rig, 28, false))
        .await
        .unwrap();
    assert_eq!(
        busy_router_receive(&mut rig.module_rx).await.header.corr,
        28
    );
}

#[derive(Clone)]
struct BusyLogWriter(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for BusyLogWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test(start_paused = true)]
async fn route_busy_legacy_credit_wait_stays_pending_and_is_logged_and_counted() {
    let logs = Arc::new(std::sync::Mutex::new(Vec::new()));
    let writer = BusyLogWriter(Arc::clone(&logs));
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    let _subscriber = tracing::subscriber::set_default(subscriber);
    let mut rig = busy_router_rig(false);
    let open: serde_json::Value =
        serde_json::from_slice(&busy_router_receive(&mut rig.client_rx).await.body).unwrap();
    assert!(open.get("request_window").is_none());
    assert!(open.get("watch_window").is_none());
    for corr in 1..=32 {
        rig.router
            .route_for_connection(&rig.client, busy_router_request(&rig, corr, true))
            .await
            .unwrap();
        busy_router_receive(&mut rig.module_rx).await;
    }
    let request = busy_router_request(&rig, 33, false);
    let router = Arc::clone(&rig.router);
    let client = rig.client.clone();
    let pending = tokio::spawn(async move { router.route_for_connection(&client, request).await });
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(6)).await;
    tokio::task::yield_now().await;
    assert!(
        !pending.is_finished(),
        "legacy request must keep waiting, not be refused"
    );
    assert!(rig.client_rx.try_recv().is_err());
    assert_eq!(
        rig.router.counters.snapshot()["reader_wait_over_5s_by_module_principal"]["epoch-router"]
            ["direct"]["legacy_credit"],
        1
    );
    let logs = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
    assert!(
        logs.contains("connection reader wait exceeded 5 seconds"),
        "{logs}"
    );
    assert!(logs.contains("legacy_credit"), "{logs}");
    rig.router
        .route_for_connection(
            &rig.module,
            route_frame(
                FrameType::StreamEnd,
                rig.pending.module_channel,
                rig.pending.module_epoch,
                1,
            ),
        )
        .await
        .unwrap();
    pending.await.unwrap().unwrap();
    assert_eq!(
        busy_router_receive(&mut rig.module_rx).await.header.corr,
        33
    );
}

#[tokio::test]
async fn route_busy_pending_bind_installs_policy_and_reply_windows_atomically() {
    let (router, forwarding, client, mut client_rx, module, _module_egress_rx, _module_rx, pending) =
        dynamic_route_fixture_with_busy(false, true);
    assert!(matches!(
        forwarding
            .lookup_data_route(
                client.connection_id,
                pending.client_channel,
                pending.client_epoch
            )
            .unwrap(),
        DataRoute::Client(DataRouteState::Reserved)
    ));
    assert!(client_rx.try_recv().is_err());
    forwarding
        .complete_pending_relay(
            module.connection_id,
            pending.corr,
            RouteBindRelayOutcome::Accepted,
        )
        .unwrap();
    let open: subc_control::ClientControlResponse =
        serde_json::from_slice(&busy_router_receive(&mut client_rx).await.body).unwrap();
    assert!(matches!(
        open,
        subc_control::ClientControlResponse::RouteOpen {
            request_window: Some(32),
            watch_window: Some(24),
            ..
        }
    ));
    let DataRoute::Client(DataRouteState::Bound(route)) = forwarding
        .lookup_data_route(
            client.connection_id,
            pending.client_channel,
            pending.client_epoch,
        )
        .unwrap()
    else {
        panic!("route must be bound");
    };
    assert!(route.flow.busy_refusals());
    assert_eq!(
        router
            .counters
            .snapshot()
            .get("route_busy_by_module_principal"),
        None
    );
}

#[tokio::test]
async fn route_busy_closed_full_binding_returns_reloading_or_stale_not_backend_or_busy() {
    for draining in [false, true] {
        let mut rig = busy_router_rig(true);
        busy_router_receive(&mut rig.client_rx).await;
        for corr in 1..=32 {
            rig.router
                .route_for_connection(&rig.client, busy_router_request(&rig, corr, false))
                .await
                .unwrap();
            busy_router_receive(&mut rig.module_rx).await;
        }
        let DataRoute::Client(DataRouteState::Bound(route)) = rig
            .router
            .forwarding
            .lookup_data_route(
                rig.client.connection_id,
                rig.pending.client_channel,
                rig.pending.client_epoch,
            )
            .unwrap()
        else {
            panic!("expected live route");
        };
        if draining {
            rig.router
                .forwarding
                .begin_module_drain("epoch-router", subc_protocol::RouteCloseReason::Reload)
                .unwrap();
        } else {
            rig.router
                .forwarding
                .release_client_route(
                    rig.client.connection_id,
                    rig.pending.client_channel,
                    rig.pending.client_epoch,
                )
                .unwrap();
        }
        let error = rig
            .router
            .forward_backend
            .handle_bound(busy_router_request(&rig, 33, false), route)
            .await
            .unwrap_err();
        if draining {
            assert!(
                matches!(error, RouterError::RouteError { ref code, .. } if code == error_codes::MODULE_RELOADING),
                "{error:?}"
            );
        } else {
            assert!(
                matches!(error, RouterError::StaleRouteEpoch { .. }),
                "{error:?}"
            );
        }
    }
}
