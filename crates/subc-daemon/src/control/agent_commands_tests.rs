#[tokio::test]
async fn agent_command_batch_caps_name_the_oversized_field() {
    let handler = ControlHandler::new(Arc::new(Registry::default()));
    let root = subc_protocol::session::AgentCommandRoot {
        pid: 42,
        start_time_ms: 1,
    };
    for (field, cap) in [("started", 256), ("finished", 256), ("known", 1_024)] {
        let request =
            Frame::build(FrameType::Request, control_flags(), 0, 0, 1, b"{}".to_vec()).unwrap();
        let reply = handler
            .handle_agent_commands(
                ConnectionId::new(998),
                request,
                if field == "started" {
                    vec![root; cap + 1]
                } else {
                    Vec::new()
                },
                if field == "finished" {
                    vec![
                        subc_protocol::session::AgentCommandFinished {
                            pid: root.pid,
                            start_time_ms: root.start_time_ms,
                            cpu_ms: None
                        };
                        cap + 1
                    ]
                } else {
                    Vec::new()
                },
                (field == "known").then(|| vec![root; cap + 1]),
            )
            .unwrap();
        let error: ErrorBody = serde_json::from_slice(&reply[0].body).unwrap();
        assert_eq!(error.code, "invalid_request");
        assert!(error.message.contains(field), "{error:?}");
    }
}

#[tokio::test]
async fn supervised_agent_commands_move_cpu_and_refuse_foreign_ownership() {
    exercise_agent_commands(0).await;
}

#[tokio::test]
async fn a_module_without_agent_declarations_has_no_agents_row() {
    exercise_agent_commands(0).await;
}

#[tokio::test]
async fn sampler_revision_conflicts_are_revalidated_before_the_reply() {
    exercise_agent_commands(2).await;
}

#[tokio::test]
async fn persistent_revision_contention_retains_starts_pending_validation() {
    exercise_agent_commands(4).await;
}

#[tokio::test]
async fn slow_agent_command_validation_retains_starts_pending_validation() {
    exercise_agent_commands_with_validation(0, false, false, DelayedDeclaration::Started).await;
}

#[tokio::test]
async fn slow_mixed_agent_commands_retain_starts_and_retry_without_double_credit() {
    exercise_agent_commands_with_validation(0, false, false, DelayedDeclaration::Mixed).await;
}

#[tokio::test]
async fn supervisor_list_with_resource_diagnostics_matches_compatibility_golden() {
    exercise_agent_commands_with_validation(0, true, false, DelayedDeclaration::None).await;
}

#[tokio::test]
async fn verified_root_capacity_refusal_is_retryable_and_records_only_that_root() {
    exercise_agent_commands_with_validation(0, false, true, DelayedDeclaration::None).await;
}

async fn exercise_agent_commands(conflicts: usize) {
    exercise_agent_commands_with_validation(conflicts, false, false, DelayedDeclaration::None)
        .await;
}

#[derive(Clone, Copy)]
enum DelayedDeclaration {
    None,
    Started,
    Mixed,
}

async fn exercise_agent_commands_with_validation(
    conflicts: usize,
    check_list_golden: bool,
    at_capacity: bool,
    delayed: DelayedDeclaration,
) {
    use std::fs;
    use subc_protocol::session::{AgentCommandFinished, AgentCommandRoot};
    async fn measured(
        handler: &ControlHandler,
        handle: &SupervisorHandle,
        deadline: tokio::time::Instant,
        minimum: u64,
    ) -> crate::resource_totals::Readings {
        loop {
            let readings = handler
                .resource_tracker
                .read_with_native_table_for_test(handle.list(), true, "test")
                .await;
            if readings
                .agent_commands
                .as_ref()
                .is_some_and(|agents| agents.first().is_some_and(|agent| agent.cpu_ms >= minimum))
            {
                return readings;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "no complete command measurement reached the independently burned CPU: {:?}",
                readings.agent_commands
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
    for reported in [false, true]
        .into_iter()
        .filter(|reported| !(check_list_golden || at_capacity) || !*reported)
    {
        let dir = ScratchDir::new("supervised-agent-commands");
        let registry = Arc::new(Registry::default());
        let handle = SupervisorHandle::new();
        let supervisor = Supervisor::new_for_test(registry.clone(), RestartPolicy::default())
            .with_handle(handle.clone());
        let program = cortexkit_test_support::cargo_profile_bin("ckdev-resource-helper");
        assert!(
            program.is_file(),
            "resource fixture was not built: {program:?}"
        );
        let module = supervisor
            .supervise_configured(
                crate::ModuleSpec {
                    script: None,
                    module_id: "resource-module".into(),
                    program,
                    args: Vec::new(),
                    env: ["XDG_DATA_HOME", "XDG_RUNTIME_DIR", "XDG_CONFIG_HOME"]
                        .into_iter()
                        .map(|key| (key.to_owned(), dir.display().to_string()))
                        .chain([(
                            "SUBC_RESOURCE_FIXTURE_DIR".to_owned(),
                            dir.display().to_string(),
                        )])
                        .collect(),
                    reserved: false,
                    reserved_prefixes: Vec::new(),
                    protocol: ModuleProtocol::Subc,
                    overlap: Default::default(),
                    carry_containment: false,
                },
                true,
            )
            .unwrap();
        let configured_version = "9.8.7-resource-test";
        let handler = ControlHandler::new(registry)
            .with_daemon_version(configured_version.to_owned())
            .with_supervisor(handle.clone());
        let (ctx, mut wire) = route_ctx(ConnectionId::new(200));
        let nonce = handle.spawn_launch_nonce_for("resource-module").unwrap();
        hello_via_sink(
            &handler,
            &ctx,
            &mut wire,
            hello_frame_with_nonce("resource-module", PROTOCOL_VERSION, 1, Some(&nonce)),
        )
        .await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let root = loop {
            if let Ok(contents) = fs::read_to_string(dir.join("root-ready")) {
                let fields: Vec<_> = contents.split_whitespace().collect();
                if fields.len() == 2 {
                    break AgentCommandRoot {
                        pid: fields[0].parse().unwrap(),
                        start_time_ms: fields[1].parse().unwrap(),
                    };
                }
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "supervised fixture never started its command"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        let request = |corr, request: &ModuleControlRequestFromModule| {
            Frame::build(
                FrameType::Request,
                control_flags(),
                0,
                0,
                corr,
                serde_json::to_vec(request).unwrap(),
            )
            .unwrap()
        };
        let foreign = AgentCommandRoot {
            pid: std::process::id(),
            start_time_ms: subc_os::process_start_time_ms(std::process::id()).unwrap(),
        };
        let before_declaration = loop {
            let reading = handler
                .resource_tracker
                .read_with_native_table_for_test(handle.list(), true, "test")
                .await;
            if reading.agent_commands.is_some()
                && reading
                    .modules
                    .get("resource-module")
                    .is_some_and(|(tree, _)| tree.is_some())
            {
                break reading;
            }
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        let no_agents = before_declaration
            .agent_commands
            .as_ref()
            .is_some_and(Vec::is_empty);
        if !no_agents {
            fs::write(dir.join("burn-root"), "").unwrap();
            fs::write(dir.join("finish-root"), "").unwrap();
            while !dir.join("reaped").exists() {
                assert!(tokio::time::Instant::now() < deadline);
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            handler.cleanup_connection(ctx.connection_id).unwrap();
            stop_resource_fixture(&module, &dir).await;
            assert!(
                no_agents,
                "a module's own process-tree uncertainty is not an agent declaration"
            );
        }
        let tree = before_declaration.modules["resource-module"]
            .0
            .as_ref()
            .unwrap();
        if tree.source == subc_control::TreeSource::ProcessTree {
            assert_eq!(tree.bound, Some(subc_control::ResourceBound::Lower));
        }
        let batch = ModuleControlRequestFromModule::AgentCommands {
            started: vec![foreign, root],
            finished: if matches!(delayed, DelayedDeclaration::Mixed) {
                vec![AgentCommandFinished {
                    pid: root.pid,
                    start_time_ms: root.start_time_ms,
                    cpu_ms: Some(u64::MAX),
                }]
            } else {
                Vec::new()
            },
            known: matches!(delayed, DelayedDeclaration::Mixed).then(Vec::new),
        };
        // Inject the walk, not the queue: revision retries must exercise real
        // validation without depending on the host's process-table walk cost.
        let validation_reads = handler.resource_tracker.inject_validation_table_for_test(
            subc_os::ProcessTable::read().unwrap(),
            if !matches!(delayed, DelayedDeclaration::None) {
                // Keep the native reader blocked beyond the unchanged 200 ms
                // validation window so expiry, rather than contention, replies.
                Duration::from_secs(1)
            } else {
                Duration::ZERO
            },
        );
        handler
            .resource_tracker
            .inject_revision_conflicts(conflicts);
        if at_capacity {
            handler
                .resource_tracker
                .fill_root_capacity_for_test("resource-module");
        }
        let replies = handler
            .handle_control_frame(&ctx, request(2, &batch))
            .await
            .unwrap();
        assert!(
            replies.is_empty(),
            "agent validation must be dispatched away from the connection reader"
        );
        let replies = [tokio::time::timeout(Duration::from_secs(2), wire.recv())
            .await
            .unwrap()
            .unwrap()];
        if matches!(delayed, DelayedDeclaration::Mixed) {
            let error: ErrorBody = serde_json::from_slice(&replies[0].body).unwrap();
            assert_eq!(error.code, "busy");
            assert!(error.message.contains("validation timed out"));
            assert!(validation_reads.load(std::sync::atomic::Ordering::SeqCst) > 0);
            assert_eq!(
                handler
                    .resource_tracker
                    .root_counts_for_test("resource-module"),
                (0, 2, 0)
            );
            // The same mixed batch must complete on retry: starts dedupe, the
            // finish stops attribution, and known=[] removes pending roots.
            handler.resource_tracker.inject_validation_table_for_test(
                subc_os::ProcessTable::read().unwrap(),
                Duration::ZERO,
            );
            for corr in [22, 23] {
                handler
                    .handle_control_frame(&ctx, request(corr, &batch))
                    .await
                    .unwrap();
                let reply = tokio::time::timeout(Duration::from_secs(2), wire.recv())
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    reply.header.ty,
                    FrameType::Response,
                    "{}",
                    String::from_utf8_lossy(&reply.body)
                );
                assert_eq!(
                    handler
                        .resource_tracker
                        .root_counts_for_test("resource-module")
                        .1,
                    0
                );
                let readings = handler
                    .resource_tracker
                    .read_with_native_table_for_test(handle.list(), true, "test")
                    .await;
                let agents = &readings.agent_commands.as_ref().unwrap()[0];
                assert_eq!(agents.process_count, 0);
                assert_eq!(
                    agents.cpu_ms, 0,
                    "a mixed retry cannot replay the pre-acceptance burn"
                );
            }
            fs::write(dir.join("burn-root"), "").unwrap();
            fs::write(dir.join("finish-root"), "").unwrap();
            while !dir.join("reaped").exists() {
                assert!(tokio::time::Instant::now() < deadline);
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            handler.cleanup_connection(ctx.connection_id).unwrap();
            stop_resource_fixture(&module, &dir).await;
            return;
        }
        if at_capacity {
            let error: ErrorBody = serde_json::from_slice(&replies[0].body).unwrap();
            fs::write(dir.join("burn-root"), "").unwrap();
            fs::write(dir.join("finish-root"), "").unwrap();
            while !dir.join("reaped").exists() {
                assert!(tokio::time::Instant::now() < deadline);
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            handler.cleanup_connection(ctx.connection_id).unwrap();
            stop_resource_fixture(&module, &dir).await;
            assert_eq!(error.code, "agent_roots_full");
            assert_eq!(
                handler.resource_tracker.loss_diagnostics()["resource-module"]["lost_identities"],
                1
            );
            return;
        }
        if conflicts >= 4 || matches!(delayed, DelayedDeclaration::Started) {
            assert_eq!(
                replies[0].header.ty,
                FrameType::Response,
                "{}",
                String::from_utf8_lossy(&replies[0].body)
            );
            assert_eq!(
                handler
                    .resource_tracker
                    .root_counts_for_test("resource-module"),
                (0, 2, 0)
            );
        }
        if conflicts >= 4 {
            assert_eq!(
                validation_reads.load(std::sync::atomic::Ordering::SeqCst),
                4
            );
        }
        if conflicts < 4 && matches!(delayed, DelayedDeclaration::None) {
            assert_eq!(
                validation_reads.load(std::sync::atomic::Ordering::SeqCst),
                conflicts + 1
            );
        }
        handler.resource_tracker.clear_validation_table_for_test();
        assert_eq!(
            replies[0].header.ty,
            FrameType::Response,
            "{}",
            String::from_utf8_lossy(&replies[0].body)
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&replies[0].body).unwrap(),
            serde_json::json!({})
        );
        let readings = measured(&handler, &handle, deadline, 0).await;
        let agents = &readings.agent_commands.as_ref().unwrap()[0];
        let historical = fs::read_to_string(dir.join("historical-cpu"))
            .unwrap()
            .parse::<u64>()
            .unwrap();
        let current = subc_os::resource_usage(root.pid).unwrap();
        let current = u64::try_from((current.cpu_user + current.cpu_system).as_millis()).unwrap();
        assert!(
            agents.cpu_ms <= current.saturating_sub(historical),
            "pre-acceptance CPU stays in the module row"
        );
        fs::write(dir.join("burn-root"), "").unwrap();
        while !dir.join("root-burned").exists() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the accepted command did not burn new CPU"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        // Reads within one second intentionally reuse the same process walk.
        // Require the known post-acceptance burn to become visible by the
        // existing setup deadline instead of treating a cached zero as fresh.
        let readings = measured(&handler, &handle, deadline, 100).await;
        let agents = &readings.agent_commands.as_ref().unwrap()[0];
        assert_eq!(
            readings.daemon.as_ref().unwrap().version,
            configured_version
        );
        assert!(
            agents.cpu_ms >= 100,
            "declared root must have burned CPU: {agents:?}"
        );
        let list_body = if check_list_golden {
            #[cfg(windows)]
            {
                let (tree, hour) = &readings.modules["resource-module"];
                assert_eq!(tree.as_ref().unwrap().source, subc_control::TreeSource::Job);
                assert_eq!(tree.as_ref().unwrap().bound, None);
                assert_eq!(
                    hour.as_ref().unwrap().bound,
                    None,
                    "an exact job hour does not inherit process-tree uncertainty"
                );
            }
            // Exercise the lower-bound member even when the native-container
            // reading would ordinarily be exact.
            handler
                .resource_tracker
                .inject_model_loss_for_test("resource-module");
            let body = loop {
                handler
                    .resource_tracker
                    .read_with_native_table_for_test(
                        handle.list(),
                        true,
                        "supervisor_list_golden",
                    )
                    .await;
                let frame =
                    Frame::build(FrameType::Request, control_flags(), 0, 0, 900, Vec::new())
                        .unwrap();
                let replies = {
                    let _readers = crate::subtree_resources::fault_read_guard().await;
                    handler.handle_supervisor_list(frame).await.unwrap()
                };
                let body: Value = serde_json::from_slice(&replies[0].body).unwrap();
                if body["modules"][0].get("tree").is_some()
                    && body["modules"][0]["cpu_last_hour"].get("bound").is_some()
                    && body["modules"][0]["resources"]["status"] == "measured"
                    && body["daemon"]["resources"]["status"] == "measured"
                    && body["daemon"].get("cpu_last_hour").is_some()
                    && body["agent_commands"][0].get("cpu_last_hour").is_some()
                {
                    break body;
                }
                if tokio::time::Instant::now() >= deadline {
                    fs::write(dir.join("finish-root"), "").unwrap();
                    handler.cleanup_connection(ctx.connection_id).unwrap();
                    stop_resource_fixture(&module, &dir).await;
                    panic!("populated list diagnostics never became available: {body}");
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            };
            Some((body, handler.registry.generation().unwrap()))
        } else {
            None
        };
        assert_eq!(
            agents.process_count, 1,
            "the foreign root must be ignored without refusing its valid batch peer"
        );
        let (consumer, _wire) = route_ctx(ConnectionId::new(201));
        let refused = handler
            .handle_control_frame(&consumer, request(3, &batch))
            .await
            .unwrap();
        assert_eq!(refused[0].header.ty, FrameType::Error);
        let body: ErrorBody = serde_json::from_slice(&refused[0].body).unwrap();
        assert_eq!(body.code, "not_supervised");
        let sampled_cpu = agents.cpu_ms;
        fs::write(dir.join("finish-root"), "").unwrap();
        while !dir.join("reaped").exists() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "supervised module did not reap command"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let reported_cpu = fs::read_to_string(dir.join("root-cpu"))
            .unwrap()
            .parse::<u64>()
            .unwrap();
        let batch = ModuleControlRequestFromModule::AgentCommands {
            started: Vec::new(),
            finished: vec![AgentCommandFinished {
                pid: root.pid,
                start_time_ms: root.start_time_ms,
                cpu_ms: reported.then_some(u64::MAX),
            }],
            known: Some(Vec::new()),
        };
        handler.resource_tracker.inject_validation_table_for_test(
            subc_os::ProcessTable::read().unwrap(),
            Duration::ZERO,
        );
        let replies = handler
            .handle_control_frame(&ctx, request(4, &batch))
            .await
            .unwrap();
        assert!(
            replies.is_empty(),
            "agent validation must be dispatched away from the connection reader"
        );
        let replies = [tokio::time::timeout(Duration::from_secs(2), wire.recv())
            .await
            .unwrap()
            .unwrap()];
        assert_eq!(replies[0].header.ty, FrameType::Response);
        let readings = measured(&handler, &handle, deadline, sampled_cpu).await;
        let credited = readings.agent_commands.as_ref().unwrap()[0].cpu_ms;
        assert!(
            credited >= sampled_cpu && credited <= reported_cpu,
            "settlement uses only CPU measured by the daemon, never the supplied u64::MAX"
        );
        assert_eq!(
            readings.agent_commands.as_ref().unwrap()[0].process_count,
            0
        );
        handler
            .handle_control_frame(&ctx, request(5, &batch))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), wire.recv())
            .await
            .unwrap()
            .unwrap();
        let readings = measured(&handler, &handle, deadline, sampled_cpu).await;
        assert_eq!(
            readings.agent_commands.as_ref().unwrap()[0].cpu_ms,
            credited,
            "duplicate finish must be idempotent"
        );
        handler.resource_tracker.clear_validation_table_for_test();
        handler.cleanup_connection(ctx.connection_id).unwrap();
        stop_resource_fixture(&module, &dir).await;
        // Compare only after the command was reaped and its module stopped, so
        // a wire regression cannot strand the live process fixture.
        if let Some((body, generation)) = list_body {
            assert_supervisor_list_golden(body, generation);
        }
    }
}

fn assert_supervisor_list_golden(mut body: Value, generation: u64) {
    assert_eq!(body["generation"], generation);
    assert_eq!(body["modules"][0]["cpu_last_hour"]["bound"], "lower");

    // Keep member presence and types, but replace live PIDs, clocks, CPU and
    // memory counters with fixed values. The memory kind depends on the OS.
    fn canonicalize_measurements(value: &mut Value) {
        match value {
            Value::Object(object) => {
                for (key, value) in object {
                    match key.as_str() {
                        "pid" | "start_time_ms" | "measured_at_ms" | "covered_ms" | "cpu_ms"
                        | "cpu_user_ms" | "cpu_system_ms" | "memory_bytes" if value.is_u64() => {
                            *value = json!(0);
                        }
                        "memory_kind"
                            if matches!(
                                value.as_str(),
                                Some("phys_footprint" | "windows_working_set")
                            ) =>
                        {
                            *value = json!("resident_set");
                        }
                        _ => canonicalize_measurements(value),
                    }
                }
            }
            Value::Array(items) => items.iter_mut().for_each(canonicalize_measurements),
            _ => {}
        }
    }
    canonicalize_measurements(&mut body);

    // Swap accounting is Linux-only. Reload evidence depends on the executable
    // and the time available to inspect it; neither is part of this fixture.
    let daemon = body["daemon"]["resources"].as_object_mut().unwrap();
    daemon.remove("swap_bytes");
    let module = body["modules"][0].as_object_mut().unwrap();
    module.remove("pending_reload");
    module["resources"].as_object_mut().unwrap().remove("swap_bytes");
    if module.get("launch_nonce_source") == Some(&json!("env")) {
        module.remove("launch_nonce_source");
    }
    if module.get("launch_nonce_env") == Some(&json!(true)) {
        module.insert("launch_nonce_env".to_owned(), json!(false));
    }

    // The native container may count platform helpers such as conhost.exe.
    // Normalize their groups and count, and the platform's sampling backend.
    // The Windows job source and its exact bounds were checked before loss.
    let tree = module.get_mut("tree").unwrap();
    tree["groups"].as_array_mut().unwrap().clear();
    if tree["process_count"].as_u64().is_some_and(|n| n > 0) {
        tree["process_count"] = json!(1);
    }
    if matches!(tree["source"].as_str(), Some("job" | "cgroup")) {
        tree["source"] = json!("process_tree");
    }
    // Process names may be truncated on Linux or have an .exe suffix on Windows.
    let name = &mut body["agent_commands"][0]["groups"][0]["name"];
    if matches!(
        name.as_str(),
        Some("ckdev-resource-" | "ckdev-resource-helper.exe")
    ) {
        *name = json!("ckdev-resource-helper");
    }

    // Other workspace crates may enable serde_json's preserve_order feature.
    body.sort_all_objects();
    let canonical = serde_json::to_string_pretty(&body).unwrap() + "\n";
    const GOLDEN: &str = include_str!("../../tests/golden/supervisor_list_resource_diagnostics.json");
    assert_eq!(
        canonical,
        GOLDEN,
        "supervisor.list encoding changed; regenerate tests/golden/supervisor_list_resource_diagnostics.json if intentional"
    );
    let current: subc_control::ClientControlResponse = serde_json::from_str(GOLDEN).unwrap();
    assert_eq!(serde_json::to_value(current).unwrap(), body);
}
