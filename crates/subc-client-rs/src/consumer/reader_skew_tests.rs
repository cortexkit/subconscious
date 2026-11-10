use super::*;
use serde_json::{json, Value};

fn fixture() -> (Arc<SubcConsumer>, mpsc::Receiver<WriteCommand>) {
    let shared = Arc::new(Shared::new(
        PathBuf::from("unused"),
        ConsumerOptions::default(),
    ));
    let (writer, wire) = mpsc::channel(32);
    {
        let mut inner = shared.lock_inner();
        inner.writer = Some(writer);
        inner.daemon_ver = "0.39.0".into();
    }
    (Arc::new(SubcConsumer { shared }), wire)
}

fn frame(corr: u64, ty: FrameType, body: Value) -> Frame {
    Frame::build(
        ty,
        Flags::new(false, Priority::Interactive, false),
        0,
        0,
        corr,
        serde_json::to_vec(&body).unwrap(),
    )
    .unwrap()
}

async fn reply(consumer: &SubcConsumer, corr: u64, body: Value) {
    let generation = consumer.shared.lock_inner().generation;
    assert!(
        dispatch_frame(
            &consumer.shared,
            generation,
            frame(corr, FrameType::Response, body)
        )
        .await
    );
}

fn target() -> Value {
    json!({"incarnation":"daemon-1", "kind":"spawn", "spawn":7,
        "authority_epoch":3, "first_party":true, "approved_declarations_digest":"approved"})
}

fn open(
    consumer: Arc<SubcConsumer>,
    id: usize,
    admitted: bool,
) -> JoinHandle<Result<RouteHandle, CallError>> {
    tokio::spawn(async move {
        let target = RouteTarget::ToolProvider {
            module_id: format!("provider-{id}"),
        };
        let identity = BindIdentity::new("test", "reader");
        if admitted {
            consumer
                .open_route_with_admission_facts(target, identity, json!({}))
                .await
        } else {
            consumer
                .open_route(target, identity, CallOptions::default())
                .await
        }
    })
}

async fn describe(
    consumer: &SubcConsumer,
    wire: &mut mpsc::Receiver<WriteCommand>,
    capabilities: Value,
) {
    let request = wire.recv().await.unwrap().frame;
    assert_eq!(
        serde_json::from_slice::<Value>(&request.body).unwrap()["op"],
        "server.describe"
    );
    reply(
        consumer,
        request.header.corr,
        json!({"op":"server.describe", "protocol_ver":1,
        "subc_ops":[], "capabilities":capabilities, "connected_clients":1}),
    )
    .await;
}

async fn accept(consumer: &SubcConsumer, request: &Frame, channel: u16, target: Option<Value>) {
    let mut body = json!({"op":"route.open", "route_channel":channel, "route_epoch":1});
    if let Some(target) = target {
        body["target"] = target;
    }
    reply(consumer, request.header.corr, body).await;
}

#[tokio::test]
async fn reader_skew_close_reason_declaration_is_gated_on_each_connection_and_every_open() {
    let (consumer, mut wire) = fixture();
    for (generation, advertised) in [(1, false), (2, true), (3, false)] {
        if generation > 1 {
            let writer = consumer.shared.lock_inner().writer.clone();
            let (client, _daemon) = tokio::io::duplex(1024);
            consumer
                .shared
                .install_reconnected(OpenedConnection {
                    stream: Box::new(client),
                    daemon_origin_trusted: true,
                    daemon_ver: "0.39.0".into(),
                })
                .unwrap();
            // Exercise the production reset, then use the same scripted writer
            // to inspect requests on the newly installed connection generation.
            let mut inner = consumer.shared.lock_inner();
            inner.reader_task.take().unwrap().abort();
            inner.writer_task.take().unwrap().abort();
            inner.writer = writer;
            assert_eq!(inner.generation, generation);
        }
        for (id, admitted) in [(1, false), (2, true)] {
            let call = open(Arc::clone(&consumer), id, admitted);
            if id == 1 {
                describe(
                    &consumer,
                    &mut wire,
                    if advertised {
                        json!([ROUTE_CLOSE_REASONS_CAPABILITY])
                    } else {
                        json!([])
                    },
                )
                .await;
            }
            let request = wire.recv().await.unwrap().frame;
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            assert_eq!(
                body["op"], "route.open",
                "description must be cached for this generation"
            );
            if advertised {
                assert_eq!(
                    body["accepts_close_reasons"],
                    json!([
                        "reload",
                        "restart",
                        "authority_changed",
                        "disable",
                        "crash",
                        "capability_denied",
                        "scope_ended",
                        "scope_carrier_removed",
                        "scope_delegation_changed",
                        "scope_parent_ended"
                    ])
                );
            } else {
                assert!(body.get("accepts_close_reasons").is_none());
            }
            accept(&consumer, &request, id as u16, None).await;
            call.await.unwrap().unwrap();
        }
    }
}

#[tokio::test(start_paused = true)]
async fn reader_skew_failed_describe_is_cached_and_never_costs_a_route() {
    for failure in ["refused", "malformed", "timeout"] {
        let (consumer, mut wire) = fixture();
        for id in 1..=2 {
            let started = Instant::now();
            let call = open(Arc::clone(&consumer), id, id == 2);
            if id == 1 {
                let request = wire.recv().await.unwrap().frame;
                assert_eq!(
                    serde_json::from_slice::<Value>(&request.body).unwrap()["op"],
                    "server.describe"
                );
                match failure {
                    "refused" => {
                        dispatch_frame(
                            &consumer.shared,
                            1,
                            frame(
                                request.header.corr,
                                FrameType::Error,
                                json!({"code":"unknown_op", "message":"old daemon"}),
                            ),
                        )
                        .await;
                    }
                    "malformed" => {
                        reply(
                            &consumer,
                            request.header.corr,
                            json!({"op":"server.describe"}),
                        )
                        .await
                    }
                    _ => tokio::time::advance(CAPABILITY_PROBE_TIMEOUT).await,
                }
            }
            let request = wire.recv().await.unwrap().frame;
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            assert_eq!(body["op"], "route.open");
            assert!(body.get("accepts_close_reasons").is_none());
            accept(&consumer, &request, id as u16, None).await;
            call.await.unwrap().unwrap();
            assert!(
                started.elapsed() <= Duration::from_secs(2),
                "probe must preserve the route's remaining budget"
            );
        }
    }
}

#[tokio::test]
async fn reader_skew_target_is_captured_and_absent_or_incomplete_targets_are_unmarked() {
    let (consumer, mut wire) = fixture();
    consumer.shared.lock_inner().accepts_close_reasons = Some(false);
    for admitted in [false, true] {
        for (id, authority) in [
            Some(target()),
            None,
            Some(json!({"kind":"spawn", "first_party":true})),
        ]
        .into_iter()
        .enumerate()
        {
            let call = open(
                Arc::clone(&consumer),
                id + usize::from(admitted) * 10,
                admitted,
            );
            let request = wire.recv().await.unwrap().frame;
            accept(&consumer, &request, (id + 1) as u16, authority).await;
            let handle = call.await.unwrap().unwrap();
            let observed = consumer.route_target(&handle);
            if id == 0 {
                let observed = observed.unwrap();
                assert_eq!(observed.spawn, Some(7));
                assert_eq!(observed.authority_epoch, Some(3));
                assert!(observed.first_party);
                assert_eq!(
                    observed.approved_declarations_digest.as_deref(),
                    Some("approved")
                );
            } else {
                assert!(observed.is_none());
            }
            consumer
                .close_handle(&handle, CloseRouteOptions::default())
                .await
                .unwrap();
            assert!(consumer.route_target(&handle).is_none());
            // Closing a handle may queue its GOODBYE before the next open.
            while wire.try_recv().is_ok() {}
        }
    }
}

#[tokio::test]
async fn reader_skew_target_unknown_kind_never_establishes_spawn_authority() {
    let (consumer, mut wire) = fixture();
    consumer.shared.lock_inner().accepts_close_reasons = Some(false);
    let mut authority = target();
    authority["kind"] = json!("future");
    let call = open(Arc::clone(&consumer), 0, true);
    let request = wire.recv().await.unwrap().frame;
    accept(&consumer, &request, 10, Some(authority)).await;
    let handle = call.await.unwrap().unwrap();
    let target = consumer.route_target(&handle).unwrap();
    assert_eq!(target.kind, RouteOpenTargetKind::Unknown);
    assert!(!target.first_party);
    assert_eq!(target.spawn, None);
    assert_eq!(target.authority_epoch, None);
    assert_eq!(target.approved_declarations_digest, None);
}

#[tokio::test]
async fn reader_skew_target_unsafe_integers_are_malformed() {
    let (consumer, mut wire) = fixture();
    consumer.shared.lock_inner().accepts_close_reasons = Some(false);
    for (id, field) in ["spawn", "authority_epoch"].into_iter().enumerate() {
        let mut authority = target();
        authority[field] = json!(1_u64 << 53);
        let call = open(Arc::clone(&consumer), id, true);
        let request = wire.recv().await.unwrap().frame;
        accept(&consumer, &request, (id + 10) as u16, Some(authority)).await;
        assert!(
            call.await.unwrap().is_err(),
            "unsafe {field} must be malformed, not rounded"
        );
    }
}

#[tokio::test]
async fn reader_skew_catalog_watch_decodes_initial_and_coalesced_pushes_and_keeps_errors() {
    let (consumer, mut wire) = fixture();
    let mut stream = consumer.catalog_watch().await.unwrap();
    let request = wire.recv().await.unwrap().frame;
    assert_eq!(request.header.channel, 0);
    assert_ne!(request.header.encode()[6] & FLAG_SUBSCRIPTION, 0);
    assert_eq!(
        serde_json::from_slice::<Value>(&request.body).unwrap(),
        json!({"op":"catalog.watch"})
    );
    for generation in [4, 19] {
        dispatch_frame(
            &consumer.shared,
            1,
            frame(
                request.header.corr,
                FrameType::StreamData,
                json!({"op":"catalog.watch", "generation":generation, "future":true}),
            ),
        )
        .await;
    }
    dispatch_frame(
        &consumer.shared,
        1,
        frame(
            request.header.corr,
            FrameType::Error,
            json!({"code":"future_watch_error", "message":"terminal"}),
        ),
    )
    .await;
    assert_eq!(stream.next().await.unwrap().unwrap().generation, 4);
    assert_eq!(stream.next().await.unwrap().unwrap().generation, 19);
    assert_eq!(
        stream.next().await.unwrap_err().code(),
        Some("future_watch_error")
    );
    assert!(stream.next().await.unwrap().is_none());
}

#[tokio::test]
async fn reader_skew_catalog_authority_fields_are_optional_discovery_facts() {
    let (consumer, mut wire) = fixture();
    for enriched in [true, false] {
        let caller = Arc::clone(&consumer);
        let call = tokio::spawn(async move { caller.catalog_list().await });
        let request = wire.recv().await.unwrap().frame;
        let mut entry = json!({"module_id":"provider", "roles":[], "control_ops":[]});
        if enriched {
            entry.as_object_mut().unwrap().extend(json!({
                "incarnation":"daemon-1", "spawn":7, "first_party":true, "origin":"future",
                "requested_declarations_digest":"requested", "approved_declarations_digest":"approved",
                "approved_declarations":{"provider":{"future":true}},
                "requested_declarations":{"provider":{"future":false}}
            }).as_object().unwrap().clone());
        }
        reply(
            &consumer,
            request.header.corr,
            json!({"op":"catalog.list", "generation":1, "modules":[entry], "subc_ops":[]}),
        )
        .await;
        let result = call.await.unwrap().unwrap();
        let entry = &result.modules[0];
        if enriched {
            assert_eq!(entry.incarnation.as_deref(), Some("daemon-1"));
            assert_eq!(entry.spawn, Some(7));
            assert_eq!(entry.first_party, Some(true));
            assert_eq!(entry.origin, Some(crate::CatalogOrigin::Other));
            assert_eq!(
                entry.requested_declarations_digest.as_deref(),
                Some("requested")
            );
            assert_eq!(
                entry.approved_declarations_digest.as_deref(),
                Some("approved")
            );
            assert!(
                entry.approved_declarations.as_ref().unwrap()["provider"]["future"]
                    .as_bool()
                    .unwrap()
            );
            assert!(
                !entry.requested_declarations.as_ref().unwrap()["provider"]["future"]
                    .as_bool()
                    .unwrap()
            );
        } else {
            assert_eq!(entry.incarnation, None);
            assert_eq!(entry.spawn, None);
            assert_eq!(entry.first_party, None);
            assert_eq!(entry.origin, None);
            assert_eq!(entry.requested_declarations_digest, None);
            assert_eq!(entry.approved_declarations_digest, None);
            assert_eq!(entry.approved_declarations, None);
            assert_eq!(entry.requested_declarations, None);
        }
    }
}

#[test]
fn reader_skew_authority_changed_reopens_and_stale_epoch_is_retryable_not_never_forwarded() {
    let push = ControlPush {
        op: "route.closed".into(),
        body: json!({"reason":"authority_changed"}),
    };
    assert_eq!(
        push.route_close_reason(),
        Some(RouteCloseReason::AuthorityChanged)
    );
    assert_eq!(
        push.route_close_reason().unwrap().disposition(),
        RouteCloseDisposition::MayReopen
    );
    assert_eq!(
        RouteCloseReason::from_wire("future").disposition(),
        RouteCloseDisposition::MustNotReopen
    );
    let refusal: ErrorBody =
        serde_json::from_value(json!({"code":"stale_authority_epoch", "message":"changed"}))
            .unwrap();
    assert!(is_retryable_route_open_code(&refusal.code));
    assert!(!is_daemon_refusal_before_forward(
        true,
        Flags::new(false, Priority::Interactive, false).with_daemon_origin(),
        &refusal.code
    ));
}

#[test]
fn reader_skew_declared_close_reasons_cover_every_mapped_variant() {
    let source = syn::parse_file(include_str!("../consumer.rs")).unwrap();
    let implementation = source
        .items
        .iter()
        .find_map(|item| {
            let syn::Item::Impl(implementation) = item else {
                return None;
            };
            let syn::Type::Path(path) = implementation.self_ty.as_ref() else {
                return None;
            };
            (path.path.is_ident("RouteCloseReason")).then_some(implementation)
        })
        .unwrap();
    let decoder = implementation
        .items
        .iter()
        .find_map(|item| {
            let syn::ImplItem::Fn(method) = item else {
                return None;
            };
            (method.sig.ident == "from_wire").then_some(method)
        })
        .unwrap();
    let syn::Stmt::Expr(syn::Expr::Match(mapping), _) = &decoder.block.stmts[0] else {
        panic!("inspect the wire mappings when the decoder structure changes")
    };
    let mapped: BTreeSet<_> = mapping
        .arms
        .iter()
        .filter_map(|arm| {
            let syn::Pat::Lit(literal) = &arm.pat else {
                return None;
            };
            let syn::Lit::Str(name) = &literal.lit else {
                return None;
            };
            Some(name.value())
        })
        .collect();
    assert!(mapped.contains("authority_changed"));
    let declared: BTreeSet<_> = ACCEPTED_CLOSE_REASONS
        .iter()
        .map(|name| (*name).to_string())
        .collect();
    assert_eq!(
        mapped, declared,
        "every handled wire reason must be declared"
    );
}

// Older scripted daemons have no capability exchange in their transcripts.
// Treat in-memory fixtures as if server.describe advertised no close-reason support.
// New connection installs still reset the cache and exercise discovery.
pub(super) fn legacy_shared(path: PathBuf, opts: ConsumerOptions) -> Shared {
    let shared = Shared::new(path, opts);
    shared.lock_inner().accepts_close_reasons = Some(false);
    shared
}

// Socket fixtures keep their existing transcript while answering the new,
// advisory capability probe like an older daemon. Production readers never use
// this adapter; discovery tests above inspect the probe without it.
pub(super) async fn legacy_read_frame<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
) -> Result<Option<Frame>, FrameIoError> {
    loop {
        let Some(request) = subc_transport::read_frame(stream).await? else {
            return Ok(None);
        };
        if request.header.ty != FrameType::Request || request.body != br#"{"op":"server.describe"}"# {
            return Ok(Some(request));
        }
        let response = frame(
            request.header.corr,
            FrameType::Response,
            json!({"op":"server.describe", "protocol_ver":1, "subc_ops":[], "capabilities":[], "connected_clients":1}),
        );
        write_frame(stream, &response).await?;
        stream.flush().await.map_err(FrameIoError::Io)?;
    }
}
