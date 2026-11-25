use super::*;
use crate::router::RouterError;
use tokio::sync::watch;

#[derive(Debug)]
pub(super) struct AcceptingDrain {
    reason: RouteCloseReason,
    phase_ended: watch::Sender<bool>,
    // A module's `module.drain_ready` request closes admission before returning
    // success. The supervisor uses that result to send notices and tear down routes.
    target: Option<ModuleDrainTarget>,
}

pub(crate) enum AcceptingDrainFinish {
    Ready {
        target: ModuleDrainTarget,
        signalled: bool,
    },
    Disconnected,
    DaemonShutdown,
}

#[cfg(unix)]
pub(super) fn wake_accepting_drains_for_shutdown_locked(inner: &ForwardingInner) {
    for drain in inner.accepting_drains.values() {
        drain.phase_ended.send_replace(true);
    }
}

impl ForwardingTable {
    pub(crate) fn begin_accepting_drain(
        &self,
        module_id: &str,
        connection_id: ConnectionId,
        reason: RouteCloseReason,
    ) -> Result<Option<(ModuleDrainTarget, watch::Receiver<bool>)>, ForwardingError> {
        let mut inner = self.write_inner()?;
        let Some(module) = inner.modules_by_id.get(module_id).cloned() else {
            return Ok(None);
        };
        let endpoint = module.endpoint;
        if inner.daemon_draining
            || inner.upgrade_fenced
            || endpoint.connection_id != connection_id
            || inner.draining_endpoints.contains_key(&endpoint)
            || inner.accepting_drains.contains_key(&endpoint)
        {
            return Ok(None);
        }
        let (phase_ended, receiver) = watch::channel(false);
        inner.accepting_drains.insert(
            endpoint,
            AcceptingDrain {
                reason,
                phase_ended,
                target: None,
            },
        );
        Ok(Some((
            ModuleDrainTarget {
                endpoint,
                sink: module.sink,
                negotiated_ver: module.negotiated_ver,
                abandoned_bindings: Vec::new(),
                excluded_subscriptions: 0,
            },
            receiver,
        )))
    }

    /// Enqueue the acknowledgement before waking teardown, which can close the
    /// module's connection as soon as there is no in-flight work.
    pub(crate) fn mark_accepting_drain_ready(
        &self,
        connection_id: ConnectionId,
        ack: Frame,
    ) -> Result<bool, RouterError> {
        let mut inner = self.write_inner().map_err(RouterError::Forwarding)?;
        let Some(&endpoint) = inner.endpoint_by_connection.get(&connection_id) else {
            return Ok(false);
        };
        let Some(drain) = inner.accepting_drains.get(&endpoint) else {
            return Ok(false);
        };
        let reason = drain.reason;
        let module = module_connection_for_endpoint_locked(&inner, endpoint)
            .expect("an accepting drain belongs to a registered endpoint")
            .clone();
        module.sink.try_send(ack)?;
        if !inner.draining_endpoints.contains_key(&endpoint) {
            let module_id = inner.module_id_by_endpoint[&endpoint].clone();
            let target = begin_drain_locked(&mut inner, &module_id, module, reason);
            let drain = inner
                .accepting_drains
                .get_mut(&endpoint)
                .expect("the drain remains registered under the forwarding lock");
            drain.target = Some(target);
            drain.phase_ended.send_replace(true);
        }
        Ok(true)
    }

    /// Reuse the admission closure triggered by the module's readiness request,
    /// or close admission when the budget expires.
    /// A disconnect removes the whole entry, including the wake sender.
    pub(crate) fn finish_accepting_drain(
        &self,
        endpoint: ModuleEndpointId,
    ) -> Result<AcceptingDrainFinish, ForwardingError> {
        let mut inner = self.write_inner()?;
        // Shutdown gates every endpoint under this same lock. After that, its
        // own notice and graceful-exit sequence are the only ones that run.
        if inner.daemon_draining {
            return Ok(AcceptingDrainFinish::DaemonShutdown);
        }
        let Some(drain) = inner.accepting_drains.get_mut(&endpoint) else {
            return Ok(AcceptingDrainFinish::Disconnected);
        };
        if let Some(target) = drain.target.take() {
            return Ok(AcceptingDrainFinish::Ready {
                target,
                signalled: true,
            });
        }
        let reason = drain.reason;
        let module = module_connection_for_endpoint_locked(&inner, endpoint)
            .expect("an accepting drain belongs to a registered endpoint")
            .clone();
        let module_id = inner.module_id_by_endpoint[&endpoint].clone();
        Ok(AcceptingDrainFinish::Ready {
            target: begin_drain_locked(&mut inner, &module_id, module, reason),
            signalled: false,
        })
    }

    pub(crate) fn module_is_accepting_drain(
        &self,
        module_id: &str,
    ) -> Result<bool, ForwardingError> {
        let inner = self.read_inner()?;
        Ok(inner.modules_by_id.get(module_id).is_some_and(|module| {
            inner.accepting_drains.contains_key(&module.endpoint)
                && !inner.draining_endpoints.contains_key(&module.endpoint)
        }))
    }
}
