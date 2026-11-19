//! User-layer edits use approved documents when stamped and active files otherwise.

use super::super::{capture::CapturedFile, GateSnapshot, InputKey};
use super::*;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use subc_config::{Digest, InputState, Layer, LayerValue, RecordedValue, ValidationContext};
use subc_control::config::{ConfigEdit, ConfigEditAction, ConfigPlanRefusal, ConfigPlanView};
use subc_protocol::config::{ConfigLayerSnapshot, ConfigStatus, ConfigView};

#[derive(Clone)]
pub(crate) struct EditPlan {
    pub baseline: GateSnapshot,
    pub unstamped: bool,
    pub refusals: Vec<ConfigPlanRefusal>,
    pub files: Vec<WriteBack>,
}

#[derive(Clone)]
pub(crate) struct WriteBack {
    pub module: String,
    pub path: std::path::PathBuf,
    pub expected: InputState,
    pub bytes: Vec<u8>,
    pub plan_time_digest: Option<Digest>,
    pub skipped: bool,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct WriteBackIntent {
    pub module_id: String,
    pub path: std::path::PathBuf,
    pub expected: InputState,
    pub plan_time_digest: Option<Digest>,
    pub candidate_digest: Digest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl WriteBack {
    pub(crate) fn intent(&self) -> WriteBackIntent {
        WriteBackIntent {
            module_id: self.module.clone(),
            path: self.path.clone(),
            expected: self.expected.clone(),
            plan_time_digest: self.plan_time_digest.clone(),
            candidate_digest: subc_config::raw_digest(&self.bytes),
            note: self
                .skipped
                .then(|| "the file has edits that aren't approved yet".into()),
        }
    }
}

fn invalid(message: &str) -> GateError {
    GateError::new("invalid_params", message)
}
fn refused(edit: &ConfigEdit, code: &str, message: &str) -> ConfigPlanRefusal {
    ConfigPlanRefusal {
        module_id: edit.module_id.clone(),
        path: edit.path.clone(),
        code: code.into(),
        message: message.into(),
        project_id: None,
        workspace_id: None,
    }
}

pub(super) fn pointer(path: &str) -> Result<Vec<String>> {
    if !path.starts_with('/') {
        return Err(invalid(
            "an edit path must name an object key with a JSON pointer",
        ));
    }
    path[1..]
        .split('/')
        .map(|part| {
            let mut chars = part.chars();
            let mut key = String::new();
            while let Some(c) = chars.next() {
                if c != '~' {
                    key.push(c);
                    continue;
                }
                match chars.next() {
                    Some('0') => key.push('~'),
                    Some('1') => key.push('/'),
                    _ => return Err(invalid("invalid JSON pointer escape")),
                }
            }
            Ok(key)
        })
        .collect()
}

fn patch(document: &mut Value, keys: &[String], action: &ConfigEditAction) -> Result<()> {
    let (key, parents) = keys
        .split_last()
        .ok_or_else(|| invalid("an edit must name a key"))?;
    let mut object = document;
    for parent in parents {
        let map = object
            .as_object_mut()
            .ok_or_else(|| invalid("arrays must be set whole; a parent must be an object"))?;
        if !map.contains_key(parent) {
            if matches!(action, ConfigEditAction::Remove { .. }) {
                return Ok(());
            }
            map.insert(parent.clone(), json!({}));
        }
        object = map.get_mut(parent).expect("parent exists");
    }
    let map = object
        .as_object_mut()
        .ok_or_else(|| invalid("arrays must be set whole; a parent must be an object"))?;
    match action {
        ConfigEditAction::Set { set } => {
            map.insert(key.clone(), set.clone());
        }
        ConfigEditAction::Remove { remove: true } => {
            map.remove(key);
        }
        _ => return Err(invalid("remove must be true")),
    }
    Ok(())
}

fn schema_path<'a>(
    mut schema: &'a Value,
    mut effective: Option<&Value>,
    keys: &[String],
) -> Result<&'a Value> {
    for key in keys {
        if schema.get("type").and_then(Value::as_str) == Some("array") {
            return Err(invalid("arrays must be set whole"));
        }
        if let Some(branches) = schema.get("oneOf").and_then(Value::as_array) {
            schema = branches
                .iter()
                .find(|branch| {
                    branch
                        .get("properties")
                        .and_then(Value::as_object)
                        .is_some_and(|properties| {
                            properties.iter().any(|(name, property)| {
                                property.get("const").is_some_and(|tag| {
                                    effective.and_then(|value| value.get(name)) == Some(tag)
                                })
                            })
                        })
                })
                .ok_or_else(|| {
                    GateError::new("value_invalid", "the path has no selected schema variant")
                })?;
        }
        schema = schema
            .get("properties")
            .and_then(|properties| properties.get(key))
            .or_else(|| {
                schema
                    .get("additionalProperties")
                    .filter(|value| value.is_object())
            })
            .ok_or_else(|| {
                GateError::new(
                    "value_invalid",
                    "the path is not declared by the admitted schema",
                )
            })?;
        effective = effective.and_then(|value| value.get(key));
        if schema
            .get("x-ck-layers")
            .and_then(Value::as_array)
            .is_some_and(|layers| !layers.iter().any(|layer| layer == "user"))
        {
            return Err(GateError::new(
                "layer_forbidden",
                "the field does not allow the user layer",
            ));
        }
    }
    Ok(schema)
}

pub(super) fn loaded_write_back_status(state: &OwnerState) -> Result<BTreeMap<String, String>> {
    let (store, body) = state.store.loaded_admission_state();
    if store != StoreState::Stamped {
        return Ok(BTreeMap::new());
    }
    let entry = body
        .generations
        .get(&super::placement::last_approved_generation(body));
    let Some(entry) = entry else {
        return Ok(BTreeMap::new());
    };
    let Some(intents) = entry
        .generation
        .changeset
        .apply_plan
        .pointer("/edit_plan/write_back")
    else {
        return Ok(BTreeMap::new());
    };
    let intents: Vec<WriteBackIntent> = serde_json::from_value(intents.clone()).map_err(|_| {
        GateError::new(
            "config_unavailable",
            "approved write-back intents are invalid",
        )
    })?;
    let mut behind = BTreeMap::new();
    for intent in intents {
        let disk = CapturedFile::open(&intent.path);
        let raw = disk
            .as_ref()
            .ok()
            .and_then(|file| file.bytes.as_deref())
            .map(subc_config::raw_digest);
        if raw.as_ref() == Some(&intent.candidate_digest) {
            continue;
        }
        let reason = if let Some(note) = intent.note {
            note
        } else if disk.is_ok() && raw == intent.plan_time_digest {
            let bytes = entry
                .delivery
                .as_ref()
                .and_then(|delivery| delivery.modules.get(&intent.module_id))
                .and_then(|view| view.layers.first())
                .and_then(|layer| layer.bytes.clone());
            if let Some(bytes) =
                bytes.filter(|bytes| subc_config::raw_digest(bytes) == intent.candidate_digest)
            {
                let file = WriteBack {
                    module: intent.module_id.clone(),
                    path: intent.path,
                    expected: intent.expected,
                    bytes,
                    plan_time_digest: intent.plan_time_digest,
                    skipped: false,
                };
                if file.write_back().is_ok() {
                    continue;
                }
            }
            "the approved settings could not be safely written back".into()
        } else {
            "the file changed after this was approved".into()
        };
        behind.insert(intent.module_id, reason);
    }
    Ok(behind)
}

impl RuntimeOwner {
    #[cfg(test)]
    pub fn write_back_status(&self) -> Result<BTreeMap<String, String>> {
        let mut state = self.lock()?;
        state.check_authority_for_read()?;
        loaded_write_back_status(&state)
    }

    pub(super) fn edit_baseline(
        &self,
        state: &mut OwnerState,
        modules: Option<&BTreeSet<String>>,
    ) -> Result<CapturedConfiguration> {
        if state.store.status().state == StoreState::Unstamped {
            return match modules {
                Some(modules) => self.capture_modules(state, modules),
                None => self.capture(state),
            };
        }
        let admitted = modules.map(|_| Self::declarations(state)).transpose()?;
        let body = state.store.body()?;
        let target = GateSnapshot::from_store(&body.approved)?;
        let delivery = body
            .generations
            .get(&super::placement::last_approved_generation(body))
            .and_then(|entry| entry.delivery.as_ref())
            .ok_or_else(|| {
                GateError::new(
                    "approved_schema_unavailable",
                    "approved layer documents are unavailable",
                )
            })?;
        if let (Some(modules), Some(admitted)) = (modules, admitted) {
            let mut declarations = delivery.declarations.clone();
            for module in modules {
                if declarations.contains_key(module) {
                    if let Some(declaration) = admitted.get(module) {
                        declarations.insert(module.clone(), declaration.clone());
                    }
                }
            }
            self.capturer
                .capture_retained_declared(&target, delivery, &declarations)
        } else {
            self.capturer.capture_retained(&target, delivery)
        }
    }

    pub(crate) fn prepare_edits(
        &self,
        base_generations: BTreeMap<String, u64>,
        edits: Vec<ConfigEdit>,
    ) -> Result<PreparedPlan> {
        if edits.is_empty() {
            return Err(invalid("edits must not be empty"));
        }
        let mut state = self.lock()?;
        Self::check_authority(&mut state, false)?;
        let unstamped = state.store.status().state == StoreState::Unstamped;
        let touched: BTreeSet<_> = edits.iter().map(|edit| edit.module_id.clone()).collect();
        let approved = self.edit_baseline(&mut state, unstamped.then_some(&touched))?;
        for module in &touched {
            let wanted = base_generations
                .get(module)
                .ok_or_else(|| invalid("every touched module needs a base generation"))?;
            let actual = crate::config_gate::module_base_generation(state.store.body()?, module);
            if *wanted != actual {
                return Err(GateError::new(
                    "config_changed_since_review",
                    "a touched module's approved generation changed",
                ));
            }
        }
        let mut bases = BTreeMap::new();
        let mut base_schemas = BTreeMap::new();
        for module in &touched {
            if let Some(captured) = approved.modules.get(module) {
                let mut view = captured.view.clone();
                view.generation = base_generations[module];
                bases.insert(module.clone(), view);
                base_schemas.insert(module.clone(), captured.schema.clone());
            }
        }
        let mut candidate = if unstamped {
            approved
        } else {
            self.edit_baseline(&mut state, Some(&touched))?
        };
        let baseline = candidate.diagnostic.clone();
        let mut refusals = Vec::new();
        let mut accepted: BTreeMap<String, Vec<ConfigEdit>> = BTreeMap::new();
        for edit in edits {
            let result: Result<()> = (|| {
                if edit.module_id == "subc" {
                    return Err(GateError::new(
                        "value_invalid",
                        "daemon settings cannot be edited in this version",
                    ));
                }
                if matches!(edit.layer.as_str(), "workspace" | "project") {
                    return Err(GateError::new(
                        "layer_not_served",
                        "workspace and project settings are not served yet",
                    ));
                }
                if edit.layer != "user" || edit.workspace_id.is_some() || edit.project_id.is_some()
                {
                    return Err(invalid("a user edit cannot select a workspace or project"));
                }
                let keys = pointer(&edit.path)?;
                let captured = candidate.modules.get_mut(&edit.module_id).ok_or_else(|| {
                    GateError::new(
                        "unknown_module",
                        "the module has no admitted settings schema",
                    )
                })?;
                schema_path(
                    captured.schema.normalized(),
                    Some(&captured.view.effective),
                    &keys,
                )?;
                let mut document = captured.file.parse()?;
                patch(&mut document, &keys, &edit.action)?;
                crate::config_gate::strict_json::check_canonical(&document, "")?;
                let reviewed = captured
                    .schema
                    .review_tuple(
                        &[LayerValue::new(Layer::User, document.clone())],
                        &ValidationContext::new(&edit.module_id, None),
                    )
                    .map_err(|refusal| {
                        GateError::new(
                            super::super::capture::refusal_code(refusal.code),
                            "the edit is refused by the admitted schema",
                        )
                    })?;
                let bytes = edit_text(captured.file.bytes.as_deref(), std::slice::from_ref(&edit))?;
                if captured.file.bytes.as_deref() != Some(bytes.as_slice()) {
                    captured.file.state = InputState::Present {
                        digest: subc_config::raw_digest(&bytes),
                    };
                }
                captured.file.bytes = Some(bytes.clone());
                captured.view = ConfigView::new(
                    0,
                    reviewed.effective,
                    reviewed.sources,
                    vec![ConfigLayerSnapshot::file(
                        &edit.module_id,
                        subc_protocol::config::ConfigLayer::User,
                        captured.file.path.to_string_lossy(),
                        Some(bytes),
                    )],
                    ConfigStatus::Pending,
                );
                let input = candidate
                    .diagnostic
                    .inputs
                    .get_mut(&InputKey::Layer {
                        module: edit.module_id.clone(),
                        path: captured.file.path.to_string_lossy().into_owned(),
                    })
                    .ok_or_else(|| invalid("approved layer input is absent"))?;
                input.state = captured.file.state.clone();
                input.value = RecordedValue::new(
                    captured
                        .schema
                        .record(&document)
                        .map_err(GateError::invalid)?,
                );
                Ok(())
            })();
            match result {
                Ok(()) => accepted
                    .entry(edit.module_id.clone())
                    .or_default()
                    .push(edit),
                Err(error) => refusals.push(refused(&edit, &error.code, &error.detail)),
            }
        }
        let mut files = Vec::new();
        let mut observed_handles = Vec::new();
        for (module, _) in accepted {
            let captured = &candidate.modules[&module];
            let mut disk = CapturedFile::open(&captured.file.path)?;
            // Never sweep an unrelated dirty file value into an approved edit.
            // If it differs from the baseline, retain the approval but leave reconciliation pending.
            let base = bases[&module].layers[0].bytes.as_deref();
            let skipped = disk.bytes.as_deref() != base;
            let bytes = captured.file.bytes.clone().unwrap_or_default();
            files.push(WriteBack {
                module,
                path: disk.path.clone(),
                expected: disk.state.clone(),
                bytes,
                plan_time_digest: disk.bytes.as_deref().map(subc_config::raw_digest),
                skipped,
            });
            observed_handles.append(&mut disk.take_handles());
        }
        let active_generations = if unstamped {
            BTreeMap::new()
        } else {
            state
                .store
                .body()?
                .versions
                .iter()
                .filter(|(tuple, _)| touched.contains(&tuple.module))
                .map(|(tuple, versions)| (tuple.clone(), versions.active))
                .collect()
        };
        let read_set = baseline.read_set(&touched)?;
        let program_keys = baseline
            .inputs
            .keys()
            .filter(|key| {
                key.is_program() && key.module().is_some_and(|module| touched.contains(module))
            })
            .cloned()
            .collect();
        let review_modules = touched
            .iter()
            .filter(|module| candidate.modules.contains_key(*module))
            .cloned()
            .collect();
        Ok(PreparedPlan {
            projects: None,
            project_capture: None,
            candidate,
            bases,
            base_schemas,
            kind: PlanKind::Change,
            review_modules,
            rereview_modules: BTreeSet::new(),
            base_revision: state.store.status().base_revision,
            active_generations,
            touched_modules: touched,
            read_set,
            program_keys,
            observed_handles,
            edits: Some(EditPlan {
                baseline,
                unstamped,
                refusals,
                files,
            }),
        })
    }
}

fn edit_text(bytes: Option<&[u8]>, edits: &[ConfigEdit]) -> Result<Vec<u8>> {
    let mut text = match bytes {
        Some(bytes) => std::str::from_utf8(bytes)
            .map_err(|_| invalid("invalid UTF-8"))?
            .to_owned(),
        None => "{}\n".to_owned(),
    };
    for edit in edits {
        let keys = pointer(&edit.path)?;
        let path: Vec<_> = keys.iter().map(String::as_str).collect();
        match &edit.action {
            ConfigEditAction::Set { set } => subc_jsonc::set_member(&mut text, &path, set),
            ConfigEditAction::Remove { .. } => {
                subc_jsonc::remove_member(&mut text, &path).map(|_| ())
            }
        }
        .map_err(|_| invalid("the layer document cannot be safely edited at this pointer"))?;
    }
    Ok(text.into_bytes())
}

impl WriteBack {
    fn unchanged(&self) -> Result<()> {
        if CapturedFile::open(&self.path)?.state != self.expected {
            return Err(GateError::new(
                "config_changed_since_review",
                "the user file changed since planning",
            ));
        }
        Ok(())
    }

    /// Use the daemon's normal atomic replacement for user-layer files. It
    /// keeps the file's mode, but not explicit ACLs or extended attributes.
    /// Repository layers need retained ancestor handles and an ACL-preserving
    /// replacement before their write-back can be supported.
    pub(super) fn write_back(&self) -> Result<()> {
        use std::io::Write;
        self.unchanged()?;
        if self.skipped {
            return Err(GateError::new(
                "file_has_unapplied_edits",
                "the user file contains unapproved edits",
            ));
        }
        let parent = self
            .path
            .parent()
            .ok_or_else(|| invalid("a user file needs a parent"))?;
        let mut nonce = [0u8; 16];
        getrandom::getrandom(&mut nonce).map_err(|_| {
            GateError::new("config_unavailable", "could not allocate a temporary file")
        })?;
        let name: String = nonce.iter().map(|byte| format!("{byte:02x}")).collect();
        let temporary = parent.join(format!(".config-{name}.tmp"));
        let result = (|| {
            let mut options = std::fs::OpenOptions::new();
            options.create_new(true).write(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options
                .open(&temporary)
                .map_err(|_| invalid("could not create the replacement file"))?;
            if let Ok(metadata) = std::fs::metadata(&self.path) {
                file.set_permissions(metadata.permissions())
                    .map_err(|_| invalid("could not preserve file permissions"))?;
            }
            file.write_all(&self.bytes)
                .and_then(|_| file.sync_all())
                .map_err(|_| invalid("could not flush the replacement file"))?;
            drop(file);
            self.unchanged()?;
            subc_os::durable_replace(&temporary, &self.path)
                .map_err(|_| invalid("could not atomically replace the user file"))
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result
    }
}

impl RuntimeOwner {
    pub(crate) fn plan_get(&self, digest: &Digest) -> Result<ConfigPlanView> {
        let mut state = self.lock()?;
        let base_revision = state.store.status().base_revision;
        let plan = state
            .gate
            .plans
            .get(digest)
            .ok_or_else(|| GateError::new("unknown_plan", "the plan is no longer pending"))?;
        if base_revision != plan.view.base_revision {
            return Err(GateError::new(
                "unknown_plan",
                "the plan has been superseded",
            ));
        }
        Ok(plan.view.clone())
    }

    pub(crate) fn plan_discard(&self, digest: &Digest) -> Result<()> {
        let mut state = self.lock()?;
        let base_revision = state.store.status().base_revision;
        if !state
            .gate
            .plans
            .get(digest)
            .is_some_and(|plan| plan.view.base_revision == base_revision)
        {
            return Err(GateError::new(
                "unknown_plan",
                "the plan is no longer pending",
            ));
        }
        if state.gate.has_open_approval(digest) {
            return Err(GateError::new(
                "plan_in_review",
                "the plan is being approved",
            ));
        }
        state.gate.expire_plan(digest);
        state.plan_deadlines.remove(digest);
        state.retained_plan_files.remove(digest);
        state.desktop_views.retain(|_, (shown, _)| shown != digest);
        Ok(())
    }

    /// Returns generation zero only for unstamped edit plans. No approval or
    /// approved-store write takes place while files remain authoritative.
    pub(crate) fn apply_unstamped_edits(&self, digest: &Digest) -> Result<Option<u64>> {
        let mut state = self.lock()?;
        let Some(plan) = state.gate.plans.get(digest) else {
            return Ok(None);
        };
        if plan.edits.is_none() {
            return Ok(None);
        }
        if !plan.view.refusals.is_empty() {
            return Err(GateError::new(
                "plan_refused",
                "the plan contains refused edits",
            ));
        }
        let Some(edit) = plan.edits.as_ref().filter(|edit| edit.unstamped).cloned() else {
            return Ok(None);
        };
        let base_revision = plan.view.base_revision;
        Self::check_authority(&mut state, false)?;
        let status = state.store.status();
        if status.state != StoreState::Unstamped || status.base_revision != base_revision {
            return Err(GateError::new(
                "config_changed_since_review",
                "configuration authority changed since planning",
            ));
        }
        for file in &edit.files {
            file.unchanged()?;
        }
        for file in &edit.files {
            file.write_back()?;
        }
        state.gate.expire_plan(digest);
        state.plan_deadlines.remove(digest);
        state.retained_plan_files.remove(digest);
        self.notify_delivery_changed();
        Ok(Some(0))
    }
}

impl PreparedPlan {
    pub(crate) fn is_edit_plan(&self) -> bool {
        self.edits.is_some()
    }
    pub(crate) fn add_edit_refusals(&mut self, refusals: Vec<ConfigPlanRefusal>) {
        if let Some(edits) = &mut self.edits {
            edits.refusals.extend(refusals);
        }
    }
}

impl PreparedPlan {
    pub(crate) fn refused_fields(&self, module: &str) -> Result<Vec<subc_config::FieldChange>> {
        let candidate = self.candidate.modules.get(module).ok_or_else(|| {
            GateError::new(
                "unknown_module",
                "the module has no admitted settings schema",
            )
        })?;
        super::super::review::expected_fields(
            module,
            &candidate.schema,
            self.bases.get(module),
            self.base_schemas.get(module),
            &candidate.view,
        )
        .map(|fields| fields.into_values().collect())
    }
}
