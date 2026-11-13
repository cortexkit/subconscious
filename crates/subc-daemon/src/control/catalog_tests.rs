use super::*;
use subc_control::{CatalogWatchRequest, CatalogWatchResponse};

fn hello_declarations(entries: Value) -> Frame {
    let mut hello: Value =
        serde_json::from_slice(&hello_frame("declaring", PROTOCOL_VERSION, 1).body).unwrap();
    hello["manifest"]["provider_declarations"] = entries;
    Frame::build(
        FrameType::Hello,
        control_flags(),
        0,
        0,
        1,
        serde_json::to_vec(&hello).unwrap(),
    )
    .unwrap()
}

fn refused(entries: Value) {
    let registry = Arc::new(Registry::default());
    let handler = ControlHandler::new(registry.clone());
    let reply = handler
        .handle_control(ConnectionId::new(1), hello_declarations(entries))
        .unwrap();
    assert_eq!(reply[0].header.ty, FrameType::Error);
    let error = parse_error(&reply[0]);
    assert_eq!(error["code"], error_codes::INVALID_PROVIDER_DECLARATIONS);
    assert!(error["message"]
        .as_str()
        .unwrap()
        .contains("provider_declarations"));
    assert!(registry.list_modules().unwrap().1.is_empty());
}

#[test]
fn hello_provider_declarations_refuses_non_object_field() {
    refused(json!([]));
}

#[test]
fn hello_provider_declarations_refuses_more_than_16_entries() {
    refused(Value::Object(
        (0..17)
            .map(|i| (format!("provider-{i}"), json!({})))
            .collect(),
    ));
}

#[test]
fn hello_provider_declarations_refuses_bad_module_key() {
    refused(json!({"../provider": {}}));
}

#[test]
fn hello_provider_declarations_refuses_non_object_value() {
    refused(json!({"provider": []}));
}

#[test]
fn hello_provider_declarations_refuses_oversized_entry() {
    // Multibyte text distinguishes compact byte length from character count.
    refused(json!({"provider": {"text": "é".repeat(8192)}}));
}

#[test]
fn hello_provider_declarations_refuses_oversized_field_with_bounded_entries() {
    let entry = json!({"text": "x".repeat(13 * 1024)});
    assert!(serde_json::to_vec(&entry).unwrap().len() < 16 * 1024);
    refused(json!({"provider-a": entry.clone(), "provider-b": entry}));
}

#[test]
fn hello_provider_declarations_canonical_number_rules_and_safe_boundary() {
    for number in [
        json!(9_007_199_254_740_992_u64),
        json!(-9_007_199_254_740_992_i64),
        json!(9_007_199_254_740_992.0_f64),
    ] {
        let handler = ControlHandler::new(Arc::new(Registry::default()));
        let reply = handler
            .handle_control(
                ConnectionId::new(1),
                hello_declarations(json!({"provider": {"nested": [number]}})),
            )
            .unwrap();
        let error = parse_error(&reply[0]);
        assert_eq!(error["code"], error_codes::INVALID_PROVIDER_DECLARATIONS);
        let message = error["message"].as_str().unwrap();
        assert!(message.contains("provider_declarations key \"provider\""));
        assert!(message.contains("exactly representable range"));
    }
    let handler = ControlHandler::new(Arc::new(Registry::default()));
    let reply = handler
        .handle_control(
            ConnectionId::new(1),
            hello_declarations(json!({"provider": {
                "max": 9_007_199_254_740_991_u64, "min": -9_007_199_254_740_991_i64, "fraction": 0.5
            }})),
        )
        .unwrap();
    assert_eq!(reply[0].header.ty, FrameType::HelloAck);
    catalog(&handler, Some("declaring"));
}

fn catalog(handler: &ControlHandler, module: Option<&str>) -> (Frame, Vec<CatalogEntry>) {
    let module_id = module.map(str::to_owned);
    let request = Frame::build(
        FrameType::Request,
        control_flags(),
        0,
        0,
        2,
        serde_json::to_vec(&ClientControlRequest::CatalogList {
            module_id: module_id.clone(),
        })
        .unwrap(),
    )
    .unwrap();
    let reply = handler
        .handle_catalog_list(request, module_id)
        .unwrap()
        .remove(0);
    let ClientControlResponse::CatalogList { modules, .. } =
        serde_json::from_slice(&reply.body).unwrap()
    else {
        panic!("expected catalog");
    };
    (reply, modules)
}

#[test]
fn catalog_provider_declarations_relay_unknown_keys_filtered_only_mark_false() {
    let handler = ControlHandler::new(Arc::new(Registry::default()));
    let requested =
        json!({"provider": {"future": {"nested": ["opaque", 1]}, "version": "provider-owned"}});
    handler
        .handle_control(ConnectionId::new(1), hello_declarations(requested.clone()))
        .unwrap();
    let (wire, unfiltered) = catalog(&handler, None);
    let entry = &unfiltered[0];
    assert_eq!(entry.first_party, Some(false));
    assert_eq!(entry.origin, Some(subc_control::CatalogOrigin::Other));
    assert!(entry.requested_declarations.is_none());
    assert!(entry.approved_declarations.is_none());
    let raw: Value = serde_json::from_slice(&wire.body).unwrap();
    assert!(raw["modules"][0].get("requested_declarations").is_none());
    assert!(raw["modules"][0].get("approved_declarations").is_none());
    let (_, filtered) = catalog(&handler, Some("declaring"));
    assert_eq!(
        serde_json::to_value(&filtered[0].requested_declarations).unwrap(),
        requested
    );
    assert_eq!(filtered[0].approved_declarations, Some(Default::default()));
    assert_eq!(
        filtered[0].requested_declarations_digest,
        entry.requested_declarations_digest
    );
    assert_eq!(
        entry.approved_declarations_digest.as_deref(),
        Some("44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a")
    );
    assert_ne!(
        entry.requested_declarations_digest,
        entry.approved_declarations_digest
    );
}

#[test]
fn catalog_provider_declarations_digest_uses_jcs_entry_then_set() {
    // Independent known bytes pin UTF-16 key ordering and ECMAScript numbers.
    let entry = json!({"\u{e000}": 1e-7, "\u{10000}": -0.0});
    let declarations = serde_json::from_value(json!({"provider": entry})).unwrap();
    let entry_bytes = "{\"entry\":{\"𐀀\":0,\"\":1e-7},\"provider\":\"provider\"}";
    use sha2::{Digest, Sha256};
    let entry_digest = format!("{:x}", Sha256::digest(entry_bytes.as_bytes()));
    let set_bytes = format!("{{\"provider\":\"{entry_digest}\"}}");
    let expected = format!("{:x}", Sha256::digest(set_bytes.as_bytes()));
    assert_eq!(
        super::super::catalog::declarations_digest(&declarations).unwrap(),
        expected
    );
}

#[tokio::test]
async fn catalog_update_provider_declarations_frozen_on_raw_frame() {
    let handler = ControlHandler::new(Arc::new(Registry::default()));
    let (ctx, mut rx) = route_ctx(ConnectionId::new(1));
    hello_via_sink(
        &handler,
        &ctx,
        &mut rx,
        hello_declarations(json!({"provider": {"immutable": true}})),
    )
    .await;
    for body in [
        br#"{"op":"catalog.update","provider_declarations":null}"#.as_slice(),
        br#"{"op":"catalog.update","provides":"not-decodable","provider_declarations":{}}"#,
    ] {
        let request =
            Frame::build(FrameType::Request, control_flags(), 0, 0, 3, body.to_vec()).unwrap();
        let reply = handler.handle_control_frame(&ctx, request).await.unwrap();
        assert_eq!(
            parse_error(&reply[0])["code"],
            error_codes::CATALOG_UPDATE_FROZEN_DECLARATIONS
        );
    }
    let (_, entries) = catalog(&handler, Some("declaring"));
    assert_eq!(
        serde_json::to_value(&entries[0].requested_declarations).unwrap(),
        json!({"provider": {"immutable": true}})
    );
}

async fn watch(handler: &ControlHandler, ctx: &RouteCtx) {
    let request = Frame::build(
        FrameType::Request,
        control_flags(),
        0,
        0,
        77,
        serde_json::to_vec(&CatalogWatchRequest::new()).unwrap(),
    )
    .unwrap();
    assert!(handler
        .handle_control_frame(ctx, request)
        .await
        .unwrap()
        .is_empty());
}

async fn generation(rx: &mut mpsc::Receiver<crate::router::OutboundFrame>) -> u64 {
    let frame = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(frame.header.ty, FrameType::StreamData);
    assert_eq!(frame.header.channel, 0);
    assert_eq!(frame.header.corr, 77);
    assert!(!frame.header.flags.is_last());
    serde_json::from_slice::<CatalogWatchResponse>(&frame.body)
        .unwrap()
        .generation
}

#[tokio::test]
async fn catalog_watch_current_appear_leave_promote_and_authority_changes() {
    let registry = Arc::new(Registry::default());
    let handler = ControlHandler::new(registry.clone());
    let (ctx, mut rx) = route_ctx(ConnectionId::new(90));
    watch(&handler, &ctx).await;
    assert_eq!(generation(&mut rx).await, 0);
    handler
        .handle_control(
            ConnectionId::new(1),
            hello_frame("one", PROTOCOL_VERSION, 1),
        )
        .unwrap();
    assert_eq!(generation(&mut rx).await, 1);
    registry
        .register_candidate_with_control_ops(
            manifest("one", PROTOCOL_VERSION),
            PROTOCOL_VERSION,
            ConnectionId::new(2),
            vec![],
        )
        .unwrap();
    assert!(
        rx.try_recv().is_err(),
        "a hidden candidate does not advance discovery"
    );
    registry.promote_candidate("one").unwrap();
    assert_eq!(generation(&mut rx).await, 2);
    // The authority owner calls the same trigger for an approved-set change,
    // first-party mark activation or lowering once those transitions exist.
    registry.catalog_authority_changed().unwrap();
    assert_eq!(generation(&mut rx).await, 3);
    handler.cleanup_connection(ConnectionId::new(2)).unwrap();
    assert_eq!(generation(&mut rx).await, 4);
    handler.cleanup_connection(ctx.connection_id).unwrap();
}

#[tokio::test]
async fn catalog_watch_coalesces_behind_backpressure_without_blocking_registry_or_forwarding() {
    let registry = Arc::new(Registry::default());
    let handler = ControlHandler::new(registry.clone());
    let (tx, mut rx) = mpsc::channel(1);
    let ctx = RouteCtx {
        connection_id: ConnectionId::new(90),
        egress: crate::FrameSink::new(tx),
    };
    ctx.egress
        .try_send(Frame::build(FrameType::Pong, control_flags(), 0, 0, 0, vec![]).unwrap())
        .unwrap();
    watch(&handler, &ctx).await;
    for _ in 0..20 {
        registry.catalog_authority_changed().unwrap();
    }
    assert_eq!(handler.registry.generation().unwrap(), 20);
    assert!(handler
        .forwarding
        .module_endpoint_for_connection(ctx.connection_id)
        .unwrap()
        .is_none());
    assert_eq!(rx.recv().await.unwrap().header.ty, FrameType::Pong);
    assert_eq!(
        generation(&mut rx).await,
        0,
        "first push is the captured current generation"
    );
    assert_eq!(
        generation(&mut rx).await,
        20,
        "intermediate advances are not replayed"
    );
    assert!(rx.try_recv().is_err());
    handler.cleanup_connection(ctx.connection_id).unwrap();
}

#[tokio::test]
async fn catalog_watch_advertised_and_cancelled_on_channel_zero() {
    let handler = ControlHandler::new(Arc::new(Registry::default()));
    let (wire, _) = catalog(&handler, None);
    let ClientControlResponse::CatalogList { subc_ops, .. } =
        serde_json::from_slice(&wire.body).unwrap()
    else {
        panic!("expected catalog");
    };
    assert!(subc_ops.iter().any(|op| op == ops::CATALOG_WATCH));
    let (ctx, mut rx) = route_ctx(ConnectionId::new(90));
    watch(&handler, &ctx).await;
    assert_eq!(generation(&mut rx).await, 0);
    let cancel = Frame::build(FrameType::Cancel, control_flags(), 0, 0, 77, vec![]).unwrap();
    assert!(handler
        .handle_control_frame(&ctx, cancel)
        .await
        .unwrap()
        .is_empty());
    assert!(handler.catalog_streams.lock().unwrap().is_empty());
    watch(&handler, &ctx).await;
    assert_eq!(generation(&mut rx).await, 0);
    handler.cleanup_connection(ctx.connection_id).unwrap();
    assert!(handler.catalog_streams.lock().unwrap().is_empty());
}

/// The bounded declaration, event, display and operation-list fields are full.
/// Legacy role schemas, operation names/descriptions, self-signal strings/counts,
/// capability counts, control-op lists and provenance strings have no universal
/// size cap. They are representative here; bounding them is a separate policy.
#[test]
fn catalog_provider_declarations_64_module_unfiltered_size_pin() {
    let handler = ControlHandler::new(Arc::new(Registry::default()));
    let mut declarations: serde_json::Map<String, Value> = (0..16)
        .map(|i| (format!("provider-{i:02}"), json!({})))
        .collect();
    declarations.insert(
        "provider-00".into(),
        json!({"text": "x".repeat(16 * 1024 - 11)}),
    );
    declarations.insert("provider-01".into(), json!({"text": ""}));
    let remaining = 24 * 1024 - serde_json::to_vec(&declarations).unwrap().len();
    declarations.insert("provider-01".into(), json!({"text": "x".repeat(remaining)}));
    assert_eq!(serde_json::to_vec(&declarations).unwrap().len(), 24 * 1024);
    let mut roles = json!({"management/v1": {"text": ""}});
    let remaining = 16 * 1024 - serde_json::to_vec(&roles).unwrap().len();
    roles["management/v1"]["text"] = json!("x".repeat(remaining));
    let event_name = vec!["a".repeat(63); 4].join(".");
    let events: Vec<_> = (0..64).map(|i| json!({
        "name": event_name, "version": u32::MAX - i,
        "headers": (0..16).map(|i| format!("h{i:02}{}", "h".repeat(29))).collect::<Vec<_>>(),
        "summary": "😀".repeat(200)
    })).collect();
    let operations: Vec<_> = (0..256)
        .map(|i| format!("op{i:03}{}", "x".repeat(250)))
        .collect();
    for i in 0..64 {
        let module = format!("{i:02}{}", "m".repeat(253));
        let manifest = json!({
            "module_id": module, "module_version": "0.1.0", "protocol_ver": PROTOCOL_VERSION,
            "display_name": "😀".repeat(40),
            "provides": [{"role": "management_surface", "operations": operations.iter().map(|name| json!({
                "name": name, "kind": "query", "description": "representative legacy description"
            })).collect::<Vec<_>>(), "config_schema": {}, "observability": [], "identity_scope": []}],
            "capabilities": {"provides": ["flow-scopes/v1"], "requires": [], "must_never_reach": []},
            "flow_operations": operations, "confirm_operations": operations,
            "operation_delivery": operations.iter().map(|name| (name.clone(), json!("watch"))).collect::<serde_json::Map<_,_>>(),
            "operation_deferral": operations.iter().map(|name| (format!("management:{name}"), json!(true))).collect::<serde_json::Map<_,_>>(),
            "events": events, "role_declarations": roles,
            "rich_ui": {"schema": "s".repeat(64), "manifest_digest": format!("sha256:{}", "f".repeat(64)), "bytes": 16_777_216, "chunks": 1024},
            "provider_declarations": declarations
        });
        let hello = json!({"manifest": manifest, "protocol_ver": PROTOCOL_VERSION});
        let frame = Frame::build(
            FrameType::Hello,
            control_flags(),
            0,
            0,
            i + 1,
            serde_json::to_vec(&hello).unwrap(),
        )
        .unwrap();
        let reply = handler
            .handle_control(ConnectionId::new(i + 1), frame)
            .unwrap();
        assert_eq!(
            reply[0].header.ty,
            FrameType::HelloAck,
            "{}",
            String::from_utf8_lossy(&reply[0].body)
        );
    }
    let (frame, modules) = catalog(&handler, None);
    assert_eq!(modules.len(), 64);
    for module in &modules {
        assert_eq!(module.first_party, Some(false));
        assert!(module.requested_declarations.is_none());
        assert!(module.approved_declarations.is_none());
        for digest in [
            &module.requested_declarations_digest,
            &module.approved_declarations_digest,
        ] {
            let digest = digest.as_ref().expect("fixed-size digest in every entry");
            assert_eq!(digest.len(), 64);
            assert!(digest
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
        }
        assert_eq!(module.events.as_ref().unwrap().len(), 64);
        assert_eq!(module.flow_operations.as_ref().unwrap().len(), 256);
        assert_eq!(module.confirm_operations.as_ref().unwrap().len(), 256);
    }
    println!("64-module unfiltered catalog bytes: {}", frame.body.len());
    assert!(frame.body.len() < 64 * 1024 * 1024);
}
