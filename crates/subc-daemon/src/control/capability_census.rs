//! The capability deny census: closes live routes an attested deny edge now
//! forbids. Route binds still pending when an edge is added are rechecked at
//! settlement instead; see `route_bind_settlement`.

use super::*;

impl ControlHandler {
    /// Reconcile only live, attested route bindings after a capability deny edge
    /// or target claim was added. This is deliberately a control-plane census:
    /// the opaque forwarding hot path must not grow a per-frame capability check.
    pub(super) fn enforce_capability_denies(&self) {
        let registrations = match self.registry.list_registered_modules() {
            Ok(snapshot) => snapshot,
            Err(err) => {
                warn!(error = %err, "failed to read registrations for capability deny census");
                return;
            }
        };
        let manifests = registrations
            .into_iter()
            .map(|registration| {
                (
                    registration.manifest.module_id.clone(),
                    registration.manifest,
                )
            })
            .collect::<BTreeMap<_, _>>();
        let census = match self.forwarding.route_census(None) {
            Ok(census) => census,
            Err(err) => {
                warn!(error = %err, "failed to read route census for capability deny enforcement");
                return;
            }
        };

        for (target_module_id, routes) in census {
            let Some(target_manifest) = manifests.get(&target_module_id) else {
                continue;
            };
            let mut closed_routes = Vec::new();
            let mut goodbyes = Vec::new();
            for route in routes {
                let Principal::Reserved {
                    module_id: opening_module_id,
                } = &route.principal
                else {
                    continue;
                };
                let Some(opening_manifest) = manifests.get(opening_module_id) else {
                    continue;
                };
                let Some(capability) = denied_capability(
                    opening_manifest.capabilities.as_ref(),
                    target_manifest.capabilities.as_ref(),
                ) else {
                    continue;
                };

                match self.forwarding.release_client_route(
                    route.goodbye_target.connection_id,
                    route.goodbye_target.channel,
                    route.goodbye_target.epoch,
                ) {
                    Ok(RouteRelease::Removed(module_goodbye)) => {
                        warn!(
                            opening_module_id,
                            target_module_id,
                            capability,
                            "force-closing route because an attested capability deny edge now matches"
                        );
                        goodbyes.push(module_goodbye);
                        goodbyes.push(route.goodbye_target.clone());
                        closed_routes.push(route);
                    }
                    Ok(RouteRelease::Stale | RouteRelease::Absent) => {}
                    Err(err) => warn!(
                        opening_module_id,
                        target_module_id,
                        capability,
                        error = %err,
                        "failed to force-close capability-denied route"
                    ),
                }
            }

            if closed_routes.is_empty() {
                continue;
            }
            send_route_control_pushes(
                &self.forwarding,
                closed_routes,
                ClientControlPush::RouteClosed {
                    module_id: target_module_id,
                    channels: Vec::new(),
                    reason: RouteCloseReason::CapabilityDenied,
                    drained: false,
                    abandoned: 0,
                    excluded_subscriptions: 0,
                    terminal: Some(false),
                },
            );
            // route.closed records the reason; the route GOODBYE ends the
            // client's handle and settles any calls still pending on it.
            self.emit_route_goodbyes(goodbyes);
        }
    }
}

impl ControlHandler {
    /// Wake at each candidate's own deadline so a stalled fresh exec emits its
    /// requirement event without depending on an operator polling a status command.
    pub fn spawn_capability_deadline_loop(self: Arc<Self>) {
        let handler = Arc::downgrade(&self);
        let evaluator = self.capability_evaluator.clone();
        tokio::spawn(async move {
            loop {
                evaluator.wait_for_change_or_deadline().await;
                let Some(handler) = handler.upgrade() else {
                    break;
                };
                handler.refresh_capability_requirements();
            }
        });
    }
}

impl ControlHandler {
    pub(super) fn runtime_capability_snapshot(
        &self,
    ) -> Result<(Vec<RuntimeModule>, Vec<RegisteredModule>), RouterError> {
        let runtime = self
            .supervisor
            .list()
            .into_iter()
            .map(|module| {
                let status = module.status().map_err(|err| {
                    RouterError::backend(0, 0, format!("failed to read capability status: {err}"))
                })?;
                Ok(RuntimeModule {
                    module_id: status.module_id,
                    state: status.state,
                    enabled: status.enabled,
                })
            })
            .collect::<Result<Vec<_>, RouterError>>()?;
        // Include every registration, published or not. The capability
        // evaluator counts a module as providing its capabilities as soon as it
        // has registered, even before `catalog.list` shows it, so a HELLO in
        // progress does not change other modules' required-capability
        // verdicts. See `Registry::list_registered_modules`.
        let registrations = self.registry.list_registered_modules().map_err(|err| {
            RouterError::backend(
                0,
                0,
                format!("failed to list capability registrations: {err}"),
            )
        })?;
        let registrations = registrations
            .into_iter()
            .map(|registration| RegisteredModule {
                module_id: registration.manifest.module_id,
                module_version: registration.manifest.module_version,
                capabilities: registration.manifest.capabilities,
            })
            .collect();
        Ok((runtime, registrations))
    }

    /// The capability side effects of a module becoming the active registration
    /// for its id: cache its manifest (logging how it differs from the cached
    /// one), run the deny census when its declarations call for one, and recompute the
    /// requirement statuses. An ordinary HELLO does this as it registers; a swap
    /// candidate's does not, and the supervisor does it at promotion instead,
    /// through [`crate::supervise::SwapPromotionObserver`].
    pub(super) fn apply_registration_capabilities(
        &self,
        registration: &crate::registry::ModuleRegistration,
    ) {
        let cached_registration = RegisteredModule {
            module_id: registration.manifest.module_id.clone(),
            module_version: registration.manifest.module_version.clone(),
            capabilities: registration.manifest.capabilities.clone(),
        };
        let change = self.capability_evaluator.record_hello(&cached_registration);
        log_manifest_change(&cached_registration.module_id, &change);
        if capability_census_trigger(None, registration.manifest.capabilities.as_ref()) {
            self.enforce_capability_denies();
        }
        self.refresh_capability_requirements();
    }

    /// Point the shared supervisor handle at this handler for swap promotions.
    /// Called wherever a handler is put behind the `Arc` the router serves, so
    /// it can be held weakly.
    pub(crate) fn install_swap_promotion_observer(self: &Arc<Self>) {
        let observer: std::sync::Weak<dyn crate::supervise::SwapPromotionObserver> =
            Arc::downgrade(self) as std::sync::Weak<ControlHandler>;
        self.supervisor.set_swap_promotion_observer(observer);
    }

    pub fn refresh_capability_requirements(&self) {
        match self.runtime_capability_snapshot() {
            Ok((runtime, registrations)) => {
                let events = self
                    .capability_evaluator
                    .evaluate_now(&runtime, &registrations);
                if !events.is_empty() {
                    self.catalog_readiness_revision
                        .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                }
                log_requirement_events(events, &self.daemon_shutdown);
            }
            Err(err) => warn!(error = %err, "failed to recompute capability requirements"),
        }
    }

    /// Why a registered module is not accepting new route binds, or `None` when
    /// it is. This is the module's effective readiness: its declared readiness
    /// first, then every `need: required` capability it declares evaluating to
    /// `provided`. `route.open` and `catalog.list` both read it here so the
    /// catalog never reports a module routable that `route.open` would refuse.
    pub(super) fn not_ready_reason(
        &self,
        registration: &crate::registry::ModuleRegistration,
    ) -> Option<NotReadyReason> {
        if !registration.ready {
            return Some(NotReadyReason {
                reason: NotReadyReason::DECLARED_NOT_READY.to_string(),
                capability: None,
            });
        }
        self.first_unprovided_required_capability(registration)
            .map(|capability| NotReadyReason {
                reason: NotReadyReason::REQUIRED_CAPABILITY_UNPROVIDED.to_string(),
                capability: Some(capability),
            })
    }

    /// The lexicographically first capability this registration declares
    /// `need: required` whose evaluator verdict is not `provided`.
    ///
    /// The verdicts are the capability evaluator's own; nothing here decides
    /// what "provided" means. The evaluator counts a capability provided as
    /// soon as a module claiming it has REGISTERED, not once that module is
    /// ready. That distinction is what keeps two modules that require each
    /// other's capabilities from deadlocking: if "provided" meant "the claimant
    /// is ready", each would wait for the other to become ready first and
    /// neither ever would. Do not tighten it to readiness.
    ///
    /// A required capability with no verdict at all means this registration's
    /// HELLO or catalog.update landed after the last recompute; recompute once
    /// rather than let a missing verdict read as either answer. If it is still
    /// missing (the recompute itself failed) the capability counts as
    /// unprovided: the refusal is retryable, and routing a module whose
    /// required provider is unknown is the outcome this check exists to stop.
    fn first_unprovided_required_capability(
        &self,
        registration: &crate::registry::ModuleRegistration,
    ) -> Option<String> {
        self.first_unprovided_required_capability_for(
            &registration.manifest.module_id,
            registration.manifest.capabilities.as_ref(),
        )
    }

    pub(super) fn first_unprovided_required_capability_for(
        &self,
        module_id: &str,
        capabilities: Option<&subc_protocol::manifest::CapabilityDeclarations>,
    ) -> Option<String> {
        let required = capabilities
            .into_iter()
            .flat_map(|declarations| declarations.requires.iter())
            .filter(|requirement| requirement.need == CapabilityNeed::Required)
            .map(|requirement| requirement.capability.as_str())
            .collect::<BTreeSet<_>>();
        if required.is_empty() {
            return None;
        }
        let verdict = |capability: &str| self.capability_evaluator.verdict(module_id, capability);
        if required
            .iter()
            .any(|capability| verdict(capability).is_none())
        {
            self.refresh_capability_requirements();
        }
        required
            .into_iter()
            .find(|capability| verdict(capability) != Some(CapabilityVerdict::Provided))
            .map(str::to_string)
    }

    pub(super) fn capability_requirement_statuses(&self) -> Vec<CapabilityRequirementStatus> {
        self.capability_evaluator
            .statuses()
            .into_iter()
            .map(RequirementStatus::into_wire)
            .collect()
    }
}

pub(super) fn append_capability_problem_detail(
    detail: Option<String>,
    capability_detail: Option<String>,
) -> Option<String> {
    match (detail, capability_detail) {
        (Some(detail), Some(capability_detail)) => Some(format!("{detail}; {capability_detail}")),
        (Some(detail), None) => Some(detail),
        (None, Some(capability_detail)) => Some(capability_detail),
        (None, None) => None,
    }
}

/// Return whether a catalog change can create a newly violating live route.
/// Removing an attested claim is intentionally excluded: it makes fewer routes
/// forbidden and therefore must leave the existing route census untouched.
pub(super) fn capability_census_trigger(
    old: Option<&CapabilityDeclarations>,
    new: Option<&CapabilityDeclarations>,
) -> bool {
    let old_provides = old
        .map(|capabilities| capabilities.provides.iter().collect::<HashSet<_>>())
        .unwrap_or_default();
    let old_denies = old
        .map(|capabilities| capabilities.must_never_reach.iter().collect::<HashSet<_>>())
        .unwrap_or_default();
    let new = new.cloned().unwrap_or(CapabilityDeclarations {
        provides: Vec::new(),
        requires: Vec::new(),
        must_never_reach: Vec::new(),
    });

    new.provides
        .iter()
        .any(|capability| !old_provides.contains(capability))
        || new
            .must_never_reach
            .iter()
            .any(|capability| !old_denies.contains(capability))
}
