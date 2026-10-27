use super::*;

fn assert_not_sent(err: &CallError, cause: NotSentCause, never_forwarded: bool) {
    assert!(matches!(err, CallError::NotSent(_)), "{err:?}");
    assert_eq!(err.not_sent_cause(), Some(cause));
    assert_eq!(err.never_forwarded(), Some(never_forwarded));
}

async fn refuse_request(shared: &Arc<Shared>, command: WriteCommand, code: &str, marked: bool) {
    assert!(shared.mark_pending_accepted(command.pending.unwrap()));
    let request = command.frame.header;
    let flags = if marked {
        request.flags.with_daemon_origin()
    } else {
        request.flags.without_daemon_origin()
    };
    let frame = Frame::build(
        FrameType::Error,
        flags,
        request.channel,
        request.epoch,
        request.corr,
        serde_json::to_vec(&ErrorBody::new(code, "refused")).unwrap(),
    )
    .unwrap();
    assert!(dispatch_frame(shared, 1, frame).await);
}

// Keep the queued frame unread: the next request must wait for writer capacity.
fn fill_writer(shared: &Arc<Shared>) -> mpsc::Receiver<WriteCommand> {
    let (writer, receiver) = mpsc::channel(1);
    writer
        .try_send(WriteCommand {
            frame: Frame::build(FrameType::Ping, Flags(0), 0, 0, 0, Vec::new()).unwrap(),
            pending: None,
        })
        .unwrap();
    shared.lock_inner().writer = Some(writer);
    receiver
}

#[tokio::test(start_paused = true)]
async fn marked_and_unmarked_retry_then_writer_timeout_preserves_whole_call_bit() {
    for code in [
        error_codes::MODULE_RELOADING,
        error_codes::UNKNOWN_CHANNEL,
        error_codes::STALE_ROUTE_EPOCH,
    ] {
        for marked in [true, false] {
            for cancellable in [false, true] {
                let (shared, consumer, mut receiver) = route_open_stand_in(seeded_random(1));
                let (_cancel, call) = consumer.call_cancellable(
                    restart_target("plexus"),
                    restart_identity(),
                    b"ask".to_vec(),
                    CallOptions {
                        timeout: Duration::from_secs(5),
                        ..CallOptions::default()
                    },
                );
                let mut call = Box::pin(call);
                let ordinary = drain_call(&consumer, b"ask");
                let mut ordinary = Box::pin(ordinary);
                let mut future: Pin<&mut dyn Future<Output = Result<Vec<u8>, CallError>>> =
                    if cancellable {
                        call.as_mut()
                    } else {
                        ordinary.as_mut()
                    };
                assert_pending(future.as_mut()).await;
                let (corr, _) = next_route_open(&mut receiver, "first attempt").await;
                assert!(dispatch_frame(&shared, 1, route_open_answer(corr, 41, None)).await);
                assert_pending(future.as_mut()).await;
                refuse_request(&shared, receiver.recv().await.unwrap(), code, marked).await;
                assert_pending(future.as_mut()).await;
                let (corr, _) = next_route_open(&mut receiver, "resend").await;
                assert!(dispatch_frame(&shared, 1, route_open_answer(corr, 42, None)).await);
                let mut full_writer = fill_writer(&shared);
                assert_pending(future.as_mut()).await;
                tokio::time::advance(Duration::from_secs(5)).await;
                let err = future.await.unwrap_err();
                assert_not_sent(&err, NotSentCause::WriterCapacity, marked);
                assert_eq!(
                    full_writer.recv().await.unwrap().frame.header.ty,
                    FrameType::Ping
                );
                assert!(
                    full_writer.try_recv().is_err(),
                    "timed-out send queued a request"
                );
                assert!(shared.lock_inner().pending.is_empty());
            }
        }
    }
}

#[tokio::test(start_paused = true)]
async fn final_marked_refusal_is_not_sent_but_unmarked_body_remains_module() {
    for code in [
        error_codes::MODULE_RELOADING,
        error_codes::UNKNOWN_CHANNEL,
        error_codes::STALE_ROUTE_EPOCH,
    ] {
        for first_marked in [true, false] {
            for final_marked in [true, false] {
                // These calls are polled by hand. Start each scenario with a
                // fresh Tokio cooperative budget so queueing need not yield.
                tokio::task::yield_now().await;
                let (shared, consumer, mut receiver) = route_open_stand_in(seeded_random(1));
                let mut call = Box::pin(drain_call(&consumer, b"ask"));
                assert_pending(call.as_mut()).await;
                let (corr, _) = next_route_open(&mut receiver, "first attempt").await;
                assert!(dispatch_frame(&shared, 1, route_open_answer(corr, 41, None)).await);
                assert_pending(call.as_mut()).await;
                refuse_request(&shared, receiver.recv().await.unwrap(), code, first_marked).await;
                assert_pending(call.as_mut()).await;
                let (corr, _) = next_route_open(&mut receiver, "last attempt").await;
                assert!(dispatch_frame(&shared, 1, route_open_answer(corr, 42, None)).await);
                assert_pending(call.as_mut()).await;
                refuse_request(&shared, receiver.recv().await.unwrap(), code, final_marked).await;
                let err = call.await.unwrap_err();
                if final_marked {
                    assert_not_sent(
                        &err,
                        NotSentCause::DaemonRefusedBeforeForward { code: code.into() },
                        first_marked,
                    );
                } else {
                    let CallError::Module(body) = &err else {
                        panic!("{err:?}");
                    };
                    assert_eq!(body.code, code);
                    assert_eq!(body.message, "refused");
                    assert_eq!(err.not_sent_cause(), None);
                    assert_eq!(err.never_forwarded(), None);
                }
                assert!(receiver.try_recv().is_err(), "call resent more than once");
            }
        }
    }
}

#[tokio::test(start_paused = true)]
async fn retry_then_route_open_failure_keeps_the_whole_call_bit_and_refusal() {
    for marked in [true, false] {
        let (shared, consumer, mut receiver) = route_open_stand_in(seeded_random(1));
        let mut call = Box::pin(drain_call(&consumer, b"ask"));
        assert_pending(call.as_mut()).await;
        let (corr, _) = next_route_open(&mut receiver, "first route").await;
        assert!(dispatch_frame(&shared, 1, route_open_answer(corr, 41, None)).await);
        assert_pending(call.as_mut()).await;
        refuse_request(
            &shared,
            receiver.recv().await.unwrap(),
            error_codes::MODULE_RELOADING,
            marked,
        )
        .await;
        assert_pending(call.as_mut()).await;
        let (corr, _) = next_route_open(&mut receiver, "refused replacement").await;
        let body = ErrorBody::new(error_codes::MODULE_REMOVED, "gone");
        assert!(dispatch_frame(&shared, 1, route_open_answer(corr, 42, Some(body))).await);
        let err = call.await.unwrap_err();
        assert_not_sent(
            &err,
            NotSentCause::RouteOpenRefused {
                code: error_codes::MODULE_REMOVED.into(),
            },
            marked,
        );
        assert_eq!(err.route_open_refusal().unwrap().message, "gone");
    }
}

#[tokio::test(start_paused = true)]
async fn dropped_open_leader_gives_waiters_a_typed_abandoned_cause() {
    let (shared, consumer, mut receiver) = route_open_stand_in(seeded_random(1));
    let mut leader = Box::pin(drain_call(&consumer, b"leader"));
    let mut waiter = Box::pin(drain_call(&consumer, b"waiter"));
    assert_pending(leader.as_mut()).await;
    let _ = next_route_open(&mut receiver, "abandoned leader").await;
    assert_pending(waiter.as_mut()).await;
    drop(leader);
    let err = waiter.await.unwrap_err();
    assert_not_sent(&err, NotSentCause::RouteOpenAbandoned, true);
    assert!(shared.lock_inner().openings.is_empty());
}

#[tokio::test(start_paused = true)]
async fn prequeue_validation_and_closed_consumer_are_typed() {
    let (shared, consumer, mut receiver) = route_open_stand_in(seeded_random(1));
    let (_, handle) =
        open_answered(&shared, &consumer, &mut receiver, None, 41, "validation").await;
    let err = consumer
        .request(
            &handle,
            vec![0; subc_protocol::MAX_FRAME_BODY_LEN as usize + 1],
            CallOptions::default(),
        )
        .await
        .unwrap_err();
    assert_not_sent(&err, NotSentCause::BeforeQueue, true);
    assert!(receiver.try_recv().is_err());
    consumer.close().await;
    let err = drain_call(&consumer, b"closed").await.unwrap_err();
    assert_not_sent(&err, NotSentCause::BeforeQueue, true);
}

#[tokio::test(start_paused = true)]
async fn sent_call_losing_route_remains_outcome_unknown_with_close_reason() {
    let (shared, consumer, mut receiver) = route_open_stand_in(seeded_random(1));
    let (_, handle) =
        open_answered(&shared, &consumer, &mut receiver, None, 41, "route loss").await;
    let mut call = Box::pin(drain_call(&consumer, b"sent"));
    assert_pending(call.as_mut()).await;
    let command = receiver.recv().await.unwrap();
    assert!(shared.mark_pending_accepted(command.pending.unwrap()));
    shared.fail_channel_pending(
        handle,
        "route lost",
        RouteEndReason::Daemon(RouteCloseReason::Restart),
    );
    let err = call.await.unwrap_err();
    // outcome_cause() is Some only for OutcomeUnknown, so this also pins the
    // variant without adding a spelling to the source inventory test.
    assert_eq!(err.outcome_cause(), Some(OutcomeUnknownCause::RouteEnded));
    assert_eq!(
        err.close_reason(),
        Some(&RouteEndReason::Daemon(RouteCloseReason::Restart))
    );
    assert_eq!(err.not_sent_cause(), None);
    assert_eq!(err.never_forwarded(), None);
}

#[test]
fn source_round_trips_preserve_causes_and_exclude_external_errors() {
    let refusal = CallError::route_open_refused("plexus".into(), ErrorBody::new("refused", "no"));
    let shared = SharedCallFailure::from(refusal);
    for failure in [shared.clone(), shared] {
        let err = failure.into_call_error();
        assert_not_sent(
            &err,
            NotSentCause::RouteOpenRefused {
                code: "refused".into(),
            },
            true,
        );
        assert_eq!(err.route_open_refusal().unwrap().message, "no");
    }
    let err = SharedCallFailure::from(CallError::StaleRouteHandle(RouteHandle::detached(41, 1)))
        .into_call_error();
    assert_not_sent(&err, NotSentCause::BeforeQueue, true);
    let external = CallError::NotSent(Box::new(SimpleError("external".into())));
    assert_eq!(external.not_sent_cause(), None);
    assert_eq!(external.never_forwarded(), None);
    let err = CallError::invalid_response("invalid control reply");
    assert_not_sent(&err, NotSentCause::InvalidResponse, false);
}

#[tokio::test]
async fn unwritten_pending_route_loss_preserves_cause_and_close_reason() {
    let (tx, rx) = oneshot::channel();
    let entry = PendingEntry::unary(tx, false, None, None);
    settle_route_pending_entries(vec![entry], "closed".into(), RouteEndReason::ClosedByCaller);
    let err = rx.await.unwrap().into_call_result().unwrap_err();
    assert_not_sent(&err, NotSentCause::WriterRejected, true);
    assert_eq!(err.close_reason(), Some(&RouteEndReason::ClosedByCaller));
}

#[tokio::test(start_paused = true)]
async fn closed_flow_control_and_completion_channel_have_typed_sources() {
    let (shared, consumer, mut receiver) = route_open_stand_in(seeded_random(1));
    let (_, _handle) = open_answered(
        &shared,
        &consumer,
        &mut receiver,
        None,
        41,
        "closed flow control",
    )
    .await;
    shared
        .lock_inner()
        .routes
        .values()
        .next()
        .unwrap()
        .sem
        .close();
    let err = drain_call(&consumer, b"closed route").await.unwrap_err();
    assert_not_sent(&err, NotSentCause::BeforeQueue, true);
    assert_eq!(err.close_reason(), Some(&RouteEndReason::Unknown));

    let key = PendingKey {
        generation: 1,
        channel: 41,
        epoch: 1,
        corr: 999,
    };
    let registration = PendingRegistration::new(Arc::clone(&shared), key, false);
    let (tx, rx) = oneshot::channel();
    drop(tx);
    let err = shared
        .wait_for_reply(registration, rx, Instant::now() + Duration::from_secs(5))
        .await
        .unwrap_err();
    assert_not_sent(&err, NotSentCause::CompletionFailed, false);
}

#[tokio::test(start_paused = true)]
async fn malformed_control_response_is_typed_but_not_proven_unforwarded() {
    let (shared, consumer, mut receiver) = route_open_stand_in(seeded_random(1));
    let mut call = Box::pin(consumer.catalog_list());
    assert_pending(call.as_mut()).await;
    let command = receiver.recv().await.unwrap();
    assert!(shared.mark_pending_accepted(command.pending.unwrap()));
    let frame = response_frame(0, 0, command.frame.header.corr, b"invalid json".to_vec());
    assert!(dispatch_frame(&shared, 1, frame).await);
    assert_not_sent(
        &call.await.unwrap_err(),
        NotSentCause::InvalidResponse,
        false,
    );
}
