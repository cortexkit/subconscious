use std::{
    fs,
    path::{Path, PathBuf},
    process,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use serde_json::Value;
use subc_core::{
    read_frame, serve_listener, write_frame, AttachAck, AttachRequest, ConfigTier, ControlHandler,
    ForwardingTable, Frame, ModuleSpec, ModuleState, ModuleStatus, Registry, RestartPolicy, Router,
    SupervisedModule, Supervisor, SUBC_SOCKET_ENV,
};
use subc_protocol::{ErrorBody, Flags, FrameType, Priority};
use tokio::{
    io::AsyncWriteExt,
    net::{UnixListener, UnixStream},
    task::JoinHandle,
    time::{sleep, timeout, Instant},
};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
const READ_TIMEOUT: Duration = Duration::from_secs(2);

struct TestServer {
    registry: Arc<Registry>,
    forwarding: Arc<ForwardingTable>,
    socket_path: PathBuf,
    temp_dir: PathBuf,
    task: JoinHandle<Result<(), subc_core::ServerError>>,
}

impl TestServer {
    fn start() -> Self {
        let temp_dir = unique_temp_dir("forwarding-server");
        fs::create_dir_all(&temp_dir).unwrap();
        let socket_path = temp_dir.join("s.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();

        let registry = Arc::new(Registry::default());
        let control = Arc::new(ControlHandler::new(Arc::clone(&registry)));
        let forwarding = control.forwarding();
        let router = Arc::new(Router::with_control_handler(control));
        let task = tokio::spawn(serve_listener(listener, router));

        Self {
            registry,
            forwarding,
            socket_path,
            temp_dir,
            task,
        }
    }

    fn stub_events_path(&self, label: &str) -> PathBuf {
        self.temp_dir.join(format!("{label}-events.jsonl"))
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.task.abort();
        let _ = fs::remove_dir_all(&self.temp_dir);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn attach_then_forward_request_round_trips_through_stub() {
    let server = TestServer::start();
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-forwarding";
    let events_path = server.stub_events_path("attach-forwarding");
    let module = supervisor
        .spawn(stub_spec_with_env(
            &server,
            module_id,
            [(
                "FAKE_AFT_EVENTS_PATH",
                events_path.to_string_lossy().into_owned(),
            )],
        ))
        .unwrap();
    wait_for_registration(&server.registry, module_id, Duration::from_secs(1)).await;

    let project = TestProject::new();
    let (mut client, ack) = attach_client(&server, &project, 101, "ses-forwarding").await;
    let attach_event = wait_for_stub_event(&events_path, Duration::from_secs(1), |event| {
        event["kind"] == "attach"
    })
    .await;
    let forwarded_config: Vec<ConfigTier> =
        serde_json::from_value(attach_event["config"].clone()).unwrap();
    assert_eq!(forwarded_config, attach_config(&project, "ses-forwarding"));
    assert_eq!(
        forwarded_config
            .iter()
            .map(|tier| tier.tier.as_str())
            .collect::<Vec<_>>(),
        vec!["user", "project"]
    );
    assert!(ack.route_channel > 0);
    assert_eq!(server.forwarding.active_binding_count().unwrap(), 1);
    assert!(server
        .forwarding
        .has_route_channel(ack.route_channel)
        .unwrap());

    let payload = br#"{"jsonrpc":"2.0","id":7,"method":"read","params":{"path":"Cargo.toml"}}"#;
    write_frame(&mut client, &data_request(ack.route_channel, 202, payload))
        .await
        .unwrap();
    client.flush().await.unwrap();

    let response = read_frame_timeout(&mut client).await;
    assert_eq!(response.header.ty, FrameType::Response);
    assert_eq!(response.header.channel, ack.route_channel);
    assert_eq!(response.header.corr, 202);
    assert_eq!(response.body, payload);

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn single_client_receives_unsolicited_push_and_response_on_bound_route() {
    let server = TestServer::start();
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-push-single";
    let module = supervisor
        .spawn(stub_spec_with_env(
            &server,
            module_id,
            [("FAKE_AFT_PUSH_ON_REQUEST", "1")],
        ))
        .unwrap();
    wait_for_registration(&server.registry, module_id, Duration::from_secs(1)).await;

    let project = TestProject::new();
    let (mut client, ack) = attach_client(&server, &project, 151, "ses-push-single").await;
    assert!(ack.route_channel > 0);

    let payload = br#"{"jsonrpc":"2.0","id":"push-single","method":"read"}"#;
    write_frame(&mut client, &data_request(ack.route_channel, 152, payload))
        .await
        .unwrap();
    client.flush().await.unwrap();

    let (push, _response) =
        read_until_push_and_response(&mut client, ack.route_channel, 152, payload).await;
    assert_eq!(push.header.channel, ack.route_channel);
    assert_eq!(push.body, b"push-event");

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_drop_sends_detach_relay_and_removes_binding() {
    let server = TestServer::start();
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-detach";
    let events_path = server.stub_events_path("detach");
    let module = supervisor
        .spawn(stub_spec_with_env(
            &server,
            module_id,
            [(
                "FAKE_AFT_EVENTS_PATH",
                events_path.to_string_lossy().into_owned(),
            )],
        ))
        .unwrap();
    wait_for_registration(&server.registry, module_id, Duration::from_secs(1)).await;

    let project = TestProject::new();
    let (client, ack) = attach_client(&server, &project, 201, "ses-detach").await;
    assert_eq!(server.forwarding.active_binding_count().unwrap(), 1);
    assert!(server
        .forwarding
        .has_route_channel(ack.route_channel)
        .unwrap());

    drop(client);

    wait_for_binding_count(&server.forwarding, 0, Duration::from_secs(1)).await;
    let detach = wait_for_stub_event(&events_path, Duration::from_secs(1), |event| {
        event["kind"] == "detach"
            && event["route_channel"].as_u64() == Some(u64::from(ack.route_channel))
    })
    .await;
    assert_eq!(
        detach["route_channel"].as_u64(),
        Some(u64::from(ack.route_channel))
    );
    assert!(!server
        .forwarding
        .has_route_channel(ack.route_channel)
        .unwrap());

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn module_frame_after_client_detach_is_dropped_and_connection_survives() {
    let server = TestServer::start();
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-stale-route";
    let events_path = server.stub_events_path("stale-route");
    let module = supervisor
        .spawn(stub_spec_with_env(
            &server,
            module_id,
            [
                (
                    "FAKE_AFT_EVENTS_PATH",
                    events_path.to_string_lossy().into_owned(),
                ),
                ("FAKE_AFT_EMIT_AFTER_DETACH", "1".to_string()),
            ],
        ))
        .unwrap();
    wait_for_registration(&server.registry, module_id, Duration::from_secs(1)).await;

    let project = TestProject::new();
    let (client, ack) = attach_client(&server, &project, 301, "ses-stale-route").await;
    drop(client);

    wait_for_binding_count(&server.forwarding, 0, Duration::from_secs(1)).await;
    wait_for_stub_event(&events_path, Duration::from_secs(1), |event| {
        event["kind"] == "stale_emit"
            && event["route_channel"].as_u64() == Some(u64::from(ack.route_channel))
    })
    .await;
    sleep(Duration::from_millis(100)).await;
    let errors: Vec<_> = stub_events(&events_path)
        .into_iter()
        .filter(|event| event.get("kind").and_then(Value::as_str) == Some("error"))
        .collect();
    assert!(
        errors.is_empty(),
        "unexpected module ERROR frames: {errors:?}"
    );

    let (mut next_client, next_ack) =
        attach_client(&server, &project, 302, "ses-stale-route-2").await;
    assert!(next_ack.route_channel > 0);
    let payload = br#"{"jsonrpc":"2.0","id":8,"method":"read"}"#;
    write_frame(
        &mut next_client,
        &data_request(next_ack.route_channel, 303, payload),
    )
    .await
    .unwrap();
    next_client.flush().await.unwrap();
    let response = read_frame_timeout(&mut next_client).await;
    assert_eq!(response.header.ty, FrameType::Response);
    assert_eq!(response.header.channel, next_ack.route_channel);
    assert_eq!(response.header.corr, 303);
    assert_eq!(response.body, payload);

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejecting_attach_returns_config_divergence_without_committing_binding_then_accepts_later()
{
    let server = TestServer::start();
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-reject";
    let rejecting = supervisor
        .spawn(stub_spec_with_env(
            &server,
            module_id,
            [("FAKE_AFT_REJECT_ATTACH", "1")],
        ))
        .unwrap();
    wait_for_registration(&server.registry, module_id, Duration::from_secs(1)).await;

    let project = TestProject::new();
    let mut client = UnixStream::connect(&server.socket_path).await.unwrap();
    let error = attach_error_on_stream(&mut client, &project, 401, "ses-reject").await;
    assert_eq!(error.code, "config_divergence");
    assert!(error.message.contains("FAKE_AFT_REJECT_ATTACH"));
    assert_eq!(server.forwarding.active_binding_count().unwrap(), 0);
    drop(client);

    rejecting.stop().await.unwrap();
    wait_for_registration_absent(&server.registry, module_id, Duration::from_secs(1)).await;

    let accepting = supervisor.spawn(stub_spec(&server, module_id)).unwrap();
    wait_for_registration(&server.registry, module_id, Duration::from_secs(1)).await;
    let (mut accepted_client, ack) = attach_client(&server, &project, 402, "ses-accept").await;
    assert!(ack.route_channel > 0);
    assert_eq!(server.forwarding.active_binding_count().unwrap(), 1);

    let payload = br#"{"jsonrpc":"2.0","id":9,"method":"read"}"#;
    write_frame(
        &mut accepted_client,
        &data_request(ack.route_channel, 403, payload),
    )
    .await
    .unwrap();
    accepted_client.flush().await.unwrap();
    let response = read_frame_timeout(&mut accepted_client).await;
    assert_eq!(response.header.ty, FrameType::Response);
    assert_eq!(response.header.channel, ack.route_channel);
    assert_eq!(response.header.corr, 403);
    assert_eq!(response.body, payload);

    accepting.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_clients_attach_same_module_and_round_trip_independently() {
    let server = TestServer::start();
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-two-clients";
    let module = supervisor.spawn(stub_spec(&server, module_id)).unwrap();
    wait_for_registration(&server.registry, module_id, Duration::from_secs(1)).await;

    let project = TestProject::new();
    let (mut first, first_ack) = attach_client(&server, &project, 501, "ses-one").await;
    let (mut second, second_ack) = attach_client(&server, &project, 502, "ses-two").await;
    assert_ne!(first_ack.route_channel, second_ack.route_channel);
    assert_eq!(server.forwarding.active_binding_count().unwrap(), 2);

    let first_payload = br#"{"jsonrpc":"2.0","id":"first"}"#;
    let second_payload = br#"{"jsonrpc":"2.0","id":"second"}"#;
    write_frame(
        &mut first,
        &data_request(first_ack.route_channel, 503, first_payload),
    )
    .await
    .unwrap();
    write_frame(
        &mut second,
        &data_request(second_ack.route_channel, 504, second_payload),
    )
    .await
    .unwrap();
    first.flush().await.unwrap();
    second.flush().await.unwrap();

    let first_response = read_frame_timeout(&mut first).await;
    let second_response = read_frame_timeout(&mut second).await;
    assert_eq!(first_response.header.ty, FrameType::Response);
    assert_eq!(first_response.header.channel, first_ack.route_channel);
    assert_eq!(first_response.header.corr, 503);
    assert_eq!(first_response.body, first_payload);
    assert_eq!(second_response.header.ty, FrameType::Response);
    assert_eq!(second_response.header.channel, second_ack.route_channel);
    assert_eq!(second_response.header.corr, 504);
    assert_eq!(second_response.body, second_payload);

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cross_session_slow_call_does_not_block_fast_call() {
    let server = TestServer::start();
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-cross-session-delay";
    let module = supervisor
        .spawn(stub_spec_with_env(
            &server,
            module_id,
            [("FAKE_AFT_DELAY_FROM_BODY", "1")],
        ))
        .unwrap();
    wait_for_registration(&server.registry, module_id, Duration::from_secs(1)).await;

    let project = TestProject::new();
    let (mut slow_client, slow_ack) = attach_client(&server, &project, 525, "ses-cross-slow").await;
    let (mut fast_client, fast_ack) = attach_client(&server, &project, 526, "ses-cross-fast").await;
    assert_ne!(slow_ack.route_channel, fast_ack.route_channel);

    let slow_payload = br#"{"delay_ms":500,"jsonrpc":"2.0","id":"slow"}"#;
    let fast_payload = br#"{"delay_ms":0,"jsonrpc":"2.0","id":"fast"}"#;
    write_frame(
        &mut slow_client,
        &data_request(slow_ack.route_channel, 527, slow_payload),
    )
    .await
    .unwrap();
    slow_client.flush().await.unwrap();
    let slow_sent = Instant::now();

    write_frame(
        &mut fast_client,
        &data_request(fast_ack.route_channel, 528, fast_payload),
    )
    .await
    .unwrap();
    fast_client.flush().await.unwrap();
    let fast_sent = Instant::now();

    let ((slow_received_at, slow_response), (fast_received_at, fast_response)) =
        timeout(Duration::from_secs(2), async {
            tokio::join!(
                async {
                    let response = read_frame_timeout(&mut slow_client).await;
                    (Instant::now(), response)
                },
                async {
                    let response = read_frame_timeout(&mut fast_client).await;
                    (Instant::now(), response)
                }
            )
        })
        .await
        .expect("timed out waiting for cross-session responses");

    assert_response(&slow_response, slow_ack.route_channel, 527, slow_payload);
    assert_response(&fast_response, fast_ack.route_channel, 528, fast_payload);

    let slow_latency = slow_received_at.duration_since(slow_sent);
    let fast_latency = fast_received_at.duration_since(fast_sent);
    eprintln!("cross-session latencies: fast={fast_latency:?}, slow={slow_latency:?}");
    assert!(
        fast_received_at < slow_received_at,
        "fast response should arrive before slow: fast_latency={fast_latency:?}, slow_latency={slow_latency:?}"
    );
    assert!(
        fast_latency < Duration::from_millis(50),
        "fast call queued behind slow call: fast_latency={fast_latency:?}, slow_latency={slow_latency:?}"
    );
    assert!(
        slow_latency >= Duration::from_millis(450),
        "slow call did not exercise the requested 500ms delay: slow_latency={slow_latency:?}, fast_latency={fast_latency:?}"
    );

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_before_response_for_cancellable_request_returns_cancelled_error() {
    let server = TestServer::start();
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-cancel-before";
    let events_path = server.stub_events_path("cancel-before");
    let module = supervisor
        .spawn(stub_spec_with_env(
            &server,
            module_id,
            [
                ("FAKE_AFT_DELAY_FROM_BODY".to_string(), "1".to_string()),
                (
                    "FAKE_AFT_EVENTS_PATH".to_string(),
                    events_path.to_string_lossy().into_owned(),
                ),
            ],
        ))
        .unwrap();
    wait_for_registration(&server.registry, module_id, Duration::from_secs(1)).await;

    let project = TestProject::new();
    let (mut client, ack) = attach_client(&server, &project, 801, "ses-cancel-before").await;
    let corr = 802;
    let payload = br#"{"delay_ms":500,"jsonrpc":"2.0","id":"cancel-before"}"#;
    write_frame(&mut client, &data_request(ack.route_channel, corr, payload))
        .await
        .unwrap();
    client.flush().await.unwrap();

    let cancel_sent = Instant::now();
    write_frame(&mut client, &cancel_frame(ack.route_channel, corr))
        .await
        .unwrap();
    client.flush().await.unwrap();

    let terminal = read_frame_timeout(&mut client).await;
    let cancel_latency = Instant::now().duration_since(cancel_sent);
    eprintln!("cancel-before-response latency: {cancel_latency:?}");
    let body = assert_error(&terminal, ack.route_channel, corr, "cancelled");
    assert!(body.message.contains("cancelled"));
    assert!(
        cancel_latency < Duration::from_millis(250),
        "cancelled terminal should arrive well before 500ms delay; latency={cancel_latency:?}"
    );

    let cancel_event = wait_for_stub_event(&events_path, Duration::from_secs(1), |event| {
        event["kind"] == "cancel"
            && event["channel"].as_u64() == Some(u64::from(ack.route_channel))
            && event["corr"].as_u64() == Some(corr)
            && event["claimed"].as_bool() == Some(true)
    })
    .await;
    assert_eq!(cancel_event["claimed"].as_bool(), Some(true));
    wait_for_stub_event(&events_path, Duration::from_secs(1), |event| {
        event["kind"] == "terminal"
            && event["terminal"] == "error"
            && event["code"] == "cancelled"
            && event["channel"].as_u64() == Some(u64::from(ack.route_channel))
            && event["corr"].as_u64() == Some(corr)
    })
    .await;
    assert_no_frame_within(&mut client, Duration::from_millis(550)).await;

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_after_response_is_idempotent_noop() {
    let server = TestServer::start();
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-cancel-after";
    let events_path = server.stub_events_path("cancel-after");
    let module = supervisor
        .spawn(stub_spec_with_env(
            &server,
            module_id,
            [
                ("FAKE_AFT_DELAY_FROM_BODY".to_string(), "1".to_string()),
                (
                    "FAKE_AFT_EVENTS_PATH".to_string(),
                    events_path.to_string_lossy().into_owned(),
                ),
            ],
        ))
        .unwrap();
    wait_for_registration(&server.registry, module_id, Duration::from_secs(1)).await;

    let project = TestProject::new();
    let (mut client, ack) = attach_client(&server, &project, 811, "ses-cancel-after").await;
    let corr = 812;
    let payload = br#"{"delay_ms":0,"jsonrpc":"2.0","id":"cancel-after"}"#;
    write_frame(&mut client, &data_request(ack.route_channel, corr, payload))
        .await
        .unwrap();
    client.flush().await.unwrap();
    let response = read_frame_timeout(&mut client).await;
    assert_response(&response, ack.route_channel, corr, payload);

    write_frame(&mut client, &cancel_frame(ack.route_channel, corr))
        .await
        .unwrap();
    client.flush().await.unwrap();
    let cancel_event = wait_for_stub_event(&events_path, Duration::from_secs(1), |event| {
        event["kind"] == "cancel"
            && event["channel"].as_u64() == Some(u64::from(ack.route_channel))
            && event["corr"].as_u64() == Some(corr)
            && event["claimed"].as_bool() == Some(false)
    })
    .await;
    assert_eq!(cancel_event["claimed"].as_bool(), Some(false));
    assert_no_frame_within(&mut client, Duration::from_millis(100)).await;

    let followup_payload = br#"{"delay_ms":0,"jsonrpc":"2.0","id":"cancel-after-followup"}"#;
    write_frame(
        &mut client,
        &data_request(ack.route_channel, corr + 1, followup_payload),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();
    let followup = read_frame_timeout(&mut client).await;
    assert_response(&followup, ack.route_channel, corr + 1, followup_payload);

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn double_cancel_emits_exactly_one_cancelled_error() {
    let server = TestServer::start();
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-double-cancel";
    let events_path = server.stub_events_path("double-cancel");
    let module = supervisor
        .spawn(stub_spec_with_env(
            &server,
            module_id,
            [
                ("FAKE_AFT_DELAY_FROM_BODY".to_string(), "1".to_string()),
                (
                    "FAKE_AFT_EVENTS_PATH".to_string(),
                    events_path.to_string_lossy().into_owned(),
                ),
            ],
        ))
        .unwrap();
    wait_for_registration(&server.registry, module_id, Duration::from_secs(1)).await;

    let project = TestProject::new();
    let (mut client, ack) = attach_client(&server, &project, 821, "ses-double-cancel").await;
    let corr = 822;
    let payload = br#"{"delay_ms":500,"jsonrpc":"2.0","id":"double-cancel"}"#;
    write_frame(&mut client, &data_request(ack.route_channel, corr, payload))
        .await
        .unwrap();
    client.flush().await.unwrap();
    write_frame(&mut client, &cancel_frame(ack.route_channel, corr))
        .await
        .unwrap();
    write_frame(&mut client, &cancel_frame(ack.route_channel, corr))
        .await
        .unwrap();
    client.flush().await.unwrap();

    let terminal = read_frame_timeout(&mut client).await;
    assert_error(&terminal, ack.route_channel, corr, "cancelled");
    wait_for_stub_event(&events_path, Duration::from_secs(1), |event| {
        event["kind"] == "cancel"
            && event["channel"].as_u64() == Some(u64::from(ack.route_channel))
            && event["corr"].as_u64() == Some(corr)
            && event["claimed"].as_bool() == Some(true)
    })
    .await;
    wait_for_stub_event(&events_path, Duration::from_secs(1), |event| {
        event["kind"] == "cancel"
            && event["channel"].as_u64() == Some(u64::from(ack.route_channel))
            && event["corr"].as_u64() == Some(corr)
            && event["claimed"].as_bool() == Some(false)
    })
    .await;
    assert_no_frame_within(&mut client, Duration::from_millis(550)).await;

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_for_uncancellable_delayed_request_allows_normal_response() {
    let server = TestServer::start();
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-uncancellable";
    let events_path = server.stub_events_path("uncancellable");
    let module = supervisor
        .spawn(stub_spec_with_env(
            &server,
            module_id,
            [
                ("FAKE_AFT_DELAY_FROM_BODY".to_string(), "1".to_string()),
                (
                    "FAKE_AFT_EVENTS_PATH".to_string(),
                    events_path.to_string_lossy().into_owned(),
                ),
            ],
        ))
        .unwrap();
    wait_for_registration(&server.registry, module_id, Duration::from_secs(1)).await;

    let project = TestProject::new();
    let (mut client, ack) = attach_client(&server, &project, 831, "ses-uncancellable").await;
    let corr = 832;
    let payload = br#"{"delay_ms":200,"uncancellable":true,"jsonrpc":"2.0","id":"uncancellable"}"#;
    write_frame(&mut client, &data_request(ack.route_channel, corr, payload))
        .await
        .unwrap();
    client.flush().await.unwrap();

    let cancel_sent = Instant::now();
    write_frame(&mut client, &cancel_frame(ack.route_channel, corr))
        .await
        .unwrap();
    client.flush().await.unwrap();
    let cancel_event = wait_for_stub_event(&events_path, Duration::from_secs(1), |event| {
        event["kind"] == "cancel"
            && event["channel"].as_u64() == Some(u64::from(ack.route_channel))
            && event["corr"].as_u64() == Some(corr)
            && event["claimed"].as_bool() == Some(false)
    })
    .await;
    assert_eq!(cancel_event["claimed"].as_bool(), Some(false));

    let response = read_frame_timeout(&mut client).await;
    let latency = Instant::now().duration_since(cancel_sent);
    assert_response(&response, ack.route_channel, corr, payload);
    assert!(
        latency >= Duration::from_millis(150),
        "uncancellable request should complete after its configured delay; latency={latency:?}"
    );
    wait_for_stub_event(&events_path, Duration::from_secs(1), |event| {
        event["kind"] == "terminal"
            && event["terminal"] == "response"
            && event["channel"].as_u64() == Some(u64::from(ack.route_channel))
            && event["corr"].as_u64() == Some(corr)
    })
    .await;

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_channel_cancel_returns_unknown_channel_error_and_survives() {
    let server = TestServer::start();
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-unknown-cancel";
    let module = supervisor.spawn(stub_spec(&server, module_id)).unwrap();
    wait_for_registration(&server.registry, module_id, Duration::from_secs(1)).await;

    let project = TestProject::new();
    let (mut client, ack) = attach_client(&server, &project, 841, "ses-unknown-cancel").await;
    let unknown_channel = if ack.route_channel == u16::MAX {
        u16::MAX - 1
    } else {
        u16::MAX
    };
    assert_ne!(unknown_channel, ack.route_channel);

    let unknown_corr = 842;
    write_frame(&mut client, &cancel_frame(unknown_channel, unknown_corr))
        .await
        .unwrap();
    client.flush().await.unwrap();
    let error_frame = read_frame_timeout(&mut client).await;
    let body = assert_error(
        &error_frame,
        unknown_channel,
        unknown_corr,
        "unknown_channel",
    );
    assert!(body.message.contains("unknown channel"));

    let payload = br#"{"jsonrpc":"2.0","id":"after-unknown-cancel"}"#;
    write_frame(
        &mut client,
        &data_request(ack.route_channel, unknown_corr + 1, payload),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();
    let response = read_frame_timeout(&mut client).await;
    assert_response(&response, ack.route_channel, unknown_corr + 1, payload);

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multi_client_fanout_pushes_route_to_each_bound_client() {
    let server = TestServer::start();
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-push-fanout";
    let module = supervisor
        .spawn(stub_spec_with_env(
            &server,
            module_id,
            [("FAKE_AFT_FANOUT_ON_REQUEST", "1")],
        ))
        .unwrap();
    wait_for_registration(&server.registry, module_id, Duration::from_secs(1)).await;

    let project = TestProject::new();
    let (mut first, first_ack) = attach_client(&server, &project, 551, "ses-fanout-one").await;
    let (mut second, second_ack) = attach_client(&server, &project, 552, "ses-fanout-two").await;
    assert_ne!(first_ack.route_channel, second_ack.route_channel);
    assert_eq!(server.forwarding.active_binding_count().unwrap(), 2);

    let first_payload = br#"{"jsonrpc":"2.0","id":"fanout-trigger"}"#;
    write_frame(
        &mut first,
        &data_request(first_ack.route_channel, 553, first_payload),
    )
    .await
    .unwrap();
    first.flush().await.unwrap();

    let (first_push, _first_response) =
        read_until_push_and_response(&mut first, first_ack.route_channel, 553, first_payload).await;
    let second_push = read_push(&mut second, second_ack.route_channel).await;
    assert_ne!(first_push.header.channel, second_ack.route_channel);
    assert_ne!(second_push.header.channel, first_ack.route_channel);

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn single_client_pipelined_requests_preserve_corr_fifo_order() {
    let server = TestServer::start();
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-fifo";
    let module = supervisor.spawn(stub_spec(&server, module_id)).unwrap();
    wait_for_registration(&server.registry, module_id, Duration::from_secs(1)).await;

    let project = TestProject::new();
    let (mut client, ack) = attach_client(&server, &project, 601, "ses-fifo").await;
    let request_count = 8u64;
    for corr in 1..=request_count {
        let body = format!(r#"{{"jsonrpc":"2.0","id":{corr}}}"#);
        write_frame(
            &mut client,
            &data_request(ack.route_channel, corr, body.as_bytes()),
        )
        .await
        .unwrap();
    }
    client.flush().await.unwrap();

    let mut response_corrs = Vec::new();
    for expected_corr in 1..=request_count {
        let response = read_frame_timeout(&mut client).await;
        assert_eq!(response.header.ty, FrameType::Response);
        assert_eq!(response.header.channel, ack.route_channel);
        assert_eq!(
            response.body,
            format!(r#"{{"jsonrpc":"2.0","id":{expected_corr}}}"#).as_bytes()
        );
        response_corrs.push(response.header.corr);
    }
    assert_eq!(response_corrs, (1..=request_count).collect::<Vec<_>>());

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serial_flow_control_window_holds_second_request_until_terminal() {
    let server = TestServer::start();
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-serial-flow";
    let events_path = server.stub_events_path("serial-flow");
    let module = supervisor
        .spawn(stub_spec_with_env(
            &server,
            module_id,
            [
                ("FAKE_AFT_CONCURRENCY".to_string(), "serial".to_string()),
                ("FAKE_AFT_DELAY_FROM_BODY".to_string(), "1".to_string()),
                (
                    "FAKE_AFT_EVENTS_PATH".to_string(),
                    events_path.to_string_lossy().into_owned(),
                ),
            ],
        ))
        .unwrap();
    wait_for_registration(&server.registry, module_id, Duration::from_secs(1)).await;

    let project = TestProject::new();
    let (mut client, ack) = attach_client(&server, &project, 901, "ses-serial-flow").await;
    let first_corr = 902;
    let second_corr = 903;
    let first_payload = br#"{"delay_ms":300,"jsonrpc":"2.0","id":"serial-1"}"#;
    let second_payload = br#"{"delay_ms":0,"jsonrpc":"2.0","id":"serial-2"}"#;

    write_frame(
        &mut client,
        &data_request(ack.route_channel, first_corr, first_payload),
    )
    .await
    .unwrap();
    write_frame(
        &mut client,
        &data_request(ack.route_channel, second_corr, second_payload),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();

    wait_for_stub_event(&events_path, Duration::from_secs(1), |event| {
        event_is_request_received(event, ack.route_channel, first_corr)
    })
    .await;
    assert_no_stub_event_within(&events_path, Duration::from_millis(100), |event| {
        event_is_request_received(event, ack.route_channel, second_corr)
    })
    .await;
    wait_for_stub_event(&events_path, Duration::from_secs(1), |event| {
        event_is_terminal(event, "response", ack.route_channel, first_corr)
    })
    .await;
    wait_for_stub_event(&events_path, Duration::from_secs(1), |event| {
        event_is_request_received(event, ack.route_channel, second_corr)
    })
    .await;

    let first_response = read_frame_timeout(&mut client).await;
    assert_response(
        &first_response,
        ack.route_channel,
        first_corr,
        first_payload,
    );
    let second_response = read_frame_timeout(&mut client).await;
    assert_response(
        &second_response,
        ack.route_channel,
        second_corr,
        second_payload,
    );

    let events = stub_events(&events_path);
    let first_request_pos = event_position(&events, |event| {
        event_is_request_received(event, ack.route_channel, first_corr)
    });
    let first_terminal_pos = event_position(&events, |event| {
        event_is_terminal(event, "response", ack.route_channel, first_corr)
    });
    let second_request_pos = event_position(&events, |event| {
        event_is_request_received(event, ack.route_channel, second_corr)
    });
    assert!(
        first_request_pos < first_terminal_pos && first_terminal_pos < second_request_pos,
        "serial flow-control ordering violated: {events:?}"
    );

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_bypasses_full_flow_control_window_and_credit_frees_on_terminal() {
    let server = TestServer::start();
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-cancel-flow";
    let events_path = server.stub_events_path("cancel-flow");
    let module = supervisor
        .spawn(stub_spec_with_env(
            &server,
            module_id,
            [
                ("FAKE_AFT_CONCURRENCY".to_string(), "serial".to_string()),
                ("FAKE_AFT_DELAY_FROM_BODY".to_string(), "1".to_string()),
                (
                    "FAKE_AFT_EVENTS_PATH".to_string(),
                    events_path.to_string_lossy().into_owned(),
                ),
            ],
        ))
        .unwrap();
    wait_for_registration(&server.registry, module_id, Duration::from_secs(1)).await;

    let project = TestProject::new();
    let (mut client, ack) = attach_client(&server, &project, 911, "ses-cancel-flow").await;
    let cancelled_corr = 912;
    let followup_corr = 913;
    let cancellable_payload = br#"{"delay_ms":500,"jsonrpc":"2.0","id":"cancel-flow"}"#;
    let followup_payload = br#"{"delay_ms":0,"jsonrpc":"2.0","id":"after-cancel-flow"}"#;

    write_frame(
        &mut client,
        &data_request(ack.route_channel, cancelled_corr, cancellable_payload),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();
    wait_for_stub_event(&events_path, Duration::from_secs(1), |event| {
        event_is_request_received(event, ack.route_channel, cancelled_corr)
    })
    .await;
    sleep(Duration::from_millis(20)).await;

    let cancel_sent = Instant::now();
    write_frame(
        &mut client,
        &cancel_frame(ack.route_channel, cancelled_corr),
    )
    .await
    .unwrap();
    write_frame(
        &mut client,
        &data_request(ack.route_channel, followup_corr, followup_payload),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();

    let cancelled = read_frame_timeout(&mut client).await;
    let cancel_latency = Instant::now().duration_since(cancel_sent);
    assert_error(&cancelled, ack.route_channel, cancelled_corr, "cancelled");
    assert!(
        cancel_latency < Duration::from_millis(250),
        "cancel should bypass full window; latency={cancel_latency:?}"
    );
    let followup = read_frame_timeout(&mut client).await;
    assert_response(
        &followup,
        ack.route_channel,
        followup_corr,
        followup_payload,
    );

    wait_for_stub_event(&events_path, Duration::from_secs(1), |event| {
        event_is_terminal(event, "error", ack.route_channel, cancelled_corr)
            && event["code"] == "cancelled"
    })
    .await;
    wait_for_stub_event(&events_path, Duration::from_secs(1), |event| {
        event_is_request_received(event, ack.route_channel, followup_corr)
    })
    .await;
    let events = stub_events(&events_path);
    let cancelled_terminal_pos = event_position(&events, |event| {
        event_is_terminal(event, "error", ack.route_channel, cancelled_corr)
    });
    let followup_request_pos = event_position(&events, |event| {
        event_is_request_received(event, ack.route_channel, followup_corr)
    });
    assert!(
        cancelled_terminal_pos < followup_request_pos,
        "follow-up request should wait for cancelled terminal credit release: {events:?}"
    );

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn flow_control_over_release_guard_does_not_grow_serial_window() {
    let server = TestServer::start();
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-over-release";
    let events_path = server.stub_events_path("over-release");
    let module = supervisor
        .spawn(stub_spec_with_env(
            &server,
            module_id,
            [
                ("FAKE_AFT_CONCURRENCY".to_string(), "serial".to_string()),
                ("FAKE_AFT_DELAY_FROM_BODY".to_string(), "1".to_string()),
                ("FAKE_AFT_DOUBLE_TERMINAL".to_string(), "1".to_string()),
                (
                    "FAKE_AFT_EVENTS_PATH".to_string(),
                    events_path.to_string_lossy().into_owned(),
                ),
            ],
        ))
        .unwrap();
    wait_for_registration(&server.registry, module_id, Duration::from_secs(1)).await;

    let project = TestProject::new();
    let (mut client, ack) = attach_client(&server, &project, 921, "ses-over-release").await;
    let warmup_corr = 922;
    let warmup_payload = br#"{"delay_ms":0,"jsonrpc":"2.0","id":"warmup"}"#;
    write_frame(
        &mut client,
        &data_request(ack.route_channel, warmup_corr, warmup_payload),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();
    let warmup_first = read_frame_timeout(&mut client).await;
    assert_response(
        &warmup_first,
        ack.route_channel,
        warmup_corr,
        warmup_payload,
    );
    let warmup_duplicate = read_frame_timeout(&mut client).await;
    assert_response(
        &warmup_duplicate,
        ack.route_channel,
        warmup_corr,
        warmup_payload,
    );

    let slow_corr = 923;
    let fast_corr = 924;
    let slow_payload = br#"{"delay_ms":300,"jsonrpc":"2.0","id":"over-release-slow"}"#;
    let fast_payload = br#"{"delay_ms":0,"jsonrpc":"2.0","id":"over-release-fast"}"#;
    write_frame(
        &mut client,
        &data_request(ack.route_channel, slow_corr, slow_payload),
    )
    .await
    .unwrap();
    write_frame(
        &mut client,
        &data_request(ack.route_channel, fast_corr, fast_payload),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();

    wait_for_stub_event(&events_path, Duration::from_secs(1), |event| {
        event_is_request_received(event, ack.route_channel, slow_corr)
    })
    .await;
    assert_no_stub_event_within(&events_path, Duration::from_millis(100), |event| {
        event_is_request_received(event, ack.route_channel, fast_corr)
    })
    .await;
    wait_for_stub_event(&events_path, Duration::from_secs(1), |event| {
        event_is_terminal(event, "response", ack.route_channel, slow_corr)
    })
    .await;
    wait_for_stub_event(&events_path, Duration::from_secs(1), |event| {
        event_is_request_received(event, ack.route_channel, fast_corr)
    })
    .await;

    let events = stub_events(&events_path);
    let slow_terminal_pos = event_position(&events, |event| {
        event_is_terminal(event, "response", ack.route_channel, slow_corr)
    });
    let fast_request_pos = event_position(&events, |event| {
        event_is_request_received(event, ack.route_channel, fast_corr)
    });
    assert!(
        slow_terminal_pos < fast_request_pos,
        "double terminal grew serial window beyond one credit: {events:?}"
    );

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocked_flow_control_acquire_wakes_when_module_tears_down() {
    let server = TestServer::start();
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-flow-teardown";
    let events_path = server.stub_events_path("flow-teardown");
    let module = supervisor
        .spawn(stub_spec_with_env(
            &server,
            module_id,
            [
                ("FAKE_AFT_CONCURRENCY".to_string(), "serial".to_string()),
                ("FAKE_AFT_DELAY_FROM_BODY".to_string(), "1".to_string()),
                (
                    "FAKE_AFT_EVENTS_PATH".to_string(),
                    events_path.to_string_lossy().into_owned(),
                ),
            ],
        ))
        .unwrap();
    wait_for_registration(&server.registry, module_id, Duration::from_secs(1)).await;

    let project = TestProject::new();
    let (mut client, ack) = attach_client(&server, &project, 931, "ses-flow-teardown").await;
    let inflight_corr = 932;
    let blocked_corr = 933;
    let inflight_payload = br#"{"delay_ms":5000,"jsonrpc":"2.0","id":"inflight"}"#;
    let blocked_payload = br#"{"delay_ms":0,"jsonrpc":"2.0","id":"blocked"}"#;

    write_frame(
        &mut client,
        &data_request(ack.route_channel, inflight_corr, inflight_payload),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();
    wait_for_stub_event(&events_path, Duration::from_secs(1), |event| {
        event_is_request_received(event, ack.route_channel, inflight_corr)
    })
    .await;

    write_frame(
        &mut client,
        &data_request(ack.route_channel, blocked_corr, blocked_payload),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();
    sleep(Duration::from_millis(50)).await;
    assert_no_stub_event_within(&events_path, Duration::from_millis(100), |event| {
        event_is_request_received(event, ack.route_channel, blocked_corr)
    })
    .await;

    module.stop().await.unwrap();
    let outcome = read_frame_or_close_timeout(&mut client, Duration::from_secs(2)).await;
    if let Some(frame) = outcome {
        assert_error(&frame, ack.route_channel, blocked_corr, "backend_error");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn module_restart_invalidates_old_generation_route_and_fresh_attach_succeeds() {
    let server = TestServer::start();
    let supervisor = supervisor(&server, 4, Duration::from_millis(20));
    let module_id = "fake-aft-generation";
    let module = supervisor
        .spawn(stub_spec_with_env(
            &server,
            module_id,
            [("FAKE_AFT_CRASH_AFTER_MS", "1500")],
        ))
        .unwrap();
    wait_for_registration(&server.registry, module_id, Duration::from_secs(1)).await;

    let project = TestProject::new();
    let (mut client, old_ack) = attach_client(&server, &project, 701, "ses-old-generation").await;
    assert_eq!(server.forwarding.active_binding_count().unwrap(), 1);

    let status = wait_for_status(&module, Duration::from_secs(3), |status| {
        status.restart_count >= 1 && status.state == ModuleState::Running && status.live
    })
    .await;
    assert!(status.restart_count >= 1);
    wait_for_binding_count(&server.forwarding, 0, Duration::from_secs(1)).await;

    let stale_request = data_request(
        old_ack.route_channel,
        702,
        br#"{"jsonrpc":"2.0","id":"old"}"#,
    );
    write_frame(&mut client, &stale_request).await.unwrap();
    client.flush().await.unwrap();
    let stale_error = read_frame_timeout(&mut client).await;
    assert_eq!(stale_error.header.ty, FrameType::Error);
    assert_eq!(stale_error.header.channel, old_ack.route_channel);
    let body: ErrorBody = serde_json::from_slice(&stale_error.body).unwrap();
    assert!(matches!(
        body.code.as_str(),
        "unknown_channel" | "module_unavailable"
    ));
    assert_eq!(server.forwarding.active_binding_count().unwrap(), 0);

    let fresh_ack = attach_on_stream(&mut client, &project, 703, "ses-new-generation").await;
    assert!(fresh_ack.route_channel > 0);
    assert_eq!(server.forwarding.active_binding_count().unwrap(), 1);
    let payload = br#"{"jsonrpc":"2.0","id":"new"}"#;
    write_frame(
        &mut client,
        &data_request(fresh_ack.route_channel, 704, payload),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();
    let response = read_frame_timeout(&mut client).await;
    assert_eq!(response.header.ty, FrameType::Response);
    assert_eq!(response.header.channel, fresh_ack.route_channel);
    assert_eq!(response.header.corr, 704);
    assert_eq!(response.body, payload);

    module.stop().await.unwrap();
}

async fn attach_client(
    server: &TestServer,
    project: &TestProject,
    corr: u64,
    session: &str,
) -> (UnixStream, AttachAck) {
    let mut client = UnixStream::connect(&server.socket_path).await.unwrap();
    let ack = attach_on_stream(&mut client, project, corr, session).await;
    (client, ack)
}

async fn attach_on_stream(
    client: &mut UnixStream,
    project: &TestProject,
    corr: u64,
    session: &str,
) -> AttachAck {
    write_frame(
        client,
        &attach_frame(corr, attach_request(project, session)),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();

    let ack_frame = read_frame_timeout(client).await;
    assert_eq!(ack_frame.header.ty, FrameType::Response);
    assert_eq!(ack_frame.header.channel, 0);
    assert_eq!(ack_frame.header.corr, corr);
    serde_json::from_slice(&ack_frame.body).unwrap()
}

async fn attach_error_on_stream(
    client: &mut UnixStream,
    project: &TestProject,
    corr: u64,
    session: &str,
) -> ErrorBody {
    write_frame(
        client,
        &attach_frame(corr, attach_request(project, session)),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();

    let error_frame = read_frame_timeout(client).await;
    assert_eq!(error_frame.header.ty, FrameType::Error);
    assert_eq!(error_frame.header.channel, 0);
    assert_eq!(error_frame.header.corr, corr);
    serde_json::from_slice(&error_frame.body).unwrap()
}

fn attach_request(project: &TestProject, session: &str) -> AttachRequest {
    AttachRequest {
        project_root: project.path.clone(),
        harness: "opencode".to_string(),
        session: session.to_string(),
        config: attach_config(project, session),
    }
}

fn attach_config(project: &TestProject, session: &str) -> Vec<ConfigTier> {
    vec![
        ConfigTier {
            tier: "user".to_string(),
            source: "/abs/user/aft.jsonc".to_string(),
            doc: "{ // user defaults\n  \"auto_accept\": false\n}".to_string(),
        },
        ConfigTier {
            tier: "project".to_string(),
            source: project
                .path
                .join("aft.jsonc")
                .to_string_lossy()
                .into_owned(),
            doc: format!(r#"{{ "session": "{session}", "semantic": true }}"#),
        },
    ]
}

fn attach_frame(corr: u64, attach: AttachRequest) -> Frame {
    let body = serde_json::to_vec(&attach).unwrap();
    Frame::build(
        FrameType::Request,
        Flags::new(false, Priority::Interactive, false),
        0,
        corr,
        body,
    )
    .unwrap()
}

fn data_request(channel: u16, corr: u64, body: &[u8]) -> Frame {
    Frame::build(
        FrameType::Request,
        Flags::new(false, Priority::Interactive, false),
        channel,
        corr,
        body.to_vec(),
    )
    .unwrap()
}

fn cancel_frame(channel: u16, corr: u64) -> Frame {
    let frame = Frame::build(
        FrameType::Cancel,
        Flags::new(false, Priority::Interactive, false),
        channel,
        corr,
        Vec::new(),
    )
    .unwrap();
    assert_eq!(frame.header.len, 0);
    assert!(frame.body.is_empty());
    frame
}

async fn read_until_push_and_response(
    stream: &mut UnixStream,
    route_channel: u16,
    response_corr: u64,
    response_body: &[u8],
) -> (Frame, Frame) {
    let mut push = None;
    let mut response = None;

    for _ in 0..2 {
        let frame = read_frame_timeout(stream).await;
        assert_eq!(frame.header.channel, route_channel);
        match frame.header.ty {
            FrameType::Push if push.is_none() => {
                assert_push(&frame, route_channel);
                push = Some(frame);
            }
            FrameType::Response if response.is_none() => {
                assert_response(&frame, route_channel, response_corr, response_body);
                response = Some(frame);
            }
            ty => panic!("unexpected frame type while waiting for PUSH and Response: {ty:?}"),
        }
    }

    (
        push.expect("missing PUSH frame"),
        response.expect("missing Response frame"),
    )
}

async fn read_push(stream: &mut UnixStream, route_channel: u16) -> Frame {
    let frame = read_frame_timeout(stream).await;
    assert_push(&frame, route_channel);
    frame
}

fn assert_push(frame: &Frame, route_channel: u16) {
    assert_eq!(frame.header.ty, FrameType::Push);
    assert_eq!(frame.header.channel, route_channel);
    assert_eq!(frame.header.corr, 0);
    assert_eq!(frame.body, b"push-event");
}

fn assert_response(frame: &Frame, route_channel: u16, corr: u64, body: &[u8]) {
    assert_eq!(frame.header.ty, FrameType::Response);
    assert_eq!(frame.header.channel, route_channel);
    assert_eq!(frame.header.corr, corr);
    assert_eq!(frame.body, body);
}

fn assert_error(frame: &Frame, route_channel: u16, corr: u64, code: &str) -> ErrorBody {
    assert_eq!(frame.header.ty, FrameType::Error);
    assert_eq!(frame.header.channel, route_channel);
    assert_eq!(frame.header.corr, corr);
    let body: ErrorBody = serde_json::from_slice(&frame.body).unwrap();
    assert_eq!(body.code, code);
    body
}

async fn read_frame_timeout(stream: &mut UnixStream) -> Frame {
    timeout(READ_TIMEOUT, async {
        read_frame(stream)
            .await
            .unwrap()
            .expect("connection should stay open")
    })
    .await
    .expect("timed out waiting for frame")
}

async fn assert_no_frame_within(stream: &mut UnixStream, wait: Duration) {
    match timeout(wait, read_frame(stream)).await {
        Err(_) => {}
        Ok(Ok(Some(frame))) => panic!("unexpected frame within {wait:?}: {frame:?}"),
        Ok(Ok(None)) => panic!("connection closed while expecting no frame for {wait:?}"),
        Ok(Err(err)) => panic!("frame read failed while expecting no frame for {wait:?}: {err}"),
    }
}

fn supervisor(server: &TestServer, max_restarts: u32, backoff: Duration) -> Supervisor {
    Supervisor::new(
        Arc::clone(&server.registry),
        RestartPolicy::new(max_restarts, backoff),
    )
    .with_drain_timeout(Duration::from_millis(25))
}

fn stub_spec(server: &TestServer, module_id: &str) -> ModuleSpec {
    stub_spec_with_env(server, module_id, std::iter::empty::<(&str, &str)>())
}

fn stub_spec_with_env<K, V, I>(server: &TestServer, module_id: &str, extra_env: I) -> ModuleSpec
where
    K: Into<String>,
    V: Into<String>,
    I: IntoIterator<Item = (K, V)>,
{
    let mut env = vec![
        (
            SUBC_SOCKET_ENV.to_string(),
            server.socket_path.to_string_lossy().into_owned(),
        ),
        ("FAKE_AFT_MODULE_ID".to_string(), module_id.to_string()),
    ];
    env.extend(
        extra_env
            .into_iter()
            .map(|(key, value)| (key.into(), value.into())),
    );

    ModuleSpec {
        module_id: module_id.to_string(),
        program: PathBuf::from(env!("CARGO_BIN_EXE_fake-aft-stub")),
        args: Vec::new(),
        env,
    }
}

async fn wait_for_registration(
    registry: &Registry,
    module_id: &str,
    wait: Duration,
) -> subc_core::ModuleRegistration {
    let deadline = Instant::now() + wait;
    loop {
        if let Some(registration) = registry.get_module(module_id).unwrap() {
            return registration;
        }
        if Instant::now() >= deadline {
            panic!("module {module_id} did not register within {wait:?}");
        }
        sleep(Duration::from_millis(10)).await;
    }
}

async fn wait_for_registration_absent(registry: &Registry, module_id: &str, wait: Duration) {
    let deadline = Instant::now() + wait;
    loop {
        if registry.get_module(module_id).unwrap().is_none() {
            return;
        }
        if Instant::now() >= deadline {
            panic!("module {module_id} registration did not release within {wait:?}");
        }
        sleep(Duration::from_millis(10)).await;
    }
}

async fn wait_for_binding_count(forwarding: &ForwardingTable, expected: usize, wait: Duration) {
    let deadline = Instant::now() + wait;
    loop {
        let count = forwarding.active_binding_count().unwrap();
        if count == expected {
            return;
        }
        if Instant::now() >= deadline {
            panic!("forwarding binding count {count} did not become {expected} within {wait:?}");
        }
        sleep(Duration::from_millis(10)).await;
    }
}

async fn wait_for_status(
    module: &SupervisedModule,
    wait: Duration,
    matches: impl Fn(&ModuleStatus) -> bool,
) -> ModuleStatus {
    let deadline = Instant::now() + wait;
    loop {
        let status = module.status().unwrap();
        if matches(&status) {
            return status;
        }
        if Instant::now() >= deadline {
            panic!(
                "module {} did not reach expected status within {wait:?}; last status: {status:?}",
                module.module_id()
            );
        }
        sleep(Duration::from_millis(10)).await;
    }
}

async fn wait_for_stub_event<F>(path: &Path, wait: Duration, matches: F) -> Value
where
    F: Fn(&Value) -> bool,
{
    let deadline = Instant::now() + wait;
    loop {
        for event in stub_events(path) {
            if matches(&event) {
                return event;
            }
        }
        if Instant::now() >= deadline {
            panic!(
                "stub event did not appear within {wait:?}; events: {:?}",
                stub_events(path)
            );
        }
        sleep(Duration::from_millis(10)).await;
    }
}

async fn assert_no_stub_event_within<F>(path: &Path, wait: Duration, matches: F)
where
    F: Fn(&Value) -> bool,
{
    let deadline = Instant::now() + wait;
    loop {
        let events = stub_events(path);
        if let Some(event) = events.iter().find(|event| matches(event)) {
            panic!("unexpected stub event within {wait:?}: {event:?}; events: {events:?}");
        }
        if Instant::now() >= deadline {
            return;
        }
        sleep(Duration::from_millis(10)).await;
    }
}

fn event_is_request_received(event: &Value, channel: u16, corr: u64) -> bool {
    event["kind"] == "request_received"
        && event["channel"].as_u64() == Some(u64::from(channel))
        && event["corr"].as_u64() == Some(corr)
}

fn event_is_terminal(event: &Value, terminal: &str, channel: u16, corr: u64) -> bool {
    event["kind"] == "terminal"
        && event["terminal"] == terminal
        && event["channel"].as_u64() == Some(u64::from(channel))
        && event["corr"].as_u64() == Some(corr)
}

fn event_position<F>(events: &[Value], matches: F) -> usize
where
    F: FnMut(&Value) -> bool,
{
    events
        .iter()
        .position(matches)
        .unwrap_or_else(|| panic!("stub event missing from {events:?}"))
}

async fn read_frame_or_close_timeout(stream: &mut UnixStream, wait: Duration) -> Option<Frame> {
    timeout(wait, read_frame(stream))
        .await
        .expect("timed out waiting for frame or clean close")
        .expect("frame read failed while waiting for frame or clean close")
}

fn stub_events(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .ok()
        .into_iter()
        .flat_map(|contents| {
            contents
                .lines()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .collect::<Vec<_>>()
        })
        .collect()
}

struct TestProject {
    path: PathBuf,
}

impl TestProject {
    fn new() -> Self {
        let path = unique_temp_dir("forwarding-project");
        fs::create_dir_all(&path).unwrap();
        Self { path }
    }
}

impl Drop for TestProject {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn unique_temp_dir(label: &str) -> PathBuf {
    let nonce = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("sc-{label}-{}-{nonce}", process::id()))
}
