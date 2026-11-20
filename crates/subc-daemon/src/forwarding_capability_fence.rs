//! The capability fence on a pending route.bind.
//!
//! `route.open` refuses an attested opener whose `must_never_reach` names a
//! capability the target provides. That check reads the registrations as they
//! were at admission, and a bind can stay pending for a while: the target may
//! claim the capability with `catalog.update`, or another registration may
//! take its place, before it acknowledges. The fence keeps what admission
//! decided on with the pending bind, so reservation can refuse a different
//! target registration and settlement can repeat the deny check against the
//! manifests current at that moment.

use subc_protocol::{manifest::CapabilityDeclarations, ErrorBody};

use super::{
    abandoned_route_target, release_reserved_route_locked, ForwardingInner, PendingRelayCompletion,
    PendingRouteBindRelayEntry, RouteBindRelayOutcome,
};
use crate::{registry::RegistrationView, scopes::admission::denied_capability, ConnectionId};

/// The refusal code of the admission-time capability deny check.
const CAPABILITY_FORBIDDEN: &str = "capability_forbidden";

/// What an attested opener's capability deny check was decided on. Present
/// only for an opener that is an attested supervised module, the only kind of
/// opener the deny policy applies to.
#[derive(Debug, Clone)]
pub(crate) struct CapabilityFence {
    pub(crate) opener_module_id: String,
    pub(crate) opener_connection: ConnectionId,
    /// The opener's declarations at admission, used if it has since gone.
    pub(crate) opener_capabilities: Option<CapabilityDeclarations>,
    pub(crate) target_module_id: String,
    /// The target registration admission checked; only it may serve the route.
    pub(crate) target_connection: ConnectionId,
}

impl CapabilityFence {
    /// The refusal for a target registration other than the admitted one.
    pub(crate) fn registration_changed(&self) -> ErrorBody {
        ErrorBody::new(
            CAPABILITY_FORBIDDEN,
            format!(
                "module_id '{}' was replaced by another registration after the open was admitted; re-open the route",
                self.target_module_id
            ),
        )
    }

    /// Why the bind may no longer commit: the registration serving it is not
    /// the admitted one, or the opener, as currently registered, must never
    /// reach a capability the target now provides.
    pub(crate) fn refusal(&self, view: &RegistrationView<'_>) -> Option<ErrorBody> {
        let Some(target) = view
            .by_connection(self.target_connection)
            .filter(|target| target.manifest.module_id == self.target_module_id)
        else {
            return Some(self.registration_changed());
        };
        let target_capabilities = target.manifest.capabilities.as_ref();
        // The opener's own registration and the active one for its id can
        // differ during a swap; either one denying the capability is enough.
        let mut openers = [
            view.by_connection(self.opener_connection),
            view.active(&self.opener_module_id),
        ]
        .into_iter()
        .flatten()
        .map(|opener| opener.manifest.capabilities.as_ref())
        .collect::<Vec<_>>();
        if openers.is_empty() {
            openers.push(self.opener_capabilities.as_ref());
        }
        let capability = openers
            .into_iter()
            .find_map(|opener| denied_capability(opener, target_capabilities))?;
        Some(ErrorBody::new(
            CAPABILITY_FORBIDDEN,
            format!(
                "module_id '{}' must never reach capability '{capability}' provided by '{}'",
                self.opener_module_id, self.target_module_id
            ),
        ))
    }
}

/// Settle an acknowledged bind without publishing it, for a reason found
/// under the forwarding lock: give back the reserved handle pair, answer the
/// waiting `route.open` with `outcome`, and return the module-side channel so
/// the caller sends one channel-scoped GOODBYE for the binding the module just
/// made. Nothing here touches the module connection, which other routes share.
pub(super) fn refuse_acked_bind_locked(
    inner: &mut ForwardingInner,
    pending: PendingRouteBindRelayEntry,
    outcome: RouteBindRelayOutcome,
) -> PendingRelayCompletion {
    release_reserved_route_locked(
        inner,
        pending.reservation.client_key,
        pending.reservation.module_key,
    );
    let abandoned = abandoned_route_target(inner, &pending.reservation);
    let _ = pending.sender.send(outcome);
    PendingRelayCompletion {
        settled: true,
        abandoned,
    }
}

impl super::ForwardingTable {
    /// Whether the bind `connection_id` is acknowledging carries a capability
    /// fence, so its settlement must hold the registry lock. An unknown or
    /// unreadable relay counts as fenced, which only costs that lock.
    pub(crate) fn pending_relay_is_fenced(&self, connection_id: ConnectionId, corr: u64) -> bool {
        let Ok(inner) = self.read_inner() else {
            return true;
        };
        let Some(endpoint) = inner.endpoint_by_connection.get(&connection_id) else {
            return true;
        };
        inner
            .pending_relays
            .get(&(*endpoint, corr))
            .is_none_or(|pending| pending.capability_fence.is_some())
    }
}
