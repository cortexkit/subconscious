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
    /// Until first-party authority is enabled, no spawn approves declarations.
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
        entry.first_party = Some(false);
        entry.origin = Some(CatalogOrigin::Other);
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
