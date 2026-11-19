use super::*;
use crate::config_gate::{apply::Transition, GateSnapshot, InputKey};
use subc_config::{Layer, RecordedValue};
use subc_protocol::config::ConfigReviewReply;

fn rig() -> Rig {
    let mut rig = Rig::new();
    rig.declaration.schema["properties"]
        .as_object_mut()
        .unwrap()
        .remove("credential");
    rig
}

fn layer(rig: &Rig, bytes: &[u8]) {
    std::fs::write(rig.path.join("cortexkit/a.jsonc"), bytes).unwrap();
}

fn activate(owner: &RuntimeOwner, generation: u64) {
    owner
        .lock()
        .unwrap()
        .store
        .record_delivery(
            &crate::config_store::Tuple {
                module: "a".into(),
                project: None,
            },
            generation,
            true,
        )
        .unwrap();
}

fn approve(owner: &RuntimeOwner, plan: &ConfigPlanView) -> u64 {
    owner
        .desktop_view_delivered(ConnectionId::new(7), plan)
        .unwrap();
    let challenge = owner.begin_approval(&plan.digest, &Rig::facts()).unwrap();
    owner
        .finish_approval(
            challenge.attempt(),
            &Rig::facts(),
            ApprovalReply::Presence(super::super::super::PresenceDecision::InteractiveApproval),
            2,
        )
        .unwrap()
}

// Exercise the same immutable-base validation and finishing path as the control
// handler, without requiring a live module transport in these store-level tests.
fn plan(owner: &RuntimeOwner, kind: PlanKind) -> ConfigPlanView {
    let prepared = owner.prepare(kind).unwrap();
    let mut fields = vec![];
    for module in &prepared.review_modules {
        let captured = &prepared.candidate.modules[module];
        let base = prepared.bases.get(module);
        let schema = prepared.base_schemas.get(module);
        let reply = super::super::super::review::expected_fields(
            module,
            &captured.schema,
            base,
            schema,
            &captured.view,
        )
        .unwrap()
        .into_values()
        .collect();
        fields.extend(
            super::super::super::review::checked_fields(
                module,
                &captured.schema,
                base,
                schema,
                &captured.view,
                ConfigReviewReply::new(reply, vec![]),
            )
            .unwrap(),
        );
    }
    owner
        .finish_plan(prepared, |old, new| {
            let mut changeset = daemon_review(old, new)?;
            changeset.fields = fields;
            Ok(changeset)
        })
        .unwrap()
}

fn restart_rig() -> (Rig, RuntimeOwner) {
    let mut rig = rig();
    rig.declaration.schema["properties"]["limit"]["x-ck-apply"] = json!("module_restart");
    layer(&rig, br#"{"limit":1}"#);
    let owner = rig.owner("plan-rows");
    rig.declare(&owner, 10);
    assert_eq!(rig.stamp(&owner), 1);
    activate(&owner, 1);
    (rig, owner)
}

#[test]
fn config_plan_rows_pending_reversion_uses_last_approval_not_active() {
    let (rig, owner) = restart_rig();
    layer(&rig, br#"{"limit":2}"#);
    assert_eq!(approve(&owner, &plan(&owner, PlanKind::Change)), 2);
    layer(&rig, br#"{"limit":1}"#);
    let prepared = owner.prepare(PlanKind::Change).unwrap();
    assert_eq!(prepared.bases["a"].generation, 2);
    assert_eq!(prepared.bases["a"].status, ConfigStatus::Pending);
    let view = plan(&owner, PlanKind::Change);
    let rows = &view.plan.changeset.fields;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].path, "/limit");
    assert_eq!(rows[0].old, RecordedValue::new(json!(2)));
    assert_eq!(rows[0].new, RecordedValue::new(json!(1)));
    assert_eq!(
        (rows[0].old_source, rows[0].new_source),
        (Layer::User, Layer::User)
    );
    assert_eq!(
        Transition::for_module(&view.plan.changeset, "a").unwrap(),
        Transition::Live
    );
    assert!(view
        .plan
        .changeset
        .policy_changes
        .iter()
        .all(|row| row["kind"] != "layer_file"));
}

#[test]
fn config_plan_rows_pending_restart_is_bound_without_an_approved_value_row() {
    let (rig, owner) = restart_rig();
    layer(&rig, br#"{"limit":2}"#);
    assert_eq!(approve(&owner, &plan(&owner, PlanKind::Change)), 2);
    layer(&rig, b"{ // same pending value\n\"limit\":2}\n");
    let view = plan(&owner, PlanKind::Change);
    assert!(view.plan.changeset.fields.is_empty());
    assert_eq!(
        view.plan.changeset.apply_plan["module_transitions"]["a"]["transition"],
        "module_restart"
    );
    assert_eq!(
        Transition::for_module(&view.plan.changeset, "a").unwrap(),
        Transition::ModuleRestart
    );
    // Runtime policy must use the module transition recorded in apply_plan,
    // not reinterpret visible rows.
    let mut changeset = view.plan.changeset.clone();
    changeset.apply_plan["module_transitions"]["a"]["transition"] = json!("live");
    assert_eq!(
        Transition::for_module(&changeset, "a").unwrap(),
        Transition::Live
    );
}

#[test]
fn config_plan_rows_stamp_records_running_values_without_a_transition() {
    let mut rig = rig();
    rig.declaration.schema["properties"]["limit"]["x-ck-apply"] = json!("daemon_restart");
    layer(&rig, br#"{"limit":1}"#);
    let owner = rig.owner("stamp-running");
    rig.declare(&owner, 10);
    let connection = ConnectionId::new(10);
    let running = owner
        .module_get(connection, "a", ConfigGetRequest::new())
        .unwrap();
    assert_eq!(running.active.unwrap().generation, 0);
    let view = plan(&owner, PlanKind::Stamp);
    assert_eq!(
        view.plan.changeset.apply_plan["module_transitions"],
        json!({})
    );
    assert_eq!(
        Transition::for_module(&view.plan.changeset, "a").unwrap(),
        Transition::Live
    );
    assert_eq!(approve(&owner, &view), 1);
    let reply = owner
        .module_get(connection, "a", ConfigGetRequest::new())
        .unwrap();
    owner
        .module_view_delivered(connection, "a", &reply, tokio::time::Instant::now(), true)
        .unwrap();
    let probe = owner.health_probe(connection, "a").unwrap().unwrap();
    owner.health_result(probe, true).unwrap();
    let pending = reply
        .pending
        .expect("stamp delivers live without a new spawn");
    assert_eq!(
        owner
            .module_applied(
                connection,
                "a",
                subc_protocol::config::ConfigAppliedRequest::new(
                    pending.generation,
                    pending.attempt.unwrap(),
                    subc_protocol::config::ConfigApplyResult::Ack,
                    vec![],
                )
            )
            .unwrap(),
        AppliedDisposition::Active
    );
}

fn descriptor(bytes: Option<&[u8]>) -> serde_json::Value {
    bytes.map_or(json!({"absent":true}), |bytes| {
        json!({
            "sha256":subc_config::raw_digest(bytes).as_str().strip_prefix("sha256:").unwrap(),
            "bytes":bytes.len(),
        })
    })
}

#[test]
fn config_plan_rows_layer_files_expose_raw_changes_without_duplicate_field_rows() {
    for case in ["comment", "format", "identity", "absent"] {
        let rig = rig();
        let old = if case == "absent" {
            None
        } else {
            Some(b"{}".as_slice())
        };
        if let Some(old) = old {
            layer(&rig, old);
        }
        let owner = rig.owner("layer-rows");
        rig.declare(&owner, 10);
        rig.stamp(&owner);
        activate(&owner, 1);
        let new: &[u8] = match case {
            "comment" => b"{ // comment\n}\n",
            "format" => b"{  }\n",
            _ => b"{}",
        };
        if case == "identity" {
            let replacement = rig.path.join("replacement");
            std::fs::write(&replacement, new).unwrap();
            std::fs::rename(replacement, rig.path.join("cortexkit/a.jsonc")).unwrap();
        } else {
            layer(&rig, new);
        }
        let view = plan(&owner, PlanKind::Change);
        assert!(view.plan.changeset.fields.is_empty(), "{case}");
        let rows: Vec<_> = view
            .plan
            .changeset
            .policy_changes
            .iter()
            .filter(|row| row["kind"] == "layer_file")
            .collect();
        assert_eq!(rows.len(), 1, "{case}");
        assert_eq!(
            *rows[0],
            json!({"kind":"layer_file", "module_id":"a", "layer":"user", "old":descriptor(old), "new":descriptor(Some(new))})
        );
        let hash = rows[0]["new"]["sha256"].as_str().unwrap();
        assert_eq!(hash.len(), 64);
        assert!(hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)));
    }
}

fn assert_snapshot_differences_have_rows(view: &ConfigPlanView, case: &str) {
    let changeset = &view.plan.changeset;
    let previous = changeset.apply_plan["previous"].as_object().unwrap();
    let snapshot = changeset.apply_plan["snapshot"].as_object().unwrap();
    let keys: std::collections::BTreeSet<_> = previous.keys().chain(snapshot.keys()).collect();
    let mut differences = 0;
    for key in keys {
        if previous.get(key) == snapshot.get(key) {
            continue;
        }
        differences += 1;
        let input: InputKey = serde_json::from_str(key).unwrap();
        let covered = match &input {
            InputKey::Daemon => changeset
                .policy_changes
                .iter()
                .any(|row| row["path"] == "subc.jsonc"),
            InputKey::Declaration(module) => changeset
                .policy_changes
                .iter()
                .any(|row| row["path"] == "declaration" && row["module_id"] == *module),
            InputKey::Entry(module) => changeset
                .launch_manifests
                .get(module)
                .is_some_and(|row| row.get("entry").is_some()),
            InputKey::Program { module, path } => changeset
                .launch_manifests
                .get(module)
                .is_some_and(|row| row.get(format!("program:{path}")).is_some()),
            InputKey::Layer { module, .. } => {
                changeset.fields.iter().any(|row| {
                    row.module_id == *module
                        && (row.old_source == Layer::User || row.new_source == Layer::User)
                }) || changeset.policy_changes.iter().any(|row| {
                    row["kind"] == "layer_file"
                        && row["module_id"] == *module
                        && row["layer"] == "user"
                })
            }
            InputKey::ProjectTuple { module, project } => changeset.apply_plan["tuple_targets"]
                .as_array()
                .is_some_and(|targets| {
                    targets
                        .iter()
                        .any(|target| target["module"] == *module && target["project"] == *project)
                }),
        };
        assert!(covered, "{case}: uncovered snapshot input {input:?}");
    }
    assert!(
        differences > 0,
        "{case}: fixture must change a captured input"
    );
}

#[test]
fn config_plan_rows_every_store_difference_has_a_visible_row() {
    for case in [
        "value",
        "source",
        "pending_reversion",
        "comment",
        "absent",
        "declaration",
        "program",
        "daemon",
        "rollback",
    ] {
        let mut rig = rig();
        if case != "absent" {
            layer(
                &rig,
                if case == "source" {
                    b"{}"
                } else {
                    br#"{"limit":1}"#
                },
            );
        }
        let owner = rig.owner("row-inventory");
        rig.declare(&owner, 10);
        rig.stamp(&owner);
        activate(&owner, 1);
        let mut kind = PlanKind::Change;
        match case {
            "value" => layer(&rig, br#"{"limit":2}"#),
            "source" => layer(&rig, br#"{"limit":1}"#),
            "pending_reversion" => {
                layer(&rig, br#"{"limit":2}"#);
                assert_eq!(approve(&owner, &plan(&owner, kind)), 2);
                layer(&rig, br#"{"limit":1}"#);
            }
            "comment" => layer(&rig, b"{ // same setting\n\"limit\":1}"),
            "absent" => layer(&rig, b"{}"),
            "declaration" => {
                rig.declaration.schema["properties"]["limit"]["description"] =
                    json!("New limit description");
                owner.disconnected(ConnectionId::new(10));
                rig.declare(&owner, 11);
            }
            "program" => std::fs::write(rig.path.join("program"), b"changed program").unwrap(),
            "daemon" | "rollback" => {
                let path = rig.path.join("subc.jsonc");
                let mut doc: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                if case == "rollback" {
                    let program = rig.path.join("new-program");
                    std::fs::write(&program, b"new program").unwrap();
                    doc["modules"]["a"]["program"] = json!(program);
                } else {
                    doc["port"] = json!(31377);
                }
                std::fs::write(path, serde_json::to_vec(&doc).unwrap()).unwrap();
                if case == "rollback" {
                    assert_eq!(approve(&owner, &plan(&owner, kind)), 2);
                    kind = PlanKind::Rollback(1);
                }
            }
            _ => unreachable!(),
        }
        let view = plan(&owner, kind);
        assert_snapshot_differences_have_rows(&view, case);
        if case == "source" {
            let row = &view.plan.changeset.fields[0];
            assert_eq!(row.old, row.new);
            assert_eq!(
                (row.old_source, row.new_source),
                (Layer::Default, Layer::User)
            );
        }
        if case == "rollback" {
            let old_program = format!("program:{}", rig.path.join("program").display());
            assert!(view.plan.changeset.launch_manifests["a"]
                .get(old_program)
                .is_some());
            let previous = GateSnapshot::from_store(
                &serde_json::from_value(view.plan.changeset.apply_plan["previous"].clone())
                    .unwrap(),
            )
            .unwrap();
            assert!(previous.inputs.keys().any(
                |key| matches!(key, InputKey::Program { path, .. } if path.ends_with("new-program"))
            ));
        }
    }
}
