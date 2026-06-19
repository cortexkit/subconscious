use std::{
    collections::BTreeMap,
    fs,
    ops::Deref,
    path::{Path, PathBuf},
    process,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use serde_json::Value;
use subc_control::{ClientControlRequest, ClientControlResponse, PollKind};
use subc_core::{
    read_frame, write_frame, ForwardingTable, Frame, ModuleSpec, ModuleState, ModuleStatus,
    Registry, RestartPolicy, SupervisedModule, Supervisor, SupervisorProcessLiveness,
};
use subc_protocol::{
    manifest::{
        Bindings, ConfigBinding, ConfigSource, IdentityBinding, IdentityScope, ModuleManifest,
        ProviderRole, StorageBinding, StorageKind, StorageScope, TrustTier,
    },
    session::ConfigTier,
    BindIdentity, ErrorBody, Flags, FrameType, ModuleHelloAckBody, ModuleHelloBody, Priority,
    RouteTarget, PROTOCOL_VERSION,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
    time::{sleep, timeout, Instant},
};

mod common;
use common::{connect_authed_client, start_test_daemon_with_process_liveness, TestDaemon};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
const READ_TIMEOUT: Duration = Duration::from_secs(2);

struct TestServer {
    daemon: TestDaemon,
    process_liveness: Arc<SupervisorProcessLiveness>,
}

impl TestServer {
    async fn start() -> Self {
        let process_liveness = Arc::new(SupervisorProcessLiveness::new());
        let daemon =
            start_test_daemon_with_process_liveness("forwarding-server", process_liveness.clone())
                .await;
        Self {
            daemon,
            process_liveness,
        }
    }

    fn stub_events_path(&self, label: &str) -> PathBuf {
        self.temp_dir.join(format!("{label}-events.jsonl"))
    }
}

impl Deref for TestServer {
    type Target = TestDaemon;

    fn deref(&self) -> &Self::Target {
        &self.daemon
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn route_open_round_trip_via_tagged_shape_forwards_through_stub() {
    let server = TestServer::start().await;
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
    assert_eq!(
        attach_event["target"]["kind"].as_str(),
        Some("tool_provider")
    );
    assert_eq!(
        attach_event["target"]["module_id"].as_str(),
        Some(module_id)
    );
    let canonical_project = fs::canonicalize(&project.path).unwrap();
    assert_eq!(
        attach_event["identity"]["project_root"].as_str(),
        Some(canonical_project.to_str().unwrap())
    );
    assert_eq!(
        attach_event["identity"]["session"].as_str(),
        Some("ses-forwarding")
    );
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
async fn non_tool_provider_hello_registers_without_hijacking_active_forwarding_module() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let provider_id = "fake-aft-role-aware-provider";
    let events_path = server.stub_events_path("role-aware-provider");
    let provider = supervisor
        .spawn(stub_spec_with_env(
            &server,
            provider_id,
            [(
                "FAKE_AFT_EVENTS_PATH",
                events_path.to_string_lossy().into_owned(),
            )],
        ))
        .unwrap();
    wait_for_registration(&server.registry, provider_id, Duration::from_secs(1)).await;

    let consumer_id = "subc-mcp-consumer-role-aware";
    let mut consumer = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    let consumer_ack =
        register_manifest_on_stream(&mut consumer, consumer_manifest(consumer_id), 301).await;
    let consumer_registration =
        wait_for_registration(&server.registry, consumer_id, Duration::from_secs(1)).await;
    assert_eq!(consumer_ack.negotiated_ver, PROTOCOL_VERSION);
    assert_eq!(consumer_registration.manifest.module_id, consumer_id);

    let project = TestProject::new();
    let (mut client, ack) = attach_client(&server, &project, 303, "ses-role-aware").await;
    let attach = wait_for_stub_event(&events_path, Duration::from_secs(1), |event| {
        event["kind"] == "attach"
            && event["route_channel"].as_u64() == Some(u64::from(ack.route_channel))
    })
    .await;
    assert_eq!(
        attach["route_channel"].as_u64(),
        Some(u64::from(ack.route_channel))
    );

    let payload = br#"{"jsonrpc":"2.0","id":"role-aware"}"#;
    write_frame(&mut client, &data_request(ack.route_channel, 304, payload))
        .await
        .unwrap();
    client.flush().await.unwrap();
    let response = read_frame_timeout(&mut client).await;
    assert_response(&response, ack.route_channel, 304, payload);

    drop(consumer);
    provider.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn route_poll_produces_zero_module_frames() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-status-cache";
    let events_path = server.stub_events_path("status-cache");
    let module = supervisor
        .spawn(stub_spec_with_env(
            &server,
            module_id,
            [
                (
                    "FAKE_AFT_EVENTS_PATH".to_string(),
                    events_path.to_string_lossy().into_owned(),
                ),
                ("FAKE_AFT_STATUS".to_string(), "indexing".to_string()),
            ],
        ))
        .unwrap();
    wait_for_registration(&server.registry, module_id, Duration::from_secs(1)).await;

    let project = TestProject::new();
    let (mut client, ack) = attach_client(&server, &project, 111, "ses-status-cache").await;
    let status_event = wait_for_stub_event(&events_path, Duration::from_secs(1), |event| {
        event["kind"] == "status_published"
            && event["route_channel"].as_u64() == Some(u64::from(ack.route_channel))
    })
    .await;
    assert_eq!(status_event["status"].as_str(), Some("indexing"));

    let poll_corr = 112;
    write_frame(
        &mut client,
        &route_poll_frame(poll_corr, PollKind::Status, ack.route_channel),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();

    let response = read_frame_timeout(&mut client).await;
    assert_status_reply(&response, poll_corr, "indexing");

    let events = stub_events(&events_path);
    assert!(
        events.iter().all(|event| matches!(
            event.get("kind").and_then(Value::as_str),
            Some("attach" | "status_published")
        )),
        "status poll should be answered locally; unexpected stub events: {events:?}"
    );

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_and_liveness_polls_are_fast_while_serial_module_is_busy() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-busy-local-poll";
    let events_path = server.stub_events_path("busy-local-poll");
    let module = supervisor
        .spawn(stub_spec_with_env(
            &server,
            module_id,
            [
                ("FAKE_AFT_CONCURRENCY".to_string(), "serial".to_string()),
                ("FAKE_AFT_DELAY_FROM_BODY".to_string(), "1".to_string()),
                ("FAKE_AFT_STATUS".to_string(), "scanning".to_string()),
                (
                    "FAKE_AFT_EVENTS_PATH".to_string(),
                    events_path.to_string_lossy().into_owned(),
                ),
            ],
        ))
        .unwrap();
    wait_for_registration(&server.registry, module_id, Duration::from_secs(1)).await;

    let project = TestProject::new();
    let (mut client, ack) = attach_client(&server, &project, 121, "ses-busy-local-poll").await;
    wait_for_stub_event(&events_path, Duration::from_secs(1), |event| {
        event["kind"] == "status_published"
            && event["route_channel"].as_u64() == Some(u64::from(ack.route_channel))
            && event["status"] == "scanning"
    })
    .await;

    let data_corr = 122;
    let status_corr = 123;
    let liveness_corr = 124;
    let payload = br#"{"delay_ms":2000,"jsonrpc":"2.0","id":"busy"}"#;
    let data_sent = Instant::now();
    write_frame(
        &mut client,
        &data_request(ack.route_channel, data_corr, payload),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();
    wait_for_stub_event(&events_path, Duration::from_secs(1), |event| {
        event_is_request_received(event, ack.route_channel, data_corr)
    })
    .await;

    let polls_sent = Instant::now();
    write_frame(
        &mut client,
        &route_poll_frame(status_corr, PollKind::Status, ack.route_channel),
    )
    .await
    .unwrap();
    write_frame(
        &mut client,
        &route_poll_frame(liveness_corr, PollKind::Liveness, ack.route_channel),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();

    let status_response = read_frame_timeout(&mut client).await;
    let liveness_response = read_frame_timeout(&mut client).await;
    let poll_latency = Instant::now().duration_since(polls_sent);
    eprintln!("busy-module local poll latency: {poll_latency:?}");
    assert_status_reply(&status_response, status_corr, "scanning");
    assert_liveness_reply(&liveness_response, liveness_corr, true);
    assert!(
        poll_latency < Duration::from_millis(300),
        "passive polls queued behind busy module: poll_latency={poll_latency:?}"
    );

    let events = stub_events(&events_path);
    assert_stub_did_not_observe_corrs(&events, &[status_corr, liveness_corr]);

    let data_response = read_frame_timeout_for(&mut client, Duration::from_secs(3)).await;
    let data_latency = Instant::now().duration_since(data_sent);
    eprintln!("busy-module data response latency: {data_latency:?}");
    assert_response(&data_response, ack.route_channel, data_corr, payload);
    assert!(
        data_latency >= Duration::from_millis(1_800),
        "busy request did not exercise the requested delay: data_latency={data_latency:?}"
    );

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn route_poll_status_cache_miss_returns_none() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-status-miss";
    let module = supervisor.spawn(stub_spec(&server, module_id)).unwrap();
    wait_for_registration(&server.registry, module_id, Duration::from_secs(1)).await;

    let project = TestProject::new();
    let (mut client, ack) = attach_client(&server, &project, 131, "ses-status-miss").await;

    let poll_corr = 132;
    write_frame(
        &mut client,
        &route_poll_frame(poll_corr, PollKind::Status, ack.route_channel),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();

    let response = read_frame_timeout(&mut client).await;
    assert_status_none_reply(&response, poll_corr);

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_cache_is_evicted_when_client_detaches() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-status-eviction";
    let events_path = server.stub_events_path("status-eviction");
    let module = supervisor
        .spawn(stub_spec_with_env(
            &server,
            module_id,
            [
                (
                    "FAKE_AFT_EVENTS_PATH".to_string(),
                    events_path.to_string_lossy().into_owned(),
                ),
                ("FAKE_AFT_STATUS".to_string(), "evict-me".to_string()),
            ],
        ))
        .unwrap();
    wait_for_registration(&server.registry, module_id, Duration::from_secs(1)).await;

    let project = TestProject::new();
    let (mut first, first_ack) = attach_client(&server, &project, 141, "ses-status-evict-1").await;
    wait_for_stub_event(&events_path, Duration::from_secs(1), |event| {
        event["kind"] == "status_published"
            && event["route_channel"].as_u64() == Some(u64::from(first_ack.route_channel))
    })
    .await;

    write_frame(
        &mut first,
        &route_poll_frame(142, PollKind::Status, first_ack.route_channel),
    )
    .await
    .unwrap();
    first.flush().await.unwrap();
    let first_status = read_frame_timeout(&mut first).await;
    assert_status_reply(&first_status, 142, "evict-me");

    drop(first);
    wait_for_binding_count(&server.forwarding, 0, Duration::from_secs(1)).await;
    wait_for_stub_event(&events_path, Duration::from_secs(1), |event| {
        event["kind"] == "detach"
            && event["route_channel"].as_u64() == Some(u64::from(first_ack.route_channel))
    })
    .await;

    let (mut second, second_ack) =
        attach_client(&server, &project, 143, "ses-status-evict-2").await;
    let second_attach = wait_for_stub_event(&events_path, Duration::from_secs(1), |event| {
        event["kind"] == "attach" && event["identity"]["session"] == "ses-status-evict-2"
    })
    .await;
    let second_module_channel = second_attach["route_channel"].as_u64().unwrap();
    wait_for_stub_event(&events_path, Duration::from_secs(1), |event| {
        event["kind"] == "status_published"
            && event["route_channel"].as_u64() == Some(second_module_channel)
    })
    .await;

    write_frame(
        &mut second,
        &route_poll_frame(145, PollKind::Status, second_ack.route_channel),
    )
    .await
    .unwrap();
    second.flush().await.unwrap();
    let second_status = read_frame_timeout(&mut second).await;
    assert_status_reply(&second_status, 145, "evict-me");

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn liveness_poll_returns_false_after_module_connection_is_gone() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-liveness-gone";
    let module = supervisor.spawn(stub_spec(&server, module_id)).unwrap();
    wait_for_registration(&server.registry, module_id, Duration::from_secs(1)).await;

    let project = TestProject::new();
    let (mut client, ack) = attach_client(&server, &project, 151, "ses-liveness-gone").await;
    assert_liveness_reply(
        &poll_liveness(&mut client, 152, ack.route_channel).await,
        152,
        true,
    );

    module.stop().await.unwrap();
    wait_for_registration_absent(&server.registry, module_id, Duration::from_secs(1)).await;
    wait_for_binding_count(&server.forwarding, 0, Duration::from_secs(1)).await;
    let goodbye = read_frame_timeout(&mut client).await;
    assert_eq!(goodbye.header.ty, FrameType::Goodbye);
    assert_eq!(goodbye.header.channel, ack.route_channel);

    let response = poll_liveness(&mut client, 153, ack.route_channel).await;
    assert_liveness_reply(&response, 153, false);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn single_client_receives_unsolicited_push_and_response_on_bound_route() {
    let server = TestServer::start().await;
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
async fn client_drop_sends_route_goodbye_and_removes_binding() {
    let server = TestServer::start().await;
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
async fn nonzero_goodbye_detaches_one_route_and_leaves_sibling_route_live() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-route-goodbye";
    let events_path = server.stub_events_path("route-goodbye");
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
    let mut client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    let first_ack = attach_on_stream(&mut client, &project, 601, "ses-route-a", module_id).await;
    let second_ack = attach_on_stream(&mut client, &project, 602, "ses-route-b", module_id).await;
    assert_eq!(server.forwarding.active_binding_count().unwrap(), 2);

    write_frame(&mut client, &goodbye_frame(first_ack.route_channel, 603))
        .await
        .unwrap();
    client.flush().await.unwrap();

    wait_for_binding_count(&server.forwarding, 1, Duration::from_secs(1)).await;
    let detach = wait_for_stub_event(&events_path, Duration::from_secs(1), |event| {
        event["kind"] == "detach"
            && event["route_channel"].as_u64() == Some(u64::from(first_ack.route_channel))
    })
    .await;
    assert_eq!(
        detach["route_channel"].as_u64(),
        Some(u64::from(first_ack.route_channel))
    );
    assert!(!server
        .forwarding
        .has_route_channel(first_ack.route_channel)
        .unwrap());
    assert!(server
        .forwarding
        .has_route_channel(second_ack.route_channel)
        .unwrap());

    let stale_payload = br#"{"jsonrpc":"2.0","id":"route-a-after-goodbye"}"#;
    write_frame(
        &mut client,
        &data_request(first_ack.route_channel, 604, stale_payload),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();
    let error_frame = read_frame_timeout(&mut client).await;
    let body = assert_error(
        &error_frame,
        first_ack.route_channel,
        604,
        "unknown_channel",
    );
    assert!(body.message.contains("unknown channel"));

    let live_payload = br#"{"jsonrpc":"2.0","id":"route-b-live"}"#;
    write_frame(
        &mut client,
        &data_request(second_ack.route_channel, 605, live_payload),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();
    let response = read_frame_timeout(&mut client).await;
    assert_response(&response, second_ack.route_channel, 605, live_payload);

    let mut unknown_channel = u16::MAX;
    while unknown_channel == first_ack.route_channel || unknown_channel == second_ack.route_channel
    {
        unknown_channel -= 1;
    }
    write_frame(&mut client, &goodbye_frame(unknown_channel, 606))
        .await
        .unwrap();
    client.flush().await.unwrap();
    assert_no_frame_within(&mut client, Duration::from_millis(100)).await;

    let after_unknown_payload = br#"{"jsonrpc":"2.0","id":"route-b-after-unknown"}"#;
    write_frame(
        &mut client,
        &data_request(second_ack.route_channel, 607, after_unknown_payload),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();
    let response = read_frame_timeout(&mut client).await;
    assert_response(
        &response,
        second_ack.route_channel,
        607,
        after_unknown_payload,
    );

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn module_frame_after_client_detach_is_dropped_and_connection_survives() {
    let server = TestServer::start().await;
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
async fn module_error_lane_rejection_is_relayed_verbatim_without_committing_binding_then_accepts_later(
) {
    let server = TestServer::start().await;
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
    let mut client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    let error = attach_error_on_stream(&mut client, &project, 401, "ses-reject", module_id).await;
    assert_eq!(error.code, "config_divergence");
    assert_eq!(
        error.message,
        "fake AFT rejected route.bind by FAKE_AFT_REJECT_ATTACH"
    );
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
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-two-clients";
    let module = supervisor.spawn(stub_spec(&server, module_id)).unwrap();
    wait_for_registration(&server.registry, module_id, Duration::from_secs(1)).await;

    let project = TestProject::new();
    let (mut first, first_ack) = attach_client(&server, &project, 501, "ses-one").await;
    let (mut second, second_ack) = attach_client(&server, &project, 502, "ses-two").await;
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
    let server = TestServer::start().await;
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
async fn same_channel_responses_return_out_of_order_by_corr() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-same-channel-oood";
    let events_path = server.stub_events_path("same-channel-oood");
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
    let (mut client, ack) = attach_client(&server, &project, 631, "ses-same-channel-oood").await;

    const CA: u64 = 632;
    const CB: u64 = 633;
    let payload_a = br#"{"delay_ms":300,"jsonrpc":"2.0","id":"req-a"}"#;
    let payload_b = br#"{"delay_ms":0,"jsonrpc":"2.0","id":"req-b"}"#;

    write_frame(&mut client, &data_request(ack.route_channel, CA, payload_a))
        .await
        .unwrap();
    write_frame(&mut client, &data_request(ack.route_channel, CB, payload_b))
        .await
        .unwrap();
    client.flush().await.unwrap();
    let sent = Instant::now();

    let first_response = read_frame_timeout(&mut client).await;
    let first_received_at = Instant::now();
    let second_response = read_frame_timeout(&mut client).await;
    let second_received_at = Instant::now();

    assert_response(&first_response, ack.route_channel, CB, payload_b);
    assert_response(&second_response, ack.route_channel, CA, payload_a);

    let b_latency = first_received_at.duration_since(sent);
    let a_latency = second_received_at.duration_since(sent);
    eprintln!(
        "same-channel out-of-order latencies: B(corr={CB})={b_latency:?}, A(corr={CA})={a_latency:?}"
    );
    assert!(
        first_received_at < second_received_at,
        "B should arrive before A: b_latency={b_latency:?}, a_latency={a_latency:?}"
    );
    assert!(
        b_latency < Duration::from_millis(80),
        "fast request B should not wait behind A's delay: b_latency={b_latency:?}"
    );
    assert!(
        a_latency >= Duration::from_millis(250),
        "slow request A should reflect ~300ms delay: a_latency={a_latency:?}"
    );

    let events = stub_events(&events_path);
    let a_recv_pos = event_position(&events, |event| {
        event_is_request_received(event, ack.route_channel, CA)
    });
    let b_recv_pos = event_position(&events, |event| {
        event_is_request_received(event, ack.route_channel, CB)
    });
    let a_terminal_pos = event_position(&events, |event| {
        event_is_terminal(event, "response", ack.route_channel, CA)
    });
    assert!(
        a_recv_pos < a_terminal_pos && b_recv_pos < a_terminal_pos,
        "A and B should both be in-flight at the stub before A's terminal: {events:?}"
    );

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_before_response_for_cancellable_request_returns_cancelled_error() {
    let server = TestServer::start().await;
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
    let server = TestServer::start().await;
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
    let server = TestServer::start().await;
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
    let server = TestServer::start().await;
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
    let server = TestServer::start().await;
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
    let server = TestServer::start().await;
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
    assert_eq!(first_push.header.channel, first_ack.route_channel);
    assert_eq!(second_push.header.channel, second_ack.route_channel);

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn single_client_pipelined_requests_preserve_corr_fifo_order() {
    let server = TestServer::start().await;
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
    let server = TestServer::start().await;
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
    let server = TestServer::start().await;
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
    let server = TestServer::start().await;
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
    let server = TestServer::start().await;
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
        if frame.header.ty == FrameType::Goodbye {
            assert_eq!(frame.header.channel, ack.route_channel);
        } else {
            assert_error(&frame, ack.route_channel, blocked_corr, "backend_error");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn module_restart_invalidates_old_generation_route_and_fresh_attach_succeeds() {
    let server = TestServer::start().await;
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
    let goodbye = read_frame_timeout(&mut client).await;
    assert_eq!(goodbye.header.ty, FrameType::Goodbye);
    assert_eq!(goodbye.header.channel, old_ack.route_channel);

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
        "unknown_channel" | "target_unavailable"
    ));
    assert_eq!(server.forwarding.active_binding_count().unwrap(), 0);

    let fresh_ack =
        attach_on_stream(&mut client, &project, 703, "ses-new-generation", module_id).await;
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multi_provider_one_client_two_providers_rewrites_independent_channel_spaces() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_a = "fake-aft-mp-one-client-a";
    let module_b = "fake-aft-mp-one-client-b";
    let events_a = server.stub_events_path("mp-one-client-a");
    let events_b = server.stub_events_path("mp-one-client-b");
    let provider_a = supervisor
        .spawn(stub_spec_with_env(
            &server,
            module_a,
            [(
                "FAKE_AFT_EVENTS_PATH",
                events_a.to_string_lossy().into_owned(),
            )],
        ))
        .unwrap();
    let provider_b = supervisor
        .spawn(stub_spec_with_env(
            &server,
            module_b,
            [(
                "FAKE_AFT_EVENTS_PATH",
                events_b.to_string_lossy().into_owned(),
            )],
        ))
        .unwrap();
    wait_for_registration(&server.registry, module_a, Duration::from_secs(1)).await;
    wait_for_registration(&server.registry, module_b, Duration::from_secs(1)).await;

    let project = TestProject::new();
    let mut client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    let ack_a = attach_on_stream(&mut client, &project, 1001, "ses-mp-a", module_a).await;
    let ack_b = attach_on_stream(&mut client, &project, 1002, "ses-mp-b", module_b).await;
    assert_ne!(ack_a.route_channel, ack_b.route_channel);
    let attach_a = wait_for_stub_event(&events_a, Duration::from_secs(1), |event| {
        event["kind"] == "attach" && event["identity"]["session"] == "ses-mp-a"
    })
    .await;
    let attach_b = wait_for_stub_event(&events_b, Duration::from_secs(1), |event| {
        event["kind"] == "attach" && event["identity"]["session"] == "ses-mp-b"
    })
    .await;
    assert_eq!(attach_a["route_channel"].as_u64(), Some(1));
    assert_eq!(attach_b["route_channel"].as_u64(), Some(1));

    let payload_a = br#"{"jsonrpc":"2.0","id":"provider-a"}"#;
    let payload_b = br#"{"jsonrpc":"2.0","id":"provider-b"}"#;
    write_frame(
        &mut client,
        &data_request(ack_a.route_channel, 1003, payload_a),
    )
    .await
    .unwrap();
    write_frame(
        &mut client,
        &data_request(ack_b.route_channel, 1004, payload_b),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();
    let first = read_frame_timeout(&mut client).await;
    let second = read_frame_timeout(&mut client).await;
    let responses = [&first, &second];
    assert!(responses
        .iter()
        .any(|frame| frame.header.channel == ack_a.route_channel
            && frame.header.corr == 1003
            && frame.body == payload_a));
    assert!(responses
        .iter()
        .any(|frame| frame.header.channel == ack_b.route_channel
            && frame.header.corr == 1004
            && frame.body == payload_b));

    provider_a.stop().await.unwrap();
    provider_b.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multi_provider_two_clients_one_provider_get_distinct_module_channels() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-mp-two-clients";
    let events_path = server.stub_events_path("mp-two-clients");
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
    let (mut first, first_ack) = attach_client(&server, &project, 1011, "ses-mp-two-one").await;
    let (mut second, second_ack) = attach_client(&server, &project, 1012, "ses-mp-two-two").await;
    let first_attach = wait_for_stub_event(&events_path, Duration::from_secs(1), |event| {
        event["kind"] == "attach" && event["identity"]["session"] == "ses-mp-two-one"
    })
    .await;
    let second_attach = wait_for_stub_event(&events_path, Duration::from_secs(1), |event| {
        event["kind"] == "attach" && event["identity"]["session"] == "ses-mp-two-two"
    })
    .await;
    let first_module_channel = first_attach["route_channel"].as_u64().unwrap();
    let second_module_channel = second_attach["route_channel"].as_u64().unwrap();
    assert_ne!(first_module_channel, second_module_channel);

    let first_payload = br#"{"jsonrpc":"2.0","id":"first-client"}"#;
    let second_payload = br#"{"jsonrpc":"2.0","id":"second-client"}"#;
    write_frame(
        &mut first,
        &data_request(first_ack.route_channel, 1013, first_payload),
    )
    .await
    .unwrap();
    write_frame(
        &mut second,
        &data_request(second_ack.route_channel, 1014, second_payload),
    )
    .await
    .unwrap();
    first.flush().await.unwrap();
    second.flush().await.unwrap();
    assert_response(
        &read_frame_timeout(&mut first).await,
        first_ack.route_channel,
        1013,
        first_payload,
    );
    assert_response(
        &read_frame_timeout(&mut second).await,
        second_ack.route_channel,
        1014,
        second_payload,
    );

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multi_provider_status_cache_translates_same_module_channel_to_client_routes() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_a = "fake-aft-mp-status-a";
    let module_b = "fake-aft-mp-status-b";
    let events_a = server.stub_events_path("mp-status-a");
    let events_b = server.stub_events_path("mp-status-b");
    let provider_a = supervisor
        .spawn(stub_spec_with_env(
            &server,
            module_a,
            [
                (
                    "FAKE_AFT_EVENTS_PATH".to_string(),
                    events_a.to_string_lossy().into_owned(),
                ),
                ("FAKE_AFT_STATUS".to_string(), "status-a".to_string()),
            ],
        ))
        .unwrap();
    let provider_b = supervisor
        .spawn(stub_spec_with_env(
            &server,
            module_b,
            [
                (
                    "FAKE_AFT_EVENTS_PATH".to_string(),
                    events_b.to_string_lossy().into_owned(),
                ),
                ("FAKE_AFT_STATUS".to_string(), "status-b".to_string()),
            ],
        ))
        .unwrap();
    wait_for_registration(&server.registry, module_a, Duration::from_secs(1)).await;
    wait_for_registration(&server.registry, module_b, Duration::from_secs(1)).await;

    let project = TestProject::new();
    let mut client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    let ack_a = attach_on_stream(&mut client, &project, 1021, "ses-status-a", module_a).await;
    let ack_b = attach_on_stream(&mut client, &project, 1022, "ses-status-b", module_b).await;
    wait_for_stub_event(&events_a, Duration::from_secs(1), |event| {
        event["kind"] == "status_published" && event["route_channel"].as_u64() == Some(1)
    })
    .await;
    wait_for_stub_event(&events_b, Duration::from_secs(1), |event| {
        event["kind"] == "status_published" && event["route_channel"].as_u64() == Some(1)
    })
    .await;

    write_frame(
        &mut client,
        &route_poll_frame(1023, PollKind::Status, ack_a.route_channel),
    )
    .await
    .unwrap();
    write_frame(
        &mut client,
        &route_poll_frame(1024, PollKind::Status, ack_b.route_channel),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();
    assert_status_reply(&read_frame_timeout(&mut client).await, 1023, "status-a");
    assert_status_reply(&read_frame_timeout(&mut client).await, 1024, "status-b");

    provider_a.stop().await.unwrap();
    provider_b.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multi_provider_cancel_rewrites_divergent_client_and_module_channels() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-mp-cancel";
    let events_path = server.stub_events_path("mp-cancel");
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
    let (_first, _first_ack) = attach_client(&server, &project, 1031, "ses-cancel-primer").await;
    let (mut second, second_ack) =
        attach_client(&server, &project, 1032, "ses-cancel-divergent").await;
    let attach = wait_for_stub_event(&events_path, Duration::from_secs(1), |event| {
        event["kind"] == "attach" && event["identity"]["session"] == "ses-cancel-divergent"
    })
    .await;
    let module_channel = attach["route_channel"].as_u64().unwrap() as u16;
    assert_ne!(second_ack.route_channel, module_channel);

    let corr = 1033;
    let payload = br#"{"delay_ms":500,"jsonrpc":"2.0","id":"cancel-divergent"}"#;
    write_frame(
        &mut second,
        &data_request(second_ack.route_channel, corr, payload),
    )
    .await
    .unwrap();
    second.flush().await.unwrap();
    wait_for_stub_event(&events_path, Duration::from_secs(1), |event| {
        event_is_request_received(event, module_channel, corr)
    })
    .await;
    write_frame(&mut second, &cancel_frame(second_ack.route_channel, corr))
        .await
        .unwrap();
    second.flush().await.unwrap();
    assert_error(
        &read_frame_timeout(&mut second).await,
        second_ack.route_channel,
        corr,
        "cancelled",
    );
    wait_for_stub_event(&events_path, Duration::from_secs(1), |event| {
        event["kind"] == "cancel"
            && event["channel"].as_u64() == Some(u64::from(module_channel))
            && event["corr"].as_u64() == Some(corr)
            && event["claimed"].as_bool() == Some(true)
    })
    .await;

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multi_provider_generation_invalidation_goodbyes_restarted_provider_only() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 4, Duration::from_millis(20));
    let module_a = "fake-aft-mp-generation-a";
    let module_b = "fake-aft-mp-generation-b";
    let provider_a = supervisor.spawn(stub_spec(&server, module_a)).unwrap();
    let provider_b = supervisor
        .spawn(stub_spec_with_env(
            &server,
            module_b,
            [("FAKE_AFT_CRASH_AFTER_MS", "800")],
        ))
        .unwrap();
    wait_for_registration(&server.registry, module_a, Duration::from_secs(1)).await;
    wait_for_registration(&server.registry, module_b, Duration::from_secs(1)).await;

    let project = TestProject::new();
    let mut client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    let ack_a = attach_on_stream(&mut client, &project, 1041, "ses-gen-a", module_a).await;
    let old_ack_b = attach_on_stream(&mut client, &project, 1042, "ses-gen-b", module_b).await;

    wait_for_status(&provider_b, Duration::from_secs(3), |status| {
        status.restart_count >= 1 && status.state == ModuleState::Running && status.live
    })
    .await;
    let goodbye = read_frame_timeout(&mut client).await;
    assert_eq!(goodbye.header.ty, FrameType::Goodbye);
    assert_eq!(goodbye.header.channel, old_ack_b.route_channel);

    let payload_a = br#"{"jsonrpc":"2.0","id":"a-still-live"}"#;
    write_frame(
        &mut client,
        &data_request(ack_a.route_channel, 1043, payload_a),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();
    assert_response(
        &read_frame_timeout(&mut client).await,
        ack_a.route_channel,
        1043,
        payload_a,
    );
    let fresh_ack_b =
        attach_on_stream(&mut client, &project, 1044, "ses-gen-b-fresh", module_b).await;
    assert_ne!(fresh_ack_b.route_channel, old_ack_b.route_channel);

    provider_a.stop().await.unwrap();
    provider_b.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multi_provider_module_death_sends_goodbye_to_each_affected_client() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let module_id = "fake-aft-mp-death";
    let module = supervisor.spawn(stub_spec(&server, module_id)).unwrap();
    wait_for_registration(&server.registry, module_id, Duration::from_secs(1)).await;

    let project = TestProject::new();
    let (mut first, first_ack) = attach_client(&server, &project, 1051, "ses-death-one").await;
    let (mut second, second_ack) = attach_client(&server, &project, 1052, "ses-death-two").await;
    assert_eq!(server.forwarding.active_binding_count().unwrap(), 2);

    module.stop().await.unwrap();
    wait_for_binding_count(&server.forwarding, 0, Duration::from_secs(1)).await;
    let first_goodbye = read_frame_timeout(&mut first).await;
    let second_goodbye = read_frame_timeout(&mut second).await;
    assert_eq!(first_goodbye.header.ty, FrameType::Goodbye);
    assert_eq!(first_goodbye.header.channel, first_ack.route_channel);
    assert_eq!(second_goodbye.header.ty, FrameType::Goodbye);
    assert_eq!(second_goodbye.header.channel, second_ack.route_channel);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multi_provider_route_open_error_mapping_unknown_unavailable_and_verbatim_rejection() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server, 1, Duration::from_millis(10));
    let project = TestProject::new();

    let mut client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    let unknown = attach_error_on_stream(
        &mut client,
        &project,
        1061,
        "ses-unknown",
        "missing-provider",
    )
    .await;
    assert_eq!(unknown.code, "unknown_module");

    let mut consumer = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    register_manifest_on_stream(
        &mut consumer,
        consumer_manifest("consumer-only-provider"),
        1062,
    )
    .await;
    let unavailable = attach_error_on_stream(
        &mut client,
        &project,
        1063,
        "ses-wrong-role",
        "consumer-only-provider",
    )
    .await;
    assert_eq!(unavailable.code, "target_unavailable");

    let rejecting_id = "fake-aft-mp-reject";
    let rejecting = supervisor
        .spawn(stub_spec_with_env(
            &server,
            rejecting_id,
            [("FAKE_AFT_REJECT_ATTACH", "1")],
        ))
        .unwrap();
    wait_for_registration(&server.registry, rejecting_id, Duration::from_secs(1)).await;
    let rejected =
        attach_error_on_stream(&mut client, &project, 1064, "ses-reject", rejecting_id).await;
    assert_eq!(rejected.code, "config_divergence");
    assert_eq!(
        rejected.message,
        "fake AFT rejected route.bind by FAKE_AFT_REJECT_ATTACH"
    );
    assert_eq!(server.forwarding.active_binding_count().unwrap(), 0);

    drop(consumer);
    rejecting.stop().await.unwrap();
}

#[derive(Debug, Clone, Copy)]
struct RouteOpenAck {
    route_channel: u16,
}

async fn attach_client(
    server: &TestServer,
    project: &TestProject,
    corr: u64,
    session: &str,
) -> (TcpStream, RouteOpenAck) {
    let module_id = active_tool_provider_module_id(&server.registry);
    let mut client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    let ack = attach_on_stream(&mut client, project, corr, session, &module_id).await;
    (client, ack)
}

async fn attach_on_stream<S>(
    client: &mut S,
    project: &TestProject,
    corr: u64,
    session: &str,
    module_id: &str,
) -> RouteOpenAck
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    write_frame(
        client,
        &attach_frame(corr, attach_request(project, session, module_id)),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();

    let ack_frame = read_frame_timeout(client).await;
    assert_eq!(ack_frame.header.ty, FrameType::Response);
    assert_eq!(ack_frame.header.channel, 0);
    assert_eq!(ack_frame.header.corr, corr);
    match serde_json::from_slice(&ack_frame.body).unwrap() {
        ClientControlResponse::RouteOpen { route_channel } => RouteOpenAck { route_channel },
        other => panic!("unexpected route.open response: {other:?}"),
    }
}

async fn attach_error_on_stream<S>(
    client: &mut S,
    project: &TestProject,
    corr: u64,
    session: &str,
    module_id: &str,
) -> ErrorBody
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    write_frame(
        client,
        &attach_frame(corr, attach_request(project, session, module_id)),
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

fn active_tool_provider_module_id(registry: &Registry) -> String {
    let (_generation, modules) = registry.list_modules().unwrap();
    modules
        .into_iter()
        .find(|registration| {
            registration
                .manifest
                .provides
                .iter()
                .any(|role| matches!(role, ProviderRole::ToolProvider { .. }))
        })
        .map(|registration| registration.manifest.module_id)
        .expect("test should have a registered tool provider")
}

fn consumer_manifest(module_id: &str) -> ModuleManifest {
    ModuleManifest {
        module_id: module_id.to_string(),
        module_version: "0.0.0-consumer".to_string(),
        protocol_ver: PROTOCOL_VERSION,
        trust_tier: TrustTier::FirstParty,
        provides: Vec::new(),
        consumes: Vec::new(),
        scheduled_tasks: Vec::new(),
        bindings: Bindings {
            storage: StorageBinding {
                kind: StorageKind::Sqlite,
                scope: StorageScope::Project,
                owns_schema: false,
            },
            config: ConfigBinding {
                source: ConfigSource::SubcMediated,
                tiers: Vec::new(),
                expansion: BTreeMap::new(),
            },
            vault_grants: Vec::new(),
            identity: IdentityBinding {
                requires: Vec::new(),
                optional: vec![IdentityScope::Project],
            },
        },
    }
}

fn hello_frame(manifest: ModuleManifest, corr: u64) -> Frame {
    let protocol_ver = manifest.protocol_ver;
    let body = serde_json::to_vec(&ModuleHelloBody {
        manifest,
        protocol_ver,
    })
    .unwrap();
    Frame::build(
        FrameType::Hello,
        Flags::new(false, Priority::Passive, false),
        0,
        corr,
        body,
    )
    .unwrap()
}

async fn register_manifest_on_stream<S>(
    stream: &mut S,
    manifest: ModuleManifest,
    corr: u64,
) -> ModuleHelloAckBody
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    write_frame(stream, &hello_frame(manifest, corr))
        .await
        .unwrap();
    stream.flush().await.unwrap();

    let ack_frame = read_frame_timeout(stream).await;
    assert_eq!(ack_frame.header.ty, FrameType::HelloAck);
    assert_eq!(ack_frame.header.channel, 0);
    assert_eq!(ack_frame.header.corr, corr);
    serde_json::from_slice(&ack_frame.body).unwrap()
}

fn attach_request(project: &TestProject, session: &str, module_id: &str) -> ClientControlRequest {
    ClientControlRequest::RouteOpen {
        target: RouteTarget::ToolProvider {
            module_id: module_id.to_string(),
        },
        identity: BindIdentity {
            project_root: project.path.clone(),
            harness: "opencode".to_string(),
            session: session.to_string(),
        },
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

fn attach_frame(corr: u64, attach: ClientControlRequest) -> Frame {
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

fn goodbye_frame(channel: u16, corr: u64) -> Frame {
    let frame = Frame::build(
        FrameType::Goodbye,
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

fn route_poll_frame(corr: u64, kind: PollKind, route_channel: u16) -> Frame {
    let body = serde_json::to_vec(&ClientControlRequest::RoutePoll {
        route_channel,
        kind,
    })
    .unwrap();
    Frame::build(
        FrameType::Request,
        Flags::new(false, Priority::Passive, false),
        0,
        corr,
        body,
    )
    .unwrap()
}

async fn poll_liveness<S>(stream: &mut S, corr: u64, route_channel: u16) -> Frame
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    write_frame(
        stream,
        &route_poll_frame(corr, PollKind::Liveness, route_channel),
    )
    .await
    .unwrap();
    stream.flush().await.unwrap();
    read_frame_timeout(stream).await
}

async fn read_until_push_and_response<S>(
    stream: &mut S,
    route_channel: u16,
    response_corr: u64,
    response_body: &[u8],
) -> (Frame, Frame)
where
    S: AsyncRead + Unpin,
{
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

async fn read_push<S>(stream: &mut S, route_channel: u16) -> Frame
where
    S: AsyncRead + Unpin,
{
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

fn assert_status_reply(frame: &Frame, corr: u64, expected_status: &str) {
    assert_eq!(frame.header.ty, FrameType::Response);
    assert_eq!(frame.header.channel, 0);
    assert_eq!(frame.header.corr, corr);
    match serde_json::from_slice(&frame.body).unwrap() {
        ClientControlResponse::RoutePoll {
            status: Some(status),
            live: None,
        } => assert_eq!(status, expected_status),
        other => panic!("unexpected route.poll status response: {other:?}"),
    }
}

fn assert_status_none_reply(frame: &Frame, corr: u64) {
    assert_eq!(frame.header.ty, FrameType::Response);
    assert_eq!(frame.header.channel, 0);
    assert_eq!(frame.header.corr, corr);
    match serde_json::from_slice(&frame.body).unwrap() {
        ClientControlResponse::RoutePoll {
            status: None,
            live: None,
        } => {}
        other => panic!("unexpected route.poll missing-status response: {other:?}"),
    }
}

fn assert_liveness_reply(frame: &Frame, corr: u64, expected_live: bool) {
    assert_eq!(frame.header.ty, FrameType::Response);
    assert_eq!(frame.header.channel, 0);
    assert_eq!(frame.header.corr, corr);
    match serde_json::from_slice(&frame.body).unwrap() {
        ClientControlResponse::RoutePoll {
            status: None,
            live: Some(live),
        } => assert_eq!(live, expected_live),
        other => panic!("unexpected route.poll liveness response: {other:?}"),
    }
}

fn assert_error(frame: &Frame, route_channel: u16, corr: u64, code: &str) -> ErrorBody {
    assert_eq!(frame.header.ty, FrameType::Error);
    assert_eq!(frame.header.channel, route_channel);
    assert_eq!(frame.header.corr, corr);
    let body: ErrorBody = serde_json::from_slice(&frame.body).unwrap();
    assert_eq!(body.code, code);
    body
}

async fn read_frame_timeout<S>(stream: &mut S) -> Frame
where
    S: AsyncRead + Unpin,
{
    timeout(READ_TIMEOUT, async {
        read_frame(stream)
            .await
            .unwrap()
            .expect("connection should stay open")
    })
    .await
    .expect("timed out waiting for frame")
}

async fn read_frame_timeout_for<S>(stream: &mut S, wait: Duration) -> Frame
where
    S: AsyncRead + Unpin,
{
    timeout(wait, async {
        read_frame(stream)
            .await
            .unwrap()
            .expect("connection should stay open")
    })
    .await
    .expect("timed out waiting for frame")
}

async fn assert_no_frame_within<S>(stream: &mut S, wait: Duration)
where
    S: AsyncRead + Unpin,
{
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
    .with_process_liveness(Arc::clone(&server.process_liveness))
    .with_drain_timeout(Duration::from_millis(25))
    .with_connection_file_path(server.connection_file_path.clone())
}

fn stub_spec(server: &TestServer, module_id: &str) -> ModuleSpec {
    stub_spec_with_env(server, module_id, std::iter::empty::<(&str, &str)>())
}

fn stub_spec_with_env<K, V, I>(_server: &TestServer, module_id: &str, extra_env: I) -> ModuleSpec
where
    K: Into<String>,
    V: Into<String>,
    I: IntoIterator<Item = (K, V)>,
{
    let mut env = vec![("FAKE_AFT_MODULE_ID".to_string(), module_id.to_string())];
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

fn assert_stub_did_not_observe_corrs(events: &[Value], forbidden_corrs: &[u64]) {
    for event in events {
        if let Some(corr) = event.get("corr").and_then(Value::as_u64) {
            assert!(
                !forbidden_corrs.contains(&corr),
                "stub observed a passive poll corr {corr}; events: {events:?}"
            );
        }
    }
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

async fn read_frame_or_close_timeout<S>(stream: &mut S, wait: Duration) -> Option<Frame>
where
    S: AsyncRead + Unpin,
{
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
