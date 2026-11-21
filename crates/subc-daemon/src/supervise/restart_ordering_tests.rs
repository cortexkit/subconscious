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
