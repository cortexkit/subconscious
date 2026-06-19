use std::{
    collections::{HashMap, HashSet},
    error::Error,
    fmt,
    sync::{Arc, Mutex, MutexGuard},
};

use subc_protocol::{manifest::Concurrency, ErrorBody};
use tokio::sync::{oneshot, Semaphore};
use tracing::warn;

use crate::{registry::ConnectionId, router::FrameSink};

/// Default per-channel request-credit window for modules that schedule internally.
const DEFAULT_MODULE_MANAGED_WINDOW: usize = 32;

/// High per-channel cap for stateless modules; this is an OOM guard, not scheduling policy.
const STATELESS_PARALLEL_WINDOW: usize = 1024;

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
    pub flow: Arc<ChannelFlow>,
}

#[derive(Debug, Clone)]
pub(crate) struct ClientRoute {
    pub sink: FrameSink,
    pub flow: Arc<ChannelFlow>,
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
    concurrency: Concurrency,
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
    status: HashMap<(ModuleEndpointId, u16), String>,
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
        concurrency: Concurrency,
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
            concurrency,
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

        let (module_sink, negotiated_ver, window) = inner
            .active_module
            .as_ref()
            .map(|module| {
                (
                    module.sink.clone(),
                    module.negotiated_ver,
                    window_for(&module.concurrency),
                )
            })
            .ok_or(ForwardingError::NoModuleConnection)?;
        let flow = Arc::new(ChannelFlow::new(window));
        inner.client_to_module.insert(
            (client_connection_id, route_channel),
            ModuleRoute {
                endpoint,
                sink: module_sink,
                negotiated_ver,
                flow: Arc::clone(&flow),
            },
        );
        inner.module_to_client.insert(
            (endpoint, route_channel),
            ClientRoute {
                sink: client_sink,
                flow,
            },
        );
        Ok(())
    }

    pub(crate) fn release_reserved_route(
        &self,
        endpoint: ModuleEndpointId,
        route_channel: u16,
    ) -> Result<(), ForwardingError> {
        let mut inner = self.lock_inner()?;
        inner.reserved_routes.remove(&(endpoint, route_channel));
        inner.status.remove(&(endpoint, route_channel));
        Ok(())
    }

    pub(crate) fn release_client_route(
        &self,
        client_connection_id: ConnectionId,
        route_channel: u16,
    ) -> Result<Option<ReleasedRoute>, ForwardingError> {
        let mut inner = self.lock_inner()?;
        Ok(release_client_route_locked(
            &mut inner,
            client_connection_id,
            route_channel,
        ))
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

    pub(crate) fn cache_status(
        &self,
        endpoint: ModuleEndpointId,
        route_channel: u16,
        status: String,
    ) -> Result<(), ForwardingError> {
        let mut inner = self.lock_inner()?;
        if !matches!(inner.active_module.as_ref(), Some(module) if module.endpoint == endpoint) {
            return Err(ForwardingError::StaleModuleEndpoint);
        }

        let route_key = (endpoint, route_channel);
        if inner.module_to_client.contains_key(&route_key)
            || inner.reserved_routes.contains(&route_key)
        {
            inner.status.insert(route_key, status);
        } else {
            warn!(
                route_channel,
                generation = endpoint.generation,
                connection_id = endpoint.connection_id.get(),
                "dropping status update for unbound route channel"
            );
        }
        Ok(())
    }

    pub(crate) fn get_status(
        &self,
        endpoint: ModuleEndpointId,
        route_channel: u16,
    ) -> Result<Option<String>, ForwardingError> {
        Ok(self
            .lock_inner()?
            .status
            .get(&(endpoint, route_channel))
            .cloned())
    }

    pub(crate) fn client_route_endpoint(
        &self,
        client_connection_id: ConnectionId,
        route_channel: u16,
    ) -> Result<Option<ModuleEndpointId>, ForwardingError> {
        Ok(self
            .lock_inner()?
            .client_to_module
            .get(&(client_connection_id, route_channel))
            .map(|route| route.endpoint))
    }

    pub(crate) fn client_route_module_id(
        &self,
        client_connection_id: ConnectionId,
        route_channel: u16,
    ) -> Result<Option<String>, ForwardingError> {
        let inner = self.lock_inner()?;
        let Some(route) = inner
            .client_to_module
            .get(&(client_connection_id, route_channel))
        else {
            return Ok(None);
        };
        Ok(inner
            .active_module
            .as_ref()
            .filter(|module| module.endpoint == route.endpoint)
            .map(|module| module.module_id.clone()))
    }

    pub(crate) fn client_route_is_bound_to_active_module(
        &self,
        client_connection_id: ConnectionId,
        route_channel: u16,
    ) -> Result<bool, ForwardingError> {
        let inner = self.lock_inner()?;
        let Some(route) = inner
            .client_to_module
            .get(&(client_connection_id, route_channel))
        else {
            return Ok(false);
        };
        Ok(matches!(
            inner.active_module.as_ref(),
            Some(module) if module.endpoint == route.endpoint
        ))
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
            let removed_flows: Vec<_> = inner
                .client_to_module
                .values()
                .filter(|route| route.endpoint == module.endpoint)
                .map(|route| Arc::clone(&route.flow))
                .collect();
            for flow in removed_flows {
                flow.close();
            }
            inner
                .client_to_module
                .retain(|_, route| route.endpoint != module.endpoint);
            inner
                .module_to_client
                .retain(|(endpoint, _), _| *endpoint != module.endpoint);
            inner
                .status
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
            if let Some(route) =
                release_client_route_locked(&mut inner, connection_id, route_channel)
            {
                released.push(route);
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

fn release_client_route_locked(
    inner: &mut ForwardingInner,
    client_connection_id: ConnectionId,
    route_channel: u16,
) -> Option<ReleasedRoute> {
    let route = inner
        .client_to_module
        .remove(&(client_connection_id, route_channel))?;
    route.flow.close();
    inner
        .module_to_client
        .remove(&(route.endpoint, route_channel));
    inner.status.remove(&(route.endpoint, route_channel));
    Some(ReleasedRoute {
        route_channel,
        module_sink: route.sink,
        negotiated_ver: route.negotiated_ver,
    })
}

/// Per-channel request-credit accounting shared by the client and module route halves.
#[derive(Debug)]
pub(crate) struct ChannelFlow {
    sem: Semaphore,
    window: usize,
}

impl ChannelFlow {
    fn new(window: usize) -> Self {
        debug_assert!(window > 0, "flow-control window must be non-zero");
        Self {
            sem: Semaphore::new(window),
            window,
        }
    }

    pub(crate) async fn acquire(&self) -> Result<(), ChannelFlowClosed> {
        let permit = self.sem.acquire().await.map_err(|_| ChannelFlowClosed)?;
        // Credits are returned by terminal frames on the module->client path, not
        // by this task's RAII lifetime.
        permit.forget();
        Ok(())
    }

    pub(crate) fn release(&self) {
        let available = self.sem.available_permits();
        if available < self.window {
            self.sem.add_permits(1);
        } else {
            // Protocol-conforming modules emit exactly one terminal per request.
            // This guard is a best-effort safety net against window growth, not a
            // security boundary against malicious peers.
            warn!(
                window = self.window,
                available, "flow-control over-release ignored"
            );
        }
    }

    pub(crate) fn close(&self) {
        self.sem.close();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ChannelFlowClosed;

impl fmt::Display for ChannelFlowClosed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "flow-control window closed")
    }
}

impl Error for ChannelFlowClosed {}

fn window_for(concurrency: &Concurrency) -> usize {
    match concurrency {
        Concurrency::Serial => 1,
        Concurrency::ModuleManaged => DEFAULT_MODULE_MANAGED_WINDOW,
        Concurrency::StatelessParallel => STATELESS_PARALLEL_WINDOW,
    }
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
