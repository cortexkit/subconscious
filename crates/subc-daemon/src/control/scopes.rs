use super::*;

impl ControlHandler {
    /// Inject the Unix wall-clock millisecond source used by every scope expiry check.
    pub fn with_wall_clock(self, clock: impl Fn() -> u64 + Send + Sync + 'static) -> Self {
        self.scopes
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .set_wall_clock(Arc::new(clock));
        self
    }
}

impl ControlHandler {
    pub(super) fn publish_scope_changes(
        &self,
        changes: &[crate::scopes::ScopeTagChange],
        expired: &[crate::scopes::ScopeExpired],
    ) -> Result<Vec<crate::forwarding::ScopeDrainedRoute>, RouterError> {
        scope_expiry::ScopeExpiryContext::new(self).publish_scope_changes(changes, expired)
    }
}

impl ControlHandler {
    pub(super) fn handle_scope_served_by(
        &self,
        connection_id: ConnectionId,
        frame: Frame,
    ) -> Result<Vec<Frame>, RouterError> {
        let registered = self
            .registry
            .get_module_by_connection(connection_id)
            .map_err(|err| RouterError::backend(0, frame.header.corr, err.to_string()))?;
        let Some(registered) = registered else {
            return Ok(vec![control_error_frame(
                &frame,
                "not_registered",
                "scope.served_by requires an active module registration owned by this connection",
            )?]);
        };
        let request: subc_control::ScopeServedByRequest = match serde_json::from_slice(&frame.body)
        {
            Ok(request) => request,
            Err(err) => {
                return Ok(vec![control_error_frame(
                    &frame,
                    "invalid_control_body",
                    format!("malformed scope.served_by body: {err}"),
                )?])
            }
        };
        let answer = self
            .scopes
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .served_by(
                &registered.manifest.module_id,
                &request.scope_ref,
                request.scope_epoch,
                &request.module_id,
            );
        let reply = subc_control::ScopeServedByReply::new(
            answer,
            self.supervisor.spawn_snapshot().cursor.daemon_incarnation,
        );
        Ok(vec![control_response_body_frame(
            &frame,
            &reply,
            "ScopeServedByReply",
        )?])
    }
}

impl ControlHandler {
    /// Resolve folders before taking the scope-table lock: a slow filesystem
    /// must not hold up admissions for unrelated scoped routes.
    pub(crate) fn prepare_scope_records(scopes: Vec<ScopeRecord>) -> ScopeRecords {
        let mut scopes = ScopeRecords::from(scopes);
        for (index, record) in scopes.records.iter_mut().enumerate() {
            if let Some(refusal) = Self::canonicalize_workdir(record) {
                scopes.refusals.insert(index, refusal);
            }
        }
        scopes
    }

    fn canonicalize_workdir(record: &mut ScopeRecord) -> Option<crate::scopes::RecordRefusal> {
        let workdir = record.workdir.as_mut()?;
        let path = std::path::Path::new(workdir);
        if !path.is_absolute() {
            return Some((
                error_codes::INVALID_WORKDIR,
                "workdir must be absolute".to_string(),
            ));
        }
        match ProjectRootId::from_path_allowing_missing(path) {
            Ok(root) => match root.as_path().to_str() {
                Some(canonical) => {
                    *workdir = canonical.to_string();
                    None
                }
                None => Some((
                    error_codes::INVALID_WORKDIR,
                    "canonical workdir is not UTF-8".to_string(),
                )),
            },
            Err(error) => Some((error_codes::INVALID_WORKDIR, error.to_string())),
        }
    }

    /// `scope.sync`: the owner is the module registered on this connection.
    /// A connection with no registration (every client connection, `direct`
    /// included) is refused `not_registered` before the table is consulted.
    pub(super) fn handle_scope_sync(
        &self,
        connection_id: ConnectionId,
        frame: Frame,
        generation: u64,
        scopes: Vec<ScopeRecord>,
    ) -> Result<Vec<Frame>, RouterError> {
        self.handle_scope_change(connection_id, frame, generation, scopes, None)
    }

    pub(super) fn handle_scope_change(
        &self,
        connection_id: ConnectionId,
        frame: Frame,
        generation: u64,
        scopes: Vec<ScopeRecord>,
        end: Option<Vec<ScopeEnd>>,
    ) -> Result<Vec<Frame>, RouterError> {
        let is_apply = end.is_some();
        let Some(registration) = self
            .registry
            .get_module_by_connection(connection_id)
            .map_err(|err| RouterError::backend(0, frame.header.corr, err.to_string()))?
        else {
            return Ok(vec![control_error_frame(
                &frame,
                "not_registered",
                "scope.sync and scope.apply require an active module registration owned by this connection",
            )?]);
        };
        let owner = registration.manifest.module_id;
        let scopes = Self::prepare_scope_records(scopes);
        let current_nonce = self.supervisor.spawn_launch_nonce_for(&owner);
        let is_current_launch = |connection: ConnectionId| {
            self.connection_is_current_launch(&owner, connection, current_nonce.as_deref())
        };
        // Lock order is the scope table, then the forwarding table: the new
        // tags are published, and the routes the change closes are selected,
        // while the scope table is still write-locked, so no admission can read
        // a record whose tag is not yet published.
        let mut table = self
            .scopes
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let outcome = match end {
            Some(end) => table.apply(
                &owner,
                connection_id,
                is_current_launch,
                generation,
                scopes,
                end,
            ),
            None => table.sync(&owner, connection_id, is_current_launch, generation, scopes),
        };
        let drained = match &outcome {
            Ok(applied) => self.publish_scope_changes(&applied.tag_changes, &applied.expired)?,
            Err(_) => Vec::new(),
        };
        let has_deadlines = outcome.is_ok() && table.has_deadlines();
        drop(table);
        if has_deadlines {
            self.scope_deadline_added.notify_one();
        }
        match outcome {
            Ok(applied) => {
                let counts = ScopeOutcomeCounts::of(&applied.results);
                info!(
                    owner = %owner,
                    op = if is_apply { SCOPE_APPLY_OP } else { SCOPE_SYNC_OP },
                    generation,
                    records = applied.results.len(),
                    created = counts.created,
                    replaced = counts.replaced,
                    updated = counts.updated,
                    unchanged = counts.unchanged,
                    refused = counts.refused,
                    ended = applied.ended.len(),
                    tag_changes = applied.tag_changes.len(),
                    routes_closed = drained.len(),
                    "scope change accepted"
                );
                let mut route_counts = std::collections::HashMap::new();
                for route in &drained {
                    *route_counts
                        .entry((
                            route.scope.owner.as_str(),
                            route.scope.scope_ref.as_str(),
                            route.scope.tag.scope_epoch,
                        ))
                        .or_insert(0usize) += 1;
                }
                for (scope, cause) in &applied.owner_ended {
                    let routes_closed = route_counts
                        .get(&(owner.as_str(), scope.scope_ref.as_str(), scope.scope_epoch))
                        .copied()
                        .unwrap_or(0);
                    info!(owner = %owner, scope_ref = %scope.scope_ref,
                        scope_epoch = scope.scope_epoch, cause = cause.as_str(),
                        routes_closed, "scope ended");
                }
                // An accepted scope change can still refuse individual records, and the
                // owner is the only party that sees the reply. Name them here so
                // an operator can tell a refused session from a missing one
                // without the owner's logs. Capped so a call that refuses
                // thousands cannot flood the log; the count above is complete.
                for refused in applied
                    .results
                    .iter()
                    .filter(|result| result.outcome == ScopeRecordOutcome::Refused)
                    .take(MAX_LOGGED_REFUSED_SCOPE_RECORDS)
                {
                    warn!(
                        owner = %owner,
                        generation,
                        scope_ref = %refused.scope_ref,
                        scope_epoch = refused.scope_epoch,
                        code = refused.code.as_deref().unwrap_or(""),
                        "scope record refused"
                    );
                }
                self.close_scope_drained_routes(drained);
                let response = if is_apply {
                    ModuleControlResponseToModule::ScopeApply {
                        generation,
                        results: applied.results,
                        end_results: applied.end_results,
                        ended: applied.ended,
                    }
                } else {
                    ModuleControlResponseToModule::ScopeSync {
                        generation,
                        results: applied.results,
                        ended: applied.ended,
                    }
                };
                Ok(vec![control_response_body_frame(
                    &frame,
                    &response,
                    "scope change response",
                )?])
            }
            Err(refusal) => {
                info!(
                    owner = %owner,
                    op = if is_apply { SCOPE_APPLY_OP } else { SCOPE_SYNC_OP },
                    generation,
                    code = refusal.code,
                    "scope change refused"
                );
                Ok(vec![control_error_frame(
                    &frame,
                    refusal.code,
                    refusal.message,
                )?])
            }
        }
    }

    /// Notify both endpoints of routes drained by a scope change.
    pub(super) fn close_scope_drained_routes(
        &self,
        drained: Vec<crate::forwarding::ScopeDrainedRoute>,
    ) {
        scope_expiry::ScopeExpiryContext::new(self).close_scope_drained_routes(drained);
    }

    /// Re-authorize a stored source on a principal the module supplies.
    ///
    /// That principal carries no spawn or authority epoch, so this reply never
    /// conveys a caller attestation or a first-party mark, and a provider must
    /// not apply automatic grants on anything it authorizes.
    pub(super) fn handle_route_authorize(
        &self,
        connection_id: ConnectionId,
        frame: Frame,
        principal: Principal,
        scope: subc_protocol::scope::RouteAuthorizeScope,
        target: String,
        expected: subc_protocol::scope::RouteAuthorizeExpected,
    ) -> Result<Vec<Frame>, RouterError> {
        use crate::scopes::admission::{admit_route, AdmissionMode};
        use subc_protocol::scope::{
            RouteAuthorizeOutcome, RouteAuthorizeResponse, ROUTE_AUTHORIZE_CAPABILITY,
        };
        let registry = self
            .registry
            .route_admission_snapshot()
            .map_err(|err| RouterError::backend(0, frame.header.corr, err.to_string()))?;
        let owner_configured = match &scope.owner {
            Principal::Reserved { module_id } => self.supervisor.is_configured(module_id),
            _ => false,
        };
        let selector = ScopeSelector::new(scope.owner, scope.scope_ref, Some(scope.epoch));
        let answer = {
            let scopes = self.scopes.read().unwrap_or_else(|p| p.into_inner());
            let permitted = registry
                .iter()
                .find(|r| r.connection_id == connection_id)
                .is_some_and(|r| {
                    scopes.is_federated_owner(&Principal::Reserved {
                        module_id: r.module_id.clone(),
                    }) && r.capabilities.as_ref().is_some_and(|caps| {
                        caps.provides
                            .iter()
                            .any(|cap| cap == ROUTE_AUTHORIZE_CAPABILITY)
                    })
                });
            if !permitted {
                drop(scopes);
                return Ok(vec![control_error_frame(&frame, error_codes::ROUTE_AUTHORIZE_NOT_PERMITTED,
                    "route.authorize requires the configured federated owner's route-authorize/v1 registration")?]);
            }
            admit_route(
                &scopes,
                &registry,
                &principal,
                &target,
                Some(&selector),
                owner_configured,
                AdmissionMode::Authorize(&expected),
            )
        };
        let response = match answer {
            Ok(admission) => {
                let mut response = RouteAuthorizeResponse::new(
                    RouteAuthorizeOutcome::Authorized,
                    "route admission authorized",
                );
                response.content_version = admission.content_version;
                response
            }
            Err(refusal) => refusal.response(),
        };
        Ok(vec![control_response_body_frame(
            &frame,
            &response,
            "RouteAuthorizeResponse",
        )?])
    }

    /// `scope.describe`: any registered module may read any scope, because a
    /// provider must read the scope a route it serves is stamped with.
    pub(super) fn handle_scope_describe(
        &self,
        connection_id: ConnectionId,
        frame: Frame,
        owner: Principal,
        scope_ref: String,
    ) -> Result<Vec<Frame>, RouterError> {
        let registered = self
            .registry
            .get_module_by_connection(connection_id)
            .map_err(|err| RouterError::backend(0, frame.header.corr, err.to_string()))?;
        let Some(registered) = registered else {
            return Ok(vec![control_error_frame(
                &frame,
                "not_registered",
                "scope.describe requires an active module registration owned by this connection",
            )?]);
        };
        let scopes = self
            .scopes
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !scopes.reader_permitted(&owner, registered.manifest.capabilities.as_ref()) {
            return Ok(vec![control_error_frame(
                &frame,
                error_codes::SCOPE_FEDERATED_UNSUPPORTED,
                "scope.describe of the federated owner requires scope-federated/v1",
            )?]);
        }
        let mut description = scopes.describe(&owner, &scope_ref);
        drop(scopes);
        if owner
            != (Principal::Reserved {
                module_id: registered.manifest.module_id.clone(),
            })
        {
            if let Some(facts) = description
                .stamp
                .as_mut()
                .and_then(|stamp| stamp.attributes.federated.as_mut())
            {
                facts.stamp_required = false;
            }
        }
        // Describe readers also decode strict ScopeAttributes. Attribution is
        // safe to omit unless the requesting module declares it understands it.
        if !registered
            .manifest
            .capabilities
            .as_ref()
            .is_some_and(|caps| {
                caps.provides
                    .iter()
                    .any(|cap| cap == subc_protocol::scope::SCOPE_ON_BEHALF_OF_CAPABILITY)
            })
        {
            if let Some(stamp) = description.stamp.as_mut() {
                stamp.attributes.on_behalf_of_agent = None;
            }
        }
        if !registered
            .manifest
            .capabilities
            .as_ref()
            .is_some_and(|caps| {
                caps.provides
                    .iter()
                    .any(|cap| cap == subc_protocol::scope::FLOW_IDENTITY_SCOPES_CAPABILITY)
            })
        {
            if let Some(stamp) = description.stamp.as_mut() {
                stamp.attributes.flow_version = None;
                stamp.attributes.flow_code_hash = None;
                stamp.attributes.flow_install_id = None;
            }
        }
        let owner_configured = match &owner {
            // Ask whether the owner is configured (`is_configured`), not
            // whether it is on the roster (`get(..).is_some()`): a supervised
            // module's process can register and describe a scope before the
            // supervisor has put it on the roster.
            Principal::Reserved { module_id } => self.supervisor.is_configured(module_id),
            _ => false,
        };
        let response = ModuleControlResponseToModule::ScopeDescribe {
            status: description.status,
            scope_epoch: description.scope_epoch,
            daemon_incarnation: self.supervisor.spawn_snapshot().cursor.daemon_incarnation,
            owner_synced: description.owner_synced,
            owner_configured,
            scope: description.stamp,
        };
        Ok(vec![control_response_body_frame(
            &frame,
            &response,
            "ModuleControlResponseToModule::ScopeDescribe",
        )?])
    }
}

/// Most refused scope records named individually in the log per sync; the
/// `refused` count on the accepted line is always complete.
const MAX_LOGGED_REFUSED_SCOPE_RECORDS: usize = 8;

/// Per-outcome counts of one accepted `scope.sync`, for its log line.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct ScopeOutcomeCounts {
    pub(super) created: usize,
    pub(super) replaced: usize,
    pub(super) updated: usize,
    pub(super) unchanged: usize,
    pub(super) refused: usize,
}

impl ScopeOutcomeCounts {
    pub(super) fn of(results: &[ScopeRecordResult]) -> Self {
        let mut counts = Self::default();
        for result in results {
            let slot = match result.outcome {
                ScopeRecordOutcome::Created => &mut counts.created,
                ScopeRecordOutcome::Replaced => &mut counts.replaced,
                ScopeRecordOutcome::Updated => &mut counts.updated,
                ScopeRecordOutcome::Unchanged => &mut counts.unchanged,
                ScopeRecordOutcome::Refused => &mut counts.refused,
            };
            *slot += 1;
        }
        counts
    }
}
