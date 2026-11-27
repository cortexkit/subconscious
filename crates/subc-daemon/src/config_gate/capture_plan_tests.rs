use super::*;
use crate::config_gate::{
    project_registry::{
        ProjectRegistry, RegistryBatchFuture, RegistryFuture, RegistryRevision, RegistryScope,
    },
    root_io::RootIoPool,
};
use crate::config_store::Tuple;
use cortexkit_project_identity::EnumerateReply;
use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
    sync::Arc,
};
use subc_protocol::config::{ConfigLayer, ConfigReviewReply, ConfigReviewRequest};

struct Registry {
    reply: Mutex<Option<EnumerateReply>>,
    pool: RootIoPool,
    enumerations: AtomicUsize,
    after_snapshot_poll: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
}

impl Registry {
    fn reply(&self) -> Result<EnumerateReply> {
        let reply = self.reply.lock().unwrap();
        self.enumerations.fetch_add(1, Ordering::Relaxed);
        reply.clone().ok_or_else(|| {
            GateError::new(
                "project_registry_unavailable",
                "fixture registry disconnected",
            )
        })
    }
}

impl ProjectRegistry for Registry {
    fn enumerate_live(
        &self,
    ) -> crate::config_gate::project_registry::RegistryEnumerationFuture<'_> {
        Box::pin(async { self.reply() })
    }

    fn root_io(&self) -> Option<&RootIoPool> {
        Some(&self.pool)
    }

    fn root_io_health(&self) -> Option<subc_control::config::ConfigRootIo> {
        self.pool.health()
    }

    fn snapshot_scoped<'a>(
        &'a self,
        module: &'a str,
        scope: &'a RegistryScope,
    ) -> RegistryFuture<'a> {
        Box::pin(async move {
            let reply = self.reply()?;
            let mut capture = std::pin::pin!(RegistryRevision::from_scope(
                &self.pool, &reply, module, scope
            ));
            let mut signal = self.after_snapshot_poll.lock().unwrap().take();
            std::future::poll_fn(|cx| {
                let result = std::future::Future::poll(capture.as_mut(), cx);
                if let Some(signal) = signal.take() {
                    let _ = signal.send(());
                }
                result
            })
            .await
        })
    }

    fn capture_batch<'a>(
        &'a self,
        scopes: &'a BTreeMap<String, RegistryScope>,
    ) -> RegistryBatchFuture<'a> {
        Box::pin(async move {
            let reply = self.reply()?;
            let mut captured = BTreeMap::new();
            for (module, scope) in scopes {
                captured.insert(
                    module.clone(),
                    RegistryRevision::from_scope(&self.pool, &reply, module, scope).await?,
                );
            }
            Ok((Some(reply), captured))
        })
    }
}

fn root(parent: &Path, name: &str) -> PathBuf {
    let path = parent.join(name);
    std::fs::create_dir_all(&path).unwrap();
    #[cfg(unix)]
    let path = path.canonicalize().unwrap();
    path
}

fn file_id(path: &Path) -> String {
    let handle = subc_os::open_directory_nofollow_all(path).unwrap();
    #[cfg(unix)]
    {
        let id = subc_os::retained_file_identity(&handle).unwrap();
        format!("unix:{}:{}", id.device, id.inode)
    }
    #[cfg(windows)]
    {
        let id = subc_os::retained_file_index(&handle).unwrap();
        format!("windows:{}:{}", id.volume_serial, id.file_index)
    }
}

fn member(id: &str, workspace: Option<&str>, roots: &[&Path]) -> serde_json::Value {
    json!({"projectId":id,"name":id,"roots":roots,"implicit":false,"refKind":"local","workspaceId":workspace,
        "rootRecords":roots.iter().map(|path| json!({"root":path,"remotes":[],"incarnation":{"kind":"token","value":"checkout"},"registrationEpoch":null,
            "identity":"bound","approval":{"state":"approved","journalSeq":1},"fileId":file_id(path)})).collect::<Vec<_>>()})
}

fn enumerate(workspace: &Path, projects: Vec<serde_json::Value>) -> EnumerateReply {
    serde_json::from_value(json!({"generation":7,"incarnation":"registry-fixture","projects":projects,
        "workspaces":[{"workspaceId":"W","name":"Workspace","root":workspace,"fileId":file_id(workspace),"approval":{"state":"approved","journalSeq":1}}]})).unwrap()
}

fn write_layer(root: &Path, limit: u64) -> PathBuf {
    let path = root.join(".cortexkit").join("a.jsonc");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, json!({"limit": limit}).to_string()).unwrap();
    path
}

#[tokio::test]
async fn config_pending_settings_refusal_of_a_deleted_workspace_does_not_settle_its_user_file() {
    let rig = CaptureRig::new();
    let project = root(&rig.project_scratch, "workspace-deletion-reader");
    rig.set_projects(vec![member("reader", Some("W"), &[&project])]);
    let workspace = write_layer(&rig.workspace, 4);
    let initial = finish(&rig.owner, rig.prepare().await.unwrap());
    rig.apply(&initial).await.unwrap();
    let user = rig.rig.path.join("cortexkit/a.jsonc");
    std::fs::write(&user, b"{\"limit\":5}").unwrap();
    std::fs::remove_file(&workspace).unwrap();
    let mut prepared = rig.owner.prepare_pending_settings().await.unwrap().unwrap();
    let captured = &prepared.candidate.modules["a"];
    let schema = &captured.schema;
    let base = prepared.bases.get("a");
    let base_schema = prepared.base_schemas.get("a");
    let accepted =
        crate::config_gate::review::expected_fields("a", schema, base, base_schema, &captured.view)
            .unwrap()
            .into_values()
            .collect();
    assert_eq!(
        crate::config_gate::review::checked_fields(
            "a",
            schema,
            base,
            base_schema,
            &captured.view,
            ConfigReviewReply::new(accepted, vec![])
        )
        .unwrap()
        .len(),
        1
    );
    let (tuple, review) = prepared.project_reviews().next().unwrap();
    assert!(review
        .approval
        .view
        .layers
        .iter()
        .any(|layer| layer.path.as_deref() == workspace.to_str() && layer.bytes.is_none()));
    let tuple = tuple.clone();
    let error = prepared
        .accept_project_review(
            &tuple,
            ConfigReviewReply::new(
                vec![],
                vec![subc_protocol::config::ConfigReviewRefusal::new(
                    "refused",
                    vec!["/limit".into()],
                    vec![],
                )],
            ),
        )
        .unwrap_err();
    assert_eq!(error.code, "module_review_refused");
    assert_eq!(error.settings_review_paths, [workspace]);
}

fn tuple(project: Option<&str>) -> Tuple {
    Tuple {
        module: "a".into(),
        project: project.map(str::to_owned),
    }
}

struct CaptureRig {
    rig: Rig,
    owner: Arc<RuntimeOwner>,
    registry: Arc<Registry>,
    project_scratch: cortexkit_test_support::ScratchDir,
    workspace: PathBuf,
    stamp: u64,
}

impl CaptureRig {
    fn new() -> Self {
        Self::with_capture_enabled(true)
    }

    fn with_capture_enabled(enabled: bool) -> Self {
        Self::with_pool(enabled, RootIoPool::new().unwrap())
    }

    fn with_pool(enabled: bool, pool: RootIoPool) -> Self {
        Self::with_state(enabled, pool, true)
    }

    fn with_state(enabled: bool, pool: RootIoPool, stamped: bool) -> Self {
        Self::with_declaration(enabled, pool, stamped, |_| {})
    }

    fn with_declaration(
        enabled: bool,
        pool: RootIoPool,
        stamped: bool,
        amend: impl FnOnce(&mut subc_protocol::config::ConfigDeclareRequest),
    ) -> Self {
        Self::with_declaration_and_sibling(enabled, pool, stamped, false, amend)
    }

    fn with_declaration_and_sibling(
        enabled: bool,
        pool: RootIoPool,
        stamped: bool,
        sibling: bool,
        amend: impl FnOnce(&mut subc_protocol::config::ConfigDeclareRequest),
    ) -> Self {
        let mut rig = Rig::new();
        rig.declaration.schema["x-ck-layers"] = json!(["user", "workspace", "project"]);
        rig.declaration.schema["properties"]
            .as_object_mut()
            .unwrap()
            .remove("credential");
        rig.declaration.schema["properties"]["limit"]["x-ck-bound"] = json!("raise_only");
        amend(&mut rig.declaration);
        if sibling {
            let path = rig.path.join("subc.jsonc");
            let mut doc: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            doc["modules"]["b"] = doc["modules"]["a"].clone();
            std::fs::write(path, serde_json::to_vec(&doc).unwrap()).unwrap();
        }
        let registry = Arc::new(Registry {
            reply: Mutex::new(None),
            pool,
            enumerations: AtomicUsize::new(0),
            after_snapshot_poll: Mutex::default(),
        });
        // Keep the test archive on the same volume as the project fixtures.
        let project_scratch = cortexkit_test_support::ScratchDir::new("capture-project-roots");
        let mut owner = rig
            .owner("capture-test")
            .with_settings_recovery_data(project_scratch.canonicalize().unwrap());
        owner.capturer.projects = registry.clone();
        if enabled {
            owner.project_capture_enabled = true;
        }
        let owner = Arc::new(owner);
        rig.declare(&owner, 10);
        if sibling {
            owner
                .declare(
                    ConnectionId::new(11),
                    "b",
                    ConfigDeclareRequest::new(
                        rig.declaration.schema.clone(),
                        rig.path.join("cortexkit/b.jsonc").to_string_lossy(),
                        vec![],
                    ),
                    true,
                )
                .unwrap();
        }
        let stamp = if stamped {
            let stamp = rig.stamp(&owner);
            owner
                .lock()
                .unwrap()
                .store
                .record_delivery(&tuple(None), stamp, true)
                .unwrap();
            stamp
        } else {
            0
        };
        // Use owned scratch directories so the registry's scan can open
        // each ancestor without depending on the Windows checkout's ACLs.
        let workspace = root(&project_scratch, "workspace");
        Self {
            rig,
            owner,
            registry,
            project_scratch,
            workspace,
            stamp,
        }
    }

    fn set_projects(&self, projects: Vec<serde_json::Value>) {
        *self.registry.reply.lock().unwrap() = Some(enumerate(&self.workspace, projects));
    }

    async fn prepare(&self) -> Result<PreparedPlan> {
        self.owner.prepare_capture(PlanKind::Change).await
    }

    fn begin(&self, view: &ConfigPlanView) -> Result<u64> {
        self.owner
            .desktop_view_delivered(ConnectionId::new(7), view)
            .unwrap();
        Ok(self
            .owner
            .begin_approval(&view.digest, &Rig::facts())?
            .attempt())
    }

    async fn apply(&self, view: &ConfigPlanView) -> Result<u64> {
        let attempt = self.begin(view)?;
        let witness = self.owner.recheck_capture_apply(attempt).await?;
        self.owner.finish_approval_with_commit_window(
            attempt,
            &Rig::facts(),
            ApprovalReply::Presence(super::super::super::PresenceDecision::InteractiveApproval),
            2,
            None,
            ApprovalRequest::for_attempt(attempt),
            witness,
        )
    }

    fn get(&self, id: &str, connection: u64) -> subc_protocol::config::ConfigGetReply {
        let mut request = ConfigGetRequest::new();
        request.project_id = Some(id.into());
        self.owner
            .module_get(ConnectionId::new(connection), "a", request)
            .unwrap()
    }

    fn ack(&self, project: &str, generation: u64) {
        self.owner
            .lock()
            .unwrap()
            .store
            .record_delivery(&tuple(Some(project)), generation, true)
            .unwrap();
    }
}

#[cfg(all(unix, feature = "test-support"))]
pub(crate) struct RouteSettingsRig(CaptureRig);

#[cfg(all(unix, feature = "test-support"))]
impl RouteSettingsRig {
    pub(crate) const MODULE: &str = "a";

    pub(crate) fn new(projects: bool, amend: impl FnOnce(&mut serde_json::Value)) -> Self {
        Self(CaptureRig::with_declaration(
            projects,
            RootIoPool::new().unwrap(),
            false,
            |declaration| amend(&mut declaration.schema),
        ))
    }

    pub(crate) fn owner(&self) -> &Arc<RuntimeOwner> {
        &self.0.owner
    }

    pub(crate) fn write_user(&self, value: serde_json::Value) {
        std::fs::write(&self.0.rig.declaration.user_file, value.to_string()).unwrap();
    }

    pub(crate) fn stamp(&self) -> u64 {
        let generation = self.0.rig.stamp(self.owner());
        self.owner()
            .lock()
            .unwrap()
            .store
            .record_delivery(&tuple(None), generation, true)
            .unwrap();
        generation
    }

    pub(crate) fn write_project(&self, id: &str, value: serde_json::Value) -> PathBuf {
        let project = root(&self.0.project_scratch, "route-project");
        let path = root(&project, ".cortexkit").join("a.jsonc");
        std::fs::write(path, value.to_string()).unwrap();
        self.0
            .set_projects(vec![member(id, Some("W"), &[&project])]);
        project
    }

    pub(crate) async fn approve_projects(&self) -> u64 {
        let plan = finish(self.owner(), self.0.prepare().await.unwrap());
        self.0.apply(&plan).await.unwrap()
    }

    pub(crate) fn begin_user_approval(&self) -> u64 {
        let owner = self.owner();
        let plan = owner
            .finish_plan(owner.prepare(PlanKind::Change).unwrap(), daemon_review)
            .unwrap();
        self.0.begin(&plan).unwrap()
    }

    pub(crate) fn finish_user_approval(owner: &RuntimeOwner, attempt: u64) -> u64 {
        owner
            .finish_approval(
                attempt,
                &Rig::facts(),
                ApprovalReply::Presence(super::super::super::PresenceDecision::InteractiveApproval),
                2,
            )
            .unwrap()
    }
}

#[tokio::test]
async fn config_pending_settings_workspace_capture_excludes_a_sibling_newly_in_quiet() {
    let rig = CaptureRig::with_declaration_and_sibling(
        true,
        RootIoPool::new().unwrap(),
        true,
        true,
        |_| {},
    );
    let project = root(&rig.project_scratch, "shared-workspace-reader");
    rig.set_projects(vec![member("P", Some("W"), &[&project])]);
    let a = write_layer(&rig.workspace, 4);
    let b = a.with_file_name("b.jsonc");
    std::fs::write(&b, b"{\"limit\":4}").unwrap();
    let (_, observed) = rig
        .owner
        .pending_settings_fingerprints()
        .await
        .unwrap()
        .unwrap();
    let ready = BTreeSet::from([a.clone(), b.clone()]);
    let witnesses = observed.witnesses(&ready);
    assert_eq!(witnesses.len(), 2);
    // B gets a new edit after its path was selected as ready and before
    // the root worker opens the workspace. That edit is not ready for review.
    std::fs::write(&b, b"{\"limit\":5}").unwrap();
    let prepared = rig
        .owner
        .prepare_confirmed_pending_settings(Some(witnesses))
        .await
        .unwrap()
        .unwrap();
    let plan = finish(&rig.owner, prepared);
    let files = &plan.settings_file_change.unwrap().files;
    assert_eq!(
        files.iter().map(|file| &file.path).collect::<Vec<_>>(),
        [&a]
    );
    assert!(plan
        .plan
        .changeset
        .fields
        .iter()
        .all(|field| field.module_id == "a"));
}

#[path = "project_edit_tests.rs"]
mod project_edit_tests;
#[path = "project_settings_tests.rs"]
mod project_settings_tests;
#[path = "unstamped_project_tests.rs"]
mod unstamped_project_tests;

fn reply(review: &crate::config_gate::capture_plan::CaptureReview) -> ConfigReviewReply {
    ConfigReviewReply::new(
        crate::config_gate::review::expected_fields(
            "a",
            &review.schema,
            review.base.as_ref(),
            review.base_schema.as_ref(),
            &review.approval.view,
        )
        .unwrap()
        .into_values()
        .collect(),
        vec![],
    )
}

fn finish(owner: &RuntimeOwner, prepared: PreparedPlan) -> ConfigPlanView {
    try_finish(owner, prepared).unwrap()
}

fn try_finish(owner: &RuntimeOwner, mut prepared: PreparedPlan) -> Result<ConfigPlanView> {
    let replies = prepared
        .project_reviews()
        .map(|(tuple, review)| (tuple.clone(), reply(review)))
        .collect::<Vec<_>>();
    let mut fields = Vec::new();
    for (tuple, reply) in replies {
        fields.extend(prepared.accept_project_review(&tuple, reply).unwrap());
    }
    owner.finish_plan(prepared, |old, new| {
        let mut changeset = daemon_review(old, new)?;
        changeset.fields = fields;
        Ok(changeset)
    })
}

#[tokio::test]
async fn config_pending_settings_workspace_freshness_at_finish_begin_and_commit() {
    for boundary in ["finish", "begin", "commit"] {
        let rig = CaptureRig::new();
        let project = root(&rig.project_scratch, "reviewed-workspace");
        rig.set_projects(vec![member("P", Some("W"), &[&project])]);
        write_layer(&rig.workspace, 4);
        let prepared = rig.owner.prepare_pending_settings().await.unwrap().unwrap();
        let error = if boundary == "finish" {
            write_layer(&rig.workspace, 5);
            try_finish(&rig.owner, prepared).unwrap_err()
        } else {
            let plan = finish(&rig.owner, prepared);
            if boundary == "begin" {
                write_layer(&rig.workspace, 5);
                rig.begin(&plan).unwrap_err()
            } else {
                let attempt = rig.begin(&plan).unwrap();
                let witness = rig.owner.recheck_capture_apply(attempt).await.unwrap();
                let _gates = rig.owner.approval_commit_gates(attempt).await.unwrap();
                write_layer(&rig.workspace, 5);
                rig.owner
                    .finish_approval_with_commit_window(
                        attempt,
                        &Rig::facts(),
                        ApprovalReply::Presence(
                            super::super::super::PresenceDecision::InteractiveApproval,
                        ),
                        2,
                        None,
                        ApprovalRequest::for_attempt(attempt),
                        witness,
                    )
                    .unwrap_err()
            }
        };
        assert_eq!(
            error.code, "config_changed_since_review",
            "{boundary}: {error}"
        );
    }
}

#[tokio::test]
async fn config_pending_settings_mixed_user_and_workspace_share_one_capture() {
    let rig = CaptureRig::new();
    let project = root(&rig.project_scratch, "mixed-workspace");
    rig.set_projects(vec![member("P", Some("W"), &[&project])]);
    std::fs::write(rig.rig.path.join("cortexkit/a.jsonc"), br#"{"limit":3}"#).unwrap();
    write_layer(&rig.workspace, 5);
    let plan = finish(
        &rig.owner,
        rig.owner.prepare_pending_settings().await.unwrap().unwrap(),
    );
    let summary = plan.settings_file_change.as_ref().unwrap();
    assert_eq!(
        summary
            .files
            .iter()
            .map(|file| file.layer)
            .collect::<Vec<_>>(),
        [subc_config::Layer::User, subc_config::Layer::Workspace]
    );
    let generation = rig.apply(&plan).await.unwrap();
    let mut state = rig.owner.lock().unwrap();
    let delivery = state.store.body().unwrap().generations[&generation]
        .delivery
        .as_ref()
        .unwrap();
    assert_eq!(delivery.modules["a"].effective["limit"], 3);
    assert_eq!(delivery.project_views["a"]["P"].effective["limit"], 5);
}

#[tokio::test]
async fn config_pending_settings_fingerprints_ignore_unapproved_workspace() {
    let rig = CaptureRig::new();
    write_layer(&rig.workspace, 5);
    let mut reply = enumerate(&rig.workspace, vec![member("P", Some("W"), &[])]);
    let approved = reply.workspaces[0].approval.take();
    *rig.registry.reply.lock().unwrap() = Some(reply.clone());
    let (_, unapproved) = rig
        .owner
        .pending_settings_fingerprints()
        .await
        .unwrap()
        .unwrap();
    assert!(unapproved.is_complete());
    assert_eq!(unapproved.observed_path_count(), 1);

    reply.workspaces[0].approval = approved;
    *rig.registry.reply.lock().unwrap() = Some(reply);
    let (_, approved) = rig
        .owner
        .pending_settings_fingerprints()
        .await
        .unwrap()
        .unwrap();
    assert!(approved.is_complete());
    assert_eq!(approved.observed_path_count(), 2);
}

#[tokio::test]
async fn config_pending_settings_fingerprint_batches_keep_healthy_roots_when_one_is_busy() {
    let rig = CaptureRig::new();
    let mut reply = enumerate(&rig.workspace, Vec::new());
    for index in 0..24 {
        let folder = root(&rig.project_scratch, &format!("poll-{index}"));
        write_layer(&folder, 4);
        let mut additional = enumerate(&folder, Vec::new()).workspaces.remove(0);
        additional.workspace_id = format!("poll-{index}");
        reply.workspaces.push(additional);
    }
    *rig.registry.reply.lock().unwrap() = Some(reply);
    let (_, all) = rig
        .owner
        .pending_settings_fingerprints()
        .await
        .unwrap()
        .unwrap();
    assert!(all.is_complete());
    assert_eq!(all.observed_path_count(), 26);

    let (entered, started) = tokio::sync::oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    let blocker = rig
        .registry
        .pool
        .submit(rig.workspace.clone(), move || {
            entered.send(()).unwrap();
            wait.recv().unwrap();
        })
        .unwrap();
    started.await.unwrap();
    let (_, partial) = rig
        .owner
        .pending_settings_fingerprints()
        .await
        .unwrap()
        .unwrap();
    release.send(()).unwrap();
    blocker.wait().await.unwrap();
    assert!(!partial.is_complete());
    assert_eq!(partial.observed_path_count(), 25);
}

#[tokio::test]
async fn config_pending_settings_healthy_workspace_survives_a_busy_sibling_root() {
    let rig = CaptureRig::new();
    let healthy_project = root(&rig.project_scratch, "healthy-project");
    let busy_project = root(&rig.project_scratch, "busy-project");
    let busy_root = root(&rig.project_scratch, "busy-workspace");
    let mut reply = enumerate(
        &rig.workspace,
        vec![
            member("healthy", Some("W"), &[&healthy_project]),
            member("busy", Some("busy-W"), &[&busy_project]),
        ],
    );
    let mut second = enumerate(&busy_root, Vec::new()).workspaces.remove(0);
    second.workspace_id = "busy-W".into();
    reply.workspaces.push(second);
    *rig.registry.reply.lock().unwrap() = Some(reply);
    write_layer(&rig.workspace, 4);
    write_layer(&busy_root, 5);

    let (entered, started) = tokio::sync::oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    let busy = rig
        .registry
        .pool
        .submit(busy_root, move || {
            entered.send(()).unwrap();
            let _ = wait.recv();
        })
        .unwrap();
    started.await.unwrap();
    let (_, observed) = rig
        .owner
        .pending_settings_fingerprints()
        .await
        .unwrap()
        .unwrap();
    assert!(!observed.is_complete());
    let prepared = rig
        .owner
        .prepare_confirmed_pending_settings(Some(observed.witnesses(&observed.confirmed_paths())))
        .await
        .unwrap()
        .unwrap();
    let plan = finish(&rig.owner, prepared);
    assert_eq!(
        plan.settings_file_change.as_ref().unwrap().files[0]
            .workspace_id
            .as_deref(),
        Some("W")
    );
    assert_eq!(plan.settings_file_change.as_ref().unwrap().files.len(), 1);
    assert!(plan
        .plan
        .changeset
        .fields
        .iter()
        .all(|field| field.project_id.as_deref() == Some("healthy")));
    rig.apply(&plan).await.unwrap();
    release.send(()).unwrap();
    let _ = busy.wait().await;
}

#[tokio::test]
async fn config_pending_settings_late_root_restore_reports_retained_copy() {
    let rig = CaptureRig::new();
    let project = root(&rig.project_scratch, "late-workspace-reader");
    rig.set_projects(vec![member("reader", Some("W"), &[&project])]);
    let path = write_layer(&rig.workspace, 4);
    let initial = finish(&rig.owner, rig.prepare().await.unwrap());
    rig.apply(&initial).await.unwrap();
    write_layer(&rig.workspace, 5);
    let plan = finish(
        &rig.owner,
        rig.owner.prepare_pending_settings().await.unwrap().unwrap(),
    );
    let (entered, started) = tokio::sync::oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    let wait = Mutex::new(Some((entered, wait)));
    *rig.owner.project_write_hook.lock().unwrap() = Some(Arc::new(move |_| {
        let (entered, wait) = wait.lock().unwrap().take().unwrap();
        entered.send(()).unwrap();
        wait.recv().unwrap();
        Ok(())
    }));
    let owner = rig.owner.clone();
    let revert = tokio::spawn(async move { owner.revert_capture_plan(plan.digest).await });
    started.await.unwrap();
    write_layer(&rig.workspace, 6);
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(30)).await;
    assert!(
        revert.await.unwrap().is_err(),
        "the waiter must time out before the root resumes"
    );
    let staged = rig.owner.polled_status().unwrap();
    assert_eq!(staged.settings_recovery_copy_count, Some(1));
    assert!(staged.settings_recovery_copies[0]
        .recovery_path
        .contains(".declined-"));
    release.send(()).unwrap();
    tokio::time::resume();
    let delivery = rig.owner.delivery_notifications();
    let copy = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let changed = delivery.notified();
            if let Some(copy) = rig
                .owner
                .polled_status()
                .unwrap()
                .settings_recovery_copies
                .into_iter()
                .find(|copy| !copy.recovery_path.contains(".declined-"))
            {
                break copy;
            }
            changed.await;
        }
    })
    .await
    .expect("root completion must publish after the waiter is gone");
    assert_eq!(copy.path, path.to_str().unwrap());
    assert_eq!(std::fs::read(copy.recovery_path).unwrap(), b"{\"limit\":5}");
    assert_eq!(std::fs::read(path).unwrap(), b"{\"limit\":6}");
}

#[cfg(unix)]
#[tokio::test]
async fn config_pending_settings_missing_workspace_inherits_sibling_mode_cap() {
    use std::os::unix::fs::PermissionsExt;
    let rig = CaptureRig::new();
    let project = root(&rig.project_scratch, "mode-reader");
    rig.set_projects(vec![member("reader", Some("W"), &[&project])]);
    let path = write_layer(&rig.workspace, 4);
    let initial = finish(&rig.owner, rig.prepare().await.unwrap());
    rig.apply(&initial).await.unwrap();
    let sibling = path.with_file_name("sibling.jsonc");
    std::fs::write(&sibling, b"{}").unwrap();
    std::fs::set_permissions(&sibling, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::remove_file(&path).unwrap();
    let plan = finish(&rig.owner, rig.prepare().await.unwrap());
    rig.owner.revert_capture_plan(plan.digest).await.unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"{\"limit\":4}");
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[tokio::test]
async fn config_pending_settings_workspace_is_one_file_and_project_drift_stays_pending() {
    let rig = CaptureRig::new();
    let first = root(&rig.project_scratch, "first");
    let second = root(&rig.project_scratch, "second");
    let unrelated = root(&rig.project_scratch, "other");
    rig.set_projects(vec![
        member("first", Some("W"), &[&first]),
        member("second", Some("W"), &[&second]),
        member("other", None, &[&unrelated]),
    ]);
    write_layer(&rig.workspace, 4);
    write_layer(&unrelated, 5);
    let initial = finish(&rig.owner, rig.prepare().await.unwrap());
    rig.apply(&initial).await.unwrap();

    write_layer(&unrelated, 7);
    assert!(rig
        .owner
        .prepare_pending_settings()
        .await
        .unwrap()
        .is_none());
    let old_explicit = finish(&rig.owner, rig.prepare().await.unwrap());
    assert!(old_explicit
        .plan
        .changeset
        .fields
        .iter()
        .any(|field| field.project_id.as_deref() == Some("other")));

    write_layer(&rig.workspace, 6);
    let prepared = rig.owner.prepare_pending_settings().await.unwrap().unwrap();
    let auto = finish(&rig.owner, prepared);
    let summary = auto.settings_file_change.as_ref().unwrap();
    assert_eq!(summary.files.len(), 1);
    assert_eq!(summary.files[0].layer, subc_config::Layer::Workspace);
    assert_eq!(summary.files[0].workspace_id.as_deref(), Some("W"));
    assert_eq!(summary.files[0].affected_projects, ["first", "second"]);
    let rows = auto
        .plan
        .changeset
        .fields
        .iter()
        .filter_map(|row| row.project_id.as_deref())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(rows, ["first", "second"].into_iter().collect());
    rig.apply(&auto).await.unwrap();
    let explicit = finish(&rig.owner, rig.prepare().await.unwrap());
    assert_eq!(
        explicit
            .plan
            .changeset
            .fields
            .iter()
            .filter_map(|field| field.project_id.as_deref())
            .collect::<Vec<_>>(),
        ["other"]
    );
}

#[tokio::test]
async fn config_pending_settings_workspace_revert_restores_approved_raw_bytes() {
    let rig = CaptureRig::new();
    let project = root(&rig.project_scratch, "workspace-reader");
    rig.set_projects(vec![member("reader", Some("W"), &[&project])]);
    let path = write_layer(&rig.workspace, 4);
    let bytes = b"{ /* preserved comment */ \"limit\": 4 }\n";
    std::fs::write(&path, bytes).unwrap();
    let initial = finish(&rig.owner, rig.prepare().await.unwrap());
    rig.apply(&initial).await.unwrap();

    write_layer(&rig.workspace, 5);
    let plan = finish(
        &rig.owner,
        rig.owner.prepare_pending_settings().await.unwrap().unwrap(),
    );
    rig.owner.revert_capture_plan(plan.digest).await.unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
}

#[tokio::test]
async fn capture_plan_first_review_identifies_each_tuple_and_persists_cutoffs() {
    let rig = CaptureRig::new();
    let private = root(&rig.project_scratch, "private");
    write_layer(&rig.workspace, 4);
    write_layer(&private, 9);
    rig.set_projects(vec![
        member("A", Some("W"), &[&private]),
        member("B", Some("W"), &[]),
        member("C", None, &[]),
    ]);
    let prepared = rig.prepare().await.unwrap();
    let mut requests = Vec::new();
    for (tuple, review) in prepared.project_reviews() {
        let request = ConfigReviewRequest::new(
            review.base.as_ref().map_or(0, |base| base.generation),
            review.approval.view.layers.clone(),
            review.base.clone(),
        );
        let request: ConfigReviewRequest =
            serde_json::from_slice(&serde_json::to_vec(&request).unwrap()).unwrap();
        assert!(request.base.is_none());
        assert_eq!(request.base_generation, 0);
        assert!(request
            .layers
            .iter()
            .all(|layer| layer.project_id == tuple.project));
        assert_eq!(request.layers[1].layer, ConfigLayer::Workspace);
        assert_eq!(request.layers[1].workspace_id.as_deref(), Some("W"));
        requests.push(tuple.project.clone().unwrap());
    }
    assert_eq!(requests, ["A", "B"]);
    let mut wrong = rig.prepare().await.unwrap();
    let mut echo = reply(wrong.project_reviews().next().unwrap().1);
    echo.fields[0].project_id = Some("B".into());
    assert_eq!(
        wrong
            .accept_project_review(&tuple(Some("A")), echo)
            .unwrap_err()
            .code,
        "module_review_invalid"
    );
    assert!(rig.owner.finish_plan(wrong, daemon_review).is_err());

    let plan = finish(&rig.owner, prepared);
    assert_eq!(plan.plan.changeset.affected_projects, ["A", "B"]);
    let generation = rig.apply(&plan).await.unwrap();
    {
        let state = rig.owner.lock().unwrap();
        let saved = &state.store.loaded_admission_state().1.generations[&generation];
        assert_eq!(
            saved.tuples,
            BTreeSet::from([tuple(Some("A")), tuple(Some("B"))])
        );
        let delivery = saved.delivery.as_ref().unwrap();
        assert_eq!(delivery.project_views["a"]["A"].effective["limit"], 9);
        assert_eq!(
            delivery.project_cutoffs["a"]["A"].workspace.effective["limit"],
            4
        );
        assert_eq!(
            delivery.project_cutoffs["a"]["A"].user.effective["limit"],
            1
        );
    }
    for id in ["A", "C"] {
        let get = rig.get(id, 10);
        if id == "A" {
            let pending = get.pending.unwrap();
            assert_eq!(pending.generation, generation);
            assert!(pending.attempt.is_some());
        } else {
            assert!(get.pending.is_none());
        }
        let view = get.active.unwrap();
        assert_eq!(view.generation, rig.stamp);
        assert_eq!(view.project_id.as_deref(), Some(id));
        assert!(view.attempt.is_none());
    }
    rig.ack("A", generation);
    assert_eq!(rig.get("A", 10).active.unwrap().effective["limit"], 9);
}

#[path = "stamped_delivery_tests.rs"]
mod stamped_delivery_tests;

#[tokio::test]
async fn capture_plan_held_paths_keep_approved_bytes_and_observed_proposals() {
    let rig = CaptureRig::new();
    let private = root(&rig.project_scratch, "private");
    let unavailable = root(&rig.project_scratch, "unavailable");
    let independent = root(&rig.project_scratch, "independent");
    let workspace = write_layer(&rig.workspace, 2);
    write_layer(&private, 8);
    write_layer(&independent, 7);
    rig.set_projects(vec![
        member("A", Some("W"), &[&private]),
        member("B", Some("W"), &[&unavailable]),
        member("C", None, &[&independent]),
    ]);
    let first = finish(&rig.owner, rig.prepare().await.unwrap());
    let initial = rig.apply(&first).await.unwrap();
    for project in ["A", "B", "C"] {
        rig.ack(project, initial);
    }
    let moved = unavailable.with_extension("moved");
    std::fs::rename(&unavailable, &moved).unwrap();
    write_layer(&rig.workspace, 3);
    write_layer(&private, 9);
    write_layer(&independent, 8);
    std::fs::write(&rig.rig.declaration.user_file, br#"{"limit":4}"#).unwrap();
    let prepared = rig.prepare().await.unwrap();
    let a = prepared
        .project_reviews()
        .find(|(tuple, _)| tuple.project.as_deref() == Some("A"))
        .unwrap()
        .1;
    assert_eq!(a.base.as_ref().unwrap().generation, initial);
    assert_eq!(a.approval.cutoffs.workspace.effective["limit"], 2);
    assert_eq!(a.approval.cutoffs.user.effective["limit"], 1);
    assert_eq!(a.approval.view.effective["limit"], 9);
    let first = finish(&rig.owner, prepared);
    assert_eq!(first.plan.changeset.affected_projects, ["A", "C"]);
    assert!(first
        .skipped
        .iter()
        .any(|notice| notice.project_id.as_deref() == Some("B")));
    assert!(first
        .skipped
        .iter()
        .any(|notice| notice.code == "config_unavailable"
            && notice.path == workspace.to_string_lossy()));
    assert!(first
        .skipped
        .iter()
        .any(|notice| notice.code == "config_unavailable"
            && notice.path == rig.rig.declaration.user_file));
    write_layer(&rig.workspace, 5);
    let second = finish(&rig.owner, rig.prepare().await.unwrap());
    assert_eq!(
        first.digest, second.digest,
        "new held bytes cannot change a reviewed digest"
    );
    assert_eq!(rig.get("A", 10).active.unwrap().effective["limit"], 8);
    let generation = rig.apply(&second).await.unwrap();
    {
        let state = rig.owner.lock().unwrap();
        let body = state.store.loaded_admission_state().1;
        let project = super::super::super::InputKey::ProjectFile {
            module: "a".into(),
            path: workspace.to_string_lossy().into_owned(),
        };
        let key = serde_json::to_string(&project).unwrap();
        let user_key = serde_json::to_string(&super::super::super::InputKey::Layer {
            module: "a".into(),
            path: rig.rig.declaration.user_file.clone(),
        })
        .unwrap();
        for key in [&key, &user_key] {
            assert_ne!(body.observed.as_ref().unwrap()[key], body.approved[key]);
            assert!(body.proposal.as_ref().unwrap().diff.contains_key(key));
        }
        let saved = body.generations[&generation].delivery.as_ref().unwrap();
        assert_eq!(
            saved.project_cutoffs["a"]["A"].workspace.effective["limit"],
            2
        );
    }
    std::fs::rename(moved, unavailable).unwrap();
    let healthy = rig.prepare().await.unwrap();
    let b = healthy
        .project_reviews()
        .find(|(tuple, _)| tuple.project.as_deref() == Some("B"))
        .unwrap()
        .1;
    assert_eq!(b.approval.view.effective["limit"], 5);
}

#[tokio::test]
async fn capture_plan_project_bound_refuses_module_and_names_project() {
    let rig = CaptureRig::new();
    let private = root(&rig.project_scratch, "private");
    write_layer(&rig.workspace, 4);
    write_layer(&private, 2);
    rig.set_projects(vec![
        member("A", Some("W"), &[]),
        member("B", Some("W"), &[&private]),
    ]);
    let prepared = rig.prepare().await.unwrap();
    let refusal = rig.owner.capture_refusal(&prepared).unwrap().unwrap();
    assert_eq!(refusal.refusals.len(), 1);
    let row = &refusal.refusals[0];
    assert_eq!(row.module_id, "a");
    assert_eq!(row.project_id.as_deref(), Some("B"));
    assert_eq!(row.workspace_id.as_deref(), Some("W"));
    assert!(row.message.contains("/limit"));
    assert!(rig.owner.lock().unwrap().gate.plans.is_empty());
}

#[tokio::test]
async fn capture_plan_removal_commits_layerless_tuple_and_rereviews_on_user_or_declaration_change()
{
    let rig = CaptureRig::new();
    let workspace = write_layer(&rig.workspace, 4);
    rig.set_projects(vec![
        member("A", Some("W"), &[]),
        member("B", Some("W"), &[]),
    ]);
    let first = finish(&rig.owner, rig.prepare().await.unwrap());
    let initial = rig.apply(&first).await.unwrap();
    for project in ["A", "B"] {
        rig.ack(project, initial);
    }
    std::fs::remove_file(workspace).unwrap();
    assert_eq!(rig.get("B", 10).active.unwrap().effective["limit"], 4);
    let removed = finish(&rig.owner, rig.prepare().await.unwrap());
    assert_eq!(removed.plan.changeset.fields.len(), 2);
    assert!(removed
        .plan
        .changeset
        .fields
        .iter()
        .all(|row| row.old_source == subc_config::Layer::Workspace
            && row.new_source == subc_config::Layer::Default));
    let generation = rig.apply(&removed).await.unwrap();
    for project in ["A", "B"] {
        rig.ack(project, generation);
        let view = rig.get(project, 10).active.unwrap();
        assert_eq!(view.generation, generation);
        assert_eq!(view.effective["limit"], 1);
        assert_eq!(view.layers.len(), 1);
    }
    let mut declaration = rig.rig.declaration.clone();
    declaration.schema["properties"]["limit"]["description"] = json!("Different description");
    rig.owner
        .declare(ConnectionId::new(11), "a", declaration, true)
        .unwrap();
    let get = rig.get("A", 11);
    assert!(get.declaration_pending.is_some());
    assert_eq!(get.active.unwrap().generation, generation);
    let rereview = finish(&rig.owner, rig.prepare().await.unwrap());
    assert_eq!(rereview.plan.changeset.affected_projects, ["A", "B"]);
    assert!(rereview.plan.changeset.fields.is_empty());
    assert!(rig
        .get("A", 11)
        .declaration_pending
        .unwrap()
        .plan_digest
        .is_some());
    rig.apply(&rereview).await.unwrap();
    assert!(rig.get("A", 11).declaration_pending.is_none());
    std::fs::write(&rig.rig.declaration.user_file, br#"{"limit":8}"#).unwrap();
    let prepared = rig.prepare().await.unwrap();
    assert!(prepared.review_modules.contains("a"));
    assert_eq!(prepared.project_reviews().count(), 2);
}

#[tokio::test]
async fn capture_plan_apply_refuses_join_but_allows_unrelated_registry_change_and_advances_base() {
    let rig = CaptureRig::new();
    write_layer(&rig.workspace, 4);
    rig.set_projects(vec![member("A", Some("W"), &[])]);
    let view = finish(&rig.owner, rig.prepare().await.unwrap());
    rig.set_projects(vec![
        member("A", Some("W"), &[]),
        member("B", Some("W"), &[]),
    ]);
    let error = rig.apply(&view).await.unwrap_err();
    assert_eq!(error.code, "config_changed_since_review");
    assert_eq!(
        rig.owner
            .lock()
            .unwrap()
            .store
            .loaded_admission_state()
            .1
            .last_generation,
        rig.stamp
    );
    let reviewed = finish(&rig.owner, rig.prepare().await.unwrap());
    write_layer(&rig.workspace, 5);
    let stale = finish(&rig.owner, rig.prepare().await.unwrap());
    assert_ne!(reviewed.digest, stale.digest);
    rig.set_projects(vec![
        member("A", Some("W"), &[]),
        member("B", Some("W"), &[]),
        member("C", None, &[]),
    ]);
    let generation = rig.apply(&stale).await.unwrap();
    let error = rig.apply(&reviewed).await.unwrap_err();
    assert_eq!(error.code, "config_changed_since_review");
    let state = rig.owner.lock().unwrap();
    let body = state.store.loaded_admission_state().1;
    assert_eq!(
        crate::config_gate::module_base_generation(body, "a"),
        generation
    );
    assert!(body
        .versions
        .iter()
        .any(|(tuple, versions)| tuple.project.as_deref() == Some("B")
            && versions.pending == Some(generation)));
    drop(state);
    let edit =
        serde_json::from_value(json!({"module_id":"a", "layer":"user", "path":"/limit", "set":6}))
            .unwrap();
    let error = rig
        .owner
        .prepare_edits(BTreeMap::from([("a".into(), rig.stamp)]), vec![edit])
        .err()
        .expect("a project approval advances the user edit base immediately");
    assert_eq!(error.code, "config_changed_since_review");
}

#[tokio::test]
async fn capture_plan_registry_outage_preserves_legacy_user_only_capture() {
    let rig = CaptureRig::new();
    std::fs::write(&rig.rig.declaration.user_file, br#"{"limit":3}"#).unwrap();
    let prepared = rig.prepare().await.unwrap();
    assert!(prepared.review_modules.contains("a"));
    assert_eq!(prepared.project_reviews().count(), 0);
    let view = finish(&rig.owner, prepared);
    assert!(view
        .plan
        .changeset
        .apply_plan
        .get("tuple_targets")
        .is_none());
    assert!(rig.apply(&view).await.is_ok());
}

#[tokio::test]
async fn capture_plan_registry_outage_refuses_known_membership_without_storing_plan() {
    let rig = CaptureRig::new();
    rig.set_projects(vec![member("A", None, &[])]);
    rig.prepare().await.unwrap();
    rig.set_projects(vec![]);
    rig.prepare().await.unwrap();
    *rig.registry.reply.lock().unwrap() = None;
    let error = rig
        .prepare()
        .await
        .err()
        .expect("known members require a fresh enumeration");
    assert_eq!(error.code, "project_registry_unavailable");
    assert!(error.detail.contains("try again"));
    assert!(error.detail.contains("fixture registry disconnected"));
    assert!(rig.owner.lock().unwrap().gate.plans.is_empty());
}

#[tokio::test]
async fn capture_plan_registry_outage_keeps_reviewed_apply_available_for_retry() {
    let rig = CaptureRig::new();
    write_layer(&rig.workspace, 4);
    rig.set_projects(vec![member("A", Some("W"), &[])]);
    let view = finish(&rig.owner, rig.prepare().await.unwrap());
    *rig.registry.reply.lock().unwrap() = None;
    let error = rig.apply(&view).await.unwrap_err();
    assert_eq!(error.code, "project_registry_unavailable");
    assert!(error.detail.contains("fixture registry disconnected"));
    {
        let state = rig.owner.lock().unwrap();
        assert!(state.gate.plans.contains_key(&view.digest));
        assert_eq!(
            state.store.loaded_admission_state().1.last_generation,
            rig.stamp
        );
    }
    rig.set_projects(vec![member("A", Some("W"), &[])]);
    assert!(rig.apply(&view).await.is_ok());
}

#[tokio::test]
async fn capture_plan_repeated_unchanged_drift_does_not_republish_proposal() {
    let rig = CaptureRig::new();
    let path = write_layer(&rig.workspace, 4);
    rig.set_projects(vec![member("A", Some("W"), &[])]);
    let first = finish(&rig.owner, rig.prepare().await.unwrap());
    let (sequence, proposal) = {
        let state = rig.owner.lock().unwrap();
        let body = state.store.loaded_admission_state().1;
        let proposal = body.proposal.clone().unwrap();
        let key = serde_json::to_string(&super::super::super::InputKey::ProjectFile {
            module: "a".into(),
            path: path.to_string_lossy().into_owned(),
        })
        .unwrap();
        assert!(proposal.diff[&key].is_some());
        assert!(!body.approved.contains_key(&key));
        (state.store.publication_sequence(), proposal)
    };
    let second = finish(&rig.owner, rig.prepare().await.unwrap());
    assert_eq!(first.digest, second.digest);
    let state = rig.owner.lock().unwrap();
    assert_eq!(state.store.publication_sequence(), sequence);
    assert_eq!(
        state.store.loaded_admission_state().1.proposal.as_ref(),
        Some(&proposal)
    );
}

#[tokio::test]
async fn capture_plan_rollout_off_preserves_user_plan_and_get_bytes() {
    let rig = CaptureRig::with_capture_enabled(false);
    write_layer(&rig.workspace, 4);
    rig.set_projects(vec![member("A", Some("W"), &[])]);
    let user = rig
        .owner
        .module_get(ConnectionId::new(10), "a", ConfigGetRequest::new())
        .unwrap();
    let bytes = serde_json::to_vec(&user).unwrap();
    let mut expected = br##"{"op":"config.get","active":{"generation":1,"effective":{"limit":1},"sources":{"/limit":"default"},"layers":[{"module_id":"a","layer":"user","path":"##.to_vec();
    expected.extend(serde_json::to_vec(&rig.rig.declaration.user_file).unwrap());
    expected.extend_from_slice(br#"}],"status":"active"}}"#);
    assert_eq!(
        String::from_utf8_lossy(&bytes),
        String::from_utf8_lossy(&expected)
    );

    std::fs::write(&rig.rig.declaration.user_file, br#"{"limit":3}"#).unwrap();
    // The baseline plan preparation does not consult the project registry.
    let baseline = finish(&rig.owner, rig.owner.prepare(PlanKind::Change).unwrap());
    let actual = finish(&rig.owner, rig.prepare().await.unwrap());
    let plan_reply = |view| subc_control::config::ConfigControlResponse::Plan { view };
    assert_eq!(
        serde_json::to_vec(&plan_reply(actual.clone())).unwrap(),
        serde_json::to_vec(&plan_reply(baseline)).unwrap()
    );
    let wire = serde_json::to_value(plan_reply(actual)).unwrap();
    let view = wire["view"].as_object().unwrap();
    assert_eq!(
        view.keys().map(String::as_str).collect::<Vec<_>>(),
        ["base_revision", "digest", "plan", "unstamped"]
    );
    assert_eq!(view["unstamped"], false);
    assert!(wire["view"]["plan"]["changeset"]["apply_plan"]
        .get("tuple_targets")
        .is_none());
}
