use super::*;
use crate::config_gate::{ApprovalFacts, ApprovalReply, PresenceDecision};
use serde_json::json;

pub(crate) struct SettingsRig {
    pub(crate) root: cortexkit_test_support::ScratchDir,
    pub(crate) owner: std::sync::Arc<RuntimeOwner>,
}

#[test]
fn config_settings_file_error_is_per_module_and_does_not_relax_planning() {
    let rig = SettingsRig::new();
    rig.declare("sample-extra", 11, true);
    std::fs::write(
        rig.root.join("cortexkit/sample-extra.jsonc"),
        br#"{"limit":6,"credential":{"vault_ref":"sample-reference"}}"#,
    )
    .unwrap();
    rig.stamp();
    rig.activate();
    rig.write(b"{");
    let ConfigControlResponse::Modules { modules, .. } =
        rig.owner.settings_modules(BTreeSet::new()).unwrap()
    else {
        panic!("expected module rows")
    };
    let broken = modules
        .iter()
        .find(|row| row.module_id == "sample-reader")
        .unwrap();
    assert_eq!(broken.file_error.as_ref().unwrap().code, "value_invalid");
    let healthy = modules
        .iter()
        .find(|row| row.module_id == "sample-extra")
        .unwrap();
    assert!(healthy.file_error.is_none());
    let shown = serde_json::to_value(
        rig.owner
            .settings_show(BTreeSet::new(), "sample-extra".into(), None, None)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(shown["effective"]["limit"], 6);
    let error = rig
        .owner
        .prepare(PlanKind::Change)
        .err()
        .expect("planning must still refuse invalid user files");
    assert_eq!(error.code, "value_invalid");
}

impl SettingsRig {
    pub(crate) fn new() -> Self {
        let root = cortexkit_test_support::ScratchDir::new("settings-reads");
        std::fs::create_dir_all(root.join("store")).unwrap();
        std::fs::create_dir_all(root.join("cortexkit")).unwrap();
        std::fs::write(root.join("subc.jsonc"), br#"{"version":1,"modules":{}}"#).unwrap();
        let owner = std::sync::Arc::new(RuntimeOwner::new(
            "settings".into(),
            ConfigStore::open_digest_only_for_tests(
                crate::config_store::StoreRoot::from_service_definition(root.join("store"))
                    .unwrap(),
            ),
            ConfigCapturer {
                daemon_path: root.join("subc.jsonc"),
                config_home: root.to_path_buf(),
                projects: std::sync::Arc::new(crate::config_gate::capture::OfflineProjectRegistry),
            },
        ));
        let rig = Self { root, owner };
        rig.declare("sample-reader", 10, true);
        rig.write(br#"{"limit":4,"group":{"enabled":true},"credential":{"vault_ref":"sample-reference"}}"#);
        rig
    }

    pub(crate) fn declare(&self, module: &str, connection: u64, title: bool) {
        let mut schema = json!({"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","unevaluatedProperties":false,"x-ck-layers":["user"],"x-ck-apply":"live","properties":{
            "limit":{"type":"integer","description":"Result limit","default":2},
            "group":{"type":"object","unevaluatedProperties":false,"properties":{"enabled":{"type":"boolean","description":"Enable group","default":false}}},
            "credential":{"type":"object","description":"Credential reference","x-ck-kind":"credential_ref","x-ck-required":true}
        }});
        if title {
            schema["title"] = json!("Sample reader");
        }
        self.owner
            .declare(
                ConnectionId::new(connection),
                module,
                ConfigDeclareRequest::new(
                    schema,
                    self.root
                        .join(format!("cortexkit/{module}.jsonc"))
                        .to_string_lossy(),
                    vec![],
                ),
                true,
            )
            .unwrap();
    }

    pub(crate) fn write(&self, bytes: &[u8]) {
        std::fs::write(self.root.join("cortexkit/sample-reader.jsonc"), bytes).unwrap();
    }

    pub(crate) fn stamp(&self) {
        self.stamp_reviewed(true);
    }

    fn stamp_reviewed(&self, reviewed: bool) {
        let mut prepared = self.owner.prepare(PlanKind::Stamp).unwrap();
        if !reviewed {
            prepared.review_modules.clear();
        }
        let plan = self.owner.finish_plan(prepared, daemon_review).unwrap();
        self.approve(&plan);
    }

    fn approve(&self, plan: &subc_control::config::ConfigPlanView) {
        let facts = ApprovalFacts {
            connection: 7,
            owner_uid: true,
            desktop_app: true,
            attested_operator_app: true,
            os_prompt_available: true,
            phone: None,
            caller: crate::caller_identity::CallerIdentity::default(),
        };
        self.owner
            .desktop_view_delivered(ConnectionId::new(7), plan)
            .unwrap();
        let challenge = self.owner.begin_approval(&plan.digest, &facts).unwrap();
        self.owner
            .finish_approval(
                challenge.attempt(),
                &facts,
                ApprovalReply::Presence(PresenceDecision::InteractiveApproval),
                1,
            )
            .unwrap();
    }

    pub(crate) fn activate(&self) {
        self.activate_generation(1);
    }

    pub(crate) fn activate_generation(&self, generation: u64) {
        self.owner
            .lock()
            .unwrap()
            .store
            .record_delivery(
                &crate::config_store::Tuple {
                    module: "sample-reader".into(),
                    project: None,
                },
                generation,
                true,
            )
            .unwrap();
    }

    pub(crate) fn edit_limit(&self, limit: u64) -> Vec<u8> {
        let edit = serde_json::from_value(json!({
            "module_id":"sample-reader", "layer":"user", "path":"/limit", "set":limit
        }))
        .unwrap();
        let prepared = self
            .owner
            .prepare_edits(BTreeMap::from([("sample-reader".into(), 1)]), vec![edit])
            .unwrap();
        let captured = &prepared.candidate.modules["sample-reader"];
        let fields = crate::config_gate::review::expected_fields(
            "sample-reader",
            &captured.schema,
            prepared.bases.get("sample-reader"),
            prepared.base_schemas.get("sample-reader"),
            &captured.view,
        )
        .unwrap()
        .into_values()
        .collect();
        let bytes = captured.file.bytes.clone().unwrap();
        let view = self
            .owner
            .finish_plan(prepared, |old, new| {
                let mut changeset = daemon_review(old, new)?;
                changeset.fields = fields;
                Ok(changeset)
            })
            .unwrap();
        self.approve(&view);
        bytes
    }

    fn show(&self, through: Option<ConfigThrough>) -> Value {
        serde_json::to_value(
            self.owner
                .settings_show(BTreeSet::new(), "sample-reader".into(), None, through)
                .unwrap(),
        )
        .unwrap()
    }
    fn modules(&self) -> Value {
        serde_json::to_value(
            self.owner
                .settings_modules(BTreeSet::from(["sample-legacy".into()]))
                .unwrap(),
        )
        .unwrap()
    }
}

#[test]
fn config_settings_missing_review_record_requires_rereview() {
    let rig = SettingsRig::new();
    rig.stamp_reviewed(false);
    let rows = rig.modules();
    assert_eq!(rows["modules"][1]["status"], "needs_rereview");
    assert_eq!(
        rows["modules"][1]["status_reason"],
        "The admitted program awaits configuration review."
    );
}

#[test]
fn config_settings_unstamped_files_and_stamped_approved_values_are_distinct() {
    let rig = SettingsRig::new();
    let direct = rig.show(None);
    assert_eq!(direct["stamped"], false);
    assert_eq!(direct["base_generation"], 0);
    assert_eq!(direct["effective"]["limit"], 4);
    assert_eq!(
        direct["effective"]["credential"]["vault_ref"],
        "sample-reference"
    );
    assert_eq!(rig.modules()["stamped"], false);
    rig.stamp();
    rig.write(br#"{"limit":99,"credential":{"vault_ref":"unreviewed-reference"}}"#);
    let approved = rig.show(None);
    assert_eq!(approved["stamped"], true);
    assert_eq!(approved["base_generation"], 1);
    assert_eq!(approved["effective"], direct["effective"]);
    assert_eq!(approved["layers"], direct["layers"]);
    let encoded = serde_json::to_string(&approved).unwrap();
    assert!(!encoded.contains("unreviewed-reference"));
    assert!(!encoded.contains("bytes"));
    assert!(!encoded.contains(".jsonc"));
}

#[test]
fn config_settings_layers_sources_and_through_default_user_workspace() {
    let rig = SettingsRig::new();
    rig.stamp();
    let full = rig.show(None);
    assert_eq!(full["layers"][0]["layer"], "default");
    assert_eq!(
        full["layers"][0]["document"],
        json!({"limit":2,"group":{"enabled":false}})
    );
    assert_eq!(full["layers"][1]["layer"], "user");
    assert_eq!(
        full["sources"],
        json!({"/limit":"user","/group/enabled":"user","/credential":"user"})
    );
    let default = rig.show(Some(ConfigThrough::Default));
    assert_eq!(default["layers"].as_array().unwrap().len(), 1);
    assert_eq!(default["effective"], full["layers"][0]["document"]);
    assert_eq!(
        default["sources"],
        json!({"/limit":"default","/group/enabled":"default"})
    );
    for through in [ConfigThrough::User, ConfigThrough::Workspace] {
        let truncated = rig.show(Some(through));
        assert_eq!(truncated["effective"], full["effective"]);
        assert_eq!(truncated["layers"], full["layers"]);
    }
}

#[test]
fn config_settings_schema_annotations_titles_ungated_and_refusals() {
    let rig = SettingsRig::new();
    let response = rig
        .owner
        .settings_schema(BTreeSet::new(), "sample-reader".into())
        .unwrap();
    let ConfigControlResponse::Schema {
        schema: admitted,
        schema_digest,
        ..
    } = response
    else {
        panic!()
    };
    let normalized = Schema::new(admitted.clone()).unwrap();
    assert_eq!(normalized.digest().unwrap(), schema_digest);
    assert_eq!(admitted["x-ck-layers"], json!(["user"]));
    assert_eq!(
        admitted["properties"]["credential"]["x-ck-kind"],
        "credential_ref"
    );
    let rows = rig.modules();
    assert_eq!(
        rows["modules"][0],
        json!({"module_id":"sample-legacy","gated":false,"status":"ungated"})
    );
    assert_eq!(rows["modules"][1]["display_name"], "Sample reader");
    for (module, code) in [
        ("missing", "unknown_module"),
        ("sample-legacy", "module_ungated"),
    ] {
        let known = BTreeSet::from(["sample-legacy".into()]);
        assert_eq!(
            rig.owner
                .settings_schema(known.clone(), module.into())
                .unwrap_err()
                .code,
            code
        );
        assert_eq!(
            rig.owner
                .settings_show(known, module.into(), None, None)
                .unwrap_err()
                .code,
            code
        );
    }
    assert_eq!(
        rig.owner
            .settings_show(
                BTreeSet::new(),
                "sample-reader".into(),
                Some("unknown-project".into()),
                None
            )
            .unwrap_err()
            .code,
        "unknown_project"
    );
    rig.declare("sample-untitled", 11, false);
    assert!(
        !serde_json::to_string(&rig.owner.settings_modules(BTreeSet::new()).unwrap())
            .unwrap()
            .contains("sample-untitled\",\"display_name")
    );
}

#[test]
fn config_settings_status_priority_and_real_drift_sources() {
    assert_eq!(
        settings_status(true, true, true, true, true, true, true),
        "failed_delivery"
    );
    assert_eq!(
        settings_status(true, false, true, false, true, false, false),
        "needs_rereview"
    );
    assert_eq!(
        settings_status(true, false, false, true, true, false, false),
        "pending_activation"
    );
    assert_eq!(
        settings_status(true, false, false, false, true, true, false),
        "write_back_behind"
    );
    assert_eq!(
        settings_status(true, false, false, false, false, true, true),
        "pending_proposal"
    );
    assert_eq!(
        settings_status(false, true, true, true, true, true, true),
        "ungated"
    );
    let rig = SettingsRig::new();
    assert_eq!(rig.modules()["modules"][1]["status"], "adopted");
    rig.stamp();
    // A newly stamped generation waits for activation, which outranks file drift.
    assert_eq!(rig.modules()["modules"][1]["status"], "pending_activation");
    rig.owner
        .lock()
        .unwrap()
        .store
        .record_delivery(
            &crate::config_store::Tuple {
                module: "sample-reader".into(),
                project: None,
            },
            1,
            true,
        )
        .unwrap();
    assert_eq!(rig.modules()["modules"][1]["status"], "adopted");
    rig.write(
        br#"{ "limit":4,"group":{"enabled":true},"credential":{"vault_ref":"sample-reference"}}"#,
    );
    assert_eq!(rig.modules()["modules"][1]["status"], "formatting_only");
    rig.write(br#"{"limit":5,"credential":{"vault_ref":"sample-reference"}}"#);
    assert_eq!(rig.modules()["modules"][1]["status"], "pending_proposal");
}

#[test]
fn config_settings_failed_delivery_reason_and_missing_approved_view() {
    let rig = SettingsRig::new();
    rig.stamp();
    rig.owner
        .lock()
        .unwrap()
        .store
        .record_delivery(
            &crate::config_store::Tuple {
                module: "sample-reader".into(),
                project: None,
            },
            1,
            false,
        )
        .unwrap();
    let rows = rig.modules();
    assert_eq!(rows["modules"][1]["status"], "failed_delivery");
    assert_eq!(
        rows["modules"][1]["status_reason"],
        "Generation 1 failed delivery."
    );
    rig.declare("sample-unapproved", 11, false);
    assert_eq!(
        rig.owner
            .settings_show(BTreeSet::new(), "sample-unapproved".into(), None, None)
            .unwrap_err()
            .code,
        "config_gated"
    );
}

#[test]
fn config_settings_module_base_uses_latest_retained_tuple_not_global_generation() {
    let rig = SettingsRig::new();
    rig.stamp();
    let mut body = rig.owner.lock().unwrap().checked_body().unwrap().clone();
    assert_eq!(
        crate::config_gate::module_base_generation(&body, "sample-reader"),
        1
    );
    let mut unrelated = body.generations[&1].clone();
    unrelated.generation.id = 2;
    unrelated.tuples = BTreeSet::from([crate::config_store::Tuple {
        module: "another-sample".into(),
        project: None,
    }]);
    body.generations.insert(2, unrelated);
    assert_eq!(
        crate::config_gate::module_base_generation(&body, "sample-reader"),
        1
    );
    let mut latest = body.generations[&1].clone();
    latest.generation.id = 3;
    latest.kind = crate::config_store::CommitKind::Development;
    body.generations.insert(3, latest);
    assert_eq!(
        crate::config_gate::module_base_generation(&body, "sample-reader"),
        3
    );
    assert_eq!(
        crate::config_gate::module_base_generation(&body, "absent"),
        0
    );
    body.stamped = false;
    assert_eq!(
        crate::config_gate::module_base_generation(&body, "sample-reader"),
        0
    );
}

#[test]
fn config_settings_pending_file_plan_uses_creation_order_and_ignores_edits() {
    let rig = SettingsRig::new();
    rig.stamp();
    rig.write(br#"{"limit":5,"credential":{"vault_ref":"sample-reference"}}"#);
    let first = rig
        .owner
        .finish_plan(rig.owner.prepare(PlanKind::Change).unwrap(), daemon_review)
        .unwrap();
    rig.write(br#"{"limit":6,"credential":{"vault_ref":"sample-reference"}}"#);
    let second = rig
        .owner
        .finish_plan(rig.owner.prepare(PlanKind::Change).unwrap(), daemon_review)
        .unwrap();
    assert_ne!(first.digest, second.digest);
    assert_eq!(rig.show(None)["pending_plan"], json!(second.digest));
    let edit = serde_json::from_value(json!({
        "module_id":"sample-reader", "layer":"user", "path":"/limit", "set":8
    }))
    .unwrap();
    let prepared = rig
        .owner
        .prepare_edits(BTreeMap::from([("sample-reader".into(), 1)]), vec![edit])
        .unwrap();
    let edited = rig.owner.finish_plan(prepared, daemon_review).unwrap();
    assert_ne!(edited.digest, second.digest);
    assert_eq!(rig.show(None)["pending_plan"], json!(second.digest));
    rig.owner.plan_discard(&second.digest).unwrap();
    assert_eq!(rig.show(None)["pending_plan"], json!(first.digest));
    rig.owner.lock().unwrap().gate.expire_plan(&first.digest);
    assert!(rig.show(None).get("pending_plan").is_none());
}

#[test]
fn config_settings_written_back_edit_is_adopted_without_file_proposal() {
    let rig = SettingsRig::new();
    rig.stamp();
    rig.activate();
    let bytes = rig.edit_limit(8);
    rig.activate_generation(2);
    assert_eq!(
        std::fs::read(rig.root.join("cortexkit/sample-reader.jsonc")).unwrap(),
        bytes
    );
    assert_eq!(rig.show(None)["effective"]["limit"], 8);
    assert_eq!(rig.show(None)["base_generation"], 2);
    assert_eq!(rig.modules()["modules"][1]["status"], "adopted");
    assert_eq!(rig.owner.polled_status().unwrap().pending_proposals, 0);
    rig.owner.observe_file_proposal().unwrap();
    assert!(rig
        .owner
        .lock()
        .unwrap()
        .store
        .body()
        .unwrap()
        .proposal
        .is_none());
    assert_eq!(rig.owner.status().unwrap().pending_proposals, 0);
    rig.owner.prepare(PlanKind::Change).unwrap();
    assert!(rig
        .owner
        .lock()
        .unwrap()
        .store
        .body()
        .unwrap()
        .proposal
        .is_none());
    assert!(rig.show(None).get("pending_plan").is_none());

    let mut formatted = bytes.clone();
    formatted.push(b'\n');
    rig.write(&formatted);
    assert_eq!(rig.modules()["modules"][1]["status"], "write_back_behind");
    assert_eq!(rig.owner.status().unwrap().pending_proposals, 1);
    rig.write(&bytes);
    assert_eq!(rig.modules()["modules"][1]["status"], "adopted");
    assert_eq!(rig.owner.status().unwrap().pending_proposals, 0);
}
