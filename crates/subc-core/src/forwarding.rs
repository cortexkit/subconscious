use std::{
    collections::{HashMap, HashSet},
    error::Error,
    fmt,
    sync::{Mutex, MutexGuard},
};

pub use subc_protocol::session::{
    AttachAck, AttachRelay, AttachRelayResponse, AttachRequest, ConfigTier, DetachRelay,
};
use subc_protocol::ErrorBody;
use tokio::sync::oneshot;

use crate::{registry::ConnectionId, router::FrameSink};

/// Module connection identity used in forwarding keys.
///
/// The generation is bumped every time a module connection is registered so a future restart cannot
/// accidentally reuse a stale `(connection_id, route_channel)` binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ModuleEndpointId {
    pub connection_id: ConnectionId,
    pub generation: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct ModuleRoute {
    pub endpoint: ModuleEndpointId,
    pub sink: FrameSink,
    pub negotiated_ver: u8,
}

#[derive(Debug, Clone)]
pub(crate) struct ClientRoute {
    pub sink: FrameSink,
}

#[derive(Debug, Clone)]
pub(crate) struct ReleasedRoute {
    pub route_channel: u16,
    pub module_sink: FrameSink,
    pub negotiated_ver: u8,
}

#[derive(Debug)]
pub(crate) struct PendingAttachRelay {
    pub endpoint: ModuleEndpointId,
    pub module_sink: FrameSink,
    pub negotiated_ver: u8,
    pub route_channel: u16,
    pub corr: u64,
    pub receiver: oneshot::Receiver<AttachRelayOutcome>,
}

#[derive(Debug, Clone)]
pub(crate) enum AttachRelayOutcome {
    Accepted,
    Rejected(ErrorBody),
    ModuleGone(String),
}

#[derive(Debug, Clone)]
struct ModuleConnection {
    module_id: String,
    endpoint: ModuleEndpointId,
    sink: FrameSink,
    negotiated_ver: u8,
}

#[derive(Debug, Default)]
struct ForwardingInner {
    active_module: Option<ModuleConnection>,
    next_generation: u64,
    next_route_channel: u16,
    next_relay_corr: u64,
    reserved_routes: HashSet<(ModuleEndpointId, u16)>,
    client_to_module: HashMap<(ConnectionId, u16), ModuleRoute>,
    module_to_client: HashMap<(ModuleEndpointId, u16), ClientRoute>,
    pending_relays: HashMap<(ModuleEndpointId, u64), oneshot::Sender<AttachRelayOutcome>>,
}

/// Dynamic forwarding state shared by the control plane and data-plane router.
#[derive(Debug, Default)]
pub struct ForwardingTable {
    inner: Mutex<ForwardingInner>,
}

impl ForwardingTable {
    pub fn register_module_connection(
        &self,
        connection_id: ConnectionId,
        module_id: String,
        negotiated_ver: u8,
        sink: FrameSink,
    ) -> Result<ModuleEndpointId, ForwardingError> {
        let mut inner = self.lock_inner()?;
        inner.next_generation = inner.next_generation.checked_add(1).unwrap_or(1);
        let endpoint = ModuleEndpointId {
            connection_id,
            generation: inner.next_generation,
        };
        inner.active_module = Some(ModuleConnection {
            module_id,
            endpoint,
            sink,
            negotiated_ver,
        });
        if inner.next_route_channel == 0 {
            inner.next_route_channel = 1;
        }
        if inner.next_relay_corr == 0 {
            inner.next_relay_corr = 1;
        }
        Ok(endpoint)
    }

    pub(crate) fn begin_attach_relay(&self) -> Result<PendingAttachRelay, ForwardingError> {
        let mut inner = self.lock_inner()?;
        let module = inner
            .active_module
            .clone()
            .ok_or(ForwardingError::NoModuleConnection)?;
        let route_channel = inner.allocate_route_channel(module.endpoint)?;
        let corr = inner.allocate_relay_corr(module.endpoint)?;
        let (sender, receiver) = oneshot::channel();
        inner
            .reserved_routes
            .insert((module.endpoint, route_channel));
        inner.pending_relays.insert((module.endpoint, corr), sender);
        Ok(PendingAttachRelay {
            endpoint: module.endpoint,
            module_sink: module.sink,
            negotiated_ver: module.negotiated_ver,
            route_channel,
            corr,
            receiver,
        })
    }

    pub(crate) fn commit_route(
        &self,
        client_connection_id: ConnectionId,
        client_sink: FrameSink,
        endpoint: ModuleEndpointId,
        route_channel: u16,
    ) -> Result<(), ForwardingError> {
        let mut inner = self.lock_inner()?;
        if !matches!(inner.active_module.as_ref(), Some(module) if module.endpoint == endpoint) {
            return Err(ForwardingError::StaleModuleEndpoint);
        }
        if !inner.reserved_routes.remove(&(endpoint, route_channel)) {
            return Err(ForwardingError::UnknownReservation { route_channel });
        }

        let (module_sink, negotiated_ver) = inner
            .active_module
            .as_ref()
            .map(|module| (module.sink.clone(), module.negotiated_ver))
            .ok_or(ForwardingError::NoModuleConnection)?;
        inner.client_to_module.insert(
            (client_connection_id, route_channel),
            ModuleRoute {
                endpoint,
                sink: module_sink,
                negotiated_ver,
            },
        );
        inner
            .module_to_client
            .insert((endpoint, route_channel), ClientRoute { sink: client_sink });
        Ok(())
    }

    pub(crate) fn release_reserved_route(
        &self,
        endpoint: ModuleEndpointId,
        route_channel: u16,
    ) -> Result<(), ForwardingError> {
        self.lock_inner()?
            .reserved_routes
            .remove(&(endpoint, route_channel));
        Ok(())
    }

    pub(crate) fn cancel_pending_relay(
        &self,
        endpoint: ModuleEndpointId,
        corr: u64,
    ) -> Result<(), ForwardingError> {
        self.lock_inner()?.pending_relays.remove(&(endpoint, corr));
        Ok(())
    }

    pub(crate) fn complete_pending_relay(
        &self,
        connection_id: ConnectionId,
        corr: u64,
        outcome: AttachRelayOutcome,
    ) -> Result<bool, ForwardingError> {
        let mut inner = self.lock_inner()?;
        let Some(endpoint) = endpoint_for_connection(&inner, connection_id) else {
            return Ok(false);
        };
        let Some(sender) = inner.pending_relays.remove(&(endpoint, corr)) else {
            return Ok(false);
        };
        let _ = sender.send(outcome);
        Ok(true)
    }

    pub(crate) fn module_endpoint_for_connection(
        &self,
        connection_id: ConnectionId,
    ) -> Result<Option<ModuleEndpointId>, ForwardingError> {
        let inner = self.lock_inner()?;
        Ok(endpoint_for_connection(&inner, connection_id))
    }

    pub(crate) fn client_route(
        &self,
        client_connection_id: ConnectionId,
        route_channel: u16,
    ) -> Result<Option<ModuleRoute>, ForwardingError> {
        Ok(self
            .lock_inner()?
            .client_to_module
            .get(&(client_connection_id, route_channel))
            .cloned())
    }

    pub(crate) fn module_route(
        &self,
        module_connection_id: ConnectionId,
        route_channel: u16,
    ) -> Result<Option<ClientRoute>, ForwardingError> {
        let inner = self.lock_inner()?;
        let Some(endpoint) = endpoint_for_connection(&inner, module_connection_id) else {
            return Ok(None);
        };
        Ok(inner
            .module_to_client
            .get(&(endpoint, route_channel))
            .cloned())
    }

    pub fn active_binding_count(&self) -> Result<usize, ForwardingError> {
        Ok(self.lock_inner()?.client_to_module.len())
    }

    pub fn has_route_channel(&self, route_channel: u16) -> Result<bool, ForwardingError> {
        let inner = self.lock_inner()?;
        Ok(inner
            .client_to_module
            .keys()
            .any(|(_, bound_channel)| *bound_channel == route_channel))
    }

    pub(crate) fn cleanup_connection(
        &self,
        connection_id: ConnectionId,
    ) -> Result<Vec<ReleasedRoute>, ForwardingError> {
        let mut inner = self.lock_inner()?;
        if let Some(module) = inner
            .active_module
            .as_ref()
            .filter(|module| module.endpoint.connection_id == connection_id)
            .cloned()
        {
            inner.active_module = None;
            inner
                .reserved_routes
                .retain(|(endpoint, _)| *endpoint != module.endpoint);
            inner
                .client_to_module
                .retain(|_, route| route.endpoint != module.endpoint);
            inner
                .module_to_client
                .retain(|(endpoint, _), _| *endpoint != module.endpoint);

            let pending_keys: Vec<_> = inner
                .pending_relays
                .keys()
                .filter(|(endpoint, _)| *endpoint == module.endpoint)
                .copied()
                .collect();
            let pending: Vec<_> = pending_keys
                .into_iter()
                .filter_map(|key| inner.pending_relays.remove(&key))
                .collect();
            for sender in pending {
                let _ = sender.send(AttachRelayOutcome::ModuleGone(format!(
                    "module '{}' connection closed during attach relay",
                    module.module_id
                )));
            }
            return Ok(Vec::new());
        }

        let route_channels: Vec<u16> = inner
            .client_to_module
            .keys()
            .filter(|(client_id, _)| *client_id == connection_id)
            .map(|(_, route_channel)| *route_channel)
            .collect();
        let mut released = Vec::with_capacity(route_channels.len());
        for route_channel in route_channels {
            if let Some(route) = inner
                .client_to_module
                .remove(&(connection_id, route_channel))
            {
                inner
                    .module_to_client
                    .remove(&(route.endpoint, route_channel));
                released.push(ReleasedRoute {
                    route_channel,
                    module_sink: route.sink,
                    negotiated_ver: route.negotiated_ver,
                });
            }
        }
        Ok(released)
    }

    fn lock_inner(&self) -> Result<MutexGuard<'_, ForwardingInner>, ForwardingError> {
        self.inner.lock().map_err(|_| ForwardingError::Poisoned)
    }
}

impl ForwardingInner {
    fn allocate_route_channel(
        &mut self,
        endpoint: ModuleEndpointId,
    ) -> Result<u16, ForwardingError> {
        let mut candidate = self.next_route_channel.max(1);
        for _ in 1..=u16::MAX {
            if candidate == 0 {
                candidate = 1;
            }
            if !self.reserved_routes.contains(&(endpoint, candidate))
                && !self.module_to_client.contains_key(&(endpoint, candidate))
            {
                self.next_route_channel = candidate.wrapping_add(1);
                if self.next_route_channel == 0 {
                    self.next_route_channel = 1;
                }
                return Ok(candidate);
            }
            candidate = candidate.wrapping_add(1);
        }
        Err(ForwardingError::RouteChannelExhausted)
    }

    fn allocate_relay_corr(&mut self, endpoint: ModuleEndpointId) -> Result<u64, ForwardingError> {
        let mut candidate = self.next_relay_corr.max(1);
        for _ in 0..u64::MAX {
            if candidate == 0 {
                candidate = 1;
            }
            if !self.pending_relays.contains_key(&(endpoint, candidate)) {
                self.next_relay_corr = candidate.wrapping_add(1);
                if self.next_relay_corr == 0 {
                    self.next_relay_corr = 1;
                }
                return Ok(candidate);
            }
            candidate = candidate.wrapping_add(1);
        }
        Err(ForwardingError::RelayCorrelationExhausted)
    }
}

fn endpoint_for_connection(
    inner: &ForwardingInner,
    connection_id: ConnectionId,
) -> Option<ModuleEndpointId> {
    inner
        .active_module
        .as_ref()
        .filter(|module| module.endpoint.connection_id == connection_id)
        .map(|module| module.endpoint)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForwardingError {
    NoModuleConnection,
    StaleModuleEndpoint,
    UnknownReservation { route_channel: u16 },
    RouteChannelExhausted,
    RelayCorrelationExhausted,
    Poisoned,
}

impl fmt::Display for ForwardingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoModuleConnection => write!(f, "no module connection is registered"),
            Self::StaleModuleEndpoint => write!(f, "module connection generation is stale"),
            Self::UnknownReservation { route_channel } => {
                write!(f, "route channel {route_channel} was not reserved")
            }
            Self::RouteChannelExhausted => write!(f, "no route channels are available"),
            Self::RelayCorrelationExhausted => {
                write!(f, "no attach-relay correlation ids are available")
            }
            Self::Poisoned => write!(f, "forwarding table lock was poisoned"),
        }
    }
}

impl Error for ForwardingError {}
