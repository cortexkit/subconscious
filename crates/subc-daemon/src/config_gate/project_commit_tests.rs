use super::*;
use crate::config_gate::targets::{ProjectReview, ProjectReviews};
use crate::config_store::{DeliverySnapshot, ProjectCutoffs, ProjectDeliveryState};
use subc_protocol::config::{
    ConfigDeclareRequest, ConfigLayer, ConfigLayerSnapshot, ConfigStatus, ConfigView,
};

fn tuple(module: &str, project: Option<&str>) -> Tuple {
    Tuple {
        module: module.into(),
        project: project.map(str::to_owned),
    }
}

fn review(module: &str, project: &str, workspace_x: u64) -> ProjectReview {
    let mut workspace = ConfigLayerSnapshot::file(
        module,
        ConfigLayer::Workspace,
        "root/.cortexkit/module.jsonc",
        Some(format!(r#"{{"x":{workspace_x}}}"#).into_bytes()),
    );
    workspace.workspace_id = Some("W".into());
    workspace.project_id = Some(project.into());
    let mut layer = ConfigLayerSnapshot::file(
        module,
        ConfigLayer::Project,
        "project/.cortexkit/module.jsonc",
        Some(br#"{"x":9}"#.to_vec()),
    );
    layer.project_id = Some(project.into());
    let view = |x, source, layers| {
        let mut view = ConfigView::new(
            0,
            json!({"x": x}),
            BTreeMap::from([("/x".into(), source)]),
            layers,
            ConfigStatus::Unstamped,
        );
        view.project_id = Some(project.into());
        view
    };
    ProjectReview {
        view: view(
            9,
            subc_config::Layer::Project,
            vec![workspace.clone(), layer],
        ),
        cutoffs: ProjectCutoffs {
            user: view(0, subc_config::Layer::Default, vec![]),
            workspace: view(workspace_x, subc_config::Layer::Workspace, vec![workspace]),
        },
        declaration: ConfigDeclareRequest::new(
            json!({"type":"object","description":"original"}),
            "module.jsonc",
            vec![],
        ),
        reviewed_build: subc_config::raw_digest(b"build-1"),
    }
}

fn project_key(module: &str, project: &str) -> InputKey {
    InputKey::ProjectTuple {
        module: module.into(),
        project: project.into(),
    }
}

fn prepare(rig: &mut Rig, reviews: ProjectReviews, fields: Vec<FieldChange>) -> ConfigPlanView {
    // The fixture's reviewed file witness is unchanged between plan and apply.
    // Gate target selection uses the stored approved payload, not these witnesses.
    for tuple in reviews.tuples.keys() {
        rig.current
            .inputs
            .entry(project_key(
                &tuple.module,
                tuple.project.as_deref().unwrap(),
            ))
            .or_insert_with(|| captured(json!({"workspace":"captured","project":"captured"})));
    }
    rig.gate
        .plan_reviewed_inner(
            &mut rig.store,
            PlanKind::Change,
            rig.current.clone(),
            &rig.current,
            ReviewRecord::Absent,
            None,
            Some(reviews),
            |_, _| {
                let mut changeset = Changeset::new();
                changeset.fields = fields;
                Ok(changeset)
            },
        )
        .unwrap()
}

fn reload(rig: &Rig) -> ConfigStore {
    ConfigStore::open_digest_only_for_tests(
        StoreRoot::from_service_definition(rig.path.clone()).unwrap(),
    )
}

#[test]
fn project_commit_targets_only_changed_pairs_without_starting_user_delivery() {
    let mut rig = Rig::new();
    let stamp = rig.stamp();
    for module in ["a", "b", "callosum"] {
        rig.store
            .record_delivery(&tuple(module, None), stamp, true)
            .unwrap();
    }
    let initial = prepare(
        &mut rig,
        ProjectReviews {
            tuples: [("a", "A"), ("a", "B"), ("b", "A"), ("b", "B")]
                .into_iter()
                .map(|(m, p)| (tuple(m, Some(p)), review(m, p, 1)))
                .collect(),
            ..ProjectReviews::default()
        },
        vec![],
    );
    let first = rig.approve(&initial, &facts(false)).unwrap();
    for tuple in &rig.store.body().unwrap().generations[&first].tuples.clone() {
        rig.store.record_delivery(tuple, first, true).unwrap();
    }

    let expected = BTreeSet::from([tuple("a", Some("A")), tuple("b", Some("B"))]);
    let view = prepare(
        &mut rig,
        ProjectReviews {
            tuples: [("a", "A", 2), ("a", "B", 1), ("b", "A", 1), ("b", "B", 2)]
                .into_iter()
                .map(|(m, p, x)| (tuple(m, Some(p)), review(m, p, x)))
                .collect(),
            ..ProjectReviews::default()
        },
        vec![],
    );
    assert!(view.plan.changeset.fields.is_empty());
    assert_eq!(
        view.plan.changeset.apply_plan["tuple_targets"],
        json!(expected)
    );
    let generation = rig.approve(&view, &facts(false)).unwrap();
    let mut restarted = reload(&rig);
    let body = restarted.body().unwrap();
    let entry = &body.generations[&generation];
    assert_eq!(entry.tuples, expected);
    assert_eq!(
        body.versions
            .iter()
            .filter(|(_, v)| v.pending == Some(generation))
            .map(|(t, _)| t.clone())
            .collect::<BTreeSet<_>>(),
        expected
    );
    assert!(entry.generation.module_states.is_empty());
    assert_eq!(
        entry.project_outcomes["a"]["A"].state,
        ProjectDeliveryState::Pending
    );
    assert_eq!(
        entry.project_outcomes["b"]["B"].state,
        ProjectDeliveryState::Pending
    );
    assert!(body
        .versions
        .iter()
        .filter(|(t, _)| t.project.is_none())
        .all(|(_, v)| v.active == Some(stamp) && v.pending.is_none()));
    let delivery = entry.delivery.as_ref().unwrap();
    assert_eq!(
        delivery
            .declarations
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["a", "b"]
    );
    for (module, project) in [("a", "A"), ("b", "B")] {
        assert_eq!(delivery.project_views[module][project].effective["x"], 9);
        assert_eq!(
            delivery.project_views[module][project].layers[0]
                .bytes
                .as_deref(),
            Some(&br#"{"x":2}"#[..])
        );
        assert_eq!(
            delivery.project_cutoffs[module][project]
                .workspace
                .effective["x"],
            2
        );
        assert_eq!(
            delivery.project_cutoffs[module][project].user.effective["x"],
            0
        );
    }
}

#[test]
fn project_commit_targets_cutoffs_declaration_and_build_without_field_changes() {
    let mut rig = Rig::new();
    rig.stamp();
    let key = tuple("a", Some("A"));
    let mut reviewed = review("a", "A", 1);
    for (cutoff, description, build, changed) in [
        (1, "old", b"one", true),
        (2, "old", b"one", true),
        (2, "new", b"one", true),
        (2, "new", b"two", true),
        (2, "new", b"two", false),
    ] {
        reviewed.cutoffs.workspace.effective["x"] = json!(cutoff);
        reviewed.declaration.schema["description"] = json!(description);
        reviewed.reviewed_build = subc_config::raw_digest(build);
        let view = prepare(
            &mut rig,
            ProjectReviews {
                tuples: BTreeMap::from([(
                    key.clone(),
                    ProjectReview {
                        view: reviewed.view.clone(),
                        cutoffs: reviewed.cutoffs.clone(),
                        declaration: reviewed.declaration.clone(),
                        reviewed_build: reviewed.reviewed_build.clone(),
                    },
                )]),
                ..ProjectReviews::default()
            },
            vec![],
        );
        assert!(view.plan.changeset.fields.is_empty());
        let id = rig.approve(&view, &facts(false)).unwrap();
        let entry = &rig.store.body().unwrap().generations[&id];
        assert_eq!(entry.tuples.contains(&key), changed);
        if changed {
            assert_eq!(
                entry.delivery.as_ref().unwrap().declarations["a"].schema["description"],
                description
            );
        }
    }
}

#[test]
fn project_commit_targets_mixed_plan_is_sorted_and_digest_bound() {
    let mut rig = Rig::new();
    rig.stamp();
    rig.current
        .inputs
        .insert(layer("a", "user"), captured(json!(11)));
    let mut inherited = FieldChange::new(
        "b",
        "/inherited",
        RecordedValue::new(json!(0)),
        RecordedValue::new(json!(1)),
        ApplyKind::RouteRebind,
    );
    inherited.project_id = Some("B".into());
    let view = prepare(
        &mut rig,
        ProjectReviews {
            tuples: BTreeMap::from([(tuple("b", Some("B")), review("b", "B", 1))]),
            ..ProjectReviews::default()
        },
        vec![inherited],
    );
    let expected = [tuple("a", None), tuple("b", Some("B"))];
    assert_eq!(
        view.plan.changeset.apply_plan["tuple_targets"],
        json!(expected)
    );
    assert!(view.plan.changeset.apply_plan["module_transitions"]["b"].is_null());
    for (index, target) in expected.iter().enumerate() {
        let mut edited = view.plan.clone();
        edited.changeset.apply_plan["tuple_targets"][index]["module"] =
            json!(format!("{}-changed", target.module));
        assert_ne!(edited.digest().unwrap(), view.digest);
    }
    let user_declaration = ConfigDeclareRequest::new(json!({"type":"object"}), "a.jsonc", vec![]);
    rig.gate
        .bind_delivery(
            &view.digest,
            DeliverySnapshot {
                declarations: BTreeMap::from([("a".into(), user_declaration.clone())]),
                ..DeliverySnapshot::default()
            },
        )
        .unwrap();
    let id = rig.approve(&view, &facts(false)).unwrap();
    let entry = &rig.store.body().unwrap().generations[&id];
    assert_eq!(entry.tuples, BTreeSet::from(expected));
    assert_eq!(
        entry
            .generation
            .module_states
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["a"]
    );
    let delivery = entry.delivery.as_ref().unwrap();
    assert_eq!(delivery.declarations["a"], user_declaration);
    assert!(delivery.declarations.contains_key("b"));
}

fn invalid_target_plan(
    include_project: bool,
    corrupt: impl FnOnce(&mut serde_json::Value),
    detail: &str,
) {
    let mut rig = Rig::new();
    let stamp = rig.stamp();
    let view = if include_project {
        prepare(
            &mut rig,
            ProjectReviews {
                tuples: BTreeMap::from([(tuple("a", Some("A")), review("a", "A", 1))]),
                ..ProjectReviews::default()
            },
            vec![],
        )
    } else {
        rig.current
            .inputs
            .insert(layer("a", "user"), captured(json!(11)));
        rig.gate
            .plan(
                &mut rig.store,
                PlanKind::Change,
                rig.current.clone(),
                &rig.current,
                scalar_review,
            )
            .unwrap()
    };
    assert_eq!(
        view.plan
            .changeset
            .apply_plan
            .get("tuple_targets")
            .is_some(),
        include_project
    );
    let challenge = rig
        .gate
        .begin_approval(&view.digest, &mut rig.store, &rig.current, &facts(true))
        .unwrap();
    corrupt(
        &mut rig
            .gate
            .plans
            .get_mut(&view.digest)
            .unwrap()
            .view
            .plan
            .changeset
            .apply_plan,
    );
    let error = rig
        .gate
        .finish_approval(
            challenge.attempt(),
            ApprovalReply::Presence(PresenceDecision::InteractiveApproval),
            &mut rig.store,
            &rig.current,
            &facts(true),
            1,
        )
        .unwrap_err();
    assert_eq!(error.code, "apply_plan_invalid");
    assert_eq!(error.detail, detail);
    assert_eq!(rig.store.body().unwrap().last_generation, stamp);
}

#[test]
fn project_commit_targets_refuse_missing_list_for_project_read_set() {
    invalid_target_plan(
        true,
        |apply| {
            apply.as_object_mut().unwrap().remove("tuple_targets");
        },
        "project plan is missing its bound tuple targets",
    );
}

#[test]
fn project_commit_targets_refuse_tuple_outside_read_set() {
    invalid_target_plan(
        true,
        |apply| {
            apply["tuple_targets"][0]["project"] = json!("B");
        },
        "bound tuple target is outside the plan read set",
    );
}

#[test]
fn project_commit_targets_refuse_disagreeing_user_only_list() {
    invalid_target_plan(
        false,
        |apply| {
            apply["tuple_targets"] = json!([]);
        },
        "bound tuple targets disagree with the user-only read set",
    );
}

#[test]
fn project_commit_targets_project_file_intents_supersede_and_adopt_by_physical_path() {
    use crate::config_gate::{capture::FileObjectId, project_registry::RegistryFileId};
    use crate::config_store::{
        ExpectedProjectFile, ProjectFileApproval, ProjectFileLayer, ProjectFileTarget,
    };
    let mut rig = Rig::new();
    rig.stamp();
    let key = tuple("a", Some("A"));
    let mut target = ProjectFileTarget {
        module: "a".into(),
        layer: ProjectFileLayer::Workspace {
            workspace_id: "W".into(),
        },
        registry_revision: subc_config::raw_digest(b"view 1"),
        root: rig.path.clone(),
        root_identity: RegistryFileId::Unix {
            device: 3,
            inode: 41,
        },
        relative_path: std::path::PathBuf::from(".cortexkit").join("a.jsonc"),
    };
    let path = target.path().unwrap();
    let mut expected = ExpectedProjectFile::Absent;
    for x in [1, 2] {
        let bytes = format!(r#"{{"x":{x}}}"#).into_bytes();
        let view = prepare(
            &mut rig,
            ProjectReviews {
                tuples: BTreeMap::from([(key.clone(), review("a", "A", x))]),
                files: vec![ProjectFileApproval::Write {
                    target: target.clone(),
                    expected: expected.clone(),
                    bytes: bytes.clone(),
                }],
            },
            vec![],
        );
        let bound = &view.plan.changeset.apply_plan["project_file_approvals"][0];
        assert_eq!(
            bound["candidate_digest"],
            json!(subc_config::raw_digest(&bytes))
        );
        assert!(bound.get("bytes").is_none());
        let id = rig.approve(&view, &facts(false)).unwrap();
        let mut restarted = reload(&rig);
        let intents = &restarted.body().unwrap().project_write_back;
        assert_eq!(intents.len(), 1);
        assert_eq!(intents[&path].target, target);
        assert_eq!(intents[&path].generation, id);
        assert_eq!(intents[&path].expected, expected);
        assert_eq!(intents[&path].bytes, bytes);
        expected = ExpectedProjectFile::Present {
            identity: FileObjectId::Unix {
                device: 3,
                inode: 42,
            },
            digest: subc_config::raw_digest(&bytes),
        };
        target.layer = ProjectFileLayer::Project {
            project_id: "A".into(),
        };
        target.registry_revision = subc_config::raw_digest(b"view 2");
    }
    let view = prepare(
        &mut rig,
        ProjectReviews {
            tuples: BTreeMap::from([(key, review("a", "A", 3))]),
            files: vec![ProjectFileApproval::Adopt {
                target,
                observed_digest: Some(subc_config::raw_digest(br#"{"x":3}"#)),
            }],
        },
        vec![],
    );
    rig.approve(&view, &facts(false)).unwrap();
    assert!(reload(&rig).body().unwrap().project_write_back.is_empty());
}
