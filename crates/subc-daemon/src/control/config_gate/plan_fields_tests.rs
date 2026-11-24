//! Plans on an unstamped install with project settings on, as the live daemon
//! runs: the module is not asked to review, yet the plan must list every
//! changed field, and a schema refusal must name the rule it failed.

use super::*;
use crate::config_gate::{
    capture::{ConfigCapturer, OfflineProjectRegistry},
    runtime::RuntimeOwner,
};
use subc_control::config::ConfigPlanView;
use subc_protocol::config::ConfigDeclareRequest;

struct Fixture {
    handler: ControlHandler,
    ctx: RouteCtx,
    _root: cortexkit_test_support::ScratchDir,
}

fn fixture(document: &[u8]) -> Fixture {
    let root = cortexkit_test_support::ScratchDir::new("config-plan-fields");
    std::fs::create_dir_all(root.join("store")).unwrap();
    std::fs::create_dir_all(root.join("cortexkit")).unwrap();
    std::fs::write(root.join("program"), b"example program").unwrap();
    std::fs::write(root.join("cortexkit/demo.jsonc"), document).unwrap();
    std::fs::write(
        root.join("subc.jsonc"),
        serde_json::to_vec(&json!({"version":1,"modules":{"demo":{"program":root.join("program"),"registration_channel":true}}})).unwrap(),
    )
    .unwrap();
    let owner = RuntimeOwner::new(
        "fields-daemon".into(),
        crate::config_store::ConfigStore::open_digest_only_for_tests(
            crate::config_store::StoreRoot::from_service_definition(root.join("store")).unwrap(),
        ),
        ConfigCapturer {
            daemon_path: root.join("subc.jsonc"),
            config_home: root.to_path_buf(),
            projects: Arc::new(OfflineProjectRegistry),
        },
    )
    .enable_project_settings();
    let schema = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object", "unevaluatedProperties": false,
        "x-ck-layers": ["user"], "x-ck-apply": "live",
        "properties": {
            "min_chars": {"type": "integer", "description": "Minimum characters", "default": 250},
            "threshold": {
                "type": "object", "description": "Threshold per model",
                "unevaluatedProperties": false,
                "patternProperties": {"^.+$": {
                    "type": "integer", "description": "Threshold percentage",
                    "default": 50, "minimum": 20, "maximum": 90
                }}
            }
        }
    });
    owner
        .declare(
            ConnectionId::new(10),
            "demo",
            ConfigDeclareRequest::new(
                schema,
                root.join("cortexkit/demo.jsonc").to_string_lossy(),
                vec![],
            ),
            true,
        )
        .unwrap();
    // No review reply is configured: a plan that asked the module to review
    // would fail, so a passing plan proves the review was skipped.
    let handler =
        ControlHandler::new(Arc::new(Registry::default())).with_config_runtime(Arc::new(owner));
    handler.observed_peers.lock().unwrap().insert(
        ConnectionId::new(11),
        subc_protocol::peer::ObservedPeer::observed_uid(subc_os::effective_uid()),
    );
    let (tx, _rx) = tokio::sync::mpsc::channel(16);
    Fixture {
        handler,
        ctx: RouteCtx {
            connection_id: ConnectionId::new(11),
            egress: crate::FrameSink::new(tx),
        },
        _root: root,
    }
}

async fn plan(fixture: &Fixture, path: &str, action: Value) -> ConfigPlanView {
    let mut edit = json!({"module_id":"demo","layer":"user","path":path});
    edit.as_object_mut()
        .unwrap()
        .extend(action.as_object().unwrap().clone());
    let replies = fixture
        .handler
        .handle_control_frame(
            &fixture.ctx,
            frame(json!({"op":"config.plan","base_generations":{"demo":0},"edits":[edit]})),
        )
        .await
        .unwrap();
    let reply: Value = serde_json::from_slice(&replies[0].body).unwrap();
    assert_eq!(replies[0].header.ty, FrameType::Response, "{reply}");
    let view: ConfigPlanView = serde_json::from_value(reply["view"].clone()).unwrap();
    let discarded = fixture
        .handler
        .handle_control_frame(
            &fixture.ctx,
            frame(json!({"op":"config.plan_discard","digest":view.digest})),
        )
        .await
        .unwrap();
    assert_eq!(discarded[0].header.ty, FrameType::Response);
    view
}

/// The one changed field's path, old value and new value.
fn only_field(view: &ConfigPlanView) -> (String, Value, Value) {
    assert!(view.refusals.is_empty(), "{:?}", view.refusals);
    let fields = &view.plan.changeset.fields;
    assert_eq!(fields.len(), 1, "{fields:?}");
    let value = |recorded: &subc_config::RecordedValue| match recorded {
        subc_config::RecordedValue::Value { value } => value.clone(),
        other => panic!("unexpected redaction {other:?}"),
    };
    (
        fields[0].path.clone(),
        value(&fields[0].old),
        value(&fields[0].new),
    )
}

#[tokio::test]
async fn config_plan_unstamped_number_edit_lists_its_field_with_old_and_new() {
    let fixture = fixture(b"{\"min_chars\":250}\n");
    let view = plan(&fixture, "/min_chars", json!({"set":777})).await;
    assert_eq!(
        only_field(&view),
        ("/min_chars".into(), json!(250), json!(777))
    );
    let field = &view.plan.changeset.fields[0];
    assert_eq!(field.module_id, "demo");
    assert_eq!(field.old_source, subc_config::Layer::User);
    assert_eq!(field.new_source, subc_config::Layer::User);
    assert_eq!(field.apply, subc_config::ApplyKind::Live);
}

#[tokio::test]
async fn config_plan_unstamped_map_entry_change_add_and_remove_list_their_field() {
    let fixture = fixture(b"{\"threshold\":{\"model-a\":50,\"model-b\":60}}\n");
    assert_eq!(
        only_field(&plan(&fixture, "/threshold/model-a", json!({"set":70})).await),
        ("/threshold/model-a".into(), json!(50), json!(70))
    );
    assert_eq!(
        only_field(&plan(&fixture, "/threshold/model-c", json!({"set":40})).await),
        ("/threshold/model-c".into(), Value::Null, json!(40))
    );
    assert_eq!(
        only_field(&plan(&fixture, "/threshold/model-b", json!({"remove":true})).await),
        ("/threshold/model-b".into(), json!(60), Value::Null)
    );
}

#[tokio::test]
async fn config_plan_value_above_maximum_names_the_bound() {
    let fixture = fixture(b"{\"threshold\":{\"model-a\":50}}\n");
    let view = plan(&fixture, "/threshold/model-a", json!({"set":95})).await;
    assert_eq!(view.refusals.len(), 1);
    assert_eq!(view.refusals[0].code, "value_invalid");
    assert_eq!(view.refusals[0].path, "/threshold/model-a");
    assert_eq!(view.refusals[0].message, "must be at most 90");
}
