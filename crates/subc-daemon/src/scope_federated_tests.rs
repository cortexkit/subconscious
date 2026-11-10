use super::*;
use subc_protocol::scope::{
    FederatedAttributes, FederatedPeerClass, RemoteClaim, ScopeAttributes,
    SCOPE_FEDERATED_CAPABILITY,
};

fn facts(claim: Option<RemoteClaim>) -> FederatedAttributes {
    FederatedAttributes::new(
        FederatedPeerClass::Computer,
        "a".repeat(32),
        "pair:1".into(),
        "Paired computer".into(),
    )
    .with_remote_claim(claim)
}

fn federated(scope_ref: &str, epoch: u64, claim: Option<RemoteClaim>) -> ScopeRecord {
    ScopeRecord::new(scope_ref, epoch, ScopeKind::Federated)
        .with_carriers(vec![carrier(AFT, Some(&[PLEXUS, OTHER]))])
        .with_attributes(ScopeAttributes::new().with_federated(Some(facts(claim))))
}

fn required_stamp() -> ScopeRecord {
    let mut record = federated("base", 1, Some(RemoteClaim::agent("agent")));
    record.attributes.federated.as_mut().unwrap().stamp_required = true;
    record
}

#[tokio::test]
async fn federated_stamp_required_rechecks_captured_replacement_before_relay() {
    use std::future::Future;
    let mut rig = federated_rig(&[SCOPE_FEDERATED_CAPABILITY]).await;
    rig.sync(vec![required_stamp()]).await;
    let (client, mut client_rx, frame) = rig.open_frame(Some(AFT), PLEXUS, Some(rig_selector("base", Some(1))));
    // Exhaust response permits to pause after admission but before the relay
    // captures its endpoint. A new connection may make a different promise.
    for _ in 0..64 { client.egress.try_send(route_bind_ack(1)).unwrap(); }
    let handler = rig.handler.clone();
    let mut open = Box::pin(handler.handle_control_frame(&client, frame));
    std::future::poll_fn(|cx| {
        assert!(open.as_mut().poll(cx).is_pending());
        std::task::Poll::Ready(())
    }).await;
    rig.handler.cleanup_connection(rig.modules[PLEXUS].0.connection_id).unwrap();
    let (replacement, mut replacement_rx) = wide_ctx(200);
    hello_via_sink(&rig.handler, &replacement, &mut replacement_rx, hello_with_capabilities(PLEXUS, 200, &[])).await;
    while client_rx.try_recv().is_ok() {}
    let replies = tokio::time::timeout(Duration::from_secs(2), open).await.expect("refused before a bind ack").unwrap();
    assert_eq!(parse_error(&replies[0])["code"], error_codes::FEDERATED_STAMP_REQUIRED);
    assert!(replacement_rx.try_recv().is_err(), "no bind reaches the legacy replacement");
    assert!(rig.modules.get_mut(PLEXUS).unwrap().1.try_recv().is_err());
    assert_eq!(rig.forwarding.reserved_route_count().unwrap(), (0, 0));
}

#[tokio::test]
async fn federated_stamp_required_refuses_admission_and_never_reaches_provider_or_nonowner_describe() {
    let mut legacy = federated_rig(&[]).await;
    legacy.sync(vec![required_stamp()]).await;
    assert_eq!(legacy.refused(Some(AFT), PLEXUS, Some(rig_selector("base", Some(1)))).await,
        error_codes::FEDERATED_STAMP_REQUIRED);
    assert!(!error_codes::is_retryable_route_open(error_codes::FEDERATED_STAMP_REQUIRED));
    let mut rig = federated_rig(&[SCOPE_FEDERATED_CAPABILITY]).await;
    rig.sync(vec![required_stamp()]).await;
    let bound = rig.bound(Some(AFT), PLEXUS, Some(rig_selector("base", Some(1)))).await;
    assert!(!String::from_utf8_lossy(&bound.bind.body).contains("stamp_required"));
    let reply = call(&rig.handler, &rig.modules[PLEXUS].0, &describe_request()).await;
    assert!(!String::from_utf8_lossy(&reply.body).contains("stamp_required"));
    // Owners may inspect the daemon instruction, but must declare the same
    // strict federated decoder capability required of every describe reader.
    rig.handler.cleanup_connection(rig.owner.connection_id).unwrap();
    let (owner, mut owner_rx) = wide_ctx(44);
    let hello = hello_frame_with_nonce(OWNER, PROTOCOL_VERSION, 44, Some(&nonce(OWNER)));
    let mut body: Value = serde_json::from_slice(&hello.body).unwrap();
    body["manifest"]["capabilities"] = serde_json::json!({"provides":[SCOPE_FEDERATED_CAPABILITY]});
    let hello = Frame::build(FrameType::Hello, control_flags(), 0, 0, 44, serde_json::to_vec(&body).unwrap()).unwrap();
    hello_via_sink(&rig.handler, &owner, &mut owner_rx, hello).await;
    let reply = call(&rig.handler, &owner, &describe_request()).await;
    let body: Value = serde_json::from_slice(&reply.body).unwrap();
    assert_eq!(body["scope"]["attributes"]["federated"]["stamp_required"], true);
}

async fn federated_rig(provides: &[&str]) -> Rig {
    let mut rig = rig_with_target_capabilities(provides).await;
    rig.handler = rig.handler.clone().with_federated_scope_owner(OWNER.into());
    rig
}

#[tokio::test]
async fn federated_display_renames_keep_bound_routes_live_and_refresh_new_stamps() {
    for claim in [
        RemoteClaim::agent("agent"),
        RemoteClaim::run("agent"),
        RemoteClaim::worker("owner", "a".repeat(64), Some("agent".into())),
        RemoteClaim::flow("flow", 1, "install", "a".repeat(64)),
    ] {
        let field = if matches!(claim, RemoteClaim::Flow { .. }) { "flow_name" } else { "agent_name" };
        let named = |name| {
            if field == "flow_name" { claim.clone().with_flow_name(name) }
            else { claim.clone().with_agent_name(name) }
        };
        let mut rig = federated_rig(&[SCOPE_FEDERATED_CAPABILITY]).await;
        rig.sync(vec![federated("claim", 1, Some(named("Original")))]).await;
        let route = rig.bound(Some(AFT), PLEXUS, Some(rig_selector("claim", Some(1)))).await;
        rig.sync(vec![federated("claim", 1, Some(named("Renamed")))]).await;
        assert!(rig.live(&route));
        let refreshed = rig.bound(Some(AFT), PLEXUS, Some(rig_selector("claim", Some(1)))).await;
        let stamp = serde_json::to_value(refreshed.stamp().unwrap()).unwrap();
        assert_eq!(stamp["attributes"]["federated"]["remote_claim"][field], "Renamed");
        assert!(rig.live(&route));
    }
}

#[tokio::test]
async fn federated_capability_membership_is_frozen_for_active_and_candidate_connections() {
    for candidate in [false, true] {
        for declared in [false, true] {
            let registry = Arc::new(Registry::default());
            let handler = ControlHandler::new(Arc::clone(&registry));
            let (ctx, mut rx) = route_ctx(ConnectionId::new(77));
            let initial = capability_manifest(
                "target",
                if declared {
                    &[SCOPE_FEDERATED_CAPABILITY]
                } else {
                    &[]
                },
                &[],
            );
            if candidate {
                registry
                    .register_candidate_with_control_ops(
                        initial.clone(),
                        PROTOCOL_VERSION,
                        ctx.connection_id,
                        module_baseline_control_ops(),
                    )
                    .unwrap();
                handler
                    .forwarding
                    .register_candidate_module_connection(
                        ctx.connection_id,
                        "target".into(),
                        PROTOCOL_VERSION,
                        manifest_concurrency(&initial),
                        ctx.egress.clone(),
                    )
                    .unwrap();
            } else {
                hello_via_sink(
                    &handler,
                    &ctx,
                    &mut rx,
                    hello_frame_with_manifest(initial.clone(), 1),
                )
                .await;
            }
            let generation = registry.generation().unwrap();
            let reply = call(
                &handler,
                &ctx,
                &ModuleControlRequestFromModule::CatalogUpdate {
                    provides: initial.provides.clone(),
                    capabilities: capability_manifest(
                        "target",
                        if declared {
                            &[]
                        } else {
                            &[SCOPE_FEDERATED_CAPABILITY]
                        },
                        &[],
                    )
                    .capabilities,
                    ready: Some(false),
                },
            )
            .await;
            assert_eq!(reply.header.ty, FrameType::Error);
            assert_eq!(
                parse_error(&reply)["code"],
                "catalog_update_frozen_capability"
            );
            assert_eq!(
                registry
                    .get_module_by_connection(ctx.connection_id)
                    .unwrap()
                    .unwrap()
                    .manifest,
                initial
            );
            assert_eq!(registry.generation().unwrap(), generation);
        }
    }
}

#[tokio::test]
async fn federated_capability_cannot_be_withdrawn_after_snapshot_while_target_egress_is_full() {
    use std::future::Future;
    for claim in [
        RemoteClaim::flow("flow", 1, "install", "a".repeat(64)),
        RemoteClaim::run("agent"),
    ] {
        let mut rig = federated_rig(&[SCOPE_FEDERATED_CAPABILITY]).await;
        rig.sync(vec![federated("claim", 1, Some(claim.clone()))])
            .await;
        let target = rig.modules[PLEXUS].0.clone();
        for _ in 0..64 {
            target.egress.try_send(route_bind_ack(1)).unwrap();
        }
        let (client, mut client_rx, frame) =
            rig.open_frame(Some(AFT), PLEXUS, Some(rig_selector("claim", Some(1))));
        let handler = rig.handler.clone();
        let mut open = Box::pin(handler.handle_control_frame(&client, frame));
        std::future::poll_fn(|cx| {
            assert!(open.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        let initial = handler
            .registry
            .get_module_by_connection(target.connection_id)
            .unwrap()
            .unwrap()
            .manifest;
        let reply = call(
            &handler,
            &target,
            &ModuleControlRequestFromModule::CatalogUpdate {
                provides: initial.provides.clone(),
                capabilities: Some(
                    serde_json::from_value(serde_json::json!({"provides":[]})).unwrap(),
                ),
                ready: None,
            },
        )
        .await;
        assert_eq!(reply.header.ty, FrameType::Error);
        assert_eq!(
            parse_error(&reply)["code"],
            "catalog_update_frozen_capability"
        );
        assert_eq!(
            handler
                .registry
                .get_module_by_connection(target.connection_id)
                .unwrap()
                .unwrap()
                .manifest,
            initial
        );
        let target_rx = &mut rig.modules.get_mut(PLEXUS).unwrap().1;
        for _ in 0..64 {
            target_rx.try_recv().unwrap();
        }
        let bind = tokio::select! {
            result = &mut open => panic!("bind must await its ack: {result:?}"),
            bind = target_rx.recv() => bind.unwrap().frame,
        };
        let body: ModuleControlRequest = serde_json::from_slice(&bind.body).unwrap();
        let ModuleControlRequest::RouteBind {
            scope: Some(stamp), ..
        } = body
        else {
            panic!("expected a stamped bind")
        };
        assert_eq!(
            stamp.attributes.federated.unwrap().remote_claim,
            Some(claim)
        );
        handler
            .handle_control_frame(&target, route_bind_ack(bind.header.corr))
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
    }
}

fn describe_request() -> ModuleControlRequestFromModule {
    ModuleControlRequestFromModule::ScopeDescribe {
        owner: Principal::Reserved {
            module_id: OWNER.into(),
        },
        scope_ref: "base".into(),
    }
}

#[tokio::test]
async fn federated_default_config_preserves_existing_local_stamp_bytes_and_advertises_support() {
    let mut rig = rig_with_target_capabilities(&[]).await;
    assert_eq!(rig.handler.federated_scope_owner, "callosum");
    assert!(rig
        .handler
        .subc_capabilities
        .iter()
        .any(|cap| cap == SCOPE_FEDERATED_CAPABILITY));
    rig.sync(vec![session(1)]).await;
    let route = rig
        .bound(Some(AFT), PLEXUS, Some(rig_selector("s", Some(1))))
        .await;
    assert_eq!(
        serde_json::to_string(&route.stamp().unwrap()).unwrap(),
        r#"{"owner":{"kind":"reserved","module_id":"prefrontal-core"},"ref":"s","scope_epoch":1,"kind":"head","attributes":{"agent_id":"agent-1","delegates":true},"owner_authorized":true}"#
    );
}

#[tokio::test]
async fn federated_stamp_is_capability_gated_but_legacy_route_keeps_epoch_tracking() {
    for claim in [
        None,
        Some(RemoteClaim::agent("agent")),
        Some(RemoteClaim::worker("worker-owner", "a".repeat(64), None)),
    ] {
        for provides in [vec![], vec![SCOPE_FEDERATED_CAPABILITY]] {
            let mut rig = federated_rig(&provides).await;
            rig.sync(vec![federated("base", 1, claim.clone())]).await;
            let mut route = rig
                .bound(Some(AFT), PLEXUS, Some(rig_selector("base", Some(1))))
                .await;
            if provides.is_empty() {
                assert!(route.stamp().is_none());
                let plain = rig.bound(Some(AFT), PLEXUS, None).await;
                // Normalize only allocation numbers in the raw bytes, preserving JSON field order.
                let normalize = |bind: &Frame| {
                    let (channel, epoch) = route_bind_channel(bind);
                    let body = std::str::from_utf8(&bind.body).unwrap();
                    let channel = format!("\"route_channel\":{channel},");
                    let epoch = format!("\"epoch\":{epoch},");
                    assert!(body.contains(&channel) && body.contains(&epoch));
                    body.replacen(&channel, "\"route_channel\":0,", 1).replacen(
                        &epoch,
                        "\"epoch\":0,",
                        1,
                    )
                };
                assert_eq!(normalize(&route.bind), normalize(&plain.bind));
            } else {
                let stamp = route.stamp().unwrap();
                assert_eq!(stamp.kind, ScopeKind::Federated);
                assert_eq!(stamp.attributes.federated, Some(facts(claim.clone())));
                assert!(!stamp.owner_authorized);
            }
            rig.sync(vec![]).await;
            assert_eq!(route.closed_reason(), RouteCloseReason::ScopeEnded);
            assert!(!rig.live(&route));
        }
    }
}

#[tokio::test]
async fn federated_flow_and_run_binds_never_downgrade_on_capability_loss() {
    for claim in [
        RemoteClaim::flow("flow", 1, "install", "a".repeat(64)),
        RemoteClaim::run("agent"),
    ] {
        let mut rig = federated_rig(&[SCOPE_FEDERATED_CAPABILITY]).await;
        rig.sync(vec![federated("claim", 1, Some(claim.clone()))])
            .await;
        let route = rig
            .bound(Some(AFT), PLEXUS, Some(rig_selector("claim", Some(1))))
            .await;
        assert!(rig.live(&route));
        let update = ModuleControlRequestFromModule::CatalogUpdate {
            provides: rig
                .handler
                .registry
                .get_module_by_connection(rig.modules[PLEXUS].0.connection_id)
                .unwrap()
                .unwrap()
                .manifest
                .provides
                .clone(),
            capabilities: Some(
                serde_json::from_value(serde_json::json!({"provides": []})).unwrap(),
            ),
            ready: None,
        };
        let reply = call(&rig.handler, &rig.modules[PLEXUS].0, &update).await;
        assert_eq!(
            parse_error(&reply)["code"],
            "catalog_update_frozen_capability"
        );
        assert!(rig.live(&route));
        // A new registration may omit the capability, but its flow/run binds must refuse.
        let mut rig = federated_rig(&[]).await;
        rig.sync(vec![federated("claim", 1, Some(claim))]).await;
        assert_eq!(
            rig.refused(Some(AFT), PLEXUS, Some(rig_selector("claim", Some(1))))
                .await,
            error_codes::TARGET_FEDERATED_UNSUPPORTED
        );
        assert!(!subc_protocol::error_codes::is_retryable_route_open(
            error_codes::TARGET_FEDERATED_UNSUPPORTED
        ));
    }
}

#[tokio::test]
async fn federated_describe_requires_capability_not_history_even_before_owner_resync() {
    let mut rig = federated_rig(&[SCOPE_FEDERATED_CAPABILITY]).await;
    let request = describe_request();
    let legacy = call(&rig.handler, &rig.owner, &request).await;
    assert_eq!(
        parse_error(&legacy)["code"],
        error_codes::SCOPE_FEDERATED_UNSUPPORTED
    );
    // A fresh table is the post-restart state: the configured owner has not re-synced.
    let reply = call(&rig.handler, &rig.modules[PLEXUS].0, &request).await;
    let body: Value = serde_json::from_slice(&reply.body).unwrap();
    assert_eq!(body["status"], "not_live");
    assert_eq!(body["owner_synced"], false);
    assert_eq!(body["owner_configured"], true);
    rig.sync(vec![federated("base", 1, None)]).await;
    let reply = call(&rig.handler, &rig.modules[PLEXUS].0, &request).await;
    let body: Value = serde_json::from_slice(&reply.body).unwrap();
    assert_eq!(body["scope"]["kind"], "federated");
    assert_eq!(body["scope"]["owner_authorized"], false);
    let legacy = call(&rig.handler, &rig.owner, &request).await;
    assert_eq!(
        parse_error(&legacy)["code"],
        error_codes::SCOPE_FEDERATED_UNSUPPORTED
    );
    assert!(!std::str::from_utf8(&legacy.body)
        .unwrap()
        .contains("peer_fingerprint"));
    rig.handler = rig.handler.clone().with_federated_scope_owner(OWNER.into());
    let restarted = call(&rig.handler, &rig.modules[PLEXUS].0, &request).await;
    let body: Value = serde_json::from_slice(&restarted.body).unwrap();
    assert_eq!(body["status"], "not_live");
    assert_eq!(body["owner_synced"], false);
}

const CONSENT: &str = "cingulate";

async fn claim_rig_with_carrier() -> (Rig, Bound) {
    let mut rig = federated_rig(&[SCOPE_FEDERATED_CAPABILITY]).await;
    let (ctx, mut rx) = wide_ctx(4);
    hello_via_sink(
        &rig.handler,
        &ctx,
        &mut rx,
        hello_with_capabilities(CONSENT, 4, &[SCOPE_FEDERATED_CAPABILITY]),
    )
    .await;
    rig.modules.insert(CONSENT.into(), (ctx, rx));
    rig.handler
        .supervisor
        .set_spawn_nonce(PLEXUS, nonce(PLEXUS));
    let base = federated("base", 1, None);
    let claim = federated("claim", 1, Some(RemoteClaim::agent("agent"))).with_parent(Some(
        subc_protocol::scope::ScopeParent::new(
            Principal::Reserved {
                module_id: OWNER.into(),
            },
            "base",
            1,
        ),
    ));
    rig.sync(vec![base.clone(), claim.clone()]).await;
    let provider = rig
        .bound(Some(AFT), PLEXUS, Some(rig_selector("claim", Some(1))))
        .await;
    let claim = claim.with_carriers(vec![
        carrier(AFT, Some(&[PLEXUS, OTHER])),
        carrier(PLEXUS, Some(&[CONSENT])),
    ]);
    let reply = call(
        &rig.handler,
        &rig.owner,
        &ModuleControlRequestFromModule::ScopeApply {
            generation: 2,
            upsert: vec![claim],
            end: vec![],
        },
    )
    .await;
    assert_eq!(reply.header.ty, FrameType::Response);
    assert!(rig.live(&provider), "adding a carrier closes nothing");
    (rig, provider)
}

#[tokio::test]
async fn federated_carrier_opens_only_cingulate() {
    let (mut rig, _provider) = claim_rig_with_carrier().await;
    assert_eq!(
        rig.refused(Some(PLEXUS), OTHER, Some(rig_selector("claim", Some(1))))
            .await,
        error_codes::SCOPE_NOT_CARRIER
    );
    let carried = rig
        .bound(Some(PLEXUS), CONSENT, Some(rig_selector("claim", Some(1))))
        .await;
    assert_eq!(
        carried.stamp().unwrap().attributes.federated,
        Some(facts(Some(RemoteClaim::agent("agent"))))
    );
}

#[tokio::test]
async fn federated_base_end_closes_claim_pending_and_carried_routes() {
    let (mut rig, mut provider) = claim_rig_with_carrier().await;
    let mut carried = rig
        .bound(Some(PLEXUS), CONSENT, Some(rig_selector("claim", Some(1))))
        .await;
    let pending = rig
        .relayed(Some(AFT), OTHER, Some(rig_selector("claim", Some(1))))
        .await;
    // The owner ends both levels atomically; the daemon deliberately does not cascade scope deletion.
    let reply = call(
        &rig.handler,
        &rig.owner,
        &ModuleControlRequestFromModule::ScopeApply {
            generation: 3,
            upsert: vec![],
            end: vec![
                subc_protocol::scope::ScopeEnd::new("base", 1),
                subc_protocol::scope::ScopeEnd::new("claim", 1),
            ],
        },
    )
    .await;
    assert_eq!(reply.header.ty, FrameType::Response);
    for route in [&mut provider, &mut carried] {
        assert!(!rig.live(route));
        assert_eq!(route.closed_reason(), RouteCloseReason::ScopeEnded);
    }
    rig.ack(&pending).await;
    let replies = pending.task.await.unwrap();
    assert_eq!(replies.len(), 1);
    assert_eq!(parse_error(&replies[0])["code"], error_codes::SCOPE_ENDED);
    assert_eq!(rig.forwarding.reserved_route_count().unwrap(), (0, 0));
}

#[tokio::test]
async fn federated_flow_capability_is_fixed_while_opener_egress_is_full() {
    use std::future::Future;
    let mut rig = federated_rig(&[SCOPE_FEDERATED_CAPABILITY]).await;
    rig.sync(vec![federated(
        "claim",
        1,
        Some(RemoteClaim::flow("flow", 1, "install", "a".repeat(64))),
    )])
    .await;
    let (client, mut rx, frame) =
        rig.open_frame(Some(AFT), PLEXUS, Some(rig_selector("claim", Some(1))));
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
    let target = rig.modules[PLEXUS].0.clone();
    let initial = handler
        .registry
        .get_module_by_connection(target.connection_id)
        .unwrap()
        .unwrap()
        .manifest;
    let reply = call(
        &handler,
        &target,
        &ModuleControlRequestFromModule::CatalogUpdate {
            provides: initial.provides,
            capabilities: Some(serde_json::from_value(serde_json::json!({"provides":[]})).unwrap()),
            ready: None,
        },
    )
    .await;
    assert_eq!(
        parse_error(&reply)["code"],
        "catalog_update_frozen_capability"
    );
    for _ in 0..64 {
        rx.try_recv().unwrap();
    }
    let target_rx = &mut rig.modules.get_mut(PLEXUS).unwrap().1;
    let bind = tokio::select! {
        result = &mut open => panic!("bind must await its ack: {result:?}"),
        bind = target_rx.recv() => bind.unwrap().frame,
    };
    handler
        .handle_control_frame(&target, route_bind_ack(bind.header.corr))
        .await
        .unwrap();
    assert!(tokio::time::timeout(Duration::from_secs(2), open)
        .await
        .unwrap()
        .unwrap()
        .is_empty());
    let (channel, epoch) = published_route(&rx.recv().await.unwrap().frame);
    assert!(matches!(
        rig.forwarding
            .lookup_data_route(client.connection_id, channel, epoch)
            .unwrap(),
        DataRoute::Client(DataRouteState::Bound(_))
    ));
}

#[tokio::test]
async fn federated_delegates_changes_in_either_direction_close_existing_local_routes() {
    for delta in [false, true] {
        let mut rig = rig().await;
        rig.sync(vec![session(1)]).await;
        for (generation, delegates) in [(2, false), (3, true)] {
            let mut route = rig
                .bound(Some(AFT), PLEXUS, Some(rig_selector("s", Some(1))))
                .await;
            let mut changed = session(1);
            changed.attributes.delegates = delegates;
            if delta {
                let reply = call(
                    &rig.handler,
                    &rig.owner,
                    &ModuleControlRequestFromModule::ScopeApply {
                        generation,
                        upsert: vec![changed],
                        end: vec![],
                    },
                )
                .await;
                assert_eq!(reply.header.ty, FrameType::Response);
            } else {
                rig.sync(vec![changed]).await;
            }
            assert!(!rig.live(&route));
            assert_eq!(
                route.closed_reason(),
                RouteCloseReason::ScopeDelegationChanged
            );
            assert_eq!(
                rig.modules
                    .get_mut(PLEXUS)
                    .unwrap()
                    .1
                    .try_recv()
                    .unwrap()
                    .frame
                    .header
                    .ty,
                FrameType::Goodbye
            );
        }
    }
}
