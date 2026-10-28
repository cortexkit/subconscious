use super::*;

fn hello_operations(field: &str, operations: Option<Value>) -> Frame {
    let hello = hello_with_flow_operations(json!(["flow-scopes/v1"]), None);
    let mut body: Value = serde_json::from_slice(&hello.body).unwrap();
    if let Some(operations) = operations {
        body["manifest"][field] = operations;
    }
    Frame::build(
        FrameType::Hello,
        control_flags(),
        0,
        0,
        401,
        serde_json::to_vec(&body).unwrap(),
    )
    .unwrap()
}

#[test]
fn invalid_operation_lists_register_with_one_warning_and_no_catalog_field() {
    for field in ["flow_operations", "confirm_operations"] {
        for operations in [
            json!(["missing"]),
            json!(["reset", "reset"]),
            json!(vec!["reset"; 257]),
            json!([true]),
            json!("reset"),
        ] {
            let handler = ControlHandler::default();
            let capture = EventCapture::default();
            let _subscriber = tracing::subscriber::set_default(
                tracing_subscriber::registry().with(capture.clone()),
            );
            let reply = handler
                .handle_control(
                    ConnectionId::new(401),
                    hello_operations(field, Some(operations)),
                )
                .unwrap();
            assert_eq!(reply[0].header.ty, FrameType::HelloAck);
            let (raw, _) = role_declarations_catalog(&handler);
            assert_eq!(raw["module_id"], "flow-ops");
            assert!(raw.get(field).is_none(), "{field}: {raw}");
            let warnings: Vec<_> = capture
                .events()
                .into_iter()
                .filter(|event| event.level == tracing::Level::WARN)
                .collect();
            assert_eq!(warnings.len(), 1, "{field}: {warnings:?}");
            assert_eq!(warnings[0].fields["module_id"], "\"flow-ops\"");
            assert_eq!(
                warnings[0].fields["message"],
                format!("dropping {field} from HELLO")
            );
        }
    }
}

#[test]
fn confirm_operations_catalog_keeps_order_empty_and_absent_without_a_capability() {
    for operations in [
        Some(json!(["reset", "diagnostics.findings"])),
        Some(json!([])),
        None,
    ] {
        let handler = ControlHandler::default();
        let hello = hello_operations("confirm_operations", operations.clone());
        let mut body: Value = serde_json::from_slice(&hello.body).unwrap();
        body["manifest"]
            .as_object_mut()
            .unwrap()
            .remove("capabilities");
        let hello = Frame::build(
            FrameType::Hello,
            control_flags(),
            0,
            0,
            401,
            serde_json::to_vec(&body).unwrap(),
        )
        .unwrap();
        let reply = handler
            .handle_control(ConnectionId::new(401), hello)
            .unwrap();
        assert_eq!(reply[0].header.ty, FrameType::HelloAck);
        let (raw, entry) = role_declarations_catalog(&handler);
        assert_eq!(raw.get("confirm_operations"), operations.as_ref());
        assert_eq!(
            entry.confirm_operations,
            operations.map(|value| serde_json::from_value::<Vec<String>>(value).unwrap())
        );
    }
}

async fn bind_route(
    handler: &ControlHandler,
    target: &RouteCtx,
    target_rx: &mut mpsc::Receiver<crate::router::OutboundFrame>,
    client: &RouteCtx,
    identity: BindIdentity,
    consumer_identity: Option<subc_control::ConsumerIdentity>,
) -> (Frame, Arc<crate::forwarding::RouteBinding>) {
    let request = ClientControlRequest::RouteOpen {
        target: RouteTarget::ToolProvider {
            module_id: "target".into(),
        },
        identity,
        consumer_identity,
        consumer_capabilities: None,
        role_versions: None,
        admission_facts: None,
        scope: None,
    };
    let frame = Frame::build(
        FrameType::Request,
        control_flags(),
        0,
        0,
        30,
        serde_json::to_vec(&request).unwrap(),
    )
    .unwrap();
    let (opened, bind) = tokio::join!(handler.handle_control_frame(client, frame), async {
        let bind = tokio::time::timeout(Duration::from_secs(2), target_rx.recv())
            .await
            .expect("route.bind arrives")
            .unwrap()
            .frame;
        handler
            .handle_control_frame(target, route_bind_ack(bind.header.corr))
            .await
            .unwrap();
        bind
    });
    assert!(opened.unwrap().is_empty());
    let (channel, epoch) = route_bind_channel(&bind);
    let crate::forwarding::DataRoute::Module(crate::forwarding::DataRouteState::Bound(binding)) =
        handler
            .forwarding
            .lookup_data_route(target.connection_id, channel, epoch)
            .unwrap()
    else {
        panic!("accepted bind must be committed")
    };
    (bind, binding)
}

#[tokio::test]
async fn observed_peer_is_opt_in_tcp_is_unobserved_and_legacy_bytes_are_unchanged() {
    use subc_os::local_peer::{ObservedPeer as OsPeer, PeerObservation, PeerUser};
    for capabilities in [
        json!([]),
        json!(["operator-app/v1"]),
        json!(["observed-peer/v1"]),
        json!(["operator-app/v1", "observed-peer/v1"]),
    ] {
        for (peer, expected) in [
            (None, json!({"status":"unobserved"})),
            (Some(OsPeer::Unobserved), json!({"status":"unobserved"})),
            (
                Some(OsPeer::Observed(PeerObservation {
                    user: PeerUser::Uid(123),
                    pid: 42,
                    exe: Some("/example/app".into()),
                    signing_id: Some("example.app".into()),
                })),
                json!({"status":"observed", "uid":123, "exe":"/example/app", "signing_id":"example.app"}),
            ),
            (
                Some(OsPeer::Observed(PeerObservation {
                    user: PeerUser::Sid("S-1-5-21".into()),
                    pid: 42,
                    exe: None,
                    signing_id: None,
                })),
                json!({"status":"observed", "sid":"S-1-5-21"}),
            ),
        ] {
            let handler = ControlHandler::default();
            let (target, mut target_rx) = route_ctx(ConnectionId::new(1));
            let mut hello: Value =
                serde_json::from_slice(&hello_frame("target", PROTOCOL_VERSION, 1).body).unwrap();
            hello["manifest"]["capabilities"] = json!({"provides": capabilities});
            // An empty advisory declaration must not prevent route admission.
            hello["manifest"]["confirm_operations"] = json!([]);
            let hello = Frame::build(
                FrameType::Hello,
                control_flags(),
                0,
                0,
                1,
                serde_json::to_vec(&hello).unwrap(),
            )
            .unwrap();
            hello_via_sink(&handler, &target, &mut target_rx, hello).await;
            let (client, _client_rx) = route_ctx(ConnectionId::new(2));
            if let Some(peer) = peer {
                handler.record_observed_peer(client.connection_id, peer);
            }
            let root = unique_project_root("peer-gate");
            let identity = BindIdentity::new(root.path().to_path_buf(), "unit", "s");
            let (bind, binding) =
                bind_route(&handler, &target, &mut target_rx, &client, identity, None).await;
            let raw: Value = serde_json::from_slice(&bind.body).unwrap();
            assert_eq!(binding.principal, Principal::Direct);
            assert!(
                raw.get("operator_app").is_none(),
                "an observation is not an app stamp"
            );
            if capabilities
                .as_array()
                .unwrap()
                .iter()
                .any(|cap| cap == "observed-peer/v1")
            {
                assert_eq!(raw["observed_peer"], expected);
            } else {
                assert!(raw.get("observed_peer").is_none());
                let legacy: ModuleControlRequest = serde_json::from_slice(&bind.body).unwrap();
                assert_eq!(bind.body, serde_json::to_vec(&legacy).unwrap());
            }
            handler.cleanup_connection(client.connection_id).unwrap();
            assert!(!handler
                .observed_peers
                .lock()
                .unwrap()
                .contains_key(&client.connection_id));
        }
    }
}

#[tokio::test]
async fn operator_app_bind_is_gated_independently_and_the_stamp_survives_route_commit() {
    for observed in [false, true] {
        for operator in [false, true] {
            for enabled in [false, true] {
                for stamped in [false, true] {
                    let handler = ControlHandler::default().with_test_operator_app_policy(
                        crate::operator_apps::TestOperatorAppPolicy {
                            macos_enabled: enabled,
                            app: "fixture",
                            requirement: "always",
                            floor: semver::Version::new(1, 2, 10),
                        },
                    );
                    let (target, mut rx) = route_ctx(ConnectionId::new(1));
                    let mut hello: Value =
                        serde_json::from_slice(&hello_frame("target", PROTOCOL_VERSION, 1).body)
                            .unwrap();
                    let mut capabilities = Vec::new();
                    if observed {
                        capabilities.push("observed-peer/v1");
                    }
                    if operator {
                        capabilities.push("operator-app/v1");
                    }
                    hello["manifest"]["capabilities"] = json!({"provides": capabilities});
                    hello_via_sink(
                        &handler,
                        &target,
                        &mut rx,
                        Frame::build(
                            FrameType::Hello,
                            control_flags(),
                            0,
                            0,
                            1,
                            serde_json::to_vec(&hello).unwrap(),
                        )
                        .unwrap(),
                    )
                    .await;
                    let (client, _client_rx) = route_ctx(ConnectionId::new(2));
                    if stamped {
                        handler.operator_apps.lock().unwrap().insert(
                            client.connection_id,
                            crate::operator_apps::OperatorAppStamp::fixture("fixture", 42),
                        );
                    }
                    let root = unique_project_root("operator-app-bind");
                    let identity = BindIdentity::new(root.path().to_path_buf(), "unit", "s");
                    let (bind, binding) =
                        bind_route(&handler, &target, &mut rx, &client, identity, None).await;
                    let raw: Value = serde_json::from_slice(&bind.body).unwrap();
                    assert_eq!(
                        raw.get("operator_app"),
                        (operator && enabled && stamped)
                            .then(|| json!({"app":"fixture", "pid":42}))
                            .as_ref()
                    );
                    assert_eq!(
                        raw.get("observed_peer"),
                        observed.then(|| json!({"status":"unobserved"})).as_ref()
                    );
                    assert!(raw.get("build_class").is_none());
                    assert_eq!(binding.principal, Principal::Direct);
                    assert_eq!(binding.operator_app.is_some(), stamped);
                    if !observed && !(operator && enabled && stamped) {
                        let legacy: ModuleControlRequest =
                            serde_json::from_slice(&bind.body).unwrap();
                        assert_eq!(bind.body, serde_json::to_vec(&legacy).unwrap());
                    }
                    handler.cleanup_connection(client.connection_id).unwrap();
                    assert!(!handler
                        .operator_apps
                        .lock()
                        .unwrap()
                        .contains_key(&client.connection_id));
                }
            }
        }
    }
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn operator_app_native_connection_is_stamped_again_by_a_restarted_daemon() {
    use subc_os::local_peer::fixture::SignedPeerFixture;
    let root = cortexkit_test_support::ScratchDir::new("operator-restart");
    let mut fixture = SignedPeerFixture::new(
        &root,
        "io.cortexkit.alfonso.fixture",
        Some("1.2.10"),
        true,
        None,
    );
    let original_pid = fixture.pid();
    for _restart in 0..2 {
        let handler = ControlHandler::default().with_test_operator_app_policy(
            crate::operator_apps::TestOperatorAppPolicy {
                macos_enabled: true,
                app: "fixture",
                requirement: "identifier \"io.cortexkit.alfonso.fixture\"",
                floor: semver::Version::new(1, 2, 10),
            },
        );
        let (target, mut rx) = route_ctx(ConnectionId::new(1));
        let mut hello: Value =
            serde_json::from_slice(&hello_frame("target", PROTOCOL_VERSION, 1).body).unwrap();
        hello["manifest"]["capabilities"] = json!({"provides":["operator-app/v1"]});
        hello_via_sink(
            &handler,
            &target,
            &mut rx,
            Frame::build(
                FrameType::Hello,
                control_flags(),
                0,
                0,
                1,
                serde_json::to_vec(&hello).unwrap(),
            )
            .unwrap(),
        )
        .await;
        let (client, _rx) = route_ctx(ConnectionId::new(2));
        let (stream, peer) = fixture.connect();
        handler.record_captured_peer(client.connection_id, peer);
        let identity = BindIdentity::new(root.to_path_buf(), "unit", "s");
        let (bind, binding) = bind_route(&handler, &target, &mut rx, &client, identity, None).await;
        assert_eq!(binding.principal, Principal::Direct);
        assert!(binding.operator_app.as_ref().unwrap().revalidate());
        let raw: Value = serde_json::from_slice(&bind.body).unwrap();
        assert_eq!(
            raw["operator_app"],
            json!({"app":"fixture", "pid":original_pid})
        );
        handler.cleanup_connection(client.connection_id).unwrap();
        drop(stream);
        drop(handler);
    }
    assert_eq!(fixture.pid(), original_pid);
}

#[tokio::test]
async fn build_class_is_callosum_production_only_captured_not_relayed_and_warned_once() {
    for opener in [None, Some("other"), Some("callosum")] {
        let supervisor = SupervisorHandle::new();
        if let Some(module_id) = opener {
            supervisor.set_spawn_nonce(module_id, "nonce".into());
        }
        let handler = ControlHandler::default().with_supervisor(supervisor);
        let (target, mut target_rx) = route_ctx(ConnectionId::new(1));
        hello_via_sink(
            &handler,
            &target,
            &mut target_rx,
            hello_frame("target", PROTOCOL_VERSION, 1),
        )
        .await;
        let (client, mut client_rx) = route_ctx(ConnectionId::new(2));
        let root = unique_project_root("build-capture");
        let harness = "fed-phone:0123456789abcdef0123456789abcdef";
        let capture = EventCapture::default();
        let _subscriber =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(capture.clone()));
        let mut legacy = None;
        for build_class in [
            None,
            Some("production"),
            Some("debug"),
            Some("Production"),
            Some("production"),
        ] {
            let mut identity = BindIdentity::new(root.path().to_path_buf(), harness, "s");
            identity.build_class = build_class.map(str::to_owned);
            let consumer = opener.map(|module_id| subc_control::ConsumerIdentity {
                module_id: module_id.into(),
                launch_nonce: "nonce".into(),
            });
            let (bind, binding) = bind_route(
                &handler,
                &target,
                &mut target_rx,
                &client,
                identity,
                consumer,
            )
            .await;
            client_rx.recv().await.unwrap();
            assert_eq!(binding.harness, harness);
            assert_eq!(
                binding.build_class.as_deref(),
                (opener == Some("callosum") && build_class == Some("production"))
                    .then_some("production")
            );
            assert_eq!(
                binding.principal,
                opener
                    .map(|module_id| Principal::Reserved {
                        module_id: module_id.into()
                    })
                    .unwrap_or(Principal::Direct)
            );
            assert!(!String::from_utf8_lossy(&bind.body).contains("build_class"));
            let mut normalized: Value = serde_json::from_slice(&bind.body).unwrap();
            normalized.as_object_mut().unwrap().remove("route_channel");
            normalized.as_object_mut().unwrap().remove("epoch");
            let bytes = serde_json::to_vec(&normalized).unwrap();
            if let Some(legacy) = &legacy {
                assert_eq!(&bytes, legacy);
            } else {
                legacy = Some(bytes);
            }
        }
        let warnings: Vec<_> = capture
            .events()
            .into_iter()
            .filter(|event| {
                event.fields.get("message").map(String::as_str)
                    == Some("ignoring build_class from a non-Callosum opener")
            })
            .collect();
        assert_eq!(warnings.len(), usize::from(opener != Some("callosum")));
        for warning in warnings {
            assert_eq!(warning.level, tracing::Level::WARN);
            assert_eq!(warning.fields["connection_id"], "2");
        }
        handler.cleanup_connection(client.connection_id).unwrap();
        assert!(!handler
            .build_class_warned
            .lock()
            .unwrap()
            .contains(&client.connection_id));
    }
}

#[cfg(unix)]
#[tokio::test]
async fn accepted_local_and_tcp_connections_relay_their_own_peer_snapshot() {
    use crate::{
        local_listener,
        server::{serve_listener, serve_local_listener, ServerAuth},
    };
    use subc_transport::{
        authenticate_client, generate_daemon_id, generate_key, ConnectionInfo, Endpoint,
        SCHEMA_VERSION,
    };

    let handler = Arc::new(ControlHandler::default());
    let (target, mut target_rx) = route_ctx(ConnectionId::new(50_000));
    let mut hello: Value =
        serde_json::from_slice(&hello_frame("target", PROTOCOL_VERSION, 1).body).unwrap();
    hello["manifest"]["capabilities"] = json!({"provides":["observed-peer/v1"]});
    let hello = Frame::build(
        FrameType::Hello,
        control_flags(),
        0,
        0,
        1,
        serde_json::to_vec(&hello).unwrap(),
    )
    .unwrap();
    hello_via_sink(&handler, &target, &mut target_rx, hello).await;
    let router = Arc::new(crate::Router::with_control_handler(Arc::clone(&handler)));
    let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let info = ConnectionInfo {
        schema: SCHEMA_VERSION,
        wire_version: Some(PROTOCOL_VERSION),
        endpoints: vec![Endpoint {
            host: "127.0.0.1".into(),
            port: tcp.local_addr().unwrap().port(),
        }],
        key: generate_key().unwrap(),
        daemon_id: generate_daemon_id().unwrap(),
        pid: std::process::id(),
        daemon_ver: "test".into(),
    };
    let socket_dir = std::env::temp_dir().join(format!("subc-peer-{}", std::process::id()));
    let config = local_listener::LocalListenerConfig::in_socket_dir(&socket_dir);
    let bound = local_listener::bind(&config, &socket_dir.join("connection.json")).unwrap();
    let subc_transport::LocalEndpoint::Unix { path } = bound.endpoint() else {
        panic!("expected Unix endpoint")
    };
    let auth = ServerAuth::new(info.key.clone(), info.daemon_id, info.daemon_ver.clone());
    let tcp_server = tokio::spawn(serve_listener(tcp, Arc::clone(&router), auth.clone()));
    let local_server = tokio::spawn(serve_local_listener(bound, router, auth));
    let mut local = tokio::net::UnixStream::connect(path).await.unwrap();
    let mut tcp = tokio::net::TcpStream::connect(("127.0.0.1", info.endpoints[0].port))
        .await
        .unwrap();
    authenticate_client(&mut local, &info, Duration::from_secs(5))
        .await
        .unwrap();
    authenticate_client(&mut tcp, &info, Duration::from_secs(5))
        .await
        .unwrap();
    for is_local in [true, false] {
        let open = route_open_frame(30, "target", unique_project_root("accepted-peer"));
        if is_local {
            crate::write_frame(&mut local, &open).await.unwrap();
        } else {
            crate::write_frame(&mut tcp, &open).await.unwrap();
        }
        let bind = tokio::time::timeout(Duration::from_secs(2), target_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let raw: Value = serde_json::from_slice(&bind.body).unwrap();
        let peer = &raw["observed_peer"];
        if is_local && peer["status"] == "observed" {
            assert_eq!(peer["uid"], rustix::process::geteuid().as_raw());
            assert!(peer.get("sid").is_none());
        } else {
            assert_eq!(peer, &json!({"status":"unobserved"}));
            #[cfg(target_os = "macos")]
            assert!(!is_local, "macOS local peers have audit tokens");
        }
        assert_eq!(raw["principal"], json!({"kind":"direct"}));
        assert!(raw.get("operator_app").is_none());
        handler
            .handle_control_frame(&target, route_bind_ack(bind.header.corr))
            .await
            .unwrap();
        let reply = if is_local {
            crate::read_frame(&mut local).await.unwrap().unwrap()
        } else {
            crate::read_frame(&mut tcp).await.unwrap().unwrap()
        };
        assert_eq!(reply.header.ty, FrameType::Response);
    }
    tcp_server.abort();
    local_server.abort();
    let _ = tcp_server.await;
    let _ = local_server.await;
    std::fs::remove_dir(&socket_dir).unwrap();
}
