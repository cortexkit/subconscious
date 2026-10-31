use super::*;

#[test]
fn daemon_origin_version_gate_fails_closed() {
    for daemon_ver in ["0.37.15", "0.38.0", "1.0.0"] {
        assert!(trusts_daemon_origin(daemon_ver), "{daemon_ver}");
    }
    for daemon_ver in [
        "0.37.14",
        "0.37.15-rc1",
        "0.38.0-rc1",
        "0.37.15+x",
        "0.37.15.1",
        "1.0",
        "",
        "test",
        " 0.37.15",
        "0.37.15 ",
        "+0.37.15",
        "0..15",
        "0.37.１５",
        "99999999999999999999.0.0",
    ] {
        assert!(!trusts_daemon_origin(daemon_ver), "{daemon_ver}");
    }
}

#[tokio::test]
async fn installed_generation_uses_authenticated_connection_file_version() {
    use subc_transport::{
        authenticate_server, generate_daemon_id, generate_key, ConnectionInfo, Endpoint,
        SCHEMA_VERSION,
    };
    use tokio::net::TcpListener;

    let shared = writer_test_shared();
    let mut servers = Vec::new();
    for (index, (daemon_ver, trusted)) in [
        ("0.37.15", true),
        ("0.37.14", false),
        ("test", false),
        ("0.38.0", true),
    ]
    .into_iter()
    .enumerate()
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let conn = ConnectionInfo {
            schema: SCHEMA_VERSION,
            wire_version: None,
            endpoints: vec![Endpoint {
                host: "127.0.0.1".into(),
                port: listener.local_addr().unwrap().port(),
            }],
            key: generate_key().unwrap(),
            daemon_id: generate_daemon_id().unwrap(),
            pid: std::process::id(),
            daemon_ver: daemon_ver.into(),
        };
        let server_conn = conn.clone();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            // The proof version is deliberately different: authentication binds
            // the connection file's daemon_id, but does not return its version.
            authenticate_server(
                &mut stream,
                &server_conn.key,
                &server_conn.daemon_id,
                "different-handshake-version",
                Duration::from_secs(5),
            )
            .await
            .unwrap();
            stream
        });
        let opened = open_connection_with_info(
            Path::new("unused-connection-file"),
            &conn,
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        servers.push(server.await.unwrap());
        let kind = if index == 0 {
            InstallKind::Initial
        } else {
            InstallKind::Reconnect
        };
        let (generation, _) = shared.install_connection(opened, kind).unwrap();
        let inner = shared.lock_inner();
        assert_eq!(inner.generation, generation);
        assert_eq!(generation, index as u64 + 1);
        assert_eq!(inner.daemon_origin_trusted, trusted, "{daemon_ver}");
        assert_eq!(inner.daemon_ver, daemon_ver);
    }
    shared.close_sync("test complete");
}

#[tokio::test]
async fn old_authenticated_daemon_refuses_route_opens_locally_without_sending() {
    use subc_transport::{
        authenticate_server, generate_daemon_id, generate_key, ConnectionInfo, Endpoint,
        SCHEMA_VERSION,
    };
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let conn = ConnectionInfo {
        schema: SCHEMA_VERSION,
        wire_version: None,
        endpoints: vec![Endpoint {
            host: "127.0.0.1".into(),
            port: listener.local_addr().unwrap().port(),
        }],
        key: generate_key().unwrap(),
        daemon_id: generate_daemon_id().unwrap(),
        pid: std::process::id(),
        daemon_ver: "0.38.10".into(),
    };
    let server_conn = conn.clone();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        // Use the connection-file version only after the handshake proves
        // that file's daemon id.
        authenticate_server(
            &mut stream,
            &server_conn.key,
            &server_conn.daemon_id,
            "99.0.0",
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        stream
    });
    let opened = open_connection_with_info(Path::new("unused"), &conn, Duration::from_secs(5))
        .await
        .unwrap();
    let mut server_stream = server.await.unwrap();
    let shared = writer_test_shared();
    shared.install_initial(opened).unwrap();
    let consumer = SubcConsumer {
        shared: Arc::clone(&shared),
    };
    let opts = CallOptions {
        timeout: Duration::from_millis(200),
        ..CallOptions::default()
    };
    let ordinary = consumer
        .open_route(restart_target("provider"), restart_identity(), opts.clone())
        .await;
    let admitted = consumer
        .open_route_with_admission_facts_and_options(
            restart_target("provider"),
            restart_identity(),
            serde_json::json!({}),
            opts,
        )
        .await;
    for err in [ordinary.unwrap_err(), admitted.unwrap_err()] {
        assert!(
            matches!(&err, CallError::DaemonTooOld { daemon_ver } if daemon_ver == "0.38.10"),
            "{err:?}"
        );
        assert_eq!(err.code(), Some("daemon_too_old"));
        assert!(err.to_string().contains("0.39.0 or newer"));
    }
    assert!(shared.lock_inner().pending.is_empty());
    assert!(
        tokio::time::timeout(Duration::from_millis(30), read_frame(&mut server_stream))
            .await
            .is_err(),
        "a locally refused route.open must never reach the daemon"
    );
    consumer.close().await;
}

#[test]
fn project_free_identity_version_gate_fails_closed() {
    for version in ["0.39.0", "0.39.1", "1.0.0"] {
        assert!(daemon_version_at_least(version, (0, 39, 0)), "{version}");
    }
    for version in [
        "0.38.10",
        "0.39.0-rc1",
        "0.39.0+build",
        "test",
        "",
        "0.39",
        "0.39.0.1",
    ] {
        assert!(!daemon_version_at_least(version, (0, 39, 0)), "{version}");
    }
}

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
    shared.lock_inner().daemon_ver = "0.39.0".into();
    receiver
}

#[tokio::test(start_paused = true)]
async fn marked_and_unmarked_retry_then_writer_timeout_preserves_whole_call_bit() {
    for code in [
        error_codes::MODULE_RELOADING,
        error_codes::UNKNOWN_CHANNEL,
        error_codes::STALE_ROUTE_EPOCH,
    ] {
        for (daemon_ver, trusted) in [("0.37.14", false), ("0.37.15", true), ("test", false)] {
            for marked in [true, false] {
                for cancellable in [false, true] {
                    let err =
                        retry_then_writer_timeout(daemon_ver, code, marked, cancellable).await;
                    assert_not_sent(&err, NotSentCause::WriterCapacity, trusted && marked);
                }
            }
        }
    }
}

#[tokio::test(start_paused = true)]
async fn old_daemon_marked_refusal_then_writer_timeout_is_not_proven_unforwarded() {
    let err =
        retry_then_writer_timeout("0.37.14", error_codes::MODULE_RELOADING, true, false).await;
    assert_not_sent(&err, NotSentCause::WriterCapacity, false);
}

async fn retry_then_writer_timeout(
    daemon_ver: &str,
    code: &str,
    marked: bool,
    cancellable: bool,
) -> CallError {
    let (shared, consumer, mut receiver) = route_open_stand_in(seeded_random(1));
    shared.lock_inner().daemon_origin_trusted = trusts_daemon_origin(daemon_ver);
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
    let mut future: Pin<&mut dyn Future<Output = Result<Vec<u8>, CallError>>> = if cancellable {
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
    assert_eq!(
        full_writer.recv().await.unwrap().frame.header.ty,
        FrameType::Ping
    );
    assert!(
        full_writer.try_recv().is_err(),
        "timed-out send queued a request"
    );
    assert!(shared.lock_inner().pending.is_empty());
    err
}

#[tokio::test(start_paused = true)]
async fn final_marked_refusal_is_not_sent_but_unmarked_body_remains_module() {
    for code in [
        error_codes::MODULE_RELOADING,
        error_codes::UNKNOWN_CHANNEL,
        error_codes::STALE_ROUTE_EPOCH,
    ] {
        for (daemon_ver, trusted) in [("0.37.14", false), ("0.37.15", true), ("test", false)] {
            for first_marked in [true, false] {
                for final_marked in [true, false] {
                    // These calls are polled by hand. Start each scenario with a
                    // fresh Tokio cooperative budget so queueing need not yield.
                    tokio::task::yield_now().await;
                    let (shared, consumer, mut receiver) = route_open_stand_in(seeded_random(1));
                    shared.lock_inner().daemon_origin_trusted = trusts_daemon_origin(daemon_ver);
                    let mut call = Box::pin(drain_call(&consumer, b"ask"));
                    assert_pending(call.as_mut()).await;
                    let (corr, _) = next_route_open(&mut receiver, "first attempt").await;
                    assert!(dispatch_frame(&shared, 1, route_open_answer(corr, 41, None)).await);
                    assert_pending(call.as_mut()).await;
                    refuse_request(&shared, receiver.recv().await.unwrap(), code, first_marked)
                        .await;
                    assert_pending(call.as_mut()).await;
                    let (corr, _) = next_route_open(&mut receiver, "last attempt").await;
                    assert!(dispatch_frame(&shared, 1, route_open_answer(corr, 42, None)).await);
                    assert_pending(call.as_mut()).await;
                    refuse_request(&shared, receiver.recv().await.unwrap(), code, final_marked)
                        .await;
                    let err = call.await.unwrap_err();
                    if trusted && final_marked {
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
}

#[tokio::test(start_paused = true)]
async fn retry_then_route_open_failure_keeps_the_whole_call_bit_and_refusal() {
    for marked in [true, false] {
        let (shared, consumer, mut receiver) = route_open_stand_in(seeded_random(1));
        shared.lock_inner().daemon_origin_trusted = trusts_daemon_origin("0.37.15");
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
async fn stale_handle_after_refusal_preserves_whole_call_history() {
    for earlier_marked in [None, Some(false), Some(true)] {
        for cancellable in [false, true] {
            let (shared, consumer, mut receiver) = route_open_stand_in(seeded_random(1));
            shared.lock_inner().daemon_origin_trusted = trusts_daemon_origin("0.37.15");
            let (_cancel, call) = consumer.call_cancellable(
                restart_target("plexus"),
                restart_identity(),
                b"ask".to_vec(),
                CallOptions::default(),
            );
            let mut call = Box::pin(call);
            let mut ordinary = Box::pin(drain_call(&consumer, b"ask"));
            let mut future: Pin<&mut dyn Future<Output = Result<Vec<u8>, CallError>>> =
                if cancellable {
                    call.as_mut()
                } else {
                    ordinary.as_mut()
                };
            assert_pending(future.as_mut()).await;
            let (corr, _) = next_route_open(&mut receiver, "first route").await;
            if let Some(marked) = earlier_marked {
                assert!(dispatch_frame(&shared, 1, route_open_answer(corr, 41, None)).await);
                assert_pending(future.as_mut()).await;
                refuse_request(
                    &shared,
                    receiver.recv().await.unwrap(),
                    error_codes::MODULE_RELOADING,
                    marked,
                )
                .await;
                assert_pending(future.as_mut()).await;
            }
            let corr = if earlier_marked.is_some() {
                next_route_open(&mut receiver, "replacement route").await.0
            } else {
                corr
            };
            assert!(dispatch_frame(&shared, 1, route_open_answer(corr, 42, None)).await);
            let handle = shared.ingress_handle(1, 42, 1).unwrap();
            // Retire the handle while route.open's reply is ready but before
            // the managed call resumes and tries to queue its request.
            shared.lock_inner().retired_routes.insert(handle);
            let err = future.await.unwrap_err();
            if let Some(marked) = earlier_marked {
                assert_not_sent(&err, NotSentCause::BeforeQueue, marked);
                assert!(err
                    .to_string()
                    .contains(&CallError::StaleRouteHandle(handle).to_string()));
            } else {
                assert!(matches!(err, CallError::StaleRouteHandle(stale) if stale == handle));
                assert_eq!(err.never_forwarded(), None);
            }
            assert!(
                receiver.try_recv().is_err(),
                "stale attempt queued a request"
            );
        }
    }
}

#[tokio::test(start_paused = true)]
async fn writer_capacity_timeouts_classify_pending_acceptance() {
    for subscription in [false, true] {
        for accepted in [false, true] {
            let shared = writer_test_shared();
            let mut receiver = fill_writer(&shared);
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut future: Pin<Box<dyn Future<Output = Result<(), CallError>>>> = if subscription {
                let permit = Arc::new(Semaphore::new(1)).acquire_owned().await.unwrap();
                Box::pin(async {
                    shared
                        .send_subscription(SubscriptionSend {
                            expected_handle: None,
                            channel: 0,
                            epoch: 0,
                            body: b"watch".to_vec(),
                            priority: Priority::Interactive,
                            admission_class: AdmissionClass::Normal,
                            event_buffer: 1,
                            deadline,
                            permit,
                        })
                        .await
                        .map(|_| ())
                })
            } else {
                Box::pin(async {
                    shared
                        .send_request(RequestSend {
                            expected_handle: None,
                            channel: 0,
                            epoch: 0,
                            body: b"ask".to_vec(),
                            binary: false,
                            priority: Priority::Interactive,
                            admission_class: AdmissionClass::Normal,
                            deadline,
                            retain_late_route_open: false,
                            route_open_reverse_requests: None,
                        })
                        .await
                        .map(|_| ())
                })
            };
            assert_pending(future.as_mut()).await;
            let key = *shared.lock_inner().pending.keys().next().unwrap();
            if accepted {
                // Acceptance cannot normally precede queueing; force it here
                // to verify timeout classification depends on the stored flag.
                assert!(shared.mark_pending_accepted(key));
            }
            tokio::time::advance(Duration::from_secs(5)).await;
            let err = future.await.unwrap_err();
            if accepted {
                assert_eq!(err.outcome_cause(), Some(OutcomeUnknownCause::Deadline));
                assert_eq!(err.not_sent_cause(), None);
            } else {
                assert_not_sent(&err, NotSentCause::WriterCapacity, true);
            }
            assert!(shared.lock_inner().pending.is_empty());
            assert_eq!(
                receiver.recv().await.unwrap().frame.header.ty,
                FrameType::Ping
            );
            assert!(receiver.try_recv().is_err());
        }
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

async fn removed_accepted_pending_error(retain_late_route_open: bool, close: bool) -> CallError {
    let shared = writer_test_shared();
    let (writer, mut receiver) = mpsc::channel(1);
    shared.lock_inner().writer = Some(writer);
    shared.lock_inner().daemon_ver = "0.39.0".into();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut request = Box::pin(shared.send_request(RequestSend {
        expected_handle: None,
        channel: 0,
        epoch: 0,
        body: b"ask".to_vec(),
        binary: false,
        priority: Priority::Interactive,
        admission_class: AdmissionClass::Normal,
        deadline,
        retain_late_route_open,
        route_open_reverse_requests: None,
    }));
    assert_pending(request.as_mut()).await;
    let key = receiver.recv().await.unwrap().pending.unwrap();
    assert!(shared.mark_pending_accepted(key));
    // Hold the completion sender in the gap where settle_pending or close_sync has removed
    // the entry under the lock but not yet settled it.
    let entry = shared.lock_inner().remove_pending(&key).unwrap();
    assert!(!entry.completion_is_closed());
    if close {
        shared.close_token.cancel();
        assert!(Instant::now() < deadline);
    } else {
        tokio::time::advance(Duration::from_secs(5)).await;
    }
    let err = request.await.unwrap_err();
    assert!(entry.completion_is_closed());
    entry.settle_failure("settlement resumed after caller completed".to_string());
    shared.close_sync("test complete");
    err
}

#[tokio::test(start_paused = true)]
async fn removed_accepted_pending_deadline_is_outcome_unknown() {
    for retain_late_route_open in [false, true] {
        let err = removed_accepted_pending_error(retain_late_route_open, false).await;
        assert_eq!(
            err.outcome_cause(),
            Some(OutcomeUnknownCause::Deadline),
            "{err:?}"
        );
        assert_ne!(err.never_forwarded(), Some(true));
    }
}

#[tokio::test(start_paused = true)]
async fn removed_accepted_pending_close_is_outcome_unknown() {
    for retain_late_route_open in [false, true] {
        let err = removed_accepted_pending_error(retain_late_route_open, true).await;
        assert_eq!(
            err.outcome_cause(),
            Some(OutcomeUnknownCause::ConsumerClosed),
            "{err:?}"
        );
        assert_ne!(err.never_forwarded(), Some(true));
    }
}

#[tokio::test(start_paused = true)]
async fn removed_accepted_pending_close_does_not_retry_whole_call() {
    let (shared, consumer, mut receiver) = route_open_stand_in(seeded_random(1));
    let (_, handle) =
        open_answered(&shared, &consumer, &mut receiver, None, 41, "close race").await;
    let started = Instant::now();
    let mut call = Box::pin(drain_call(&consumer, b"mutation"));
    assert_pending(call.as_mut()).await;
    let command = receiver.recv().await.unwrap();
    let key = command.pending.unwrap();
    assert_eq!(key.channel, handle.channel);
    assert!(shared.mark_pending_accepted(key));
    // Leave the accepted entry unsettled while close wakes the whole call's reply waiter.
    let entry = shared.lock_inner().remove_pending(&key).unwrap();
    shared.close_sync("consumer closed during accepted request settlement");
    let err = call.await.unwrap_err();
    assert_eq!(Instant::now(), started);
    // A retry would lose the original close cause by failing before queueing on the closed consumer.
    assert_eq!(
        err.outcome_cause(),
        Some(OutcomeUnknownCause::ConsumerClosed),
        "{err:?}"
    );
    assert_eq!(err.not_sent_cause(), None);
    assert_ne!(err.never_forwarded(), Some(true));
    assert!(receiver.try_recv().is_err());
    entry.settle_failure("consumer closed".to_string());
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
