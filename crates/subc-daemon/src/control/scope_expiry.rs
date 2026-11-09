use super::*;

/// Scope expiry owns only routing state, never configuration-store authority.
pub(super) struct ScopeExpiryContext {
    scopes: Arc<RwLock<ScopeTable>>,
    forwarding: Arc<ForwardingTable>,
    counters: DaemonCounters,
}

impl ScopeExpiryContext {
    pub(super) fn new(handler: &ControlHandler) -> Self {
        Self {
            scopes: handler.scopes.clone(),
            forwarding: handler.forwarding.clone(),
            counters: handler.counters.clone(),
        }
    }

    pub(super) fn has_deadlines(&self) -> bool {
        self.scopes
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .has_deadlines()
    }

    pub(super) fn sweep(&self) -> Result<(), RouterError> {
        let mut table = self.scopes.write().unwrap_or_else(|p| p.into_inner());
        let swept = table.sweep_expired();
        let drained = self.publish_scope_changes(&swept.tag_changes, &swept.expired)?;
        drop(table);
        self.close_scope_drained_routes(drained);
        Ok(())
    }

    pub(super) fn publish_scope_changes(
        &self,
        changes: &[crate::scopes::ScopeTagChange],
        expired: &[crate::scopes::ScopeExpired],
    ) -> Result<Vec<crate::forwarding::ScopeDrainedRoute>, RouterError> {
        if changes.is_empty() {
            return Ok(Vec::new());
        }
        let drained = self
            .forwarding
            .publish_scope_changes(changes)
            .map_err(RouterError::Forwarding)?;
        let mut counts = HashMap::new();
        for route in &drained {
            *counts
                .entry((
                    route.scope.owner.as_str(),
                    route.scope.scope_ref.as_str(),
                    route.scope.tag.scope_epoch,
                ))
                .or_insert(0usize) += 1;
        }
        for scope in expired {
            let routes_closed = counts
                .get(&(
                    scope.owner.as_str(),
                    scope.scope_ref.as_str(),
                    scope.scope_epoch,
                ))
                .copied()
                .unwrap_or(0);
            warn!(owner = %scope.owner, scope_ref = %scope.scope_ref,
                scope_epoch = scope.scope_epoch, expires_at_ms = scope.expires_at_ms,
                routes_closed, "scope expired");
        }
        Ok(drained)
    }

    /// Send channel GOODBYEs to both endpoints and group client route.closed
    /// pushes by module and reason, preserving delivery-failure escalation.
    pub(super) fn close_scope_drained_routes(
        &self,
        drained: Vec<crate::forwarding::ScopeDrainedRoute>,
    ) {
        if drained.is_empty() {
            return;
        }
        let mut pushes: BTreeMap<(String, String), (RouteCloseReason, Vec<EndpointRoute>)> =
            BTreeMap::new();
        let mut goodbyes = Vec::with_capacity(drained.len() * 2);
        for route in drained {
            warn!(module_id = %route.module_id, reason = ?route.reason,
                client_connection_id = route.client.connection_id.get(), route_channel = route.client.channel,
                "closing route because its scope changed");
            pushes
                .entry((route.module_id.clone(), format!("{:?}", route.reason)))
                .or_insert_with(|| (route.reason, Vec::new()))
                .1
                .push(EndpointRoute {
                    goodbye_target: route.client.clone(),
                    principal: Principal::Unverified,
                    bound_at: Instant::now(),
                    draining: false,
                    drain_reason: None,
                });
            goodbyes.push(route.module);
            goodbyes.push(route.client);
        }
        for ((module_id, _), (reason, routes)) in pushes {
            send_route_control_pushes(
                &self.forwarding,
                routes,
                ClientControlPush::RouteClosed {
                    module_id,
                    channels: Vec::new(),
                    reason,
                    drained: false,
                    abandoned: 0,
                    excluded_subscriptions: 0,
                    terminal: Some(false),
                },
            );
        }
        self.emit_route_goodbyes(goodbyes);
    }

    pub(super) fn emit_route_goodbyes(&self, released_routes: Vec<GoodbyeTarget>) {
        for released in released_routes {
            let frame = match Frame::build_with_version(
                released.negotiated_ver,
                FrameType::Goodbye,
                control_flags(),
                released.channel,
                released.epoch,
                0,
                Vec::new(),
            ) {
                Ok(frame) => frame,
                Err(err) => {
                    warn!(route_channel = released.channel, error = %err, "failed to build route GOODBYE frame");
                    continue;
                }
            };
            if !released.close_on_delivery_failure() {
                crate::forwarding::send_module_route_goodbye(
                    &self.counters,
                    &released.sink,
                    frame,
                    released.module_id.as_deref(),
                    "client route released",
                );
                continue;
            }
            if let Err(err) = released.sink.try_send(frame) {
                warn!(target_connection_id = released.connection_id.get(), route_channel = released.channel,
                    error = %err, "route GOODBYE was not delivered to client; closing target connection");
                if self
                    .forwarding
                    .escalate_client_delivery_failure(
                        released.connection_id,
                        released.channel,
                        released.epoch,
                        CloseReason::new(
                            "route_goodbye_delivery_failed",
                            format!(
                                "failed to enqueue route GOODBYE for channel {}: {err}",
                                released.channel
                            ),
                        ),
                        crate::forwarding::UndeliveredFrame {
                            module_id: released.module_id.as_deref(),
                            sink: &released.sink,
                        },
                    )
                    .unwrap_or(false)
                {
                    self.counters.increment_goodbye_relay_client_failed();
                }
            }
        }
    }
}
