use super::*;
use std::time::Duration;
use subc_control::config::ConfigEdit;

fn edit(layer: &str, project: Option<&str>, workspace: Option<&str>, value: u64) -> ConfigEdit {
    let mut body = json!({"module_id":"a","layer":layer,"path":"/limit","set":value});
    if let Some(project) = project {
        body["project_id"] = json!(project);
    }
    if let Some(workspace) = workspace {
        body["workspace_id"] = json!(workspace);
    }
    serde_json::from_value(body).unwrap()
}

fn base(rig: &CaptureRig) -> u64 {
    let mut state = rig.owner.lock().unwrap();
    crate::config_gate::module_base_generation(state.store.body().unwrap(), "a")
}

async fn prepare(rig: &CaptureRig, edits: Vec<ConfigEdit>) -> Result<PreparedPlan> {
    rig.owner
        .prepare_control_edits(BTreeMap::from([("a".into(), base(rig))]), edits)
        .await
}

async fn publish(rig: &CaptureRig, view: &ConfigPlanView) -> (u64, super::super::CaptureApply) {
    let attempt = rig.begin(view).unwrap();
    let witness = rig
        .owner
        .recheck_capture_apply(attempt)
        .await
        .unwrap()
        .unwrap();
    let generation = rig
        .owner
        .finish_approval_with_commit_window(
            attempt,
            &Rig::facts(),
            ApprovalReply::Presence(crate::config_gate::PresenceDecision::InteractiveApproval),
            2,
            None,
            ApprovalRequest::for_attempt(attempt),
            Some(witness.clone()),
        )
        .unwrap();
    (generation, witness)
}

async fn apply_edit(rig: &CaptureRig, plan: PreparedPlan) -> u64 {
    let view = finish(&rig.owner, plan);
    let (generation, witness) = publish(rig, &view).await;
    rig.owner
        .write_approved_project_files(generation, witness)
        .await;
    generation
}

fn intents(rig: &CaptureRig) -> BTreeMap<PathBuf, crate::config_store::ProjectWriteBackIntent> {
    rig.owner
        .lock()
        .unwrap()
        .store
        .body()
        .unwrap()
        .project_write_back
        .clone()
}

#[tokio::test]
async fn project_edit_first_layer_reviews_shared_root_readers_and_checks_joiners() {
    let rig = CaptureRig::new();
    let shared = root(&rig.project_scratch, "shared");
    let other = root(&rig.project_scratch, "other-workspace");
    let project = member("P", Some("W"), &[&shared]);
    let joined = member("Q", Some("other"), &[&shared]);
    let set = |projects| {
        let mut reply = enumerate(&rig.workspace, projects);
        reply.workspaces.push(
            serde_json::from_value(
                json!({"workspaceId":"other","name":"Other","root":other,"fileId":file_id(&other)}),
            )
            .unwrap(),
        );
        *rig.registry.reply.lock().unwrap() = Some(reply);
    };
    set(vec![project.clone()]);
    let request = edit("project", Some("P"), None, 9);
    let view = finish(
        &rig.owner,
        prepare(&rig, vec![request.clone()]).await.unwrap(),
    );
    let attempt = rig.begin(&view).unwrap();
    set(vec![project, joined]);
    assert_eq!(
        rig.owner
            .recheck_capture_apply(attempt)
            .await
            .err()
            .unwrap()
            .code,
        "config_changed_since_review"
    );
    assert!(!shared.join(".cortexkit").exists());
    rig.owner.lock().unwrap().gate.cancel_attempt(attempt);

    let plan = prepare(&rig, vec![request]).await.unwrap();
    let reviews = plan
        .project_reviews()
        .map(|(tuple, review)| {
            assert!(review.base.is_none());
            assert_eq!(review.approval.view.effective["limit"], 9);
            assert_eq!(review.approval.view.layers[1].project_id, tuple.project);
            tuple.project.as_deref().unwrap()
        })
        .collect::<Vec<_>>();
    assert_eq!(reviews, ["P", "Q"]);
    let generation = apply_edit(&rig, plan).await;
    let mut state = rig.owner.lock().unwrap();
    assert_eq!(
        state.store.body().unwrap().generations[&generation].tuples,
        BTreeSet::from([tuple(Some("P")), tuple(Some("Q"))])
    );
    assert_eq!(
        crate::config_gate::capture::parse_layer(
            &std::fs::read(shared.join(".cortexkit/a.jsonc")).unwrap()
        )
        .unwrap()["limit"],
        9
    );
    assert!(state.store.body().unwrap().project_write_back.is_empty());
}

#[tokio::test]
async fn project_edit_same_root_writes_once_and_preserves_jsonc_and_mode() {
    let rig = CaptureRig::new();
    let b = root(&rig.project_scratch, "b");
    rig.set_projects(vec![
        member("A", Some("W"), &[&rig.workspace]),
        member("B", Some("W"), &[&b]),
    ]);
    let path = write_layer(&rig.workspace, 4);
    std::fs::write(&path, b"{\r\n  // keep this\r\n  \"limit\": 4,\r\n}\r\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
    }
    let capture = finish(&rig.owner, rig.prepare().await.unwrap());
    rig.apply(&capture).await.unwrap();
    let writes = Arc::new(AtomicUsize::new(0));
    *rig.owner.project_write_hook.lock().unwrap() = Some(Arc::new({
        let writes = writes.clone();
        move |published| {
            if published {
                writes.fetch_add(1, Ordering::Relaxed);
            }
            Ok(())
        }
    }));
    let plan = prepare(&rig, vec![edit("workspace", Some("A"), None, 6)])
        .await
        .unwrap();
    assert_eq!(
        plan.project_reviews()
            .map(|(tuple, review)| (
                tuple.project.as_deref().unwrap(),
                review.approval.view.layers[1].layer.clone()
            ))
            .collect::<Vec<_>>(),
        [("A", ConfigLayer::Project), ("B", ConfigLayer::Workspace)]
    );
    apply_edit(&rig, plan).await;
    assert_eq!(writes.load(Ordering::Relaxed), 1);
    assert_eq!(
        std::fs::read(&path).unwrap(),
        b"{\r\n  // keep this\r\n  \"limit\": 6,\r\n}\r\n"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }
    let remove = serde_json::from_value(json!({"module_id":"a","layer":"workspace","workspace_id":"W","path":"/limit","remove":true})).unwrap();
    apply_edit(&rig, prepare(&rig, vec![remove]).await.unwrap()).await;
    let bytes = std::fs::read(path).unwrap();
    assert_eq!(
        crate::config_gate::capture::parse_layer(&bytes).unwrap(),
        json!({})
    );
}

#[tokio::test]
async fn project_edit_root_refusals_and_unavailability_have_distinct_shapes() {
    let rig = CaptureRig::new();
    let a = root(&rig.project_scratch, "a");
    let b = root(&rig.project_scratch, "b");
    let missing = root(&rig.project_scratch, "missing");
    rig.set_projects(vec![
        member("A", Some("W"), &[&a, &b]),
        member("B", None, &[]),
        member("C", None, &[&missing]),
    ]);
    let cases = [("A", "layer_root_ambiguous"), ("B", "layer_root_invalid")];
    for (id, code) in cases {
        let prepared = prepare(&rig, vec![edit("project", Some(id), None, 5)])
            .await
            .unwrap();
        let view = finish(&rig.owner, prepared);
        assert_eq!(view.refusals[0].code, code);
        assert_eq!(view.refusals[0].project_id.as_deref(), Some(id));
    }
    assert!(!a.join(".cortexkit").exists() && !b.join(".cortexkit").exists());
    write_layer(&a, 5);
    write_layer(&b, 5);
    let plan = prepare(&rig, vec![edit("project", Some("A"), None, 7)])
        .await
        .unwrap();
    assert_eq!(
        finish(&rig.owner, plan).refusals[0].code,
        "layer_root_ambiguous"
    );
    assert_eq!(
        prepare(&rig, vec![edit("workspace", Some("A"), Some("other"), 5)])
            .await
            .err()
            .unwrap()
            .code,
        "workspace_mismatch"
    );
    for (project, workspace) in [(Some("B"), None), (None, Some("no-members"))] {
        assert_eq!(
            prepare(&rig, vec![edit("workspace", project, workspace, 5)])
                .await
                .err()
                .unwrap()
                .code,
            "unknown_workspace"
        );
    }
    let mut reply = rig.registry.reply().unwrap();
    reply.workspaces[0].root = None;
    *rig.registry.reply.lock().unwrap() = Some(reply);
    assert_eq!(
        prepare(&rig, vec![edit("workspace", None, Some("W"), 5)])
            .await
            .err()
            .unwrap()
            .code,
        "unknown_workspace"
    );
    std::fs::remove_dir(&missing).unwrap();
    assert_eq!(
        prepare(&rig, vec![edit("project", Some("C"), None, 5)])
            .await
            .err()
            .unwrap()
            .code,
        "config_unavailable"
    );
    #[cfg(unix)]
    {
        let layer = a.join(".cortexkit/a.jsonc");
        std::fs::remove_file(&layer).unwrap();
        std::os::unix::fs::symlink(b.join(".cortexkit/a.jsonc"), &layer).unwrap();
        rig.set_projects(vec![member("A", None, &[&a])]);
        let prepared = prepare(&rig, vec![edit("project", Some("A"), None, 7)])
            .await
            .unwrap();
        assert_eq!(
            finish(&rig.owner, prepared).refusals[0].code,
            "layer_symlink"
        );
    }
}

#[tokio::test]
async fn project_edit_dirty_sibling_stays_proposed_and_capture_clears_dirty_target_intent() {
    let rig = CaptureRig::new();
    let b = root(&rig.project_scratch, "b");
    rig.set_projects(vec![
        member("A", Some("W"), &[]),
        member("B", Some("W"), &[&b]),
    ]);
    let workspace = write_layer(&rig.workspace, 4);
    let sibling = write_layer(&b, 9);
    let capture = finish(&rig.owner, rig.prepare().await.unwrap());
    rig.apply(&capture).await.unwrap();
    std::fs::write(&sibling, b"{\"limit\": 5}").unwrap();
    let request = edit("workspace", None, Some("W"), 6);
    let plan = prepare(&rig, vec![request.clone()]).await.unwrap();
    assert!(
        !plan.capture_refused(),
        "the unapproved sibling must not be checked against the new bound"
    );
    assert_eq!(
        plan.project_reviews()
            .find(|(tuple, _)| tuple.project.as_deref() == Some("B"))
            .unwrap()
            .1
            .approval
            .view
            .effective["limit"],
        9
    );
    apply_edit(&rig, plan).await;
    assert_eq!(std::fs::read(&sibling).unwrap(), b"{\"limit\": 5}");
    {
        let mut state = rig.owner.lock().unwrap();
        let body = state.store.body().unwrap();
        let status = body.project_write_back_status("a", &sibling).unwrap();
        assert!(status.pending_proposal && !status.write_back_behind);
        let key = crate::config_gate::capture_plan::file_key("a", &sibling);
        let approved = crate::config_gate::GateSnapshot::from_store(&body.approved).unwrap();
        let observed =
            crate::config_gate::GateSnapshot::from_store(body.observed.as_ref().unwrap()).unwrap();
        assert_ne!(approved.inputs[&key].state, observed.inputs[&key].state);
        assert!(body.proposal.is_some());
    }

    std::fs::write(&workspace, b"{\"limit\": 3}").unwrap();
    let plan = prepare(&rig, vec![edit("workspace", None, Some("W"), 7)])
        .await
        .unwrap();
    apply_edit(&rig, plan).await;
    assert_eq!(std::fs::read(&workspace).unwrap(), b"{\"limit\": 3}");
    assert!(intents(&rig).contains_key(&workspace));
    {
        let mut state = rig.owner.lock().unwrap();
        let status = state
            .store
            .body()
            .unwrap()
            .project_write_back_status("a", &workspace)
            .unwrap();
        assert!(status.pending_proposal && status.write_back_behind);
    }
    let capture = finish(&rig.owner, rig.prepare().await.unwrap());
    rig.apply(&capture).await.unwrap();
    assert!(intents(&rig).is_empty());
    assert_eq!(
        rig.owner
            .lock()
            .unwrap()
            .store
            .body()
            .unwrap()
            .project_write_back_status("a", &workspace)
            .unwrap(),
        Default::default()
    );
}

#[tokio::test]
async fn project_edit_first_publish_never_replaces_another_writer() {
    let rig = CaptureRig::new();
    let private = root(&rig.project_scratch, "private");
    rig.set_projects(vec![member("A", None, &[&private])]);
    let path = private.join(".cortexkit/a.jsonc");
    *rig.owner.project_write_hook.lock().unwrap() = Some(Arc::new({
        let path = path.clone();
        move |published| {
            if !published {
                std::fs::write(&path, b"{\"limit\": 8}").unwrap();
            }
            Ok(())
        }
    }));
    let plan = prepare(&rig, vec![edit("project", Some("A"), None, 4)])
        .await
        .unwrap();
    apply_edit(&rig, plan).await;
    assert_eq!(std::fs::read(&path).unwrap(), b"{\"limit\": 8}");
    assert!(intents(&rig).contains_key(&path));
}

#[tokio::test]
async fn project_edit_late_write_adopts_by_content_after_restart_without_writing() {
    let rig = CaptureRig::with_pool(
        true,
        RootIoPool::with_timeout(Duration::from_millis(500)).unwrap(),
    );
    let private = root(&rig.project_scratch, "private");
    rig.set_projects(vec![member("A", None, &[&private])]);
    let path = private.join(".cortexkit/a.jsonc");
    let view = finish(
        &rig.owner,
        prepare(&rig, vec![edit("project", Some("A"), None, 4)])
            .await
            .unwrap(),
    );
    let next = finish(
        &rig.owner,
        prepare(&rig, vec![edit("project", Some("A"), None, 5)])
            .await
            .unwrap(),
    );
    let (generation, witness) = publish(&rig, &view).await;
    let (release, hold) = std::sync::mpsc::channel();
    let hold = Mutex::new(hold);
    let writes = Arc::new(AtomicUsize::new(0));
    *rig.owner.project_write_hook.lock().unwrap() = Some(Arc::new({
        let writes = writes.clone();
        move |published| {
            if published {
                writes.fetch_add(1, Ordering::Relaxed);
                hold.lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(10))
                    .unwrap();
            }
            Ok(())
        }
    }));
    rig.owner
        .write_approved_project_files(generation, witness.clone())
        .await;
    assert_eq!(writes.load(Ordering::Relaxed), 1);
    assert!(intents(&rig).contains_key(&path));
    assert_eq!(rig.registry.pool.health().unwrap().charged_workers, 1);
    assert!(
        rig.owner
            .lock()
            .unwrap()
            .store
            .body()
            .unwrap()
            .project_write_back_status("a", &path)
            .unwrap()
            .write_back_behind
    );
    rig.owner
        .write_approved_project_files(generation, witness)
        .await;
    assert_eq!(writes.load(Ordering::Relaxed), 1);
    assert_eq!(
        rig.owner
            .apply_control_edits(next.digest)
            .await
            .unwrap_err()
            .code,
        "config_unavailable"
    );
    let error = prepare(&rig, vec![edit("project", Some("A"), None, 5)])
        .await
        .err()
        .unwrap();
    assert_eq!(error.code, "config_unavailable");
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while rig.registry.pool.is_busy(&private) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    let mut restarted = rig.rig.owner("restarted");
    restarted.project_capture_enabled = true;
    restarted.capturer.projects = rig.registry.clone();
    *restarted.project_write_hook.lock().unwrap() =
        Some(Arc::new(|_| panic!("reconciliation must not write")));
    let restarted = Arc::new(restarted);
    let expected = intents(&rig)[&path].bytes.clone();
    std::fs::write(&path, b"{\"limit\": 3}").unwrap();
    restarted.refresh_project_membership().await.unwrap();
    assert!(restarted
        .lock()
        .unwrap()
        .store
        .body()
        .unwrap()
        .project_write_back
        .contains_key(&path));
    std::fs::write(&path, &expected).unwrap();
    let mut remapped = member("A", None, &[&private]);
    remapped["rootRecords"][0]["incarnation"]["value"] = json!("new-checkout");
    rig.set_projects(vec![remapped]);
    restarted.refresh_project_membership().await.unwrap();
    assert!(restarted
        .lock()
        .unwrap()
        .store
        .body()
        .unwrap()
        .project_write_back
        .contains_key(&path));
    rig.set_projects(vec![member("A", None, &[&private])]);
    restarted.refresh_project_membership().await.unwrap();
    assert!(restarted
        .lock()
        .unwrap()
        .store
        .body()
        .unwrap()
        .project_write_back
        .is_empty());
    assert_eq!(
        restarted
            .lock()
            .unwrap()
            .store
            .body()
            .unwrap()
            .project_write_back_status("a", &path)
            .unwrap(),
        Default::default()
    );
    assert_eq!(std::fs::read(path).unwrap(), expected);
}

#[tokio::test]
async fn project_edit_prepublication_failure_does_not_block_the_next_approval() {
    let rig = CaptureRig::new();
    let private = root(&rig.project_scratch, "private");
    rig.set_projects(vec![member("A", None, &[&private])]);
    let path = private.join(".cortexkit/a.jsonc");
    *rig.owner.project_write_hook.lock().unwrap() = Some(Arc::new(|published| {
        assert!(!published);
        Err(GateError::new(
            "write_back_failed",
            "injected before publication",
        ))
    }));
    let plan = prepare(&rig, vec![edit("project", Some("A"), None, 4)])
        .await
        .unwrap();
    apply_edit(&rig, plan).await;
    assert!(!path.exists());
    assert!(intents(&rig).contains_key(&path));
    *rig.owner.project_write_hook.lock().unwrap() = None;
    let plan = prepare(&rig, vec![edit("project", Some("A"), None, 5)])
        .await
        .unwrap();
    apply_edit(&rig, plan).await;
    assert!(intents(&rig).is_empty());
    assert_eq!(
        crate::config_gate::capture::parse_layer(&std::fs::read(path).unwrap()).unwrap()["limit"],
        5
    );
}

#[tokio::test]
async fn project_edit_unstamped_checks_only_target_and_distinguishes_late_publication() {
    let rig = CaptureRig::with_state(true, RootIoPool::new().unwrap(), false);
    let b = root(&rig.project_scratch, "b");
    rig.set_projects(vec![
        member("A", Some("W"), &[]),
        member("B", Some("W"), &[&b]),
    ]);
    let workspace = write_layer(&rig.workspace, 4);
    let sibling = write_layer(&b, 9);
    let request = edit("workspace", None, Some("W"), 6);
    assert_eq!(
        prepare(
            &rig,
            vec![request.clone(), edit("project", Some("B"), None, 10)]
        )
        .await
        .err()
        .unwrap()
        .code,
        "edit_spans_files"
    );
    let plan = prepare(&rig, vec![request]).await.unwrap();
    assert_eq!(plan.project_reviews().count(), 0);
    let view = finish(&rig.owner, plan);
    std::fs::write(&sibling, b"{\"limit\": 8}").unwrap();
    assert_eq!(
        rig.owner.apply_control_edits(view.digest).await.unwrap(),
        Some(0)
    );
    assert_eq!(std::fs::read(&workspace).unwrap(), b"{\"limit\":6}");
    assert_eq!(std::fs::read(&sibling).unwrap(), b"{\"limit\": 8}");

    let view = finish(
        &rig.owner,
        prepare(&rig, vec![edit("workspace", None, Some("W"), 7)])
            .await
            .unwrap(),
    );
    *rig.owner.project_write_hook.lock().unwrap() = Some(Arc::new(|published| {
        if published {
            Err(GateError::new(
                "config_unavailable",
                "injected after rename",
            ))
        } else {
            Ok(())
        }
    }));
    assert_eq!(
        rig.owner
            .apply_control_edits(view.digest)
            .await
            .unwrap_err()
            .code,
        "config_unavailable"
    );
    assert_eq!(std::fs::read(&workspace).unwrap(), b"{\"limit\":7}");
    assert!(intents(&rig).is_empty());
}
