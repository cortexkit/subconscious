use std::{
    collections::{HashMap, HashSet},
    error::Error,
    fmt,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex, MutexGuard,
    },
};

use subc_protocol::{manifest::Concurrency, ErrorBody};
use tokio::sync::{oneshot, Semaphore};
use tracing::{debug, warn};

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

/// Client-local route key. A route channel is unique only within one client connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ClientRouteKey {
    pub connection_id: ConnectionId,
    pub channel: u16,
}

/// Module-local route key. A route channel is unique only within one live module endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ModuleRouteKey {
    pub endpoint: ModuleEndpointId,
    pub channel: u16,
}

#[derive(Debug, Clone)]
pub(crate) struct ModuleRoute {
    pub endpoint: ModuleEndpointId,
    pub sink: FrameSink,
    pub negotiated_ver: u8,
    pub module_channel: u16,
    pub flow: Arc<ChannelFlow>,
}

#[derive(Debug, Clone)]
pub(crate) struct ClientRoute {
    pub connection_id: ConnectionId,
    pub sink: FrameSink,
    pub negotiated_ver: u8,
    pub client_channel: u16,
    pub flow: Arc<ChannelFlow>,
}

/// Which kind of peer a route GOODBYE is being delivered to. This decides what
/// happens when the GOODBYE cannot be enqueued (egress full/closed):
/// - `Client`: escalate to closing that client connection (a socket close is a
///   stronger teardown signal, and a full client egress means it is the slow
///   client we would drop anyway — finding #6).
/// - `Module`: best-effort DROP, never close. A client-disconnect notifies the
///   SHARED module that one client's route is gone; closing the module on its
///   egress backpressure would tear down every co-tenant client (the exact
///   cross-tenant blast radius #3/#6 exist to prevent — and was observed when a
///   flooding dead client filled BOTH its own and the module's egress, so its
///   route-gone GOODBYE to the module failed and closed the shared connection).
///   subc has already removed the route from its forwarding state and drops
///   stale module frames for the released channel (see router.rs), so subc's
///   own routing is correct. The residual: under SUSTAINED module-egress
///   backpressure a module-targeted route-gone notification can be lost, which a
///   module using it for client-refcounting (e.g. AFT's session accounting)
///   would miss. This is INTENTIONALLY ACCEPTED, not a gap. A consuming module
///   must bound stale bindings with its own idle-activity reaper (last-touched
///   TTL) independent of route-gone signals — AFT confirmed this and locked it
///   as a Phase 4 acceptance criterion, so a lost GOODBYE degrades to "the
///   binding stays warm until its idle TTL" (bounded wasted resources), never an
///   unbounded leak; disk-durable replay is unaffected. A dedicated reliable
///   module control lane was evaluated (Oracle bg_87eda7f1) and deliberately NOT
///   built: it would add starvation-avoidance machinery to the thin core for a
///   bounded warm-resource window that is not a correctness issue. never-close
///   is the invariant that matters here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GoodbyeTargetKind {
    Client,
    Module,
}

#[derive(Debug, Clone)]
pub(crate) struct GoodbyeTarget {
    pub connection_id: ConnectionId,
    pub sink: FrameSink,
    pub negotiated_ver: u8,
    pub channel: u16,
    pub kind: GoodbyeTargetKind,
}

impl GoodbyeTarget {
    /// True only when an undeliverable GOODBYE should escalate to closing the
    /// target connection. Never escalate for module recipients.
    pub(crate) fn close_on_delivery_failure(&self) -> bool {
        matches!(self.kind, GoodbyeTargetKind::Client)
    }
}

#[derive(Debug)]
pub(crate) struct PendingRouteBindRelay {
    pub endpoint: ModuleEndpointId,
    pub module_sink: FrameSink,
    pub negotiated_ver: u8,
    pub client_connection_id: ConnectionId,
    pub client_channel: u16,
    pub module_channel: u16,
    pub corr: u64,
    pub receiver: oneshot::Receiver<RouteBindRelayOutcome>,
}

#[derive(Debug, Clone)]
pub(crate) struct ModuleDrainTarget {
    pub endpoint: ModuleEndpointId,
    pub sink: FrameSink,
    pub negotiated_ver: u8,
}

#[derive(Debug, Clone)]
pub(crate) enum RouteBindRelayOutcome {
    Accepted,
    Rejected(ErrorBody),
    ModuleGone(String),
}

#[derive(Debug, Clone)]
struct ModuleConnection {
    endpoint: ModuleEndpointId,
    sink: FrameSink,
    negotiated_ver: u8,
    concurrency: Concurrency,
}

#[derive(Debug, Default)]
struct ForwardingInner {
    modules_by_id: HashMap<String, ModuleConnection>,
    endpoint_by_connection: HashMap<ConnectionId, ModuleEndpointId>,
    module_id_by_endpoint: HashMap<ModuleEndpointId, String>,
    draining_endpoints: HashSet<ModuleEndpointId>,
    next_generation: u64,
    next_relay_corr: u64,
    reserved_client: HashMap<ClientRouteKey, ModuleRouteKey>,
    reserved_module: HashMap<ModuleRouteKey, ClientRouteKey>,
    next_client_channel: HashMap<ConnectionId, u16>,
    next_module_channel: HashMap<ModuleEndpointId, u16>,
    client_to_module: HashMap<ClientRouteKey, ModuleRoute>,
    module_to_client: HashMap<ModuleRouteKey, ClientRoute>,
    status: HashMap<ClientRouteKey, String>,
    pending_relays: HashMap<(ModuleEndpointId, u64), oneshot::Sender<RouteBindRelayOutcome>>,
}

#[derive(Debug, Clone)]
pub(crate) struct CloseReason {
    code: &'static str,
    message: String,
}

impl CloseReason {
    pub(crate) fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl fmt::Display for CloseReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

pub(crate) type ConnectionCloseReceiver = oneshot::Receiver<CloseReason>;

/// Dynamic forwarding state shared by the control plane and data-plane router.
#[derive(Debug, Default)]
pub struct ForwardingTable {
    inner: Mutex<ForwardingInner>,
    close_registry: Mutex<HashMap<ConnectionId, oneshot::Sender<CloseReason>>>,
}

impl ForwardingTable {
    pub(crate) fn register_connection_close(
        &self,
        connection_id: ConnectionId,
    ) -> ConnectionCloseReceiver {
        let (sender, receiver) = oneshot::channel();
        let replaced = self
            .lock_close_registry()
            .insert(connection_id, sender)
            .is_some();
        if replaced {
            warn!(
                connection_id = connection_id.get(),
                "replaced existing connection close registration"
            );
        }
        receiver
    }

    pub(crate) fn unregister_connection_close(&self, connection_id: ConnectionId) {
        self.lock_close_registry().remove(&connection_id);
    }

    pub(crate) fn request_connection_close(
        &self,
        connection_id: ConnectionId,
        reason: CloseReason,
    ) {
        let sender = self.lock_close_registry().remove(&connection_id);
        if let Some(sender) = sender {
            debug!(
                connection_id = connection_id.get(),
                close_reason = %reason,
                "requesting connection close"
            );
            let _ = sender.send(reason);
        } else {
            debug!(
                connection_id = connection_id.get(),
                close_reason = %reason,
                "connection close request ignored for inactive connection"
            );
        }
    }

    pub fn register_module_connection(
        &self,
        connection_id: ConnectionId,
        module_id: String,
        negotiated_ver: u8,
        concurrency: Concurrency,
        sink: FrameSink,
    ) -> Result<ModuleEndpointId, ForwardingError> {
        let mut inner = self.lock_inner()?;
        if let Some(old_endpoint) = inner.endpoint_by_connection.remove(&connection_id) {
            let _ = remove_module_connection_locked(&mut inner, old_endpoint);
        }

        inner.next_generation = inner.next_generation.checked_add(1).unwrap_or(1);
        let endpoint = ModuleEndpointId {
            connection_id,
            generation: inner.next_generation,
        };
        inner.endpoint_by_connection.insert(connection_id, endpoint);
        inner
            .module_id_by_endpoint
            .insert(endpoint, module_id.clone());
        inner.next_module_channel.insert(endpoint, 1);
        if inner.next_relay_corr == 0 {
            inner.next_relay_corr = 1;
        }
        inner.modules_by_id.insert(
            module_id.clone(),
            ModuleConnection {
                endpoint,
                sink,
                negotiated_ver,
                concurrency,
            },
        );
        Ok(endpoint)
    }

    pub(crate) fn begin_route_bind_relay_for(
        &self,
        client_connection_id: ConnectionId,
        module_id: &str,
    ) -> Result<PendingRouteBindRelay, ForwardingError> {
        self.begin_route_bind_relay_inner(client_connection_id, module_id)
    }

    fn begin_route_bind_relay_inner(
        &self,
        client_connection_id: ConnectionId,
        expected_module_id: &str,
    ) -> Result<PendingRouteBindRelay, ForwardingError> {
        let mut inner = self.lock_inner()?;
        let module = inner
            .modules_by_id
            .get(expected_module_id)
            .cloned()
            .ok_or(ForwardingError::NoModuleConnection)?;
        if inner.draining_endpoints.contains(&module.endpoint) {
            return Err(ForwardingError::ModuleReloading {
                module_id: expected_module_id.to_string(),
            });
        }
        let client_channel = inner.allocate_client_channel(client_connection_id)?;
        let module_channel = inner.allocate_module_channel(module.endpoint)?;
        let corr = inner.allocate_relay_corr(module.endpoint)?;

        let client_key = ClientRouteKey {
            connection_id: client_connection_id,
            channel: client_channel,
        };
        let module_key = ModuleRouteKey {
            endpoint: module.endpoint,
            channel: module_channel,
        };
        let (sender, receiver) = oneshot::channel();
        inner.reserved_client.insert(client_key, module_key);
        inner.reserved_module.insert(module_key, client_key);
        inner.pending_relays.insert((module.endpoint, corr), sender);

        Ok(PendingRouteBindRelay {
            endpoint: module.endpoint,
            module_sink: module.sink,
            negotiated_ver: module.negotiated_ver,
            client_connection_id,
            client_channel,
            module_channel,
            corr,
            receiver,
        })
    }

    pub(crate) fn commit_route(
        &self,
        client_connection_id: ConnectionId,
        client_sink: FrameSink,
        client_negotiated_ver: u8,
        endpoint: ModuleEndpointId,
        client_channel: u16,
        module_channel: u16,
    ) -> Result<(), ForwardingError> {
        let mut inner = self.lock_inner()?;
        let module_id = inner
            .module_id_by_endpoint
            .get(&endpoint)
            .cloned()
            .ok_or(ForwardingError::StaleModuleEndpoint)?;
        if inner.draining_endpoints.contains(&endpoint) {
            return Err(ForwardingError::ModuleReloading { module_id });
        }

        let client_key = ClientRouteKey {
            connection_id: client_connection_id,
            channel: client_channel,
        };
        let module_key = ModuleRouteKey {
            endpoint,
            channel: module_channel,
        };
        if inner.reserved_client.remove(&client_key) != Some(module_key)
            || inner.reserved_module.remove(&module_key) != Some(client_key)
        {
            return Err(ForwardingError::UnknownReservation {
                client_channel,
                module_channel,
            });
        }

        let module = inner
            .modules_by_id
            .get(&module_id)
            .filter(|module| module.endpoint == endpoint)
            .cloned()
            .ok_or(ForwardingError::StaleModuleEndpoint)?;

        let flow = Arc::new(ChannelFlow::new(window_for(&module.concurrency)));
        inner.client_to_module.insert(
            client_key,
            ModuleRoute {
                endpoint,
                sink: module.sink,
                negotiated_ver: module.negotiated_ver,
                module_channel,
                flow: Arc::clone(&flow),
            },
        );
        inner.module_to_client.insert(
            module_key,
            ClientRoute {
                connection_id: client_connection_id,
                sink: client_sink,
                negotiated_ver: client_negotiated_ver,
                client_channel,
                flow,
            },
        );
        Ok(())
    }

    pub(crate) fn release_reserved_route(
        &self,
        client_connection_id: ConnectionId,
        client_channel: u16,
        endpoint: ModuleEndpointId,
        module_channel: u16,
    ) -> Result<(), ForwardingError> {
        let mut inner = self.lock_inner()?;
        release_reserved_route_locked(
            &mut inner,
            ClientRouteKey {
                connection_id: client_connection_id,
                channel: client_channel,
            },
            ModuleRouteKey {
                endpoint,
                channel: module_channel,
            },
        );
        Ok(())
    }

    pub(crate) fn release_client_route(
        &self,
        client_connection_id: ConnectionId,
        client_channel: u16,
    ) -> Result<Option<GoodbyeTarget>, ForwardingError> {
        let mut inner = self.lock_inner()?;
        Ok(release_client_route_locked(
            &mut inner,
            ClientRouteKey {
                connection_id: client_connection_id,
                channel: client_channel,
            },
        ))
    }

    pub(crate) fn release_module_route(
        &self,
        module_connection_id: ConnectionId,
        module_channel: u16,
    ) -> Result<Option<GoodbyeTarget>, ForwardingError> {
        let mut inner = self.lock_inner()?;
        let Some(endpoint) = inner
            .endpoint_by_connection
            .get(&module_connection_id)
            .copied()
        else {
            return Ok(None);
        };
        Ok(release_module_route_locked(
            &mut inner,
            ModuleRouteKey {
                endpoint,
                channel: module_channel,
            },
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
        outcome: RouteBindRelayOutcome,
    ) -> Result<bool, ForwardingError> {
        let mut inner = self.lock_inner()?;
        let Some(endpoint) = inner.endpoint_by_connection.get(&connection_id).copied() else {
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
        Ok(self
            .lock_inner()?
            .endpoint_by_connection
            .get(&connection_id)
            .copied())
    }

    pub(crate) fn has_live_module_connection(
        &self,
        module_id: &str,
    ) -> Result<bool, ForwardingError> {
        Ok(self.lock_inner()?.modules_by_id.contains_key(module_id))
    }

    pub(crate) fn client_route(
        &self,
        client_connection_id: ConnectionId,
        client_channel: u16,
    ) -> Result<Option<ModuleRoute>, ForwardingError> {
        Ok(self
            .lock_inner()?
            .client_to_module
            .get(&ClientRouteKey {
                connection_id: client_connection_id,
                channel: client_channel,
            })
            .cloned())
    }

    pub(crate) fn module_route(
        &self,
        module_connection_id: ConnectionId,
        module_channel: u16,
    ) -> Result<Option<ClientRoute>, ForwardingError> {
        let inner = self.lock_inner()?;
        let Some(endpoint) = inner
            .endpoint_by_connection
            .get(&module_connection_id)
            .copied()
        else {
            return Ok(None);
        };
        Ok(inner
            .module_to_client
            .get(&ModuleRouteKey {
                endpoint,
                channel: module_channel,
            })
            .cloned())
    }

    pub(crate) fn cache_status(
        &self,
        endpoint: ModuleEndpointId,
        module_channel: u16,
        status: String,
    ) -> Result<(), ForwardingError> {
        let mut inner = self.lock_inner()?;
        if !inner.module_id_by_endpoint.contains_key(&endpoint) {
            return Err(ForwardingError::StaleModuleEndpoint);
        }

        let module_key = ModuleRouteKey {
            endpoint,
            channel: module_channel,
        };
        let client_key = inner
            .module_to_client
            .get(&module_key)
            .map(|route| ClientRouteKey {
                connection_id: route.connection_id,
                channel: route.client_channel,
            })
            .or_else(|| inner.reserved_module.get(&module_key).copied());

        if let Some(client_key) = client_key {
            inner.status.insert(client_key, status);
        } else {
            warn!(
                module_channel,
                generation = endpoint.generation,
                connection_id = endpoint.connection_id.get(),
                "dropping status update for unbound module route channel"
            );
        }
        Ok(())
    }

    pub(crate) fn get_status(
        &self,
        client_connection_id: ConnectionId,
        client_channel: u16,
    ) -> Result<Option<String>, ForwardingError> {
        Ok(self
            .lock_inner()?
            .status
            .get(&ClientRouteKey {
                connection_id: client_connection_id,
                channel: client_channel,
            })
            .cloned())
    }

    pub(crate) fn client_route_module_id(
        &self,
        client_connection_id: ConnectionId,
        client_channel: u16,
    ) -> Result<Option<String>, ForwardingError> {
        let inner = self.lock_inner()?;
        let Some(route) = inner.client_to_module.get(&ClientRouteKey {
            connection_id: client_connection_id,
            channel: client_channel,
        }) else {
            return Ok(None);
        };
        Ok(inner.module_id_by_endpoint.get(&route.endpoint).cloned())
    }

    pub(crate) fn client_route_is_bound_to_live_module(
        &self,
        client_connection_id: ConnectionId,
        client_channel: u16,
    ) -> Result<bool, ForwardingError> {
        let inner = self.lock_inner()?;
        let Some(route) = inner.client_to_module.get(&ClientRouteKey {
            connection_id: client_connection_id,
            channel: client_channel,
        }) else {
            return Ok(false);
        };
        Ok(inner.module_id_by_endpoint.contains_key(&route.endpoint))
    }

    pub fn active_binding_count(&self) -> Result<usize, ForwardingError> {
        Ok(self.lock_inner()?.client_to_module.len())
    }

    pub fn has_route_channel(&self, route_channel: u16) -> Result<bool, ForwardingError> {
        let inner = self.lock_inner()?;
        Ok(inner
            .client_to_module
            .keys()
            .any(|key| key.channel == route_channel))
    }

    pub(crate) fn begin_module_drain(
        &self,
        module_id: &str,
    ) -> Result<Option<ModuleDrainTarget>, ForwardingError> {
        let mut inner = self.lock_inner()?;
        let Some(module) = inner.modules_by_id.get(module_id).cloned() else {
            return Ok(None);
        };
        let endpoint = module.endpoint;
        inner.draining_endpoints.insert(endpoint);

        let flows = inner
            .client_to_module
            .values()
            .filter(|route| route.endpoint == endpoint)
            .map(|route| Arc::clone(&route.flow))
            .collect::<Vec<_>>();
        for flow in flows {
            flow.close();
        }

        let pending_keys = inner
            .pending_relays
            .keys()
            .filter(|(pending_endpoint, _)| *pending_endpoint == endpoint)
            .copied()
            .collect::<Vec<_>>();
        let pending = pending_keys
            .into_iter()
            .filter_map(|key| inner.pending_relays.remove(&key))
            .collect::<Vec<_>>();

        let reserved_module_keys = inner
            .reserved_module
            .keys()
            .filter(|module_key| module_key.endpoint == endpoint)
            .copied()
            .collect::<Vec<_>>();
        for module_key in reserved_module_keys {
            if let Some(client_key) = inner.reserved_module.get(&module_key).copied() {
                release_reserved_route_locked(&mut inner, client_key, module_key);
            }
        }

        let error = ErrorBody {
            code: "module_reloading".to_string(),
            message: format!("module_id '{module_id}' is reloading"),
        };
        for sender in pending {
            let _ = sender.send(RouteBindRelayOutcome::Rejected(error.clone()));
        }

        Ok(Some(ModuleDrainTarget {
            endpoint,
            sink: module.sink,
            negotiated_ver: module.negotiated_ver,
        }))
    }

    pub(crate) fn endpoint_in_flight_count(
        &self,
        endpoint: ModuleEndpointId,
    ) -> Result<usize, ForwardingError> {
        let inner = self.lock_inner()?;
        Ok(inner
            .client_to_module
            .values()
            .filter(|route| route.endpoint == endpoint)
            .map(|route| route.flow.in_flight())
            .sum())
    }

    pub(crate) fn endpoint_is_draining(
        &self,
        endpoint: ModuleEndpointId,
    ) -> Result<bool, ForwardingError> {
        Ok(self.lock_inner()?.draining_endpoints.contains(&endpoint))
    }

    pub(crate) fn module_is_draining(&self, module_id: &str) -> Result<bool, ForwardingError> {
        let inner = self.lock_inner()?;
        Ok(inner
            .modules_by_id
            .get(module_id)
            .is_some_and(|module| inner.draining_endpoints.contains(&module.endpoint)))
    }

    pub(crate) fn release_module_endpoint_routes(
        &self,
        endpoint: ModuleEndpointId,
    ) -> Result<Vec<GoodbyeTarget>, ForwardingError> {
        let mut inner = self.lock_inner()?;
        let module_keys = inner
            .module_to_client
            .keys()
            .filter(|module_key| module_key.endpoint == endpoint)
            .copied()
            .collect::<Vec<_>>();
        let mut released = Vec::with_capacity(module_keys.len());
        for module_key in module_keys {
            if let Some(target) = release_module_route_locked(&mut inner, module_key) {
                released.push(target);
            }
        }
        Ok(released)
    }

    pub(crate) fn cleanup_connection(
        &self,
        connection_id: ConnectionId,
    ) -> Result<Vec<GoodbyeTarget>, ForwardingError> {
        let mut inner = self.lock_inner()?;
        if let Some(endpoint) = inner.endpoint_by_connection.remove(&connection_id) {
            return Ok(remove_module_connection_locked(&mut inner, endpoint));
        }

        let client_keys: Vec<ClientRouteKey> = inner
            .client_to_module
            .keys()
            .filter(|key| key.connection_id == connection_id)
            .copied()
            .collect();
        let mut released = Vec::with_capacity(client_keys.len());
        for client_key in client_keys {
            if let Some(route) = release_client_route_locked(&mut inner, client_key) {
                released.push(route);
            }
        }

        let reserved_client_keys: Vec<ClientRouteKey> = inner
            .reserved_client
            .keys()
            .filter(|key| key.connection_id == connection_id)
            .copied()
            .collect();
        for client_key in reserved_client_keys {
            if let Some(module_key) = inner.reserved_client.get(&client_key).copied() {
                let module_target = inner
                    .module_id_by_endpoint
                    .get(&module_key.endpoint)
                    .and_then(|module_id| inner.modules_by_id.get(module_id))
                    // Reserved (pre-commit) route torn down on client disconnect:
                    // tells the SHARED module to drop the reservation → Module kind
                    // (best-effort drop; never close the module).
                    .map(|module| GoodbyeTarget {
                        connection_id: module.endpoint.connection_id,
                        sink: module.sink.clone(),
                        negotiated_ver: module.negotiated_ver,
                        channel: module_key.channel,
                        kind: GoodbyeTargetKind::Module,
                    });
                release_reserved_route_locked(&mut inner, client_key, module_key);
                if let Some(module_target) = module_target {
                    released.push(module_target);
                }
            }
        }
        inner.next_client_channel.remove(&connection_id);

        Ok(released)
    }

    fn lock_inner(&self) -> Result<MutexGuard<'_, ForwardingInner>, ForwardingError> {
        self.inner.lock().map_err(|_| ForwardingError::Poisoned)
    }

    fn lock_close_registry(
        &self,
    ) -> MutexGuard<'_, HashMap<ConnectionId, oneshot::Sender<CloseReason>>> {
        self.close_registry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl ForwardingInner {
    fn allocate_client_channel(
        &mut self,
        connection_id: ConnectionId,
    ) -> Result<u16, ForwardingError> {
        let mut candidate = self
            .next_client_channel
            .get(&connection_id)
            .copied()
            .unwrap_or(1)
            .max(1);
        for _ in 1..=u16::MAX {
            if candidate == 0 {
                candidate = 1;
            }
            let key = ClientRouteKey {
                connection_id,
                channel: candidate,
            };
            if !self.reserved_client.contains_key(&key) && !self.client_to_module.contains_key(&key)
            {
                let next = next_channel(candidate);
                self.next_client_channel.insert(connection_id, next);
                return Ok(candidate);
            }
            candidate = candidate.wrapping_add(1);
        }
        Err(ForwardingError::ClientRouteChannelExhausted { connection_id })
    }

    fn allocate_module_channel(
        &mut self,
        endpoint: ModuleEndpointId,
    ) -> Result<u16, ForwardingError> {
        let mut candidate = self
            .next_module_channel
            .get(&endpoint)
            .copied()
            .unwrap_or(1)
            .max(1);
        for _ in 1..=u16::MAX {
            if candidate == 0 {
                candidate = 1;
            }
            let key = ModuleRouteKey {
                endpoint,
                channel: candidate,
            };
            // Released module channels are eligible for reuse within this endpoint generation.
            // A buggy live module emitting frames for an old route after reuse can still
            // misdeliver; a per-route epoch/tombstone design is a separate protocol change.
            if !self.reserved_module.contains_key(&key) && !self.module_to_client.contains_key(&key)
            {
                let next = next_channel(candidate);
                self.next_module_channel.insert(endpoint, next);
                return Ok(candidate);
            }
            candidate = candidate.wrapping_add(1);
        }
        Err(ForwardingError::ModuleRouteChannelExhausted { endpoint })
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

fn next_channel(channel: u16) -> u16 {
    let next = channel.wrapping_add(1);
    if next == 0 {
        1
    } else {
        next
    }
}

fn release_reserved_route_locked(
    inner: &mut ForwardingInner,
    client_key: ClientRouteKey,
    module_key: ModuleRouteKey,
) {
    if inner.reserved_client.get(&client_key).copied() == Some(module_key) {
        inner.reserved_client.remove(&client_key);
    }
    if inner.reserved_module.get(&module_key).copied() == Some(client_key) {
        inner.reserved_module.remove(&module_key);
    }
    inner.status.remove(&client_key);
}

fn release_client_route_locked(
    inner: &mut ForwardingInner,
    client_key: ClientRouteKey,
) -> Option<GoodbyeTarget> {
    let route = inner.client_to_module.remove(&client_key)?;
    route.flow.close();
    inner.module_to_client.remove(&ModuleRouteKey {
        endpoint: route.endpoint,
        channel: route.module_channel,
    });
    inner.status.remove(&client_key);
    // Notifies the SHARED module that this client's route is gone → Module kind
    // (best-effort drop on backpressure; never close the module).
    Some(GoodbyeTarget {
        connection_id: route.endpoint.connection_id,
        sink: route.sink,
        negotiated_ver: route.negotiated_ver,
        channel: route.module_channel,
        kind: GoodbyeTargetKind::Module,
    })
}

fn release_module_route_locked(
    inner: &mut ForwardingInner,
    module_key: ModuleRouteKey,
) -> Option<GoodbyeTarget> {
    let route = inner.module_to_client.remove(&module_key)?;
    route.flow.close();
    let client_key = ClientRouteKey {
        connection_id: route.connection_id,
        channel: route.client_channel,
    };
    inner.client_to_module.remove(&client_key);
    inner.status.remove(&client_key);
    // Notifies the CLIENT that its route is gone (module-side teardown) → Client
    // kind (escalate to closing the client on backpressure, finding #6).
    Some(GoodbyeTarget {
        connection_id: route.connection_id,
        sink: route.sink,
        negotiated_ver: route.negotiated_ver,
        channel: route.client_channel,
        kind: GoodbyeTargetKind::Client,
    })
}

fn remove_module_connection_locked(
    inner: &mut ForwardingInner,
    endpoint: ModuleEndpointId,
) -> Vec<GoodbyeTarget> {
    inner.draining_endpoints.remove(&endpoint);
    let module_id = inner.module_id_by_endpoint.remove(&endpoint);
    if let Some(module_id) = module_id.as_ref() {
        inner.modules_by_id.remove(module_id);
    }
    inner.endpoint_by_connection.remove(&endpoint.connection_id);
    inner.next_module_channel.remove(&endpoint);
    let reserved_module_keys: Vec<ModuleRouteKey> = inner
        .reserved_module
        .keys()
        .filter(|module_key| module_key.endpoint == endpoint)
        .copied()
        .collect();
    for module_key in reserved_module_keys {
        if let Some(client_key) = inner.reserved_module.get(&module_key).copied() {
            release_reserved_route_locked(inner, client_key, module_key);
        }
    }

    let pending_keys: Vec<_> = inner
        .pending_relays
        .keys()
        .filter(|(pending_endpoint, _)| *pending_endpoint == endpoint)
        .copied()
        .collect();
    let pending: Vec<_> = pending_keys
        .into_iter()
        .filter_map(|key| inner.pending_relays.remove(&key))
        .collect();
    for sender in pending {
        let module_label = module_id.as_deref().unwrap_or("unknown");
        let _ = sender.send(RouteBindRelayOutcome::ModuleGone(format!(
            "module '{module_label}' connection closed during route.bind relay"
        )));
    }

    let module_keys: Vec<ModuleRouteKey> = inner
        .module_to_client
        .keys()
        .filter(|module_key| module_key.endpoint == endpoint)
        .copied()
        .collect();
    let mut released = Vec::with_capacity(module_keys.len());
    for module_key in module_keys {
        if let Some(target) = release_module_route_locked(inner, module_key) {
            released.push(target);
        }
    }
    released
}

/// Per-channel request-credit accounting shared by the client and module route halves.
#[derive(Debug)]
pub(crate) struct ChannelFlow {
    sem: Semaphore,
    window: usize,
    in_flight: AtomicUsize,
}

impl ChannelFlow {
    fn new(window: usize) -> Self {
        debug_assert!(window > 0, "flow-control window must be non-zero");
        Self {
            sem: Semaphore::new(window),
            window,
            in_flight: AtomicUsize::new(0),
        }
    }

    pub(crate) async fn acquire(&self) -> Result<(), ChannelFlowClosed> {
        let permit = self.sem.acquire().await.map_err(|_| ChannelFlowClosed)?;
        // Credits are returned by terminal frames on the module->client path, not
        // by this task's RAII lifetime. Track the outstanding count separately so
        // a reload drain can close admission while still observing old requests.
        self.in_flight.fetch_add(1, Ordering::AcqRel);
        permit.forget();
        Ok(())
    }

    pub(crate) fn release(&self) {
        let mut observed = self.in_flight.load(Ordering::Acquire);
        loop {
            if observed == 0 {
                // Protocol-conforming modules emit exactly one terminal per request.
                // This guard is a best-effort safety net against window growth, not a
                // security boundary against malicious peers.
                warn!(
                    window = self.window,
                    available = self.sem.available_permits(),
                    "flow-control over-release ignored"
                );
                return;
            }
            match self.in_flight.compare_exchange_weak(
                observed,
                observed - 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    if !self.sem.is_closed() {
                        self.sem.add_permits(1);
                    }
                    return;
                }
                Err(next) => observed = next,
            }
        }
    }

    pub(crate) fn in_flight(&self) -> usize {
        self.in_flight.load(Ordering::Acquire)
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
    ModuleReloading {
        module_id: String,
    },
    StaleModuleEndpoint,
    UnknownReservation {
        client_channel: u16,
        module_channel: u16,
    },
    ClientRouteChannelExhausted {
        connection_id: ConnectionId,
    },
    ModuleRouteChannelExhausted {
        endpoint: ModuleEndpointId,
    },
    RelayCorrelationExhausted,
    Poisoned,
}

impl fmt::Display for ForwardingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoModuleConnection => write!(f, "no module connection is registered"),
            Self::ModuleReloading { module_id } => {
                write!(f, "module_id '{module_id}' is reloading")
            }
            Self::StaleModuleEndpoint => write!(f, "module connection generation is stale"),
            Self::UnknownReservation {
                client_channel,
                module_channel,
            } => write!(
                f,
                "route reservation client channel {client_channel} / module channel {module_channel} was not found"
            ),
            Self::ClientRouteChannelExhausted { connection_id } => write!(
                f,
                "no client route channels are available for connection {}",
                connection_id.get()
            ),
            Self::ModuleRouteChannelExhausted { endpoint } => write!(
                f,
                "no module route channels are available for endpoint generation {} on connection {}",
                endpoint.generation,
                endpoint.connection_id.get()
            ),
            Self::RelayCorrelationExhausted => {
                write!(f, "no route.bind relay correlation ids are available")
            }
            Self::Poisoned => write!(f, "forwarding table lock was poisoned"),
        }
    }
}

impl Error for ForwardingError {}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    #[test]
    fn multi_provider_route_limit_reports_per_client_exhaustion_without_affecting_second_client() {
        let forwarding = ForwardingTable::default();
        let module_connection = ConnectionId::new(10);
        let exhausted_client = ConnectionId::new(20);
        let second_client = ConnectionId::new(30);
        let (module_tx, _module_rx) = mpsc::channel(1);
        let endpoint = forwarding
            .register_module_connection(
                module_connection,
                "route-limit-provider".to_string(),
                1,
                Concurrency::ModuleManaged,
                FrameSink::new(module_tx),
            )
            .unwrap();

        {
            let mut inner = forwarding.inner.lock().unwrap();
            for channel in 1..=u16::MAX {
                inner.reserved_client.insert(
                    ClientRouteKey {
                        connection_id: exhausted_client,
                        channel,
                    },
                    ModuleRouteKey {
                        endpoint,
                        channel: 1,
                    },
                );
            }
        }

        let err = forwarding
            .begin_route_bind_relay_for(exhausted_client, "route-limit-provider")
            .unwrap_err();
        assert!(matches!(
            err,
            ForwardingError::ClientRouteChannelExhausted { connection_id }
                if connection_id == exhausted_client
        ));

        let pending = forwarding
            .begin_route_bind_relay_for(second_client, "route-limit-provider")
            .unwrap();
        assert_eq!(pending.client_channel, 1);
    }

    #[test]
    fn released_module_channels_are_reused_after_wrap_without_slot_leak() {
        let forwarding = ForwardingTable::default();
        let module_connection = ConnectionId::new(40);
        let client = ConnectionId::new(50);
        let (module_tx, _module_rx) = mpsc::channel(1);
        forwarding
            .register_module_connection(
                module_connection,
                "slot-reuse-provider".to_string(),
                1,
                Concurrency::ModuleManaged,
                FrameSink::new(module_tx),
            )
            .unwrap();

        let mut wrapped_channel = None;
        for index in 0..=usize::from(u16::MAX) {
            let pending = forwarding
                .begin_route_bind_relay_for(client, "slot-reuse-provider")
                .unwrap();
            if index == usize::from(u16::MAX) {
                wrapped_channel = Some(pending.module_channel);
            }
            forwarding
                .cancel_pending_relay(pending.endpoint, pending.corr)
                .unwrap();
            forwarding
                .release_reserved_route(
                    pending.client_connection_id,
                    pending.client_channel,
                    pending.endpoint,
                    pending.module_channel,
                )
                .unwrap();
        }

        assert_eq!(wrapped_channel, Some(1));
    }

    #[test]
    fn cleanup_connection_prunes_stale_next_client_channel_cursor() {
        let forwarding = ForwardingTable::default();
        let client = ConnectionId::new(60);
        forwarding
            .inner
            .lock()
            .unwrap()
            .next_client_channel
            .insert(client, 41);

        let released = forwarding.cleanup_connection(client).unwrap();

        assert!(released.is_empty());
        assert!(!forwarding
            .inner
            .lock()
            .unwrap()
            .next_client_channel
            .contains_key(&client));
    }
}
