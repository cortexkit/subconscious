//! Provider-owned discovery data and coalesced catalog notifications.
//!
//! Catalog reads use registration snapshots and in-memory spawn facts, never
//! the configuration owner's lock or keychain. Watchers await egress only in
//! their own tasks: neither registry nor forwarding locks span those awaits.

use super::*;
use sha2::{Digest, Sha256};
use subc_control::{CatalogOrigin, CatalogWatchRequest, CatalogWatchResponse};
use subc_protocol::provider_declarations::{
    validate_hello_provider_declarations, ProviderDeclarations,
};

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

/// SHA-256 over the JSON Canonicalization Scheme (JCS) encoding of
/// {provider, entry}, then JCS({provider: entry_digest}).
/// Unlike config digests these wire digests have no domain envelope or prefix.
pub(crate) fn declarations_digest(entries: &ProviderDeclarations) -> Result<String, String> {
    let mut digests = serde_json::Map::new();
    for (provider, entry) in entries {
        let value = serde_json::json!({"provider": provider, "entry": entry});
        digests.insert(
            provider.clone(),
            serde_json::Value::String(jcs_digest(&value)?),
        );
    }
    jcs_digest(&serde_json::Value::Object(digests))
}

fn jcs_digest(value: &serde_json::Value) -> Result<String, String> {
    let bytes = subc_config::canonical_json(value).map_err(|err| err.to_string())?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

impl ControlHandler {
    /// Until first-party authority is enabled, no spawn approves declarations.
    /// The spawn authority record is the only future source of approvals;
    /// requested bytes alone never grant approval.
    fn catalog_approved_declarations(
        &self,
        _registration: &crate::registry::ModuleRegistration,
    ) -> ProviderDeclarations {
        ProviderDeclarations::new()
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
        let approved = self.catalog_approved_declarations(registration);
        let mut entry = CatalogEntry::new(&registration.manifest.module_id, vec![], vec![]);
        entry.incarnation = Some(self.supervisor.spawn_snapshot().cursor.daemon_incarnation);
        // The authority integration supplies this registration's spawn record,
        // keyed by its connection: a module-id lookup during a blue/green swap
        // could name the replacement rather than this registration's process.
        // Leave spawn absent until that exact record is available.
        entry.first_party = Some(false);
        entry.origin = Some(CatalogOrigin::Other);
        entry.requested_declarations_digest = Some(declarations_digest(&requested)?);
        entry.approved_declarations_digest = Some(declarations_digest(&approved)?);
        if filtered {
            entry.requested_declarations = Some(requested);
            entry.approved_declarations = Some(approved);
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
