//! Settings reads separate admitted schema metadata from approved delivery values.

use super::*;
use crate::config_gate::{reviewed_build, GateSnapshot, InputKey};
use crate::config_store::{HistoryEntry, StoreBody};
use serde_json::Value;
use subc_config::{Layer, Schema};
use subc_control::config::{
    ConfigControlResponse, ConfigModule, ConfigSettingsLayer, ConfigThrough,
};

struct SettingsSnapshot {
    body: StoreBody,
    declarations: BTreeMap<String, ConfigDeclareRequest>,
    pending: BTreeMap<String, Digest>,
    known: BTreeSet<String>,
}

impl RuntimeOwner {
    pub(crate) fn subscribe_settings(&self) -> tokio::sync::watch::Receiver<u64> {
        self.settings_changed.subscribe()
    }

    fn settings_snapshot(&self, mut known: BTreeSet<String>) -> Result<SettingsSnapshot> {
        let mut state = self.lock()?;
        let body = state.checked_body()?.clone();
        let declarations = Self::declarations(&mut state.state)?;
        let approved = GateSnapshot::from_store(&body.approved)?;
        known.extend(approved.inputs.keys().filter_map(|key| match key {
            InputKey::Entry(module) | InputKey::Declaration(module) => Some(module.clone()),
            _ => None,
        }));
        known.extend(declarations.keys().cloned());
        // The polling projection also knows configured modules without schemas.
        if !body.stamped {
            known.extend(
                self.polled_snapshot()?
                    .projection
                    .status
                    .ungated_modules
                    .iter()
                    .cloned(),
            );
        }
        let pending = known
            .iter()
            .filter_map(|module| {
                state
                    .gate
                    .pending_file_plan(module)
                    .map(|digest| (module.clone(), digest))
            })
            .collect();
        Ok(SettingsSnapshot {
            body,
            declarations,
            pending,
            known,
        })
    }

    pub(crate) fn settings_modules(
        &self,
        known: BTreeSet<String>,
    ) -> Result<ConfigControlResponse> {
        #[cfg(test)]
        if let Some(hook) = self.settings_projection_hook.lock().unwrap().as_mut() {
            hook()?;
        }
        let snapshot = self.settings_snapshot(known)?;
        let approved = GateSnapshot::from_store(&snapshot.body.approved)?;
        let write_back = self.write_back_status()?;
        let observed = if snapshot.body.stamped {
            Some(
                self.capturer
                    .capture_polled_configuration(&snapshot.declarations)?
                    .proposal_snapshot(&approved),
            )
        } else {
            None
        };
        let rereview = if snapshot.body.stamped {
            reviewed_build::pending(&snapshot.body, snapshot.declarations.keys())?
        } else {
            BTreeMap::new()
        };
        let mut modules = Vec::new();
        for module in &snapshot.known {
            let mut row = ConfigModule::default();
            row.module_id = module.clone();
            row.gated = snapshot.declarations.contains_key(module);
            if let Some(declaration) = snapshot.declarations.get(module) {
                let schema = schema(declaration)?;
                row.display_name = schema
                    .normalized()
                    .get("title")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                row.schema_digest = Some(schema.digest().map_err(GateError::invalid)?);
            }
            let entry = approved_entry(&snapshot.body, module);
            row.approved_generation = entry.map(|entry| entry.generation.id);
            let versions = snapshot
                .body
                .versions
                .iter()
                .find(|(tuple, _)| tuple.module == *module && tuple.project.is_none())
                .map(|(_, versions)| versions);
            row.active_generation = versions.and_then(|versions| versions.active);
            if !snapshot.body.stamped && row.gated {
                row.approved_generation = Some(0);
                row.active_generation = Some(0);
            }
            row.pending_plan = snapshot.pending.get(module).cloned();
            let failed = entry.filter(|entry| entry.generation.never_redeliver.contains(module));
            let mut proposal = false;
            let mut formatting = false;
            if let Some(observed) = &observed {
                for key in approved
                    .changed_keys(observed)
                    .into_iter()
                    .filter(|key| key.module() == Some(module.as_str()) && !key.is_program())
                {
                    if approved.inputs.get(key).map(|input| &input.value)
                        == observed.inputs.get(key).map(|input| &input.value)
                    {
                        formatting = true;
                    } else {
                        proposal = true;
                    }
                }
            }
            let behind = write_back.get(module).cloned();
            row.status = settings_status(
                row.gated,
                failed.is_some(),
                rereview.contains_key(module),
                versions.is_some_and(|versions| versions.pending.is_some()),
                behind.is_some(),
                proposal,
                formatting,
            )
            .into();
            row.status_reason = match row.status.as_str() {
                "failed_delivery" => failed
                    .map(|entry| format!("Generation {} failed delivery.", entry.generation.id)),
                "needs_rereview" => {
                    Some("The admitted program awaits configuration review.".into())
                }
                "write_back_behind" => behind,
                _ => None,
            };
            modules.push(row);
        }
        Ok(ConfigControlResponse::Modules {
            modules,
            stamped: snapshot.body.stamped,
        })
    }

    pub(crate) fn settings_schema(
        &self,
        known: BTreeSet<String>,
        module: String,
    ) -> Result<ConfigControlResponse> {
        let snapshot = self.settings_snapshot(known)?;
        let schema = schema(snapshot.declaration(&module)?)?;
        Ok(ConfigControlResponse::Schema {
            module_id: module,
            schema_digest: schema.digest().map_err(GateError::invalid)?,
            schema: schema.normalized().clone(),
        })
    }

    pub(crate) fn settings_show(
        &self,
        known: BTreeSet<String>,
        module: String,
        project_id: Option<String>,
        through: Option<ConfigThrough>,
    ) -> Result<ConfigControlResponse> {
        let snapshot = self.settings_snapshot(known)?;
        let declaration = snapshot.declaration(&module)?;
        if project_id.is_some() {
            // Project tuples are not admitted by the current capture/delivery path.
            return Err(GateError::new(
                "unknown_project",
                "project has no approved configuration tuple",
            ));
        }
        let (generation, view, approved_declaration) = if snapshot.body.stamped {
            let entry = approved_entry(&snapshot.body, &module).ok_or_else(|| {
                GateError::new("config_gated", "module has no approved configuration")
            })?;
            let delivery = entry.delivery.as_ref().ok_or_else(|| {
                GateError::new("config_gated", "approved delivery is unavailable")
            })?;
            let view = delivery.modules.get(&module).cloned().ok_or_else(|| {
                GateError::new("config_gated", "approved module view is unavailable")
            })?;
            let declaration =
                delivery.declarations.get(&module).cloned().ok_or_else(|| {
                    GateError::new("config_gated", "approved schema is unavailable")
                })?;
            (
                crate::config_gate::module_base_generation(&snapshot.body, &module),
                view,
                declaration,
            )
        } else {
            let captured = self.capturer.capture_modules(
                &BTreeMap::from([(module.clone(), declaration.clone())]),
                &BTreeSet::from([module.clone()]),
            )?;
            let captured = captured
                .modules
                .get(&module)
                .ok_or_else(|| GateError::new("config_gated", "module view is unavailable"))?;
            (0, captured.view.clone(), declaration.clone())
        };
        let defaults = schema(&approved_declaration)?.defaults();
        let mut layers = vec![ConfigSettingsLayer::new(Layer::Default, defaults.clone())];
        for layer in &view.layers {
            let layer_name = match layer.layer {
                subc_protocol::config::ConfigLayer::User => Layer::User,
                subc_protocol::config::ConfigLayer::Workspace => Layer::Workspace,
                subc_protocol::config::ConfigLayer::Project => Layer::Project,
                _ => {
                    return Err(GateError::new(
                        "config_gated",
                        "approved layer is unsupported",
                    ))
                }
            };
            if !included(layer_name, through) {
                continue;
            }
            let document = match &layer.bytes {
                Some(bytes) => {
                    let text = std::str::from_utf8(bytes).map_err(GateError::invalid)?;
                    let text = subc_jsonc::jsonc_to_json(text).map_err(GateError::invalid)?;
                    crate::config_gate::strict_json::parse(&text)?
                }
                None => serde_json::json!({}),
            };
            let mut document = ConfigSettingsLayer::new(layer_name, document);
            document.workspace_id = layer.workspace_id.clone();
            document.project_id = layer.project_id.clone();
            layers.push(document);
        }
        layers.sort_by_key(|layer| match layer.layer {
            Layer::Default => 0,
            Layer::User => 1,
            Layer::Workspace => 2,
            Layer::Project => 3,
            _ => 4,
        });
        let (effective, sources) = if through == Some(ConfigThrough::Default) {
            let mut sources = BTreeMap::new();
            default_sources(&defaults, "", &mut sources);
            (defaults, sources)
        } else {
            // Only user layers are admitted today. Keep the stored reviewed result,
            // rather than merging it again under a newer declaration's defaults.
            (view.effective, view.sources)
        };
        Ok(ConfigControlResponse::Show {
            module_id: module.clone(),
            project_id,
            through,
            stamped: snapshot.body.stamped,
            base_generation: generation,
            effective,
            sources,
            layers,
            pending_plan: snapshot.pending.get(&module).cloned(),
        })
    }
}

impl SettingsSnapshot {
    fn declaration(&self, module: &str) -> Result<&ConfigDeclareRequest> {
        if !self.known.contains(module) {
            return Err(GateError::new("unknown_module", "module is unknown"));
        }
        self.declarations.get(module).ok_or_else(|| {
            GateError::new("module_ungated", "module declares no configuration schema")
        })
    }
}

fn schema(declaration: &ConfigDeclareRequest) -> Result<Schema> {
    Schema::new(declaration.schema.clone())
        .map_err(|_| GateError::new("schema_invalid", "admitted schema is invalid"))
}

fn approved_entry<'a>(body: &'a StoreBody, module: &str) -> Option<&'a HistoryEntry> {
    body.generations.values().rev().find(|entry| {
        !matches!(
            entry.kind,
            crate::config_store::CommitKind::Development
                | crate::config_store::CommitKind::DeclarationApproval
        ) && entry
            .tuples
            .iter()
            .any(|tuple| tuple.module == module && tuple.project.is_none())
    })
}

fn included(layer: Layer, through: Option<ConfigThrough>) -> bool {
    match through {
        Some(ConfigThrough::Default) => layer == Layer::Default,
        Some(ConfigThrough::User) => matches!(layer, Layer::Default | Layer::User),
        Some(ConfigThrough::Workspace) => layer != Layer::Project,
        None => true,
    }
}

fn default_sources(value: &Value, path: &str, sources: &mut BTreeMap<String, Layer>) {
    if let Value::Object(object) = value {
        if !object.is_empty() && !subc_config::is_vault_reference(value) {
            for (key, value) in object {
                default_sources(
                    value,
                    &format!("{path}/{}", key.replace('~', "~0").replace('/', "~1")),
                    sources,
                );
            }
            return;
        }
    }
    if !path.is_empty() {
        sources.insert(path.into(), Layer::Default);
    }
}

/// Inability to run approved settings outranks file drift; a real proposal
/// outranks a formatting-only change. Ungated modules never claim adoption.
#[allow(clippy::too_many_arguments)]
fn settings_status(
    gated: bool,
    failed: bool,
    rereview: bool,
    activation: bool,
    write_back: bool,
    proposal: bool,
    formatting: bool,
) -> &'static str {
    if !gated {
        "ungated"
    } else if failed {
        "failed_delivery"
    } else if rereview {
        "needs_rereview"
    } else if activation {
        "pending_activation"
    } else if write_back {
        "write_back_behind"
    } else if proposal {
        "pending_proposal"
    } else if formatting {
        "formatting_only"
    } else {
        "adopted"
    }
}

#[cfg(test)]
#[path = "settings_tests.rs"]
pub(crate) mod tests;
