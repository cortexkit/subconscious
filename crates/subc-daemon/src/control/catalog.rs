//! Provider-owned discovery data and coalesced catalog notifications.
//!
//! Catalog reads use registration snapshots and in-memory spawn facts, never
//! the configuration owner's lock or keychain. Watchers await egress only in
//! their own tasks: neither registry nor forwarding locks span those awaits.

use super::*;
pub(crate) use crate::spawn_authority::declarations_digest;
use crate::spawn_authority::{ApprovedDeclarations, AuthorityCapture};
use subc_control::{CatalogOrigin, CatalogWatchRequest, CatalogWatchResponse};
use subc_protocol::provider_declarations::validate_hello_provider_declarations;

#[derive(Default)]
pub(super) struct CatalogCache {
    #[cfg(test)]
    pub(super) builds: u64,
    key: Option<(u64, u64)>,
    full: Vec<u8>,
    filtered: BTreeMap<String, Vec<u8>>,
    empty: Vec<u8>,
}

#[derive(Serialize)]
struct CatalogBody<'a> {
    op: &'static str,
    generation: u64,
    modules: &'a [CatalogEntry],
    subc_ops: &'a [String],
}

impl CatalogCache {
    fn body(&self, module_id: Option<&str>) -> Vec<u8> {
        match module_id {
            None => self.full.clone(),
            Some(module_id) => self.filtered.get(module_id).unwrap_or(&self.empty).clone(),
        }
    }
}

/// The config canonicalizer is the single numeric rule for declarations and
/// their digests. Refuse at HELLO so every accepted entry can be digested later.
pub(super) fn validate_hello_declarations(hello: &serde_json::Value) -> Result<(), String> {
    validate_hello_provider_declarations(hello).map_err(|err| err.to_string())?;
    if let Some(entries) = hello["manifest"]["provider_declarations"].as_object() {
        for (provider, entry) in entries {
            subc_config::canonical_json(entry).map_err(|err| {
                format!("invalid provider_declarations key {provider:?}: a number is outside the exactly representable range or the entry cannot be canonicalized: {err}")
            })?;
        }
    }
    Ok(())
}

impl ControlHandler {
    pub(super) fn cached_catalog_body(
        &self,
        corr: u64,
        module_id: Option<&str>,
    ) -> Result<Vec<u8>, RouterError> {
        let mut cache = self.catalog_cache.lock().unwrap_or_else(|p| p.into_inner());
        let key = (
            self.registry
                .generation()
                .map_err(|err| RouterError::backend(0, corr, format!("registry error: {err}")))?,
            self.catalog_readiness_revision
                .load(std::sync::atomic::Ordering::Acquire),
        );
        if cache.key == Some(key) {
            return Ok(cache.body(module_id));
        }
        let ClientControlResponse::CatalogList {
            generation,
            mut modules,
            subc_ops,
        } = self.catalog_list_snapshot(corr, None, true)?
        else {
            unreachable!("catalog snapshot returns a catalog response")
        };
        let encode = |modules: &[CatalogEntry]| {
            serde_json::to_vec(&CatalogBody {
                op: "catalog.list",
                generation,
                modules,
                subc_ops: &subc_ops,
            })
            .map_err(|err| {
                RouterError::backend(0, corr, format!("failed to encode catalog: {err}"))
            })
        };
        let mut filtered = BTreeMap::new();
        for entry in &mut modules {
            filtered.insert(
                entry.module_id.clone(),
                encode(std::slice::from_ref(entry))?,
            );
            entry.requested_declarations = None;
            entry.approved_declarations = None;
        }
        // Only registered IDs occupy the filtered cache; arbitrary absent filters
        // share the empty reply. No caller-provided value can add cache entries.
        *cache = CatalogCache {
            #[cfg(test)]
            builds: cache.builds + 1,
            key: Some(key),
            full: encode(&modules)?,
            filtered,
            empty: encode(&[])?,
        };
        // A concurrent registry or verdict update invalidates this snapshot on
        // the next request, including an update during the first serialization.
        if generation != key.0 {
            cache.key = None;
        }
        Ok(cache.body(module_id))
    }

    /// The spawn authority record is the only source of approvals;
    /// requested bytes alone never grant approval.
    fn catalog_approved_declarations(
        &self,
        authority: Option<&AuthorityCapture>,
    ) -> ApprovedDeclarations {
        authority
            .map(|capture| capture.approved.clone())
            .unwrap_or_else(ApprovedDeclarations::none_approved)
    }

    pub(super) fn catalog_discovery_entry(
        &self,
        registration: &crate::registry::ModuleRegistration,
        filtered: bool,
    ) -> Result<CatalogEntry, String> {
        let requested = registration
            .manifest
            .provider_declarations
            .clone()
            .unwrap_or_default();
        // Release the forwarding read lock before capturing authority state.
        // Connection identity distinguishes registrations even when their
        // module ids match during a process replacement.
        let authority = self
            .forwarding
            .target_authority_for_connection(registration.connection_id)
            .map_err(|err| err.to_string())?
            .and_then(|record| record.capture());
        let approved = self.catalog_approved_declarations(authority.as_ref());
        let mut entry = CatalogEntry::new(&registration.manifest.module_id, vec![], vec![]);
        entry.incarnation = Some(authority.as_ref().map_or_else(
            || self.supervisor.spawn_snapshot().cursor.daemon_incarnation,
            |capture| capture.incarnation.clone(),
        ));
        entry.spawn = authority.as_ref().map(|capture| capture.key.spawn);
        // Discovery only: providers authorize on the bind's `caller`.
        entry.first_party = Some(
            authority
                .as_ref()
                .is_some_and(|capture| capture.first_party),
        );
        entry.origin = Some(match authority.as_ref().map(|capture| capture.origin) {
            Some(subc_protocol::peer::CallerOrigin::Development) => CatalogOrigin::Development,
            Some(subc_protocol::peer::CallerOrigin::Release) => CatalogOrigin::Release,
            _ => CatalogOrigin::Other,
        });
        entry.requested_declarations_digest = Some(declarations_digest(&requested)?);
        entry.approved_declarations_digest = Some(approved.digest);
        if filtered {
            entry.requested_declarations = Some(requested);
            entry.approved_declarations = Some(approved.entries);
        }
        Ok(entry)
    }

    pub(super) fn handle_catalog_watch(
        &self,
        ctx: &RouteCtx,
        frame: Frame,
    ) -> Result<Vec<Frame>, RouterError> {
        if serde_json::from_slice::<CatalogWatchRequest>(&frame.body).is_err() {
            return Ok(vec![control_error_frame(
                &frame,
                "invalid_control_body",
                "malformed catalog.watch",
            )?]);
        }
        let mut receiver = self
            .registry
            .subscribe_catalog_changes()
            .map_err(|err| RouterError::backend(0, frame.header.corr, err.to_string()))?;
        let initial = *receiver.borrow_and_update();
        let key = (ctx.connection_id, frame.header.corr);
        let mut streams = self
            .catalog_streams
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        streams.retain(|_, task| !task.is_finished());
        if streams.contains_key(&key) {
            return Ok(vec![control_error_frame(
                &frame,
                "duplicate_subscription",
                "catalog.watch correlation id already in use",
            )?]);
        }
        let sink = ctx.egress.clone();
        let task = tokio::spawn(async move {
            let push = |generation| -> Result<Frame, RouterError> {
                let body = serde_json::to_vec(&CatalogWatchResponse::new(generation))
                    .map_err(|err| RouterError::backend(0, frame.header.corr, err.to_string()))?;
                Frame::build_with_version(
                    response_version(&frame),
                    FrameType::StreamData,
                    control_flags(),
                    0,
                    0,
                    frame.header.corr,
                    body,
                )
                .map_err(|err| RouterError::backend(0, frame.header.corr, err.to_string()))
            };
            let Ok(first) = push(initial) else {
                return;
            };
            if sink.send(first).await.is_err() {
                return;
            }
            while receiver.changed().await.is_ok() {
                // Wait for space before reading the value, so a slow watcher
                // gets the latest generation, not a replay of missed advances.
                let Ok(permit) = sink.reserve_owned().await else {
                    return;
                };
                let generation = *receiver.borrow_and_update();
                let Ok(next) = push(generation) else {
                    return;
                };
                if permit.send(next) {
                    return;
                }
            }
        });
        streams.insert(key, task.abort_handle());
        Ok(Vec::new())
    }

    pub(super) fn cancel_catalog_subscription(&self, connection: ConnectionId, corr: u64) -> bool {
        if let Some(task) = self
            .catalog_streams
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&(connection, corr))
        {
            task.abort();
            true
        } else {
            false
        }
    }

    pub(super) fn close_catalog_subscriptions(&self, connection: ConnectionId) {
        self.catalog_streams
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|(owner, _), task| {
                if *owner == connection {
                    task.abort();
                    false
                } else {
                    true
                }
            });
    }
}

impl ControlHandler {
    pub(super) fn handle_catalog_update_role_declarations(
        &self,
        connection_id: ConnectionId,
        frame: Frame,
        role_declarations: serde_json::Value,
    ) -> Result<Vec<Frame>, RouterError> {
        if let Err(err) = validate_role_declarations_value(Some(&role_declarations)) {
            return Ok(vec![control_error_frame(
                &frame,
                "invalid_role_declarations",
                err.to_string(),
            )?]);
        }
        let declarations = serde_json::from_value(role_declarations)
            .map_err(|err| RouterError::backend(0, frame.header.corr, err.to_string()))?;
        if self
            .registry
            .replace_role_declarations_for_connection(connection_id, declarations)
            .map_err(|err| RouterError::backend(0, frame.header.corr, err.to_string()))?
            .is_none()
        {
            return Ok(vec![control_error_frame(
                &frame,
                "not_registered",
                "catalog.update_role_declarations requires an active module registration owned by this connection",
            )?]);
        }
        control_response_body_frame(
            &frame,
            &ModuleControlResponseToModule::CatalogUpdateRoleDeclarations {},
            "ModuleControlResponseToModule::CatalogUpdateRoleDeclarations",
        )
        .map(|frame| vec![frame])
    }

    pub(super) fn handle_catalog_update(
        &self,
        connection_id: ConnectionId,
        frame: Frame,
        provides: Vec<ProviderRole>,
        capabilities: Option<CapabilityDeclarations>,
        ready: Option<bool>,
    ) -> Result<Vec<Frame>, RouterError> {
        self.refresh_capability_requirements();
        let Some(registration) = self
            .registry
            .get_module_by_connection(connection_id)
            .map_err(|err| RouterError::backend(0, frame.header.corr, err.to_string()))?
        else {
            return Ok(vec![control_error_frame(
                &frame,
                "not_registered",
                "catalog.update requires an active module registration owned by this connection",
            )?]);
        };

        if let Some(message) =
            catalog_update_frozen_field_message(&registration.manifest, &provides)
        {
            return Ok(vec![control_error_frame(
                &frame,
                "catalog_update_frozen_field",
                message,
            )?]);
        }

        let mut candidate = registration.manifest.clone();
        candidate.provides = provides.clone();
        candidate.capabilities = capabilities
            .clone()
            .or_else(|| registration.manifest.capabilities.clone());
        if let Some(rich_ui) = &candidate.rich_ui {
            if let Err(err) = rich_ui.validate() {
                return Ok(vec![control_error_frame(
                    &frame,
                    "invalid_manifest",
                    err.to_string(),
                )?]);
            }
        }
        if let Err(err) = candidate.validate_capability_grammar() {
            return Ok(vec![control_error_frame(
                &frame,
                "invalid_capability_grammar",
                err.to_string(),
            )?]);
        }

        // Updates must honor the same reserved owner as initial registration;
        // otherwise an empty HELLO could acquire the claim after admission.
        let mut conflicts = self
            .capability_evaluator
            .reserved_hello_refusals(&candidate.module_id, candidate.capabilities.as_ref());
        if let Some(conflict) = conflicts.first() {
            let message = format!(
                "capability '{}' is reserved for module_id '{}'; claimant '{}' was refused",
                conflict.capability, conflict.claimants[0], candidate.module_id
            );
            for conflict in &mut conflicts {
                conflict.source = DuplicateClaimSource::CatalogUpdate;
            }
            log_duplicate_claim_events(conflicts);
            return Ok(vec![control_error_frame(
                &frame,
                "reserved_capability",
                message,
            )?]);
        }

        let updated = match self.registry.replace_catalog_for_connection(
            connection_id,
            provides,
            capabilities,
            ready,
        ) {
            Err(crate::registry::RegistryError::FederatedCapabilityChanged) => {
                return Ok(vec![control_error_frame(
                    &frame,
                    error_codes::CATALOG_UPDATE_FROZEN_CAPABILITY,
                    "scope-federated/v1 is fixed at HELLO; restart the module to change it",
                )?])
            }
            Err(crate::registry::RegistryError::CallerAttestationCapabilityChanged) => {
                return Ok(vec![control_error_frame(
                    &frame,
                    error_codes::CATALOG_UPDATE_FROZEN_CAPABILITY,
                    "caller-attestation/v1 is fixed at HELLO; restart the module to change it",
                )?])
            }
            result => {
                result.map_err(|err| RouterError::backend(0, frame.header.corr, err.to_string()))?
            }
        };
        if updated.is_none() {
            return Ok(vec![control_error_frame(
                &frame,
                "not_registered",
                "catalog.update requires an active module registration owned by this connection",
            )?]);
        }
        if let Ok((_, registrations)) = self.runtime_capability_snapshot() {
            log_duplicate_claim_events(
                self.capability_evaluator
                    .duplicate_claims(DuplicateClaimSource::CatalogUpdate, &registrations),
            );
        }
        if capability_census_trigger(
            registration.manifest.capabilities.as_ref(),
            updated
                .as_ref()
                .and_then(|entry| entry.manifest.capabilities.as_ref()),
        ) {
            self.enforce_capability_denies();
        }
        self.refresh_capability_requirements();

        let response = ModuleControlResponseToModule::CatalogUpdate {};
        control_response_body_frame(
            &frame,
            &response,
            "ModuleControlResponseToModule::CatalogUpdate",
        )
        .map(|frame| vec![frame])
    }
}

impl ControlHandler {
    pub(super) fn handle_catalog_list(
        &self,
        frame: Frame,
        module_id: Option<String>,
    ) -> Result<Vec<Frame>, RouterError> {
        let body = self.cached_catalog_body(frame.header.corr, module_id.as_deref())?;
        Ok(vec![Frame::build_with_version(
            response_version(&frame),
            FrameType::Response,
            control_flags(),
            0,
            0,
            frame.header.corr,
            body,
        )
        .map_err(RouterError::FrameBuild)?])
    }

    #[cfg(test)]
    pub(super) fn uncached_catalog_list(
        &self,
        corr: u64,
        module_id: Option<&str>,
    ) -> Result<ClientControlResponse, RouterError> {
        self.catalog_list_snapshot(corr, module_id, module_id.is_some())
    }

    fn catalog_list_snapshot(
        &self,
        corr: u64,
        module_id: Option<&str>,
        include_declarations: bool,
    ) -> Result<ClientControlResponse, RouterError> {
        let (generation, modules) = self
            .registry
            .list_modules_with_nonce_verification()
            .map_err(|err| RouterError::backend(0, corr, format!("registry error: {err}")))?;
        let entries = modules
            .into_iter()
            .filter(|(registration, _)| {
                module_id
                    .map(|wanted| registration.manifest.module_id == wanted)
                    .unwrap_or(true)
            })
            .map(|(registration, nonce_verified)| {
                let entry = self
                    .catalog_discovery_entry(&registration, include_declarations)
                    .map_err(|err| RouterError::backend(0, corr, err))?;
                let not_ready = self.not_ready_reason(&registration);
                let mut entry = entry;
                entry.roles = registration.manifest.provides;
                entry.control_ops = registration.control_ops;
                Ok(entry
                    .with_ready(not_ready.is_none())
                    .with_display_name(registration.manifest.display_name)
                    .with_not_ready(not_ready)
                    .with_module_version(Some(registration.manifest.module_version))
                    .with_provenance(registration.manifest.provenance)
                    .with_capabilities(registration.manifest.capabilities)
                    .with_self_signals(registration.manifest.self_signals)
                    .with_events(registration.manifest.events)
                    .with_operation_delivery(registration.manifest.operation_delivery)
                    .with_operation_payload(registration.manifest.operation_payload)
                    .with_flow_operations(registration.manifest.flow_operations)
                    .with_operation_deferral(registration.manifest.operation_deferral)
                    .with_confirm_operations(registration.manifest.confirm_operations)
                    .with_rich_ui(registration.manifest.rich_ui)
                    .with_role_declarations(registration.manifest.role_declarations)
                    .with_nonce_verified(nonce_verified))
            })
            .collect::<Result<Vec<_>, RouterError>>()?;
        Ok(ClientControlResponse::CatalogList {
            generation,
            modules: entries,
            subc_ops: subc_ops(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ProviderRoleKind {
    ToolProvider,
    PipelineStage,
    ManagementSurface,
    InternalService,
}

fn provider_role_kind(role: &ProviderRole) -> ProviderRoleKind {
    match role {
        ProviderRole::ToolProvider { .. } => ProviderRoleKind::ToolProvider,
        ProviderRole::PipelineStage { .. } => ProviderRoleKind::PipelineStage,
        ProviderRole::ManagementSurface { .. } => ProviderRoleKind::ManagementSurface,
        ProviderRole::InternalService { .. } => ProviderRoleKind::InternalService,
    }
}

fn provider_role_kind_set(roles: &[ProviderRole]) -> BTreeSet<ProviderRoleKind> {
    roles.iter().map(provider_role_kind).collect()
}

fn catalog_update_frozen_field_message(
    registered: &ModuleManifest,
    provides: &[ProviderRole],
) -> Option<String> {
    let old_has_provides = !registered.provides.is_empty();
    let new_has_provides = !provides.is_empty();
    if old_has_provides != new_has_provides {
        return Some(format!(
            "catalog.update cannot change module '{}' between supervision-only and routable; routability is fixed at HELLO",
            registered.module_id
        ));
    }

    if provider_role_kind_set(&registered.provides) != provider_role_kind_set(provides) {
        return Some(format!(
            "catalog.update cannot change provider role kinds for module '{}'; role kinds are fixed at HELLO",
            registered.module_id
        ));
    }

    let registered_concurrency = manifest_concurrency(registered);
    let mut candidate = registered.clone();
    candidate.provides = provides.to_vec();
    let candidate_concurrency = manifest_concurrency(&candidate);
    if candidate_concurrency != registered_concurrency {
        return Some(format!(
            "catalog.update cannot change module '{}' concurrency from {:?} to {:?}; concurrency is fixed at HELLO",
            registered.module_id, registered_concurrency, candidate_concurrency
        ));
    }

    // control_ops live beside the manifest in the HELLO body, not inside
    // ModuleManifest, so a provides-only catalog.update cannot change them.
    None
}
