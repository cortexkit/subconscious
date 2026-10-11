use super::*;
use subc_protocol::scope::ScopeAttributes;

fn run_scope(delegates: bool) -> ScopeRecord {
    ScopeRecord::new("s", 1, ScopeKind::Ephemeral)
        .with_carriers(vec![carrier(AFT, None)])
        .with_attributes(
            ScopeAttributes::new()
                .with_agent_id(Some("agent-1".into()))
                .with_run_id(Some("run:7".into()))
                .with_delegates(delegates),
        )
        .with_expires_at_ms(Some(2_000))
}

fn flow_scope() -> ScopeRecord {
    ScopeRecord::new("s", 1, ScopeKind::Ephemeral)
        .with_carriers(vec![carrier(AFT, None)])
        .with_attributes(ScopeAttributes::new().with_flow_id(Some("flow:7".into())))
        .with_expires_at_ms(Some(2_000))
}

async fn call_mode_rig(provides: &[&str]) -> Rig {
    let mut rig = rig_with_target_capabilities(provides).await;
    rig.handler = rig.handler.clone().with_wall_clock(|| 1_000);
    rig
}

fn bind_snapshot(bind: &Frame) -> Value {
    let mut body: Value = serde_json::from_slice(&bind.body).unwrap();
    // Route allocation and temporary directory names vary between opens; the
    // remaining wire fields must match the fixture, including omitted fields.
    body["route_channel"] = serde_json::json!(1);
    body["epoch"] = serde_json::json!(1);
    body["identity"]["project_root"] = serde_json::json!("/project");
    body
}

fn assert_golden(bind: &Frame, golden: &str) {
    let expected: Value = serde_json::from_str(golden).unwrap();
    assert_eq!(bind_snapshot(bind), expected);
}

#[tokio::test]
async fn call_modes_declarer_stamps_classic_codemode_flow_and_deadlines() {
    for (record, mode) in [
        (None, "classic"),
        (Some(session(1).with_expires_at_ms(Some(2_000))), "classic"),
        (Some(run_scope(false)), "codemode"),
        (Some(flow_scope()), "flow"),
    ] {
        let mut rig = call_mode_rig(&[CALL_MODES_CAPABILITY]).await;
        let scoped = record.is_some();
        if let Some(record) = record {
            rig.sync(vec![record]).await;
        }
        let route = rig
            .bound(
                Some(AFT),
                PLEXUS,
                scoped.then(|| rig_selector("s", Some(1))),
            )
            .await;
        assert!(rig.live(&route));
        let body: Value = serde_json::from_slice(&route.bind.body).unwrap();
        assert_eq!(body["call_mode"], mode);
        if scoped {
            assert_eq!(body["scope"]["expires_at_ms"], 2_000);
            assert_eq!(route.stamp().unwrap().expires_at_ms, Some(2_000));
        } else {
            assert!(body.get("scope").is_none());
        }
    }
}

#[tokio::test]
async fn call_modes_legacy_scoped_binds_match_golden_without_new_fields() {
    for (provides, record, golden) in [
        (
            vec![],
            None,
            include_str!("../tests/golden/route_bind_legacy_unscoped.json"),
        ),
        (
            vec!["agent-run-scopes/v1"],
            Some(run_scope(false)),
            include_str!("../tests/golden/route_bind_legacy_run.json"),
        ),
        (
            vec!["flow-scopes/v1"],
            Some(flow_scope()),
            include_str!("../tests/golden/route_bind_legacy_flow.json"),
        ),
    ] {
        let mut rig = call_mode_rig(&provides).await;
        let scoped = record.is_some();
        if let Some(record) = record {
            rig.sync(vec![record]).await;
        }
        let route = rig
            .bound(
                Some(AFT),
                PLEXUS,
                scoped.then(|| rig_selector("s", Some(1))),
            )
            .await;
        assert!(rig.live(&route));
        assert_golden(&route.bind, golden);
    }
}

#[tokio::test]
async fn call_modes_declarer_run_bind_matches_golden_and_describe_returns_deadline() {
    let mut rig = call_mode_rig(&[CALL_MODES_CAPABILITY]).await;
    rig.sync(vec![run_scope(false)]).await;
    let route = rig
        .bound(Some(AFT), PLEXUS, Some(rig_selector("s", Some(1))))
        .await;
    assert_golden(
        &route.bind,
        include_str!("../tests/golden/route_bind_declarer_run.json"),
    );
    let reply = call(
        &rig.handler,
        &rig.owner,
        &ModuleControlRequestFromModule::ScopeDescribe {
            owner: Principal::Reserved {
                module_id: OWNER.into(),
            },
            scope_ref: "s".into(),
        },
    )
    .await;
    let body: Value = serde_json::from_slice(&reply.body).unwrap();
    assert_eq!(body["scope"]["expires_at_ms"], 2_000);
    assert_eq!(body["scope"]["attributes"]["run_id"], "run:7");
}

#[tokio::test]
async fn call_modes_delegating_run_admitted_to_declarer() {
    let mut rig = call_mode_rig(&[CALL_MODES_CAPABILITY]).await;
    rig.sync(vec![run_scope(true)]).await;
    let route = rig
        .bound(Some(AFT), PLEXUS, Some(rig_selector("s", Some(1))))
        .await;
    assert!(rig.live(&route));
    let body: Value = serde_json::from_slice(&route.bind.body).unwrap();
    assert_eq!(body["call_mode"], "codemode");
    assert_eq!(body["scope"]["attributes"]["delegates"], true);
}

#[tokio::test]
async fn call_modes_delegating_run_refused_at_admission_before_reservation() {
    let mut rig = call_mode_rig(&["agent-run-scopes/v1"]).await;
    rig.sync(vec![run_scope(true)]).await;
    let (client, _client_rx, frame) =
        rig.open_frame(Some(AFT), PLEXUS, Some(rig_selector("s", Some(1))));
    // Filling the response queue proves that refusal occurs before reservation,
    // rather than merely observing the same refusal from the relay re-check.
    for _ in 0..64 {
        client.egress.try_send(route_bind_ack(1)).unwrap();
    }
    let replies = tokio::time::timeout(
        Duration::from_millis(100),
        rig.handler.handle_control_frame(&client, frame),
    )
    .await
    .expect("delegating runs must be refused at admission")
    .unwrap();
    let body = parse_error(&replies[0]);
    assert_eq!(body["code"], error_codes::TARGET_AGENT_RUN_UNSUPPORTED);
    assert!(body["message"]
        .as_str()
        .unwrap()
        .contains("delegating agent authority"));
    assert!(body["message"]
        .as_str()
        .unwrap()
        .contains(CALL_MODES_CAPABILITY));
    assert!(rig.modules.get_mut(PLEXUS).unwrap().1.try_recv().is_err());
    assert_eq!(rig.forwarding.reserved_route_count().unwrap(), (0, 0));
}

async fn relay_to_legacy_replacement(delegates: bool) {
    use std::future::Future;

    let mut rig = call_mode_rig(&[CALL_MODES_CAPABILITY]).await;
    rig.sync(vec![run_scope(delegates)]).await;
    let (client, mut client_rx, frame) =
        rig.open_frame(Some(AFT), PLEXUS, Some(rig_selector("s", Some(1))));
    // Pause after admission by exhausting response permits. The relay must use
    // the replacement's legacy declaration, not the admitted module's modes.
    for _ in 0..64 {
        client.egress.try_send(route_bind_ack(1)).unwrap();
    }
    let handler = rig.handler.clone();
    let mut open = Box::pin(handler.handle_control_frame(&client, frame));
    std::future::poll_fn(|cx| {
        assert!(open.as_mut().poll(cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    assert_eq!(rig.forwarding.reserved_route_count().unwrap(), (0, 0));
    rig.handler
        .cleanup_connection(rig.modules[PLEXUS].0.connection_id)
        .unwrap();
    let (replacement, mut replacement_rx) = wide_ctx(200);
    hello_via_sink(
        &rig.handler,
        &replacement,
        &mut replacement_rx,
        hello_with_capabilities(PLEXUS, 200, &["agent-run-scopes/v1"]),
    )
    .await;
    while client_rx.try_recv().is_ok() {}
    if delegates {
        let replies = tokio::time::timeout(Duration::from_secs(2), open)
            .await
            .expect("the delegating run is refused without awaiting an ack")
            .unwrap();
        let body = parse_error(&replies[0]);
        assert_eq!(body["code"], error_codes::TARGET_AGENT_RUN_UNSUPPORTED);
        assert!(body["message"]
            .as_str()
            .unwrap()
            .contains("delegating agent authority"));
        assert!(
            replacement_rx.try_recv().is_err(),
            "no delegating bind reaches the legacy replacement"
        );
        assert_eq!(rig.forwarding.reserved_route_count().unwrap(), (0, 0));
    } else {
        let bind = tokio::select! {
            result = &mut open => panic!("non-delegating legacy bind refused: {result:?}"),
            outbound = tokio::time::timeout(Duration::from_secs(2), replacement_rx.recv()) => {
                outbound.expect("the replacement receives a bind").unwrap().frame
            }
        };
        assert_golden(
            &bind,
            include_str!("../tests/golden/route_bind_legacy_run.json"),
        );
        rig.handler
            .handle_control_frame(&replacement, route_bind_ack(bind.header.corr))
            .await
            .unwrap();
        assert!(tokio::time::timeout(Duration::from_secs(2), open)
            .await
            .unwrap()
            .unwrap()
            .is_empty());
        let (channel, epoch) = published_route(&client_rx.recv().await.unwrap().frame);
        assert!(matches!(
            rig.forwarding
                .lookup_data_route(client.connection_id, channel, epoch)
                .unwrap(),
            DataRoute::Client(DataRouteState::Bound(_))
        ));
        assert_eq!(rig.forwarding.reserved_route_count().unwrap(), (0, 0));
    }
    assert!(rig.modules.get_mut(PLEXUS).unwrap().1.try_recv().is_err());
}

#[tokio::test]
async fn call_modes_relay_legacy_run_omits_new_fields_from_admission_declarer() {
    relay_to_legacy_replacement(false).await;
}

#[tokio::test]
async fn call_modes_relay_delegating_run_refused_and_reservation_released() {
    relay_to_legacy_replacement(true).await;
}
