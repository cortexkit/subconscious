#[derive(Default)]
struct WireWriteGate {
    blocked: std::sync::atomic::AtomicBool,
    writer: Mutex<Option<std::task::Waker>>,
}

impl WireWriteGate {
    fn unblock(&self) {
        self.blocked.store(false, std::sync::atomic::Ordering::SeqCst);
        if let Some(writer) = self.writer.lock().unwrap().take() { writer.wake(); }
    }
}

struct GatedWire {
    stream: tokio::net::UnixStream,
    gate: Arc<WireWriteGate>,
}

impl tokio::io::AsyncRead for GatedWire {
    fn poll_read(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>, buf: &mut tokio::io::ReadBuf<'_>) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}

impl tokio::io::AsyncWrite for GatedWire {
    fn poll_write(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>, buf: &[u8]) -> std::task::Poll<io::Result<usize>> {
        let mut writer = self.gate.writer.lock().unwrap();
        if self.gate.blocked.load(std::sync::atomic::Ordering::SeqCst) {
            *writer = Some(cx.waker().clone());
            return std::task::Poll::Pending;
        }
        drop(writer);
        std::pin::Pin::new(&mut self.stream).poll_write(cx, buf)
    }
    fn poll_flush(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.stream).poll_flush(cx)
    }
    fn poll_shutdown(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

struct DrainWireFixture {
    spec: ModuleSpec,
    runtime: SupervisorRuntimeConfig,
    registry: Arc<Registry>,
    child: Option<SupervisedChild>,
    spawn: Arc<RegistrationSpawn>,
    peer: tokio::net::UnixStream,
    _router: Arc<crate::Router>,
    gate: Arc<WireWriteGate>,
    _info: subc_transport::ConnectionInfo,
    _scratch: cortexkit_test_support::ScratchDir,
}

impl Drop for DrainWireFixture {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child { let _ = child.start_kill(); }
    }
}

async fn drain_wire_fixture() -> DrainWireFixture {
    let scratch = cortexkit_test_support::ScratchDir::new("channel-drain-wire");
    let registry = Arc::new(Registry::default());
    let forwarding = Arc::new(ForwardingTable::default());
    let handle = SupervisorHandle::new();
    let supervisor = Supervisor::new_for_test(registry.clone(), RestartPolicy::default())
        .with_handle(handle.clone()).with_forwarding(forwarding.clone());
    let mut runtime = supervisor.runtime_config();
    runtime.drain_timeout = Duration::from_secs(2);
    let mut spec = ModuleSpec {
        module_id: "channel-module".into(), program: "/bin/sleep".into(), args: vec!["60".into()],
        env: ["HOME", "XDG_DATA_HOME", "XDG_RUNTIME_DIR", "XDG_CONFIG_HOME"].into_iter()
            .map(|key| (key.into(), scratch.display().to_string())).collect(),
        reserved: false, reserved_prefixes: vec![], protocol: ModuleProtocol::None,
        overlap: Default::default(), carry_containment: false,
    };
    let mut child = spawn_child_in_slot_async(&spec, &runtime, SpawnRole::Plain, false).await.unwrap();
    #[cfg(target_os = "macos")]
    child.confirm_privacy_exec().await;
    spec.protocol = ModuleProtocol::Subc;
    let spawn = RegistrationSpawn::new(&handle, &spec.module_id, SpawnRole::Plain, None);
    child.registration_spawn = Some(spawn.clone());
    let control = crate::ControlHandler::with_forwarding(registry.clone(), forwarding.clone()).with_supervisor(handle);
    let router = Arc::new(crate::Router::with_control_handler(Arc::new(control)));
    let (auth, info) = auth();
    let (parent, peer) = std::os::unix::net::UnixStream::pair().unwrap();
    *spawn.endpoint.lock().unwrap() = Some(parent.try_clone().unwrap());
    spawn.deadline.set(Instant::now() + Duration::from_secs(5)).unwrap();
    parent.set_nonblocking(true).unwrap(); peer.set_nonblocking(true).unwrap();
    let gate = Arc::new(WireWriteGate::default());
    let stream = GatedWire { stream: tokio::net::UnixStream::from_std(parent).unwrap(), gate: gate.clone() };
    let served = router.clone(); let launch = spawn.clone();
    tokio::spawn(async move { let _ = crate::server::handle_connection_with_origin(stream, served, auth,
        crate::server::LocalOrigin::RegistrationChannel(launch)).await; });
    spawn.ready();
    let mut peer = tokio::net::UnixStream::from_std(peer).unwrap();
    subc_transport::authenticate_client(&mut peer, &info, Duration::from_secs(2)).await.unwrap();
    let mut hello_body: subc_protocol::ModuleHelloBody = serde_json::from_slice(&hello(&spec.module_id).body).unwrap();
    hello_body.manifest.provides = vec![subc_protocol::manifest::ProviderRole::ToolProvider {
        tools: vec![], identity_scope: vec![], concurrency: subc_protocol::manifest::Concurrency::ModuleManaged,
        emits_push: true, sub_supervises: false,
    }];
    let hello = Frame::build(FrameType::Hello, super::super::control_flags(), 0, 0, 1, serde_json::to_vec(&hello_body).unwrap()).unwrap();
    subc_transport::write_frame(&mut peer, &hello).await.unwrap();
    assert_eq!(wire_next(&mut peer).await.header.ty, FrameType::HelloAck);
    DrainWireFixture { spec, runtime, registry, child: Some(child), spawn, peer, _router: router, gate, _info: info, _scratch: scratch }
}

async fn wire_next<S: tokio::io::AsyncRead + Unpin>(peer: &mut S) -> Frame {
    tokio::time::timeout(Duration::from_secs(3), subc_transport::read_frame(peer)).await.unwrap().unwrap().expect("frame before EOF")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn registration_channel_plain_stop_flushes_goodbye_before_shutdown() {
    let mut fixture = drain_wire_fixture().await;
    fixture.gate.blocked.store(true, std::sync::atomic::Ordering::SeqCst);
    let runtime = fixture.runtime.clone(); let registry = fixture.registry.clone(); let spec = fixture.spec.clone();
    let spawn = fixture.spawn.clone();
    let mut drain = tokio::spawn(async move {
        let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::starting()));
        let result = begin_forwarding_drain(&spec, &runtime, &registry, &snapshot, None, RouteCloseReason::Disable).await;
        spawn.retire();
        result
    });
    let ended_early = tokio::time::timeout(Duration::from_millis(100), &mut drain).await.is_ok();
    fixture.gate.unblock();
    if !ended_early { drain.await.unwrap().unwrap(); }
    let draining = wire_next(&mut fixture.peer).await;
    assert_eq!(serde_json::from_slice::<serde_json::Value>(&draining.body).unwrap()["op"], "module.draining");
    assert_eq!(wire_next(&mut fixture.peer).await.header.ty, FrameType::Goodbye);
    assert!(subc_transport::read_frame(&mut fixture.peer).await.unwrap().is_none());
    let mut child = fixture.child.take().unwrap();
    child.start_kill().unwrap(); child.wait().await.unwrap(); child.finish_exit(false).await.unwrap();
    assert!(!ended_early, "plain stop must wait for the writer's bounded GOODBYE flush before retiring");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn registration_channel_swap_keeps_inflight_reply_and_goodbyes_before_eof() {
    let mut fixture = drain_wire_fixture().await;
    let forwarding = fixture.runtime.forwarding.as_ref().unwrap().clone();
    let handle = fixture.runtime.supervisor_handle.as_ref().unwrap().clone();
    let endpoint = forwarding.active_module_endpoint("channel-module").unwrap().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (server_auth, _) = auth();
    let server = tokio::spawn(crate::server::serve_listener(listener, fixture._router.clone(), server_auth));
    let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
    subc_transport::authenticate_client(&mut client, &fixture._info, Duration::from_secs(2)).await.unwrap();
    let open = Frame::build(FrameType::Request, super::super::control_flags(), 0, 0, 20,
        serde_json::to_vec(&serde_json::json!({"op":"route.open", "target":{"kind":"tool_provider", "module_id":"channel-module"},
            "identity":{"harness":"test", "session":"swap-wire"}})).unwrap()).unwrap();
    subc_transport::write_frame(&mut client, &open).await.unwrap();
    let bind = wire_next(&mut fixture.peer).await;
    assert_eq!(serde_json::from_slice::<serde_json::Value>(&bind.body).unwrap()["op"], "route.bind");
    let accepted = Frame::build(FrameType::Response, super::super::control_flags(), 0, 0, bind.header.corr,
        serde_json::to_vec(&subc_protocol::session::ModuleControlResponse::RouteBindAck {}).unwrap()).unwrap();
    subc_transport::write_frame(&mut fixture.peer, &accepted).await.unwrap();
    let opened = wire_next(&mut client).await;
    let opened: serde_json::Value = serde_json::from_slice(&opened.body).unwrap();
    let channel = opened["route_channel"].as_u64().unwrap() as u16;
    let epoch = opened["route_epoch"].as_u64().unwrap() as u32;
    let request = Frame::build(FrameType::Request, super::super::control_flags(), channel, epoch, 21, b"held request".to_vec()).unwrap();
    subc_transport::write_frame(&mut client, &request).await.unwrap();
    let held = wire_next(&mut fixture.peer).await;
    assert_eq!(held.body, b"held request");
    assert_eq!(forwarding.endpoint_in_flight_count(endpoint).unwrap(), 1);

    handle.open_swap("channel-module", String::new());
    let candidate = RegistrationSpawn::new(&handle, "channel-module", SpawnRole::SwapCandidate, None);
    candidate.ready();
    let (auth, info) = auth();
    fixture._router.install_registration_service(auth);
    let (parent, peer) = std::os::unix::net::UnixStream::pair().unwrap();
    peer.set_nonblocking(true).unwrap();
    let mut candidate_peer = tokio::net::UnixStream::from_std(peer).unwrap();
    handle.serve_registration(parent, candidate.clone(), std::process::id(), Instant::now() + Duration::from_secs(5));
    subc_transport::authenticate_client(&mut candidate_peer, &info, Duration::from_secs(2)).await.unwrap();
    subc_transport::write_frame(&mut candidate_peer, &hello("channel-module")).await.unwrap();
    assert_eq!(wire_next(&mut candidate_peer).await.header.ty, FrameType::HelloAck);
    forwarding.cutover_candidate("channel-module").unwrap().unwrap();
    fixture.registry.promote_candidate("channel-module").unwrap().unwrap();
    handle.promote_swap_nonce("channel-module", false);
    let spec = fixture.spec.clone(); let runtime = fixture.runtime.clone(); let registry = fixture.registry.clone();
    let child = fixture.child.take();
    let drain = tokio::spawn(async move {
        let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::starting()));
        crate::supervise::swap::retire_incumbent(&spec, &runtime, &registry, &forwarding, &snapshot,
            child, Some(endpoint), endpoint.connection_id, 1).await;
    });
    let draining = wire_next(&mut fixture.peer).await;
    assert_eq!(serde_json::from_slice::<serde_json::Value>(&draining.body).unwrap()["op"], "module.draining");
    let response = Frame::build(FrameType::Response, held.header.flags, held.header.channel, held.header.epoch, held.header.corr, b"held reply".to_vec()).unwrap();
    subc_transport::write_frame(&mut fixture.peer, &response).await.unwrap();
    loop {
        let reply = wire_next(&mut client).await;
        if reply.header.ty == FrameType::Response {
            assert_eq!(reply.header.corr, 21); assert_eq!(reply.body, b"held reply"); break;
        }
    }
    loop {
        let verdict = wire_next(&mut client).await;
        if verdict.header.ty == FrameType::Goodbye {
            assert_eq!(verdict.header.channel, channel);
            break;
        }
    }
    let final_goodbye = wire_next(&mut fixture.peer).await;
    assert_eq!(final_goodbye.header.ty, FrameType::Goodbye);
    assert_eq!(final_goodbye.header.channel, 0);
    assert!(subc_transport::read_frame(&mut fixture.peer).await.unwrap().is_none());
    tokio::time::timeout(Duration::from_secs(5), drain).await.unwrap().unwrap();
    candidate.retire(); server.abort();
}
