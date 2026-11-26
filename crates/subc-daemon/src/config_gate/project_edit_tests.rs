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
        .write_approved_project_files(generation, witness, None)
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
        .write_approved_project_files(generation, witness.clone(), None)
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
        .write_approved_project_files(generation, witness, None)
        .await;
    assert_eq!(writes.load(Ordering::Relaxed), 1);
    assert_eq!(
        rig.owner
            .apply_control_edits(next.digest, None)
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

// The test-only OS prompt policy below exists only with `test-support`.
#[cfg(all(
    feature = "test-support",
    any(target_os = "linux", target_os = "macos")
))]
#[tokio::test]
async fn stalled_project_write_keeps_upgrade_out_after_waiter_expires() {
    use crate::operator_confirm::{OperatorProvider, OperatorWithdraw, ProviderResult};
    use sha2::Digest;
    struct Approve;
    impl OperatorProvider for Approve {
        fn config_gate_ready(&self) -> bool {
            true
        }
        fn prompt(
            &self,
            _: &str,
            _: Box<dyn FnOnce(Arc<dyn OperatorWithdraw>) + Send>,
        ) -> ProviderResult {
            ProviderResult::Approved
        }
    }

    let mut rig = CaptureRig::with_state(
        true,
        RootIoPool::with_timeout(Duration::from_secs(1)).unwrap(),
        false,
    );
    // Keep module a's settings, but disable it so config.apply does not
    // try to launch its nonexecutable fixture file during reconciliation.
    let config = rig.rig.path.join("subc.jsonc");
    let mut daemon_config: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&config).unwrap()).unwrap();
    daemon_config["modules"]["a"]["enabled"] = serde_json::json!(false);
    std::fs::write(&config, serde_json::to_vec(&daemon_config).unwrap()).unwrap();
    rig.stamp = rig.rig.stamp(&rig.owner);
    rig.owner
        .lock()
        .unwrap()
        .store
        .record_delivery(&tuple(None), rig.stamp, true)
        .unwrap();
    let private = root(&rig.project_scratch, "writer-held");
    rig.set_projects(vec![member("A", None, &[&private])]);
    let path = private.join(".cortexkit/a.jsonc");
    let view = finish(
        &rig.owner,
        prepare(&rig, vec![edit("project", Some("A"), None, 4)])
            .await
            .unwrap(),
    );
    let (entered, reached) = tokio::sync::oneshot::channel();
    let entered = Mutex::new(Some(entered));
    let (published, publication_seen) = tokio::sync::oneshot::channel();
    let published_file = Mutex::new(Some(published));
    let (release, hold) = std::sync::mpsc::channel();
    let hold = Mutex::new(hold);
    *rig.owner.project_write_hook.lock().unwrap() = Some(Arc::new(move |published| {
        if !published {
            entered.lock().unwrap().take().unwrap().send(()).unwrap();
        } else {
            published_file
                .lock()
                .unwrap()
                .take()
                .unwrap()
                .send(())
                .unwrap();
        }
        hold.lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(10))
            .unwrap();
        Ok(())
    }));
    let daemon = root(&rig.project_scratch, "upgrade-daemon");
    let registry = Arc::new(crate::Registry::default());
    let handle = crate::supervise::SupervisorHandle::new();
    let supervisor = crate::Supervisor::new_for_test(registry.clone(), Default::default())
        .with_handle(handle.clone());
    let executable = cortexkit_test_support::cargo_profile_bin("ck-subc-under-test");
    let checksum = format!(
        "{:x}",
        sha2::Sha256::digest(std::fs::read(&executable).unwrap())
    );
    let declared = crate::supervise::ModuleSpec {
        module_id: "write-fixture".into(),
        program: "/bin/sleep".into(),
        args: vec!["60".into()],
        env: ["XDG_DATA_HOME", "XDG_RUNTIME_DIR", "XDG_CONFIG_HOME"]
            .into_iter()
            .map(|key| (key.into(), daemon.display().to_string()))
            .collect(),
        protocol: subc_control::ModuleProtocol::None,
        script: None,
        carry_containment: false,
        reserved: false,
        reserved_prefixes: vec![],
        overlap: Default::default(),
    };
    let forwarding = Arc::new(crate::ForwardingTable::default());
    forwarding
        .operator_confirms()
        .configure(Arc::new(Approve), Default::default());
    let runtime =
        crate::bootstrap::upgrade_runtime_for_test(&daemon, &supervisor, executable).await;
    let mut control = crate::ControlHandler::with_forwarding(registry, forwarding)
        .with_config_runtime(rig.owner.clone())
        .with_supervisor(handle)
        .with_supervisor_rescan(supervisor.clone(), rig.rig.path.join("subc.jsonc"), None)
        .with_config_os_prompt_policy_for_tests();
    control.upgrade = Some(runtime.clone());
    let control = Arc::new(control);
    let router = Arc::new(crate::Router::with_control_handler(control.clone()));
    let (connection, mut client, task) = crate::server::config_test_connection_loop(router);
    control.record_observed_peer(
        connection,
        subc_os::local_peer::ObservedPeer::Observed(subc_os::local_peer::PeerObservation {
            user: subc_os::local_peer::PeerUser::Uid(subc_os::effective_uid()),
            pid: std::process::id(),
            exe: None,
            signing_id: None,
        }),
    );
    rig.owner.desktop_view_delivered(connection, &view).unwrap();
    *rig.owner.trusted_review_connection_for_test.lock().unwrap() = Some(connection);
    let (finished, waiter_done) = tokio::sync::oneshot::channel();
    *rig.owner.project_write_waiter_done.lock().unwrap() = Some(finished);
    let request = |corr, body: serde_json::Value| {
        crate::Frame::build(
            subc_protocol::FrameType::Request,
            subc_protocol::Flags::new(false, subc_protocol::Priority::Passive, false),
            0,
            0,
            corr,
            serde_json::to_vec(&body).unwrap(),
        )
        .unwrap()
    };
    async fn response(client: &mut tokio::io::DuplexStream) -> crate::Frame {
        tokio::time::timeout(Duration::from_secs(10), crate::read_frame(client))
            .await
            .unwrap()
            .unwrap()
            .unwrap()
    }
    fn assert_settings_busy(reply: crate::Frame) {
        assert_eq!(
            reply.header.ty,
            subc_protocol::FrameType::Error,
            "the upgrade must not begin while the settings writer is blocked: {}",
            String::from_utf8_lossy(&reply.body)
        );
        let refused: subc_protocol::ErrorBody = serde_json::from_slice(&reply.body).unwrap();
        assert_eq!(refused.code, "upgrade_busy");
        assert_eq!(refused.detail.unwrap()["reason"], "settings");
    }
    crate::write_frame(
        &mut client,
        &request(
            11,
            serde_json::json!({"op":"config.apply", "digest":view.digest}),
        ),
    )
    .await
    .unwrap();
    let applied = response(&mut client).await;
    assert_eq!(
        applied.header.ty,
        subc_protocol::FrameType::Response,
        "{}",
        String::from_utf8_lossy(&applied.body)
    );
    tokio::time::timeout(Duration::from_secs(5), reached)
        .await
        .unwrap()
        .unwrap();
    let registered = supervisor
        .register_configured_with_health(
            declared,
            true,
            Default::default(),
            Some(0),
            Default::default(),
            Duration::from_secs(5),
        )
        .unwrap();
    let child = supervisor
        .start_registered_configured_async(registered, None, true)
        .await
        .unwrap();
    let pid = child.status().unwrap().pid.unwrap();
    tokio::time::timeout(Duration::from_secs(5), waiter_done)
        .await
        .unwrap()
        .unwrap();
    let upgrade = |corr| {
        request(
            corr,
            serde_json::json!({"op":"daemon.upgrade_in_place","executable_sha256":checksum}),
        )
    };
    crate::write_frame(&mut client, &upgrade(12)).await.unwrap();
    assert_settings_busy(response(&mut client).await);
    assert!(rig.registry.pool.health().unwrap().charged_workers > 0);
    assert!(!path.exists());
    assert_eq!(
        child.status().unwrap().pid,
        Some(pid),
        "the busy upgrade must not start stopping the child"
    );
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), publication_seen)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&std::fs::read(&path).unwrap()).unwrap()
            ["limit"],
        4
    );
    // Publication is visible before the worker verifies it and drops its settings permit.
    crate::write_frame(&mut client, &upgrade(13)).await.unwrap();
    assert_settings_busy(response(&mut client).await);
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), runtime.wait_for_settings_idle())
        .await
        .unwrap();
    crate::write_frame(&mut client, &upgrade(14)).await.unwrap();
    let receipt = response(&mut client).await;
    assert_eq!(
        receipt.header.ty,
        subc_protocol::FrameType::Response,
        "{}",
        String::from_utf8_lossy(&receipt.body)
    );
    tokio::time::timeout(Duration::from_secs(5), runtime.wait_for_idle())
        .await
        .unwrap();
    assert_ne!(
        child.status().unwrap().pid,
        Some(pid),
        "the valid candidate must get as far as planned stop"
    );
    child.set_enabled(false).await.unwrap();
    drop(client);
    task.await.unwrap().unwrap();
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
        rig.owner
            .apply_control_edits(view.digest, None)
            .await
            .unwrap(),
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
            .apply_control_edits(view.digest, None)
            .await
            .unwrap_err()
            .code,
        "config_unavailable"
    );
    assert_eq!(std::fs::read(&workspace).unwrap(), b"{\"limit\":7}");
    assert!(intents(&rig).is_empty());
}

#[tokio::test]
async fn project_edit_unstamped_refuses_invalid_or_unreadable_affected_tuple() {
    let rig = CaptureRig::with_state(true, RootIoPool::new().unwrap(), false);
    let b = root(&rig.project_scratch, "b");
    let projects = vec![member("A", Some("W"), &[]), member("B", Some("W"), &[&b])];
    rig.set_projects(projects);
    let workspace = write_layer(&rig.workspace, 4);
    let sibling = write_layer(&b, 6);
    let request = edit("workspace", None, Some("W"), 7);
    let prepared = prepare(&rig, vec![request.clone()]).await.unwrap();
    assert_eq!(prepared.project_reviews().count(), 0);
    let view = rig.owner.capture_refusal(&prepared).unwrap().unwrap();
    assert_eq!(view.refusals.len(), 1);
    assert_eq!(view.refusals[0].project_id.as_deref(), Some("B"));
    assert_eq!(
        rig.owner
            .apply_control_edits(view.digest, None)
            .await
            .unwrap(),
        None
    );
    assert_eq!(std::fs::read(&workspace).unwrap(), br#"{"limit":4}"#);

    std::fs::remove_file(&sibling).unwrap();
    std::fs::create_dir(&sibling).unwrap();
    let prepared = prepare(&rig, vec![request]).await.unwrap();
    let view = rig.owner.capture_refusal(&prepared).unwrap().unwrap();
    assert_eq!(view.refusals.len(), 1);
    assert_eq!(view.refusals[0].project_id.as_deref(), Some("B"));
    assert_eq!(view.refusals[0].code, "layer_not_regular");
    assert_eq!(
        view.refusals[0].message,
        "entry does not have the requested file type"
    );
    assert_eq!(std::fs::read(&workspace).unwrap(), br#"{"limit":4}"#);
}

#[tokio::test]
async fn project_edit_unstamped_rechecks_membership_target_bytes_and_registry_outage() {
    let rig = CaptureRig::with_state(true, RootIoPool::new().unwrap(), false);
    let a = member("A", Some("W"), &[]);
    rig.set_projects(vec![a.clone()]);
    let target = write_layer(&rig.workspace, 4);
    let plan = || async {
        finish(
            &rig.owner,
            prepare(&rig, vec![edit("workspace", None, Some("W"), 6)])
                .await
                .unwrap(),
        )
    };
    let view = plan().await;
    rig.set_projects(vec![a.clone(), member("B", Some("W"), &[])]);
    assert_eq!(
        rig.owner
            .apply_control_edits(view.digest, None)
            .await
            .unwrap_err()
            .code,
        "config_changed_since_review"
    );
    assert_eq!(std::fs::read(&target).unwrap(), br#"{"limit":4}"#);
    rig.set_projects(vec![a.clone()]);
    let view = plan().await;
    *rig.registry.reply.lock().unwrap() = None;
    assert_eq!(
        rig.owner
            .apply_control_edits(view.digest.clone(), None)
            .await
            .unwrap_err()
            .code,
        "project_registry_unavailable"
    );
    rig.set_projects(vec![a]);
    std::fs::write(&target, br#"{"limit":5}"#).unwrap();
    assert_eq!(
        rig.owner
            .apply_control_edits(view.digest, None)
            .await
            .unwrap_err()
            .code,
        "config_changed_since_review"
    );
    assert_eq!(std::fs::read(&target).unwrap(), br#"{"limit":5}"#);
}

#[tokio::test]
async fn project_edit_unstamped_lists_each_reading_projects_changed_field() {
    let rig = CaptureRig::with_state(true, RootIoPool::new().unwrap(), false);
    rig.set_projects(vec![member("A", Some("W"), &[])]);
    write_layer(&rig.workspace, 4);
    let plan = prepare(&rig, vec![edit("workspace", None, Some("W"), 6)])
        .await
        .unwrap();
    assert_eq!(plan.project_reviews().count(), 0);
    let fields = plan.unreviewed_fields().unwrap();
    assert_eq!(fields.len(), 1, "{fields:?}");
    assert_eq!(fields[0].module_id, "a");
    assert_eq!(fields[0].project_id.as_deref(), Some("A"));
    assert_eq!(fields[0].path, "/limit");
    assert_eq!(fields[0].old, subc_config::RecordedValue::new(json!(4)));
    assert_eq!(fields[0].new, subc_config::RecordedValue::new(json!(6)));
    assert_eq!(fields[0].old_source, subc_config::Layer::Workspace);
    assert_eq!(fields[0].new_source, subc_config::Layer::Workspace);
}
