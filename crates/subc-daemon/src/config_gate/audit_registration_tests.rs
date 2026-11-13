use super::*;

#[tokio::test]
async fn audit_registration_non_hold_enable_error_fails_reconciliation() {
    let fixture = legacy().await;
    fixture.module.set_enabled(false).await.unwrap();
    fixture.owner.exhaust_delivery_spawns_for_test();
    fixture.rig.stamp(&fixture.owner);
    let result = fixture
        .handler
        .reconcile_cutover_for_test(fixture.owner.clone(), 1)
        .await;
    let status = fixture.owner.polled_status().unwrap();
    drop(fixture.primary);
    fixture.module.stop().await.unwrap();
    let error =
        result.expect_err("an unrecorded internal gate failure must fail apply reconciliation");
    assert_eq!(error.code, "rescan_failed");
    assert!(
        error.detail.contains("delivery_spawn_exhausted"),
        "{error:?}"
    );
    assert!(
        status.gated_modules.is_empty(),
        "internal failure is not an admission hold"
    );
}

#[tokio::test]
async fn audit_registration_approved_enable_resets_the_operator_crash_budget() {
    let fixture = legacy().await;
    fixture.module.set_enabled(false).await.unwrap();
    fixture.module.seed_crash_budget_for_test(2);
    fixture.rig.stamp(&fixture.owner);
    fixture
        .handler
        .reconcile_cutover_for_test(fixture.owner.clone(), 1)
        .await
        .unwrap();
    let status = fixture.module.status().unwrap();
    drop(fixture.primary);
    fixture.module.stop().await.unwrap();
    assert!(status.enabled && status.process_alive);
    assert_eq!(
        status.restart_count, 0,
        "an approved disabled-to-enabled transition is an operator enable"
    );
}

fn entry(rig: &Rig, field: &str, value: serde_json::Value) {
    let path = rig.path.join("subc.jsonc");
    let mut document: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    document["modules"]["a"][field] = value;
    std::fs::write(path, serde_json::to_vec(&document).unwrap()).unwrap();
}

async fn until(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !condition() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("supervisor transition must complete");
}

struct Legacy {
    rig: Rig,
    owner: Arc<RuntimeOwner>,
    handle: SupervisorHandle,
    module: crate::supervise::SupervisedModule,
    handler: Arc<crate::ControlHandler>,
    registry: Arc<crate::Registry>,
    router: Arc<crate::Router>,
    primary: crate::control::claims::tests::Net,
    nonce: String,
}

#[tokio::test]
async fn parking_poison_keeps_bootstrap_async_failed_module_monitored() {
    parking_poison_startup(false, false).await;
}

#[tokio::test]
async fn parking_poison_keeps_bootstrap_sync_failed_module_monitored() {
    parking_poison_startup(false, true).await;
}

#[tokio::test]
async fn parking_poison_keeps_approved_failed_module_monitored() {
    parking_poison_startup(true, false).await;
}

async fn parking_poison_startup(strict: bool, synchronous: bool) {
    let rig = rig();
    let owner = Arc::new(rig.owner("poison-during-startup-park"));
    rig.stamp(&owner);
    std::fs::write(
        rig.path.join("store").join(crate::config_store::STORE_FILE),
        b"tampered",
    )
    .unwrap();
    assert!(owner.check_recovery().unwrap());
    assert!(owner.has_recorded_admission_hold("a"));
    let handle = channel_handle();
    let supervisor =
        Supervisor::new_for_test(Arc::new(crate::Registry::default()), Default::default())
            .with_handle(handle.clone())
            .with_config_runtime(owner.clone());
    let registered = supervisor
        .register_configured_with_health(
            spec(&rig),
            true,
            Default::default(),
            None,
            Default::default(),
            Duration::from_secs(5),
        )
        .unwrap();
    let module = handle.get("a").unwrap();
    let poison = owner.clone();
    *owner.config_park_hook.lock().unwrap() = Some(Box::new(move || {
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = poison.inner.lock().unwrap();
            panic!("injected owner poison between hold classification and parking");
        }))
        .is_err());
    }));
    let result = if strict {
        supervisor
            .start_registered_approved_async(registered, subc_protocol::Principal::Direct)
            .await
    } else if synchronous {
        supervisor.start_registered_configured(registered, None, true)
    } else {
        supervisor
            .start_registered_configured_async(registered, None, true)
            .await
    };
    let state = module.status().unwrap();
    // This command must traverse the monitor even for a terminal Failed module;
    // stop() would return early without demonstrating a live command receiver.
    let monitored = module.config_exit_hold().await;
    let _ = module.stop().await;
    assert_eq!(
        state.state,
        ModuleState::Failed,
        "parking failure must finalize startup"
    );
    assert!(
        monitored.is_ok(),
        "the retained module must have its monitor: {monitored:?}"
    );
    if strict {
        assert!(result
            .as_ref()
            .is_err_and(|error| error.to_string().contains("config_unavailable")));
    } else {
        assert!(
            result.is_ok(),
            "bootstrap must tolerate the finalized failure"
        );
    }
}

async fn legacy() -> Legacy {
    use crate::control::claims::tests::Net;
    let rig = rig();
    wire_fixture_document(&rig, json!(["600"]));
    entry(&rig, "overlap", json!("safe"));
    let owner = Arc::new(rig.owner("legacy-cutover"));
    let handle = channel_handle();
    let registry = Arc::new(crate::Registry::default());
    let forwarding = Arc::new(crate::ForwardingTable::default());
    let supervisor = Supervisor::new_for_test(registry.clone(), RestartPolicy::default())
        .with_handle(handle.clone())
        .with_forwarding(forwarding.clone())
        .with_config_runtime(owner.clone())
        .with_drain_timeout(Duration::from_millis(20));
    let module = supervisor.spawn_async(spec(&rig)).await.unwrap();
    let nonce = handle.spawn_launch_nonce_for("a").unwrap();
    let handler = Arc::new(
        crate::ControlHandler::with_forwarding(registry.clone(), forwarding)
            .with_supervisor(handle.clone())
            .with_supervisor_rescan(supervisor, rig.path.join("subc.jsonc"), None)
            .with_config_runtime(owner.clone()),
    );
    let router = Arc::new(crate::Router::with_control_handler(handler.clone()));
    let mut primary = Net::new(router.clone()).await;
    let hello = subc_protocol::ModuleHelloBody {
        manifest: subc_protocol::manifest::ModuleManifest::builder("a", "1.0.0").build(),
        protocol_ver: subc_protocol::PROTOCOL_VERSION,
        control_ops: None,
        launch_nonce: Some(nonce.clone()),
    };
    let hello = subc_protocol::Frame::build(
        subc_protocol::FrameType::Hello,
        subc_protocol::Flags::new(false, subc_protocol::Priority::Passive, false),
        0,
        0,
        1,
        serde_json::to_vec(&hello).unwrap(),
    )
    .unwrap();
    assert_eq!(
        primary.call(hello).await.header.ty,
        subc_protocol::FrameType::HelloAck
    );
    Legacy {
        rig,
        owner,
        handle,
        module,
        handler,
        registry,
        router,
        primary,
        nonce,
    }
}

#[tokio::test]
async fn audit_registration_wire_start_keeps_unrecorded_gate_error_code() {
    operator_gate_error_reply("start").await;
}

#[tokio::test]
async fn audit_registration_wire_enable_keeps_unrecorded_gate_error_code() {
    operator_gate_error_reply("enable").await;
}

#[tokio::test]
async fn audit_registration_wire_restart_keeps_unrecorded_gate_error_code() {
    operator_gate_error_reply("restart").await;
}

#[tokio::test]
async fn audit_registration_wire_reload_keeps_unrecorded_gate_error_code() {
    operator_gate_error_reply("reload").await;
}

async fn operator_gate_error_reply(operation: &str) {
    let rig = rig();
    let owner = Arc::new(rig.owner("operator-error-code"));
    rig.stamp(&owner);
    let handle = channel_handle();
    let registry = Arc::new(crate::Registry::default());
    let supervisor = Supervisor::new_for_test(registry.clone(), RestartPolicy::default())
        .with_handle(handle.clone())
        .with_config_runtime(owner.clone());
    let mut unapproved = spec(&rig);
    unapproved.module_id = "not-approved".into();
    let module = supervisor
        .supervise_configured_requested_async(
            unapproved,
            operation != "enable",
            Default::default(),
            None,
            Default::default(),
            Duration::from_secs(5),
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        module.state().unwrap(),
        if operation == "enable" {
            ModuleState::Disabled
        } else {
            ModuleState::Failed
        }
    );
    assert!(!owner.has_recorded_admission_hold("not-approved"));
    // Exercise conversion of a supervisor error into a control-protocol reply,
    // not the handler's earlier preflight. The supervisor has the real gate.
    let handler = Arc::new(crate::ControlHandler::new(registry).with_supervisor(handle));
    registration_declaration_poll(handler.clone(), "supervisor.list").await;
    let request = if matches!(operation, "restart" | "reload") {
        json!({"op":format!("supervisor.{operation}"),"module_id":"not-approved"})
    } else {
        json!({"op":"supervisor.set_enabled","module_id":"not-approved","enabled":true})
    };
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    let ctx = crate::RouteCtx {
        connection_id: ConnectionId::new(50),
        egress: crate::FrameSink::new(tx),
    };
    let frames = handler
        .handle_control_frame(&ctx, crate::control::claims::tests::request(request))
        .await
        .unwrap();
    let frame = if frames.is_empty() {
        rx.recv().await.unwrap().frame
    } else {
        frames[0].clone()
    };
    let reply: serde_json::Value = serde_json::from_slice(&frame.body).unwrap();
    module.stop().await.unwrap();
    assert_eq!(reply["code"], "config_gated", "{operation}: {reply}");
    assert!(
        reply["message"]
            .as_str()
            .unwrap()
            .contains("module is not approved"),
        "{reply}"
    );
    assert!(!owner.has_recorded_admission_hold("not-approved"));
}

#[tokio::test]
async fn audit_registration_committed_stamp_retries_or_parks_cutover_without_failed_apply() {
    for mode in ["busy", "recovery", "refused"] {
        let fixture = legacy().await;
        let old = fixture.module.status().unwrap();
        if mode == "recovery" {
            fixture.owner.lock().unwrap().recovery_exit_hold = true;
        }
        fixture.rig.stamp(&fixture.owner);
        let gate = fixture.owner.module_gate("a").unwrap();
        let held = if mode == "busy" {
            Some(gate.lock_owned().await)
        } else {
            None
        };
        if mode == "refused" {
            std::fs::write(fixture.rig.path.join("changed"), b"unapproved program").unwrap();
            std::fs::rename(
                fixture.rig.path.join("changed"),
                fixture.rig.path.join("program"),
            )
            .unwrap();
        }
        fixture.handler.reconcile_cutover_for_test(fixture.owner.clone(), 1).await
            .expect("a committed stamp must not become a failed apply when replacement admission defers or refuses");
        let status = fixture.module.status().unwrap();
        assert!(
            !status.process_alive || status.pid != old.pid,
            "legacy child remains: {status:?}"
        );
        assert!(!fixture.handle.legacy_nonce_present_for_test("a"));
        assert!(!fixture
            .handle
            .spawned_consumer_authorized("a", &fixture.nonce));
        if mode == "busy" {
            assert!(fixture.owner.stamp_channel_cutover_pending());
            assert_eq!(status.state, ModuleState::Restarting);
            drop(held);
            until(|| fixture.module.status().unwrap().process_alive).await;
            fixture
                .handler
                .reconcile_cutover_for_test(fixture.owner.clone(), 1)
                .await
                .unwrap();
            assert!(!fixture.owner.stamp_channel_cutover_pending());
        } else {
            drop(held);
            assert!(!fixture.owner.stamp_channel_cutover_pending());
        }
        drop(fixture.primary);
        fixture.module.stop().await.unwrap();
    }
}

#[tokio::test]
async fn audit_registration_stamp_protocol_none_stops_the_running_nonce_child() {
    let fixture = legacy().await;
    let old = fixture.module.status().unwrap();
    entry(&fixture.rig, "protocol", json!("none"));
    fixture.rig.stamp(&fixture.owner);
    fixture
        .handler
        .reconcile_cutover_for_test(fixture.owner.clone(), 1)
        .await
        .unwrap();
    let next = fixture.module.status().unwrap();
    assert!(next.process_alive);
    assert_ne!(next.pid, old.pid);
    assert_eq!(next.protocol, subc_control::ModuleProtocol::None);
    assert!(!next.legacy_registration_spawn);
    assert!(!fixture.handle.legacy_nonce_present_for_test("a"));
    drop(fixture.primary);
    fixture.module.stop().await.unwrap();
}

#[tokio::test]
async fn audit_registration_unstamp_completes_with_stamp_cutover_pending() {
    let fixture = legacy().await;
    fixture.rig.stamp(&fixture.owner);
    assert!(fixture.owner.stamp_channel_cutover_pending());
    let generation = approve(&fixture.rig, &fixture.owner, PlanKind::Unstamp);
    assert!(!fixture.owner.stamp_channel_cutover_pending());
    fixture
        .handler
        .reconcile_cutover_for_test(fixture.owner.clone(), generation)
        .await
        .unwrap();
    assert_eq!(fixture.owner.lock().unwrap().authority_exit, None);
    assert_eq!(fixture.owner.polled_status().unwrap().state, "unstamped");
    assert!(!fixture.owner.is_recovering());
    drop(fixture.primary);
    fixture.module.stop().await.unwrap();
}

#[tokio::test]
async fn audit_registration_recovery_restamp_reports_commit_while_actor_reconciles() {
    let fixture = legacy().await;
    let connection = ConnectionId::new(500);
    fixture.handler.record_local_connection(connection);
    fixture.handler.record_captured_peer(
        connection,
        subc_os::local_peer::ObservedPeer::Unobserved.into(),
    );
    fixture.handler.record_observed_peer(
        connection,
        subc_os::local_peer::ObservedPeer::Observed(subc_os::local_peer::PeerObservation {
            user: subc_os::local_peer::PeerUser::Uid(subc_os::effective_uid()),
            pid: std::process::id(),
            exe: None,
            signing_id: None,
        }),
    );
    fixture.rig.stamp(&fixture.owner);
    std::fs::write(
        fixture
            .rig
            .path
            .join("store")
            .join(crate::config_store::STORE_FILE),
        b"tampered",
    )
    .unwrap();
    assert!(fixture.owner.check_recovery().unwrap());
    assert_eq!(
        fixture.owner.polled_status().unwrap().state,
        "store_unverified"
    );
    let gate = fixture.owner.module_gate("a").unwrap();
    let held = gate.lock_owned().await;
    let generation = fixture.rig.stamp(&fixture.owner);
    assert_eq!(generation, 1);
    assert_eq!(
        fixture.owner.lock().unwrap().store.loaded_status().state,
        crate::config_store::StoreState::Stamped
    );

    // Keep the replacement behind its module gate so reconciliation must
    // acknowledge the committed stamp with the actor still waiting to start.
    fixture
        .handler
        .reconcile_cutover_for_test(fixture.owner.clone(), generation)
        .await
        .unwrap();
    let actor = fixture.module.status().unwrap();
    let pending = fixture.owner.stamp_channel_cutover_pending();
    let status = registration_declaration_diagnostic(&fixture.handler, "config.status").await;
    let refreshed = fixture.owner.status().unwrap();
    let admission_held = fixture.owner.exit_is_held("a");
    drop(held);
    drop(fixture.primary);
    fixture.module.stop().await.unwrap();

    assert_eq!(actor.state, ModuleState::Restarting);
    assert!(!actor.process_alive);
    assert!(pending);
    assert!(admission_held);
    assert_eq!(status["status"]["state"], "stamped", "{status}");
    assert_eq!(refreshed.state, "stamped");
    assert!(
        status["status"]["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|warning| warning.as_str().is_some_and(
                |warning| warning.contains("waiting for registration channel cutover")
            )),
        "{status}"
    );
}

#[cfg(all(feature = "test-support", feature = "dev-desktop-approval"))]
#[tokio::test]
async fn audit_registration_committed_stamp_enable_program_refusal_still_replies_success() {
    committed_enable_reply(false, false).await;
}

#[cfg(all(feature = "test-support", feature = "dev-desktop-approval"))]
#[tokio::test]
async fn audit_registration_non_hold_enable_error_returns_rescan_failed_from_apply() {
    committed_enable_reply(true, false).await;
}

#[cfg(all(feature = "test-support", feature = "dev-desktop-approval"))]
#[tokio::test]
async fn audit_registration_new_module_spawn_error_returns_rescan_failed_from_apply() {
    committed_enable_reply(true, true).await;
}

#[cfg(all(feature = "test-support", feature = "dev-desktop-approval"))]
async fn committed_enable_reply(non_hold_error: bool, absent_from_roster: bool) {
    committed_enable_ordered(non_hold_error, absent_from_roster, false).await;
}

#[cfg(all(feature = "test-support", feature = "dev-desktop-approval"))]
#[tokio::test]
async fn approved_creation_worker_first_failure_cannot_be_reconciled_as_success() {
    committed_enable_ordered(true, true, true).await;
}

#[cfg(all(feature = "test-support", feature = "dev-desktop-approval"))]
async fn committed_enable_ordered(
    non_hold_error: bool,
    absent_from_roster: bool,
    worker_first: bool,
) {
    struct Presence;
    impl crate::operator_confirm::OperatorProvider for Presence {
        fn config_gate_ready(&self) -> bool {
            true
        }
        fn prompt(
            &self,
            _: &str,
            _: Box<dyn FnOnce(Arc<dyn crate::operator_confirm::OperatorWithdraw>) + Send>,
        ) -> crate::operator_confirm::ProviderResult {
            unreachable!()
        }
        fn prompt_gate(
            &self,
            _: &str,
            _: Box<dyn FnOnce(Arc<dyn crate::operator_confirm::OperatorWithdraw>) + Send>,
        ) -> crate::operator_confirm::ProviderResult {
            crate::operator_confirm::ProviderResult::Approved
        }
    }
    let fixture = legacy().await;
    fixture.module.set_enabled(false).await.unwrap();
    if absent_from_roster {
        fixture.module.stop().await.unwrap();
        fixture.handle.retire("a").unwrap();
        assert!(fixture.handle.get("a").is_none());
    }
    let handler = Arc::new(
        (*fixture.handler)
            .clone()
            .with_config_os_prompt_policy_for_tests(),
    );
    handler
        .forwarding()
        .operator_confirms()
        .configure(Arc::new(Presence), Default::default());
    registration_declaration_poll(handler.clone(), "config.status").await;
    handler.record_local_connection(ConnectionId::new(50));
    handler.record_captured_peer(
        ConnectionId::new(50),
        subc_os::local_peer::ObservedPeer::Unobserved.into(),
    );
    registration_declaration_poll(handler.clone(), "config.status").await;
    let plan = fixture
        .owner
        .finish_plan(
            fixture.owner.prepare(PlanKind::Stamp).unwrap(),
            daemon_review,
        )
        .unwrap();
    fixture
        .owner
        .desktop_view_delivered(ConnectionId::new(50), &plan)
        .unwrap();
    if non_hold_error {
        fixture.owner.exhaust_delivery_spawns_for_test();
    } else {
        let program = fixture.rig.path.join("program");
        fixture.owner.lock().unwrap().publication_hook = Some(Box::new(move || {
            let mut bytes = std::fs::read(&program).unwrap();
            bytes.extend_from_slice(b"post-approval change");
            std::fs::write(program, bytes).unwrap();
        }));
    }
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    let ctx = crate::RouteCtx {
        connection_id: ConnectionId::new(50),
        egress: crate::FrameSink::new(tx),
    };
    let (reached, arrived) = tokio::sync::oneshot::channel();
    let (release, proceed) = tokio::sync::oneshot::channel();
    if worker_first {
        handler.pause_apply_reconcile_for_test(reached, proceed);
    }
    let apply_handler = handler.clone();
    let apply = tokio::spawn(async move {
        apply_handler
            .handle_control_frame(
                &ctx,
                crate::control::claims::tests::request(
                    json!({"op":"config.apply","digest":plan.digest}),
                ),
            )
            .await
            .unwrap()
    });
    if worker_first {
        arrived.await.unwrap();
        let worker = handler.spawn_config_delivery_worker().unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if fixture
                    .handle
                    .get("a")
                    .is_some_and(|module| module.state().unwrap() == ModuleState::Failed)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the real worker must create and fail the absent actor before the apply pass");
        fixture
            .handle
            .get("a")
            .unwrap()
            .config_exit_hold()
            .await
            .unwrap();
        worker.abort();
        let _ = worker.await;
        release.send(()).unwrap();
    }
    let frames = apply.await.unwrap();
    let frame = if frames.is_empty() {
        rx.recv().await.unwrap().frame
    } else {
        frames[0].clone()
    };
    let reply: serde_json::Value = serde_json::from_slice(&frame.body).unwrap();
    let reconciled = fixture
        .handle
        .get("a")
        .expect("even a failed new actor must stay in the roster");
    reconciled.config_exit_hold().await.unwrap();
    let parked = reconciled.status().unwrap();
    drop(fixture.primary);
    reconciled.stop().await.unwrap();
    assert!(parked.enabled);
    assert!(!parked.process_alive);
    assert_eq!(parked.restart_count, 0);
    if non_hold_error {
        assert_eq!(reply["code"], "rescan_failed", "{reply}");
        assert!(
            reply["message"]
                .as_str()
                .unwrap()
                .contains("delivery_spawn_exhausted"),
            "{reply}"
        );
        assert_eq!(parked.state, ModuleState::Failed);
        if worker_first {
            let (_, _, _, approved) = fixture
                .owner
                .authority_exit_configuration(1)
                .unwrap()
                .expect("the failed generation must remain unreconciled");
            assert!(approved.changes_entry("a"));
            assert!(
                reconciled
                    .reconcile_approved_configuration(approved)
                    .await
                    .is_err(),
                "every reconcile must retain the generation's launch failure"
            );
        }
        assert!(fixture.owner.check_module_hold("a").is_ok());
        assert!(fixture
            .owner
            .polled_status()
            .unwrap()
            .gated_modules
            .is_empty());
    } else {
        assert_eq!(reply["op"], "config.apply", "{reply}");
        assert_eq!(reply["generation"], 1, "{reply}");
        assert_eq!(parked.state, ModuleState::Stopped);
        assert!(fixture.owner.check_module_hold("a").is_err());
    }
}

#[tokio::test]
async fn audit_registration_pending_cutover_worker_uses_retry_backoff() {
    use std::future::Future;
    use std::task::Poll;
    let fixture = legacy().await;
    fixture.rig.stamp(&fixture.owner);
    let gate = fixture.owner.module_gate("a").unwrap();
    let held = gate.lock_owned().await;
    tokio::time::pause();
    let changed = fixture.owner.delivery_notifications();
    let mut notification = std::pin::pin!(changed.notified());
    let _ = std::future::poll_fn(|cx| Poll::Ready(notification.as_mut().poll(cx))).await;
    let (tx, mut wakes) = tokio::sync::mpsc::unbounded_channel();
    let handler = Arc::new((*fixture.handler).clone().with_delivery_wakes_for_test(tx));
    let worker = handler.spawn_config_delivery_worker().unwrap();
    let mut timers = Vec::new();
    let mut notifications = 0;
    while timers.len() < 4 {
        let (cause, at) = wakes.recv().await.unwrap();
        if cause == "timer" {
            timers.push(at);
        } else {
            notifications += 1;
            assert!(
                notifications <= 1,
                "worker consumed its own reconciliation wakeups"
            );
        }
    }
    for (pair, seconds) in timers.windows(2).zip([2, 4, 8]) {
        assert!(
            pair[1] >= pair[0] + Duration::from_secs(seconds),
            "{timers:?}"
        );
    }
    assert!(fixture.owner.stamp_channel_cutover_pending());
    worker.abort();
    let _ = worker.await;
    drop(held);
    drop(fixture.primary);
    fixture.module.stop().await.unwrap();
}

#[tokio::test]
async fn audit_registration_candidate_promotion_after_stamp_cannot_restore_a_nonce() {
    use crate::control::claims::tests::Net;
    let fixture = legacy().await;
    let swapping = {
        let module = fixture.module.clone();
        tokio::spawn(async move { module.swap(Some(Duration::from_secs(10))).await })
    };
    until(|| {
        fixture
            .handle
            .registration_for_test("a")
            .is_some_and(|spawn| spawn.is_candidate())
    })
    .await;
    let proof = fixture.handle.registration_proof_for_test("a").unwrap();
    let mut candidate = Net::new(fixture.router.clone()).await;
    let hello = module_hello(Some(proof), ConnectionId::new(2));
    let mut body: serde_json::Value = serde_json::from_slice(&hello.body).unwrap();
    body["manifest"]["ready"] = json!(false);
    let hello = subc_protocol::Frame::build(
        hello.header.ty,
        hello.header.flags,
        0,
        0,
        1,
        serde_json::to_vec(&body).unwrap(),
    )
    .unwrap();
    assert_eq!(
        candidate.call(hello).await.header.ty,
        subc_protocol::FrameType::HelloAck
    );
    fixture.rig.stamp(&fixture.owner);
    fixture
        .handle
        .apply_identity_configuration(&spec(&fixture.rig));
    assert!(!fixture.handle.legacy_nonce_present_for_test("a"));
    fixture
        .registry
        .replace_catalog_for_connection(ConnectionId::new(2), vec![], None, Some(true))
        .unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(5), swapping)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(outcome, crate::supervise::SwapOutcome::Cutover);
    assert!(
        !fixture.handle.legacy_nonce_present_for_test("a"),
        "promotion restored a nonce after the stamp retired it"
    );
    fixture
        .handler
        .reconcile_cutover_for_test(fixture.owner.clone(), 1)
        .await
        .unwrap();
    assert!(!fixture.module.status().unwrap().legacy_registration_spawn);
    drop(candidate);
    drop(fixture.primary);
    fixture.module.stop().await.unwrap();
}

#[tokio::test]
async fn audit_registration_missing_support_holds_crash_budget_and_reapproval_revives() {
    let rig = rig();
    wire_fixture_document(&rig, json!(["600"]));
    let owner = Arc::new(rig.owner("declaration-release"));
    rig.stamp(&owner);
    let handle = channel_handle();
    let registry = Arc::new(crate::Registry::default());
    let supervisor = Supervisor::new_for_test(registry.clone(), RestartPolicy::default())
        .with_handle(handle.clone())
        .with_config_runtime(owner.clone());
    let module = supervisor.spawn_async(spec(&rig)).await.unwrap();
    module.seed_crash_budget_for_test(2);
    let before = module.status().unwrap();
    assert_eq!(before.restart_count, 2);
    entry(&rig, "registration_channel", json!(false));
    approve(&rig, &owner, PlanKind::Change);
    assert!(owner.exit_is_held("a"));
    let handler = Arc::new(
        crate::ControlHandler::new(registry.clone())
            .with_supervisor(handle.clone())
            .with_config_runtime(owner.clone()),
    );
    let running = registration_declaration_poll(handler.clone(), "supervisor.provenance").await;
    assert_eq!(
        running["modules"][0]["registration_declaration"], "declared",
        "{running}"
    );
    rustix::process::kill_process(
        rustix::process::Pid::from_raw(before.pid.unwrap() as i32).unwrap(),
        rustix::process::Signal::KILL,
    )
    .unwrap();
    until(|| !module.status().unwrap().process_alive).await;
    until(|| module.status().unwrap().state == ModuleState::Stopped).await;
    let held = module.status().unwrap();
    assert_eq!(held.restart_count, before.restart_count);
    assert_eq!(held.lifetime_restarts, before.lifetime_restarts);
    assert!(module.next_retry_at_ms().unwrap().is_none());
    assert_eq!(module.terminal_history().entries.len(), 1);
    let handler = Arc::new(
        crate::ControlHandler::new(registry)
            .with_supervisor(handle)
            .with_config_runtime(owner.clone()),
    );
    let worker = handler.spawn_config_delivery_worker().unwrap();
    entry(&rig, "registration_channel", json!(true));
    approve(&rig, &owner, PlanKind::Change);
    until(|| module.status().unwrap().process_alive).await;
    let revived = module.status().unwrap();
    worker.abort();
    module.stop().await.unwrap();
    assert_eq!(revived.spawn_generation, before.spawn_generation + 1);
    assert_eq!(revived.restart_count, before.restart_count);
    assert_eq!(revived.restart_count, 2);
}

#[test]
fn audit_registration_recorded_program_proposal_remains_pending_on_poll() {
    let rig = rig();
    let owner = rig.owner("program-proposal");
    rig.stamp(&owner);
    std::fs::write(rig.path.join("program"), b"changed program bytes").unwrap();
    owner.observe_file_proposal().unwrap();
    assert!(owner
        .lock()
        .unwrap()
        .store
        .loaded_admission_state()
        .1
        .proposal
        .is_some());
    assert_eq!(owner.polled_status().unwrap().pending_proposals, 1);
}

#[test]
fn audit_registration_poisoned_owner_is_not_a_current_status_projection() {
    let rig = rig();
    let owner = Arc::new(rig.owner("poisoned-status"));
    rig.stamp(&owner);
    let held = owner.clone();
    assert!(std::thread::spawn(move || {
        let _guard = held.inner.lock().unwrap();
        panic!("poison configuration owner");
    })
    .join()
    .is_err());
    assert_eq!(
        owner.polled_status().unwrap_err().code,
        "config_unavailable"
    );
    assert!(matches!(
        owner.approved_daemon_view(),
        ApprovedDaemonView::Unavailable
    ));
    assert!(owner.is_recovering());
}

#[tokio::test]
async fn audit_registration_list_does_not_treat_an_unavailable_publication_as_unstamped() {
    let fixture = legacy().await;
    let handler = Arc::new(
        (*fixture.handler)
            .clone()
            .with_capability_config([("a".into(), true)], Default::default()),
    );
    fixture.rig.stamp(&fixture.owner);
    let owner = fixture.owner.clone();
    assert!(std::thread::spawn(move || {
        let _guard = owner.inner.lock().unwrap();
        panic!("poison configuration owner");
    })
    .join()
    .is_err());
    let list = registration_declaration_poll(handler, "supervisor.list").await;
    assert!(
        list["modules"][0].get("configured_enabled").is_none(),
        "{list}"
    );
    drop(fixture.primary);
    fixture.module.stop().await.unwrap();
}

#[test]
fn audit_registration_cutover_status_uses_the_committed_snapshot() {
    let rig = rig();
    wire_fixture_document(&rig, json!(["600"]));
    let owner = rig.owner("cutover-status");
    rig.stamp(&owner);
    assert!(owner.stamp_channel_cutover_pending());
    entry(&rig, "args", json!(["599"]));
    let status = owner.polled_status().unwrap();
    assert_eq!(status.state, "stamped");
    assert_eq!(status.pending_proposals, 1);
}

#[tokio::test]
async fn audit_registration_unreadable_unstamped_census_is_unknown() {
    let rig = rig();
    wire_fixture_document(&rig, json!(["600"]));
    let owner = Arc::new(rig.owner("bounded-census"));
    let handle = channel_handle();
    let registry = Arc::new(crate::Registry::default());
    let supervisor = Supervisor::new_for_test(registry.clone(), RestartPolicy::default())
        .with_handle(handle.clone());
    let module = supervisor
        .supervise_configured_requested_async(
            spec(&rig),
            false,
            Default::default(),
            None,
            RestartPolicy::default(),
            Duration::from_secs(5),
            None,
        )
        .await
        .unwrap();
    let mut second = spec(&rig);
    second.module_id = "b".into();
    let second = supervisor
        .supervise_configured_requested_async(
            second,
            false,
            Default::default(),
            None,
            RestartPolicy::default(),
            Duration::from_secs(5),
            None,
        )
        .await
        .unwrap();
    let handler = crate::ControlHandler::new(registry)
        .with_supervisor(handle)
        .with_config_runtime(owner.clone());
    let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let observed = calls.clone();
    *owner.census_probe.lock().unwrap() = Some(Arc::new(move || {
        observed.lock().unwrap().push(std::thread::current().id())
    }));
    let async_thread = std::thread::current().id();
    registration_declaration_diagnostic(&handler, "supervisor.provenance").await;
    assert_eq!(
        calls.lock().unwrap().len(),
        1,
        "one census read, not one per module"
    );
    assert_ne!(calls.lock().unwrap()[0], async_thread);
    calls.lock().unwrap().clear();
    let path = rig.path.join("subc.jsonc");
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    let response = registration_declaration_diagnostic(&handler, "supervisor.provenance").await;
    assert!(
        response["modules"][0]
            .get("registration_declaration")
            .is_none(),
        "{response}"
    );
    assert_eq!(calls.lock().unwrap().len(), 1);
    assert_ne!(calls.lock().unwrap()[0], async_thread);
    module.stop().await.unwrap();
    second.stop().await.unwrap();
    std::fs::remove_dir(path).unwrap();
}

#[tokio::test]
async fn audit_registration_list_does_not_refuse_disabled_or_running_modules() {
    let rig = rig();
    wire_fixture_document(&rig, json!(["600"]));
    let owner = Arc::new(rig.owner("actual-refusal"));
    rig.stamp(&owner);
    let handle = channel_handle();
    let registry = Arc::new(crate::Registry::default());
    let supervisor = Supervisor::new_for_test(registry.clone(), RestartPolicy::default())
        .with_handle(handle.clone())
        .with_config_runtime(owner.clone());
    let module = supervisor.spawn_async(spec(&rig)).await.unwrap();
    entry(&rig, "registration_channel", json!(false));
    approve(&rig, &owner, PlanKind::Change);
    let handler = crate::ControlHandler::new(registry)
        .with_supervisor(handle)
        .with_config_runtime(owner.clone());
    let running = registration_declaration_diagnostic(&handler, "supervisor.list").await;
    assert!(module.status().unwrap().process_alive);
    assert!(
        running["modules"][0].get("admission_refused").is_none(),
        "{running}"
    );
    module.set_enabled(false).await.unwrap();
    let disabled = registration_declaration_diagnostic(&handler, "supervisor.list").await;
    assert!(
        disabled["modules"][0].get("admission_refused").is_none(),
        "{disabled}"
    );
}

#[tokio::test]
async fn audit_registration_operator_stopped_module_never_paints_a_spawn_refusal() {
    let fixture = legacy().await;
    fixture.rig.stamp(&fixture.owner);
    fixture.module.stop().await.unwrap();
    entry(&fixture.rig, "registration_channel", json!(false));
    approve(&fixture.rig, &fixture.owner, PlanKind::Change);
    // Operator stop terminates the monitor. A later hold notification cannot
    // turn that finished monitor into an attempted spawn or write a refusal.
    assert!(matches!(
        fixture.module.config_exit_hold().await,
        Err(crate::supervise::SuperviseError::CommandClosed { .. })
    ));
    let stopped = fixture.module.status().unwrap();
    assert!(!stopped.process_alive);
    assert!(stopped.admission_refused.is_none());
    assert_eq!(stopped.state, ModuleState::Stopped);
    let list = registration_declaration_poll(fixture.handler.clone(), "supervisor.list").await;
    assert!(
        list["modules"][0].get("admission_refused").is_none(),
        "{list}"
    );
    drop(fixture.primary);
}

#[tokio::test]
async fn audit_registration_hello_warning_compares_the_running_spawn_entry() {
    let fixture = gated_claim_fixture_with_support(Some(false)).await;
    entry(&fixture.rig, "registration_channel", json!(false));
    approve(&fixture.rig, &fixture.owner, PlanKind::Change);
    let response = registration_declaration_diagnostic(&fixture.handler, "supervisor.health").await;
    assert!(
        response["modules"][0]["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("HELLO registration_channel"),
        "{response}"
    );
    drop(fixture.primary);
    fixture.module.stop().await.unwrap();
}
