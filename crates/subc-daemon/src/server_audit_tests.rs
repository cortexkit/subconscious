// Regression test for a connection-reader defect found by a read-only audit of
// reverse requests on legacy (not busy-refusal) routes.

fn audit_route_frame(ty: FrameType, channel: u16, epoch: u32, corr: u64, body: &[u8]) -> Frame {
    Frame::build(
        ty,
        Flags::new(false, Priority::Interactive, false),
        channel,
        epoch,
        corr,
        body.to_vec(),
    )
    .unwrap()
}

async fn audit_read<S: AsyncRead + Unpin>(stream: &mut S, what: &str) -> Frame {
    timeout(TEST_DEADLINE, read_frame(stream))
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
        .expect("frame decodes")
        .unwrap_or_else(|| panic!("connection closed before {what}"))
}

/// A compatibility client on a Serial route has one request in flight when
/// the provider asks it a reverse question. The client sends a second request
/// and then the answer. The answer must still reach the provider: the second
/// request waiting for the route's only credit must not park the connection
/// reader in front of it, since the first request cannot finish without it.
#[tokio::test]
async fn reverse_response_reaches_the_module_while_a_second_serial_request_waits_for_credit() {
    use subc_control::{ClientControlRequest, ClientControlResponse};
    use subc_protocol::{
        manifest::{Concurrency, ExecutionMode, IdentityScope, ModuleManifest, ProviderRole, Tool},
        session::{ModuleControlRequest, ModuleControlResponse},
        BindIdentity, ModuleHelloBody, RouteTarget,
    };

    let router = Arc::new(Router::with_default_self_handler());
    let (auth, conn) = test_auth();
    let (mut module, module_stream) = duplex(64 * 1024);
    let module_server = tokio::spawn(handle_connection(
        module_stream,
        Arc::clone(&router),
        auth.clone(),
    ));
    authenticate(&mut module, &conn).await;
    let manifest = ModuleManifest::builder("serial-reverse", "0.1.0")
        .protocol_ver(PROTOCOL_VERSION)
        .provides(vec![ProviderRole::ToolProvider {
            tools: vec![Tool {
                name: "ask".into(),
                description: None,
                execution_mode: ExecutionMode::Pure,
                schema: serde_json::json!({"type": "object"}),
            }],
            identity_scope: vec![IdentityScope::Project, IdentityScope::Session],
            concurrency: Concurrency::Serial,
            emits_push: false,
            sub_supervises: false,
        }])
        .build();
    let hello = Frame::build(
        FrameType::Hello,
        Flags::new(false, Priority::Passive, false),
        0,
        0,
        1,
        serde_json::to_vec(&ModuleHelloBody {
            manifest,
            protocol_ver: PROTOCOL_VERSION,
            control_ops: None,
            launch_nonce: None,
        })
        .unwrap(),
    )
    .unwrap();
    crate::write_frame(&mut module, &hello).await.unwrap();
    assert_eq!(
        audit_read(&mut module, "HELLO_ACK").await.header.ty,
        FrameType::HelloAck
    );

    let (mut client, client_stream) = duplex(64 * 1024);
    let client_server = tokio::spawn(handle_connection(client_stream, router, auth));
    authenticate(&mut client, &conn).await;
    // busy_refusals is not requested: this is a legacy credit-waiting route.
    let open = Frame::build(
        FrameType::Request,
        Flags::new(false, Priority::Passive, false),
        0,
        0,
        2,
        serde_json::to_vec(&ClientControlRequest::RouteOpen {
            busy_refusals: None,
            target: RouteTarget::ToolProvider {
                module_id: "serial-reverse".into(),
            },
            identity: BindIdentity::new("unit", "session"),
            consumer_identity: None,
            consumer_capabilities: None,
            role_versions: None,
            admission_facts: None,
            scope: None,
        })
        .unwrap(),
    )
    .unwrap();
    crate::write_frame(&mut client, &open).await.unwrap();
    let bind = audit_read(&mut module, "route.bind").await;
    let ModuleControlRequest::RouteBind {
        route_channel: module_channel,
        epoch: module_epoch,
        ..
    } = serde_json::from_slice::<ModuleControlRequest>(&bind.body).unwrap()
    else {
        panic!("expected route.bind");
    };
    let ack = Frame::build(
        FrameType::Response,
        Flags::new(false, Priority::Passive, false),
        0,
        0,
        bind.header.corr,
        serde_json::to_vec(&ModuleControlResponse::RouteBindAck {}).unwrap(),
    )
    .unwrap();
    crate::write_frame(&mut module, &ack).await.unwrap();
    let opened = audit_read(&mut client, "the route.open response").await;
    let ClientControlResponse::RouteOpen {
        route_channel: client_channel,
        route_epoch: client_epoch,
        ..
    } = serde_json::from_slice::<ClientControlResponse>(&opened.body).unwrap()
    else {
        panic!("expected a route.open response");
    };

    // Request 10 takes the Serial route's only credit and reaches the module.
    crate::write_frame(
        &mut client,
        &audit_route_frame(
            FrameType::Request,
            client_channel,
            client_epoch,
            10,
            b"first",
        ),
    )
    .await
    .unwrap();
    let first = audit_read(&mut module, "request 10").await;
    assert_eq!(
        (first.header.ty, first.header.corr),
        (FrameType::Request, 10)
    );

    // While running request 10 the module asks the client a reverse question.
    crate::write_frame(
        &mut module,
        &audit_route_frame(
            FrameType::Request,
            module_channel,
            module_epoch,
            77,
            b"question",
        ),
    )
    .await
    .unwrap();
    let question = audit_read(&mut client, "the reverse request").await;
    assert_eq!(
        (question.header.ty, question.header.corr),
        (FrameType::Request, 77)
    );

    // The client sends another request, then answers the question.
    crate::write_frame(
        &mut client,
        &audit_route_frame(
            FrameType::Request,
            client_channel,
            client_epoch,
            11,
            b"second",
        ),
    )
    .await
    .unwrap();
    crate::write_frame(
        &mut client,
        &audit_route_frame(
            FrameType::Response,
            client_channel,
            client_epoch,
            77,
            b"answer",
        ),
    )
    .await
    .unwrap();

    let answer = timeout(TEST_DEADLINE, read_frame(&mut module)).await;
    let answer = match answer {
        Ok(Ok(Some(frame))) => frame,
        Ok(other) => panic!("module connection ended before the answer: {other:?}"),
        Err(_) => panic!(
            "the client's answer to reverse request 77 never reached the module within \
             {TEST_DEADLINE:?}: the connection reader is parked on request 11 waiting for the \
             Serial credit request 10 holds, and request 10 is waiting for that answer"
        ),
    };
    assert_eq!(
        (answer.header.ty, answer.header.corr),
        (FrameType::Response, 77)
    );
    drop(client);
    drop(module);
    let _ = client_server.await;
    let _ = module_server.await;
}

/// The connection reader itself reserves an `activity.subscribe` before
/// handing it to a task, so a CANCEL that immediately follows it on the wire
/// ends the subscription (one STREAM_END) instead of being answered
/// `unknown_subscription` ahead of a feed that installs afterwards.
#[tokio::test]
async fn activity_cancel_right_behind_its_subscribe_on_the_wire_ends_it() {
    let scratch = cortexkit_test_support::ScratchDir::new("audit-wire-cancel");
    let control = Arc::new(
        ControlHandler::new(Arc::new(Registry::default()))
            .with_activity_ledger(scratch.path(), "audit".into()),
    );
    let router = Arc::new(Router::with_control_handler(control));
    let (auth, conn) = test_auth();
    let (mut client, client_stream) = duplex(64 * 1024);
    let server = tokio::spawn(handle_connection(client_stream, router, auth));
    authenticate(&mut client, &conn).await;
    let subscribe = Frame::build(
        FrameType::Request,
        Flags::new(false, Priority::Passive, false),
        0,
        0,
        2,
        serde_json::to_vec(&serde_json::json!({"op": "activity.subscribe"})).unwrap(),
    )
    .unwrap();
    let cancel = Frame::build(
        FrameType::Cancel,
        Flags::new(false, Priority::Passive, false),
        0,
        0,
        2,
        Vec::new(),
    )
    .unwrap();
    // One write, so the reader finds the CANCEL already buffered behind the
    // subscribe it has just handed off.
    let mut bytes = Vec::new();
    for frame in [&subscribe, &cancel] {
        bytes.extend_from_slice(&frame.header.encode());
        bytes.extend_from_slice(&frame.body);
    }
    client.write_all(&bytes).await.unwrap();

    let end = audit_read(&mut client, "the subscription's end").await;
    assert_eq!(
        (end.header.ty, end.header.corr),
        (FrameType::StreamEnd, 2),
        "{:?}",
        String::from_utf8_lossy(&end.body)
    );
    let later = timeout(Duration::from_millis(300), read_frame(&mut client)).await;
    assert!(
        later.is_err(),
        "the correlation already ended, yet the client was sent {later:?}"
    );
    drop(client);
    let _ = server.await;
}
