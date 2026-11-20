//! Settling a module's reply to a relayed `route.bind`, with the capability
//! fence an attested opener's open carries, and refusing a bind that could not
//! be queued to the module before the open's relay deadline.
//!
//! A fenced bind settles while the registry lock is held. A `catalog.update`
//! or HELLO that adds a deny edge changes the registry under that same lock
//! and only then runs the capability census over committed routes, so it
//! either lands before settlement, which then refuses the bind, or after the
//! commit, and its census closes the route.

use super::*;
use crate::forwarding::{CapabilityFence, PendingRelayCompletion};
use crate::registry::RouteRegistration;

/// The fence for an open admitted for `opener`, an attested supervised
/// module, to `target`; `None` when the opener is not one.
pub(super) fn fence(
    opener: &Option<Arc<RouteRegistration>>,
    target: &RouteRegistration,
) -> Option<CapabilityFence> {
    opener.as_ref().map(|opener| CapabilityFence {
        opener_module_id: opener.module_id.clone(),
        opener_connection: opener.connection_id,
        opener_capabilities: opener.capabilities.clone(),
        target_module_id: target.module_id.clone(),
        target_connection: target.connection_id,
    })
}

impl ControlHandler {
    /// The refusal for a route.bind that could not even be put on the target's
    /// egress queue before the open's relay deadline: the same code as an ack
    /// timeout, under its own reason label.
    pub(super) fn unsent_bind_timeout_refusal(
        &self,
        ctx: &RouteCtx,
        caller: &RouteOpenCaller,
        frame: &Frame,
        target_module_id: &str,
    ) -> Result<Frame, RouterError> {
        let budget = self.route_bind_relay_timeout_for(target_module_id);
        warn!(
            module_id = %target_module_id,
            timeout_ms = budget.as_millis() as u64,
            "route.bind relay timed out before the bind could be queued to the module"
        );
        self.route_open_refusal_frame(
            ctx,
            caller,
            frame,
            target_module_id,
            "relay_send_timed_out",
            "module_timeout",
            format!("module_id '{target_module_id}' did not answer route.bind within {budget:?}"),
        )
    }

    pub(super) fn settle_route_bind_relay(
        &self,
        connection_id: ConnectionId,
        corr: u64,
        outcome: RouteBindRelayOutcome,
    ) -> Result<PendingRelayCompletion, ForwardingError> {
        if !matches!(outcome, RouteBindRelayOutcome::Accepted)
            || !self.forwarding.pending_relay_is_fenced(connection_id, corr)
        {
            // Only a fenced bind needs the registry: other acks settle
            // without serializing registry readers behind forwarding.
            return self
                .forwarding
                .complete_pending_relay(connection_id, corr, outcome);
        }
        self.registry
            .with_registrations_locked(|registrations| {
                self.forwarding.complete_pending_relay_fenced(
                    connection_id,
                    corr,
                    outcome,
                    |fence| fence.refusal(registrations),
                )
            })
            .unwrap_or_else(|err| {
                warn!(error = %err, "registry unavailable while settling a route.bind");
                Err(ForwardingError::Poisoned)
            })
    }
}
