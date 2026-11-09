#[tokio::test]
async fn supervisor_list_refuses_excess_detached_work_before_spawning_it() {
    let handler = ControlHandler::new(Arc::new(Registry::default()));
    let _held = Arc::clone(&handler.list_admission)
        .acquire_many_owned(4)
        .await
        .unwrap();
    let (ctx, mut wire) = route_ctx(ConnectionId::new(902));
    let request =
        |corr| Frame::build(FrameType::Request, control_flags(), 0, 0, corr, Vec::new()).unwrap();
    let reply = handler
        .handle_client_control_request(
            &ctx,
            request(1),
            ClientControlRequest::SupervisorList {},
            &mut None,
        )
        .await
        .unwrap();
    assert!(
        reply.is_empty(),
        "busy list replies cannot return through the blocking reader egress path"
    );
    let reply = wire.recv().await.unwrap();
    let error: ErrorBody = serde_json::from_slice(&reply.body).unwrap();
    assert_eq!(error.code, "busy");
    let next = handler
        .handle_client_control_request(
            &ctx,
            request(2),
            ClientControlRequest::SupervisorSpawnSnapshot {},
            &mut None,
        )
        .await
        .unwrap();
    assert_eq!(
        next.len(),
        1,
        "other control traffic remains readable at list capacity"
    );
}

#[tokio::test]
async fn list_busy_refusals_with_full_egress_still_dispatch_the_following_goodbye() {
    let registry = Arc::new(Registry::default());
    let handler = Arc::new(ControlHandler::new(Arc::clone(&registry)));
    let router = Router::with_control_handler(Arc::clone(&handler));
    let connection = router.begin_connection();
    let (sender, mut receiver) = mpsc::channel(1);
    let ctx = RouteCtx {
        connection_id: connection.id(),
        egress: crate::router::FrameSink::new(sender),
    };
    // supervisor.list is a consumer operation; registered module endpoints
    // reject it before admission, so exercise the real consumer route here.
    let _held = Arc::clone(&handler.list_admission)
        .acquire_many_owned(4)
        .await
        .unwrap();
    ctx.egress
        .send(Frame::build(FrameType::Pong, control_flags(), 0, 0, 0, Vec::new()).unwrap())
        .await
        .unwrap();
    for corr in 1..=12 {
        let frame = Frame::build(
            FrameType::Request,
            control_flags(),
            0,
            0,
            corr,
            br#"{"op":"supervisor.list"}"#.to_vec(),
        )
        .unwrap();
        tokio::time::timeout(
            Duration::from_millis(20),
            router.route_for_connection(&ctx, frame),
        )
        .await
        .expect("busy list delivery cannot park the reader on a full writer")
        .unwrap();
    }
    assert_eq!(
        handler.counters.snapshot()["agent_command_refusals_dropped_unlabelled"],
        8
    );
    let goodbye = Frame::build(FrameType::Goodbye, control_flags(), 0, 0, 200, Vec::new()).unwrap();
    tokio::time::timeout(
        Duration::from_millis(20),
        router.route_for_connection(&ctx, goodbye),
    )
    .await
    .expect("the frame after list overflow must still be read")
    .unwrap();
    assert!(registry.list_modules().unwrap().1.is_empty());
    assert_eq!(receiver.try_recv().unwrap().header.corr, 0);
}

#[tokio::test]
async fn duplicate_or_closed_declarations_do_not_park_the_real_reader_or_close_its_session() {
    for closed in [false, true] {
        let registry = Arc::new(Registry::default());
        let handler = Arc::new(ControlHandler::new(Arc::clone(&registry)));
        let router = Router::with_control_handler(Arc::clone(&handler));
        let connection = router.begin_connection();
        let (sender, mut receiver) = mpsc::channel(1);
        let ctx = RouteCtx {
            connection_id: connection.id(),
            egress: crate::router::FrameSink::new(sender),
        };
        router
            .route_for_connection(&ctx, hello_frame("queue-failure", PROTOCOL_VERSION, 1))
            .await
            .unwrap();
        receiver.recv().await.unwrap();
        handler
            .agent_declarations
            .fault_for_test(&handler, &ctx, closed, 2);
        ctx.egress
            .send(Frame::build(FrameType::Pong, control_flags(), 0, 0, 0, Vec::new()).unwrap())
            .await
            .unwrap();
        let request = Frame::build(
            FrameType::Request,
            control_flags(),
            0,
            0,
            2,
            br#"{"op":"supervisor.agent_commands","started":[],"finished":[]}"#.to_vec(),
        )
        .unwrap();
        tokio::time::timeout(
            Duration::from_millis(50),
            router.route_for_connection(&ctx, request),
        )
        .await
        .expect("enqueue refusal must not await ordinary connection-loop egress")
        .unwrap();
        assert!(registry.get_module("queue-failure").unwrap().is_some());
        let goodbye =
            Frame::build(FrameType::Goodbye, control_flags(), 0, 0, 3, Vec::new()).unwrap();
        tokio::time::timeout(
            Duration::from_millis(50),
            router.route_for_connection(&ctx, goodbye),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(registry.get_module("queue-failure").unwrap().is_none());
    }
}

#[tokio::test]
async fn a_contended_status_snapshot_cannot_park_the_async_runtime() {
    let registry = Arc::new(Registry::default());
    let handle = SupervisorHandle::new();
    let supervisor = Supervisor::new_for_test(registry.clone(), RestartPolicy::default())
        .with_handle(handle.clone());
    let module = supervisor
        .supervise_configured(
            crate::ModuleSpec {
                module_id: "snapshot-lock".into(),
                program: PathBuf::from("disabled-module"),
                args: Vec::new(),
                env: Vec::new(),
                reserved: false,
                reserved_prefixes: Vec::new(),
                protocol: ModuleProtocol::Subc,
                overlap: Default::default(),
                carry_containment: false,
            },
            false,
        )
        .unwrap();
    module.stop().await.unwrap();
    let handler = ControlHandler::new(registry).with_supervisor(handle);
    let (ready, waiting) = std::sync::mpsc::channel();
    let (release, held) = std::sync::mpsc::channel();
    let owner = std::thread::spawn(move || {
        module.with_resource_snapshot_locked_for_test(|| {
            ready.send(()).unwrap();
            held.recv_timeout(Duration::from_secs(2)).is_ok()
        })
    });
    waiting.recv_timeout(Duration::from_secs(2)).unwrap();
    let (ctx, mut wire) = route_ctx(ConnectionId::new(903));
    let request = Frame::build(FrameType::Request, control_flags(), 0, 0, 1, Vec::new()).unwrap();
    assert!(handler
        .handle_client_control_request(
            &ctx,
            request,
            ClientControlRequest::SupervisorList {},
            &mut None
        )
        .await
        .unwrap()
        .is_empty());
    // The lock owner releases only after this runtime runs another future. A
    // watchdog releases it on failure, so the regression fails rather than hangs.
    tokio::time::sleep(Duration::from_millis(5)).await;
    let released = release.send(()).is_ok();
    assert!(
        owner.join().unwrap() && released,
        "status lock waiting must run on a blocking worker, not this runtime"
    );
    tokio::time::timeout(Duration::from_secs(5), wire.recv())
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn full_egress_and_refusal_overflow_do_not_park_the_real_module_router() {
    let registry = Arc::new(Registry::default());
    let handler = Arc::new(ControlHandler::new(Arc::clone(&registry)));
    let router = Router::with_control_handler(Arc::clone(&handler));
    let connection = router.begin_connection();
    let (sender, mut receiver) = mpsc::channel(1);
    let ctx = RouteCtx {
        connection_id: connection.id(),
        egress: crate::router::FrameSink::new(sender),
    };
    router
        .route_for_connection(&ctx, hello_frame("queue-module", PROTOCOL_VERSION, 100))
        .await
        .unwrap();
    assert_eq!(
        receiver.recv().await.unwrap().header.ty,
        FrameType::HelloAck
    );
    ctx.egress
        .send(Frame::build(FrameType::Pong, control_flags(), 0, 0, 0, Vec::new()).unwrap())
        .await
        .unwrap();
    let declaration = |corr| {
        Frame::build(
            FrameType::Request,
            control_flags(),
            0,
            0,
            corr,
            br#"{"op":"supervisor.agent_commands","started":[],"finished":[]}"#.to_vec(),
        )
        .unwrap()
    };
    for corr in 1..=16 {
        tokio::time::timeout(
            Duration::from_millis(20),
            router.route_for_connection(&ctx, declaration(corr)),
        )
        .await
        .expect("a full writer cannot make a busy refusal park the module reader")
        .unwrap();
    }
    let describe = handler
        .handle_server_describe(
            Frame::build(FrameType::Request, control_flags(), 0, 0, 200, Vec::new()).unwrap(),
        )
        .unwrap();
    let diagnostic: Value = serde_json::from_slice(&describe[0].body).unwrap();
    assert_eq!(
        diagnostic["counters"]["agent_command_refusals_dropped_by_module"]["queue-module"], 8,
        "four admitted requests plus four bounded refusals leave eight visibly dropped replies"
    );
    let goodbye = Frame::build(FrameType::Goodbye, control_flags(), 0, 0, 201, Vec::new()).unwrap();
    tokio::time::timeout(
        Duration::from_millis(20),
        router.route_for_connection(&ctx, goodbye),
    )
    .await
    .expect("Goodbye following overflow must still reach the real router")
    .unwrap();
    assert!(
        registry.get_module("queue-module").unwrap().is_none(),
        "the later frame was actually dispatched, not just buffered"
    );
    assert_eq!(
        receiver.try_recv().unwrap().header.corr,
        0,
        "no writer capacity was released to let Goodbye progress"
    );
}
