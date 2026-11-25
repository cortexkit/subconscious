// Regression tests for module-to-client splice defects found by a read-only
// audit of the router. Each test states the behaviour that should hold.

/// One module (registered with `concurrency`) and one client bound by one
/// route. The client's egress sink has `byte_budget` bytes, so a test can make
/// small frames fit while a larger one has to wait.
async fn audit_route(
    module_id: &str,
    concurrency: Concurrency,
    byte_budget: usize,
) -> (
    Arc<Router>,
    Arc<ForwardingTable>,
    crate::forwarding::ModuleEndpointId,
    mpsc::Receiver<OutboundFrame>,
    RouteCtx,
    crate::forwarding::PendingRouteBindRelay,
    mpsc::Receiver<OutboundFrame>,
) {
    let forwarding = Arc::new(ForwardingTable::default());
    let control = Arc::new(ControlHandler::with_forwarding(
        Arc::new(crate::Registry::default()),
        Arc::clone(&forwarding),
    ));
    let router = Arc::new(Router::with_control_handler(control));
    let module_connection = ConnectionId::new(1_100);
    let client_connection = ConnectionId::new(1_101);
    let (module_tx, module_rx) = mpsc::channel(8);
    let endpoint = forwarding
        .register_module_connection(
            module_connection,
            module_id.to_string(),
            1,
            concurrency,
            FrameSink::new(module_tx),
        )
        .unwrap();
    let (client_tx, mut client_rx) = mpsc::channel(64);
    let client_sink = FrameSink::with_byte_budget(client_tx, byte_budget);
    let route = forwarding
        .begin_route_bind_relay_for_test(client_connection, client_sink, 900, module_id)
        .unwrap();
    forwarding
        .complete_pending_relay(
            module_connection,
            route.corr,
            RouteBindRelayOutcome::Accepted,
        )
        .unwrap();
    assert_eq!(client_rx.recv().await.unwrap().header.corr, 900);
    let (module_egress_tx, _module_egress_rx) = mpsc::channel(8);
    let module_ctx = RouteCtx {
        connection_id: module_connection,
        egress: FrameSink::new(module_egress_tx),
    };
    (
        router, forwarding, endpoint, client_rx, module_ctx, route, module_rx,
    )
}

fn oversized_stream_chunk(route: &crate::forwarding::PendingRouteBindRelay, corr: u64) -> Frame {
    stream_frame(
        route.module_channel,
        route.module_epoch,
        corr,
        vec![b's'; crate::server::CONNECTION_EGRESS_BYTE_BUDGET - subc_protocol::HEADER_LEN + 1],
    )
}

#[tokio::test]
async fn delivered_request_keeps_stream_refusal_when_next_socket_write_blocks() {
    let (router, forwarding, _, mut client_replies, module, route, module_queue) =
        audit_route("writer-stall", Concurrency::ModuleManaged, crate::server::CONNECTION_EGRESS_BYTE_BUDGET).await;
    let binding = module_route_for_test(&router, &module, &route);
    let client = RouteCtx { connection_id: binding.client_connection_id, egress: binding.client_sink.clone() };
    let first = route_frame(FrameType::Request, route.client_channel, route.client_epoch, 401);
    let second = Frame::build(FrameType::Request, first.header.flags, route.client_channel, route.client_epoch, 402, vec![b'x'; 64 * 1024]).unwrap();
    router.route_for_connection(&client, first).await.unwrap();
    router.route_for_connection(&client, second).await.unwrap();
    let (socket, mut peer) = tokio::io::duplex(128);
    let writer = tokio::spawn(crate::server::drain_writer(socket, module_queue));
    assert_eq!(crate::read_frame(&mut peer).await.unwrap().unwrap().header.corr, 401);
    // Leave B unread so it occupies the writer; check that CANCEL for A
    // can enter the same queue without needing B to finish writing.
    let blocked = binding.module_sink.backlog().queued_frames;
    assert_eq!(blocked, 1);

    router.route_for_connection(&module, oversized_stream_chunk(&route, 401)).await.unwrap();
    assert_eq!(client_replies.try_recv().unwrap().header.ty, FrameType::Error);
    router.route_for_connection(&module, stream_frame(route.module_channel, route.module_epoch, 401, b"late".to_vec())).await.unwrap();
    assert!(client_replies.try_recv().is_err(), "the oversized chunk already ended this request at the client");
    assert_eq!(binding.module_sink.backlog().queued_frames, blocked + 1, "CANCEL is queued while B is blocked");
    assert!(!forwarding.upgrade_admission.borrow().fenced());
    writer.abort();
    let _ = writer.await;
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn upgrade_goodbye_waits_for_writer_slot_and_blocks_late_cancel_and_pong() {
    let (router, forwarding, _, mut client_replies, mut module, route, mut module_queue) =
        audit_route("carried-writer", Concurrency::ModuleManaged, crate::server::CONNECTION_EGRESS_BYTE_BUDGET).await;
    let binding = module_route_for_test(&router, &module, &route);
    module.egress = binding.module_sink.clone();
    binding.flow.acquire_tagged(401, false).await.unwrap();
    let slot = module.egress.reserve_owned().await.unwrap();
    forwarding.set_upgrade_admission(crate::control::upgrade_in_place::Admission::Fenced);
    let goodbye = module.egress.send_upgrade_goodbye(route_frame(FrameType::Goodbye, 0, 0, 0));
    tokio::pin!(goodbye);
    assert!(poll_once(goodbye.as_mut()).await.is_pending());
    assert!(module_queue.try_recv().is_err(), "GOODBYE jumped a writer slot already held by a request");
    assert!(!slot.send(route_frame(FrameType::Request, route.module_channel, route.module_epoch, 401)));
    assert_eq!(module_queue.try_recv().unwrap().header.ty, FrameType::Request);
    assert!(poll_once(goodbye.as_mut()).await.is_pending());
    let cut = module_queue.try_recv().unwrap();
    assert_eq!(cut.header.ty, FrameType::Goodbye);

    router.route_for_connection(&module, oversized_stream_chunk(&route, 401)).await.unwrap();
    assert_eq!(client_replies.try_recv().unwrap().header.ty, FrameType::Error);
    router.route_for_connection(&module, route_frame(FrameType::Ping, 0, 0, 9)).await.unwrap();
    assert!(module_queue.try_recv().is_err(), "no CANCEL or PONG may be queued behind upgrade GOODBYE");
    cut.flushed.unwrap().send(()).unwrap();
    goodbye.await.unwrap();
}

/// A module that emits one stream chunk larger than the client's byte budget
/// is refused with `frame_too_large`, but it is still computing the request.
/// On a Serial route the next request must not be admitted to the module until
/// the module itself ends the first one.
#[tokio::test]
async fn oversized_stream_chunk_keeps_the_serial_credit_until_the_module_ends_the_request() {
    let (router, _forwarding, _endpoint, mut client_rx, module_ctx, route, _module_rx) =
        audit_route(
            "oversize-serial-provider",
            Concurrency::Serial,
            crate::server::CONNECTION_EGRESS_BYTE_BUDGET,
        )
        .await;
    let binding = module_route_for_test(&router, &module_ctx, &route);
    binding.flow.acquire_tagged(401, false).await.unwrap();

    router
        .route_for_connection(&module_ctx, oversized_stream_chunk(&route, 401))
        .await
        .unwrap();
    assert_frame_size_error(
        &client_rx
            .try_recv()
            .expect("the client is told frame_too_large"),
        route.client_channel,
        route.client_epoch,
        401,
        crate::server::CONNECTION_EGRESS_BYTE_BUDGET + 1,
    );

    // The module has emitted no terminal for 401, so it is still running it.
    let held_after_refusal = binding.flow.drain_in_flight();
    let second = binding.flow.acquire_tagged(402, false);
    tokio::pin!(second);
    assert!(
        poll_once(second.as_mut()).await.is_pending(),
        "a second request entered the Serial route while the module was still running \
         request 401: the oversized STREAM_DATA chunk released 401's credit \
         (credits held after the refusal: {held_after_refusal})"
    );
    // The module's own terminal ends 401 and admits the waiting request.
    router
        .route_for_connection(
            &module_ctx,
            route_frame(
                FrameType::StreamEnd,
                route.module_channel,
                route.module_epoch,
                401,
            ),
        )
        .await
        .unwrap();
    assert!(poll_once(second.as_mut()).await.is_ready());
    assert!(
        client_rx.try_recv().is_err(),
        "the client already saw 401 end"
    );
}

/// After an oversized stream chunk ends the client's view of a request with
/// `frame_too_large`, the client must see nothing more on that correlation:
/// the module's later chunks and its STREAM_END are not delivered.
#[tokio::test]
async fn frames_after_an_oversized_stream_chunk_are_not_forwarded_to_the_client() {
    let (router, _forwarding, _endpoint, mut client_rx, module_ctx, route, mut module_rx) =
        audit_route(
            "oversize-continuation-provider",
            Concurrency::ModuleManaged,
            crate::server::CONNECTION_EGRESS_BYTE_BUDGET,
        )
        .await;
    let binding = module_route_for_test(&router, &module_ctx, &route);
    binding.flow.acquire_tagged(401, false).await.unwrap();

    router
        .route_for_connection(&module_ctx, oversized_stream_chunk(&route, 401))
        .await
        .unwrap();
    let error = client_rx
        .try_recv()
        .expect("the client is told frame_too_large");
    assert_eq!(error.header.ty, FrameType::Error);
    let cancel = module_rx
        .try_recv()
        .expect("the module is asked to stop the refused request");
    assert_eq!(
        (
            cancel.header.ty,
            cancel.header.channel,
            cancel.header.epoch,
            cancel.header.corr
        ),
        (
            FrameType::Cancel,
            route.module_channel,
            route.module_epoch,
            401
        )
    );

    // The module keeps going: one ordinary chunk, then its terminal.
    router
        .route_for_connection(
            &module_ctx,
            stream_frame(
                route.module_channel,
                route.module_epoch,
                401,
                b"late".to_vec(),
            ),
        )
        .await
        .unwrap();
    router
        .route_for_connection(
            &module_ctx,
            route_frame(
                FrameType::StreamEnd,
                route.module_channel,
                route.module_epoch,
                401,
            ),
        )
        .await
        .unwrap();

    let mut after_error = Vec::new();
    while let Ok(outbound) = client_rx.try_recv() {
        after_error.push((outbound.header.ty, outbound.header.corr));
    }
    assert!(
        after_error.is_empty(),
        "corr 401 already ended with frame_too_large, yet the client was sent {after_error:?}"
    );
    assert_eq!(
        binding.flow.drain_in_flight(),
        0,
        "the module's STREAM_END releases the credit"
    );
}

/// Stand in for the supervisor's restart drain from the moment it polls for
/// quiescence: when the endpoint holds no request credit it reports
/// `drained: true`, releases the endpoint's routes and sends each client a
/// route GOODBYE, as `wait_for_forwarding_quiescence` followed by
/// `send_route_goodbyes` do. Returns whether the drain completed.
fn drain_if_quiescent(
    forwarding: &ForwardingTable,
    endpoint: crate::forwarding::ModuleEndpointId,
) -> bool {
    if forwarding.endpoint_in_flight_count(endpoint).unwrap() != 0 {
        return false;
    }
    for target in forwarding.release_module_endpoint_routes(endpoint).unwrap() {
        let goodbye = Frame::build(
            FrameType::Goodbye,
            crate::control::control_flags(),
            target.channel,
            target.epoch,
            0,
            Vec::new(),
        )
        .unwrap();
        target
            .sink
            .try_send(goodbye)
            .expect("the small route GOODBYE fits the client's remaining budget");
    }
    true
}

/// The last call of a draining module completes while its ordinary-sized
/// response cannot yet fit the client's byte budget. The drain must not
/// report quiescence and send the route GOODBYE ahead of that response: the
/// client would settle the call as closed and then discard the reply.
#[tokio::test(start_paused = true)]
async fn restart_drain_delivers_a_completed_reply_before_the_route_goodbye() {
    const BUDGET: usize = 4_096;
    const ROOM_LEFT: usize = 600;
    let (router, forwarding, endpoint, mut client_rx, module_ctx, route, _module_rx) =
        audit_route("drain-reply-provider", Concurrency::ModuleManaged, BUDGET).await;
    let binding = module_route_for_test(&router, &module_ctx, &route);
    binding.flow.acquire_tagged(401, false).await.unwrap();

    // Earlier traffic the client has not read yet leaves room for a header-only
    // GOODBYE but not for the response below.
    binding
        .client_sink
        .try_send(stream_frame(
            route.client_channel,
            route.client_epoch,
            300,
            vec![b'q'; BUDGET - ROOM_LEFT - subc_protocol::HEADER_LEN],
        ))
        .unwrap();
    let response = Frame::build(
        FrameType::Response,
        Flags::new(false, Priority::Interactive, false),
        route.module_channel,
        route.module_epoch,
        401,
        vec![b'r'; 1_500],
    )
    .unwrap();
    let delivery = tokio::spawn({
        let router = Arc::clone(&router);
        let module_ctx = module_ctx.clone();
        async move { router.route_for_connection(&module_ctx, response).await }
    });
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    assert!(
        !delivery.is_finished(),
        "the response must still be waiting for client egress room"
    );

    let mut drained = drain_if_quiescent(&forwarding, endpoint);

    // The client resumes reading: the old traffic leaves, the response fits.
    let earlier = client_rx.recv().await.unwrap();
    assert_eq!(earlier.header.corr, 300);
    drop(earlier);
    delivery.await.unwrap().unwrap();
    if !drained {
        drained = drain_if_quiescent(&forwarding, endpoint);
    }
    assert!(
        drained,
        "the drain completes once the reply has been queued"
    );

    let mut arrived = Vec::new();
    while let Ok(outbound) = client_rx.try_recv() {
        arrived.push((outbound.header.ty, outbound.header.corr));
    }
    let response_at = arrived
        .iter()
        .position(|(ty, corr)| *ty == FrameType::Response && *corr == 401);
    let goodbye_at = arrived.iter().position(|(ty, _)| *ty == FrameType::Goodbye);
    assert!(
        matches!((response_at, goodbye_at), (Some(response), Some(goodbye)) if response < goodbye),
        "the drain saw zero in-flight credits while corr 401's response still waited for client \
         egress, so the route GOODBYE reached the client first: {arrived:?}"
    );
}

/// On a legacy Serial route, a request that must wait for the route's only
/// credit is queued rather than holding the connection reader: the client's
/// answer to the module's reverse question still reaches the module at once,
/// and the waiting request and its CANCEL follow, in order, when the running
/// request ends.
#[tokio::test]
async fn legacy_serial_route_queues_a_waiting_request_without_holding_the_reader() {
    let (router, _forwarding, _endpoint, _client_rx, module_ctx, route, mut module_rx) =
        audit_route(
            "legacy-queue-provider",
            Concurrency::Serial,
            crate::server::CONNECTION_EGRESS_BYTE_BUDGET,
        )
        .await;
    let (client_tx, _client_error_rx) = mpsc::channel(8);
    let client_ctx = RouteCtx {
        connection_id: ConnectionId::new(1_101),
        egress: FrameSink::new(client_tx),
    };
    let client = |ty, corr| route_frame(ty, route.client_channel, route.client_epoch, corr);
    for (ty, corr) in [
        (FrameType::Request, 10),
        (FrameType::Request, 11),
        (FrameType::Cancel, 11),
        (FrameType::Response, 77),
    ] {
        tokio::time::timeout(
            Duration::from_secs(1),
            router.route_for_connection(&client_ctx, client(ty, corr)),
        )
        .await
        .unwrap_or_else(|_| panic!("the reader was held by {ty:?} {corr}"))
        .unwrap();
    }
    let mut reached = Vec::new();
    while let Ok(outbound) = module_rx.try_recv() {
        reached.push((outbound.header.ty, outbound.header.corr));
    }
    assert_eq!(
        reached,
        vec![(FrameType::Request, 10), (FrameType::Response, 77)],
        "only the running request and the reverse answer reach the module"
    );

    // The module ends request 10; the waiting request, then its CANCEL, go.
    router
        .route_for_connection(
            &module_ctx,
            route_frame(
                FrameType::Response,
                route.module_channel,
                route.module_epoch,
                10,
            ),
        )
        .await
        .unwrap();
    for expected in [(FrameType::Request, 11), (FrameType::Cancel, 11)] {
        let next = tokio::time::timeout(Duration::from_secs(1), module_rx.recv())
            .await
            .expect("the waiting frames are forwarded once credit frees")
            .unwrap();
        assert_eq!((next.header.ty, next.header.corr), expected);
    }
}

/// A CANCEL for a request still waiting for credit goes directly behind that
/// request, not behind later requests: once the running request ends, the
/// module gets the waiting request and then its CANCEL, while the later
/// request keeps waiting for credit the CANCEL needs none of.
#[tokio::test]
async fn cancel_of_a_waiting_request_follows_it_ahead_of_later_requests() {
    let (router, _forwarding, _endpoint, _client_rx, module_ctx, route, mut module_rx) =
        audit_route(
            "legacy-cancel-provider",
            Concurrency::Serial,
            crate::server::CONNECTION_EGRESS_BYTE_BUDGET,
        )
        .await;
    let (client_tx, _client_error_rx) = mpsc::channel(8);
    let client_ctx = RouteCtx {
        connection_id: ConnectionId::new(1_101),
        egress: FrameSink::new(client_tx),
    };
    let client = |ty, corr| route_frame(ty, route.client_channel, route.client_epoch, corr);
    for (ty, corr) in [
        (FrameType::Request, 10),
        (FrameType::Request, 11),
        (FrameType::Request, 12),
        (FrameType::Cancel, 11),
    ] {
        router
            .route_for_connection(&client_ctx, client(ty, corr))
            .await
            .unwrap();
    }
    assert_eq!(
        module_rx.recv().await.unwrap().header.corr,
        10,
        "request 10 runs"
    );
    router
        .route_for_connection(
            &module_ctx,
            route_frame(
                FrameType::Response,
                route.module_channel,
                route.module_epoch,
                10,
            ),
        )
        .await
        .unwrap();
    for expected in [(FrameType::Request, 11), (FrameType::Cancel, 11)] {
        let next = tokio::time::timeout(Duration::from_secs(1), module_rx.recv())
            .await
            .unwrap_or_else(|_| panic!("{expected:?} never reached the module"))
            .unwrap();
        assert_eq!((next.header.ty, next.header.corr), expected);
    }
    assert!(
        module_rx.try_recv().is_err(),
        "request 12 still waits for the credit request 11 holds"
    );
}

/// A request the forwarding task is still trying to put on a stalled
/// module's queue must not reach the module once its route is released.
#[tokio::test]
async fn a_waiting_request_never_reaches_the_module_after_its_route_is_released() {
    let (router, forwarding, _endpoint, _client_rx, module_ctx, route, mut module_rx) =
        audit_route(
            "legacy-release-provider",
            Concurrency::Serial,
            crate::server::CONNECTION_EGRESS_BYTE_BUDGET,
        )
        .await;
    let binding = module_route_for_test(&router, &module_ctx, &route);
    let (client_tx, _client_error_rx) = mpsc::channel(8);
    let client_ctx = RouteCtx {
        connection_id: ConnectionId::new(1_101),
        egress: FrameSink::new(client_tx),
    };
    let client = |corr| {
        route_frame(
            FrameType::Request,
            route.client_channel,
            route.client_epoch,
            corr,
        )
    };
    router
        .route_for_connection(&client_ctx, client(10))
        .await
        .unwrap();
    router
        .route_for_connection(&client_ctx, client(11))
        .await
        .unwrap();
    // The module stops reading: its queue fills.
    while binding
        .module_sink
        .try_send(
            Frame::build(
                FrameType::Push,
                Flags::new(false, Priority::Passive, false),
                0,
                0,
                0,
                Vec::new(),
            )
            .unwrap(),
        )
        .is_ok()
    {}
    // Request 10 ends; the task takes the credit for 11 and waits for room.
    router
        .route_for_connection(
            &module_ctx,
            route_frame(
                FrameType::Response,
                route.module_channel,
                route.module_epoch,
                10,
            ),
        )
        .await
        .unwrap();
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    forwarding
        .release_client_route(
            ConnectionId::new(1_101),
            route.client_channel,
            route.client_epoch,
        )
        .unwrap();
    // The module resumes reading.
    let mut reached = Vec::new();
    for _ in 0..4 {
        while let Ok(outbound) = module_rx.try_recv() {
            reached.push((outbound.header.ty, outbound.header.corr));
        }
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
    }
    assert!(
        !reached.contains(&(FrameType::Request, 11)),
        "request 11 reached the module after its route was released: {reached:?}"
    );
}

/// Records the route's held credits whenever the delivery-failure path logs
/// that it is closing the client connection.
#[derive(Clone)]
struct HeldAtEscalation {
    flow: Arc<crate::forwarding::ChannelFlow>,
    held: Arc<std::sync::Mutex<Vec<usize>>>,
}

impl std::io::Write for HeldAtEscalation {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        if String::from_utf8_lossy(buffer).contains("closing client connection") {
            self.held.lock().unwrap().push(self.flow.drain_in_flight());
        }
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A final reply the client can no longer take is settled by the
/// delivery-failure path. Its credit must still be held while that path marks
/// the client closing, so a drain polling in between cannot report the
/// request finished and `drained: true` before the failure is owned.
#[tokio::test]
async fn a_failed_final_reply_keeps_its_credit_until_the_client_close_is_requested() {
    let (router, forwarding, _endpoint, _client_rx, module_ctx, route, _module_rx) = audit_route(
        "failed-reply-provider",
        Concurrency::ModuleManaged,
        crate::server::CONNECTION_EGRESS_BYTE_BUDGET,
    )
    .await;
    let binding = module_route_for_test(&router, &module_ctx, &route);
    binding.flow.acquire_tagged(401, false).await.unwrap();
    let _close = forwarding.register_connection_close(ConnectionId::new(1_101));
    binding.client_sink.close_client_delivery();
    let probe = HeldAtEscalation {
        flow: Arc::clone(&binding.flow),
        held: Arc::new(std::sync::Mutex::new(Vec::new())),
    };
    let held = Arc::clone(&probe.held);
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(move || probe.clone())
        .finish();
    let _subscriber = tracing::subscriber::set_default(subscriber);

    router
        .route_for_connection(
            &module_ctx,
            route_frame(
                FrameType::Response,
                route.module_channel,
                route.module_epoch,
                401,
            ),
        )
        .await
        .unwrap();
    assert_eq!(
        *held.lock().unwrap(),
        vec![1],
        "credits held when the client close was requested"
    );
    assert_eq!(binding.flow.drain_in_flight(), 0, "and retired after it");
}
