use super::{
    check_module_connection_role_locked, ConnectionId, ForwardingError, ForwardingInner,
    ForwardingTable, FrameSink, InnerGuard, RwLockWriteGuard,
};
use crate::supervise::resume_hello::PreparedAck;

impl ForwardingTable {
    /// Queue HELLO_ACK after validation and return the locked forwarding state
    /// so callers can publish the endpoint before another sender can see it.
    pub(super) fn register_ack(
        &self,
        sink: &FrameSink,
        connection_id: ConnectionId,
        ack: Option<PreparedAck<'_>>,
        check_slot: impl Fn(&ForwardingInner) -> Result<(), ForwardingError>,
    ) -> Result<InnerGuard<RwLockWriteGuard<'_, ForwardingInner>>, ForwardingError> {
        let check = |inner: &ForwardingInner| {
            if inner.daemon_draining || inner.closing_connections.contains(&connection_id) {
                return Err(ForwardingError::ConnectionClosing { connection_id });
            }
            check_module_connection_role_locked(inner, connection_id)?;
            check_slot(inner)
        };
        let mut inner = self.write_inner()?;
        check(&inner)?;
        let Some(ack) = ack else { return Ok(inner) };
        let unavailable = || ForwardingError::ModuleEgressUnavailable { connection_id };
        if !ack.needs_commit() {
            sink.try_send(ack.frame).map_err(|_| unavailable())?;
            return Ok(inner);
        }
        let permit = sink
            .try_reserve_registration_ack(&ack.frame)
            .ok_or_else(unavailable)?;
        drop(inner);
        // A close during this write may leave a resume-token hash in the roster
        // without sending the token in HELLO_ACK. Nobody can present that token.
        // If the module's older token no longer matches at the next upgrade,
        // the daemon restarts the module instead of adopting its running process.
        let ack = ack
            .persist()
            .map_err(ForwardingError::HelloRegistrationRefused)?;
        inner = self.write_inner()?;
        check(&inner)?;
        if sink.is_closed() || sink.ingress_terminal() || sink.egress_terminal() {
            return Err(unavailable());
        }
        permit.send(
            ack.finish()
                .map_err(ForwardingError::HelloRegistrationRefused)?,
        );
        Ok(inner)
    }
}
