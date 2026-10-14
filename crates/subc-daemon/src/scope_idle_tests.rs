use subc_protocol::scope::ScopeRecordResult;

const HASH: &str = "4bb06f8e4e3a7715d201d573d0aa423762e55dabd61a2c02278fa56cc6d294e0";
const PROOF: &str = "BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc";

fn direct_session(epoch: u64) -> ScopeRecord {
    let mut record = session(epoch);
    record.carriers.push(ScopeCarrier::new(Principal::Direct));
    record
}

#[tokio::test]
async fn idle_replace_ignores_reserved_routes() {
    for delta in [false, true] {
        for opener in [OWNER, AFT] {
            let mut rig = rig().await;
            rig.sync(vec![session(1)]).await;
            let mut route = rig
                .bound(Some(opener), PLEXUS, Some(rig.selector("s", Some(1))))
                .await;
            assert_eq!(
                change(&mut rig, vec![session(2).with_replace_only_if_idle(true)], delta)
                    .await[0]
                    .outcome,
                ScopeRecordOutcome::Replaced
            );
            assert!(!rig.live(&route));
            assert_eq!(route.closed_reason(), RouteCloseReason::ScopeEnded);
        }
    }
}

#[tokio::test]
async fn reserved_hold_presence_records_nothing() {
    for opener in [OWNER, AFT] {
        let mut rig = rig().await;
        rig.sync(vec![session(1)]).await;
        let route = rig
            .bound(
                Some(opener),
                PLEXUS,
                Some(rig.selector("s", Some(1)).with_hold_presence(true)),
            )
            .await;
        rig.handler
            .cleanup_connection(rig.modules[PLEXUS].0.connection_id)
            .unwrap();
        assert!(!rig.live(&route));
        assert_eq!(
            change(&mut rig, vec![session(2).with_replace_only_if_idle(true)], true)
                .await[0]
                .outcome,
            ScopeRecordOutcome::Replaced
        );
    }
}

async fn change(rig: &mut Rig, records: Vec<ScopeRecord>, delta: bool) -> Vec<ScopeRecordResult> {
    rig.generation += 1;
    let request = if delta {
        ModuleControlRequestFromModule::ScopeApply {
            generation: rig.generation,
            upsert: records,
            end: vec![],
        }
    } else {
        ModuleControlRequestFromModule::ScopeSync {
            generation: rig.generation,
            scopes: records,
        }
    };
    let reply = call(&rig.handler, &rig.owner, &request).await;
    match serde_json::from_slice(&reply.body).unwrap() {
        ModuleControlResponseToModule::ScopeSync { results, .. }
        | ModuleControlResponseToModule::ScopeApply { results, .. } => results,
        other => panic!("unexpected reply: {other:?}"),
    }
}

fn refused(result: &ScopeRecordResult) {
    assert_eq!(result.outcome, ScopeRecordOutcome::Refused);
    assert_eq!(
        result.code.as_deref(),
        Some(error_codes::SCOPE_EPOCH_IN_USE)
    );
    assert_eq!(
        result.message.as_deref(),
        Some("routes are still bound to the live scope epoch")
    );
}

#[tokio::test]
async fn idle_replace_refuses_live_route_then_succeeds_after_close() {
    for delta in [false, true] {
        let mut rig = rig().await;
        rig.sync(vec![direct_session(1)]).await;
        let route = rig
            .bound(None, PLEXUS, Some(rig.selector("s", Some(1))))
            .await;
        let replacement = direct_session(2)
            .with_bind_proof_sha256(Some(HASH.into()))
            .with_replace_only_if_idle(true);
        let before = describe(&rig.handler, &rig.owner, OWNER, "s").await;
        let results = change(
            &mut rig,
            vec![
                replacement.clone(),
                head("independent", 1).with_replace_only_if_idle(true),
            ],
            delta,
        )
        .await;
        refused(&results[0]);
        assert_eq!(results[1].outcome, ScopeRecordOutcome::Created);
        assert!(rig.live(&route));
        assert_eq!(describe(&rig.handler, &rig.owner, OWNER, "s").await, before);
        rig.handler
            .cleanup_connection(route.client.connection_id)
            .unwrap();
        while rig.modules.get_mut(PLEXUS).unwrap().1.try_recv().is_ok() {}
        let results = change(&mut rig, vec![replacement], delta).await;
        assert_eq!(results[0].outcome, ScopeRecordOutcome::Replaced);
        assert_eq!(
            rig.refused(None, PLEXUS, Some(rig.selector("s", Some(2))))
                .await,
            error_codes::SCOPE_BIND_PROOF_REQUIRED
        );
        let new_route = rig
            .bound(
                None,
                PLEXUS,
                Some(
                    rig.selector("s", Some(2))
                        .with_bind_proof(Some(PROOF.into())),
                ),
            )
            .await;
        assert!(rig.live(&new_route));
    }
}

#[tokio::test]
async fn idle_hash_change_refuses_bound_but_unchanged_never_refuses() {
    let mut rig = rig().await;
    rig.sync(vec![direct_session(1)]).await;
    let route = rig
        .bound(None, PLEXUS, Some(rig.selector("s", Some(1))))
        .await;
    let results = change(
        &mut rig,
        vec![direct_session(1).with_replace_only_if_idle(true)],
        true,
    )
    .await;
    assert_eq!(results[0].outcome, ScopeRecordOutcome::Unchanged);
    let update = direct_session(1)
        .with_bind_proof_sha256(Some(HASH.into()))
        .with_replace_only_if_idle(true);
    refused(&change(&mut rig, vec![update.clone()], true).await[0]);
    assert!(rig.live(&route));
    rig.handler
        .cleanup_connection(route.client.connection_id)
        .unwrap();
    while rig.modules.get_mut(PLEXUS).unwrap().1.try_recv().is_ok() {}
    assert_eq!(
        change(&mut rig, vec![update], true).await[0].outcome,
        ScopeRecordOutcome::Updated
    );
    assert_eq!(
        rig.refused(None, PLEXUS, Some(rig.selector("s", Some(1))))
            .await,
        error_codes::SCOPE_BIND_PROOF_REQUIRED
    );
    let new_route = rig
        .bound(
            None,
            PLEXUS,
            Some(
                rig.selector("s", Some(1))
                    .with_bind_proof(Some(PROOF.into())),
            ),
        )
        .await;
    assert!(rig.live(&new_route));
}

#[tokio::test]
async fn unconditional_replace_still_closes_routes() {
    let mut rig = rig().await;
    rig.sync(vec![direct_session(1)]).await;
    let mut route = rig
        .bound(None, PLEXUS, Some(rig.selector("s", Some(1))))
        .await;
    assert_eq!(
        change(&mut rig, vec![direct_session(2)], true).await[0].outcome,
        ScopeRecordOutcome::Replaced
    );
    assert!(!rig.live(&route));
    assert_eq!(route.closed_reason(), RouteCloseReason::ScopeEnded);
}

#[tokio::test]
async fn idle_replace_counts_admitted_bind_before_egress_reservation() {
    use std::future::Future;
    let mut rig = rig().await;
    rig.sync(vec![direct_session(1)]).await;
    let (client, mut rx, frame) =
        rig.open_frame(None, PLEXUS, Some(rig.selector("s", Some(1))));
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
    refused(
        &change(
            &mut rig,
            vec![direct_session(2).with_replace_only_if_idle(true)],
            true,
        )
        .await[0],
    );
    // Dropping the admitted future releases usage even before forwarding has
    // reserved its route handles.
    drop(open);
    rx.try_recv().unwrap();
    assert_eq!(
        change(
            &mut rig,
            vec![direct_session(2).with_replace_only_if_idle(true)],
            true
        )
        .await[0]
            .outcome,
        ScopeRecordOutcome::Replaced
    );
}

#[tokio::test]
async fn presence_survives_target_disconnect_until_holder_disconnect() {
    let mut rig = rig().await;
    rig.sync(vec![direct_session(1)]).await;
    let route = rig
        .bound(
            None,
            PLEXUS,
            Some(rig.selector("s", Some(1)).with_hold_presence(true)),
        )
        .await;
    rig.handler
        .cleanup_connection(rig.modules[PLEXUS].0.connection_id)
        .unwrap();
    assert!(!rig.live(&route));
    let replacement = direct_session(2).with_replace_only_if_idle(true);
    refused(&change(&mut rig, vec![replacement.clone()], true).await[0]);
    rig.handler
        .cleanup_connection(route.client.connection_id)
        .unwrap();
    assert_eq!(
        change(&mut rig, vec![replacement], true).await[0].outcome,
        ScopeRecordOutcome::Replaced
    );
}

#[tokio::test]
async fn refused_open_does_not_hold_presence() {
    let mut rig = rig().await;
    rig.sync(vec![direct_session(1).with_bind_proof_sha256(Some(HASH.into()))])
        .await;
    assert_eq!(
        rig.refused(
            None,
            PLEXUS,
            Some(rig.selector("s", Some(1)).with_hold_presence(true))
        )
        .await,
        error_codes::SCOPE_BIND_PROOF_REQUIRED
    );
    assert_eq!(
        change(
            &mut rig,
            vec![direct_session(2).with_replace_only_if_idle(true)],
            true
        )
        .await[0]
            .outcome,
        ScopeRecordOutcome::Replaced
    );
}

#[tokio::test]
async fn unconditional_replace_drops_old_epoch_presence() {
    let mut rig = rig().await;
    rig.sync(vec![direct_session(1)]).await;
    let route = rig
        .bound(
            None,
            PLEXUS,
            Some(rig.selector("s", Some(1)).with_hold_presence(true)),
        )
        .await;
    assert_eq!(
        change(&mut rig, vec![direct_session(2)], true).await[0].outcome,
        ScopeRecordOutcome::Replaced
    );
    assert!(!rig.live(&route));
    assert_eq!(
        change(
            &mut rig,
            vec![direct_session(3).with_replace_only_if_idle(true)],
            true
        )
        .await[0]
            .outcome,
        ScopeRecordOutcome::Replaced
    );
}

#[tokio::test]
async fn presence_holder_limit_does_not_refuse_opens() {
    let mut rig = rig().await;
    rig.sync(vec![direct_session(1)]).await;
    let mut routes = Vec::new();
    for _ in 0..9 {
        routes.push(
            rig.bound(
                None,
                PLEXUS,
                Some(rig.selector("s", Some(1)).with_hold_presence(true)),
            )
            .await,
        );
    }
    rig.handler
        .cleanup_connection(rig.modules[PLEXUS].0.connection_id)
        .unwrap();
    let replacement = direct_session(2).with_replace_only_if_idle(true);
    refused(&change(&mut rig, vec![replacement.clone()], true).await[0]);
    for route in &routes[..8] {
        rig.handler
            .cleanup_connection(route.client.connection_id)
            .unwrap();
    }
    assert_eq!(
        change(&mut rig, vec![replacement], true).await[0].outcome,
        ScopeRecordOutcome::Replaced
    );
}

#[tokio::test]
async fn idle_condition_is_checked_under_scope_write_lock() {
    let mut rig = rig().await;
    rig.sync(vec![direct_session(1)]).await;
    let scopes = Arc::downgrade(&rig.handler.scopes);
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = Arc::clone(&calls);
    rig.handler.scopes.write().unwrap().idle_check_hook = Some(Arc::new(move || {
        let scopes = scopes.upgrade().unwrap();
        assert!(
            matches!(scopes.try_read(), Err(std::sync::TryLockError::WouldBlock)),
            "idle condition must exclude admission until apply completes"
        );
        assert!(
            matches!(scopes.try_write(), Err(std::sync::TryLockError::WouldBlock)),
            "route.open must not slip between idle check and apply"
        );
        observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }));
    assert_eq!(
        change(
            &mut rig,
            vec![direct_session(2).with_replace_only_if_idle(true)],
            true
        )
        .await[0]
            .outcome,
        ScopeRecordOutcome::Replaced
    );
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn closed_route_snapshots_do_not_keep_epoch_busy() {
    let mut rig = rig().await;
    rig.sync(vec![direct_session(1)]).await;
    let route = rig
        .bound(None, PLEXUS, Some(rig.selector("s", Some(1))))
        .await;
    let snapshot = rig
        .forwarding
        .lookup_data_route(route.client.connection_id, route.channel, route.epoch)
        .unwrap();
    rig.handler
        .cleanup_connection(route.client.connection_id)
        .unwrap();
    assert_eq!(
        change(
            &mut rig,
            vec![direct_session(2).with_replace_only_if_idle(true)],
            true
        )
        .await[0]
            .outcome,
        ScopeRecordOutcome::Replaced
    );
    drop(snapshot);
}

#[tokio::test]
async fn module_refused_open_does_not_hold_presence() {
    let mut rig = rig().await;
    rig.sync(vec![direct_session(1)]).await;
    let relayed = rig
        .relayed(
            None,
            PLEXUS,
            Some(rig.selector("s", Some(1)).with_hold_presence(true)),
        )
        .await;
    let rejection = control_error_body_frame(
        &relayed.bind,
        ErrorBody::new("not_allowed", "provider refused"),
    )
    .unwrap();
    rig.handler
        .handle_control_frame(&rig.modules[PLEXUS].0, rejection)
        .await
        .unwrap();
    let replies = relayed.task.await.unwrap();
    assert_eq!(parse_error(&replies[0])["code"], "not_allowed");
    assert_eq!(
        change(
            &mut rig,
            vec![direct_session(2).with_replace_only_if_idle(true)],
            true
        )
        .await[0]
            .outcome,
        ScopeRecordOutcome::Replaced
    );
}
