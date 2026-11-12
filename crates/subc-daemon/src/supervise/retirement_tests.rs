use super::*;
use crate::control::claims::tests::{mint, prefix_connection, request, rig, Net, Wire};
use crate::forwarding::RouteBindRelayOutcome;
use subc_control::{ClaimPresentRequest, ClaimPurpose};
use subc_protocol::FrameType;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn provider_hello(module: &str) -> subc_protocol::Frame {
    use subc_protocol::manifest::{
        Concurrency, ExecutionMode, IdentityScope, ModuleManifest, ProviderRole, Tool,
    };
    let body = subc_protocol::ModuleHelloBody {
        manifest: ModuleManifest::builder(module, "1.0.0")
            .provides(vec![ProviderRole::ToolProvider {
                tools: vec![Tool {
                    name: "read".into(),
                    description: None,
                    execution_mode: ExecutionMode::Pure,
                    schema: serde_json::json!({"type":"object"}),
                }],
                identity_scope: vec![IdentityScope::Project, IdentityScope::Session],
                concurrency: Concurrency::ModuleManaged,
                emits_push: true,
                sub_supervises: false,
            }])
            .build(),
        protocol_ver: subc_protocol::PROTOCOL_VERSION,
        control_ops: None,
        launch_nonce: None,
    };
    subc_protocol::Frame::build(
        FrameType::Hello,
        control_flags(),
        0,
        0,
        1,
        serde_json::to_vec(&body).unwrap(),
    )
    .unwrap()
}

async fn hold_retirement(
    spawn: &Arc<RegistrationSpawn>,
) -> (std::sync::mpsc::Sender<()>, tokio::task::JoinHandle<()>) {
    let (reached, observed) = tokio::sync::oneshot::channel();
    let (release, paused) = std::sync::mpsc::channel();
    *spawn.retirement_pause.lock().unwrap() = Some(RetirementPause {
        reached,
        release: paused,
    });
    let spawn = spawn.clone();
    let task = tokio::task::spawn_blocking(move || spawn.retire());
    tokio::time::timeout(Duration::from_secs(2), observed)
        .await
        .unwrap()
        .unwrap();
    (release, task)
}

fn data_frame(ty: FrameType, channel: u16, epoch: u32, corr: u64) -> subc_protocol::Frame {
    subc_protocol::Frame::build(
        ty,
        control_flags(),
        channel,
        epoch,
        corr,
        b"must-not-cross".to_vec(),
    )
    .unwrap()
}

async fn assert_open_socket_has_no_frame(net: &mut Net) {
    let result = tokio::time::timeout(
        Duration::from_millis(100),
        subc_transport::read_frame(&mut net.client),
    )
    .await;
    assert!(
        result.is_err(),
        "retired egress delivered a frame, or teardown won before the check: {result:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn registration_retirement_egress_prefix_target_blocks_request_after_window_commit() {
    let mut rig = rig().await;
    let mut module = prefix_connection(&mut rig, "claims:egress-target").await;
    assert_eq!(
        module
            .call(provider_hello("claims:egress-target"))
            .await
            .header
            .ty,
        FrameType::HelloAck
    );
    tokio::time::timeout(Duration::from_secs(2), async {
        while rig
            .handler
            .forwarding()
            .active_module_endpoint("claims:egress-target")
            .unwrap()
            .is_none()
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let target = module.connection_id(&rig.handler);
    let target_sink = rig
        .spawn
        .claims
        .get()
        .unwrap()
        .connection_sink_for_test(target);
    let forwarding = rig.handler.forwarding();
    let mut client = Wire::new(&rig.router);
    let mut pending = forwarding
        .begin_route_bind_relay_for_test(
            client.connection.id(),
            client.ctx.egress.clone(),
            910,
            "claims:egress-target",
        )
        .unwrap();
    let (checked, resume) = forwarding.pause_route_commit_for_test();
    let table = forwarding.clone();
    let corr = pending.corr;
    let completion = tokio::task::spawn_blocking(move || {
        table.complete_pending_relay(target, corr, RouteBindRelayOutcome::Accepted)
    });
    tokio::time::timeout(Duration::from_secs(2), checked)
        .await
        .unwrap()
        .unwrap();
    let (finish, retirement) = hold_retirement(&rig.spawn).await;
    resume.send(()).unwrap();
    assert!(completion.await.unwrap().unwrap().settled);
    assert!(matches!(
        pending.receiver.try_recv().unwrap(),
        RouteBindRelayOutcome::Accepted
    ));
    drop(pending);
    let reply = client.replies.recv().await.unwrap().frame;
    let subc_control::ClientControlResponse::RouteOpen {
        route_channel,
        route_epoch,
        ..
    } = serde_json::from_slice(&reply.body).unwrap()
    else {
        panic!("expected committed route")
    };
    assert_eq!(forwarding.active_binding_count().unwrap(), 1);
    rig.router
        .route_for_connection(
            &client.ctx,
            data_frame(FrameType::Request, route_channel, route_epoch, 911),
        )
        .await
        .unwrap();
    assert_open_socket_has_no_frame(&mut module).await;
    assert_eq!(
        forwarding.active_binding_count().unwrap(),
        1,
        "close processing must remain held back"
    );
    assert!(target_sink.ingress_terminal());
    assert!(target_sink.egress_terminal());
    drop(target_sink);
    finish.send(()).unwrap();
    retirement.await.unwrap();
    assert!(module.recv().await.is_none());
    assert_eq!(forwarding.active_binding_count().unwrap(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn registration_retirement_egress_claim_opener_blocks_reverse_request_and_push() {
    let mut rig = rig().await;
    let mut provider = Wire::new(&rig.router);
    assert_eq!(
        provider
            .call(&rig.router, provider_hello("egress-provider"))
            .await
            .header
            .ty,
        FrameType::HelloAck
    );
    let claim = mint(&mut rig, ClaimPurpose::SelfConnection).await.claim;
    let mut client = Net::new(rig.router.clone()).await;
    assert_eq!(
        client
            .call(request(ClaimPresentRequest::new(claim)))
            .await
            .header
            .ty,
        FrameType::Response
    );
    let connection = client.connection_id(&rig.handler);
    let sink = rig
        .spawn
        .claims
        .get()
        .unwrap()
        .connection_sink_for_test(connection);
    let forwarding = rig.handler.forwarding();
    let pending = forwarding
        .begin_route_bind_relay_for_test(connection, sink.clone(), 920, "egress-provider")
        .unwrap();
    forwarding
        .complete_pending_relay(
            provider.connection.id(),
            pending.corr,
            RouteBindRelayOutcome::Accepted,
        )
        .unwrap();
    assert_eq!(client.recv().await.unwrap().header.ty, FrameType::Response);
    let (finish, retirement) = hold_retirement(&rig.spawn).await;
    assert_eq!(forwarding.active_binding_count().unwrap(), 1);
    for (ty, corr) in [(FrameType::Request, 921), (FrameType::Push, 0)] {
        rig.router
            .route_for_connection(
                &provider.ctx,
                data_frame(ty, pending.module_channel, pending.module_epoch, corr),
            )
            .await
            .unwrap();
    }
    assert_open_socket_has_no_frame(&mut client).await;
    assert_eq!(
        forwarding.active_binding_count().unwrap(),
        1,
        "teardown must not make the delivery assertion pass"
    );
    assert!(sink.egress_terminal());
    drop(sink);
    finish.send(()).unwrap();
    retirement.await.unwrap();
    assert!(client.recv().await.is_none());
    assert_eq!(forwarding.active_binding_count().unwrap(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn registration_retirement_primary_fences_reverse_request_buffered_with_bind_ack() {
    let handle = SupervisorHandle::new();
    let control =
        crate::ControlHandler::new(Arc::new(Registry::default())).with_supervisor(handle.clone());
    let forwarding = control.forwarding();
    let router = Arc::new(crate::Router::with_control_handler(Arc::new(control)));
    let spawn = RegistrationSpawn::new(&handle, "buffered-primary", SpawnRole::Plain, None);
    spawn.ready();
    let mut primary = Wire::new(&router);
    spawn.bind(primary.connection.id());
    assert_eq!(
        primary
            .call(&router, provider_hello("buffered-primary"))
            .await
            .header
            .ty,
        FrameType::HelloAck
    );
    let mut client = Wire::new(&router);
    let pending = forwarding
        .begin_route_bind_relay_for_test(
            client.connection.id(),
            client.ctx.egress.clone(),
            930,
            "buffered-primary",
        )
        .unwrap();
    let ack = subc_protocol::Frame::build(
        FrameType::Response,
        control_flags(),
        0,
        0,
        pending.corr,
        serde_json::to_vec(&subc_protocol::session::ModuleControlResponse::RouteBindAck {})
            .unwrap(),
    )
    .unwrap();
    let reverse = data_frame(
        FrameType::Request,
        pending.module_channel,
        pending.module_epoch,
        931,
    );
    let (daemon, module) = std::os::unix::net::UnixStream::pair().unwrap();
    *spawn.endpoint.lock().unwrap() = Some(daemon.try_clone().unwrap());
    daemon.set_nonblocking(true).unwrap();
    module.set_nonblocking(true).unwrap();
    let mut daemon = tokio::net::UnixStream::from_std(daemon).unwrap();
    let mut module = tokio::net::UnixStream::from_std(module).unwrap();
    let mut bytes = Vec::new();
    for frame in [&ack, &reverse] {
        bytes.extend_from_slice(&frame.header.encode());
        bytes.extend_from_slice(&frame.body);
    }
    module.write_all(&bytes).await.unwrap();
    let decoded_ack = subc_transport::read_frame(&mut daemon)
        .await
        .unwrap()
        .unwrap();
    router
        .route_for_connection(&primary.ctx, decoded_ack)
        .await
        .unwrap();
    assert_eq!(
        client.replies.recv().await.unwrap().frame.header.ty,
        FrameType::Response
    );
    let (finish, retirement) = hold_retirement(&spawn).await;
    // Choose the read branch explicitly: queued bytes can win the server's
    // select even though the channel-closed notification is also ready.
    let decoded_reverse = subc_transport::read_frame(&mut daemon)
        .await
        .unwrap()
        .expect("queued bytes remain readable after shutdown on Linux");
    assert_eq!(decoded_reverse.header.corr, 931);
    router
        .route_for_connection_ingress(&primary.ctx, decoded_reverse, None, false)
        .await
        .unwrap();
    assert!(
        client.replies.try_recv().is_err(),
        "a primary frame decoded after shutdown must not reach a client"
    );
    assert!(primary.ctx.egress.ingress_terminal());
    assert!(primary.ctx.egress.egress_terminal());
    assert_eq!(
        forwarding.active_binding_count().unwrap(),
        1,
        "cleanup must not make the ingress assertion pass"
    );
    finish.send(()).unwrap();
    retirement.await.unwrap();
    drop(client);
    assert_eq!(forwarding.active_binding_count().unwrap(), 0);
}

#[tokio::test]
async fn registration_retirement_drop_keeps_launch_tie_while_pending_relays_exist() {
    let mut rig = rig().await;
    let token = mint(&mut rig, ClaimPurpose::SelfConnection).await.claim;
    let mut client = Wire::new(&rig.router);
    assert_eq!(
        client
            .call(&rig.router, request(ClaimPresentRequest::new(token)))
            .await
            .header
            .ty,
        FrameType::Response
    );
    let forwarding = rig.handler.forwarding();
    let connection = client.connection.id();
    let _pending = forwarding
        .begin_route_bind_relay_for_test(
            connection,
            client.ctx.egress.clone(),
            940,
            "claims-module",
        )
        .unwrap();
    let observed = forwarding.observe_cleanup_tie_for_test();
    drop(client);
    let (linked, pending) = observed.await.unwrap();
    assert_eq!(
        pending, 1,
        "the cleanup boundary must still contain a pending bind"
    );
    assert!(
        linked,
        "the launch tie must survive until pending bind cleanup runs"
    );
    assert!(!forwarding.spawn_connection_recorded_for_test(connection));
    assert_eq!(forwarding.reserved_route_count().unwrap(), (0, 0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn registration_retirement_goodbye_keeps_claim_bind_fence_until_read_side_cleanup() {
    let mut rig = tokio::time::timeout(Duration::from_secs(10), rig())
        .await
        .unwrap();
    let mut provider = Wire::new(&rig.router);
    assert_eq!(
        provider
            .call_bounded(&rig.router, provider_hello("retirement-provider"))
            .await
            .header
            .ty,
        FrameType::HelloAck
    );
    let token = tokio::time::timeout(
        Duration::from_secs(2),
        mint(&mut rig, ClaimPurpose::SelfConnection),
    )
    .await
    .unwrap()
    .claim;
    let mut client = Wire::new(&rig.router);
    assert_eq!(
        client
            .call_bounded(&rig.router, request(ClaimPresentRequest::new(token)))
            .await
            .header
            .ty,
        FrameType::Response
    );
    let client_id = client.connection.id();
    assert!(Arc::ptr_eq(
        &rig.claim_registration_for_test(client_id),
        &rig.spawn
    ));
    rig.router
        .route_for_connection(
            &client.ctx,
            subc_protocol::Frame::build(FrameType::Goodbye, control_flags(), 0, 0, 2, Vec::new())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(Arc::ptr_eq(
        &rig.claim_registration_for_test(client_id),
        &rig.spawn
    ));
    let open = subc_control::ClientControlRequest::RouteOpen {
        busy_refusals: None,
        target: subc_protocol::RouteTarget::ToolProvider {
            module_id: "retirement-provider".into(),
        },
        identity: subc_protocol::BindIdentity::new("retirement-test", "goodbye"),
        consumer_identity: None,
        consumer_capabilities: None,
        role_versions: None,
        admission_facts: None,
        scope: None,
    };
    let router = rig.router.clone();
    let ctx = client.ctx.clone();
    let opening =
        tokio::spawn(async move { router.route_for_connection(&ctx, request(open)).await });
    let bind = tokio::time::timeout(Duration::from_secs(2), provider.replies.recv())
        .await
        .unwrap()
        .unwrap()
        .frame;
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&bind.body).unwrap()["op"],
        "route.bind"
    );
    rig.spawn.retire();
    assert!(!rig.spawn.launch_ready());
    assert!(client.ctx.egress.ingress_terminal() && client.ctx.egress.egress_terminal());
    assert!(
        !provider.ctx.egress.ingress_terminal() && !provider.ctx.egress.egress_terminal(),
        "the unrelated provider must remain live"
    );
    let ack = subc_protocol::Frame::build(
        FrameType::Response,
        control_flags(),
        0,
        0,
        bind.header.corr,
        serde_json::to_vec(&subc_protocol::session::ModuleControlResponse::RouteBindAck {})
            .unwrap(),
    )
    .unwrap();
    rig.router
        .route_for_connection(&provider.ctx, ack)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), opening)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    // The opener belongs to the retired launch, so even the refusal reply is
    // suppressed. Forwarding state, not a reply on that sink, proves denial.
    assert!(
        tokio::time::timeout(Duration::from_millis(100), client.replies.recv())
            .await
            .is_err(),
        "a retired opener must not receive a post-retirement reply"
    );
    assert_eq!(
        rig.handler.forwarding().reserved_route_count().unwrap(),
        (0, 0)
    );
    assert_eq!(rig.handler.forwarding().active_binding_count().unwrap(), 0);
    assert!(rig
        .handler
        .forwarding()
        .spawn_connection_recorded_for_test(client_id));
    drop(client);
    assert!(
        !rig.handler
            .forwarding()
            .spawn_connection_recorded_for_test(client_id),
        "read-side cleanup must release the launch tie after routes are gone"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn registration_retirement_target_after_readiness_cannot_deliver_committed_request() {
    let registry = Arc::new(Registry::default());
    let handle = SupervisorHandle::new();
    let control = crate::ControlHandler::new(registry.clone()).with_supervisor(handle.clone());
    let forwarding = control.forwarding();
    let spawn = RegistrationSpawn::new(&handle, "racing-target", SpawnRole::Plain, None);
    spawn.ready();
    let (auth, info) = super::review_tests::auth();
    let (router, mut target, _copy) =
        super::review_tests::serve_control(&handle, spawn.clone(), auth, control);
    subc_transport::authenticate_client(&mut target, &info, Duration::from_secs(2))
        .await
        .unwrap();
    subc_transport::write_frame(&mut target, &provider_hello("racing-target"))
        .await
        .unwrap();
    assert_eq!(
        subc_transport::read_frame(&mut target)
            .await
            .unwrap()
            .unwrap()
            .header
            .ty,
        FrameType::HelloAck
    );
    tokio::time::timeout(Duration::from_secs(2), async {
        while !spawn.registered() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let target_id = registry
        .get_module("racing-target")
        .unwrap()
        .unwrap()
        .connection_id;
    let mut client = Wire::new(&router);
    let mut pending = forwarding
        .begin_route_bind_relay_for_test(
            client.connection.id(),
            client.ctx.egress.clone(),
            800,
            "racing-target",
        )
        .unwrap();
    let (observed, release) = forwarding.pause_route_commit_for_test();
    let table = forwarding.clone();
    let completion = tokio::task::spawn_blocking(move || {
        table.complete_pending_relay(target_id, pending.corr, RouteBindRelayOutcome::Accepted)
    });
    tokio::time::timeout(Duration::from_secs(2), observed)
        .await
        .unwrap()
        .unwrap();
    spawn.retire();
    assert!(!spawn.launch_ready());
    release.send(()).unwrap();
    assert!(completion.await.unwrap().unwrap().settled);
    assert!(
        matches!(
            pending.receiver.try_recv().unwrap(),
            RouteBindRelayOutcome::Accepted
        ),
        "the fixture must reach the permitted check-then-commit window"
    );
    let opened = client.replies.recv().await.unwrap().frame;
    let subc_control::ClientControlResponse::RouteOpen {
        route_channel,
        route_epoch,
        ..
    } = serde_json::from_slice(&opened.body).unwrap()
    else {
        panic!("expected a committed route.open")
    };
    router
        .route_for_connection(
            &client.ctx,
            subc_protocol::Frame::build(
                FrameType::Request,
                control_flags(),
                route_channel,
                route_epoch,
                801,
                b"must-not-cross".to_vec(),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let received = tokio::time::timeout(
        Duration::from_secs(2),
        subc_transport::read_frame(&mut target),
    )
    .await
    .unwrap();
    assert!(
        !matches!(received, Ok(Some(_))),
        "a retired target received a frame through the committed route: {received:?}"
    );
    tokio::time::timeout(Duration::from_secs(2), async {
        while forwarding.active_binding_count().unwrap() != 0
            || forwarding
                .module_endpoint_for_connection(target_id)
                .unwrap()
                .is_some()
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("read-side cleanup must remove the transient route");
}

struct PreExecSibling {
    release: std::io::PipeWriter,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for PreExecSibling {
    fn drop(&mut self) {
        use std::io::Write;
        let _ = self.release.write_all(&[1]);
    }
}

async fn pre_exec_sibling(endpoint: &std::os::unix::net::UnixStream) -> PreExecSibling {
    use std::os::fd::AsRawFd;
    let mut command = std::process::Command::new("/bin/true");
    let (mut ready, release) =
        subc_os::fork_exec_test::pause_before_exec(&mut command, endpoint.as_raw_fd()).unwrap();
    let task = tokio::task::spawn_blocking(move || {
        command.spawn().unwrap().wait().unwrap();
    });
    tokio::task::spawn_blocking(move || {
        use std::io::Read;
        ready.read_exact(&mut [0; 8]).unwrap();
    })
    .await
    .unwrap();
    PreExecSibling { release, task }
}

async fn first_observation(mode: &'static str, caller: bool) {
    let (endpoint, peer) = std::os::unix::net::UnixStream::pair().unwrap();
    let sibling = pre_exec_sibling(&endpoint).await;
    let mut rig = rig().await;
    let token = mint(&mut rig, ClaimPurpose::SelfConnection).await.claim;
    let pending_claim = mint(&mut rig, ClaimPurpose::Helper).await.claim;
    let parked_token = mint(&mut rig, ClaimPurpose::SelfConnection).await.claim;
    // This connection has no server loop, so teardown cannot remove the pending
    // bind before its liveness check is exercised.
    let mut parked = Wire::new(&rig.router);
    assert_eq!(
        parked
            .call(&rig.router, request(ClaimPresentRequest::new(parked_token)))
            .await
            .header
            .ty,
        FrameType::Response
    );
    let mut bound = Net::new(rig.router.clone()).await;
    assert_eq!(
        bound
            .call(request(ClaimPresentRequest::new(token)))
            .await
            .header
            .ty,
        FrameType::Response
    );
    let bound_id = bound.connection_id(&rig.handler);
    let forwarding = rig.handler.forwarding();
    let (tx, mut replies) = tokio::sync::mpsc::channel(8);
    let sink = crate::FrameSink::new(tx);
    let existing = forwarding
        .begin_route_bind_relay_for_test(bound_id, sink.clone(), 899, "claims-module")
        .unwrap();
    forwarding
        .complete_pending_relay(
            rig.primary.ctx.connection_id,
            existing.corr,
            RouteBindRelayOutcome::Accepted,
        )
        .unwrap();
    assert_eq!(
        replies.try_recv().unwrap().frame.header.ty,
        FrameType::Response
    );
    assert_eq!(forwarding.active_binding_count().unwrap(), 1);
    let client = if caller {
        parked.connection.id()
    } else {
        crate::ConnectionId::new(80_000)
    };
    let mut pending = forwarding
        .begin_route_bind_relay_for_test(client, sink.clone(), 900, "claims-module")
        .unwrap();
    let primary = rig.primary.ctx.connection_id;

    let stream = endpoint.try_clone().unwrap();
    *rig.spawn.endpoint.lock().unwrap() = Some(endpoint);
    peer.set_nonblocking(true).unwrap();
    let mut peer = tokio::net::UnixStream::from_std(peer).unwrap();
    let (reached, observation) = tokio::sync::oneshot::channel();
    let (release, paused) = std::sync::mpsc::channel();
    *rig.spawn.retirement_pause.lock().unwrap() = Some(RetirementPause {
        reached,
        release: paused,
    });

    let supervisor =
        Supervisor::new_for_test(Arc::new(Registry::default()), RestartPolicy::default());
    let runtime = supervisor.runtime_config();
    let spec = ModuleSpec {
        module_id: "retirement-probe".into(),
        program: "/bin/sleep".into(),
        args: vec!["300".into()],
        env: Vec::new(),
        reserved: false,
        reserved_prefixes: Vec::new(),
        protocol: ModuleProtocol::None,
        overlap: Default::default(),
        carry_containment: true,
    };
    let mut child = spawn_and_mark_running(
        &spec,
        &runtime,
        &Arc::new(Mutex::new(SupervisorSnapshot::starting())),
    )
    .unwrap();
    let pid = child.pid;
    assert!(
        child.child.container().is_some(),
        "fixture must retain its group pin"
    );
    child.registration_spawn = Some(rig.spawn.clone());
    let handle = SupervisorHandle::new();
    handle.set_spawn_nonce("retirement-probe", "retirement-nonce".into());
    let binding = handle
        .begin_program_binding("retirement-probe", SpawnRole::Plain)
        .unwrap();
    binding.attach_registration(&rig.spawn);
    child._program_binding = Some(binding.clone());
    child.child.start_kill().unwrap();
    while subc_os::child_exit::observe(pid).unwrap().is_none() {
        tokio::task::yield_now().await;
    }
    let spawn = rig.spawn.clone();
    // Retirement's test hook blocks synchronously. Keep it off the I/O workers
    // so a wake parked in a worker's local queue cannot delay the assertion task.
    let executor = tokio::runtime::Handle::current();
    let observer = tokio::task::spawn_blocking(move || {
        executor.block_on(async move {
            match mode {
                "wait" => {
                    child.wait().await.unwrap();
                }
                "try_wait" => {
                    assert!(child.try_wait().unwrap().is_some());
                }
                "finish_exit" => {
                    child.finish_exit(true).await.unwrap();
                }
                "hook" => {
                    binding.retirement_hook().upgrade().unwrap().retire();
                }
                "eof" => {
                    stream.set_nonblocking(true).unwrap();
                    let stream = tokio::net::UnixStream::from_std(stream).unwrap();
                    let mut stream = RegistrationStream { stream, spawn };
                    assert_eq!(stream.read(&mut [0]).await.unwrap(), 0);
                }
                _ => unreachable!(),
            }
            child
        })
    });
    if mode == "eof" {
        peer.shutdown().await.unwrap();
    }
    tokio::time::timeout(Duration::from_secs(2), observation)
        .await
        .expect("the exit observation must synchronously retire before cleanup")
        .unwrap();
    // Retirement has disabled launch readiness and voided pending claims, but
    // has not notified connection loops yet. Neither the unreaped child nor a
    // pre-exec descriptor copy can keep the channel or a claim alive here.
    assert!(!rig.spawn.launch_ready());
    assert_eq!(rig.spawn.claims.get().unwrap().pending_count_for_test(), 0);
    assert!(subc_os::child_exit::observe(pid).unwrap().is_some());
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), peer.read(&mut [0]))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    let completion = forwarding
        .complete_pending_relay(primary, pending.corr, RouteBindRelayOutcome::Accepted)
        .unwrap();
    assert!(completion.settled);
    assert!(completion.abandoned.is_some());
    assert!(
        !matches!(
            pending.receiver.try_recv().unwrap(),
            RouteBindRelayOutcome::Accepted
        ),
        "bind commit must recheck spawn liveness before close notifications run"
    );
    assert!(
        replies.try_recv().is_err(),
        "a retired spawn must not publish route.open"
    );
    assert_eq!(forwarding.reserved_route_count().unwrap(), (0, 0));
    release.send(()).unwrap();
    let mut child = observer.await.unwrap();
    assert!(bound.recv().await.is_none(), "bound connection must close");
    assert_eq!(
        forwarding.active_binding_count().unwrap(),
        0,
        "bound routes must close with their connection"
    );
    let mut refused = Net::new(rig.router.clone()).await;
    let reply = refused
        .call(request(ClaimPresentRequest::new(pending_claim)))
        .await;
    crate::control::claims::tests::error(&reply, "claim_invalid");
    assert!(refused.recv().await.is_none());
    child.finish_exit(false).await.unwrap();
    use std::io::Write;
    let mut sibling = sibling;
    sibling.release.write_all(&[1]).unwrap();
    (&mut sibling.task).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn registration_retirement_wait_fences_claims_and_bind_with_unreaped_pin() {
    first_observation("wait", true).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn registration_retirement_try_wait_fences_claims_and_bind() {
    first_observation("try_wait", true).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn registration_retirement_finish_exit_fences_claims_and_bind() {
    first_observation("finish_exit", true).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn registration_retirement_carried_hook_fences_claims_and_bind() {
    first_observation("hook", true).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn registration_retirement_eof_fences_claims_and_bind_before_cleanup() {
    first_observation("eof", true).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn registration_retirement_target_exit_refuses_pending_bind() {
    first_observation("eof", false).await;
}
