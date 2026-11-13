fn recovery_module(
    registry: &Arc<Registry>,
    module_id: &str,
    env: Vec<(String, String)>,
    max_restarts: u32,
) -> (SupervisorHandle, crate::supervise::SupervisedModule) {
    let handle = SupervisorHandle::new();
    let supervisor = Supervisor::new_for_test(
        Arc::clone(registry),
        RestartPolicy::new(max_restarts, Duration::ZERO),
    )
    .with_handle(handle.clone());
    let module = supervisor
        .supervise_configured(
            ModuleSpec {
                module_id: module_id.into(),
                program: fake_aft_stub_path(),
                args: vec![],
                env,
                reserved: false,
                reserved_prefixes: vec![],
                protocol: ModuleProtocol::Subc,
                overlap: Default::default(),
                carry_containment: false,
            },
            true,
        )
        .unwrap();
    (handle, module)
}

#[tokio::test]
async fn route_open_disabled_module_is_not_expected_back() {
    let registry = Arc::new(Registry::default());
    let (supervisor, module) = recovery_module(
        &registry,
        "disabled-recovery",
        vec![("FAKE_AFT_NEVER_CONNECT".into(), "1".into())],
        1,
    );
    module.set_enabled(false).await.unwrap();
    assert_eq!(module.state().unwrap(), ModuleState::Disabled);
    let handler = ControlHandler::new(registry).with_supervisor(supervisor);
    let (ctx, _rx) = route_ctx(ConnectionId::new(2001));
    let response = handler
        .handle_control_frame(
            &ctx,
            route_open_frame(
                1,
                module.module_id(),
                unique_project_root("disabled-recovery"),
            ),
        )
        .await
        .unwrap();
    let error = parse_error(&response[0]);
    assert_eq!(error["code"], "target_unavailable");
    assert_eq!(error["detail"]["state"], "disabled");
    assert_eq!(error["detail"]["expected_back"], false);
}

#[tokio::test]
async fn route_open_exhausted_restart_budget_is_not_expected_back() {
    let registry = Arc::new(Registry::default());
    let (supervisor, module) = recovery_module(
        &registry,
        "exhausted-recovery",
        vec![("FAKE_AFT_EXIT_CODE".into(), "23".into())],
        1,
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        while module.state().unwrap() != ModuleState::Failed {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(module.status().unwrap().restart_count, 1);
    assert_eq!(module.terminal_history().entries.len(), 2);
    let handler = ControlHandler::new(registry).with_supervisor(supervisor);
    let (ctx, _rx) = route_ctx(ConnectionId::new(2002));
    let response = handler
        .handle_control_frame(
            &ctx,
            route_open_frame(
                1,
                module.module_id(),
                unique_project_root("exhausted-recovery"),
            ),
        )
        .await
        .unwrap();
    let error = parse_error(&response[0]);
    assert_eq!(error["code"], "target_unavailable");
    assert_eq!(error["detail"]["state"], "failed");
    assert_eq!(error["detail"]["expected_back"], false);
}

#[tokio::test]
async fn route_open_stopped_without_restart_budget_is_not_expected_back() {
    let registry = Arc::new(Registry::default());
    let (supervisor, module) = recovery_module(
        &registry,
        "stopped-recovery",
        vec![("FAKE_AFT_NEVER_CONNECT".into(), "1".into())],
        0,
    );
    module.stop().await.unwrap();
    assert_eq!(module.state().unwrap(), ModuleState::Stopped);
    assert!(module.status().unwrap().enabled);
    let handler = ControlHandler::new(registry).with_supervisor(supervisor);
    let (ctx, _rx) = route_ctx(ConnectionId::new(2008));
    let response = handler
        .handle_control_frame(
            &ctx,
            route_open_frame(
                1,
                module.module_id(),
                unique_project_root("stopped-recovery"),
            ),
        )
        .await
        .unwrap();
    let error = parse_error(&response[0]);
    assert_eq!(error["code"], "target_unavailable");
    assert_eq!(error["detail"]["state"], "stopped");
    assert_eq!(error["detail"]["expected_back"], false);
}

#[tokio::test]
async fn route_open_declared_not_ready_preserves_reason_and_is_expected_back() {
    for supervised in [false, true] {
        let registry = Arc::new(Registry::default());
        let (supervisor, module) = recovery_module(
            &registry,
            "not-ready-recovery",
            vec![("FAKE_AFT_NEVER_CONNECT".into(), "1".into())],
            0,
        );
        let mut handler = ControlHandler::new(registry);
        if supervised {
            handler = handler.with_supervisor(supervisor);
        }
        let (module_ctx, mut module_rx) = route_ctx(ConnectionId::new(2003));
        let mut hello = hello_frame("not-ready-recovery", PROTOCOL_VERSION, 1);
        let mut body: Value = serde_json::from_slice(&hello.body).unwrap();
        body["manifest"]["ready"] = json!(false);
        hello.body = serde_json::to_vec(&body).unwrap();
        hello_via_sink(&handler, &module_ctx, &mut module_rx, hello).await;
        let (ctx, _rx) = route_ctx(ConnectionId::new(2004));
        let response = handler
            .handle_control_frame(
                &ctx,
                route_open_frame(
                    2,
                    module.module_id(),
                    unique_project_root("not-ready-recovery"),
                ),
            )
            .await
            .unwrap();
        let error = parse_error(&response[0]);
        handler
            .cleanup_connection(module_ctx.connection_id)
            .unwrap();
        module.stop().await.unwrap();
        assert_eq!(error["code"], "module_warming");
        assert_eq!(error["detail"]["reason"], "declared_not_ready");
        if supervised {
            assert_eq!(error["detail"]["state"], "running");
            assert_eq!(error["detail"]["expected_back"], true);
        } else {
            assert!(error["detail"].get("state").is_none());
            assert!(error["detail"].get("expected_back").is_none());
        }
    }
}

#[tokio::test]
async fn route_open_registered_availability_refusals_report_supervised_recovery_only() {
    struct ReplacingLiveness(bool);
    impl crate::supervise::ModuleProcessLiveness for ReplacingLiveness {
        fn process_live(&self, _module_id: &str) -> Option<bool> {
            Some(false)
        }
        fn process_replacing(&self, _module_id: &str) -> bool {
            self.0
        }
    }

    for supervised in [false, true] {
        for reason in [
            "registration_not_active",
            "supervisor_not_live",
            "no_forwarding_connection",
            "forwarding_draining",
            "process_replacing",
        ] {
            let reloading = matches!(reason, "forwarding_draining" | "process_replacing");
            let registry = Arc::new(Registry::default());
            let (supervisor, module) = recovery_module(
                &registry,
                "registered-recovery",
                vec![("FAKE_AFT_NEVER_CONNECT".into(), "1".into())],
                if reloading { 0 } else { 1 },
            );
            let mut handler = ControlHandler::new(Arc::clone(&registry));
            if supervised {
                handler = handler.with_supervisor(supervisor);
            }
            let (module_ctx, mut module_rx) = route_ctx(ConnectionId::new(2006));
            hello_via_sink(
                &handler,
                &module_ctx,
                &mut module_rx,
                hello_frame(module.module_id(), PROTOCOL_VERSION, 1),
            )
            .await;
            match reason {
                "registration_not_active" => {
                    registry
                        .set_module_state_for_test(module.module_id(), ChannelState::Closed)
                        .unwrap();
                }
                "supervisor_not_live" | "process_replacing" => {
                    handler = handler.with_process_liveness(Arc::new(ReplacingLiveness(reloading)));
                }
                "no_forwarding_connection" => {
                    handler
                        .forwarding
                        .cleanup_connection(module_ctx.connection_id)
                        .unwrap();
                }
                "forwarding_draining" => {
                    handler
                        .forwarding
                        .begin_module_drain(module.module_id(), RouteCloseReason::Reload)
                        .unwrap()
                        .unwrap();
                }
                _ => unreachable!(),
            }
            let (ctx, _rx) = route_ctx(ConnectionId::new(2007));
            let response = handler
                .handle_control_frame(
                    &ctx,
                    route_open_frame(2, module.module_id(), unique_project_root(reason)),
                )
                .await
                .unwrap();
            let error = parse_error(&response[0]);
            handler
                .cleanup_connection(module_ctx.connection_id)
                .unwrap();
            module.stop().await.unwrap();
            assert_eq!(
                error["code"],
                if reloading {
                    "module_reloading"
                } else {
                    "target_unavailable"
                },
                "{reason}"
            );
            if supervised {
                assert_eq!(error["detail"]["state"], "running", "{reason}");
                assert_eq!(error["detail"]["expected_back"], true, "{reason}");
            } else {
                assert!(error.get("detail").is_none(), "{reason}: {error}");
            }
        }
    }
}
