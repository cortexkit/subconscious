use super::*;
use crate::config_gate::{
    capture::{ConfigCapturer, OfflineProjectRegistry},
    runtime::RuntimeOwner,
};
use subc_control::config::ConfigPlanView;
use subc_protocol::config::{ConfigDeclareRequest, ConfigReviewReply};

fn normalize_capture(value: Value, root: &str, fingerprints: bool) -> Value {
    match value {
        Value::Object(map) => {
            let file_id = map.get("platform").and_then(Value::as_str) == Some("unix");
            let mut entries: Vec<_> = map.into_iter().collect();
            entries.sort_by(|left, right| left.0.cmp(&right.0));
            Value::Object(
                entries
                    .into_iter()
                    .map(|(key, value)| {
                        let value = if fingerprints && key == "digest" {
                            json!(format!("sha256:{}", "a".repeat(64)))
                        } else if file_id && matches!(key.as_str(), "device" | "inode") {
                            json!("1")
                        } else {
                            normalize_capture(
                                value,
                                root,
                                fingerprints
                                    || matches!(
                                        key.as_str(),
                                        "state" | "expected" | "input_vector" | "reviewed_programs"
                                    ),
                            )
                        };
                        (key.replace(root, "/home/example"), value)
                    })
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|value| normalize_capture(value, root, fingerprints))
                .collect(),
        ),
        Value::String(value) if fingerprints && value.starts_with("sha256:") => {
            json!(format!("sha256:{}", "a".repeat(64)))
        }
        Value::String(value) => Value::String(value.replace(root, "/home/example")),
        other => other,
    }
}

fn normalized_view(view: ConfigPlanView, root: &str) -> ConfigPlanView {
    let mut plan: subc_config::Plan = serde_json::from_value(normalize_capture(
        serde_json::to_value(view.plan).unwrap(),
        root,
        false,
    ))
    .unwrap();
    plan.daemon_incarnation = "example-daemon".into();
    let mut normalized = ConfigPlanView::new(plan).unwrap();
    normalized.refusals = view.refusals;
    normalized
}

async fn call(handler: &ControlHandler, ctx: &RouteCtx, body: Value) -> Value {
    let replies = handler
        .handle_control_frame(ctx, frame(body))
        .await
        .unwrap();
    assert_eq!(replies.len(), 1);
    let reply: Value = serde_json::from_slice(&replies[0].body).unwrap();
    assert_eq!(replies[0].header.ty, FrameType::Response, "{reply}");
    reply
}

async fn refused_call(handler: &ControlHandler, ctx: &RouteCtx, body: Value, code: &str) -> Value {
    let replies = handler
        .handle_control_frame(ctx, frame(body))
        .await
        .unwrap();
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0].header.ty, FrameType::Error);
    let reply: Value = serde_json::from_slice(&replies[0].body).unwrap();
    assert_eq!(reply["code"], code);
    reply
}

#[tokio::test]
async fn config_plan_vectors_match_real_handler_bytes() {
    use sha2::{Digest as _, Sha256};
    let root = cortexkit_test_support::ScratchDir::new("config-plan-vectors");
    std::fs::create_dir_all(root.join("store")).unwrap();
    std::fs::create_dir_all(root.join("cortexkit")).unwrap();
    std::fs::write(root.join("program"), b"example program").unwrap();
    std::fs::write(
        root.join("cortexkit/demo.jsonc"),
        b"{ // Keep this comment.\n \"limit\":1}\n",
    )
    .unwrap();
    std::fs::write(root.join("subc.jsonc"), serde_json::to_vec(&json!({"version":1,"modules":{"demo":{"program":root.join("program"),"registration_channel":true}}})).unwrap()).unwrap();
    let owner = Arc::new(RuntimeOwner::new(
        "vector-daemon".into(),
        crate::config_store::ConfigStore::open_digest_only_for_tests(
            crate::config_store::StoreRoot::from_service_definition(root.join("store")).unwrap(),
        ),
        ConfigCapturer {
            daemon_path: root.join("subc.jsonc"),
            config_home: root.to_path_buf(),
            projects: Arc::new(OfflineProjectRegistry),
        },
    ));
    owner.declare(ConnectionId::new(10), "demo", ConfigDeclareRequest::new(json!({"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","unevaluatedProperties":false,"x-ck-layers":["user"],"x-ck-apply":"live","properties":{"limit":{"type":"integer","description":"Limit","default":1}}}), root.join("cortexkit/demo.jsonc").to_string_lossy(), vec![]), true).unwrap();
    let mut handler = ControlHandler::new(Arc::new(Registry::default())).with_config_runtime(owner);
    handler.observed_peers.lock().unwrap().insert(
        ConnectionId::new(11),
        subc_protocol::peer::ObservedPeer::observed_uid(subc_os::effective_uid()),
    );
    let (tx, _rx) = tokio::sync::mpsc::channel(16);
    let ctx = RouteCtx {
        connection_id: ConnectionId::new(11),
        egress: crate::FrameSink::new(tx),
    };
    let mut field = subc_config::FieldChange::new(
        "demo",
        "/limit",
        subc_config::RecordedValue::new(json!(1)),
        subc_config::RecordedValue::new(json!(4)),
        subc_config::ApplyKind::Live,
    );
    field.old_source = subc_config::Layer::User;
    field.new_source = subc_config::Layer::User;
    handler.config_review_test_reply = Some(ConfigReviewReply::new(vec![field], vec![]));
    let request = json!({"op":"config.plan","base_generations":{"demo":0},"edits":[{"module_id":"demo","layer":"user","path":"/limit","set":4}]});
    let reply = call(&handler, &ctx, request.clone()).await;
    let actual: ConfigPlanView = serde_json::from_value(reply["view"].clone()).unwrap();
    assert!(
        reply["view"].get("refusals").is_none(),
        "empty refusals must be absent"
    );
    let normalized = normalized_view(actual.clone(), root.to_str().unwrap());
    assert!(normalized.refusals.is_empty());
    assert_eq!(normalized.digest, normalized.plan.digest().unwrap());
    let mut cases = BTreeMap::new();
    cases.insert(
        "plan-edit.json",
        json!({"request":request,"reply":{"op":"config.plan","view":normalized}}),
    );
    let get = call(
        &handler,
        &ctx,
        json!({"op":"config.plan_get","digest":actual.digest}),
    )
    .await;
    let fetched: ConfigPlanView = serde_json::from_value(get["view"].clone()).unwrap();
    assert_eq!(fetched, actual);
    cases.insert("plan-get.json", json!({"request":{"op":"config.plan_get","digest":normalized.digest},"reply":{"op":"config.plan_get","view":normalized}}));
    let discard = call(
        &handler,
        &ctx,
        json!({"op":"config.plan_discard","digest":actual.digest}),
    )
    .await;
    cases.insert(
        "plan-discard.json",
        json!({"request":{"op":"config.plan_discard","digest":normalized.digest},"reply":discard}),
    );
    handler.config_review_test_reply = Some(ConfigReviewReply::new(vec![], vec![]));
    let request = json!({"op":"config.plan","base_generations":{"demo":0},"edits":[{"module_id":"demo","layer":"user","path":"/limit","set":null}]});
    let refused = call(&handler, &ctx, request.clone()).await;
    assert_eq!(
        refused["view"]["refusals"],
        json!([{
            "module_id":"demo", "path":"/limit", "code":"value_invalid",
            "message":"the edit is refused by the admitted schema"
        }])
    );
    let refused_get = call(
        &handler,
        &ctx,
        json!({"op":"config.plan_get","digest":refused["view"]["digest"]}),
    )
    .await;
    assert_eq!(refused_get["view"], refused["view"]);
    let refused = normalized_view(
        serde_json::from_value(refused["view"].clone()).unwrap(),
        root.to_str().unwrap(),
    );
    assert_eq!(refused.refusals.len(), 1);
    cases.insert(
        "plan-refused.json",
        json!({"request":request,"reply":{"op":"config.plan","view":refused}}),
    );
    handler.config_review_test_reply = Some(ConfigReviewReply::new(
        vec![],
        vec![subc_protocol::config::ConfigReviewRefusal::new(
            "out_of_bounds",
            vec!["/limit".into()],
            vec!["untrusted reviewer text".into()],
        )],
    ));
    let reviewed = call(&handler, &ctx, json!({"op":"config.plan","base_generations":{"demo":0},"edits":[{"module_id":"demo","layer":"user","path":"/limit","set":4}]})).await;
    assert_eq!(
        reviewed["view"]["refusals"][0]["code"],
        "module_review_refused"
    );
    assert!(!reviewed.to_string().contains("untrusted reviewer text"));
    for body in [
        json!({"op":"config.plan","base_generations":{"demo":0}}),
        json!({"op":"config.plan","edits":[]}),
    ] {
        let replies = handler
            .handle_control_frame(&ctx, frame(body))
            .await
            .unwrap();
        assert_eq!(replies[0].header.ty, FrameType::Error);
        let error: Value = serde_json::from_slice(&replies[0].body).unwrap();
        assert_eq!(error["code"], "invalid_params");
    }

    let request = json!({"op":"config.plan","base_generations":{"demo":1},"edits":[{"module_id":"demo","layer":"user","path":"/limit","set":4}]});
    let reply = refused_call(
        &handler,
        &ctx,
        request.clone(),
        "config_changed_since_review",
    )
    .await;
    cases.insert(
        "plan-base-changed.json",
        json!({"request":request,"reply":reply}),
    );

    let mut field = subc_config::FieldChange::new(
        "demo",
        "/limit",
        subc_config::RecordedValue::new(json!(1)),
        subc_config::RecordedValue::new(json!(4)),
        subc_config::ApplyKind::Live,
    );
    field.old_source = subc_config::Layer::User;
    field.new_source = subc_config::Layer::User;
    handler.config_review_test_reply = Some(ConfigReviewReply::new(vec![field], vec![]));
    let edit_request = json!({"op":"config.plan","base_generations":{"demo":0},"edits":[{"module_id":"demo","layer":"user","path":"/limit","set":4}]});
    let planned = call(&handler, &ctx, edit_request.clone()).await;
    let view = normalized_view(
        serde_json::from_value(planned["view"].clone()).unwrap(),
        root.to_str().unwrap(),
    );
    std::fs::write(root.join("cortexkit/demo.jsonc"), b"{\"limit\":2}").unwrap();
    let reply = refused_call(
        &handler,
        &ctx,
        json!({"op":"config.apply","digest":planned["view"]["digest"]}),
        "config_changed_since_review",
    )
    .await;
    cases.insert(
        "apply-changed.json",
        json!({"request":{"op":"config.apply","digest":view.digest},"reply":reply}),
    );

    std::fs::write(
        root.join("cortexkit/demo.jsonc"),
        b"{ // Keep this comment.\n \"limit\":1}\n",
    )
    .unwrap();
    let planned = call(&handler, &ctx, edit_request).await;
    let view = normalized_view(
        serde_json::from_value(planned["view"].clone()).unwrap(),
        root.to_str().unwrap(),
    );
    let reply = call(
        &handler,
        &ctx,
        json!({"op":"config.apply","digest":planned["view"]["digest"]}),
    )
    .await;
    assert_eq!(
        reply,
        json!({"op":"config.apply","generation":0,"restart_required":false})
    );
    let written = std::fs::read_to_string(root.join("cortexkit/demo.jsonc")).unwrap();
    assert!(written.contains("// Keep this comment."));
    assert!(written.contains("\"limit\":4"));
    cases.insert(
        "apply-unstamped.json",
        json!({"request":{"op":"config.apply","digest":view.digest},"reply":reply}),
    );
    let directory =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vectors/config-settings-v1");
    let update = std::env::var_os("UPDATE_GOLDEN").is_some();
    let mut sums = String::new();
    for (name, sample) in cases {
        let sample = normalize_capture(sample, root.to_str().unwrap(), false);
        let mut bytes = serde_json::to_vec_pretty(&sample).unwrap();
        bytes.push(b'\n');
        if update {
            std::fs::create_dir_all(&directory).unwrap();
            std::fs::write(directory.join(name), &bytes).unwrap();
        }
        assert_eq!(
            std::fs::read(directory.join(name)).unwrap(),
            bytes,
            "{name}; regenerate with UPDATE_GOLDEN=1"
        );
        sums.push_str(&format!("{:x}  {name}\n", Sha256::digest(&bytes)));
    }
    if update {
        let mut combined: Vec<String> = std::fs::read_to_string(directory.join("SHA256SUMS"))
            .unwrap_or_default()
            .lines()
            .filter(|line| {
                !line.split_once("  ").is_some_and(|(_, name)| {
                    name.starts_with("plan-") || name.starts_with("apply-")
                })
            })
            .map(str::to_owned)
            .collect();
        combined.extend(sums.lines().map(str::to_owned));
        combined.sort_by(|left, right| {
            left.split_once("  ")
                .map(|(_, name)| name)
                .cmp(&right.split_once("  ").map(|(_, name)| name))
        });
        std::fs::write(
            directory.join("SHA256SUMS"),
            format!("{}\n", combined.join("\n")),
        )
        .unwrap();
    }
    let saved = std::fs::read_to_string(directory.join("SHA256SUMS")).unwrap();
    let own_rows: Vec<_> = saved
        .lines()
        .filter(|line| {
            line.split_once("  ")
                .is_some_and(|(_, name)| name.starts_with("plan-") || name.starts_with("apply-"))
        })
        .collect();
    assert_eq!(format!("{}\n", own_rows.join("\n")), sums);
}
