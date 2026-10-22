use super::*;
use crate::supervise::RestartPolicy;
use cortexkit_test_support::ScratchDir;
use subc_control::{ActivityActor, ActivityQueryRequest, ActivityRecordRequest};
#[cfg(unix)]
use tokio::time::timeout;

fn query() -> ActivityQueryRequest {
    serde_json::from_value(serde_json::json!({"op":"activity.query", "limit":1000})).unwrap()
}

async fn flush(handler: &ControlHandler) {
    let body: ActivityRecordRequest = serde_json::from_value(serde_json::json!({"op":"activity.record", "kind":"barrier.flushed", "summary":"Queue flushed"})).unwrap();
    handler
        .activity
        .as_ref()
        .unwrap()
        .record(
            "barrier".into(),
            body,
            ActivityActor {
                principal: Principal::Direct,
                scope_owner: None,
                agent_id: None,
                run_id: None,
                flow_id: None,
                project_root: None,
                workdir: None,
                project_id: None,
            },
            1,
        )
        .await
        .unwrap();
}

async fn request(
    handler: &ControlHandler,
    ctx: &RouteCtx,
    body: ClientControlRequest,
) -> Vec<Frame> {
    handler
        .handle_client_control_request(
            ctx,
            Frame::build(FrameType::Request, control_flags(), 0, 0, 1, Vec::new()).unwrap(),
            body,
            &mut None,
        )
        .await
        .unwrap()
}

#[cfg(unix)]
fn spec(home: &ScratchDir, protocol: subc_control::ModuleProtocol) -> ModuleSpec {
    ModuleSpec {
        module_id: "lifecycle-worker".into(),
        program: "/bin/sleep".into(),
        args: vec!["600".into()],
        env: ["XDG_DATA_HOME", "XDG_RUNTIME_DIR", "XDG_CONFIG_HOME"]
            .into_iter()
            .map(|key| (key.into(), home.path().display().to_string()))
            .collect(),
        reserved: false,
        reserved_prefixes: vec![],
        protocol,
        overlap: Default::default(),
    }
}

#[cfg(unix)]
#[tokio::test]
async fn lifecycle_requested_restart_disable_refusal_and_noop_use_verified_requester() {
    let home = ScratchDir::new("lifecycle-requester");
    let registry = Arc::new(Registry::default());
    let handle = SupervisorHandle::new();
    let supervisor = Supervisor::new_for_test(
        registry.clone(),
        RestartPolicy::new(2, Duration::from_millis(300)),
    )
    .with_handle(handle.clone())
    .with_drain_timeout(Duration::ZERO);
    let handler = ControlHandler::new(registry.clone())
        .with_supervisor(handle.clone())
        .with_activity_ledger(home.path(), "requests".into());
    let module = supervisor
        .supervise_configured(spec(&home, subc_control::ModuleProtocol::None), true)
        .unwrap();
    let (ctx, _rx) = route_ctx(ConnectionId::new(303));
    registry
        .register_with_control_ops(
            manifest("caller", PROTOCOL_VERSION),
            PROTOCOL_VERSION,
            ctx.connection_id,
            vec![],
        )
        .unwrap();
    let mut feed = handler
        .activity
        .as_ref()
        .unwrap()
        .subscribe(None)
        .await
        .unwrap();
    let reply = request(
        &handler,
        &ctx,
        ClientControlRequest::SupervisorRestart {
            module_id: module.module_id().into(),
            drain_timeout_ms: Some(0),
        },
    )
    .await;
    assert_eq!(reply[0].header.ty, FrameType::Response);
    assert!(
        handler
            .activity
            .as_ref()
            .unwrap()
            .query(query())
            .await
            .unwrap()
            .entries
            .is_empty(),
        "initiation ack must not record completion"
    );
    let restarted = timeout(Duration::from_secs(5), feed.receiver.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(restarted.kind, "subc.module_restarted");
    assert_eq!(
        restarted.actor.principal,
        Principal::Reserved {
            module_id: "caller".into()
        }
    );
    assert_eq!(
        module.state().unwrap(),
        crate::supervise::ModuleState::Running
    );
    request(
        &handler,
        &ctx,
        ClientControlRequest::SupervisorSetEnabled {
            module_id: module.module_id().into(),
            enabled: false,
        },
    )
    .await;
    let disabled = timeout(Duration::from_secs(5), feed.receiver.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(disabled.kind, "subc.module_disabled");
    assert_eq!(disabled.actor.principal, restarted.actor.principal);
    assert_eq!(
        module.state().unwrap(),
        crate::supervise::ModuleState::Disabled
    );
    let noop = request(
        &handler,
        &ctx,
        ClientControlRequest::SupervisorSetEnabled {
            module_id: module.module_id().into(),
            enabled: false,
        },
    )
    .await;
    assert_eq!(
        serde_json::from_slice::<Value>(&noop[0].body).unwrap()["applied"],
        false
    );
    let refused = request(
        &handler,
        &ctx,
        ClientControlRequest::SupervisorRestart {
            module_id: module.module_id().into(),
            drain_timeout_ms: None,
        },
    )
    .await;
    assert_eq!(refused[0].header.ty, FrameType::Error);
    let unknown = request(
        &handler,
        &ctx,
        ClientControlRequest::SupervisorSetEnabled {
            module_id: "unknown".into(),
            enabled: false,
        },
    )
    .await;
    assert_eq!(unknown[0].header.ty, FrameType::Error);
    flush(&handler).await;
    let entries = handler
        .activity
        .as_ref()
        .unwrap()
        .query(query())
        .await
        .unwrap()
        .entries;
    assert_eq!(
        entries
            .iter()
            .filter(|entry| entry.kind.starts_with("subc."))
            .count(),
        2
    );
}

/// The running-image evidence `supervisor.provenance` reports for `module`,
/// read once the supervisor is willing to report it.
///
/// On macOS the child first runs the `ck-subc` launch trampoline, and status
/// withholds the pid (so the image check reads `NotRunning`) until the
/// supervision loop has processed the trampoline's exec acknowledgement. That
/// acknowledgement is handled asynchronously, so a status read made as soon
/// as the module has registered can come back before it. Both the started
/// activity entry and status wait for exec confirmation. This waits for that withheld state
/// to end, yielding so the supervision task can run, and fails at once on any
/// other non-match. Elsewhere the pid is never withheld, so the first read
/// already matches.
#[cfg(unix)]
async fn reported_running_image(
    module: &crate::supervise::SupervisedModule,
) -> subc_control::RunningImageEvidence {
    loop {
        let (status, image) = module.status_and_running_image_agreement().await.unwrap();
        match image {
            subc_control::RunningImageAgreement::Match { evidence } => return evidence,
            subc_control::RunningImageAgreement::Unavailable {
                reason: subc_control::RunningImageUnavailableReason::NotRunning,
            } if status.pid.is_none()
                && status.state == crate::supervise::ModuleState::Running =>
            {
                tokio::task::yield_now().await
            }
            other => panic!("running image never matched: {other:?} with status {status:?}"),
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn lifecycle_registration_on_new_build_without_requester_matches_provenance() {
    let home = ScratchDir::new("lifecycle-registration");
    let registry = Arc::new(Registry::default());
    let handle = SupervisorHandle::new();
    let supervisor = Supervisor::new_for_test(registry.clone(), RestartPolicy::new(0, Duration::ZERO))
        .with_handle(handle.clone())
        .with_drain_timeout(Duration::ZERO);
    let handler = ControlHandler::new(registry)
        .with_supervisor(handle)
        .with_activity_ledger(home.path(), "registration".into());
    let module = supervisor
        .supervise_configured(spec(&home, subc_control::ModuleProtocol::Subc), true)
        .unwrap();
    let mut feed = handler
        .activity
        .as_ref()
        .unwrap()
        .subscribe(None)
        .await
        .unwrap();
    let (ctx, mut rx) = route_ctx(ConnectionId::new(7));
    hello_via_sink(
        &handler,
        &ctx,
        &mut rx,
        hello_frame(module.module_id(), PROTOCOL_VERSION, 1),
    )
    .await;
    let started = timeout(Duration::from_secs(5), feed.receiver.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(started.kind, "subc.module_started");
    assert_eq!(
        started.actor.principal,
        Principal::Reserved {
            module_id: "subc".into()
        }
    );
    let evidence = timeout(Duration::from_secs(5), reported_running_image(&module))
        .await
        .expect("sleep running image must have provenance evidence once exec is confirmed");
    assert_eq!(
        started.details["new_evidence"],
        serde_json::to_value(evidence).unwrap()
    );
    assert_eq!(started.details["old_evidence"], Value::Null);
    assert_eq!(
        handler.activity.as_ref().unwrap().lifecycle_digest_skips(),
        0
    );
    handler.cleanup_connection(ctx.connection_id).unwrap();
    module.stop().await.unwrap();
}

#[tokio::test]
async fn lifecycle_rescan_preview_noop_and_changes_record_only_applied_roster() {
    let home = ScratchDir::new("lifecycle-rescan");
    let config = home.path().join("daemon.jsonc");
    let registry = Arc::new(Registry::default());
    let handle = SupervisorHandle::new();
    let supervisor = Supervisor::new_for_test(registry.clone(), RestartPolicy::new(0, Duration::ZERO))
        .with_handle(handle.clone());
    let handler = ControlHandler::new(registry)
        .with_supervisor(handle)
        .with_activity_ledger(home.path(), "rescan".into())
        .with_supervisor_rescan(supervisor, &config, None);
    let (ctx, _rx) = route_ctx(ConnectionId::new(8));
    // Disabled modules exercise roster changes without platform-specific processes.
    std::fs::write(
        &config,
        r#"{"version":1,"modules":{"old":{"program":"unused", "enabled":false},"keep":{"program":"unused", "enabled":false}}}"#,
    )
    .unwrap();
    let preview = request(
        &handler,
        &ctx,
        ClientControlRequest::SupervisorRescan { preview: true },
    )
    .await;
    assert_eq!(preview[0].header.ty, FrameType::Response);
    assert!(handler.supervisor.list().is_empty());
    flush(&handler).await;
    assert!(!handler
        .activity
        .as_ref()
        .unwrap()
        .query(query())
        .await
        .unwrap()
        .entries
        .iter()
        .any(|entry| entry.kind.starts_with("subc.")));
    request(
        &handler,
        &ctx,
        ClientControlRequest::SupervisorRescan { preview: false },
    )
    .await;
    request(
        &handler,
        &ctx,
        ClientControlRequest::SupervisorRescan { preview: false },
    )
    .await;
    flush(&handler).await;
    let entries = handler
        .activity
        .as_ref()
        .unwrap()
        .query(query())
        .await
        .unwrap()
        .entries;
    let config_changes = entries
        .iter()
        .filter(|entry| entry.kind == "subc.config_changed")
        .collect::<Vec<_>>();
    assert_eq!(
        config_changes.len(),
        1,
        "a no-change rescan adds no config activity entry"
    );
    assert_eq!(config_changes[0].summary, "settings changed");
    assert_eq!(
        config_changes[0].details,
        serde_json::json!({"added":["keep", "old"]})
            .as_object()
            .unwrap()
            .clone()
    );
    assert_eq!(config_changes[0].actor.principal, Principal::Direct);

    std::fs::write(
        &config,
        r#"{"version":1,"modules":{"keep":{"program":"changed", "enabled":false},"new":{"program":"unused", "enabled":false}}}"#,
    )
    .unwrap();
    request(
        &handler,
        &ctx,
        ClientControlRequest::SupervisorRescan { preview: true },
    )
    .await;
    flush(&handler).await;
    let preview_entries = handler
        .activity
        .as_ref()
        .unwrap()
        .query(query())
        .await
        .unwrap()
        .entries;
    assert_eq!(
        preview_entries
            .iter()
            .filter(|entry| entry.kind == "subc.config_changed")
            .count(),
        1,
        "a preview is silent"
    );

    request(
        &handler,
        &ctx,
        ClientControlRequest::SupervisorRescan { preview: false },
    )
    .await;
    flush(&handler).await;
    let entries = handler
        .activity
        .as_ref()
        .unwrap()
        .query(query())
        .await
        .unwrap()
        .entries;
    let config_changes = entries
        .iter()
        .filter(|entry| entry.kind == "subc.config_changed")
        .collect::<Vec<_>>();
    assert_eq!(config_changes.len(), 2);
    assert_eq!(config_changes[0].summary, "settings changed");
    assert_eq!(
        config_changes[0].details,
        serde_json::json!({"added":["new"], "removed":["old"], "changed":["keep"]})
            .as_object()
            .unwrap()
            .clone()
    );
    assert!(config_changes
        .iter()
        .all(|entry| entry.actor.principal == Principal::Direct));

    let rescans = entries
        .iter()
        .filter(|entry| entry.kind == "subc.module_rescanned")
        .collect::<Vec<_>>();
    assert_eq!(rescans.len(), 2);
    assert_eq!(
        rescans[0].details,
        serde_json::json!({"added":["new"], "removed":["old"]})
            .as_object()
            .unwrap()
            .clone()
    );
    assert_eq!(
        rescans[1].details,
        serde_json::json!({"added":["keep", "old"], "removed":[]})
            .as_object()
            .unwrap()
            .clone()
    );
    assert!(rescans
        .iter()
        .all(|entry| entry.actor.principal == Principal::Direct));
    let stopped = entries
        .iter()
        .filter(|entry| entry.kind == "subc.module_stopped")
        .collect::<Vec<_>>();
    assert_eq!(stopped.len(), 1);
    assert_eq!(stopped[0].actor.principal, Principal::Direct);
}

#[tokio::test(flavor = "current_thread")]
async fn lifecycle_dropped_append_warns_kind_and_describe_reports_counters() {
    let home = ScratchDir::new("lifecycle-describe");
    let mut handler = ControlHandler::new(Arc::new(Registry::default()));
    let pause = Arc::new((Mutex::new(true), std::sync::Condvar::new()));
    let ledger = crate::activity::ActivityLedger::start(
        home.path().join("activity.jsonl"),
        "describe".into(),
        1,
        crate::activity::WriterHooks {
            pause: Some(pause.clone()),
            ..Default::default()
        },
    );
    handler.activity = Some(ledger.clone());
    let capture = EventCapture::default();
    let _subscriber =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(capture.clone()));
    ledger.module_transition("subc.module_disabled", "worker", Principal::Direct);
    ledger.module_transition("subc.module_stopped", "worker", Principal::Direct);
    ledger.skip_start_digest();
    let frame = Frame::build(FrameType::Request, control_flags(), 0, 0, 1, Vec::new()).unwrap();
    let response = handler.handle_server_describe(frame).unwrap();
    let body: Value = serde_json::from_slice(&response[0].body).unwrap();
    assert_eq!(body["counters"]["activity_lifecycle_append_failures"], 1);
    assert_eq!(body["counters"]["activity_started_digest_skips"], 1);
    assert!(body["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .any(|capability| capability == "activity-ledger/v1"));
    let warning = capture
        .events()
        .into_iter()
        .find(|event| {
            event.fields.get("message").map(String::as_str)
                == Some("lifecycle activity append failed or dropped")
        })
        .expect("dropped lifecycle warning");
    assert_eq!(warning.level, tracing::Level::WARN);
    assert_eq!(
        warning.fields.get("kind").map(String::as_str),
        Some("subc.module_stopped")
    );
    *pause.0.lock().unwrap() = false;
    pause.1.notify_all();
}

#[cfg(unix)]
#[tokio::test]
async fn lifecycle_swap_records_at_cutover_not_candidate_start_or_refusal() {
    let home = ScratchDir::new("lifecycle-swap");
    let registry = Arc::new(Registry::default());
    let forwarding = Arc::new(ForwardingTable::default());
    let handle = SupervisorHandle::new();
    let supervisor = Supervisor::new_for_test(registry.clone(), RestartPolicy::new(0, Duration::ZERO))
        .with_handle(handle.clone())
        .with_forwarding(forwarding.clone())
        .with_drain_timeout(Duration::ZERO);
    let handler = ControlHandler::with_forwarding(registry.clone(), forwarding.clone())
        .with_supervisor(handle.clone())
        .with_activity_ledger(home.path(), "swap".into());
    let mut launch = spec(&home, subc_control::ModuleProtocol::Subc);
    launch.overlap = crate::supervise::ModuleOverlap::Safe;
    let module = supervisor.supervise_configured(launch, true).unwrap();
    let (incumbent, mut incumbent_rx) = route_ctx(ConnectionId::new(20));
    hello_via_sink(
        &handler,
        &incumbent,
        &mut incumbent_rx,
        hello_frame(module.module_id(), PROTOCOL_VERSION, 1),
    )
    .await;
    let (operator, _operator_rx) = route_ctx(ConnectionId::new(21));
    registry
        .register_with_control_ops(
            manifest("swapper", PROTOCOL_VERSION),
            PROTOCOL_VERSION,
            operator.connection_id,
            vec![],
        )
        .unwrap();
    let (candidate, _candidate_rx) = route_ctx(ConnectionId::new(22));
    let (reply, ()) = tokio::join!(
        request(
            &handler,
            &operator,
            ClientControlRequest::SupervisorSwap {
                module_id: module.module_id().into(),
                ready_timeout_ms: Some(5000)
            }
        ),
        async {
            timeout(Duration::from_secs(5), async {
                while !handle.swap_open(module.module_id()) {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            flush(&handler).await;
            assert!(
                !handler
                    .activity
                    .as_ref()
                    .unwrap()
                    .query(query())
                    .await
                    .unwrap()
                    .entries
                    .iter()
                    .any(|entry| entry.kind == "subc.module_swapped"),
                "candidate warm-up is not a completed swap"
            );
            let mut manifest = manifest(module.module_id(), PROTOCOL_VERSION);
            manifest.ready = Some(true);
            registry
                .register_candidate_with_control_ops(
                    manifest,
                    PROTOCOL_VERSION,
                    candidate.connection_id,
                    vec![],
                )
                .unwrap();
            forwarding
                .register_candidate_module_connection(
                    candidate.connection_id,
                    module.module_id().into(),
                    PROTOCOL_VERSION,
                    Concurrency::ModuleManaged,
                    candidate.egress.clone(),
                )
                .unwrap();
        }
    );
    assert_eq!(reply[0].header.ty, FrameType::Response);
    flush(&handler).await;
    let swapped = handler
        .activity
        .as_ref()
        .unwrap()
        .query(query())
        .await
        .unwrap()
        .entries
        .into_iter()
        .filter(|entry| entry.kind == "subc.module_swapped")
        .collect::<Vec<_>>();
    assert_eq!(swapped.len(), 1);
    assert_eq!(
        swapped[0].actor.principal,
        Principal::Reserved {
            module_id: "swapper".into()
        }
    );
    assert_eq!(
        registry
            .get_module(module.module_id())
            .unwrap()
            .unwrap()
            .connection_id,
        candidate.connection_id
    );
    handler.cleanup_connection(incumbent.connection_id).unwrap();
    handler.cleanup_connection(candidate.connection_id).unwrap();
    request(
        &handler,
        &operator,
        ClientControlRequest::SupervisorSetEnabled {
            module_id: module.module_id().into(),
            enabled: false,
        },
    )
    .await;
    let refused = request(
        &handler,
        &operator,
        ClientControlRequest::SupervisorSwap {
            module_id: module.module_id().into(),
            ready_timeout_ms: Some(1),
        },
    )
    .await;
    assert_eq!(refused[0].header.ty, FrameType::Error);
    flush(&handler).await;
    assert_eq!(
        handler
            .activity
            .as_ref()
            .unwrap()
            .query(query())
            .await
            .unwrap()
            .entries
            .iter()
            .filter(|entry| entry.kind == "subc.module_swapped")
            .count(),
        1
    );
}

#[cfg(unix)]
#[tokio::test]
async fn lifecycle_supervision_never_waits_on_a_paused_lifecycle_writer() {
    let home = ScratchDir::new("lifecycle-nonblocking");
    let registry = Arc::new(Registry::default());
    let handle = SupervisorHandle::new();
    let pause = Arc::new((Mutex::new(true), std::sync::Condvar::new()));
    let ledger = crate::activity::ActivityLedger::start(
        home.path().join("activity.jsonl"),
        "paused".into(),
        1,
        crate::activity::WriterHooks {
            pause: Some(pause.clone()),
            ..Default::default()
        },
    );
    handle.set_activity(Some(ledger.clone()));
    let supervisor = Supervisor::new_for_test(registry, RestartPolicy::new(0, Duration::ZERO))
        .with_handle(handle)
        .with_drain_timeout(Duration::ZERO);
    let module = supervisor
        .supervise_configured(spec(&home, subc_control::ModuleProtocol::None), true)
        .unwrap();
    let before = module.status().unwrap().spawn_generation;
    let began = std::time::Instant::now();
    module
        .restart_requested(Some(0), Some(Principal::Direct))
        .await
        .unwrap();
    timeout(Duration::from_secs(5), async {
        while module.status().unwrap().spawn_generation == before {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    module
        .stop_requested(Some(Principal::Direct))
        .await
        .unwrap();
    assert!(
        began.elapsed() < Duration::from_secs(1),
        "supervision waited for activity storage"
    );
    assert_eq!(
        module.state().unwrap(),
        crate::supervise::ModuleState::Stopped
    );
    assert_eq!(
        ledger.lifecycle_failures(),
        1,
        "second transition drops rather than waiting on a full queue"
    );
    *pause.0.lock().unwrap() = false;
    pause.1.notify_all();
}

#[cfg(unix)]
#[tokio::test]
async fn lifecycle_crash_restarts_and_unrequested_exits_record_nothing() {
    let home = ScratchDir::new("lifecycle-crashes");
    for code in [0, 1] {
        let registry = Arc::new(Registry::default());
        let handle = SupervisorHandle::new();
        let handler = ControlHandler::new(registry.clone())
            .with_supervisor(handle.clone())
            .with_activity_ledger(&home.path().join(format!("exit-{code}")), "exit".into());
        let supervisor =
            Supervisor::new_for_test(registry, RestartPolicy::new(2, Duration::ZERO)).with_handle(handle);
        let mut launch = spec(&home, subc_control::ModuleProtocol::None);
        launch.program = "/bin/sh".into();
        launch.args = vec!["-c".into(), format!("exit {code}")];
        let module = supervisor.supervise_configured(launch, true).unwrap();
        timeout(Duration::from_secs(5), async {
            while !matches!(
                module.state().unwrap(),
                crate::supervise::ModuleState::Stopped | crate::supervise::ModuleState::Failed
            ) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        if code == 1 {
            assert!(
                module.status().unwrap().spawn_generation > 1,
                "test must reach an actual crash restart"
            );
        }
        flush(&handler).await;
        assert!(!handler
            .activity
            .as_ref()
            .unwrap()
            .query(query())
            .await
            .unwrap()
            .entries
            .iter()
            .any(|entry| entry.kind.starts_with("subc.")));
    }
}

#[cfg(unix)]
#[tokio::test]
async fn lifecycle_requested_enable_stamps_requester_on_registered_start() {
    let home = ScratchDir::new("lifecycle-requested-start");
    let registry = Arc::new(Registry::default());
    let handle = SupervisorHandle::new();
    let supervisor = Supervisor::new_for_test(registry.clone(), RestartPolicy::new(0, Duration::ZERO))
        .with_handle(handle.clone())
        .with_drain_timeout(Duration::ZERO);
    let handler = ControlHandler::new(registry.clone())
        .with_supervisor(handle)
        .with_activity_ledger(home.path(), "start".into());
    let module = supervisor
        .supervise_configured(spec(&home, subc_control::ModuleProtocol::Subc), false)
        .unwrap();
    let (caller, _caller_rx) = route_ctx(ConnectionId::new(31));
    registry
        .register_with_control_ops(
            manifest("caller", PROTOCOL_VERSION),
            PROTOCOL_VERSION,
            caller.connection_id,
            vec![],
        )
        .unwrap();
    let mut feed = handler
        .activity
        .as_ref()
        .unwrap()
        .subscribe(None)
        .await
        .unwrap();
    request(
        &handler,
        &caller,
        ClientControlRequest::SupervisorSetEnabled {
            module_id: module.module_id().into(),
            enabled: true,
        },
    )
    .await;
    let (provider, mut provider_rx) = route_ctx(ConnectionId::new(32));
    hello_via_sink(
        &handler,
        &provider,
        &mut provider_rx,
        hello_frame(module.module_id(), PROTOCOL_VERSION, 1),
    )
    .await;
    let start = timeout(Duration::from_secs(5), feed.receiver.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(start.kind, "subc.module_started");
    assert_eq!(
        start.actor.principal,
        Principal::Reserved {
            module_id: "caller".into()
        }
    );
    handler.cleanup_connection(provider.connection_id).unwrap();
    module.stop().await.unwrap();
}

#[cfg(target_os = "macos")]
async fn held_exec_registration(refuse: bool, immediate_exit: bool) -> Vec<subc_control::ActivityEntry> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let home = ScratchDir::new("held-exec-registration");
    let registry = Arc::new(Registry::default());
    let handle = SupervisorHandle::new();
    let supervisor = Supervisor::new_for_test(registry.clone(), RestartPolicy::new(0, Duration::ZERO))
        .with_handle(handle.clone())
        .with_drain_timeout(Duration::ZERO)
        .with_live_children_record(home.join("live-children.json"));
    let handler = ControlHandler::new(registry)
        .with_supervisor(handle)
        .with_activity_ledger(home.path(), "held-exec".into());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut launch = spec(&home, subc_control::ModuleProtocol::Subc);
    if immediate_exit {
        launch.program = "/bin/sh".into();
        launch.args = vec!["-c".into(), "exit 0".into()];
    }
    launch.env.push(("SUBC_TEST_BEFORE_EXEC_CONFIRM".into(), listener.local_addr().unwrap().to_string()));
    let module = supervisor.spawn(launch).unwrap();
    let (mut pause, _) = timeout(Duration::from_secs(5), listener.accept()).await.unwrap().unwrap();
    let mut ready = [0];
    timeout(Duration::from_secs(5), pause.read_exact(&mut ready)).await.unwrap().unwrap();
    assert_eq!(&ready, b"R");
    let mut pid = [0; 4];
    pause.read_exact(&mut pid).await.unwrap();
    if immediate_exit {
        tokio::task::spawn_blocking(move || {
            subc_os::fork_exec_test::wait_for_child_exit_without_reaping(u32::from_ne_bytes(pid))
        }).await.unwrap().unwrap();
    }
    // Exercise accepted HELLO and the real writer queue while confirmation is
    // held, rather than constructing a lifecycle append directly.
    let (ctx, mut rx) = route_ctx(ConnectionId::new(901));
    hello_via_sink(&handler, &ctx, &mut rx, hello_frame(module.module_id(), PROTOCOL_VERSION, 1)).await;
    pause.write_all(if refuse { b"F" } else { b"X" }).await.unwrap();
    timeout(Duration::from_secs(5), flush(&handler)).await.unwrap();
    let starts = handler.activity.as_ref().unwrap().query(query()).await.unwrap().entries
        .into_iter().filter(|entry| entry.kind == "subc.module_started").collect();
    handler.cleanup_connection(ctx.connection_id).unwrap();
    module.stop().await.unwrap();
    starts
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn refused_exec_never_appends_module_started() {
    assert!(held_exec_registration(true, false).await.is_empty());
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn confirmed_exec_registration_appends_start() {
    assert_eq!(held_exec_registration(false, false).await.len(), 1);
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn immediate_successful_exit_is_not_refusal() {
    assert_eq!(held_exec_registration(false, true).await.len(), 1);
}
