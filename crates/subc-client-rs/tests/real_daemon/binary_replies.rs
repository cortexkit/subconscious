use super::*;

const BINARY_MODULE_ID: &str = "subc-client-rs-binary-replies";

fn binary_body() -> Vec<u8> {
    [0, 0xff, 0x80, b'}'].repeat(256 * 1024)
}

struct BinaryReplyHandler;

#[async_trait]
impl ModuleHandler for BinaryReplyHandler {
    async fn handle(&self, ctx: RequestCtx, body: Vec<u8>) -> HandlerOutcome {
        if body == b"binary" {
            ctx.clone().mark_binary_reply();
            HandlerOutcome::Response(binary_body())
        } else {
            HandlerOutcome::Response(body)
        }
    }
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
        let (_, serving) = spawn_inline_module_with_handler(
            &daemon.connection_file,
            inline_module_manifest(BINARY_MODULE_ID, &["bytes"]),
            BinaryReplyHandler,
        )
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
