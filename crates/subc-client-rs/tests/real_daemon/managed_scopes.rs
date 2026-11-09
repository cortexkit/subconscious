use super::*;

fn scoped_options(harness: &ScopeHarness, scope_ref: &str, epoch: u64) -> CallOptions {
    CallOptions {
        scope: Some(scope_selector(scope_ref, epoch)),
        ..harness.carrier_options()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn calls_share_routes_only_for_the_same_scope_and_epoch() {
    let mut harness = start_scope_harness().await;
    harness
        .sync(json!([scope_record("a", 1), scope_record("b", 1)]))
        .await;
    let consumer = harness.connect().await;
    for scoped_first in [false, true] {
        let identity = consumer_identity(if scoped_first {
            "scoped-first"
        } else {
            "plain-first"
        });
        let opts = |scoped| {
            if scoped {
                scoped_options(&harness, "a", 1)
            } else {
                harness.carrier_options()
            }
        };
        consumer
            .call(
                tool_target(SCOPE_PROVIDER),
                identity.clone(),
                b"{}".to_vec(),
                opts(scoped_first),
            )
            .await
            .unwrap();
        consumer
            .call_with_reply(
                tool_target(SCOPE_PROVIDER),
                identity.clone(),
                b"{}".to_vec(),
                opts(!scoped_first),
            )
            .await
            .unwrap();
        let (_cancel, reply) = consumer.call_cancellable(
            tool_target(SCOPE_PROVIDER),
            identity.clone(),
            b"{}".to_vec(),
            opts(true),
        );
        reply.await.unwrap();
        let (_cancel, reply) = consumer.call_cancellable_with_reply(
            tool_target(SCOPE_PROVIDER),
            identity.clone(),
            b"{}".to_vec(),
            scoped_options(&harness, "b", 1),
        );
        reply.await.unwrap();
        let attaches = harness.provider_attaches();
        let expected = if scoped_first { 6 } else { 3 };
        assert_eq!(
            attaches.len(),
            expected,
            "one route per scope and unscoped identity: {attaches:?}"
        );
        let mut refs = attaches[expected - 3..]
            .iter()
            .map(|attach| attach["scope"]["ref"].as_str())
            .collect::<Vec<_>>();
        refs.sort();
        assert_eq!(refs, [None, Some("a"), Some("b")]);
    }

    let identity = consumer_identity("plain-first");
    let err = consumer
        .call(
            tool_target(SCOPE_PROVIDER),
            identity.clone(),
            b"{}".to_vec(),
            scoped_options(&harness, "a", 2),
        )
        .await
        .expect_err("a different epoch must not hit epoch 1's cached route");
    assert_eq!(err.route_open_refusal().unwrap().code, "scope_ended");
    assert_eq!(err.never_forwarded(), Some(true));
    harness
        .sync(json!([scope_record("a", 2), scope_record("b", 1)]))
        .await;
    consumer
        .call(
            tool_target(SCOPE_PROVIDER),
            identity,
            b"{}".to_vec(),
            scoped_options(&harness, "a", 2),
        )
        .await
        .unwrap();
    let attaches = harness.provider_attaches();
    assert_eq!(attaches.len(), 7);
    assert_eq!(attaches[6]["scope"]["scope_epoch"], 2);
    consumer.close().await;
    harness.stop();
}

#[derive(Clone, Default)]
struct ScopedBinaryHandler {
    scopes: Arc<Mutex<Vec<Option<ScopeStamp>>>>,
}

#[async_trait]
impl ModuleHandler for ScopedBinaryHandler {
    async fn on_bind(&self, request: &RouteBindRequest) -> BindDecision {
        self.scopes.lock().unwrap().push(request.scope.clone());
        BindDecision::accept()
    }

    async fn handle(&self, ctx: RequestCtx, body: Vec<u8>) -> HandlerOutcome {
        if body == b"watch" {
            ctx.emit(b"tick".to_vec()).await.unwrap();
            ctx.cancelled().await;
            return HandlerOutcome::Streamed;
        }
        let payload = if body == b"binary" {
            vec![0, 255, 128, 42]
        } else {
            let (head, payload) = ctx.request_payload().unwrap().unwrap();
            assert_eq!(head, json!({"method": "chunk"}));
            payload.to_vec()
        };
        ctx.mark_binary_reply();
        HandlerOutcome::Response(payload)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn framed_uploads_and_binary_replies_keep_their_scope() {
    let mut harness = start_scope_harness().await;
    harness.sync(json!([scope_record("binary", 1)])).await;
    let mut manifest = inline_module_manifest("scoped-binary", &[]);
    manifest.provides.push(ProviderRole::ManagementSurface {
        operations: vec![ManagementOperation {
            name: "chunk".into(),
            kind: ManagementOperationKind::Mutate,
            description: None,
        }],
        config_schema: json!({}),
        observability: vec![],
        identity_scope: vec![],
        concurrency: Concurrency::Serial,
    });
    manifest.operation_payload.insert(
        "chunk".into(),
        subc_protocol::manifest::OperationPayload::new(4),
    );
    let handler = ScopedBinaryHandler::default();
    let (_, serving) = spawn_inline_module_with_handler(
        &harness.daemon.connection_file,
        manifest,
        handler.clone(),
    )
    .await;
    wait_for_catalog_module(
        &harness.daemon.connection_file,
        "scoped-binary",
        START_TIMEOUT,
    )
    .await;
    let consumer = harness.connect().await;
    let target = RouteTarget::ManagementSurface {
        module_id: "scoped-binary".into(),
    };
    let identity = consumer_identity("scoped-binary");
    let opts = scoped_options(&harness, "binary", 1);
    let bytes = vec![0, 255, 128, 42];
    let reply = consumer
        .call_framed_with_reply(
            target.clone(),
            identity.clone(),
            json!({"method": "chunk"}),
            bytes.clone(),
            opts.clone(),
        )
        .await
        .unwrap();
    assert_eq!(reply.body(), bytes);
    assert!(reply.is_binary());
    let (_cancel, reply) = consumer.call_framed_cancellable_with_reply(
        target.clone(),
        identity.clone(),
        json!({"method": "chunk"}),
        bytes.clone(),
        opts.clone(),
    );
    let reply = reply.await.unwrap();
    assert_eq!(reply.body(), bytes);
    assert!(reply.is_binary());
    let (_cancel, reply) =
        consumer.call_cancellable_with_reply(target, identity, b"binary".to_vec(), opts);
    let reply = reply.await.unwrap();
    assert_eq!(reply.body(), bytes);
    assert!(reply.is_binary());
    let scopes = handler.scopes.lock().unwrap().clone();
    assert_eq!(scopes.len(), 1, "all variants share the scoped route");
    let stamp =
        serde_json::to_value(scopes[0].as_ref().expect("the bind must carry scope")).unwrap();
    assert_eq!(stamp["ref"], "binary");
    assert_eq!(stamp["scope_epoch"], 1);
    consumer.close().await;
    harness.stop();
    assert!(serving.await.unwrap().is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scope_close_settles_calls_and_later_calls_reopen_to_a_daemon_refusal() {
    let mut harness = start_scope_harness().await;
    harness.sync(json!([scope_record("pending", 1)])).await;
    let consumer = Arc::new(harness.connect().await);
    let mut pushes = consumer.control_pushes(16);
    let opts = CallOptions {
        timeout: Duration::from_secs(60),
        ..scoped_options(&harness, "pending", 1)
    };
    let identity = consumer_identity("scoped-pending");
    let mut pending = Vec::new();
    for _ in 0..2 {
        let consumer = Arc::clone(&consumer);
        let identity = identity.clone();
        let opts = opts.clone();
        pending.push(tokio::spawn(async move {
            let (_cancel, reply) = consumer.call_cancellable_with_reply(
                tool_target(SCOPE_PROVIDER),
                identity,
                serde_json::to_vec(&json!({"name":"hold", "arguments":{"delay_ms":30_000}}))
                    .unwrap(),
                opts,
            );
            reply.await
        }));
    }
    timeout(EVENT_TIMEOUT, async {
        loop {
            let count = read_events(&harness.provider_events)
                .iter()
                .filter(|event| {
                    event["kind"] == "request_received" && event["body_json"]["name"] == "hold"
                })
                .count();
            if count == 2 {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("both calls must have reached the provider");

    // Advancing the epoch ends the old scope while leaving a live record, so
    // the daemon can identify a later open under the old selector as ended.
    harness.sync(json!([scope_record("pending", 2)])).await;
    let closed = timeout(EVENT_TIMEOUT, async {
        loop {
            let push = pushes.recv().await.unwrap();
            if push.op == "route.closed" {
                break push;
            }
        }
    })
    .await
    .expect("scope closure must reach the consumer");
    assert_eq!(closed.body["reason"], "scope_ended");
    for call in pending {
        let err = timeout(EVENT_TIMEOUT, call)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(matches!(err, CallError::OutcomeUnknown(_)), "{err}");
        assert_eq!(err.outcome_cause(), Some(OutcomeUnknownCause::RouteEnded));
        assert_eq!(err.never_forwarded(), None);
        assert_eq!(
            err.close_reason(),
            Some(&RouteEndReason::Daemon(RouteCloseReason::ScopeEnded))
        );
    }
    let err = consumer
        .call(tool_target(SCOPE_PROVIDER), identity, b"{}".to_vec(), opts)
        .await
        .expect_err("a later call must attempt a fresh open, not reuse the ended route");
    assert!(matches!(err, CallError::NotSent(_)), "{err}");
    assert_eq!(err.never_forwarded(), Some(true));
    assert_eq!(err.route_open_refusal().unwrap().code, "scope_ended");
    assert_eq!(
        harness.provider_attaches().len(),
        1,
        "the refused reopen must not bind"
    );
    consumer.close().await;
    harness.stop();
}

fn assert_scope_argument_error<T: std::fmt::Debug>(result: Result<T, CallError>, message: &str) {
    let err = result.expect_err("scope must not be silently ignored");
    assert!(matches!(err, CallError::NotSent(_)), "{err}");
    assert_eq!(err.never_forwarded(), Some(true));
    assert!(
        err.route_open_refusal().is_none(),
        "local mistakes have no daemon code"
    );
    assert!(err.to_string().contains(message), "{err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn route_open_rejects_ambiguous_scope_options() {
    let mut harness = start_scope_harness().await;
    harness
        .sync(json!([scope_record("a", 1), scope_record("b", 1)]))
        .await;
    let consumer = harness.connect().await;
    let target = tool_target(SCOPE_PROVIDER);
    let identity = consumer_identity("scope-arguments");
    assert_scope_argument_error(
        consumer
            .open_route(
                target.clone(),
                identity.clone(),
                scoped_options(&harness, "a", 1),
            )
            .await,
        "open_route_scoped",
    );
    assert_scope_argument_error(
        consumer
            .open_route_scoped(
                target.clone(),
                identity.clone(),
                scope_selector("a", 1),
                scoped_options(&harness, "b", 1),
            )
            .await,
        "scope",
    );
    assert!(
        harness.provider_attaches().is_empty(),
        "argument errors must not open a route"
    );
    let explicit = consumer
        .open_route_scoped(
            target.clone(),
            identity.clone(),
            scope_selector("a", 1),
            harness.carrier_options(),
        )
        .await
        .unwrap();
    let equal = consumer
        .open_route_scoped(
            target,
            identity,
            scope_selector("a", 1),
            scoped_options(&harness, "a", 1),
        )
        .await
        .unwrap();
    assert_eq!(explicit, equal);
    consumer.close().await;
    harness.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handle_and_admitted_apis_reject_scope_without_sending() {
    let mut harness = start_scope_harness().await;
    harness.sync(json!([scope_record("a", 1)])).await;
    let consumer = harness.connect().await;
    let target = tool_target(SCOPE_PROVIDER);
    let identity = consumer_identity("handle-scope-arguments");
    let handle = consumer
        .open_route_scoped(
            target.clone(),
            identity.clone(),
            scope_selector("a", 1),
            harness.carrier_options(),
        )
        .await
        .unwrap();
    let opts = scoped_options(&harness, "a", 1);
    assert_scope_argument_error(
        consumer
            .close_handle(
                &handle,
                CloseRouteOptions {
                    scope: Some(scope_selector("a", 1)),
                    ..CloseRouteOptions::default()
                },
            )
            .await,
        "scope",
    );
    assert_scope_argument_error(
        consumer
            .request(&handle, b"{}".to_vec(), opts.clone())
            .await,
        "scope",
    );
    let (_cancel, reply) = consumer.request_cancellable(&handle, b"{}".to_vec(), opts.clone());
    assert_scope_argument_error(reply.await, "scope");
    let err = match consumer
        .subscribe_route(
            &handle,
            b"{}".to_vec(),
            SubscribeOptions {
                scope: Some(scope_selector("a", 1)),
                ..fast_subscribe_options()
            },
        )
        .await
    {
        Ok(_) => panic!("subscribe_route must not silently ignore scope"),
        Err(err) => err,
    };
    assert_scope_argument_error::<()>(Err(err), "scope");
    let reservation = consumer
        .reserve(&handle, Instant::now() + EVENT_TIMEOUT)
        .await
        .unwrap();
    let (_corr, reply) = reservation.send(
        subc_client_rs::RequestBody::new(b"{}".to_vec()).unwrap(),
        opts.clone(),
    );
    assert_scope_argument_error(reply.await, "scope");
    assert_scope_argument_error(
        consumer
            .open_route_with_admission_facts_and_options(target, identity, json!({}), opts)
            .await,
        "scope",
    );
    assert!(!read_events(&harness.provider_events)
        .iter()
        .any(|event| event["kind"] == "request_received"));
    consumer
        .request(&handle, b"{}".to_vec(), fast_call_options())
        .await
        .unwrap();
    consumer.close().await;
    harness.stop();
}

fn scoped_subscribe_options(harness: &ScopeHarness, epoch: u64) -> SubscribeOptions {
    SubscribeOptions {
        scope: Some(scope_selector("watch", epoch)),
        consumer_identity: Some(harness.carrier.clone()),
        ..fast_subscribe_options()
    }
}

async fn start_scoped_watch(
    harness: &ScopeHarness,
) -> (
    ScopedBinaryHandler,
    tokio::task::JoinHandle<Result<(), SubcModuleError>>,
) {
    let handler = ScopedBinaryHandler::default();
    let (_, serving) = spawn_inline_module_with_handler(
        &harness.daemon.connection_file,
        inline_module_manifest("scoped-watch", &["watch"]),
        handler.clone(),
    )
    .await;
    wait_for_catalog_module(
        &harness.daemon.connection_file,
        "scoped-watch",
        START_TIMEOUT,
    )
    .await;
    (handler, serving)
}

async fn assert_tick(subscription: &mut subc_client_rs::Subscription) {
    assert_eq!(
        timeout(EVENT_TIMEOUT, subscription.events().recv())
            .await
            .unwrap()
            .unwrap(),
        b"tick"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subscriptions_keep_scope_receive_events_and_end_without_reopening() {
    let mut harness = start_scope_harness().await;
    harness.sync(json!([scope_record("watch", 1)])).await;
    let (handler, serving) = start_scoped_watch(&harness).await;
    let consumer = harness.connect().await;
    let target = tool_target("scoped-watch");
    let identity = consumer_identity("scoped-watch");
    consumer
        .call(
            target.clone(),
            identity.clone(),
            b"binary".to_vec(),
            scoped_options(&harness, "watch", 1),
        )
        .await
        .unwrap();
    let mut scoped = consumer
        .subscribe(
            target.clone(),
            identity.clone(),
            b"watch".to_vec(),
            scoped_subscribe_options(&harness, 1),
        )
        .await
        .unwrap();
    let mut unscoped = consumer
        .subscribe(
            target.clone(),
            identity.clone(),
            b"watch".to_vec(),
            SubscribeOptions {
                scope: None,
                ..scoped_subscribe_options(&harness, 1)
            },
        )
        .await
        .unwrap();
    assert_tick(&mut scoped).await;
    assert_tick(&mut unscoped).await;
    let scopes = handler.scopes.lock().unwrap().clone();
    assert_eq!(
        scopes.len(),
        2,
        "scoped call and watch share a route, unscoped watch does not"
    );
    assert!(scopes[0].is_some());
    assert!(scopes[1].is_none());
    harness.sync(json!([scope_record("watch", 2)])).await;
    let err = timeout(EVENT_TIMEOUT, scoped.closed())
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(
        err.close_reason(),
        Some(&RouteEndReason::Daemon(RouteCloseReason::ScopeEnded))
    );
    for (removed, code) in [(false, "scope_ended"), (true, "scope_not_live")] {
        if removed {
            harness.sync(json!([])).await;
        }
        let err = match consumer
            .subscribe(
                target.clone(),
                identity.clone(),
                b"watch".to_vec(),
                scoped_subscribe_options(&harness, 1),
            )
            .await
        {
            Ok(_) => panic!("ended scope must not reopen a watch"),
            Err(err) => err,
        };
        assert!(matches!(err, CallError::NotSent(_)), "{err}");
        assert_eq!(err.never_forwarded(), Some(true));
        assert_eq!(err.route_open_refusal().unwrap().code, code);
    }
    assert_eq!(
        handler.scopes.lock().unwrap().len(),
        2,
        "closed watch must never reopen as unscoped"
    );
    consumer.close().await;
    harness.stop();
    assert!(serving.await.unwrap().is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn close_route_selects_only_the_requested_scope_for_calls_and_watches() {
    let mut harness = start_scope_harness().await;
    harness.sync(json!([scope_record("watch", 1)])).await;
    let (_, serving) = start_scoped_watch(&harness).await;
    let consumer = harness.connect().await;
    let target = tool_target("scoped-watch");
    let identity = consumer_identity("close-scoped-watch");
    for close_scoped_first in [true, false] {
        consumer
            .call(
                target.clone(),
                identity.clone(),
                b"binary".to_vec(),
                scoped_options(&harness, "watch", 1),
            )
            .await
            .unwrap();
        let scoped_handle = consumer
            .open_route_scoped(
                target.clone(),
                identity.clone(),
                scope_selector("watch", 1),
                harness.carrier_options(),
            )
            .await
            .unwrap();
        let unscoped_handle = consumer
            .open_route(target.clone(), identity.clone(), harness.carrier_options())
            .await
            .unwrap();
        let mut scoped = consumer
            .subscribe(
                target.clone(),
                identity.clone(),
                b"watch".to_vec(),
                scoped_subscribe_options(&harness, 1),
            )
            .await
            .unwrap();
        let mut unscoped = consumer
            .subscribe(
                target.clone(),
                identity.clone(),
                b"watch".to_vec(),
                SubscribeOptions {
                    scope: None,
                    ..scoped_subscribe_options(&harness, 1)
                },
            )
            .await
            .unwrap();
        assert_tick(&mut scoped).await;
        assert_tick(&mut unscoped).await;
        for selected in [close_scoped_first, !close_scoped_first] {
            consumer
                .close_route(
                    target.clone(),
                    identity.clone(),
                    CloseRouteOptions {
                        scope: selected.then(|| scope_selector("watch", 1)),
                        consumer_identity: Some(harness.carrier.clone()),
                        ..CloseRouteOptions::default()
                    },
                )
                .await;
            let (closed, handle, other) = if selected {
                (&mut scoped, scoped_handle, unscoped_handle)
            } else {
                (&mut unscoped, unscoped_handle, scoped_handle)
            };
            let err = timeout(EVENT_TIMEOUT, closed.closed())
                .await
                .unwrap()
                .unwrap_err();
            assert_eq!(err.close_reason(), Some(&RouteEndReason::ClosedByCaller));
            assert!(matches!(
                consumer
                    .request(&handle, b"binary".to_vec(), fast_call_options())
                    .await,
                Err(CallError::StaleRouteHandle(_))
            ));
            if selected == close_scoped_first {
                consumer
                    .request(&other, b"binary".to_vec(), fast_call_options())
                    .await
                    .expect("closing one cache entry leaves the other usable");
            }
        }
    }
    consumer.close().await;
    harness.stop();
    assert!(serving.await.unwrap().is_ok());
}
