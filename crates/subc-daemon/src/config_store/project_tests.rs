use super::*;
use cortexkit_project_identity::EnumerateReply;
use serde_json::json;
use subc_protocol::config::{ConfigStatus, ConfigView};

fn project(id: &str) -> Tuple {
    Tuple {
        module: "sample".into(),
        project: Some(id.into()),
    }
}

fn project_entry(projects: &[&str], x: u64) -> HistoryEntry {
    let mut entry = entry(CommitKind::Approved);
    entry.tuples = projects.iter().map(|id| project(id)).collect();
    let view = |x| {
        ConfigView::new(
            0,
            json!({"x":x}),
            BTreeMap::new(),
            vec![],
            ConfigStatus::Unstamped,
        )
    };
    let mut delivery = DeliverySnapshot::default();
    for id in projects {
        let mut effective = view(x);
        effective.project_id = Some((*id).into());
        delivery
            .project_views
            .entry("sample".into())
            .or_default()
            .insert((*id).into(), effective);
        delivery
            .project_cutoffs
            .entry("sample".into())
            .or_default()
            .insert(
                (*id).into(),
                ProjectCutoffs {
                    user: view(x + 100),
                    workspace: view(x + 200),
                },
            );
    }
    entry.delivery = Some(delivery);
    entry
}

fn generation(store: &mut ConfigStore, projects: &[&str], x: u64) -> u64 {
    store
        .commit_generation(
            project_entry(projects, x),
            snapshot(1, 1),
            snapshot(1, 1),
            x,
        )
        .unwrap()
}

#[test]
fn config_store_projects_outcomes_and_suppression_are_isolated_after_restart() {
    let rig = Rig::new();
    let mut store = rig.open(true);
    let stamp = commit(&mut store, CommitKind::Stamp);
    store.record_delivery(&tuple(), stamp, true).unwrap();
    let a = project("A");
    let b = project("B");
    let mut approved = project_entry(&["A", "B"], 1);
    approved.tuples.insert(tuple());
    approved
        .generation
        .module_states
        .insert("sample".into(), subc_config::GenerationState::Pending);
    let id = store
        .commit_generation(approved, snapshot(1, 1), snapshot(1, 1), 1)
        .unwrap();
    store.record_delivery(&a, id, false).unwrap();
    drop(store);
    let mut restarted = rig.open(true);
    assert!(!restarted.resume_delivery(&a, id).unwrap());
    assert!(restarted.resume_delivery(&b, id).unwrap());
    let body = restarted.body().unwrap();
    assert_eq!(
        body.generations[&id].project_outcomes["sample"]["B"].state,
        ProjectDeliveryState::Resumed
    );
    restarted.record_delivery(&b, id, true).unwrap();
    drop(restarted);
    let mut restarted = rig.open(true);
    let body = restarted.body().unwrap();
    let approved = &body.generations[&id];
    assert_eq!(approved.outcome, "pending");
    assert_eq!(
        approved.generation.module_states["sample"],
        subc_config::GenerationState::Pending
    );
    assert!(approved.generation.never_redeliver.is_empty());
    assert_eq!(
        approved.project_outcomes["sample"]["A"].state,
        ProjectDeliveryState::Failed
    );
    assert_eq!(
        approved.project_outcomes["sample"]["B"].state,
        ProjectDeliveryState::Active
    );
    assert!(approved.redelivery_suppressed(&a));
    assert!(!approved.redelivery_suppressed(&b));
    assert!(!approved.redelivery_suppressed(&tuple()));
    let versions = |key| &body.versions.iter().find(|(t, _)| t == key).unwrap().1;
    assert_eq!(versions(&a).pending, None);
    assert_eq!(versions(&b).active, Some(id));
    let user = tuple();
    assert_eq!(versions(&user).active, Some(stamp));
    assert_eq!(versions(&user).pending, Some(id));
}

#[test]
fn config_store_projects_anchor_retry_keeps_project_ack_and_user_failure() {
    let rig = Rig::new();
    let mut store = rig.open(true);
    let stamp = commit(&mut store, CommitKind::Stamp);
    store.record_delivery(&tuple(), stamp, true).unwrap();
    let key = project("A");
    let id = generation(&mut store, &["A"], 1);
    let user = commit(&mut store, CommitKind::Approved);
    rig.key.lock().unwrap().set_anchor_failure(true);
    error_code(
        store.record_delivery(&key, id, true),
        StoreErrorCode::AnchorPending,
    );
    error_code(
        store.record_delivery(&tuple(), user, false),
        StoreErrorCode::AnchorPending,
    );
    rig.key.lock().unwrap().set_anchor_failure(false);
    store.retry_anchor().unwrap();
    let mut restarted = rig.open(true);
    let body = restarted.body().unwrap();
    let approved = &body.generations[&id];
    let failed = &body.generations[&user];
    assert_eq!(failed.outcome, "failed");
    assert_eq!(
        failed.generation.module_states["sample"],
        subc_config::GenerationState::Failed
    );
    assert!(failed.redelivery_suppressed(&tuple()));
    assert!(!approved.redelivery_suppressed(&key));
    assert_eq!(
        approved.project_outcomes["sample"]["A"].state,
        ProjectDeliveryState::Active
    );
    assert_eq!(
        body.versions
            .iter()
            .find(|(t, _)| t == &key)
            .unwrap()
            .1
            .active,
        Some(id)
    );
}

#[test]
fn config_store_projects_pruning_preserves_views_and_cutoffs_for_all_three_pointers() {
    let rig = Rig::new();
    let mut store = rig.open(false);
    let stamp = commit(&mut store, CommitKind::Stamp);
    store.record_delivery(&tuple(), stamp, true).unwrap();
    let key = project("A");
    let recovery = generation(&mut store, &["A"], 1);
    store.record_delivery(&key, recovery, true).unwrap();
    let active = generation(&mut store, &["A"], 2);
    store.record_delivery(&key, active, true).unwrap();
    let mut rejected = Vec::new();
    for value in 3..30 {
        let id = generation(&mut store, &["A"], value);
        store.record_delivery(&key, id, false).unwrap();
        rejected.push(id);
        let user = commit(&mut store, CommitKind::Approved);
        store.record_delivery(&tuple(), user, true).unwrap();
    }
    let pending = generation(&mut store, &["A"], 30);
    let mut restarted = rig.open(false);
    let body = restarted.body().unwrap();
    assert!(body.pruned.contains_key(&rejected[0]));
    assert!(body.pruned.contains_key(&stamp));
    let pointers = &body.versions.iter().find(|(t, _)| t == &key).unwrap().1;
    assert_eq!(
        (pointers.recovery, pointers.active, pointers.pending),
        (Some(recovery), Some(active), Some(pending))
    );
    for (id, x) in [(recovery, 1), (active, 2), (pending, 30)] {
        let data = body.generations[&id].delivery.as_ref().unwrap();
        assert_eq!(data.project_views["sample"]["A"].effective["x"], x);
        assert_eq!(
            data.project_cutoffs["sample"]["A"].user.effective["x"],
            x + 100
        );
        assert_eq!(
            data.project_cutoffs["sample"]["A"].workspace.effective["x"],
            x + 200
        );
    }
}

fn enumeration(has_project: bool, has_workspace: bool, generation: i64) -> EnumerateReply {
    let projects = has_project.then(|| {
        json!({
            "projectId":"A", "name":"A", "roots":[], "implicit":false,
            "refKind":"local", "workspaceId":"W", "rootRecords":[],
        })
    });
    let workspaces = has_workspace.then(|| json!({"workspaceId":"W","name":"W"}));
    serde_json::from_value(json!({
        "projects": projects.into_iter().collect::<Vec<_>>(),
        "workspaces": workspaces.into_iter().collect::<Vec<_>>(),
        "generation": generation, "incarnation": "registry",
    }))
    .unwrap()
}

#[test]
fn config_store_projects_only_complete_success_replaces_membership() {
    use crate::config_gate::GateError;
    let rig = Rig::new();
    let mut store = rig.open(true);
    commit(&mut store, CommitKind::Stamp);
    let id = generation(&mut store, &["A"], 1);
    let base = store.status().base_revision;
    assert!(store.body().unwrap().registry_membership.is_none());
    let complete = enumeration(true, true, 7);
    assert!(store.replace_registry_membership(Ok(&complete)).unwrap());
    let bytes = rig.bytes();
    assert!(!store.replace_registry_membership(Ok(&complete)).unwrap());
    assert_eq!(
        store
            .replace_registry_membership(Err(GateError::new(
                "project_registry_unavailable",
                "offline"
            )))
            .unwrap_err()
            .code,
        "project_registry_unavailable"
    );
    assert_eq!(rig.bytes(), bytes);
    let partial = enumeration(true, false, 8);
    assert_eq!(
        store
            .replace_registry_membership(Ok(&partial))
            .unwrap_err()
            .code,
        "project_registry_unavailable"
    );
    assert_eq!(rig.bytes(), bytes);
    drop(store);
    let mut restarted = rig.open(true);
    let saved = restarted
        .body()
        .unwrap()
        .registry_membership
        .as_ref()
        .unwrap();
    assert_eq!(
        saved.projects,
        BTreeMap::from([("A".into(), Some("W".into()))])
    );
    assert_eq!(saved.revision.generation, 7);
    assert!(restarted
        .replace_registry_membership(Ok(&enumeration(false, true, 9)))
        .unwrap());
    assert_eq!(restarted.status().base_revision, base);
    let mut restarted = rig.open(true);
    let body = restarted.body().unwrap();
    assert!(body
        .registry_membership
        .as_ref()
        .unwrap()
        .projects
        .is_empty());
    assert_eq!(
        body.registry_membership
            .as_ref()
            .unwrap()
            .revision
            .generation,
        9
    );
    assert_eq!(
        body.versions
            .iter()
            .find(|(t, _)| t == &project("A"))
            .unwrap()
            .1
            .pending,
        Some(id)
    );
}

#[test]
fn config_store_projects_pre_change_user_only_envelope_loads_unchanged() {
    let rig = Rig::new();
    let mut store = rig.open(true);
    let mut stamp = entry(CommitKind::Stamp);
    stamp.delivery = Some(DeliverySnapshot {
        modules: BTreeMap::from([(
            "sample".into(),
            ConfigView::new(
                0,
                json!({"x":1}),
                BTreeMap::new(),
                vec![],
                ConfigStatus::Unstamped,
            ),
        )]),
        ..DeliverySnapshot::default()
    });
    store
        .commit_generation(stamp, snapshot(1, 1), snapshot(1, 1), 1)
        .unwrap();
    let bytes = rig.bytes();
    let pre_change: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let raw = &pre_change["body"];
    for field in ["registry_membership", "project_write_back"] {
        assert!(raw.get(field).is_none());
    }
    let history = &raw["generations"]["1"];
    assert!(history.get("project_outcomes").is_none());
    for field in [
        "project_views",
        "project_cutoffs",
        "project_approval_digests",
    ] {
        assert!(history["delivery"].get(field).is_none());
    }
    drop(store);
    let mut restarted = rig.open(true);
    let body = restarted.body().unwrap();
    assert_eq!(body.versions[0].0, tuple());
    assert_eq!(body.versions[0].1.pending, Some(1));
    assert!(body.registry_membership.is_none());
    assert!(body.project_write_back.is_empty());
    let history = &body.generations[&1];
    assert!(history.project_outcomes.is_empty());
    let delivery = history.delivery.as_ref().unwrap();
    assert_eq!(delivery.modules["sample"].effective["x"], 1);
    assert!(delivery.project_views.is_empty() && delivery.project_cutoffs.is_empty());
    assert_eq!(rig.bytes(), bytes);
}
