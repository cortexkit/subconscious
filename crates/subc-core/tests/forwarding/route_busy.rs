use super::*;
use subc_protocol::session::{ModuleControlRequest, ModuleControlResponse};

#[derive(Clone, Copy)]
struct BusyWireRoute {
    client_channel: u16,
    client_epoch: u32,
    module_channel: u16,
    module_epoch: u32,
}

async fn busy_module(server: &TestServer, module_id: &str) -> TcpStream {
    let mut module = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    register_manifest_on_stream(&mut module, tool_provider_manifest(module_id), 1).await;
    module
}

async fn busy_open(
    client: &mut TcpStream,
    module: &mut TcpStream,
    module_id: &str,
    corr: u64,
    opted_in: bool,
) -> (BusyWireRoute, Vec<u8>) {
    let request = ClientControlRequest::RouteOpen {
        target: RouteTarget::ToolProvider {
            module_id: module_id.into(),
        },
        identity: BindIdentity::new("route-busy-test", "same-session"),
        consumer_identity: None,
        consumer_capabilities: None,
        role_versions: None,
        busy_refusals: opted_in.then_some(true),
        admission_facts: None,
        scope: None,
    };
    write_frame(client, &control_request_frame(corr, request))
        .await
        .unwrap();
    client.flush().await.unwrap();
    let bind_frame = read_frame_timeout(module).await;
    let ModuleControlRequest::RouteBind {
        route_channel: module_channel,
        epoch: module_epoch,
        ..
    } = serde_json::from_slice(&bind_frame.body).unwrap()
    else {
        panic!("expected route.bind");
    };
    let ack = Frame::build_with_version(
        bind_frame.header.ver,
        FrameType::Response,
        Flags::new(false, Priority::Passive, false),
        0,
        0,
        bind_frame.header.corr,
        serde_json::to_vec(&ModuleControlResponse::RouteBindAck {}).unwrap(),
    )
    .unwrap();
    write_frame(module, &ack).await.unwrap();
    module.flush().await.unwrap();
    let reply = read_frame_timeout(client).await;
    assert_eq!(reply.header.corr, corr);
    let ClientControlResponse::RouteOpen {
        route_channel: client_channel,
        route_epoch: client_epoch,
        request_window,
        watch_window,
    } = serde_json::from_slice(&reply.body).unwrap()
    else {
        panic!("expected route.open acceptance");
    };
    assert_eq!(request_window, opted_in.then_some(32));
    assert_eq!(watch_window, opted_in.then_some(24));
    (
        BusyWireRoute {
            client_channel,
            client_epoch,
            module_channel,
            module_epoch,
        },
        bind_frame.body,
    )
}

async fn busy_send(client: &mut TcpStream, route: BusyWireRoute, corr: u64, watch: bool) {
    let frame = if watch {
        subscription_request(
            route.client_channel,
            route.client_epoch,
            corr,
            b"held-watch",
        )
    } else {
        data_request(
            route.client_channel,
            route.client_epoch,
            corr,
            b"held-request",
        )
    };
    write_frame(client, &frame).await.unwrap();
    client.flush().await.unwrap();
}

async fn busy_terminal(module: &mut TcpStream, route: BusyWireRoute, corr: u64, ty: FrameType) {
    let terminal = Frame::build(
        ty,
        Flags::new(false, Priority::Passive, false),
        route.module_channel,
        route.module_epoch,
        corr,
        b"done".to_vec(),
    )
    .unwrap();
    write_frame(module, &terminal).await.unwrap();
    module.flush().await.unwrap();
}

async fn busy_expect_refusal(
    client: &mut TcpStream,
    route: BusyWireRoute,
    corr: u64,
    requests: u64,
    watches: u64,
) {
    let frame = read_frame_timeout_for(client, Duration::from_secs(1)).await;
    let error = assert_error(&frame, route.client_channel, corr, error_codes::ROUTE_BUSY);
    assert_eq!(frame.header.epoch, route.client_epoch);
    assert!(frame.header.flags.is_daemon_origin());
    assert_eq!(
        error.detail,
        Some(serde_json::json!({
            "reason": "no_credit", "request_in_flight": requests, "watch_in_flight": watches,
            "request_window": 32, "watch_window": 24,
        }))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn route_busy_watch_incident_leaves_same_route_and_other_module_live() {
    let server = TestServer::start().await;
    let mut module = busy_module(&server, "busy-held").await;
    let mut neighbor = busy_module(&server, "busy-neighbor").await;
    let mut client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    let (route, _) = busy_open(&mut client, &mut module, "busy-held", 10, true).await;
    let (other, _) = busy_open(&mut client, &mut neighbor, "busy-neighbor", 11, true).await;
    for corr in 101..=124 {
        busy_send(&mut client, route, corr, true).await;
        let held = read_frame_timeout(&mut module).await;
        assert_eq!(held.header.corr, corr);
        assert!(held.header.flags.is_subscription());
    }
    busy_send(&mut client, route, 125, true).await;
    busy_send(&mut client, route, 126, false).await;
    busy_send(&mut client, other, 127, false).await;
    busy_expect_refusal(&mut client, route, 125, 24, 24).await;
    assert_eq!(read_frame_timeout(&mut module).await.header.corr, 126);
    assert_eq!(read_frame_timeout(&mut neighbor).await.header.corr, 127);
    busy_terminal(&mut neighbor, other, 127, FrameType::Response).await;
    assert_response(
        &read_frame_timeout(&mut client).await,
        other.client_channel,
        127,
        b"done",
    );

    // None of the 24 watches or the ordinary request has ended. Seven more
    // ordinary requests fill, but never enlarge, the unchanged total window.
    for corr in 201..=207 {
        busy_send(&mut client, route, corr, false).await;
        assert_eq!(read_frame_timeout(&mut module).await.header.corr, corr);
    }
    busy_send(&mut client, route, 208, false).await;
    busy_expect_refusal(&mut client, route, 208, 32, 24).await;
    assert_no_frame_within(&mut module, Duration::from_millis(100)).await;
    busy_terminal(&mut module, route, 101, FrameType::StreamEnd).await;
    assert_eq!(read_frame_timeout(&mut client).await.header.corr, 101);
    busy_send(&mut client, route, 209, true).await;
    assert_eq!(read_frame_timeout(&mut module).await.header.corr, 209);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn route_busy_full_ordinary_window_refuses_and_reads_the_next_frame() {
    let server = TestServer::start().await;
    let mut module = busy_module(&server, "busy-full").await;
    let mut client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    let (route, _) = busy_open(&mut client, &mut module, "busy-full", 10, true).await;
    for corr in 101..=132 {
        busy_send(&mut client, route, corr, false).await;
        assert_eq!(read_frame_timeout(&mut module).await.header.corr, corr);
    }
    busy_send(&mut client, route, 133, false).await;
    write_frame(
        &mut client,
        &control_request_frame(999, ClientControlRequest::ServerDescribe {}),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();
    busy_expect_refusal(&mut client, route, 133, 32, 0).await;
    let describe = read_frame_timeout_for(&mut client, Duration::from_secs(1)).await;
    assert_eq!(describe.header.corr, 999);
    let body: Value = serde_json::from_slice(&describe.body).unwrap();
    assert_eq!(
        body["counters"]["route_busy_by_module_principal"]["busy-full"]["direct"],
        1
    );
    assert_no_frame_within(&mut module, Duration::from_millis(100)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn route_busy_unopted_connection_keeps_waiting_and_reports_its_credit_wait() {
    let server = TestServer::start().await;
    let mut module = busy_module(&server, "busy-legacy").await;
    let mut client = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    let (route, _) = busy_open(&mut client, &mut module, "busy-legacy", 10, false).await;
    for corr in 101..=132 {
        busy_send(&mut client, route, corr, true).await;
        assert_eq!(read_frame_timeout(&mut module).await.header.corr, corr);
    }
    busy_send(&mut client, route, 133, false).await;
    write_frame(
        &mut client,
        &control_request_frame(999, ClientControlRequest::ServerDescribe {}),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();
    assert_no_frame_within(&mut client, Duration::from_millis(100)).await;
    let mut inspector = connect_authed_client(&server.connection_file_path)
        .await
        .unwrap();
    let deadline = Instant::now() + SETUP_TIMEOUT;
    loop {
        let counters = server_counter_key_value(
            &mut inspector,
            1000,
            "reader_wait_over_5s_by_module_principal",
        )
        .await;
        if counters
            .as_ref()
            .is_some_and(|value| value["busy-legacy"]["direct"]["legacy_credit"] == 1)
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "legacy wait did not become observable: {counters:?}"
        );
        sleep(Duration::from_millis(10)).await;
    }
    assert_no_frame_within(&mut client, Duration::from_millis(100)).await;
    assert_no_frame_within(&mut module, Duration::from_millis(100)).await;
    busy_terminal(&mut module, route, 101, FrameType::StreamEnd).await;
    assert_eq!(read_frame_timeout(&mut module).await.header.corr, 133);
    let first = read_frame_timeout(&mut client).await;
    let second = read_frame_timeout(&mut client).await;
    let mut corrs = [first.header.corr, second.header.corr];
    corrs.sort();
    assert_eq!(
        corrs,
        [101, 999],
        "release unblocks both the queued request and the reader"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn route_busy_opt_in_does_not_change_module_bind_bytes() {
    let mut binds = Vec::new();
    for opted_in in [false, true] {
        // Fresh endpoints allocate the same first handles. Comparing their
        // independently built binds does not hide handle changes in a rewrite.
        let server = TestServer::start().await;
        let mut module = busy_module(&server, "busy-bind").await;
        let mut client = connect_authed_client(&server.connection_file_path)
            .await
            .unwrap();
        let (_, bytes) = busy_open(&mut client, &mut module, "busy-bind", 10, opted_in).await;
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(body.get("busy_refusals").is_none());
        assert!(body.get("request_window").is_none());
        assert!(body.get("watch_window").is_none());
        binds.push(bytes);
    }
    assert_eq!(binds[0], binds[1]);
}
