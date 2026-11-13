use super::*;
use crate::config_gate::{GateSnapshot, InputKey};
use crate::config_store::{CommitKind, StoreBody};

pub(super) fn origin_evidence(
    body: &StoreBody,
    module: &str,
) -> Option<crate::program_origin::ProgramOriginEvidence> {
    if !cfg!(feature = "dev-placement") {
        return None;
    }
    body.generations
        .values()
        .rev()
        .take_while(|entry| {
            matches!(
                entry.kind,
                CommitKind::Development | CommitKind::DeclarationApproval
            )
        })
        .filter(|entry| entry.kind == CommitKind::Development)
        .find_map(|entry| entry.generation.changeset.launch_manifests.get(module))
        .and_then(|manifest| manifest.get("origin_evidence"))
        .and_then(|value| serde_json::from_value(value.clone()).ok())
}

pub(super) fn last_approved_generation(body: &StoreBody) -> u64 {
    body.generations
        .iter()
        .rev()
        .find(|(_, entry)| {
            !matches!(
                entry.kind,
                CommitKind::Development | CommitKind::DeclarationApproval
            )
        })
        .map(|(id, _)| *id)
        .unwrap_or(0)
}

/// Development admission overlays only programs. Configuration delivery keeps
/// using its existing active and pending pointers, even across a daemon restart.
pub(super) fn overlay_programs(body: &StoreBody, module: &str, snapshot: &mut GateSnapshot) {
    for entry in body.generations.values().rev().take_while(|entry| {
        matches!(
            entry.kind,
            CommitKind::Development | CommitKind::DeclarationApproval
        )
    }) {
        if entry.kind != CommitKind::Development {
            continue;
        }
        if !entry
            .generation
            .changeset
            .launch_manifests
            .contains_key(module)
        {
            continue;
        }
        if let Ok(placed) = GateSnapshot::from_store(&entry.configuration) {
            snapshot
                .inputs
                .retain(|key, _| !(key.is_program() && key.module() == Some(module)));
            snapshot.inputs.extend(
                placed
                    .inputs
                    .into_iter()
                    .filter(|(key, _)| key.is_program() && key.module() == Some(module)),
            );
        }
        break;
    }
}

impl RuntimeOwner {
    #[cfg(all(test, unix))]
    pub(crate) fn place_program(
        &self,
        module: &str,
        path: &str,
        sha256: &Digest,
    ) -> Result<Option<u64>> {
        self.place_program_with_manifest(module, path, sha256, None)
    }

    pub(crate) fn place_program_with_manifest(
        &self,
        module: &str,
        path: &str,
        sha256: &Digest,
        manifest: Option<&serde_json::Value>,
    ) -> Result<Option<u64>> {
        let mut state = self.lock()?;
        Self::check_authority(&mut state, false)?;
        if state.store.status().state == StoreState::Unstamped {
            return Ok(None);
        }
        if !cfg!(feature = "dev-placement") {
            return Err(GateError::new(
                "program_refused",
                "daemon was built without dev-placement; use config.plan and config.apply",
            ));
        }
        let body = state.store.body()?;
        let approved = GateSnapshot::from_store(&body.approved)?;
        let mut placed = approved.clone();
        overlay_programs(body, module, &mut placed);
        let approved_entry = body
            .generations
            .get(&last_approved_generation(body))
            .ok_or_else(|| GateError::new("program_refused", "no approved generation"))?;
        let delivery = approved_entry.delivery.clone();
        let command = delivery
            .as_ref()
            .and_then(|delivery| delivery.daemon.as_ref())
            .and_then(|layer| layer.bytes.as_ref().zip(layer.path.as_ref()))
            .and_then(|(bytes, path)| {
                std::str::from_utf8(bytes)
                    .ok()
                    .and_then(|doc| crate::daemon_config::parse_with_activity(doc, path).ok())
            })
            .and_then(|(config, _)| {
                config
                    .modules
                    .into_iter()
                    .find(|entry| entry.module_id == module)
            })
            .ok_or_else(|| {
                GateError::new("program_refused", "module has no approved launch entry")
            })?;
        let entry = placed
            .inputs
            .get(&InputKey::Entry(module.into()))
            .and_then(|input| match &input.value {
                subc_config::RecordedValue::Value { value } => Some(value),
                _ => None,
            })
            .ok_or_else(|| GateError::new("program_refused", "module entry is missing"))?;
        let launch = super::super::program::capture(
            &command.module_spec(),
            &super::super::program::program_files(entry)?,
        )?;
        let requested = std::fs::canonicalize(path)
            .map_err(|error| GateError::new("program_refused", error))?;
        if !launch.inputs.values().any(|input| match &input.value {
            subc_config::RecordedValue::Value { value } => {
                value
                    .get("path")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|captured| std::path::Path::new(captured) == requested)
                    && value.get("raw").and_then(serde_json::Value::as_str) == Some(sha256.as_str())
            }
            _ => false,
        }) {
            return Err(GateError::new(
                "program_refused",
                "placed path and sha256 do not match a covered program file",
            ));
        }
        let before = placed.clone();
        placed
            .inputs
            .retain(|key, _| !(key.is_program() && key.module() == Some(module)));
        placed.inputs.extend(launch.inputs.clone());
        let mut changeset = daemon_review(&before, &placed)?;
        changeset
            .launch_manifests
            .entry(module.into())
            .or_insert_with(|| serde_json::json!({}));
        if let Some(manifest) = manifest.filter(|manifest| manifest["module_id"] == module) {
            subc_protocol::provider_declarations::validate_hello_provider_declarations(
                &serde_json::json!({"manifest": manifest}),
            )
            .map_err(GateError::invalid)?;
            if let Some(declarations) = manifest.get("provider_declarations") {
                subc_config::canonical_json(declarations).map_err(GateError::invalid)?;
            }
            let evidence = crate::program_origin::ProgramOriginEvidence {
                origin: subc_protocol::peer::CallerOrigin::Development,
                module_id: module.into(),
                published_commit: manifest["provenance"]["build_git_sha"]
                    .as_str()
                    .unwrap_or_default()
                    .into(),
                program_digest: super::super::program::approved_base(&placed, module),
                launch: crate::program_origin::LaunchWitness::capture(&command.module_spec())
                    .map_err(GateError::invalid)?,
                hosts_third_party_code: manifest["hosts_third_party_code"].as_bool(),
                declarations: serde_json::from_value(
                    manifest
                        .get("provider_declarations")
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!({})),
                )
                .map_err(GateError::invalid)?,
            };
            changeset
                .launch_manifests
                .get_mut(module)
                .expect("placement launch manifest")["origin_evidence"] =
                serde_json::to_value(evidence).map_err(GateError::invalid)?;
        }
        let observed = self.capture(&mut state)?.diagnostic.to_store()?;
        let vector = placed.read_set(&BTreeSet::from([module.into()]))?;
        let digest =
            subc_config::raw_digest(&serde_json::to_vec(&changeset).map_err(GateError::invalid)?);
        let mut generation = subc_config::Generation::new(
            0,
            subc_config::ApprovalChannel::DevelopmentPlacement,
            digest,
            vector,
            changeset,
        );
        generation.development = true;
        let configuration = placed.to_store()?;
        let entry = crate::config_store::HistoryEntry {
            generation,
            kind: CommitKind::Development,
            approval_id: "development-placement (unapproved)".into(),
            outcome: "development".into(),
            tuples: BTreeSet::new(),
            configuration,
            delivery,
        };
        let approved = approved.to_store()?;
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(u128::from(u64::MAX)) as u64;
        let id = state
            .store
            .commit_generation(entry, approved, observed, now_ms)?;
        // Keep capture handles alive until after publication; spawn still performs
        // its own identity check against the recorded placement.
        drop(launch);
        Ok(Some(id))
    }
}
