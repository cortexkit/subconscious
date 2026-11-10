pub use subc_protocol::scope::{
    RouteAuthorizeExpected, RouteAuthorizeOutcome, RouteAuthorizeResponse, RouteAuthorizeScope,
};
use subc_protocol::{scope::ROUTE_AUTHORIZE_OP, session::ModuleControlRequestFromModule};

use crate::{ModuleControlReply, ModuleHandle, Principal, ScopeCallError};

impl ModuleHandle {
    /// Ask whether `principal` may still open `target` using the stored scope
    /// and its expected claim (`route.authorize`). Requires a registered module
    /// connection: the daemon permits only its configured federated owner with
    /// `route-authorize/v1` in that module's registration.
    ///
    /// Judgments about the route, including refusal outcomes such as
    /// `NotAuthorized`, are values in the `Ok` response. Daemon Error frames
    /// instead refuse the call itself and return [`ScopeCallError::Refused`],
    /// preserving their code and message. In particular,
    /// `route_authorize_not_permitted` means the asking module may not use this
    /// operation, not that the route is unauthorized. Transport, timeout,
    /// unsupported operation, and malformed or unexpected replies are also call
    /// errors. Without `route.authorize` in HELLO_ACK's `subc_ops`, nothing is sent.
    ///
    /// This is a point-in-time judgment, not a reservation: a later route open
    /// may still be refused.
    pub async fn route_authorize(
        &self,
        principal: Principal,
        scope: RouteAuthorizeScope,
        target: String,
        expected: RouteAuthorizeExpected,
    ) -> Result<RouteAuthorizeResponse, ScopeCallError> {
        if !self.shared.supports_route_authorize {
            return Err(ScopeCallError::NotSupported {
                op: ROUTE_AUTHORIZE_OP,
            });
        }
        let request = ModuleControlRequestFromModule::RouteAuthorize {
            principal,
            scope,
            target,
            expected,
        };
        match self.scope_call(ROUTE_AUTHORIZE_OP, &request).await? {
            ModuleControlReply::RouteAuthorize(response) => Ok(response),
            other => Err(ScopeCallError::Protocol(format!(
                "unexpected {ROUTE_AUTHORIZE_OP} response: {other:?}"
            ))),
        }
    }
}
