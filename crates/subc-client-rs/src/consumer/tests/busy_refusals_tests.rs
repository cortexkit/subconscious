use super::*;

struct Rig {
    shared: Arc<Shared>,
    consumer: Arc<SubcConsumer>,
    rx: mpsc::Receiver<WriteCommand>,
    route: RouteState,
    key: RouteKey,
}

fn target() -> RouteTarget {
    RouteTarget::ToolProvider {
        module_id: "busy-provider".into(),
    }
}
fn identity() -> BindIdentity {
    BindIdentity::new("test", "busy")
}

async fn assert_pending<F: Future>(mut future: Pin<&mut F>) {
    std::future::poll_fn(|cx| {
        assert!(future.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
}

impl Rig {
    fn new(total: Option<usize>, watches: Option<usize>) -> Self {
        let mut shared =
            reader_skew_tests::legacy_shared(PathBuf::from("unused"), ConsumerOptions::default());
        shared.identity_from_environment = false;
        let shared = Arc::new(shared);
        let (tx, rx) = mpsc::channel(128);
        let route = RouteState::windows(RouteHandle::new(7, 1, 1), total, watches);
        let key = RouteKey::new(&target(), &identity(), None, None);
        {
            let mut inner = shared.lock_inner();
            inner.writer = Some(tx);
            inner.daemon_ver = "0.39.0".into();
            inner.daemon_origin_trusted = true;
            inner.cache_route(key.clone(), route.clone());
        }
        let consumer = Arc::new(SubcConsumer {
            shared: Arc::clone(&shared),
        });
        Self {
            shared,
            consumer,
            rx,
            route,
            key,
        }
    }

    fn consumer(&self) -> Arc<SubcConsumer> {
        Arc::clone(&self.consumer)
    }

    fn call(&self, body: &[u8]) -> JoinHandle<Result<Vec<u8>, CallError>> {
        let consumer = self.consumer();
        let body = body.to_vec();
        tokio::spawn(async move {
            consumer
                .call(target(), identity(), body, CallOptions::default())
                .await
        })
    }

    async fn written(&mut self) -> WriteCommand {
        let command = self.rx.recv().await.unwrap();
        if let Some(key) = command.pending {
            assert!(self.shared.mark_pending_accepted(key));
        }
        command
    }

    fn terminal(&self, command: &WriteCommand, ty: FrameType, daemon: bool) {
        let terminal = match ty {
            FrameType::Error => PendingTerminal::Error {
                body: ErrorBody {
                    code: "route_busy".into(),
                    message: "full".into(),
                    detail: Some(serde_json::json!({ "reason": "no_credit" })),
                },
                flags: if daemon {
                    Flags(0).with_daemon_origin()
                } else {
                    Flags(0)
                },
            },
            FrameType::StreamEnd => PendingTerminal::StreamEnd,
            _ => PendingTerminal::Response {
                generation: 1,
                body: b"ok".to_vec(),
                flags: Flags(0),
            },
        };
        self.shared
            .settle_pending(command.pending.unwrap(), terminal);
    }
}

#[tokio::test(start_paused = true)]
async fn busy_saturated_route_poll_does_not_take_a_data_slot() {
    let mut rig = Rig::new(Some(1), None);
    let held = rig.call(b"held");
    let request = rig.written().await;
    assert_eq!(rig.route.sem.available_permits(), 0);

    let consumer = rig.consumer();
    let handle = rig.route.handle;
    let mut poll =
        Box::pin(consumer.poll_route(&handle, PollKind::Liveness, Duration::from_secs(1)));
    assert_pending(poll.as_mut()).await;
    let command = rig
        .rx
        .try_recv()
        .expect("route.poll must bypass the full data window");
    assert_eq!(command.frame.header.channel, 0);
    let key = command.pending.unwrap();
    assert!(rig.shared.mark_pending_accepted(key));
    assert!(!rig.shared.lock_inner().wire_slots.contains_key(&key));
    assert_eq!(rig.route.sem.available_permits(), 0);

    let body = serde_json::to_vec(&ClientControlResponse::RoutePoll {
        route_channel: handle.channel,
        route_epoch: handle.epoch,
        status: Some("ready".into()),
        live: Some(true),
    })
    .unwrap();
    assert!(dispatch_frame(&rig.shared, 1, response_frame(0, 0, key.corr, body)).await);
    assert_eq!(poll.await.unwrap().live, Some(true));
    assert_eq!(rig.route.sem.available_permits(), 0);
    rig.terminal(&request, FrameType::Response, false);
    assert_eq!(held.await.unwrap().unwrap(), b"ok");
}

#[tokio::test(start_paused = true)]
async fn busy_response_deadline_goodbyes_a_hung_route_without_a_terminal() {
    let mut rig = Rig::new(Some(1), None);
    let consumer = rig.consumer();
    let call = tokio::spawn(async move {
        consumer
            .call(
                target(),
                identity(),
                b"never answered".to_vec(),
                CallOptions {
                    timeout: Duration::from_millis(10),
                    ..CallOptions::default()
                },
            )
            .await
    });
    let request = rig.written().await;
    assert!(rig
        .shared
        .lock_inner()
        .wire_slots
        .contains_key(&request.pending.unwrap()));
    tokio::time::advance(Duration::from_millis(10)).await;
    assert_eq!(
        call.await.unwrap().unwrap_err().outcome_cause(),
        Some(OutcomeUnknownCause::Deadline)
    );

    let frames: Vec<_> = std::iter::from_fn(|| rig.rx.try_recv().ok())
        .map(|command| command.frame)
        .collect();
    let goodbyes: Vec<_> = frames
        .iter()
        .filter(|frame| frame.header.ty == FrameType::Goodbye)
        .collect();
    assert_eq!(
        goodbyes.len(),
        1,
        "retirement must not wait for the hung module's terminal"
    );
    assert_eq!(goodbyes[0].header.channel, rig.route.handle.channel);
    assert_eq!(goodbyes[0].header.epoch, rig.route.handle.epoch);
    let inner = rig.shared.lock_inner();
    assert!(inner.wire_slots.is_empty());
    assert!(inner.retired_routes.is_empty());
    assert!(!inner.route_epochs.contains_key(&rig.route.handle.channel));
}

#[tokio::test(start_paused = true)]
async fn busy_serial_retry_keeps_its_permit_and_submission_order() {
    let mut rig = Rig::new(Some(1), None);
    let b = rig.call(b"B");
    let first = rig.written().await;
    let c = rig.call(b"C");
    tokio::task::yield_now().await;
    rig.terminal(&first, FrameType::Error, true);
    tokio::task::yield_now().await;
    assert!(rig.rx.try_recv().is_err());
    let retry = rig.written().await;
    assert_eq!(retry.frame.body, b"B");
    assert_ne!(retry.frame.header.corr, first.frame.header.corr);
    rig.terminal(&retry, FrameType::Response, false);
    assert_eq!(b.await.unwrap().unwrap(), b"ok");
    let last = rig.written().await;
    assert_eq!(last.frame.body, b"C");
    rig.terminal(&last, FrameType::Response, false);
    c.await.unwrap().unwrap();
    assert_eq!(rig.route.sem.available_permits(), 1);
}

#[tokio::test(start_paused = true)]
async fn busy_forged_and_old_daemon_refusals_do_not_retry() {
    for opted_in in [true, false] {
        let mut rig = Rig::new(opted_in.then_some(1), None);
        let call = rig.call(b"work");
        let sent = rig.written().await;
        rig.terminal(&sent, FrameType::Error, !opted_in);
        assert!(
            matches!(call.await.unwrap(), Err(CallError::Module(body)) if body.code == "route_busy")
        );
        assert!(rig.rx.try_recv().is_err());
    }
}

#[tokio::test(start_paused = true)]
async fn busy_timeout_slot_waits_for_late_terminal_exactly_once() {
    let mut rig = Rig::new(Some(1), None);
    let consumer = rig.consumer();
    let handle = rig.route.handle;
    let first = tokio::spawn(async move {
        consumer
            .request(
                &handle,
                vec![],
                CallOptions {
                    timeout: Duration::from_millis(10),
                    ..CallOptions::default()
                },
            )
            .await
    });
    let sent = rig.written().await;
    assert!(
        first.await.unwrap().unwrap_err().outcome_cause() == Some(OutcomeUnknownCause::Deadline)
    );
    assert_eq!(rig.route.sem.available_permits(), 0);
    assert_eq!(rig.shared.lock_inner().wire_slots.len(), 1);
    // The timeout's advisory Cancel does not return daemon credit.
    while rig.rx.try_recv().is_ok() {}
    let second = rig.call(b"second");
    tokio::task::yield_now().await;
    assert!(rig.rx.try_recv().is_err());
    rig.terminal(&sent, FrameType::Response, false);
    let next = rig.written().await;
    rig.terminal(&sent, FrameType::Response, false);
    assert_eq!(rig.route.sem.available_permits(), 0);
    rig.terminal(&next, FrameType::Response, false);
    second.await.unwrap().unwrap();
    assert_eq!(rig.route.sem.available_permits(), 1);
}

#[tokio::test(start_paused = true)]
async fn busy_proven_unsent_rolls_back_but_completion_failure_keeps_slot() {
    let mut rig = Rig::new(Some(1), None);
    let consumer = rig.consumer();
    assert!(consumer
        .request(
            &rig.route.handle,
            vec![],
            CallOptions {
                timeout: Duration::ZERO,
                ..CallOptions::default()
            }
        )
        .await
        .is_err());
    assert_eq!(rig.route.sem.available_permits(), 1);
    assert!(rig.shared.lock_inner().wire_slots.is_empty());
    let handle = rig.route.handle;
    let request = tokio::spawn(async move {
        consumer
            .request(&handle, vec![], CallOptions::default())
            .await
    });
    let sent = rig.written().await;
    // A missing completion sender says nothing about a write already accepted.
    drop(
        rig.shared
            .lock_inner()
            .remove_pending(&sent.pending.unwrap()),
    );
    let error = request.await.unwrap().unwrap_err();
    assert_eq!(
        error.outcome_cause(),
        Some(OutcomeUnknownCause::CompletionFailed)
    );
    assert_eq!(error.never_forwarded(), None);
    assert_eq!(rig.route.sem.available_permits(), 0);
    rig.terminal(&sent, FrameType::StreamEnd, false);
    assert_eq!(rig.route.sem.available_permits(), 1);
}

#[tokio::test(start_paused = true)]
async fn busy_retry_replacement_acquires_the_new_routes_slot() {
    let mut rig = Rig::new(Some(1), None);
    let call = rig.call(b"work");
    let first = rig.written().await;
    rig.terminal(&first, FrameType::Error, true);
    tokio::task::yield_now().await;
    rig.shared
        .invalidate_route(&rig.key, Some(rig.route.handle));
    let new_route = RouteState::windows(RouteHandle::new(8, 2, 1), Some(1), None);
    rig.shared
        .lock_inner()
        .cache_route(rig.key.clone(), new_route.clone());
    let blocker = Arc::clone(&new_route.sem).acquire_owned().await.unwrap();
    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::task::yield_now().await;
    assert!(rig.rx.try_recv().is_err());
    assert_eq!(rig.route.sem.available_permits(), 1);
    drop(blocker);
    let retried = rig.written().await;
    assert_eq!(retried.frame.header.channel, 8);
    assert_eq!(new_route.sem.available_permits(), 0);
    rig.terminal(&retried, FrameType::Response, false);
    call.await.unwrap().unwrap();
    assert_eq!(new_route.sem.available_permits(), 1);
}

#[tokio::test(start_paused = true)]
async fn busy_watch_lane_leaves_total_capacity_for_ordinary_calls() {
    let mut rig = Rig::new(Some(32), Some(24));
    let mut subscriptions = Vec::new();
    for _ in 0..24 {
        let subscription = rig
            .consumer()
            .subscribe(
                target(),
                identity(),
                b"watch".to_vec(),
                SubscribeOptions::default(),
            )
            .await
            .unwrap();
        let sent = rig.written().await;
        assert_ne!(sent.frame.header.flags.0 & FLAG_SUBSCRIPTION, 0);
        subscriptions.push((subscription, sent));
    }
    let consumer = rig.consumer();
    let queued = tokio::spawn(async move {
        consumer
            .subscribe(
                target(),
                identity(),
                b"queued".to_vec(),
                SubscribeOptions::default(),
            )
            .await
    });
    tokio::task::yield_now().await;
    assert_eq!(rig.route.sem.available_permits(), 8);
    let ordinary = rig.call(b"ordinary");
    let sent = rig.written().await;
    assert_eq!(sent.frame.body, b"ordinary");
    rig.terminal(&sent, FrameType::Response, false);
    ordinary.await.unwrap().unwrap();
    subscriptions[0].0.unsubscribe().unwrap();
    assert_eq!(rig.route.sem.available_permits(), 8);
    rig.terminal(&subscriptions[0].1, FrameType::StreamEnd, false);
    while let Ok(cancel) = rig.rx.try_recv() {
        assert_eq!(cancel.frame.header.ty, FrameType::Cancel);
    }
    let subscription = queued.await.unwrap().unwrap();
    let sent = rig.written().await;
    assert_eq!(sent.frame.body, b"queued");
    drop(subscription);
    rig.shared.close_sync("test finished");
}

#[tokio::test(start_paused = true)]
async fn busy_subscription_retry_keeps_original_receiver_and_fresh_correlations() {
    let mut rig = Rig::new(Some(1), Some(1));
    let mut subscription = rig
        .consumer()
        .subscribe(
            target(),
            identity(),
            b"watch".to_vec(),
            SubscribeOptions::default(),
        )
        .await
        .unwrap();
    let first = rig.written().await;
    rig.terminal(&first, FrameType::Error, true);
    let retried = rig.written().await;
    assert_ne!(first.frame.header.corr, retried.frame.header.corr);
    assert_eq!(rig.route.sem.available_permits(), 0);
    rig.shared
        .route_stream_data(retried.pending.unwrap(), b"event".to_vec());
    assert_eq!(subscription.events().recv().await.unwrap(), b"event");
    // A repeated terminal for the refused correlation cannot free the new one.
    rig.terminal(&first, FrameType::Response, false);
    assert_eq!(rig.route.sem.available_permits(), 0);
    rig.terminal(&retried, FrameType::StreamEnd, false);
    subscription.closed().await.unwrap();
    assert_eq!(rig.route.sem.available_permits(), 1);
    assert_eq!(rig.route.watch_sem.as_ref().unwrap().available_permits(), 1);
}

#[tokio::test(start_paused = true)]
async fn busy_subscription_unsubscribe_or_drop_during_backoff_releases_without_cancel() {
    for drop_it in [false, true] {
        let mut rig = Rig::new(Some(1), Some(1));
        let subscription = rig
            .consumer()
            .subscribe_route(&rig.route.handle, vec![], SubscribeOptions::default())
            .await
            .unwrap();
        let first = rig.written().await;
        rig.terminal(&first, FrameType::Error, true);
        if !drop_it {
            subscription.unsubscribe().unwrap();
        }
        drop(subscription);
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert!(rig.rx.try_recv().is_err());
        assert_eq!(rig.route.sem.available_permits(), 1);
        assert_eq!(rig.route.watch_sem.as_ref().unwrap().available_permits(), 1);
    }
}

#[tokio::test(start_paused = true)]
async fn busy_subscription_close_or_replacement_during_backoff_ends_the_stream() {
    for replacement in [false, true] {
        let mut rig = Rig::new(Some(1), Some(1));
        let mut subscription = rig
            .consumer()
            .subscribe_route(&rig.route.handle, vec![], SubscribeOptions::default())
            .await
            .unwrap();
        let first = rig.written().await;
        rig.terminal(&first, FrameType::Error, true);
        if replacement {
            rig.shared
                .invalidate_route(&rig.key, Some(rig.route.handle));
            let next = RouteState::windows(RouteHandle::new(8, 2, 1), Some(1), Some(1));
            rig.shared.lock_inner().cache_route(rig.key.clone(), next);
        } else {
            rig.consumer()
                .close_handle(&rig.route.handle, CloseRouteOptions::default())
                .await
                .unwrap();
        }
        assert!(subscription.closed().await.is_err());
        assert!(matches!(
            subscription.ended().await,
            SubscriptionEnd::RouteClosed { .. }
        ));
        assert_eq!(rig.route.sem.available_permits(), 1);
        assert_eq!(rig.route.watch_sem.as_ref().unwrap().available_permits(), 1);
        while let Ok(command) = rig.rx.try_recv() {
            assert_eq!(command.frame.header.ty, FrameType::Goodbye);
        }
    }
}

#[tokio::test(start_paused = true)]
async fn busy_subscription_backpressure_keeps_slot_until_late_terminal() {
    let mut rig = Rig::new(Some(1), Some(1));
    let mut subscription = rig
        .consumer()
        .subscribe_route(
            &rig.route.handle,
            vec![],
            SubscribeOptions {
                event_buffer: 1,
                ..SubscribeOptions::default()
            },
        )
        .await
        .unwrap();
    let sent = rig.written().await;
    rig.shared.route_stream_data(sent.pending.unwrap(), vec![1]);
    rig.shared.route_stream_data(sent.pending.unwrap(), vec![2]);
    assert_eq!(subscription.ended().await, SubscriptionEnd::Backpressure);
    assert_eq!(rig.route.sem.available_permits(), 0);
    assert_eq!(rig.route.watch_sem.as_ref().unwrap().available_permits(), 0);
    rig.terminal(&sent, FrameType::StreamEnd, false);
    assert_eq!(rig.route.sem.available_permits(), 1);
    assert_eq!(rig.route.watch_sem.as_ref().unwrap().available_permits(), 1);
}

#[tokio::test(start_paused = true)]
async fn busy_terminals_and_route_close_release_slots_once() {
    for terminal in [
        FrameType::Response,
        FrameType::Error,
        FrameType::StreamEnd,
        FrameType::Goodbye,
    ] {
        let mut rig = Rig::new(Some(1), None);
        let consumer = rig.consumer();
        let handle = rig.route.handle;
        let call = tokio::spawn(async move {
            consumer
                .request(&handle, vec![], CallOptions::default())
                .await
        });
        let sent = rig.written().await;
        if terminal == FrameType::Goodbye {
            let goodbye = Frame::build(
                FrameType::Goodbye,
                Flags(0),
                handle.channel,
                handle.epoch,
                0,
                vec![],
            )
            .unwrap();
            dispatch_frame(&rig.shared, 1, goodbye.clone()).await;
            dispatch_frame(&rig.shared, 1, goodbye).await;
        } else {
            rig.terminal(&sent, terminal, false);
            rig.terminal(&sent, terminal, false);
        }
        let _ = call.await.unwrap();
        assert_eq!(rig.route.sem.available_permits(), 1);
        assert!(rig.shared.lock_inner().wire_slots.is_empty());
    }
}

#[tokio::test(start_paused = true)]
async fn busy_cancellation_during_backoff_ends_as_never_forwarded() {
    let mut rig = Rig::new(Some(1), None);
    let consumer = rig.consumer();
    let (cancel, future) =
        consumer.request_cancellable(&rig.route.handle, vec![], CallOptions::default());
    tokio::pin!(future);
    assert_pending(future.as_mut()).await;
    let sent = rig.written().await;
    rig.terminal(&sent, FrameType::Error, true);
    assert_pending(future.as_mut()).await;
    cancel.cancel().await;
    let error = future.await.unwrap_err();
    assert_eq!(error.code(), Some("route_busy"));
    assert_eq!(error.never_forwarded(), Some(true));
    assert!(rig.rx.try_recv().is_err());
    assert_eq!(rig.route.sem.available_permits(), 1);
}

#[tokio::test(start_paused = true)]
async fn busy_exhausted_call_and_subscription_are_never_forwarded() {
    for subscription in [false, true] {
        let mut rig = Rig::new(Some(1), Some(1));
        let consumer = rig.consumer();
        let handle = rig.route.handle;
        let result = tokio::spawn(async move {
            if subscription {
                let mut stream = consumer
                    .subscribe_route(
                        &handle,
                        vec![],
                        SubscribeOptions {
                            route_open_timeout: Duration::from_secs(1),
                            ..SubscribeOptions::default()
                        },
                    )
                    .await?;
                stream.closed().await
            } else {
                consumer
                    .request(
                        &handle,
                        vec![],
                        CallOptions {
                            timeout: Duration::from_secs(1),
                            ..CallOptions::default()
                        },
                    )
                    .await
                    .map(|_| ())
            }
        });
        let start = Instant::now();
        while !result.is_finished() {
            tokio::select! {
                biased;
                () = tokio::time::sleep_until(start + Duration::from_secs(2)) => break,
                command = rig.rx.recv() => {
                    let command = command.unwrap();
                    if let Some(key) = command.pending { assert!(rig.shared.mark_pending_accepted(key)); }
                    rig.terminal(&command, FrameType::Error, true);
                }
            }
        }
        let error = result.await.unwrap().unwrap_err();
        assert_eq!(error.code(), Some("route_busy"));
        assert_eq!(error.never_forwarded(), Some(true));
        assert!(Instant::now() - start >= Duration::from_secs(1));
        assert_eq!(rig.route.sem.available_permits(), 1);
    }
}

#[tokio::test(start_paused = true)]
async fn busy_forwarded_retry_lost_stays_outcome_unknown() {
    let mut rig = Rig::new(Some(1), None);
    let call = rig.call(b"work");
    let first = rig.written().await;
    rig.terminal(&first, FrameType::Error, true);
    let _retry = rig.written().await;
    rig.shared.handle_generation_drop(1, "lost".into());
    assert_eq!(
        call.await.unwrap().unwrap_err().outcome_cause(),
        Some(OutcomeUnknownCause::ConnectionFailed)
    );
    assert_eq!(rig.route.sem.available_permits(), 1);
}

#[tokio::test(start_paused = true)]
async fn busy_reservation_drop_retains_only_written_credit() {
    for written in [false, true] {
        let mut rig = Rig::new(Some(1), None);
        let consumer = rig.consumer();
        let reservation = consumer
            .reserve(&rig.route.handle, Instant::now() + Duration::from_secs(1))
            .await
            .unwrap();
        if written {
            let (_, reply) =
                reservation.send(RequestBody::new(vec![]).unwrap(), CallOptions::default());
            let sent = rig.written().await;
            drop(reply);
            assert_eq!(rig.route.sem.available_permits(), 0);
            rig.terminal(&sent, FrameType::Response, false);
        } else {
            drop(reservation);
        }
        assert_eq!(rig.route.sem.available_permits(), 1);
        assert!(rig.shared.lock_inner().wire_slots.is_empty());
    }
}

#[tokio::test(start_paused = true)]
async fn busy_route_open_opts_in_and_adopts_windows_before_calls() {
    let mut rig = Rig::new(None, None);
    rig.shared.lock_inner().remove_route(&rig.key);
    let consumer = rig.consumer();
    let opening = tokio::spawn(async move {
        consumer
            .open_route(target(), identity(), CallOptions::default())
            .await
    });
    let request = rig.written().await;
    let body: serde_json::Value = serde_json::from_slice(&request.frame.body).unwrap();
    assert_eq!(body["busy_refusals"], true);
    let response = Frame::build(FrameType::Response, Flags(0), 0, 0, request.frame.header.corr,
        serde_json::to_vec(&serde_json::json!({ "op": "route.open", "route_channel": 8, "route_epoch": 2, "request_window": 32, "watch_window": 24 })).unwrap()).unwrap();
    dispatch_frame(&rig.shared, 1, response).await;
    let handle = opening.await.unwrap().unwrap();
    let route = rig.shared.route_state(handle).unwrap();
    assert!(route.opted_in);
    assert_eq!(route.sem.available_permits(), 32);
    assert_eq!(route.watch_sem.unwrap().available_permits(), 24);
}

#[tokio::test(start_paused = true)]
async fn busy_old_daemon_keeps_the_existing_large_local_window() {
    let mut rig = Rig::new(None, None);
    let calls = (0..34).map(|_| rig.call(b"work")).collect::<Vec<_>>();
    let mut requests = Vec::new();
    for _ in 0..34 {
        requests.push(rig.written().await);
    }
    assert!(rig.shared.lock_inner().wire_slots.is_empty());
    for request in requests {
        rig.terminal(&request, FrameType::Response, false);
    }
    for call in calls {
        call.await.unwrap().unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn busy_waiting_cancel_wins_when_capacity_becomes_ready() {
    let mut rig = Rig::new(Some(1), None);
    let first = rig.call(b"first");
    let sent = rig.written().await;
    let consumer = rig.consumer();
    let (cancel, future) =
        consumer.request_cancellable(&rig.route.handle, vec![], CallOptions::default());
    tokio::pin!(future);
    assert_pending(future.as_mut()).await;
    assert_eq!(cancel.cancel().await, CancelOutcome::Requested);
    rig.terminal(&sent, FrameType::Response, false);
    first.await.unwrap().unwrap();
    assert_eq!(future.await.unwrap_err().never_forwarded(), Some(true));
    assert!(rig.rx.try_recv().is_err());
    assert_eq!(rig.route.sem.available_permits(), 1);
}

#[tokio::test(start_paused = true)]
async fn busy_slot_wait_cancellation_and_deadline_never_write() {
    let mut rig = Rig::new(Some(1), None);
    let first = rig.call(b"first");
    let sent = rig.written().await;
    let consumer = rig.consumer();
    let (cancel, future) =
        consumer.request_cancellable(&rig.route.handle, vec![], CallOptions::default());
    tokio::pin!(future);
    assert_pending(future.as_mut()).await;
    cancel.cancel().await;
    assert_eq!(future.await.unwrap_err().never_forwarded(), Some(true));
    assert_eq!(
        consumer
            .request(
                &rig.route.handle,
                vec![],
                CallOptions {
                    timeout: Duration::from_millis(10),
                    ..CallOptions::default()
                }
            )
            .await
            .unwrap_err()
            .never_forwarded(),
        Some(true)
    );
    assert!(rig.rx.try_recv().is_err());
    assert_eq!(rig.shared.lock_inner().wire_slots.len(), 1);
    rig.terminal(&sent, FrameType::Response, false);
    first.await.unwrap().unwrap();
    assert_eq!(rig.route.sem.available_permits(), 1);
}

#[tokio::test(start_paused = true)]
async fn busy_total_window_queues_33rd_without_blocking_another_route() {
    let mut rig = Rig::new(Some(32), None);
    let calls = (0..32).map(|_| rig.call(b"held")).collect::<Vec<_>>();
    let mut requests = Vec::new();
    for _ in 0..32 {
        requests.push(rig.written().await);
    }
    let queued = rig.call(b"queued");
    tokio::task::yield_now().await;
    assert!(rig.rx.try_recv().is_err());
    let other = RouteState::windows(RouteHandle::new(9, 1, 1), Some(1), None);
    rig.shared.install_one_shot_route(other.clone()).unwrap();
    let consumer = rig.consumer();
    let sibling = tokio::spawn(async move {
        consumer
            .request(&other.handle, b"sibling".to_vec(), CallOptions::default())
            .await
    });
    let sent = rig.written().await;
    assert_eq!(sent.frame.header.channel, 9);
    rig.terminal(&sent, FrameType::Response, false);
    sibling.await.unwrap().unwrap();
    rig.terminal(&requests[0], FrameType::Response, false);
    let next = rig.written().await;
    assert_eq!(next.frame.body, b"queued");
    rig.terminal(&next, FrameType::Response, false);
    for request in requests.into_iter().skip(1) {
        rig.terminal(&request, FrameType::Response, false);
    }
    for call in calls {
        call.await.unwrap().unwrap();
    }
    queued.await.unwrap().unwrap();
    assert_eq!(rig.route.sem.available_permits(), 32);
}
