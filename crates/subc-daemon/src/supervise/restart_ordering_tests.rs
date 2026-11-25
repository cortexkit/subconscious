use super::*;
use std::io::Write;
use tokio::io::AsyncReadExt;

#[test]
#[ignore = "child process fixture"]
fn serving_child() {
    let Ok(address_file) = std::env::var("RESTART_TEST_ADDRESS_FILE") else {
        return;
    };
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    std::fs::write(address_file, listener.local_addr().unwrap().to_string()).unwrap();
    for stream in listener.incoming() {
        writeln!(stream.unwrap(), "{}", std::process::id()).unwrap();
    }
}

fn serving_spec(home: &std::path::Path, protocol: ModuleProtocol) -> ModuleSpec {
    let mut env = vec![(
        "RESTART_TEST_ADDRESS_FILE".into(),
        home.join("address").display().to_string(),
    )];
    env.extend(
        ["XDG_DATA_HOME", "XDG_RUNTIME_DIR", "XDG_CONFIG_HOME"]
            .into_iter()
            .map(|key| (key.into(), home.display().to_string())),
    );
    ModuleSpec {
        script: None,
        module_id: "restart-serving-child".into(),
        program: std::env::current_exe().unwrap(),
        args: vec![
            "--exact".into(),
            "supervise::restart_ordering_tests::serving_child".into(),
            "--ignored".into(),
            "--nocapture".into(),
        ],
        env,
        reserved: false,
        reserved_prefixes: vec![],
        protocol,
        overlap: Default::default(),
        carry_containment: false,
    }
}

async fn poll_restart_into_queue(mut restart: std::pin::Pin<&mut impl std::future::Future>) {
    // The actor is blocked and the command queue has room. Poll the API
    // through queue admission, leaving only its acknowledgement pending.
    std::future::poll_fn(|cx| {
        assert!(restart.as_mut().poll(cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
}

async fn restart_before_publication(requests: usize) {
    let home = cortexkit_test_support::ScratchDir::new("restart-before-publication");
    let address_file = home.path().join("address");
    let spec = serving_spec(home.path(), ModuleProtocol::None);
    let mut supervisor =
        Supervisor::new_for_test(Arc::new(Registry::default()), RestartPolicy::default());
    let (spawned_tx, spawned_rx) = oneshot::channel();
    let spawned_tx = Mutex::new(Some(spawned_tx));
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let release_rx = Mutex::new(release_rx);
    // Hold publication, not the child: it must answer a real request before
    // the restart enters the queue, even though status still has generation 0.
    supervisor.test_after_first_spawn = AfterFirstSpawnHook(Some(Arc::new(move |_| {
        spawned_tx.lock().unwrap().take().unwrap().send(()).unwrap();
        release_rx
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(20))
            .unwrap();
    })));
    let registered = supervisor
        .register_configured_with_health(
            spec.clone(),
            true,
            HealthConfig::default(),
            None,
            RestartPolicy::new(3, Duration::ZERO),
            DEFAULT_REGISTER_TIMEOUT,
        )
        .unwrap();
    let module = registered.module.clone();
    let startup = tokio::task::spawn_blocking(move || {
        supervisor.start_registered_configured(registered, None, true)
    });
    spawned_rx.await.unwrap();
    let old_pid = tokio::time::timeout(Duration::from_secs(10), async {
        let address = loop {
            if let Ok(address) = std::fs::read_to_string(&address_file) {
                if !address.is_empty() {
                    break address;
                }
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        };
        let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).await.unwrap();
        reply.trim().parse::<u32>().unwrap()
    })
    .await
    .expect("child must serve before generation publication");
    assert_eq!(
        lock_snapshot(&module.inner.snapshot)
            .unwrap()
            .spawn_generation,
        0
    );
    let mut replies = Vec::new();
    for _ in 0..requests {
        let mut restart = Box::pin(module.restart(Some(0)));
        poll_restart_into_queue(restart.as_mut()).await;
        replies.push(restart);
    }
    release_tx.send(()).unwrap();
    startup.await.unwrap().unwrap();
    for reply in replies {
        reply.await.unwrap();
    }
    // Restart schedules its replacement after completing the handler. Wait
    // for that separate actor turn before placing the command barrier.
    let replacement = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if lock_snapshot(&module.inner.snapshot)
                .unwrap()
                .spawn_generation
                > 1
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await;
    // Commands are handled serially. The update-spec reply proves every
    // restart handler has finished before we inspect the snapshot; restart's
    // own ACK only acknowledges initiation, not replacement completion.
    module.update_spec_for_test(spec).await.unwrap();
    let (generation, pid) = {
        let state = lock_snapshot(&module.inner.snapshot).unwrap();
        (state.spawn_generation, state.pid)
    };
    module.stop().await.unwrap();
    replacement.expect("restart must publish a replacement for the already-serving child");
    assert_ne!(
        pid,
        Some(old_pid),
        "a serving child predating the restart must be replaced"
    );
    assert_eq!(generation, 2, "queued restarts must share one replacement");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_received_after_serving_before_generation_publication_replaces_child() {
    restart_before_publication(1).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_queued_behind_restart_does_not_kill_its_replacement() {
    restart_before_publication(2).await;
}

#[tokio::test]
async fn manual_restart_merges_only_before_health_replacement_spawns() {
    use subc_protocol::{manifest::Concurrency, session::ModuleControlResponse};

    const MARGIN: Duration = Duration::from_secs(10);
    async fn running_child(module: &SupervisedModule, generation: u64) -> ModuleStatus {
        timeout(MARGIN, async {
            loop {
                let status = module.status().unwrap();
                if status.spawn_generation >= generation
                    && status.state == ModuleState::Running
                    && status.pid.is_some()
                {
                    return status;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("no running child at generation {generation} or later"))
    }

    for queue_during_drain in [true, false] {
        let home = cortexkit_test_support::ScratchDir::new("restart-behind-health");
        let spec = serving_spec(home.path(), ModuleProtocol::Subc);
        let registry = Arc::new(Registry::default());
        let forwarding = Arc::new(ForwardingTable::default());
        let policy = RestartPolicy::new(3, Duration::ZERO);
        let supervisor =
            Supervisor::new_for_test(registry.clone(), policy).with_forwarding(forwarding.clone());
        let module = supervisor
            .supervise_configured_with_health(
                spec.clone(),
                true,
                HealthConfig {
                    cadence: Duration::ZERO,
                    on_failing: HealthAction::Restart,
                    ..HealthConfig::default()
                },
                Some(20_000),
                policy,
            )
            .unwrap();
        let initial = running_child(&module, 1).await;
        let initial_pid = i32::try_from(initial.pid.unwrap()).unwrap();

        // Stand in for HELLO and the child's control peer. Only the original
        // process advertises health, so there are no probes on replacements.
        let connection = ConnectionId::new(1);
        let (tx, mut rx) = mpsc::channel(8);
        forwarding
            .register_module_connection(
                connection,
                spec.module_id.clone(),
                subc_protocol::PROTOCOL_VERSION,
                Concurrency::ModuleManaged,
                FrameSink::new(tx),
            )
            .unwrap();
        registry
            .register_with_control_ops(
                subc_protocol::manifest::ModuleManifest::builder(&spec.module_id, "1.0.0")
                    .protocol_ver(subc_protocol::PROTOCOL_VERSION)
                    .build(),
                subc_protocol::PROTOCOL_VERSION,
                connection,
                vec![MODULE_CONTROL_OP_HEALTH_CHECK.into()],
            )
            .unwrap();
        let probe = timeout(MARGIN, rx.recv()).await.unwrap().unwrap();
        forwarding
            .complete_module_control_rpc(
                connection,
                probe.header.corr,
                Some(MODULE_CONTROL_OP_HEALTH_CHECK),
                ModuleControlRpcOutcome::Response(ModuleControlResponse::HealthCheck {
                    status: HealthStatus::Failing,
                    detail: None,
                    metrics: None,
                }),
            )
            .unwrap();
        let drain = timeout(MARGIN, rx.recv()).await.unwrap().unwrap();
        assert!(matches!(
            serde_json::from_slice::<ModuleControlCommand>(&drain.body).unwrap(),
            ModuleControlCommand::Draining {
                reason: RouteCloseReason::Restart,
                ..
            }
        ));
        let goodbye = timeout(MARGIN, rx.recv()).await.unwrap().unwrap();
        assert_eq!(goodbye.header.ty, FrameType::Goodbye);
        assert_eq!(module.state().unwrap(), ModuleState::Draining);
        assert!(cortexkit_test_support::process_alive(initial_pid));

        let mut restart = std::pin::pin!(module.restart(Some(0)));
        if queue_during_drain {
            poll_restart_into_queue(restart.as_mut()).await;
        }
        // Complete both halves of the synthetic peer's shutdown: exit the
        // process and release its registration as a disconnect would.
        rustix::process::kill_process(
            rustix::process::Pid::from_raw(initial_pid).unwrap(),
            rustix::process::Signal::TERM,
        )
        .unwrap();
        forwarding.cleanup_connection(connection).unwrap();
        registry.deregister_connection(connection).unwrap();
        let replacement = running_child(&module, initial.spawn_generation + 1).await;
        assert_eq!(replacement.spawn_generation, initial.spawn_generation + 1);
        assert_ne!(replacement.pid, initial.pid);
        // In the late case this is the first poll, after the replacement's
        // PID and generation are observable. The ACK alone isn't a barrier.
        timeout(MARGIN, restart).await.unwrap().unwrap();
        timeout(MARGIN, module.update_spec_for_test(spec))
            .await
            .unwrap()
            .unwrap();

        let drains = if queue_during_drain { 1 } else { 2 };
        let expected_generation = initial.spawn_generation + drains;
        let final_child = running_child(&module, expected_generation).await;
        assert_eq!(final_child.spawn_generation, expected_generation);
        if queue_during_drain {
            assert_eq!(final_child.pid, replacement.pid);
            assert_eq!(final_child.restart_count, 1, "keep the health restart");
        } else {
            assert_ne!(final_child.pid, replacement.pid);
            assert!(!cortexkit_test_support::process_alive(
                i32::try_from(replacement.pid.unwrap()).unwrap()
            ));
        }
        assert!(cortexkit_test_support::process_alive(
            i32::try_from(final_child.pid.unwrap()).unwrap()
        ));
        let history = module.terminal_history();
        assert_eq!(history.entries.len(), drains as usize, "{history:?}");
        assert!(history
            .entries
            .iter()
            .all(|entry| entry.disposition == TerminalDisposition::Restarting));
        module.stop().await.unwrap();
    }
}

#[tokio::test]
async fn restart_queued_before_replacement_spawn_keeps_the_new_child() {
    let supervisor =
        Supervisor::new_for_test(Arc::new(Registry::default()), RestartPolicy::default());
    let registered = supervisor
        .register_configured_with_health(
            ModuleSpec {
                script: None,
                module_id: "queued-before-replacement".into(),
                // `/bin/sleep` itself, not `/bin/sh -c "exec sleep"`: on
                // macOS a module whose running image is not the configured
                // program is refused once its exec is confirmed (see
                // `record_write_stall_tests::sleeping_spec`).
                program: "/bin/sleep".into(),
                args: vec!["600".into()],
                env: vec![],
                reserved: false,
                reserved_prefixes: vec![],
                protocol: ModuleProtocol::None,
                overlap: Default::default(),
                carry_containment: false,
            },
            true,
            HealthConfig::default(),
            None,
            RestartPolicy::default(),
            DEFAULT_REGISTER_TIMEOUT,
        )
        .unwrap();
    let RegisteredConfiguredModule {
        mut spec,
        mut runtime,
        module,
        mut commands,
    } = registered;
    let snapshot = &module.inner.snapshot;
    let mut restart = Box::pin(module.restart(Some(0)));
    std::future::poll_fn(|cx| {
        assert!(std::future::Future::poll(restart.as_mut(), cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    // Drive the actor explicitly so a replacement is definitely spawned
    // after receipt, but before this queued command is dequeued.
    let mut child = Some(spawn_and_mark_running(&spec, &runtime, snapshot).unwrap());
    let replacement_pid = child.as_ref().unwrap().id();
    let command = commands.recv().await.unwrap();
    assert!(
        handle_supervisor_command(
            command,
            &mut spec,
            &mut runtime,
            &supervisor.registry,
            &supervisor.process_liveness,
            snapshot,
            &mut child,
            &mut commands,
            &mut VecDeque::new(),
        )
        .await
    );
    restart.await.unwrap();
    let kept_pid = child.as_ref().and_then(SupervisedChild::id);
    let pending = lock_snapshot(snapshot).unwrap().respawn_pending;
    if let Some(mut child) = child {
        child.start_kill().unwrap();
        child.wait().await.unwrap();
    }
    assert_eq!(
        kept_pid, replacement_pid,
        "a newer replacement must survive"
    );
    assert!(
        !pending,
        "a satisfied restart must not schedule another spawn"
    );
}

mod accepting_drain {
    use super::*;
    use crate::{
        router::{OutboundFrame, RouteCtx},
        ControlHandler, Router,
    };
    use serde_json::json;
    use subc_protocol::{Flags, Frame, FrameType, ModuleHelloAckBody, PROTOCOL_VERSION};

    const ID: &str = "accepting-drain-test";
    const LONG_BUDGET: Duration = Duration::from_secs(600);

    struct Rig {
        spec: ModuleSpec,
        runtime: SupervisorRuntimeConfig,
        registry: Arc<Registry>,
        snapshot: SharedSnapshot,
        forwarding: Arc<ForwardingTable>,
        router: Router,
        module: RouteCtx,
        module_rx: mpsc::Receiver<OutboundFrame>,
        client: RouteCtx,
        client_rx: mpsc::Receiver<OutboundFrame>,
    }

    fn connection(id: u64) -> (RouteCtx, mpsc::Receiver<OutboundFrame>) {
        let (tx, rx) = mpsc::channel(16);
        (
            RouteCtx {
                connection_id: ConnectionId::new(id),
                egress: FrameSink::new(tx),
            },
            rx,
        )
    }

    fn frame(ty: FrameType, corr: u64, body: Value) -> Frame {
        Frame::build(ty, Flags(0), 0, 0, corr, serde_json::to_vec(&body).unwrap()).unwrap()
    }

    fn body(frame: &Frame) -> Value {
        serde_json::from_slice(&frame.body).unwrap()
    }

    async fn next(rx: &mut mpsc::Receiver<OutboundFrame>) -> Frame {
        timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap()
            .frame
    }

    fn open(corr: u64) -> Frame {
        frame(
            FrameType::Request,
            corr,
            json!({
                "op":"route.open", "target":{"kind":"tool_provider", "module_id":ID},
                "identity":{"harness":"test", "session":"accepting-drain"}
            }),
        )
    }

    impl Rig {
        async fn new(opted_in: bool) -> Self {
            let registry = Arc::new(Registry::default());
            let forwarding = Arc::new(ForwardingTable::default());
            let liveness = Arc::new(SupervisorProcessLiveness::new());
            let runtime = Supervisor::new_for_test(registry.clone(), RestartPolicy::default())
                .with_forwarding(forwarding.clone())
                .runtime_config();
            let spec = ModuleSpec {
                module_id: ID.into(),
                ..serving_spec(std::path::Path::new("."), ModuleProtocol::Subc)
            };
            let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::starting()));
            update_snapshot(&snapshot, Some(ID), |state| {
                state.state = ModuleState::Running;
                state.enabled = true;
                state.process_alive = true;
            })
            .unwrap();
            liveness.track(ID.into(), snapshot.clone());
            let control = ControlHandler::with_forwarding(registry.clone(), forwarding.clone())
                .with_process_liveness(liveness.clone());
            let router = Router::with_control_handler(Arc::new(control));
            let (module, mut module_rx) = connection(101);
            let (client, client_rx) = connection(102);
            let mut manifest = subc_protocol::manifest::ModuleManifest::builder(ID, "1.0.0")
                .protocol_ver(PROTOCOL_VERSION)
                .build();
            manifest.provides = vec![subc_protocol::manifest::ProviderRole::ToolProvider {
                tools: vec![],
                identity_scope: vec![],
                concurrency: subc_protocol::manifest::Concurrency::ModuleManaged,
                emits_push: true,
                sub_supervises: false,
            }];
            if opted_in {
                manifest.capabilities = Some(
                    serde_json::from_value(json!({"provides":["accepting-drain/v1"]})).unwrap(),
                );
            }
            router
                .route_for_connection(
                    &module,
                    frame(
                        FrameType::Hello,
                        1,
                        json!({
                        "protocol_ver": PROTOCOL_VERSION, "manifest":manifest
                        }),
                    ),
                )
                .await
                .unwrap();
            let ack = next(&mut module_rx).await;
            assert_eq!(ack.header.ty, FrameType::HelloAck, "{:?}", body(&ack));
            let ack: ModuleHelloAckBody = serde_json::from_slice(&ack.body).unwrap();
            assert!(ack
                .subc_capabilities
                .iter()
                .any(|cap| cap == "accepting-drain/v1"));
            assert!(ack.subc_ops.iter().any(|op| op == "module.drain_ready"));
            Self {
                spec,
                runtime,
                registry,
                snapshot,
                forwarding,
                router,
                module,
                module_rx,
                client,
                client_rx,
            }
        }

        fn drain(&self, budget: Duration) -> tokio::task::JoinHandle<StopNotice> {
            let spec = self.spec.clone();
            let runtime = self.runtime.clone();
            let registry = self.registry.clone();
            let snapshot = self.snapshot.clone();
            tokio::spawn(async move {
                begin_forwarding_drain_with_timeout(
                    &spec,
                    &runtime,
                    &registry,
                    &snapshot,
                    None,
                    RouteCloseReason::Restart,
                    budget,
                )
                .await
                .unwrap()
            })
        }

        async fn ready(&self, sender: &RouteCtx, corr: u64) {
            self.router
                .route_for_connection(
                    sender,
                    frame(FrameType::Request, corr, json!({"op":"module.drain_ready"})),
                )
                .await
                .unwrap();
        }

        async fn bind(&mut self, corr: u64) -> (u16, u32) {
            let (open_result, ()) = tokio::join!(
                self.router.route_for_connection(&self.client, open(corr)),
                async {
                    let bind = next(&mut self.module_rx).await;
                    assert_eq!(body(&bind)["op"], "route.bind");
                    self.router
                        .route_for_connection(
                            &self.module,
                            frame(
                                FrameType::Response,
                                bind.header.corr,
                                json!({"op":"route.bind"}),
                            ),
                        )
                        .await
                        .unwrap();
                }
            );
            open_result.unwrap();
            let opened = next(&mut self.client_rx).await;
            assert_eq!(opened.header.ty, FrameType::Response, "{:?}", body(&opened));
            let opened = body(&opened);
            (
                opened["route_channel"].as_u64().unwrap() as u16,
                opened["route_epoch"].as_u64().unwrap() as u32,
            )
        }
    }

    #[tokio::test(start_paused = true)]
    async fn accepting_drain_admits_new_routes_and_requests_until_ready_then_closes() {
        let mut rig = Rig::new(true).await;
        let drain = rig.drain(LONG_BUDGET);
        let notice = next(&mut rig.module_rx).await;
        assert_eq!(body(&notice)["op"], "module.draining");
        assert_eq!(body(&notice)["accepting"], true);
        assert_eq!(
            lock_snapshot(&rig.snapshot).unwrap().state,
            ModuleState::Draining
        );
        assert!(rig.forwarding.module_is_accepting_drain(ID).unwrap());

        rig.ready(&rig.client, 2).await;
        assert_eq!(
            body(&next(&mut rig.client_rx).await)["code"],
            "not_registered"
        );
        let (channel, epoch) = rig.bind(3).await;
        let request = |corr| {
            Frame::build(
                FrameType::Request,
                Flags(0),
                channel,
                epoch,
                corr,
                b"work".to_vec(),
            )
            .unwrap()
        };
        rig.router
            .route_for_connection(&rig.client, request(4))
            .await
            .unwrap();
        let held = next(&mut rig.module_rx).await;
        assert_eq!(held.body, b"work");

        rig.ready(&rig.module, 5).await;
        rig.ready(&rig.module, 6).await;
        for corr in [5, 6] {
            let ack = next(&mut rig.module_rx).await;
            assert_eq!(ack.header.ty, FrameType::Response);
            assert_eq!(ack.header.corr, corr);
            assert_eq!(body(&ack), json!({"op":"module.drain_ready"}));
        }
        assert!(!rig.forwarding.module_is_accepting_drain(ID).unwrap());
        let closing = next(&mut rig.client_rx).await;
        assert_eq!(body(&closing)["op"], "route.closing");
        rig.router
            .route_for_connection(&rig.client, open(7))
            .await
            .unwrap();
        assert_eq!(
            body(&next(&mut rig.client_rx).await)["code"],
            "module_reloading"
        );
        let refused = rig
            .router
            .route_for_connection(&rig.client, request(8))
            .await
            .unwrap_err();
        assert!(
            matches!(refused, crate::router::RouterError::RouteError { code, .. } if code == "module_reloading")
        );
        assert!(rig.module_rx.try_recv().is_err());

        let response = Frame::build(
            FrameType::Response,
            held.header.flags,
            held.header.channel,
            held.header.epoch,
            held.header.corr,
            b"finished".to_vec(),
        )
        .unwrap();
        rig.router
            .route_for_connection(&rig.module, response)
            .await
            .unwrap();
        assert_eq!(next(&mut rig.client_rx).await.body, b"finished");
        let closed = next(&mut rig.client_rx).await;
        assert_eq!(body(&closed)["op"], "route.closed");
        assert_eq!(body(&closed)["drained"], true);
        assert_eq!(
            drain.await.unwrap(),
            StopNotice::AcceptingDrain {
                sent_over_connection: true
            }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn accepting_drain_budget_expiry_closes_admission() {
        let mut rig = Rig::new(true).await;
        let started = Instant::now();
        let drain = rig.drain(LONG_BUDGET);
        assert_eq!(body(&next(&mut rig.module_rx).await)["accepting"], true);
        let (channel, epoch) = rig.bind(2).await;
        let held = Frame::build(
            FrameType::Request,
            Flags(0),
            channel,
            epoch,
            3,
            b"held".to_vec(),
        )
        .unwrap();
        rig.router
            .route_for_connection(&rig.client, held)
            .await
            .unwrap();
        assert_eq!(next(&mut rig.module_rx).await.body, b"held");
        tokio::time::advance(LONG_BUDGET - Duration::from_secs(1)).await;
        assert!(rig.forwarding.module_is_accepting_drain(ID).unwrap());
        assert!(!drain.is_finished());
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(body(&next(&mut rig.client_rx).await)["op"], "route.closing");
        assert_eq!(started.elapsed(), LONG_BUDGET);
        assert!(!rig.forwarding.module_is_accepting_drain(ID).unwrap());
        tokio::time::advance(Duration::from_secs(4)).await;
        assert!(rig.client_rx.try_recv().is_err());
        tokio::time::advance(Duration::from_secs(1)).await;
        let closed = body(&next(&mut rig.client_rx).await);
        assert_eq!(closed["op"], "route.closed");
        assert_eq!(closed["drained"], false);
        assert_eq!(
            drain.await.unwrap(),
            StopNotice::AcceptingDrain {
                sent_over_connection: true
            }
        );
        assert_eq!(started.elapsed(), LONG_BUDGET + Duration::from_secs(5));
        let goodbye = loop {
            let frame = next(&mut rig.module_rx).await;
            if frame.header.channel == 0 {
                break frame;
            }
        };
        assert_eq!(
            (goodbye.header.ty, goodbye.header.channel),
            (FrameType::Goodbye, 0)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn accepting_drain_disconnect_does_not_wait_for_the_budget() {
        let mut rig = Rig::new(true).await;
        let started = Instant::now();
        let drain = rig.drain(LONG_BUDGET);
        assert_eq!(body(&next(&mut rig.module_rx).await)["accepting"], true);
        rig.forwarding
            .cleanup_connection(rig.module.connection_id)
            .unwrap();
        rig.registry
            .deregister_connection(rig.module.connection_id)
            .unwrap();
        assert_eq!(
            drain.await.unwrap(),
            StopNotice::AcceptingDrain {
                sent_over_connection: false
            }
        );
        assert_eq!(started.elapsed(), Duration::ZERO);
        assert!(!rig.forwarding.module_is_accepting_drain(ID).unwrap());
    }

    #[tokio::test(start_paused = true)]
    async fn accepting_drain_hands_teardown_to_daemon_shutdown_without_waiting() {
        let mut rig = Rig::new(true).await;
        let started = Instant::now();
        let restart = tokio::spawn({
            let spec = rig.spec.clone();
            let runtime = rig.runtime.clone();
            let registry = rig.registry.clone();
            let snapshot = rig.snapshot.clone();
            async move {
                super::super::restart::restart_child(
                    &spec,
                    &runtime,
                    &registry,
                    &SupervisorProcessLiveness::new(),
                    &snapshot,
                    &mut None,
                    LONG_BUDGET,
                )
                .await
            }
        });
        assert_eq!(body(&next(&mut rig.module_rx).await)["accepting"], true);
        let endpoint = rig.forwarding.active_module_endpoint(ID).unwrap().unwrap();
        assert_eq!(
            rig.forwarding.begin_daemon_drain().unwrap(),
            vec![(ID.into(), endpoint)]
        );
        restart.await.unwrap().unwrap();
        assert_eq!(started.elapsed(), Duration::ZERO);
        assert!(rig.registry.get_module(ID).unwrap().is_some());
        assert!(
            rig.module_rx.try_recv().is_err(),
            "only shutdown sends the GOODBYE"
        );
        assert!(!lock_snapshot(&rig.snapshot).unwrap().respawn_pending);
    }

    #[tokio::test(start_paused = true)]
    async fn accepting_drain_ready_outside_a_drain_is_refused_without_changing_admission() {
        let mut rig = Rig::new(true).await;
        rig.ready(&rig.module, 2).await;
        let refusal = next(&mut rig.module_rx).await;
        assert_eq!(refusal.header.ty, FrameType::Error);
        assert_eq!(body(&refusal)["code"], "invalid_request");
        assert_eq!(
            lock_snapshot(&rig.snapshot).unwrap().state,
            ModuleState::Running
        );
        rig.bind(3).await;
    }

    #[tokio::test(start_paused = true)]
    async fn accepting_drain_now_and_legacy_drains_never_open_the_accepting_phase() {
        for (opted_in, budget) in [(true, Duration::ZERO), (false, LONG_BUDGET)] {
            let mut rig = Rig::new(opted_in).await;
            let (channel, _) = rig.bind(2).await;
            let started = Instant::now();
            let drain = rig.drain(budget);
            let notice = next(&mut rig.module_rx).await;
            assert_eq!(body(&notice)["op"], "module.draining");
            assert!(body(&notice).get("accepting").is_none());
            assert!(!rig.forwarding.module_is_accepting_drain(ID).unwrap());
            assert_eq!(body(&next(&mut rig.client_rx).await)["op"], "route.closing");
            let closed = next(&mut rig.client_rx).await;
            assert_eq!(body(&closed)["op"], "route.closed");
            assert_eq!(body(&closed)["channels"], json!([channel]));
            assert_eq!(drain.await.unwrap(), StopNotice::SentOverConnection);
            assert_eq!(started.elapsed(), Duration::ZERO);
        }
    }

    #[tokio::test]
    async fn accepting_drain_ready_restarts_the_supervised_child() {
        let mut rig = Rig::new(true).await;
        let home = cortexkit_test_support::ScratchDir::new("accepting-drain-restart");
        let spec = ModuleSpec {
            module_id: ID.into(),
            ..serving_spec(home.path(), ModuleProtocol::Subc)
        };
        let policy = RestartPolicy::new(3, Duration::ZERO);
        let supervisor = Supervisor::new_for_test(rig.registry.clone(), policy)
            .with_forwarding(rig.forwarding.clone());
        let module = supervisor
            .supervise_configured_with_health(
                spec,
                true,
                HealthConfig::default(),
                Some(LONG_BUDGET.as_millis() as u64),
                policy,
            )
            .unwrap();
        let before = module.status().unwrap();
        let pid = rustix::process::Pid::from_raw(before.pid.unwrap() as i32).unwrap();
        let restart = tokio::spawn({
            let module = module.clone();
            async move { module.restart(Some(60_000)).await }
        });
        let notice = next(&mut rig.module_rx).await;
        assert_eq!(body(&notice)["accepting"], true);
        let remaining_ms = body(&notice)["deadline_ms"]
            .as_u64()
            .unwrap()
            .saturating_sub(unix_ms_now());
        assert!(
            (50_000..=60_000).contains(&remaining_ms),
            "restart override sets the accepting budget: {remaining_ms}"
        );
        rig.ready(&rig.module, 2).await;
        assert_eq!(
            next(&mut rig.module_rx).await.header.ty,
            FrameType::Response
        );
        assert_eq!(next(&mut rig.module_rx).await.header.ty, FrameType::Goodbye);
        // Stand in for the module closing its registration and then exiting.
        rig.forwarding
            .cleanup_connection(rig.module.connection_id)
            .unwrap();
        rig.registry
            .deregister_connection(rig.module.connection_id)
            .unwrap();
        rustix::process::kill_process(pid, rustix::process::Signal::TERM).unwrap();
        timeout(Duration::from_secs(10), restart)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let after = timeout(Duration::from_secs(10), async {
            loop {
                let status = module.status().unwrap();
                if status.spawn_generation > before.spawn_generation
                    && status.state == ModuleState::Running
                {
                    break status;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(after.spawn_generation, before.spawn_generation + 1);
        assert_ne!(after.pid, before.pid);
        module.stop().await.unwrap();
    }
}
