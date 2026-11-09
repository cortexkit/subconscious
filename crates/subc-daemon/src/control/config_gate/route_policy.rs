use super::*;
use crate::config_gate::runtime::{
    ApprovedRoutePolicy, RoutePolicyAuthority, RoutePolicyInstall, RoutePolicyInstallation,
    RoutePolicyReceipt,
};

pub(super) struct ModuleRouteAuthority(
    pub(super) ControlHandler,
    pub(super) std::sync::Weak<crate::config_gate::runtime::RuntimeOwner>,
);

struct AckedPolicyReceipt;
impl RoutePolicyReceipt for AckedPolicyReceipt {
    fn commit(&mut self) {}
    fn restore(&mut self) {}
}

impl RoutePolicyAuthority for ModuleRouteAuthority {
    fn requires_module_ack(&self) -> bool {
        true
    }

    fn install_and_revoke<'a>(&'a self, policy: ApprovedRoutePolicy) -> RoutePolicyInstall<'a> {
        Box::pin(async move {
            if !self
                .1
                .upgrade()
                .ok_or_else(|| GateError::new("config_gated", "configuration authority is absent"))?
                .route_policy_authorized(&policy)?
            {
                return Err(GateError::new(
                    "delivery_superseded",
                    "route policy ACK is no longer current or has expired",
                ));
            }
            // RuntimeOwner calls this only after the target's configuration ACK.
            // The module owns bind policy; the daemon invalidates old decisions.
            let (routes, goodbyes) = self
                .0
                .forwarding
                .rebind_module_policy(policy.module())
                .map_err(|error| GateError::new("route_policy_revoke_failed", error))?;
            send_route_control_pushes(
                &self.0.forwarding,
                routes,
                ClientControlPush::RouteClosed {
                    module_id: policy.module().to_owned(),
                    channels: Vec::new(),
                    reason: RouteCloseReason::Reload,
                    drained: false,
                    abandoned: 0,
                    excluded_subscriptions: 0,
                    terminal: Some(false),
                },
            );
            self.0.emit_route_goodbyes(goodbyes);
            Ok(RoutePolicyInstallation::new(Box::new(AckedPolicyReceipt)))
        })
    }
}
