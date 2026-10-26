use super::*;
use subc_client_rs::CALL_MODES_CAPABILITY;
use subc_protocol::{session::ModuleControlResponse, ModuleHelloBody, PROTOCOL_VERSION};

async fn scopes_with_deadline() -> (ScopeOwnerRun, ScopeStamp) {
    let deadline = unix_ms_now() + 120_000;
    let run = run_scope_owner_with_config(
        json!([
            sync_step(
                1,
                json!([
                    {
                        "ref": "run-a", "scope_epoch": 1, "kind": "ephemeral",
                        "carriers": [{ "principal": { "kind": "direct" } }],
                        "attributes": { "agent_id": "agent-7", "run_id": "run-7" },
                        "expires_at_ms": deadline,
                    },
                    {
                        "ref": "flow-a", "scope_epoch": 1, "kind": "ephemeral",
                        "carriers": [{ "principal": { "kind": "direct" } }],
                        "attributes": { "flow_id": "flow:7" },
                    },
                    {
                        "ref": "plain-a", "scope_epoch": 1, "kind": "head",
                        "carriers": [{ "principal": { "kind": "direct" } }],
                    },
                ])
            ),
            describe_step(&sdk_scope_owner(), "run-a"),
        ]),
        json!({ "scope_authority_owners": [SDK_SCOPE_OWNER] }),
    )
    .await;
    for result in run.results[0]["ok"]["results"].as_array().unwrap() {
        assert_eq!(result["outcome"], "created", "{:?}", run.results);
    }
    let described: ScopeStamp =
        serde_json::from_value(run.results[1]["ok"]["scope"].clone()).unwrap();
    assert_eq!(described.expires_at_ms, Some(deadline));
    (run, described)
}

fn selector(scope_ref: &str) -> ScopeSelector {
    ScopeSelector::new(sdk_scope_owner(), scope_ref.to_string(), Some(1))
}

#[derive(Clone, Default)]
struct ModeRecorder(Arc<Mutex<Vec<RouteBindRequest>>>);

#[async_trait]
impl ModuleHandler for ModeRecorder {
    async fn handle(&self, _ctx: RequestCtx, body: Vec<u8>) -> HandlerOutcome {
        HandlerOutcome::Response(body)
    }

    async fn on_bind(&self, request: &RouteBindRequest) -> BindDecision {
        self.0.lock().unwrap().push(request.clone());
        BindDecision::accept()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn declarer_sdk_receives_scope_modes_and_the_described_run_deadline() {
    let (mut run, described) = scopes_with_deadline().await;
    let recorder = ModeRecorder::default();
    let (provider, serve_task) = spawn_inline_module_with_handler(
        &run.daemon.connection_file,
        inline_capability_module_manifest("mode-provider", Some(CALL_MODES_CAPABILITY)),
        recorder.clone(),
    )
    .await;
    wait_for_catalog_module(&run.daemon.connection_file, "mode-provider", START_TIMEOUT).await;
    let consumer = SubcConsumer::connect(&run.daemon.connection_file, fast_consumer_options())
        .await
        .unwrap();
    for (scope_ref, expected) in [
        (None, CallMode::Classic),
        (Some("plain-a"), CallMode::Classic),
        (Some("run-a"), CallMode::Codemode),
        (Some("flow-a"), CallMode::Flow),
    ] {
        let identity = consumer_identity(scope_ref.unwrap_or("unscoped"));
        if let Some(scope_ref) = scope_ref {
            consumer
                .open_route_scoped(
                    tool_target("mode-provider"),
                    identity,
                    selector(scope_ref),
                    fast_call_options(),
                )
                .await
                .unwrap();
        } else {
            consumer
                .open_route(tool_target("mode-provider"), identity, fast_call_options())
                .await
                .unwrap();
        }
        let bind = recorder.0.lock().unwrap().last().unwrap().clone();
        assert_eq!(bind.call_mode, expected);
        match scope_ref {
            Some("run-a") => assert_eq!(bind.scope, Some(described.clone())),
            Some(_) => assert_eq!(bind.scope.unwrap().expires_at_ms, None),
            None => assert_eq!(bind.scope, None),
        }
    }
    let reply = provider
        .scope_describe(sdk_scope_owner(), "run-a".to_string())
        .await
        .unwrap();
    assert_eq!(reply.scope, Some(described));
    consumer.close().await;
    run.daemon.kill_and_wait();
    assert!(serve_task.await.unwrap().is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn installed_flow_identity_is_gated_in_real_daemon_binds_and_describe() {
    use subc_protocol::scope::FLOW_IDENTITY_SCOPES_CAPABILITY;

    let attributes = json!({
        "flow_id": "flow:7",
        "flow_version": 3,
        "flow_code_hash": "a".repeat(64),
        "flow_install_id": "install:7",
    });
    let mut run = run_scope_owner_with_config(
        json!([
            sync_step(
                1,
                json!([{
                    "ref": "flow-a", "scope_epoch": 1, "kind": "ephemeral",
                    "carriers": [{ "principal": { "kind": "direct" } }],
                    "attributes": attributes,
                }])
            ),
            describe_step(&sdk_scope_owner(), "flow-a"),
        ]),
        json!({ "scope_authority_owners": [SDK_SCOPE_OWNER] }),
    )
    .await;
    assert_eq!(run.results[0]["ok"]["results"][0]["outcome"], "created");
    assert_eq!(
        run.results[1]["ok"]["scope"]["attributes"],
        json!({"flow_id": "flow:7"})
    );

    let consumer = SubcConsumer::connect(&run.daemon.connection_file, fast_consumer_options())
        .await
        .unwrap();
    let mut serve_tasks = Vec::new();
    for (module_id, supported) in [("flow-legacy", false), ("flow-identity", true)] {
        let mut manifest =
            inline_capability_module_manifest(module_id, Some(CALL_MODES_CAPABILITY));
        if supported {
            manifest
                .capabilities
                .as_mut()
                .unwrap()
                .provides
                .push(FLOW_IDENTITY_SCOPES_CAPABILITY.into());
        }
        let recorder = ModeRecorder::default();
        let (provider, serve_task) = spawn_inline_module_with_handler(
            &run.daemon.connection_file,
            manifest,
            recorder.clone(),
        )
        .await;
        wait_for_catalog_module(&run.daemon.connection_file, module_id, START_TIMEOUT).await;
        consumer
            .open_route_scoped(
                tool_target(module_id),
                consumer_identity(module_id),
                selector("flow-a"),
                fast_call_options(),
            )
            .await
            .unwrap();
        let stamp = recorder
            .0
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .scope
            .clone()
            .unwrap();
        assert!(stamp.owner_authorized);
        assert_eq!(
            serde_json::to_value(&stamp.attributes).unwrap(),
            if supported {
                attributes.clone()
            } else {
                json!({"flow_id": "flow:7"})
            }
        );
        let reply = provider
            .scope_describe(sdk_scope_owner(), "flow-a".into())
            .await
            .unwrap();
        assert_eq!(reply.scope, Some(stamp));
        serve_tasks.push(serve_task);
    }
    consumer.close().await;
    run.daemon.kill_and_wait();
    for task in serve_tasks {
        assert!(task.await.unwrap().is_ok());
    }
}

// Capture raw binds so SDK fallback cannot conceal a missing field, and typed
// decoding cannot conceal a new field accidentally sent to a legacy reader.
async fn wire_provider(path: &Path, module_id: &str, capability: Option<&str>) -> TcpStream {
    let mut stream = connect_authed_client(path).await.unwrap();
    let hello = ModuleHelloBody {
        manifest: inline_capability_module_manifest(module_id, capability),
        protocol_ver: PROTOCOL_VERSION,
        control_ops: Some(vec!["health.check".to_string()]),
        launch_nonce: None,
    };
    let frame = Frame::build(
        FrameType::Hello,
        Flags::new(false, Priority::Interactive, false),
        0,
        0,
        1,
        serde_json::to_vec(&hello).unwrap(),
    )
    .unwrap();
    write_frame(&mut stream, &frame).await.unwrap();
    stream.flush().await.unwrap();
    assert_eq!(
        read_frame_timeout(&mut stream).await.header.ty,
        FrameType::HelloAck
    );
    wait_for_catalog_module(path, module_id, START_TIMEOUT).await;
    stream
}

async fn capture_bind(
    consumer: &SubcConsumer,
    provider: &mut TcpStream,
    module_id: &str,
    scope_ref: Option<&str>,
) -> Value {
    let open = async {
        let identity = consumer_identity(scope_ref.unwrap_or("unscoped"));
        if let Some(scope_ref) = scope_ref {
            consumer
                .open_route_scoped(
                    tool_target(module_id),
                    identity,
                    selector(scope_ref),
                    fast_call_options(),
                )
                .await
        } else {
            consumer
                .open_route(tool_target(module_id), identity, fast_call_options())
                .await
        }
    };
    let capture = async {
        loop {
            let frame = read_frame_timeout(provider).await;
            let body = json_body(&frame.body);
            let response = match body["op"].as_str() {
                Some("health.check") => json!({ "op": "health.check", "status": "ok" }),
                Some("route.bind") => {
                    serde_json::to_value(ModuleControlResponse::RouteBindAck {}).unwrap()
                }
                other => panic!("unexpected provider request: {other:?}"),
            };
            let ack = Frame::build(
                FrameType::Response,
                Flags::new(false, Priority::Interactive, false),
                0,
                0,
                frame.header.corr,
                serde_json::to_vec(&response).unwrap(),
            )
            .unwrap();
            write_frame(provider, &ack).await.unwrap();
            provider.flush().await.unwrap();
            if body["op"] == "route.bind" {
                return body;
            }
        }
    };
    let (opened, body) = tokio::join!(open, capture);
    opened.expect("scope migration must admit the provider");
    body
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn declarer_wire_binds_stamp_all_modes_and_the_run_deadline() {
    let (mut run, described) = scopes_with_deadline().await;
    let mut provider = wire_provider(
        &run.daemon.connection_file,
        "wire-declarer",
        Some(CALL_MODES_CAPABILITY),
    )
    .await;
    let consumer = SubcConsumer::connect(&run.daemon.connection_file, fast_consumer_options())
        .await
        .unwrap();
    for (scope_ref, mode) in [
        (None, "classic"),
        (Some("run-a"), "codemode"),
        (Some("flow-a"), "flow"),
    ] {
        let bind = capture_bind(&consumer, &mut provider, "wire-declarer", scope_ref).await;
        assert_eq!(bind["call_mode"], mode);
        if scope_ref == Some("run-a") {
            assert_eq!(
                bind["scope"]["expires_at_ms"].as_u64(),
                described.expires_at_ms
            );
            assert_eq!(bind["scope"], serde_json::to_value(&described).unwrap());
        }
    }
    consumer.close().await;
    run.daemon.kill_and_wait();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_wire_binds_omit_call_mode_and_deadline_even_under_run_and_flow_scopes() {
    let (mut run, described) = scopes_with_deadline().await;
    assert!(described.expires_at_ms.is_some());
    let consumer = SubcConsumer::connect(&run.daemon.connection_file, fast_consumer_options())
        .await
        .unwrap();
    for (module_id, capability, scope_ref, attribute, expected) in [
        ("wire-unscoped", None, None, None, None),
        (
            "wire-legacy-run",
            Some("agent-run-scopes/v1"),
            Some("run-a"),
            Some("run_id"),
            Some("run-7"),
        ),
        (
            "wire-legacy-flow",
            Some("flow-scopes/v1"),
            Some("flow-a"),
            Some("flow_id"),
            Some("flow:7"),
        ),
    ] {
        let mut provider = wire_provider(&run.daemon.connection_file, module_id, capability).await;
        let bind = capture_bind(&consumer, &mut provider, module_id, scope_ref).await;
        assert!(bind.get("call_mode").is_none(), "{bind}");
        assert!(bind["scope"].get("expires_at_ms").is_none(), "{bind}");
        if let Some(attribute) = attribute {
            assert_eq!(bind["scope"]["attributes"][attribute].as_str(), expected);
            assert_eq!(bind["scope"]["ref"].as_str(), scope_ref);
        } else {
            assert!(bind.get("scope").is_none(), "{bind}");
        }
    }
    consumer.close().await;
    run.daemon.kill_and_wait();
}
