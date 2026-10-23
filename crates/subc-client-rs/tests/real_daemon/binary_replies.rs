use super::*;

const BINARY_MODULE_ID: &str = "subc-client-rs-binary-replies";

fn binary_body() -> Vec<u8> {
    [0, 0xff, 0x80, b'}'].repeat(256 * 1024)
}

struct BinaryReplyHandler;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn payload_catalog_bridge_preserves_declarations_and_legacy_wire_bytes() {
    let harness = BinaryHarness::start().await;
    let (_, legacy_serving) = spawn_inline_module_with_handler(
        &harness.daemon.connection_file,
        ModuleManifest::builder("subc-client-rs-legacy-payload", "1.0.0").build(),
        BinaryReplyHandler,
    )
    .await;
    wait_for_catalog_module(
        &harness.daemon.connection_file,
        "subc-client-rs-legacy-payload",
        START_TIMEOUT,
    )
    .await;
    let consumer = harness.consumer().await;
    let catalog = consumer.catalog_list().await.unwrap();
    let declared = catalog
        .modules
        .iter()
        .find(|entry| entry.module_id == BINARY_MODULE_ID)
        .unwrap();
    assert_eq!(declared.operation_payload.len(), 1);
    assert_eq!(declared.operation_payload["chunk"].max_bytes(), 1_048_576);

    let mut client = connect_authed_client(&harness.daemon.connection_file)
        .await
        .unwrap();
    let request = Frame::build(
        FrameType::Request,
        Flags::new(false, Priority::Interactive, false),
        0,
        0,
        10,
        serde_json::to_vec(&subc_control::ClientControlRequest::CatalogList {
            module_id: Some("subc-client-rs-legacy-payload".into()),
        })
        .unwrap(),
    )
    .unwrap();
    write_frame(&mut client, &request).await.unwrap();
    let reply = read_frame_timeout(&mut client).await;
    assert_eq!(reply.header.ty, FrameType::Response);
    // Pin the daemon's actual entry bytes, not a re-serialization of the new type.
    let legacy_entry = br#"{"module_id":"subc-client-rs-legacy-payload","ready":true,"module_version":"1.0.0","roles":[],"control_ops":["route.bind","route.status","health.check"]}"#;
    assert!(
        reply
            .body
            .windows(legacy_entry.len())
            .any(|bytes| bytes == legacy_entry),
        "actual catalog body: {}",
        String::from_utf8_lossy(&reply.body)
    );
    assert!(!String::from_utf8(reply.body)
        .unwrap()
        .contains("operation_payload"));
    consumer.close().await;
    harness.stop().await;
    assert!(legacy_serving.await.unwrap().is_ok());
}

#[async_trait]
impl ModuleHandler for BinaryReplyHandler {
    async fn handle(&self, ctx: RequestCtx, body: Vec<u8>) -> HandlerOutcome {
        if body.starts_with(&[0, 0]) || body == b"payload-probe" {
            return match ctx.request_payload() {
                Ok(Some((head, payload))) => {
                    assert_eq!(
                        head,
                        serde_json::json!({"method": "chunk", "params": {"offset": 17}})
                    );
                    assert_eq!(payload, binary_body());
                    HandlerOutcome::Response(
                        subc_protocol::framed_body::encode(&head, payload).unwrap(),
                    )
                }
                Ok(None) => HandlerOutcome::Response(b"none".to_vec()),
                Err(error) => HandlerOutcome::Error {
                    code: error.to_string(),
                    message: "malformed framed request".into(),
                },
            };
        }
        if body == b"binary" {
            ctx.clone().mark_binary_reply();
            HandlerOutcome::Response(binary_body())
        } else {
            HandlerOutcome::Response(body)
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_payload_honors_flag_and_preserves_one_mebibyte_through_real_daemon() {
    let harness = BinaryHarness::start().await;
    let consumer = harness.consumer().await;
    let target = subc_protocol::RouteTarget::ManagementSurface {
        module_id: BINARY_MODULE_ID.into(),
    };
    let identity = consumer_identity("framed-request");
    let head = serde_json::json!({"method": "chunk", "params": {"offset": 17}});
    let payload = binary_body();
    let framed = subc_protocol::framed_body::encode(&head, &payload).unwrap();
    let reply = consumer
        .call_framed_with_reply(
            target.clone(),
            identity.clone(),
            head.clone(),
            payload.clone(),
            fast_call_options(),
        )
        .await
        .unwrap();
    assert_eq!(reply.body(), framed);
    let (_cancel, reply) = consumer.call_framed_cancellable_with_reply(
        target.clone(),
        identity.clone(),
        head,
        payload,
        fast_call_options(),
    );
    assert_eq!(reply.await.unwrap().body(), framed);
    assert_eq!(
        consumer
            .call(
                target.clone(),
                identity.clone(),
                framed,
                fast_call_options()
            )
            .await
            .unwrap(),
        b"none"
    );
    assert_eq!(
        consumer
            .call(
                target,
                identity,
                b"payload-probe".to_vec(),
                fast_call_options()
            )
            .await
            .unwrap(),
        b"none"
    );
    consumer.close().await;
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_framed_request_returns_named_refusal_through_real_daemon() {
    let harness = BinaryHarness::start().await;
    let mut client = connect_authed_client(&harness.daemon.connection_file)
        .await
        .unwrap();
    let (channel, epoch) = open_route(&mut client, BINARY_MODULE_ID, 1).await;
    let mut request = data_request(channel, epoch, 2, b"\0\0\0\x03{}");
    request.header.flags = Flags::new(true, Priority::Interactive, false);
    write_frame(&mut client, &request).await.unwrap();
    let reply = read_frame_timeout(&mut client).await;
    assert_eq!(reply.header.ty, FrameType::Error);
    let error: ErrorBody = serde_json::from_slice(&reply.body).unwrap();
    assert_eq!(error.code, "HeadLongerThanBody");
    harness.stop().await;
}

struct BinaryHarness {
    daemon: LiveDaemon,
    serving: tokio::task::JoinHandle<Result<(), SubcModuleError>>,
    _temp: ScratchDir,
}

impl BinaryHarness {
    async fn start() -> Self {
        let workspace = workspace_root();
        let daemon_bin = ensure_binary(
            &workspace,
            &[
                "build",
                "--locked",
                "-p",
                "subc-core",
                "--bin",
                "ck-subc",
                "--message-format=json",
            ],
        );
        let temp = unique_temp_dir("subc-client-rs-binary-replies");
        let runtime_dir = temp.join("runtime");
        let config_dir = temp.join("config");
        fs::create_dir_all(&runtime_dir).unwrap();
        write_empty_config(&config_dir);
        let daemon = spawn_daemon(&daemon_bin, &runtime_dir, &config_dir);
        wait_for_connection_file(&daemon.connection_file, START_TIMEOUT).await;
        let mut manifest = inline_module_manifest(BINARY_MODULE_ID, &["bytes"]);
        manifest
            .provides
            .push(subc_protocol::manifest::ProviderRole::ManagementSurface {
                operations: vec![subc_protocol::manifest::ManagementOperation {
                    name: "chunk".into(),
                    kind: subc_protocol::manifest::ManagementOperationKind::Mutate,
                    description: None,
                }],
                config_schema: serde_json::json!({}),
                observability: vec![],
                identity_scope: vec![],
                concurrency: subc_protocol::manifest::Concurrency::Serial,
            });
        manifest.operation_payload.insert(
            "chunk".into(),
            subc_protocol::manifest::OperationPayload::new(1_048_576),
        );
        let (_, serving) =
            spawn_inline_module_with_handler(&daemon.connection_file, manifest, BinaryReplyHandler)
                .await;
        wait_for_catalog_module(&daemon.connection_file, BINARY_MODULE_ID, START_TIMEOUT).await;
        Self {
            daemon,
            serving,
            _temp: temp,
        }
    }

    async fn consumer(&self) -> SubcConsumer {
        SubcConsumer::connect(&self.daemon.connection_file, fast_consumer_options())
            .await
            .unwrap()
    }

    async fn stop(mut self) {
        self.daemon.kill_and_wait();
        assert!(self.serving.await.unwrap().is_ok());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_reply_round_trips_with_exact_flags_through_real_daemon() {
    let harness = BinaryHarness::start().await;
    let mut client = connect_authed_client(&harness.daemon.connection_file)
        .await
        .unwrap();
    let (channel, epoch) = open_route(&mut client, BINARY_MODULE_ID, 1).await;
    for (corr, body, expected_body, binary) in [
        (2, b"binary".as_slice(), binary_body(), true),
        (
            3,
            b"{\"ok\":true}".as_slice(),
            b"{\"ok\":true}".to_vec(),
            false,
        ),
    ] {
        write_frame(&mut client, &data_request(channel, epoch, corr, body))
            .await
            .unwrap();
        let frame = read_frame_timeout(&mut client).await;
        assert_eq!(frame.header.ty, FrameType::Response);
        assert_eq!(
            (frame.header.channel, frame.header.epoch, frame.header.corr),
            (channel, epoch, corr)
        );
        assert_eq!(
            frame.header.flags,
            Flags::new(binary, Priority::Interactive, false)
        );
        assert_eq!(frame.body, expected_body);
    }
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_reply_consumer_returns_non_utf8_bytes_unchanged() {
    let harness = BinaryHarness::start().await;
    let consumer = harness.consumer().await;
    let identity = consumer_identity("binary-bytes");
    let target = tool_target(BINARY_MODULE_ID);
    let route = consumer
        .open_route(target.clone(), identity.clone(), fast_call_options())
        .await
        .unwrap();
    assert_eq!(
        consumer
            .request(&route, b"binary".to_vec(), fast_call_options())
            .await
            .unwrap(),
        binary_body()
    );
    assert_eq!(
        consumer
            .call(
                target.clone(),
                identity.clone(),
                b"binary".to_vec(),
                fast_call_options()
            )
            .await
            .unwrap(),
        binary_body()
    );
    let (_cancel, reply) =
        consumer.call_cancellable(target, identity, b"binary".to_vec(), fast_call_options());
    assert_eq!(reply.await.unwrap(), binary_body());
    consumer.close().await;
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_reply_managed_companions_report_terminal_binary_flag() {
    let harness = BinaryHarness::start().await;
    let consumer = harness.consumer().await;
    let identity = consumer_identity("binary-metadata");
    let target = tool_target(BINARY_MODULE_ID);
    for (body, expected, binary) in [
        (b"binary".to_vec(), binary_body(), true),
        (b"{\"ok\":true}".to_vec(), b"{\"ok\":true}".to_vec(), false),
    ] {
        let (_cancel, reply) = consumer.call_cancellable_with_reply(
            target.clone(),
            identity.clone(),
            body.clone(),
            fast_call_options(),
        );
        let reply = reply.await.unwrap();
        assert_eq!(reply.is_binary(), binary);
        assert_eq!(reply.body(), expected);
        assert_eq!(reply.into_body(), expected);
        let reply = consumer
            .call_with_reply(target.clone(), identity.clone(), body, fast_call_options())
            .await
            .unwrap();
        assert_eq!(reply.is_binary(), binary);
        assert_eq!(reply.into_body(), expected);
    }
    consumer.close().await;
    harness.stop().await;
}
