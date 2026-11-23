use super::*;
use subc_control::config::{ConfigEdit, ConfigPlanView};

fn rig() -> Rig {
    let mut rig = Rig::new();
    rig.declaration.schema["properties"]
        .as_object_mut()
        .unwrap()
        .remove("credential");
    rig
}
fn file(rig: &Rig) -> std::path::PathBuf {
    rig.path.join("cortexkit/a.jsonc")
}
fn edit(path: &str, action: serde_json::Value) -> ConfigEdit {
    let mut body = json!({"module_id":"a", "layer":"user", "path":path});
    body.as_object_mut()
        .unwrap()
        .extend(action.as_object().unwrap().clone());
    serde_json::from_value(body).unwrap()
}
fn plan(owner: &RuntimeOwner, generation: u64, edits: Vec<ConfigEdit>) -> ConfigPlanView {
    let prepared = owner
        .prepare_edits(BTreeMap::from([("a".into(), generation)]), edits)
        .unwrap();
    let mut changeset = Changeset::new();
    for (module, captured) in &prepared.candidate.modules {
        if prepared.review_modules.contains(module) {
            changeset.fields.extend(
                crate::config_gate::review::expected_fields(
                    module,
                    &captured.schema,
                    prepared.bases.get(module),
                    prepared.base_schemas.get(module),
                    &captured.view,
                )
                .unwrap()
                .into_values(),
            );
        }
    }
    owner
        .finish_plan(prepared, |old, new| {
            let mut reviewed = daemon_review(old, new)?;
            reviewed.fields = changeset.fields;
            Ok(reviewed)
        })
        .unwrap()
}
fn approve(owner: &RuntimeOwner, view: &ConfigPlanView) -> u64 {
    owner
        .desktop_view_delivered(ConnectionId::new(7), view)
        .unwrap();
    let challenge = owner.begin_approval(&view.digest, &Rig::facts()).unwrap();
    owner
        .finish_approval(
            challenge.attempt(),
            &Rig::facts(),
            ApprovalReply::Presence(crate::config_gate::PresenceDecision::InteractiveApproval),
            2,
        )
        .unwrap()
}

#[test]
fn config_edit_stamped_write_back_preserves_comment_and_get_view() {
    let rig = rig();
    let original = b"{\r\n  // Keep this unrelated comment exactly.\r\n  \"limit\": 1,\r\n}\r\n";
    std::fs::write(file(&rig), original).unwrap();
    let owner = rig.owner("edit-comments");
    rig.declare(&owner, 10);
    rig.stamp(&owner);
    let view = plan(&owner, 1, vec![edit("/limit", json!({"set":4}))]);
    assert_eq!(view, owner.plan_get(&view.digest).unwrap());
    assert!(view.refusals.is_empty());
    assert!(!serde_json::to_value(&view)
        .unwrap()
        .as_object()
        .unwrap()
        .contains_key("refusals"));
    assert_eq!(
        view.plan.changeset.fields[0].old,
        subc_config::RecordedValue::new(json!(1))
    );
    assert_eq!(
        view.plan.changeset.fields[0].new,
        subc_config::RecordedValue::new(json!(4))
    );
    assert_eq!(approve(&owner, &view), 2);
    assert_eq!(
        std::fs::read(file(&rig)).unwrap(),
        String::from_utf8(original.to_vec())
            .unwrap()
            .replace("\"limit\": 1", "\"limit\": 4")
            .as_bytes()
    );
    assert!(owner.write_back_status().unwrap().is_empty());
    assert_eq!(
        owner.plan_get(&view.digest).unwrap_err().code,
        "unknown_plan"
    );
}

#[test]
fn config_edit_stamped_concurrent_file_change_commits_without_clobber() {
    let rig = rig();
    std::fs::write(file(&rig), b"{\"limit\":1}").unwrap();
    let owner = rig.owner("edit-drift");
    rig.declare(&owner, 10);
    rig.stamp(&owner);
    let view = plan(&owner, 1, vec![edit("/limit", json!({"set":4}))]);
    let drift = b"{ // Someone is editing this file.\n \"limit\":99}";
    std::fs::write(file(&rig), drift).unwrap();
    assert_eq!(approve(&owner, &view), 2);
    assert_eq!(std::fs::read(file(&rig)).unwrap(), drift);
    assert_eq!(
        owner.write_back_status().unwrap()["a"],
        "the file changed after this was approved"
    );
    assert_eq!(
        owner.approved_snapshot().unwrap().modules["a"].effective["limit"],
        4
    );
}

#[test]
fn config_edit_ignores_preexisting_file_drift_including_invalid_json() {
    let rig = rig();
    std::fs::write(file(&rig), b"{\"limit\":2}").unwrap();
    let owner = rig.owner("edit-approved-only");
    rig.declare(&owner, 10);
    rig.stamp(&owner);
    std::fs::write(file(&rig), b"not JSON").unwrap();
    let view = plan(&owner, 1, vec![edit("/limit", json!({"set":4}))]);
    assert_eq!(
        view.plan.changeset.fields[0].old,
        subc_config::RecordedValue::new(json!(2))
    );
    assert_eq!(approve(&owner, &view), 2);
    assert_eq!(std::fs::read(file(&rig)).unwrap(), b"not JSON");
    assert!(owner.write_back_status().unwrap().contains_key("a"));
}

#[test]
fn config_edit_unstamped_apply_preserves_comments_without_store_publication() {
    let rig = rig();
    let text = b"{ // Keep this comment.\n \"limit\": 2}";
    std::fs::write(file(&rig), text).unwrap();
    let owner = rig.owner("edit-unstamped");
    rig.declare(&owner, 10);
    let before = std::fs::read(rig.path.join("store").join(crate::config_store::STORE_FILE)).ok();
    let view = plan(&owner, 0, vec![edit("/limit", json!({"set":5}))]);
    assert_eq!(owner.apply_unstamped_edits(&view.digest).unwrap(), Some(0));
    assert_eq!(
        std::fs::read(file(&rig)).unwrap(),
        b"{ // Keep this comment.\n \"limit\": 5}"
    );
    assert_eq!(
        std::fs::read(rig.path.join("store").join(crate::config_store::STORE_FILE)).ok(),
        before
    );
    assert_eq!(owner.status().unwrap().state, "unstamped");
}

#[test]
fn config_edit_unstamped_concurrent_file_change_is_refused() {
    let rig = rig();
    std::fs::write(file(&rig), b"{\"limit\":2}").unwrap();
    let owner = rig.owner("edit-cas");
    rig.declare(&owner, 10);
    let view = plan(&owner, 0, vec![edit("/limit", json!({"set":5}))]);
    std::fs::write(file(&rig), b"{\"limit\":3}").unwrap();
    assert_eq!(
        owner.apply_unstamped_edits(&view.digest).unwrap_err().code,
        "config_changed_since_review"
    );
    assert_eq!(std::fs::read(file(&rig)).unwrap(), b"{\"limit\":3}");
}

#[test]
fn config_edit_refusals_are_keyed_and_block_the_whole_plan() {
    let rig = rig();
    std::fs::write(file(&rig), b"{\"limit\":1}").unwrap();
    let owner = rig.owner("edit-refusals");
    rig.declare(&owner, 10);
    rig.stamp(&owner);
    let mut denied = edit("/limit", json!({"set":2}));
    denied.layer = "workspace".into();
    let mut daemon = edit("/port", json!({"set":5}));
    daemon.module_id = "subc".into();
    let prepared = owner
        .prepare_edits(
            BTreeMap::from([("a".into(), 1), ("subc".into(), 0)]),
            vec![
                edit("/limit", json!({"set":null})),
                edit("/unknown", json!({"remove":true})),
                denied,
                daemon,
            ],
        )
        .unwrap();
    let view = owner
        .finish_plan(prepared, |_, _| Ok(Changeset::new()))
        .unwrap();
    let rows = &view.refusals;
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[0].path, "/limit");
    assert_eq!(rows[0].code, "value_invalid");
    assert_eq!(rows[1].code, "value_invalid");
    assert_eq!(rows[2].code, "layer_not_served");
    assert_eq!(
        rows[2].message,
        "workspace and project settings are not served yet"
    );
    assert_eq!(rows[3].module_id, "subc");
    assert_eq!(rows[3].code, "value_invalid");
    assert_eq!(owner.plan_get(&view.digest).unwrap(), view);
    assert_eq!(
        owner
            .begin_approval(&view.digest, &Rig::facts())
            .unwrap_err()
            .code,
        "plan_refused"
    );
    assert_eq!(
        owner.apply_unstamped_edits(&view.digest).unwrap_err().code,
        "plan_refused"
    );
}

#[test]
fn config_edit_generation_checks_only_touched_modules() {
    let rig = rig();
    let owner = rig.owner("edit-generation");
    rig.declare(&owner, 10);
    rig.stamp(&owner);
    let edits = vec![edit("/limit", json!({"set":4}))];
    assert_eq!(
        owner
            .prepare_edits(BTreeMap::new(), edits.clone())
            .err()
            .unwrap()
            .code,
        "invalid_params"
    );
    assert_eq!(
        owner
            .prepare_edits(BTreeMap::from([("a".into(), 0)]), edits.clone())
            .err()
            .unwrap()
            .code,
        "config_changed_since_review"
    );
    assert!(owner
        .prepare_edits(
            BTreeMap::from([("a".into(), 1), ("other".into(), 999)]),
            edits
        )
        .is_ok());
}

#[test]
fn config_edit_discard_unknown_and_in_review() {
    let rig = rig();
    let owner = rig.owner("edit-discard");
    rig.declare(&owner, 10);
    rig.stamp(&owner);
    let view = plan(&owner, 1, vec![edit("/limit", json!({"set":4}))]);
    owner
        .desktop_view_delivered(ConnectionId::new(7), &view)
        .unwrap();
    let challenge = owner.begin_approval(&view.digest, &Rig::facts()).unwrap();
    assert_eq!(
        owner.plan_discard(&view.digest).unwrap_err().code,
        "plan_in_review"
    );
    owner
        .lock()
        .unwrap()
        .gate
        .cancel_attempt(challenge.attempt());
    owner.plan_discard(&view.digest).unwrap();
    assert_eq!(
        owner.plan_discard(&view.digest).unwrap_err().code,
        "unknown_plan"
    );
    assert_eq!(
        owner.plan_get(&view.digest).unwrap_err().code,
        "unknown_plan"
    );
}

#[test]
fn config_edit_creates_parents_decodes_pointers_and_removes_commas() {
    let mut rig = rig();
    rig.declaration.schema["properties"]["nested"] = json!({"type":"object","unevaluatedProperties":false,"properties":{"a/b~c":{"type":"integer","description":"Nested count","default":1}}});
    std::fs::write(file(&rig), b"{\n // Keep this comment.\n \"limit\":2,\n}").unwrap();
    let owner = rig.owner("edit-pointers");
    rig.declare(&owner, 10);
    let view = plan(
        &owner,
        0,
        vec![
            edit("/nested/a~1b~0c", json!({"set":7})),
            edit("/limit", json!({"remove":true})),
        ],
    );
    owner.apply_unstamped_edits(&view.digest).unwrap();
    let text = std::fs::read_to_string(file(&rig)).unwrap();
    assert!(text.contains("// Keep this comment.\n"));
    let value: serde_json::Value =
        serde_json::from_str(&subc_jsonc::jsonc_to_json(&text).unwrap()).unwrap();
    assert_eq!(value, json!({"nested":{"a/b~c":7}}));
    let remove = plan(&owner, 0, vec![edit("/nested", json!({"remove":true}))]);
    owner.apply_unstamped_edits(&remove.digest).unwrap();
    let text = std::fs::read_to_string(file(&rig)).unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&subc_jsonc::jsonc_to_json(&text).unwrap())
            .unwrap(),
        json!({})
    );
}

fn map_rig() -> Rig {
    let mut rig = rig();
    rig.declaration.schema["properties"]["queues"] = json!({
        "type": "object",
        "unevaluatedProperties": false,
        "patternProperties": {
            "^[a-z]+[.][a-z]+$": {
                "type": "object",
                "unevaluatedProperties": false,
                "properties": {
                    "weight": {"type": "integer", "description": "Weight", "default": 1},
                    "enabled": {"type": "boolean", "description": "Enabled", "default": true}
                }
            }
        }
    });
    std::fs::write(
        file(&rig),
        br#"{"queues":{"primary.queue":{"weight":2,"enabled":false},"backup.queue":{"weight":3}},"limit":2}"#,
    )
    .unwrap();
    rig
}

fn user_document(rig: &Rig) -> serde_json::Value {
    let text = std::fs::read_to_string(file(rig)).unwrap();
    serde_json::from_str(&subc_jsonc::jsonc_to_json(&text).unwrap()).unwrap()
}

#[test]
fn config_edit_map_set_creates_and_replaces_entries() {
    let rig = map_rig();
    let owner = rig.owner("edit-map-set");
    rig.declare(&owner, 10);
    let view = plan(
        &owner,
        0,
        vec![
            edit("/queues/new.queue", json!({"set":{"weight":4}})),
            edit("/queues/primary.queue", json!({"set":{"weight":5}})),
        ],
    );
    assert!(view.refusals.is_empty(), "{:?}", view.refusals);
    owner.apply_unstamped_edits(&view.digest).unwrap();
    assert_eq!(
        user_document(&rig),
        json!({"queues":{"primary.queue":{"weight":5},"backup.queue":{"weight":3},"new.queue":{"weight":4}},"limit":2})
    );
}

#[test]
fn config_edit_map_set_changes_only_the_named_entry_field() {
    let rig = map_rig();
    let owner = rig.owner("edit-map-field");
    rig.declare(&owner, 10);
    let view = plan(
        &owner,
        0,
        vec![edit("/queues/primary.queue/weight", json!({"set":6}))],
    );
    assert!(view.refusals.is_empty(), "{:?}", view.refusals);
    owner.apply_unstamped_edits(&view.digest).unwrap();
    assert_eq!(
        user_document(&rig),
        json!({"queues":{"primary.queue":{"weight":6,"enabled":false},"backup.queue":{"weight":3}},"limit":2})
    );
}

#[test]
fn config_edit_map_remove_deletes_only_the_named_entry() {
    let rig = map_rig();
    let owner = rig.owner("edit-map-remove");
    rig.declare(&owner, 10);
    let view = plan(
        &owner,
        0,
        vec![edit("/queues/primary.queue", json!({"remove":true}))],
    );
    assert!(view.refusals.is_empty(), "{:?}", view.refusals);
    owner.apply_unstamped_edits(&view.digest).unwrap();
    assert_eq!(
        user_document(&rig),
        json!({"queues":{"backup.queue":{"weight":3}},"limit":2})
    );
}

#[test]
fn config_edit_map_refuses_a_key_that_matches_no_pattern() {
    let rig = map_rig();
    let owner = rig.owner("edit-map-unknown");
    rig.declare(&owner, 10);
    let path = "/queues/unmatched";
    let view = plan(
        &owner,
        0,
        vec![
            edit(path, json!({"set":{"weight":4}})),
            edit(path, json!({"remove":true})),
        ],
    );
    assert_eq!(view.refusals.len(), 2);
    for refusal in &view.refusals {
        assert_eq!(refusal.path, path);
        assert_eq!(refusal.code, "value_invalid");
        assert_eq!(
            refusal.message,
            "the path is not declared by the admitted schema"
        );
    }
}

#[test]
fn config_edit_refuses_invalid_pointers_arrays_layers_and_unknown_modules() {
    let mut rig = rig();
    rig.declaration.schema["properties"]["items"] = json!({"type":"array","description":"Items","default":[],"items":{"type":"integer","description":"Item","default":0}});
    let owner = rig.owner("edit-errors");
    rig.declare(&owner, 10);
    let mut project = edit("/limit", json!({"set":2}));
    project.layer = "project".into();
    let mut invalid_layer = project.clone();
    invalid_layer.layer = "default".into();
    let mut invalid_identity = edit("/limit", json!({"set":2}));
    invalid_identity.project_id = Some("project-one".into());
    let mut unknown = edit("/limit", json!({"set":2}));
    unknown.module_id = "missing".into();
    let prepared = owner
        .prepare_edits(
            BTreeMap::from([("a".into(), 0), ("missing".into(), 0)]),
            vec![
                edit("bad", json!({"set":2})),
                edit("/bad~2", json!({"set":2})),
                edit("/items/0", json!({"set":2})),
                project,
                invalid_layer,
                invalid_identity,
                unknown,
            ],
        )
        .unwrap();
    let view = owner
        .finish_plan(prepared, |_, _| Ok(Changeset::new()))
        .unwrap();
    let codes: Vec<_> = view.refusals.iter().map(|row| row.code.as_str()).collect();
    assert_eq!(
        codes,
        [
            "invalid_params",
            "invalid_params",
            "invalid_params",
            "layer_not_served",
            "invalid_params",
            "invalid_params",
            "unknown_module"
        ]
    );
    assert_eq!(view.refusals[2].path, "/items/0");
    assert_eq!(view.refusals[2].message, "arrays must be set whole");
}

#[test]
fn config_edit_global_plan_limit_includes_edit_plans() {
    let rig = rig();
    let owner = rig.owner("edit-capacity");
    rig.declare(&owner, 10);
    let mut first = None;
    for limit in 2..66 {
        let view = plan(&owner, 0, vec![edit("/limit", json!({"set":limit}))]);
        first.get_or_insert(view.digest);
    }
    let prepared = owner
        .prepare_edits(
            BTreeMap::from([("a".into(), 0)]),
            vec![edit("/limit", json!({"set":66}))],
        )
        .unwrap();
    assert_eq!(
        owner
            .finish_plan(prepared, |_, _| Ok(Changeset::new()))
            .unwrap_err()
            .code,
        "config_busy"
    );
    owner.plan_discard(&first.unwrap()).unwrap();
    let _ = plan(&owner, 0, vec![edit("/limit", json!({"set":66}))]);
}

#[test]
fn config_edit_plan_expiry_and_supersession_are_unknown_to_readers() {
    let rig = rig();
    let owner = rig.owner("edit-lifetime");
    rig.declare(&owner, 10);
    rig.stamp(&owner);
    let expired = plan(&owner, 1, vec![edit("/limit", json!({"set":3}))]);
    owner.lock().unwrap().plan_deadlines.insert(
        expired.digest.clone(),
        Instant::now() - Duration::from_secs(1),
    );
    owner.sweep_expired().unwrap();
    assert_eq!(
        owner.plan_get(&expired.digest).unwrap_err().code,
        "unknown_plan"
    );
    let superseded = plan(&owner, 1, vec![edit("/limit", json!({"set":3}))]);
    let applied = plan(&owner, 1, vec![edit("/limit", json!({"set":4}))]);
    approve(&owner, &applied);
    assert_eq!(
        owner.plan_get(&superseded.digest).unwrap_err().code,
        "unknown_plan"
    );
    assert_eq!(
        owner.plan_discard(&superseded.digest).unwrap_err().code,
        "unknown_plan"
    );
}

#[test]
fn config_edit_write_back_intents_recover_after_restart() {
    let rig = rig();
    let original = b"{ // Keep this comment.\n \"limit\":1}";
    std::fs::write(file(&rig), original).unwrap();
    let owner = rig.owner("edit-intent-before-restart");
    rig.declare(&owner, 10);
    rig.stamp(&owner);
    let view = plan(&owner, 1, vec![edit("/limit", json!({"set":4}))]);
    let intent = &view.plan.changeset.apply_plan["edit_plan"]["write_back"][0];
    assert_eq!(intent["path"], json!(file(&rig)));
    assert_eq!(
        intent["plan_time_digest"],
        json!(subc_config::raw_digest(original))
    );
    // Simulate a process ending after durable approval but before file reconciliation.
    // The immutable intent in the reviewed plan and delivery bytes remain intact.
    owner
        .lock()
        .unwrap()
        .gate
        .plans
        .get_mut(&view.digest)
        .unwrap()
        .edits
        .as_mut()
        .unwrap()
        .files
        .clear();
    approve(&owner, &view);
    assert_eq!(std::fs::read(file(&rig)).unwrap(), original);
    drop(owner);
    let restarted = rig.owner("edit-intent-after-restart");
    assert!(restarted.write_back_status().unwrap().is_empty());
    assert_eq!(
        std::fs::read(file(&rig)).unwrap(),
        b"{ // Keep this comment.\n \"limit\":4}"
    );
    assert!(restarted.write_back_status().unwrap().is_empty());
    std::fs::write(file(&rig), b"{\"limit\":8}").unwrap();
    assert_eq!(
        restarted.write_back_status().unwrap()["a"],
        "the file changed after this was approved"
    );
    assert_eq!(std::fs::read(file(&rig)).unwrap(), b"{\"limit\":8}");
}

#[test]
fn config_edit_write_back_retry_failure_is_reported() {
    let rig = rig();
    let original = b"{\"limit\":1}";
    std::fs::write(file(&rig), original).unwrap();
    let owner = rig.owner("edit-intent-failed-retry");
    rig.declare(&owner, 10);
    rig.stamp(&owner);
    let view = plan(&owner, 1, vec![edit("/limit", json!({"set":4}))]);
    owner
        .lock()
        .unwrap()
        .gate
        .plans
        .get_mut(&view.digest)
        .unwrap()
        .edits
        .as_mut()
        .unwrap()
        .files
        .clear();
    approve(&owner, &view);
    // Keep the same content but replace the file object, so the write-back identity check fails.
    let replacement = rig.path.join("replacement.jsonc");
    std::fs::write(&replacement, original).unwrap();
    std::fs::rename(replacement, file(&rig)).unwrap();
    assert_eq!(
        owner.write_back_status().unwrap()["a"],
        "the approved settings could not be safely written back"
    );
    assert_eq!(std::fs::read(file(&rig)).unwrap(), original);
}

#[test]
fn config_edit_preexisting_dirty_file_records_visible_skipped_intent() {
    let rig = rig();
    std::fs::write(file(&rig), b"{\"limit\":1}").unwrap();
    let owner = rig.owner("edit-intent-dirty");
    rig.declare(&owner, 10);
    rig.stamp(&owner);
    let dirty = b"{ // Work in progress.\n \"limit\":9}";
    std::fs::write(file(&rig), dirty).unwrap();
    let view = plan(&owner, 1, vec![edit("/limit", json!({"set":4}))]);
    assert_eq!(
        view.plan.changeset.apply_plan["edit_plan"]["write_back"][0]["note"],
        "the file has edits that aren't approved yet"
    );
    approve(&owner, &view);
    assert_eq!(std::fs::read(file(&rig)).unwrap(), dirty);
    drop(owner);
    let restarted = rig.owner("edit-intent-dirty-restart");
    assert_eq!(
        restarted.write_back_status().unwrap()["a"],
        "the file has edits that aren't approved yet"
    );
    assert_eq!(std::fs::read(file(&rig)).unwrap(), dirty);
}

#[test]
fn config_edit_unrelated_module_movement_is_carried_forward() {
    let rig = rig();
    let owner = rig.owner("edit-per-module-base");
    rig.declare(&owner, 10);
    let mut declaration = rig.declaration.clone();
    declaration.user_file = rig
        .path
        .join("cortexkit/b.jsonc")
        .to_string_lossy()
        .into_owned();
    owner
        .declare(ConnectionId::new(11), "b", declaration, true)
        .unwrap();
    rig.stamp(&owner);
    let prepared = owner
        .prepare_edits(
            BTreeMap::from([("a".into(), 1)]),
            vec![edit("/limit", json!({"set":4}))],
        )
        .unwrap();
    let mut other = edit("/limit", json!({"set":8}));
    other.module_id = "b".into();
    let other = owner
        .prepare_edits(BTreeMap::from([("b".into(), 1)]), vec![other])
        .unwrap();
    let view = owner
        .finish_plan(other, |_, _| Ok(Changeset::new()))
        .unwrap();
    approve(&owner, &view);
    let view = owner
        .finish_plan(prepared, |_, _| Ok(Changeset::new()))
        .unwrap();
    approve(&owner, &view);
    let approved = owner.approved_snapshot().unwrap();
    assert_eq!(approved.modules["a"].effective["limit"], 4);
    assert_eq!(approved.modules["b"].effective["limit"], 8);
}

#[test]
fn config_edit_layer_policy_refuses_even_removing_an_absent_key() {
    let mut rig = rig();
    rig.declaration.schema["x-ck-layers"] = json!(["user", "project"]);
    rig.declaration.schema["properties"]["limit"]["x-ck-layers"] = json!(["project"]);
    let owner = rig.owner("edit-layer-policy");
    rig.declare(&owner, 10);
    let view = plan(&owner, 0, vec![edit("/limit", json!({"remove":true}))]);
    assert_eq!(view.refusals[0].code, "layer_forbidden");
    assert_eq!(view.refusals[0].path, "/limit");
}

#[test]
fn config_edit_uses_admitted_schema_and_reviews_its_transition() {
    let mut rig = rig();
    let owner = rig.owner("edit-admitted-schema");
    rig.declare(&owner, 10);
    rig.stamp(&owner);
    rig.declaration.schema["properties"]["extra"] =
        json!({"type":"integer","description":"Extra count","default":0});
    owner.disconnected(ConnectionId::new(10));
    rig.declare(&owner, 11);
    let view = plan(&owner, 1, vec![edit("/extra", json!({"set":7}))]);
    assert!(view.refusals.is_empty());
    let row = view
        .plan
        .changeset
        .fields
        .iter()
        .find(|row| row.path == "/extra")
        .unwrap();
    assert_eq!(row.new, subc_config::RecordedValue::new(json!(7)));
    let declaration = view
        .plan
        .changeset
        .policy_changes
        .iter()
        .find(|row| row["path"] == "declaration")
        .unwrap();
    assert_eq!(
        declaration["new"]["value"]["schema"]["properties"]["extra"]["default"],
        0
    );
    assert!(declaration["old"]["value"]["schema"]["properties"]["extra"].is_null());
}

#[test]
fn config_edit_touched_module_movement_during_review_is_refused() {
    let rig = rig();
    let owner = rig.owner("edit-stale-module");
    rig.declare(&owner, 10);
    rig.stamp(&owner);
    let prepared = owner
        .prepare_edits(
            BTreeMap::from([("a".into(), 1)]),
            vec![edit("/limit", json!({"set":3}))],
        )
        .unwrap();
    let current = plan(&owner, 1, vec![edit("/limit", json!({"set":4}))]);
    approve(&owner, &current);
    assert_eq!(
        owner
            .finish_plan(prepared, |_, _| Ok(Changeset::new()))
            .unwrap_err()
            .code,
        "config_changed_since_review"
    );
}

#[test]
fn config_edit_unstamped_no_op_is_a_valid_plan() {
    let rig = rig();
    std::fs::write(file(&rig), b"{\"limit\":1}").unwrap();
    let owner = rig.owner("edit-no-op");
    rig.declare(&owner, 10);
    let view = plan(&owner, 0, vec![edit("/limit", json!({"set":1}))]);
    assert!(view.refusals.is_empty());
    assert!(view.plan.changeset.fields.is_empty());
    assert_eq!(owner.apply_unstamped_edits(&view.digest).unwrap(), Some(0));
    assert_eq!(std::fs::read(file(&rig)).unwrap(), b"{\"limit\":1}");
}
