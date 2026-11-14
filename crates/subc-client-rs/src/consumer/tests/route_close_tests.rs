use super::*;

async fn close(
    consumer: &SubcConsumer,
    handle: RouteHandle,
    by_handle: bool,
    opts: CloseRouteOptions,
) {
    if by_handle {
        consumer.close_handle(&handle, opts).await.unwrap();
    } else {
        consumer
            .close_route(restart_target("teardown"), restart_identity(), opts)
            .await;
    }
}

async fn assert_closed_once(
    shared: &Arc<Shared>,
    consumer: &SubcConsumer,
    rx: &mut mpsc::Receiver<WriteCommand>,
    route: &RouteState,
) {
    tokio::time::timeout(Duration::from_secs(1), async {
        while shared
            .lock_inner()
            .route_epochs
            .contains_key(&route.handle.channel)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cancelled close must queue GOODBYE and uninstall the handle");
    assert!(route.sem.is_closed());
    {
        let inner = shared.lock_inner();
        assert!(!inner.retired_routes.contains(&route.handle));
        assert!(!inner.retirement_goodbyes.contains(&route.handle));
        assert!(!inner.retirement_queue.contains(&route.handle));
    }
    // The fake daemon must receive exactly one old GOODBYE before a request on
    // the reused channel, even when dropping the close handed work to the flusher.
    tokio::time::timeout(
        Duration::from_secs(1),
        assert_reopened_request_after_goodbye(shared, consumer, rx, route.handle),
    )
    .await
    .expect("GOODBYE must precede the reopened request");
    assert!(
        rx.try_recv().is_err(),
        "close must not queue a second GOODBYE"
    );
}

async fn cancel_during_writer_wait(by_handle: bool) {
    let (shared, consumer, mut rx, _peer, key, route) = route_teardown_stand_in();
    let mut preceding = route.clone();
    preceding.handle = RouteHandle::new(route.handle.channel + 1, 31, 1);
    let preceding_key = RouteKey::new(
        &restart_target("preceding"),
        &restart_identity(),
        None,
        None,
    );
    shared
        .lock_inner()
        .cache_route(preceding_key.clone(), preceding.clone());
    shared.invalidate_route_with_deadline(&preceding_key, Some(preceding.handle), true);

    let mut closing = Box::pin(close(
        &consumer,
        route.handle,
        by_handle,
        CloseRouteOptions::default(),
    ));
    assert_pending(closing.as_mut()).await;
    assert!(!shared.lock_inner().routes.contains_key(&key));
    drop(closing);
    assert_eq!(
        shared.lock_inner().route_epochs.get(&route.handle.channel),
        Some(&route.handle),
        "the channel stays occupied while GOODBYE waits for writer capacity"
    );
    assert_eq!(rx.recv().await.unwrap().frame.body, b"filler");
    let goodbye = tokio::time::timeout(Duration::from_secs(1), rx.recv())
        .await
        .unwrap()
        .unwrap()
        .frame;
    assert_eq!(goodbye.header.ty, FrameType::Goodbye);
    assert_eq!(
        (goodbye.header.channel, goodbye.header.epoch),
        (preceding.handle.channel, preceding.handle.epoch),
        "cancelled closes must not overtake earlier retirement GOODBYEs"
    );
    assert_closed_once(&shared, &consumer, &mut rx, &route).await;
    consumer.close().await;
}

async fn cancel_during_drain(by_handle: bool) {
    let (shared, consumer, mut rx, _peer, key, route) = route_teardown_stand_in();
    assert_eq!(rx.recv().await.unwrap().frame.body, b"filler");
    let mut request = Box::pin(consumer.request(
        &route.handle,
        b"unanswered".to_vec(),
        CallOptions::default(),
    ));
    assert_pending(request.as_mut()).await;
    let command = rx.recv().await.unwrap();
    assert_eq!(command.frame.header.ty, FrameType::Request);
    assert!(shared.mark_pending_accepted(command.pending.unwrap()));
    let mut closing = Box::pin(close(
        &consumer,
        route.handle,
        by_handle,
        CloseRouteOptions {
            drain: true,
            ..CloseRouteOptions::default()
        },
    ));
    assert_pending(closing.as_mut()).await;
    assert!(!shared.lock_inner().routes.contains_key(&key));
    assert!(!route.sem.is_closed(), "drain has not finished yet");
    drop(closing);
    assert!(route.sem.is_closed(), "cancellation must give up the drain");
    let error = request.await.unwrap_err();
    assert_eq!(error.close_reason(), Some(&RouteEndReason::ClosedByCaller));
    assert_eq!(error.outcome_cause(), Some(OutcomeUnknownCause::RouteEnded));
    assert!(shared.lock_inner().pending.is_empty());
    assert!(shared.lock_inner().wire_slots.is_empty());
    assert_closed_once(&shared, &consumer, &mut rx, &route).await;
    consumer.close().await;
}

async fn cancel_when_writer_becomes_ready(by_handle: bool) {
    // Exercise both sides of the readiness boundary deterministically: drop before
    // polling the ready close, or poll it to completion and then drop its future.
    for complete in [false, true] {
        let (shared, consumer, mut rx, _peer, _key, route) = route_teardown_stand_in();
        let mut closing = Box::pin(close(
            &consumer,
            route.handle,
            by_handle,
            CloseRouteOptions::default(),
        ));
        assert_pending(closing.as_mut()).await;
        assert_eq!(rx.recv().await.unwrap().frame.body, b"filler");
        if complete {
            closing.as_mut().await;
        }
        drop(closing);
        assert_closed_once(&shared, &consumer, &mut rx, &route).await;
        consumer.close().await;
    }
}

#[tokio::test]
async fn route_close_cancel_route_during_writer_wait() {
    cancel_during_writer_wait(false).await;
}

#[tokio::test]
async fn route_close_cancel_handle_during_writer_wait() {
    cancel_during_writer_wait(true).await;
}

#[tokio::test]
async fn route_close_cancel_route_during_drain() {
    cancel_during_drain(false).await;
}

#[tokio::test]
async fn route_close_cancel_handle_during_drain() {
    cancel_during_drain(true).await;
}

#[tokio::test]
async fn route_close_cancel_route_when_writer_becomes_ready() {
    cancel_when_writer_becomes_ready(false).await;
}

#[tokio::test]
async fn route_close_cancel_handle_when_writer_becomes_ready() {
    cancel_when_writer_becomes_ready(true).await;
}
#[tokio::test]
async fn route_closed_terminal_survives_goodbye_for_sent_and_unsent_calls() {
    for terminal in [Some(true), Some(false), None] {
        for accepted in [false, true] {
            for channels in [true, false] {
                for reason in [RouteCloseReason::Restart, RouteCloseReason::Reload] {
                    let (shared, consumer, mut rx, _peer, _key, route) = route_teardown_stand_in();
                    rx.recv().await.unwrap();
                    shared
                        .lock_inner()
                        .route_modules
                        .insert(route.handle.channel, "provider".into());
                    let mut request = Box::pin(consumer.request(
                        &route.handle,
                        b"unanswered".to_vec(),
                        CallOptions::default(),
                    ));
                    assert_pending(request.as_mut()).await;
                    let command = rx.recv().await.unwrap();
                    if accepted {
                        assert!(shared.mark_pending_accepted(command.pending.unwrap()));
                    }
                    let wire_reason = match reason {
                        RouteCloseReason::Restart => "restart",
                        RouteCloseReason::Reload => "reload",
                        _ => unreachable!(),
                    };
                    let mut body = serde_json::json!({
                        "op": "route.closed",
                        "module_id": "provider",
                        "reason": wire_reason,
                    });
                    if let Some(terminal) = terminal {
                        body["terminal"] = serde_json::json!(terminal);
                    }
                    if channels {
                        body["channels"] = serde_json::json!([route.handle.channel]);
                    }
                    let push = Frame::build(
                        FrameType::Push,
                        Flags(0),
                        0,
                        0,
                        0,
                        serde_json::to_vec(&body).unwrap(),
                    )
                    .unwrap();
                    dispatch_frame(&shared, 1, push).await;
                    let goodbye = Frame::build(
                        FrameType::Goodbye,
                        Flags(0),
                        route.handle.channel,
                        route.handle.epoch,
                        0,
                        vec![],
                    )
                    .unwrap();
                    dispatch_frame(&shared, 1, goodbye).await;
                    let error = request.await.unwrap_err();
                    assert_eq!(error.terminal(), terminal);
                    assert_eq!(
                        error.close_reason(),
                        Some(&RouteEndReason::Daemon(reason.clone()))
                    );
                    assert_eq!(
                        error.outcome_cause(),
                        accepted.then_some(OutcomeUnknownCause::RouteEnded)
                    );
                    assert_eq!(
                        error.not_sent_cause(),
                        (!accepted).then_some(NotSentCause::WriterRejected)
                    );
                    assert_eq!(reason.disposition(), RouteCloseDisposition::MayReopen);
                    consumer.close().await;
                }
            }
        }
    }
}
