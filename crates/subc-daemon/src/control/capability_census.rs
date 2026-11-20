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
