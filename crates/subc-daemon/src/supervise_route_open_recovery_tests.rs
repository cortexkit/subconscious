async fn route_open_for_recovery_snapshot(
    state: ModuleState,
    respawn_pending: bool,
    max_restarts: u32,
) -> serde_json::Value {
    use crate::{
        control::ControlHandler,
        registry::ConnectionId,
        router::{FrameSink, RouteCtx},
    };
    use subc_protocol::{Flags, Frame, FrameType};

    let registry = Arc::new(Registry::default());
    let handle = SupervisorHandle::new();
    let supervisor = Supervisor::new_for_test(
        Arc::clone(&registry),
        RestartPolicy::new(max_restarts, Duration::ZERO),
    )
    .with_handle(handle.clone());
    let module = supervisor
        .supervise_configured(
            ModuleSpec {
                module_id: "recovery-snapshot".into(),
                program: fake_aft_stub_path(),
                args: vec![],
                env: vec![],
                reserved: false,
                reserved_prefixes: vec![],
                protocol: ModuleProtocol::Subc,
                overlap: Default::default(),
                carry_containment: false,
            },
            false,
        )
        .unwrap();
    let monitor = module.inner.monitor.lock().unwrap().take().unwrap();
    monitor.abort();
    assert!(monitor.await.unwrap_err().is_cancelled());
    update_snapshot(
        &module.inner.snapshot,
        Some(module.module_id()),
        |snapshot| {
            snapshot.state = state;
            snapshot.enabled = true;
            snapshot.respawn_pending = respawn_pending;
        },
    )
    .unwrap();
    let handler = ControlHandler::new(registry).with_supervisor(handle);
    let (tx, _rx) = mpsc::channel(8);
    let ctx = RouteCtx {
        connection_id: ConnectionId::new(2005),
        egress: FrameSink::new(tx),
    };
    let frame = Frame::build(
        FrameType::Request,
        Flags(0),
        0,
        0,
        1,
        serde_json::to_vec(&serde_json::json!({
            "op": "route.open",
            "target": { "kind": "tool_provider", "module_id": module.module_id() },
            "identity": { "harness": "unit", "session": "recovery" },
        }))
        .unwrap(),
    )
    .unwrap();
    let replies = handler.handle_control_frame(&ctx, frame).await.unwrap();
    assert_eq!(replies[0].header.ty, FrameType::Error);
    serde_json::from_slice(&replies[0].body).unwrap()
}

#[tokio::test]
async fn route_open_starting_module_is_expected_back() {
    // supervise_configured creates SupervisorSnapshot::starting() with enabled
    // true before its first process is admitted by spawn_and_mark_running.
    let error = route_open_for_recovery_snapshot(ModuleState::Starting, false, 1).await;
    assert_eq!(error["code"], "module_warming");
    assert_eq!(error["detail"]["state"], "starting");
    assert_eq!(error["detail"]["expected_back"], true);
}

#[tokio::test]
async fn route_open_stopped_with_respawn_pending_is_expected_back() {
    // SupervisorCommand::Drain passes Stopped and enabled=None to
    // drain_optional_child, retaining an already-reserved respawn flag until
    // command processing ends. A pending respawn takes precedence over budget.
    let error = route_open_for_recovery_snapshot(ModuleState::Stopped, true, 0).await;
    assert_eq!(error["code"], "target_unavailable");
    assert_eq!(error["detail"]["state"], "stopped");
    assert_eq!(error["detail"]["expected_back"], true);
}
