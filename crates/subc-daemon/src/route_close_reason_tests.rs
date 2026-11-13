use super::*;
use crate::scopes::{ScopeDrain, ScopeTagChange};
use subc_control::RouteOpenRequestExt;

#[tokio::test]
async fn route_close_reasons_capability_is_advertised() {
    let handler = ControlHandler::new(Arc::new(Registry::default()));
    let (ctx, _rx) = route_ctx(ConnectionId::new(901));
    let request = Frame::build(
        FrameType::Request,
        control_flags(),
        0,
        0,
        1,
        serde_json::to_vec(&ClientControlRequest::ServerDescribe {}).unwrap(),
    )
    .unwrap();
    let replies = handler.handle_control_frame(&ctx, request).await.unwrap();
    let ClientControlResponse::ServerDescribe { capabilities, .. } =
        serde_json::from_slice(&replies[0].body).unwrap()
    else {
        panic!("expected server.describe");
    };
    assert!(capabilities
        .iter()
        .any(|capability| capability == subc_protocol::ROUTE_CLOSE_REASONS_CAPABILITY));
}

async fn open_with_close_reasons(
    rig: &mut Rig,
    client: &RouteCtx,
    client_rx: &mut mpsc::Receiver<crate::router::OutboundFrame>,
    accepts: Option<Vec<String>>,
) -> ((u16, u32), (u16, u32)) {
    let selector = rig.selector("s", Some(1));
    let (_, _unused_rx, mut frame) = rig.open_frame(Some(AFT), PLEXUS, Some(selector));
    let mut body = serde_json::from_slice(&frame.body).unwrap();
    RouteOpenRequestExt::new()
        .with_accepts_close_reasons(accepts)
        .write_to(&mut body)
        .unwrap();
    frame.body = serde_json::to_vec(&body).unwrap();
    let handler = rig.handler.clone();
    let client = client.clone();
    let task = tokio::spawn(async move { handler.handle_control_frame(&client, frame).await });
    let (module, module_rx) = rig.modules.get_mut(PLEXUS).unwrap();
    let bind = tokio::time::timeout(Duration::from_secs(2), module_rx.recv())
        .await
        .expect("route.open reaches the target")
        .unwrap();
    let module_route = route_bind_channel(&bind.frame);
    rig.handler
        .handle_control_frame(module, route_bind_ack(bind.header.corr))
        .await
        .unwrap();
    assert!(task.await.unwrap().unwrap().is_empty());
    let opened = client_rx.recv().await.unwrap();
    (published_route(&opened.frame), module_route)
}

fn expect_close_pushes(
    rx: &mut mpsc::Receiver<crate::router::OutboundFrame>,
    closing: bool,
    expected: &[(RouteCloseReason, &[u16])],
) {
    let mut observed = Vec::new();
    for _ in expected {
        let frame = rx.try_recv().expect("route close push is delivered");
        assert_eq!(frame.header.ty, FrameType::Push);
        let push: ClientControlPush = serde_json::from_slice(&frame.body).unwrap();
        let (reason, channels) = match push {
            ClientControlPush::RouteClosing {
                reason, channels, ..
            } if closing => (reason, channels),
            ClientControlPush::RouteClosed {
                reason,
                channels,
                terminal: Some(false),
                ..
            } if !closing => (reason, channels),
            other => panic!("unexpected close push: {other:?}"),
        };
        observed.push((reason, channels));
    }
    observed.sort_unstable_by(|left, right| left.1.cmp(&right.1));
    let mut expected = expected
        .iter()
        .map(|(reason, channels)| (*reason, channels.to_vec()))
        .collect::<Vec<_>>();
    expected.sort_unstable_by(|left, right| left.1.cmp(&right.1));
    assert_eq!(observed, expected);
}

fn expect_goodbyes(
    rx: &mut mpsc::Receiver<crate::router::OutboundFrame>,
    expected_routes: &[(u16, u32)],
) {
    let mut observed = Vec::new();
    while let Ok(frame) = rx.try_recv() {
        assert_eq!(frame.header.ty, FrameType::Goodbye);
        assert!(
            frame.body.is_empty(),
            "route GOODBYE carries no close reason"
        );
        observed.push((frame.header.channel, frame.header.epoch));
    }
    observed.sort_unstable();
    let mut expected = expected_routes.to_vec();
    expected.sort_unstable();
    assert_eq!(observed, expected);
}

#[tokio::test]
async fn route_close_reasons_follow_each_open_and_target_gets_only_goodbyes() {
    let mut rig = rig().await;
    rig.sync(vec![session(1)]).await;
    let (client, mut client_rx) = wide_ctx(901);
    let (legacy, mut legacy_rx) = wide_ctx(902);
    let declarations = [
        Some(vec!["authority_changed".into(), "future_reason".into()]),
        None,
        Some(Vec::new()),
        Some(vec!["future_reason".into()]),
    ];
    let mut client_routes = Vec::new();
    let mut module_routes = Vec::new();
    for accepts in declarations {
        let (route, module_route) =
            open_with_close_reasons(&mut rig, &client, &mut client_rx, accepts.clone()).await;
        let DataRoute::Client(DataRouteState::Bound(binding)) = rig
            .forwarding
            .lookup_data_route(client.connection_id, route.0, route.1)
            .unwrap()
        else {
            panic!("the acknowledged route is stored");
        };
        assert_eq!(binding.accepts_close_reasons, accepts);
        client_routes.push(route);
        module_routes.push(module_route);
    }
    let (legacy_route, module_route) =
        open_with_close_reasons(&mut rig, &legacy, &mut legacy_rx, None).await;
    module_routes.push(module_route);
    let (target, _) = &rig.modules[PLEXUS];
    let endpoint = rig
        .forwarding
        .module_endpoint_for_connection(target.connection_id)
        .unwrap()
        .unwrap();
    let routes = rig.forwarding.endpoint_routes(endpoint).unwrap();
    send_route_control_pushes(
        &rig.forwarding,
        routes,
        ClientControlPush::RouteClosing {
            module_id: PLEXUS.into(),
            channels: Vec::new(),
            reason: RouteCloseReason::AuthorityChanged,
        },
    );

    // The same connection can negotiate differently on each route. Grouping
    // solely by connection would either leak the new reason or hide it.
    let declared = [client_routes[0].0];
    let undeclared = client_routes[1..]
        .iter()
        .map(|route| route.0)
        .collect::<Vec<_>>();
    expect_close_pushes(
        &mut client_rx,
        true,
        &[
            (RouteCloseReason::AuthorityChanged, &declared),
            (RouteCloseReason::Reload, &undeclared),
        ],
    );
    expect_close_pushes(
        &mut legacy_rx,
        true,
        &[(RouteCloseReason::Reload, &[legacy_route.0])],
    );
    assert!(client_rx.try_recv().is_err());
    assert!(legacy_rx.try_recv().is_err());
    assert!(rig.modules.get_mut(PLEXUS).unwrap().1.try_recv().is_err());

    // Inject only the close cause: selection, route removal, recipient capture,
    // pushes and both GOODBYEs use the existing live-route close path.
    let before = rig.forwarding.published_scope_tag(OWNER, "s").unwrap();
    let drained = rig
        .forwarding
        .publish_scope_changes(&[ScopeTagChange {
            owner: OWNER.into(),
            scope_ref: "s".into(),
            before: Some(before),
            after: None,
            drain: ScopeDrain::All(RouteCloseReason::AuthorityChanged),
        }])
        .unwrap();
    assert_eq!(drained.len(), 5);
    rig.handler.close_scope_drained_routes(drained);
    assert_eq!(rig.forwarding.active_binding_count().unwrap(), 0);
    expect_close_pushes(
        &mut client_rx,
        false,
        &[
            (RouteCloseReason::AuthorityChanged, &declared),
            (RouteCloseReason::Reload, &undeclared),
        ],
    );
    expect_close_pushes(
        &mut legacy_rx,
        false,
        &[(RouteCloseReason::Reload, &[legacy_route.0])],
    );
    expect_goodbyes(&mut client_rx, &client_routes);
    expect_goodbyes(&mut legacy_rx, &[legacy_route]);
    expect_goodbyes(&mut rig.modules.get_mut(PLEXUS).unwrap().1, &module_routes);
}
