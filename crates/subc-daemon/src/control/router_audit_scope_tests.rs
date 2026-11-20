// Regression test for a scoped-route defect found by a read-only audit of
// route publication: a bind admitted before its scope's absolute deadline must
// not publish a route after that deadline.

/// The opener's bind is relayed just before the scope's wall-clock deadline
/// and the module accepts it just after, before any expiry sweep has run. The
/// open must be refused as an expired scope rather than publish a route the
/// scope no longer covers, and the module told to drop the binding it made.
#[tokio::test]
async fn scoped_bind_accepted_after_the_scope_deadline_does_not_publish_the_route() {
    let mut rig = rig().await;
    let clock = Arc::new(std::sync::atomic::AtomicU64::new(1_000));
    let source = Arc::clone(&clock);
    rig.handler = rig
        .handler
        .clone()
        .with_wall_clock(move || source.load(std::sync::atomic::Ordering::SeqCst));
    rig.sync(vec![session(1).with_expires_at_ms(Some(2_000))])
        .await;

    let relayed = rig
        .relayed(Some(AFT), PLEXUS, Some(rig_selector("s", Some(1))))
        .await;
    let (bind_channel, bind_epoch) = route_bind_channel(&relayed.bind);
    // The deadline passes while the bind is pending; no sweep runs.
    clock.store(2_000, std::sync::atomic::Ordering::SeqCst);
    rig.ack(&relayed).await;
    let Relayed {
        mut client_rx,
        task,
        client,
        ..
    } = relayed;
    let replies = task.await.unwrap();
    let published = client_rx.try_recv().ok().map(|outbound| {
        let (channel, epoch) = published_route(&outbound.frame);
        (
            channel,
            epoch,
            matches!(
                rig.forwarding
                    .lookup_data_route(client.connection_id, channel, epoch)
                    .unwrap(),
                DataRoute::Client(DataRouteState::Bound(_))
            ),
        )
    });
    assert!(
        published.is_none(),
        "the module accepted the bind after the scope's 2000 ms deadline, yet the open was \
         answered with a route (channel, epoch, live) = {published:?}; open replies {replies:?}"
    );
    assert_eq!(replies.len(), 1, "the open is refused: {replies:?}");
    // The same refusal an open arriving after the deadline gets.
    let refusal = parse_error(&replies[0]);
    assert_eq!(refusal["code"], error_codes::SCOPE_NOT_LIVE, "{refusal}");
    assert!(
        refusal["message"]
            .as_str()
            .unwrap()
            .contains("deadline 2000 has passed"),
        "{refusal}"
    );
    assert_eq!(rig.forwarding.reserved_route_count().unwrap(), (0, 0));
    // The module is told to drop just the binding it created.
    let (_, plexus_rx) = rig.modules.get_mut(PLEXUS).unwrap();
    let goodbye = plexus_rx
        .try_recv()
        .expect("the module is sent a GOODBYE for the abandoned bind")
        .frame;
    assert_eq!(
        (
            goodbye.header.ty,
            goodbye.header.channel,
            goodbye.header.epoch
        ),
        (FrameType::Goodbye, bind_channel, bind_epoch)
    );
}

/// The wall clock can step backwards (an NTP step, a VM resync) between two
/// reads. Settlement must judge the scope deadline from one read: a guard
/// that saw the deadline pass and a body that no longer does must not panic
/// under the forwarding write lock, which would poison it for every route.
#[tokio::test]
async fn scope_deadline_settlement_survives_a_wall_clock_step_back() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let mut rig = rig().await;
    // Until armed the clock reads 1000. Armed, its next read is 2000, the
    // deadline, and every later read has stepped back to 1000.
    let armed = Arc::new(AtomicBool::new(false));
    let clock_armed = Arc::clone(&armed);
    rig.handler = rig.handler.clone().with_wall_clock(move || {
        if clock_armed.swap(false, Ordering::SeqCst) {
            2_000
        } else {
            1_000
        }
    });
    rig.sync(vec![session(1).with_expires_at_ms(Some(2_000))])
        .await;
    let relayed = rig
        .relayed(Some(AFT), PLEXUS, Some(rig_selector("s", Some(1))))
        .await;
    armed.store(true, Ordering::SeqCst);
    rig.ack(&relayed).await;
    let replies = relayed.task.await.unwrap();
    assert_eq!(replies.len(), 1, "the open is refused: {replies:?}");
    assert_eq!(
        parse_error(&replies[0])["code"],
        error_codes::SCOPE_NOT_LIVE
    );
    // The forwarding table is still usable.
    assert_eq!(rig.forwarding.reserved_route_count().unwrap(), (0, 0));
}
