use super::settings_observation::SettingsFiles;
use super::*;
use crate::config_gate::{
    capture::CapturedFile, capture_plan::approved_user, project_registry::RegistryScope,
    GateSnapshot, InputKey,
};
use std::path::{Path, PathBuf};

impl OwnerState {
    pub(super) fn claim_automatic_settings_plan(
        &mut self,
        digest: &Digest,
        token: u64,
    ) -> Result<()> {
        let plan =
            self.gate.plans.get_mut(digest).ok_or_else(|| {
                GateError::new("plan_unknown", "plan is absent or already applied")
            })?;
        if plan.kept_after_refused_decline || plan.decline_accepted || plan.restoring {
            return Err(GateError::new(
                "settings_decline_kept",
                "the declined settings plan is waiting for a manual restore",
            ));
        }
        if plan
            .settings_attempt_owner
            .is_some_and(|owner| owner != token)
        {
            return Err(GateError::new(
                "config_busy",
                "a settings approval already owns this plan",
            ));
        }
        plan.settings_attempt_owner = Some(token);
        Ok(())
    }
    pub(super) fn pending_settings_card_at(&self, base: u64) -> Option<Digest> {
        let (digest, attempt) = self.pending_settings_card.as_ref()?;
        self.gate
            .plans
            .get(digest)
            .filter(|plan| plan.view.base_revision == base)?;
        self.gate
            .approval_digest(*attempt)
            .ok()
            .map(|_| digest.clone())
    }

    fn active_settings_approval(&self) -> Option<(&Digest, u64, bool)> {
        let (digest, attempt, phone) = self.open_settings_approval.as_ref()?;
        self.gate
            .approval_digest(*attempt)
            .ok()
            .map(|_| (digest, *attempt, *phone))
    }

    pub(super) fn check_settings_approval(&self, digest: &Digest, daemon: bool) -> Result<()> {
        if let Some((open, _, phone)) = self.active_settings_approval() {
            if daemon {
                return Err(GateError::new(
                    "config_busy",
                    "a settings approval is already open",
                ));
            }
            if open == digest && phone {
                return Err(approval_pending(digest));
            }
        }
        Ok(())
    }
}

fn approval_pending(digest: &Digest) -> GateError {
    GateError::new(
        "config_approval_pending",
        "a card for this change is open on an approving device",
    )
    .with_wire_detail(serde_json::json!({"digest": digest}))
}

fn fingerprint_paths(paths: impl IntoIterator<Item = PathBuf>) -> SettingsFiles {
    let mut files = SettingsFiles::default();
    for path in paths {
        if files.capture(&path).is_err() {
            files.mark_incomplete();
        }
    }
    files
}

type UserRestore = (
    PathBuf,
    subc_config::InputState,
    Option<Vec<u8>>,
    Option<super::super::restore::UserRestoreMetadata>,
);

struct RestoreFiles {
    creation_sequence: u64,
    users: Vec<UserRestore>,
    projects: Vec<super::super::restore::ProjectRestore>,
    superseded: Vec<PathBuf>,
    restored: Vec<PathBuf>,
}

fn plan_file_paths(plan: &super::super::PendingPlan) -> BTreeSet<PathBuf> {
    use crate::config_store::ProjectFileApproval;
    plan.changed
        .iter()
        .filter_map(|key| match key {
            InputKey::Layer { path, .. } if plan.snapshot.inputs.contains_key(key) => {
                Some(PathBuf::from(path))
            }
            _ => None,
        })
        .chain(plan.project_files.iter().filter_map(|file| match file {
            ProjectFileApproval::Adopt { target, .. } => target.path(),
            _ => None,
        }))
        .collect()
}

use subc_control::config::ConfigRevertFile;

#[derive(Default)]
struct RestoreResults {
    files: Vec<ConfigRevertFile>,
    first: Option<GateError>,
    completed: BTreeSet<PathBuf>,
    failed: BTreeSet<PathBuf>,
}

impl RestoreResults {
    fn record(&mut self, path: &Path, result: Result<()>) {
        match &result {
            Ok(()) => {
                self.completed.insert(path.to_owned());
            }
            Err(error) if error.code != "settings_restore_superseded" => {
                self.failed.insert(path.to_owned());
                if error.restore_installed {
                    self.completed.insert(path.to_owned());
                }
            }
            Err(_) => {}
        }
        let path = path.to_string_lossy().into_owned();
        let row = match result {
            Ok(()) => ConfigRevertFile::restored(path),
            Err(error) => {
                let row = ConfigRevertFile::refused(
                    path,
                    error.code.clone(),
                    error.detail.clone(),
                    error.wire_detail.as_deref().cloned(),
                );
                self.first.get_or_insert(error);
                row
            }
        };
        self.files.push(row);
    }

    fn failed(paths: impl IntoIterator<Item = PathBuf>, error: GateError) -> GateError {
        Self::failed_with_progress(paths, &[], &[], error)
    }

    fn failed_with_progress(
        paths: impl IntoIterator<Item = PathBuf>,
        superseded: &[PathBuf],
        restored: &[PathBuf],
        error: GateError,
    ) -> GateError {
        let mut results = Self {
            first: Some(error.clone()),
            ..Self::default()
        };
        for path in paths {
            results.record(
                &path,
                if restored.contains(&path) {
                    Ok(())
                } else if superseded.contains(&path) {
                    Err(restore_superseded())
                } else {
                    Err(error.clone())
                },
            );
        }
        results.finish().unwrap_err()
    }

    fn finish(self) -> Result<Vec<ConfigRevertFile>> {
        if let Some(mut error) = self.first {
            let old = error.wire_detail.take().map(|detail| *detail);
            let mut detail = match old {
                Some(serde_json::Value::Object(detail)) => detail,
                Some(value) => serde_json::Map::from_iter([("failure".into(), value)]),
                None => serde_json::Map::new(),
            };
            detail.insert("files".into(), serde_json::json!(self.files));
            return Err(error.with_wire_detail(detail.into()));
        }
        Ok(self.files)
    }
}

impl RuntimeOwner {
    pub(crate) fn set_settings_review_errors(
        &self,
        review_base: u64,
        errors: Vec<subc_control::config::ConfigStatusFileError>,
    ) -> Result<bool> {
        let mut state = self.lock()?;
        let (store_state, body) = state.store.loaded_admission_state();
        if store_state != StoreState::Stamped || body.base_revision != review_base {
            return Ok(false);
        }
        if state.settings_review_errors_base != Some(review_base) {
            state.settings_review_errors.clear();
            state.settings_review_errors_base = Some(review_base);
        }
        state.settings_review_errors.retain(|previous| {
            !errors
                .iter()
                .any(|error| error.error.path == previous.error.path)
        });
        state.settings_review_errors.extend(errors);
        Ok(true)
    }

    pub(crate) fn stamped_settings_base(&self) -> Option<u64> {
        let snapshot = self.polled_snapshot().ok()?;
        (snapshot.projection.status.state == "stamped")
            .then_some(snapshot.projection.status.base_revision)
    }

    pub(crate) fn clear_changed_settings_review_errors(
        &self,
        changed: &BTreeSet<PathBuf>,
    ) -> Result<()> {
        self.lock()?.settings_review_errors.retain(|row| {
            row.error
                .path
                .as_deref()
                .is_none_or(|path| !changed.contains(Path::new(path)))
        });
        Ok(())
    }

    pub(crate) fn clear_settings_review_success(
        &self,
        base: u64,
        paths: &BTreeSet<PathBuf>,
    ) -> Result<()> {
        let mut state = self.lock()?;
        let (store, body) = state.store.loaded_admission_state();
        if store == StoreState::Stamped
            && body.base_revision == base
            && state.settings_review_errors_base == Some(base)
        {
            state.settings_review_errors.retain(|row| {
                row.error
                    .path
                    .as_deref()
                    .is_none_or(|path| !paths.contains(Path::new(path)))
            });
        }
        Ok(())
    }
    pub(crate) fn with_settings_recovery_data(mut self, data: PathBuf) -> Self {
        self.settings_recovery_data = data;
        self
    }

    pub(crate) async fn revert_capture_plan(
        self: &std::sync::Arc<Self>,
        digest: Digest,
    ) -> Result<Vec<ConfigRevertFile>> {
        let owner = self.clone();
        tokio::spawn(async move { owner.restore_and_finish(digest).await })
            .await
            .map_err(|_| GateError::new("config_unavailable", "settings restore worker stopped"))?
    }

    async fn restore_and_finish(
        self: &std::sync::Arc<Self>,
        digest: Digest,
    ) -> Result<Vec<ConfigRevertFile>> {
        let restore_digest = digest.clone();
        let RestoreFiles {
            users,
            projects,
            creation_sequence,
            superseded,
            restored,
        } = self
            .on_blocking(move |owner| owner.prepare_restore_files(&restore_digest))
            .await
            .map_err(|error| {
                if error
                    .wire_detail
                    .as_deref()
                    .is_some_and(|detail| detail.get("files").is_some())
                {
                    error
                } else {
                    RestoreResults::failed([], error)
                }
            })?;
        let paths = || {
            users
                .iter()
                .map(|(path, ..)| path.clone())
                .chain(projects.iter().filter_map(|project| project.target.path()))
                .chain(superseded.iter().cloned())
                .chain(restored.iter().cloned())
        };
        let archive = self
            .on_blocking(|owner| {
                crate::config_gate::recovery_inventory::RecoveryArchive::open(
                    &owner.settings_recovery_data,
                    owner.settings_recovery_updates.clone(),
                    |directory| {
                        #[cfg(test)]
                        if let Some(hook) = owner.recovery_probe_hook.lock().unwrap().take() {
                            hook()?;
                        }
                        subc_os::probe_retained_directory_write(directory)
                    },
                )
                .map(std::sync::Arc::new)
            })
            .await;
        let archive = match archive {
            Ok(archive) => archive,
            Err(error) => {
                let failed = paths()
                    .filter(|path| !superseded.contains(path) && !restored.contains(path))
                    .collect();
                let kept = self
                    .on_blocking(move |owner| {
                        owner.finish_restore_plan(
                            &digest,
                            creation_sequence,
                            false,
                            BTreeSet::new(),
                            failed,
                        )
                    })
                    .await;
                return Err(restore_keep_error(
                    RestoreResults::failed_with_progress(paths(), &superseded, &restored, error),
                    kept,
                ));
            }
        };
        let active_digest = digest.clone();
        let owned = self
            .on_blocking(move |owner| owner.begin_restore_plan(&active_digest, creation_sequence))
            .await
            .map_err(|error| {
                RestoreResults::failed_with_progress(paths(), &superseded, &restored, error)
            })?;
        let mut results = RestoreResults::default();
        for path in restored {
            results.record(&path, Ok(()));
        }
        for path in superseded {
            results.record(
                &path,
                Err(GateError::new(
                    "settings_restore_superseded",
                    "a newer declined plan owns this settings file",
                )),
            );
        }
        for (path, expected, bytes, metadata) in users {
            if owned.as_ref().is_some_and(|paths| !paths.contains(&path)) {
                results.record(&path, Err(restore_superseded()));
                continue;
            }
            let reported = path.clone();
            let archive = archive.clone();
            let result = self
                .on_blocking(move |_owner| {
                    let metadata = metadata.ok_or_else(super::super::restore::changed)?;
                    let result = metadata.restore(
                        &path,
                        &expected,
                        bytes.as_deref(),
                        &archive,
                        || {
                            #[cfg(test)]
                            if let Some(hook) = _owner.user_restore_hook.lock().unwrap().take() {
                                hook(&path)?;
                            }
                            Ok(())
                        },
                        || {
                            #[cfg(test)]
                            if let Some(hook) =
                                _owner.after_restore_read_hook.lock().unwrap().take()
                            {
                                let _ = hook(&path);
                            }
                        },
                    );
                    _owner.publish_restore_result(&result);
                    result.map_err(|mut error| {
                        if error.code == "config_changed_since_review"
                            && error.wire_detail.is_none()
                        {
                            error.detail = "the user file changed since planning".into();
                        }
                        error
                    })
                })
                .await;
            if let Err(error) = &result {
                tracing::warn!(path = %reported.display(), code = %error.code, detail = %error.detail, "settings file not restored");
            }
            results.record(&reported, result);
        }
        if !projects.is_empty() {
            let authority = async {
                let mut scopes = BTreeMap::<String, RegistryScope>::new();
                for restore in &projects {
                    let target = &restore.target;
                    let scope = scopes.entry(target.module.clone()).or_default();
                    match &target.layer {
                        crate::config_store::ProjectFileLayer::Workspace { workspace_id } => {
                            scope.workspaces.insert(workspace_id.clone());
                        }
                        crate::config_store::ProjectFileLayer::Project { project_id } => {
                            scope.projects.insert(project_id.clone());
                        }
                    }
                }
                let (_, revisions) = self.capturer.projects.capture_batch(&scopes).await?;
                let pool = self.capturer.projects.root_io().ok_or_else(|| {
                    GateError::new("config_unavailable", "registry root I/O is unavailable")
                })?;
                Ok::<_, GateError>((revisions, pool))
            }
            .await;
            for restore in projects {
                let target = restore.target.clone();
                let path = target
                    .path()
                    .unwrap_or_else(|| target.root.join(&target.relative_path));
                if owned.as_ref().is_some_and(|paths| !paths.contains(&path)) {
                    results.record(&path, Err(restore_superseded()));
                    continue;
                }
                let result =
                    match authority
                        .as_ref()
                        .map_err(Clone::clone)
                        .and_then(|(revisions, pool)| {
                            revisions
                                .get(&target.module)
                                .ok_or_else(capture_plan::changed)
                                .and_then(|revision| {
                                    let owner = self.clone();
                                    let restore_digest = digest.clone();
                                    let completed_path = path.clone();
                                    revision.submit_restore(
                                        pool,
                                        restore,
                                        archive.clone(),
                                        move |result| {
                                            owner.publish_restore_result(result);
                                            if result.is_ok()
                                                || result
                                                    .as_ref()
                                                    .is_err_and(|error| error.restore_installed)
                                            {
                                                let _ = owner.note_restored_file(
                                                    &restore_digest,
                                                    creation_sequence,
                                                    completed_path,
                                                );
                                            }
                                        },
                                        #[cfg(test)]
                                        self.project_write_hook.lock().unwrap().clone(),
                                    )
                                })
                        }) {
                        Ok(job) => job
                            .wait()
                            .await
                            .map_err(GateError::from)
                            .and_then(|result| result),
                        Err(error) => Err(error),
                    };
                if let Err(error) = &result {
                    tracing::warn!(root = %target.root.display(), module_id = %target.module, code = %error.code,
                            detail = %error.detail, "settings file not restored");
                }
                results.record(&path, result);
            }
        }
        self.notify_delivery_changed();
        let completed = std::mem::take(&mut results.completed);
        let failed = std::mem::take(&mut results.failed);
        let kept = self
            .on_blocking(move |owner| {
                owner.finish_restore_plan(&digest, creation_sequence, true, completed, failed)
            })
            .await;
        match results.finish() {
            Ok(files) => {
                kept?;
                Ok(files)
            }
            Err(error) => Err(restore_keep_error(error, kept)),
        }
    }

    fn publish_restore_result(&self, result: &Result<()>) {
        let copy = result
            .as_ref()
            .err()
            .and_then(|error| error.wire_detail.as_deref())
            .and_then(|detail| {
                serde_json::from_value::<subc_control::config::ConfigSettingsRecoveryCopy>(
                    detail.clone(),
                )
                .ok()
            });
        if let Some(copy) = copy {
            tracing::warn!(path = %copy.path, recovery_path = %copy.recovery_path, "settings recovery copy kept");
            self.notify_delivery_changed();
        }
    }

    #[cfg(all(test, unix))]
    pub(crate) fn refuse_next_recovery_probe_for_test(&self) {
        *self.recovery_probe_hook.lock().unwrap() = Some(Box::new(|| {
            Err(std::io::ErrorKind::ReadOnlyFilesystem.into())
        }));
    }

    #[cfg(all(test, unix))]
    pub(crate) fn keep_plan_for_test(&self, digest: &Digest) -> Result<()> {
        let mut state = self.lock()?;
        Self::keep_declined_files(&mut state, digest)
    }

    #[cfg(all(test, unix))]
    pub(crate) fn fill_kept_paths_for_test(&self, digest: &Digest) {
        let mut state = self.lock().unwrap();
        state
            .gate
            .plans
            .get_mut(digest)
            .unwrap()
            .kept_after_refused_decline = true;
        for index in 0..64 {
            let path = self
                .settings_recovery_data
                .join(format!("fixture-kept-{index}.jsonc"));
            state.gate.kept_paths.insert(path, digest.clone());
        }
    }

    #[cfg(all(test, unix))]
    pub(crate) fn set_user_restore_hook_for_test(&self, hook: UserRestoreHook) {
        *self.user_restore_hook.lock().unwrap() = Some(hook);
    }

    #[cfg(all(test, unix))]
    pub(crate) fn set_after_restore_read_hook_for_test(&self, hook: UserRestoreHook) {
        *self.after_restore_read_hook.lock().unwrap() = Some(hook);
    }

    fn prepare_restore_files(&self, digest: &Digest) -> Result<RestoreFiles> {
        use crate::config_store::ProjectFileApproval;
        use subc_config::InputState;

        let mut state = self.lock()?;
        Self::check_authority(&mut state, false)?;
        let (store_state, body) = state.store.loaded_admission_state();
        let base_revision = body.base_revision;
        let plan = state
            .gate
            .plans
            .get(digest)
            .filter(|plan| plan.view.base_revision == base_revision)
            .ok_or_else(|| GateError::new("unknown_plan", "the plan is no longer pending"))?;
        if plan.kind != PlanKind::Change
            || plan.edits.is_some()
            || plan.development_approval.is_some()
        {
            return Err(GateError::new(
                "revert_not_capture",
                "only a pending file capture plan can be reverted",
            ));
        }
        if store_state != StoreState::Stamped {
            return Err(GateError::new(
                "revert_not_capture",
                "there are no approved settings to restore",
            ));
        }
        let owns = |path: &Path| {
            !plan.kept_after_refused_decline || state.gate.kept_paths.get(path) == Some(digest)
        };
        let paths = plan_file_paths(plan);
        let superseded: Vec<_> = paths
            .iter()
            .filter(|path| !owns(path) && !plan.restored_paths.contains(*path))
            .cloned()
            .collect();
        let restored: Vec<_> = plan.restored_paths.iter().cloned().collect();
        let result = (|| {
            let approved = GateSnapshot::from_store(&body.approved)?;
            let mut users = Vec::new();
            for key in &plan.changed {
                let InputKey::Layer { module, path } = key else {
                    continue;
                };
                if !owns(Path::new(path)) || plan.restored_paths.contains(Path::new(path)) {
                    continue;
                }
                let Some(captured) = plan.snapshot.inputs.get(key) else {
                    continue;
                };
                let bytes = if approved
                    .inputs
                    .get(key)
                    .is_some_and(|input| input.state != InputState::Absent)
                {
                    let bytes = approved_user(body, module)
                        .and_then(|view| {
                            view.layers
                                .iter()
                                .find(|layer| layer.path.as_deref() == Some(path))
                        })
                        .and_then(|layer| layer.bytes.clone())
                        .ok_or_else(|| {
                            GateError::new(
                                "config_unavailable",
                                "approved user file bytes are not retained",
                            )
                        })?;
                    Some(bytes)
                } else {
                    None
                };
                users.push((
                    path.into(),
                    captured.state.clone(),
                    bytes,
                    plan.user_restore.get(Path::new(path)).cloned(),
                ));
            }
            let mut projects = Vec::new();
            for approval in &plan.project_files {
                let ProjectFileApproval::Adopt { target, .. } = approval else {
                    continue;
                };
                let authority = plan
                    .project_capture
                    .as_ref()
                    .and_then(|capture| capture.authority.get(&target.module))
                    .ok_or_else(capture_plan::changed)?;
                let path = target.path().ok_or_else(capture_plan::changed)?;
                if !owns(&path) || plan.restored_paths.contains(&path) {
                    continue;
                }
                let bytes = if approved
                    .inputs
                    .get(&capture_plan::file_key(&target.module, &path))
                    .is_some_and(|input| input.state != InputState::Absent)
                {
                    let bytes = capture_plan::approved_files(body, &approved, &target.module)
                        .get(&path)
                        .and_then(|layer| layer.bytes.clone())
                        .ok_or_else(|| {
                            GateError::new(
                                "config_unavailable",
                                "approved workspace or project file bytes are not retained",
                            )
                        })?;
                    Some(bytes)
                } else {
                    None
                };
                projects.push(authority.restore_file(target, bytes)?);
            }
            Ok((users, projects))
        })();
        let (users, projects) = result.map_err(|error| {
            RestoreResults::failed_with_progress(paths, &superseded, &restored, error)
        })?;
        Ok(RestoreFiles {
            users,
            projects,
            creation_sequence: plan.creation_sequence,
            superseded,
            restored,
        })
    }

    fn begin_restore_plan(
        &self,
        digest: &Digest,
        creation_sequence: u64,
    ) -> Result<Option<BTreeSet<PathBuf>>> {
        let mut state = self.lock()?;
        Self::check_authority(&mut state, false)?;
        let (store_state, body) = state.store.loaded_admission_state();
        if store_state != StoreState::Stamped
            || !state.gate.plans.get(digest).is_some_and(|plan| {
                plan.view.base_revision == body.base_revision
                    && plan.creation_sequence == creation_sequence
            })
        {
            return Err(GateError::new(
                "unknown_plan",
                "the plan is no longer pending",
            ));
        }
        let owned = state.gate.plans[digest]
            .kept_after_refused_decline
            .then(|| {
                state
                    .gate
                    .kept_paths
                    .iter()
                    .filter(|(_, kept)| *kept == digest)
                    .map(|(path, _)| path.clone())
                    .collect()
            });
        let plan = state
            .gate
            .plans
            .get_mut(digest)
            .expect("checked pending plan");
        if plan.restoring {
            return Err(GateError::new(
                "config_busy",
                "the settings plan is being restored",
            ));
        }
        plan.restoring = true;
        if let Some((_, attempt, _)) = state
            .open_settings_approval
            .take_if(|(open, _, _)| open == digest)
        {
            state.cancel_approval(attempt);
        }
        if let Some((_, attempt)) = state
            .pending_settings_card
            .take_if(|(open, _)| open == digest)
        {
            state.cancel_approval(attempt);
        }
        self.settings_attempts_changed.notify_waiters();
        Ok(owned)
    }

    fn finish_restore_plan(
        &self,
        digest: &Digest,
        sequence: u64,
        started: bool,
        completed: BTreeSet<PathBuf>,
        failed: BTreeSet<PathBuf>,
    ) -> Result<()> {
        let mut state = self.lock()?;
        let Some(plan) = state
            .gate
            .plans
            .get_mut(digest)
            .filter(|plan| plan.creation_sequence == sequence)
        else {
            return Ok(());
        };
        if plan.restoring != started {
            return Err(GateError::new(
                "config_busy",
                "the settings plan is being restored",
            ));
        }
        plan.restoring = false;
        plan.restored_paths.extend(completed);
        let restored = plan.restored_paths.clone();
        let failed: BTreeSet<_> = failed
            .into_iter()
            .filter(|path| {
                state.gate.kept_paths.get(path).is_none_or(|kept| {
                    kept == digest
                        || state
                            .gate
                            .plans
                            .get(kept)
                            .is_none_or(|owner| owner.creation_sequence <= sequence)
                })
            })
            .collect();
        state.gate.kept_paths.retain(|path, kept| {
            kept != digest || !restored.contains(path) || failed.contains(path)
        });
        if failed.is_empty() && started {
            Self::discard_settings_plan(&mut state, digest);
        } else if !failed.is_empty() {
            if let Err(error) = Self::keep_plan_paths(&mut state, digest, failed) {
                if let Some(plan) = state.gate.plans.get_mut(digest) {
                    plan.decline_accepted = false;
                }
                return Err(error);
            }
        }
        Ok(())
    }

    fn note_restored_file(&self, digest: &Digest, sequence: u64, path: PathBuf) -> Result<()> {
        let mut state = self.lock()?;
        if let Some(plan) = state
            .gate
            .plans
            .get_mut(digest)
            .filter(|plan| plan.creation_sequence == sequence)
        {
            plan.restored_paths.insert(path.clone());
        }
        Ok(())
    }
}

fn restore_superseded() -> GateError {
    GateError::new(
        "settings_restore_superseded",
        "a newer declined plan owns this settings file",
    )
}

fn restore_keep_error(mut error: GateError, keep: Result<()>) -> GateError {
    if let Err(keep) = keep {
        error.code = keep.code;
        error.detail = format!("{}; {}", keep.detail, error.detail);
    }
    error
}

impl PreparedPlan {
    pub(crate) fn settings_review_base(&self) -> u64 {
        self.base_revision
    }
    pub(crate) fn settings_review_files(&self) -> Vec<subc_control::config::ConfigStatusFileError> {
        use subc_control::config::{ConfigFileError, ConfigFileErrorLayer, ConfigStatusFileError};
        let row = |module: &str, path: &Path| {
            let mut error = ConfigFileError::new(
                "module_review_refused",
                "the module reviewer refused the settings edit",
            );
            error.path = Some(path.to_string_lossy().into_owned());
            ConfigStatusFileError::new(module.into(), error)
        };
        let mut files = Vec::new();
        if let Some(users) = &self.settings_users {
            files.extend(
                users
                    .iter()
                    .map(|module| row(module, &self.candidate.modules[module].file.path)),
            );
        }
        if let Some(capture) = &self.project_capture {
            files.extend(capture.files.iter().filter_map(|file| {
                let target = file.target();
                let crate::config_store::ProjectFileLayer::Workspace { workspace_id } =
                    &target.layer
                else {
                    return None;
                };
                let mut row = row(&target.module, &target.path()?);
                row.workspace_id = Some(workspace_id.clone());
                row.error.layer = Some(ConfigFileErrorLayer::Workspace);
                Some(row)
            }));
        }
        files
    }
}

impl RuntimeOwner {
    pub(crate) async fn pending_settings_fingerprints(
        self: &std::sync::Arc<Self>,
    ) -> Result<Option<(u64, SettingsFiles)>> {
        let Some((base, paths, modules, registry_required)) = self
            .on_blocking(|owner| {
                let state = owner.lock()?;
                let (store_state, body) = state.store.loaded_admission_state();
                if state.recovery_exit_hold || store_state != StoreState::Stamped {
                    return Ok(None);
                }
                let keys = body
                    .approved
                    .keys()
                    .map(|key| serde_json::from_str::<InputKey>(key).map_err(GateError::invalid))
                    .collect::<Result<Vec<_>>>()?;
                let modules: BTreeSet<_> = keys
                    .iter()
                    .filter_map(|key| match key {
                        InputKey::Declaration(module) => Some(module.clone()),
                        _ => None,
                    })
                    .collect();
                let paths = keys
                    .iter()
                    .filter_map(|key| match key {
                        InputKey::Layer { module, path } if modules.contains(module) => {
                            Some(PathBuf::from(path))
                        }
                        _ => None,
                    })
                    .collect::<BTreeSet<_>>();
                Ok(Some((
                    body.base_revision,
                    paths,
                    modules,
                    capture_plan::known_projects(body),
                )))
            })
            .await?
        else {
            return Ok(None);
        };
        let mut files = self
            .on_blocking(move |_| Ok(fingerprint_paths(paths)))
            .await?;
        if self.project_capture_enabled && !modules.is_empty() {
            match self.capturer.projects.enumerate_live().await {
                Ok(reply) => {
                    let roots = reply
                        .projects
                        .iter()
                        .flat_map(|project| &project.roots)
                        .collect::<BTreeSet<_>>();
                    let mut work = BTreeMap::new();
                    for workspace in reply.workspaces {
                        if let Some(root) = workspace.root {
                            // A root used as both layers is a project-layer
                            // input: like project files, it is only marked as
                            // having unapplied edits in status, with no automatic card.
                            if roots.contains(&root) {
                                continue;
                            }
                            let paths = modules
                                .iter()
                                .map(|module| {
                                    crate::config_gate::project_registry::layer_path(
                                        Path::new(&root),
                                        module,
                                    )
                                })
                                .collect::<Vec<_>>();
                            work.insert(PathBuf::from(root), paths);
                        }
                    }
                    if let Some(pool) = self.capturer.projects.root_io() {
                        let mut work = work.into_iter();
                        loop {
                            let batch: Vec<_> = work
                                .by_ref()
                                .take(pool.batch_slots())
                                .map(|(root, paths)| {
                                    pool.submit(root, move || fingerprint_paths(paths))
                                })
                                .collect();
                            if batch.is_empty() {
                                break;
                            }
                            for job in batch {
                                match job {
                                    Ok(job) => match job.wait().await {
                                        Ok(current) => files.merge(current),
                                        Err(_) => files.mark_incomplete(),
                                    },
                                    Err(_) => files.mark_incomplete(),
                                }
                            }
                        }
                    } else {
                        files.merge(
                            self.on_blocking(move |_| {
                                Ok(fingerprint_paths(work.into_values().flatten()))
                            })
                            .await?,
                        );
                    }
                }
                Err(_) if registry_required => files.mark_incomplete(),
                Err(_) => {}
            }
        }
        Ok(Some((base, files)))
    }

    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn mark_settings_card_open(&self, digest: &Digest, attempt: u64) -> Result<()> {
        let mut state = self.lock()?;
        state.check_settings_approval(digest, true)?;
        self.set_settings_approval(&mut state, digest, attempt, true, true);
        Ok(())
    }

    pub(super) fn reserve_settings_approval(
        &self,
        state: &mut OwnerState,
        digest: &Digest,
        challenge: &ApprovalChallenge,
        daemon: bool,
    ) {
        self.set_settings_approval(
            state,
            digest,
            challenge.attempt(),
            matches!(challenge, ApprovalChallenge::Phone { .. }),
            daemon,
        );
    }

    fn set_settings_approval(
        &self,
        state: &mut OwnerState,
        digest: &Digest,
        attempt: u64,
        phone: bool,
        daemon: bool,
    ) {
        if let Some((open, prior, _)) = state.active_settings_approval() {
            debug_assert!(!daemon);
            let withdrawn = open.clone();
            state.cancel_approval(prior);
            if state
                .pending_settings_card
                .as_ref()
                .is_some_and(|(_, id)| *id == prior)
            {
                state.pending_settings_card = None;
                if !state.gate.plans.get(&withdrawn).is_some_and(|plan| {
                    plan.kept_after_refused_decline || plan.decline_accepted || plan.restoring
                }) {
                    Self::discard_settings_plan(state, &withdrawn);
                }
            }
        }
        state.open_settings_approval = Some((digest.clone(), attempt, phone));
        if daemon {
            state.pending_settings_card = Some((digest.clone(), attempt));
        }
        self.settings_attempts_changed.notify_waiters();
    }

    pub(crate) fn settings_attempt_active(&self, attempt: u64) -> Result<bool> {
        Ok(self
            .lock()?
            .active_settings_approval()
            .is_some_and(|(_, current, _)| current == attempt))
    }

    pub(crate) async fn wait_settings_attempt_replaced(self: &std::sync::Arc<Self>, attempt: u64) {
        loop {
            let changed = self.settings_attempts_changed.notified();
            if !self
                .on_blocking(move |owner| owner.settings_attempt_active(attempt))
                .await
                .unwrap_or(false)
            {
                return;
            }
            changed.await;
        }
    }

    #[cfg(all(test, unix))]
    pub(crate) fn pending_settings_card(&self) -> Result<Option<Digest>> {
        Ok(self
            .polled_snapshot()?
            .projection
            .status
            .pending_settings_card
            .clone())
    }

    #[cfg(all(test, unix))]
    pub(crate) fn pending_settings_card_reason(&self) -> Result<Option<String>> {
        Ok(self
            .polled_snapshot()?
            .projection
            .status
            .pending_settings_card_reason
            .clone())
    }

    #[cfg(all(test, unix))]
    pub(crate) fn has_automatic_reservation_for_test(&self, digest: &Digest) -> bool {
        self.lock()
            .unwrap()
            .pending_settings_card
            .as_ref()
            .is_some_and(|(open, _)| open == digest)
    }

    pub(crate) fn set_pending_settings_card_reason(
        &self,
        review_base: u64,
        reason: Option<&str>,
    ) -> Result<bool> {
        let mut state = self.lock()?;
        let (store_state, body) = state.store.loaded_admission_state();
        if store_state == StoreState::Stamped && body.base_revision == review_base {
            state.pending_settings_card_reason = reason.map(str::to_owned);
            return Ok(true);
        }
        Ok(false)
    }

    pub(crate) fn claim_automatic_settings_plan(&self, digest: &Digest, token: u64) -> Result<()> {
        self.lock()?.claim_automatic_settings_plan(digest, token)
    }

    pub(crate) fn withdraw_settings_card(
        &self,
        digest: &Digest,
        token: u64,
        retain_plan: bool,
    ) -> Result<()> {
        let mut state = self.lock()?;
        if !state
            .gate
            .plans
            .get(digest)
            .is_some_and(|plan| plan.settings_attempt_owner == Some(token))
        {
            return Ok(());
        }
        if let Some((_, attempt)) = state
            .pending_settings_card
            .take_if(|(open, _)| open == digest)
        {
            // Cancellation can retire the attempt before the worker
            // reaches cleanup. The recorded reservation still proves ownership
            // unless a newer Apply has replaced it.
            if !state
                .open_settings_approval
                .as_ref()
                .is_some_and(|(open, id, _)| open == digest && *id == attempt)
            {
                return Ok(());
            }
            state.cancel_approval(attempt);
            state.open_settings_approval = None;
        } else if state
            .open_settings_approval
            .as_ref()
            .is_some_and(|(open, _, _)| open == digest)
        {
            return Ok(());
        }
        if !retain_plan
            && state.gate.plans.get(digest).is_some_and(|plan| {
                plan.settings_users.is_some()
                    && !(plan.kept_after_refused_decline || plan.decline_accepted || plan.restoring)
            })
        {
            Self::discard_settings_plan(&mut state, digest);
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn keep_declined_files(state: &mut OwnerState, digest: &Digest) -> Result<()> {
        let paths = state
            .gate
            .plans
            .get(digest)
            .map(plan_file_paths)
            .ok_or_else(|| GateError::new("unknown_plan", "the plan is no longer pending"))?;
        Self::keep_plan_paths(state, digest, paths)
    }

    fn keep_plan_paths(
        state: &mut OwnerState,
        digest: &Digest,
        paths: BTreeSet<PathBuf>,
    ) -> Result<()> {
        let base = state.store.loaded_admission_state().1.base_revision;
        let stale: BTreeSet<_> = state
            .gate
            .kept_paths
            .values()
            .filter(|digest| {
                state
                    .gate
                    .plans
                    .get(*digest)
                    .is_none_or(|plan| plan.view.base_revision != base)
            })
            .cloned()
            .collect();
        for old in stale {
            Self::discard_settings_plan(state, &old);
        }
        state
            .gate
            .plans
            .get(digest)
            .filter(|plan| plan.view.base_revision == base)
            .ok_or_else(|| GateError::new("unknown_plan", "the plan is no longer pending"))?;
        let additional = paths
            .iter()
            .filter(|path| !state.gate.kept_paths.contains_key(*path))
            .count();
        if state.gate.kept_paths.len() + additional > 64 {
            return Err(GateError::new(
                "settings_keep_limit",
                "the restore failed and the kept settings file limit is reached",
            ));
        }
        let mut replaced = BTreeSet::new();
        for path in paths {
            if let Some(old) = state.gate.kept_paths.insert(path, digest.clone()) {
                if old != *digest {
                    replaced.insert(old);
                }
            }
        }
        state
            .gate
            .plans
            .get_mut(digest)
            .expect("checked pending plan")
            .kept_after_refused_decline = true;
        for old in replaced {
            if !state.gate.kept_paths.values().any(|owner| owner == &old) {
                Self::discard_settings_plan(state, &old);
            }
        }
        Ok(())
    }

    pub(super) fn discard_settings_plan(state: &mut OwnerState, digest: &Digest) {
        state.gate.expire_plan(digest);
        state.plan_deadlines.remove(digest);
        state.retained_plan_files.remove(digest);
        state.desktop_views.retain(|_, (shown, _)| shown != digest);
    }

    pub(crate) fn accept_settings_decline(
        &self,
        attempt: u64,
        facts: &ApprovalFacts,
        reply: &ApprovalReply,
    ) -> Result<Option<Digest>> {
        let mut state = self.lock()?;
        let Some((digest, open)) = state.pending_settings_card.as_ref() else {
            return Ok(None);
        };
        if *open != attempt || !state.gate.settings_declined(attempt, facts, reply) {
            return Ok(None);
        }
        let digest = digest.clone();
        let plan = state
            .gate
            .plans
            .get_mut(&digest)
            .ok_or_else(|| GateError::new("unknown_plan", "the plan is no longer pending"))?;
        let token = plan
            .settings_attempt_owner
            .ok_or_else(|| GateError::new("unknown_plan", "the settings attempt has no owner"))?;
        // Commit the decision before trying the restore. Kept-path capacity
        // is charged only if the restore fails and leaves paths to retain.
        plan.decline_accepted = true;
        state.last_settings_decline = Some((digest.clone(), token));
        Ok(Some(digest))
    }

    pub(crate) fn settings_decline_accepted(&self, digest: &Digest, token: u64) -> Result<bool> {
        Ok(self
            .lock()?
            .last_settings_decline
            .as_ref()
            .is_some_and(|(declined, owner)| declined == digest && *owner == token))
    }

    #[cfg(all(test, unix))]
    pub(crate) fn set_after_settings_decline_hook_for_test(
        &self,
        hook: impl FnOnce() + Send + 'static,
    ) {
        *self.after_settings_decline_hook.lock().unwrap() = Some(Box::new(hook));
    }

    #[cfg(test)]
    pub(crate) fn after_settings_decline_for_test(&self) {
        let hook = self.after_settings_decline_hook.lock().unwrap().take();
        if let Some(hook) = hook {
            hook();
        }
    }
}

impl RuntimeOwner {
    pub(super) fn capture_settings_users(
        &self,
        state: &OwnerState,
        selected: Option<&BTreeSet<String>>,
        confirmed: Option<&BTreeMap<PathBuf, subc_config::InputState>>,
    ) -> Result<(CapturedConfiguration, BTreeSet<String>, BTreeSet<PathBuf>)> {
        let (_, body) = state.store.loaded_admission_state();
        let approved = GateSnapshot::from_store(&body.approved)?;
        let delivery = body
            .generations
            .get(&super::placement::last_approved_generation(body))
            .and_then(|entry| entry.delivery.as_ref())
            .ok_or_else(|| {
                GateError::new("config_unavailable", "approved settings are unavailable")
            })?;
        let mut candidate = self
            .capturer
            .capture_approved_settings(&approved, delivery)?;
        let mut modules = BTreeSet::new();
        let mut settled = BTreeSet::new();
        for (module, capture) in &mut candidate.modules {
            if confirmed.is_some_and(|paths| !paths.contains_key(&capture.file.path)) {
                continue;
            }
            if selected.is_some_and(|users| !users.contains(module)) {
                continue;
            }
            let (live, input) =
                match self
                    .capturer
                    .capture_module(module, &capture.declaration, CapturedFile::open)
                {
                    Ok(live) => live,
                    Err(error) if selected.is_none() => {
                        if !error.is_capture_race()
                            && !matches!(
                                error.code.as_str(),
                                "input_unavailable" | "config_unavailable"
                            )
                        {
                            settled.insert(capture.file.path.clone());
                        }
                        continue;
                    }
                    Err(error) => return Err(error),
                };
            let key = InputKey::Layer {
                module: module.clone(),
                path: live.file.path.to_string_lossy().into_owned(),
            };
            if confirmed.is_some_and(|paths| paths.get(&live.file.path) != Some(&live.file.state)) {
                continue;
            }
            if selected.is_some()
                || approved
                    .inputs
                    .get(&key)
                    .is_none_or(|old| input.value != old.value)
            {
                candidate.diagnostic.inputs.insert(key, input);
                *capture = live;
                modules.insert(module.clone());
            } else {
                settled.insert(live.file.path);
            }
        }
        Ok((candidate, modules, settled))
    }

    #[cfg(test)]
    pub(crate) async fn prepare_pending_settings(
        self: &std::sync::Arc<Self>,
    ) -> Result<Option<PreparedPlan>> {
        self.prepare_confirmed_pending_settings(None).await
    }

    #[cfg(test)]
    pub(crate) async fn prepare_confirmed_pending_settings(
        self: &std::sync::Arc<Self>,
        confirmed: Option<BTreeMap<PathBuf, subc_config::InputState>>,
    ) -> Result<Option<PreparedPlan>> {
        Ok(self.prepare_settings_attempt(confirmed).await?.plan)
    }

    pub(crate) async fn prepare_settings_attempt(
        self: &std::sync::Arc<Self>,
        confirmed: Option<BTreeMap<PathBuf, subc_config::InputState>>,
    ) -> Result<SettingsPreparation> {
        let Some((base, scopes)) = self
            .on_blocking(|owner| {
                let mut state = owner.lock()?;
                if state.recovery_exit_hold || state.store.status().state != StoreState::Stamped {
                    return Ok(None);
                }
                Self::check_authority(&mut state, false)?;
                let body = state.store.body()?;
                let approved = GateSnapshot::from_store(&body.approved)?;
                let scopes = approved
                    .inputs
                    .keys()
                    .filter_map(|key| match key {
                        InputKey::Declaration(module) => {
                            Some((module.clone(), RegistryScope::all()))
                        }
                        _ => None,
                    })
                    .collect::<BTreeMap<_, _>>();
                Ok(Some((body.base_revision, scopes)))
            })
            .await?
        else {
            return Ok(SettingsPreparation::default());
        };
        if scopes.is_empty() {
            return Ok(SettingsPreparation::default());
        }
        let batch = if self.project_capture_enabled {
            Some(self.capturer.projects.capture_batch(&scopes).await)
        } else {
            None
        };
        self.on_blocking(move |owner| {
            let mut state = owner.lock()?;
            Self::check_authority(&mut state, false)?;
            let (store_state, body) = state.store.loaded_admission_state();
            if store_state != StoreState::Stamped || body.base_revision != base {
                return Err(GateError::stale());
            }
            let approved = GateSnapshot::from_store(&body.approved)?;
            let (mut candidate, users, mut settled) =
                owner.capture_settings_users(&state, None, confirmed.as_ref())?;
            let revision_batch = match batch {
                Some(Ok((_, revisions))) => revisions,
                Some(Err(error))
                    if error.code != "project_registry_unavailable"
                        || capture_plan::known_projects(body) =>
                {
                    return Err(capture_plan::registry_retry(error));
                }
                _ => BTreeMap::new(),
            };
            let mut projects = CapturePlan::default();
            for (module, revision) in revision_batch {
                let Some(captured) = candidate.modules.get(&module) else {
                    continue;
                };
                let roots =
                    ModuleRoots::settings(revision, body, &approved, captured, confirmed.as_ref());
                settled.extend(roots.settled_settings.iter().cloned());
                if let Some(authority) = roots.review(
                    body,
                    &approved,
                    approved_user(body, &module),
                    captured,
                    &mut projects.witnesses,
                    &mut projects.reviews,
                    &mut projects.files,
                    &mut projects.refusals,
                )? {
                    projects.context.authority.insert(module, authority);
                }
            }
            if users.is_empty() && projects.files.is_empty() {
                return Ok(SettingsPreparation {
                    plan: None,
                    settled,
                });
            }
            let mut touched = users.clone();
            touched.extend(projects.reviews.keys().map(|tuple| tuple.module.clone()));
            touched.extend(
                projects
                    .files
                    .iter()
                    .map(|file| file.target().module.clone()),
            );
            let mut bases = BTreeMap::new();
            let mut base_schemas = BTreeMap::new();
            for module in &users {
                if let Some(view) = approved_user(body, module) {
                    let mut view = view.clone();
                    view.generation = crate::config_gate::module_base_generation(body, module);
                    bases.insert(module.clone(), view);
                }
                base_schemas.insert(module.clone(), candidate.modules[module].schema.clone());
            }
            let active_generations = body
                .versions
                .iter()
                .filter(|(tuple, _)| touched.contains(&tuple.module))
                .map(|(tuple, versions)| (tuple.clone(), versions.active))
                .collect();
            let read_set = candidate.diagnostic.read_set(&touched)?;
            let observed_handles = candidate.take_handles();
            Ok(SettingsPreparation {
                settled,
                plan: Some(PreparedPlan {
                    edits: None,
                    projects: None,
                    project_capture: (!projects.context.authority.is_empty()).then_some(projects),
                    settings_users: Some(users.clone()),
                    candidate,
                    bases,
                    base_schemas,
                    kind: PlanKind::Change,
                    review_modules: users,
                    rereview_modules: BTreeSet::new(),
                    base_revision: base,
                    active_generations,
                    touched_modules: touched,
                    read_set,
                    program_keys: Vec::new(),
                    observed_handles,
                }),
            })
        })
        .await
    }
}

#[derive(Default)]
pub(crate) struct SettingsPreparation {
    pub(crate) plan: Option<PreparedPlan>,
    pub(crate) settled: BTreeSet<PathBuf>,
}
