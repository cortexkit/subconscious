use std::{
    collections::{HashMap, HashSet},
    error::Error,
    fmt,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard,
    },
    time::Duration,
};

use subc_control::{ClientControlResponse, RouteCloseReason};
use subc_protocol::{
    manifest::Concurrency, session::ModuleControlResponse, ErrorBody, Flags, FrameType, Principal,
    Priority,
};
use tokio::sync::{mpsc, oneshot, Semaphore};
use tokio::time::Instant;
use tracing::{debug, warn};

use crate::{observability::DaemonCounters, registry::ConnectionId, router::FrameSink, Frame};

/// Default per-channel request-credit window for modules that schedule internally.
const DEFAULT_MODULE_MANAGED_WINDOW: usize = 32;

/// High per-channel cap for stateless modules; this is an OOM guard, not scheduling policy.
const STATELESS_PARALLEL_WINDOW: usize = 1024;

/// A stopped probe cycle cannot retain its last unanswered correlation forever.
/// Active endpoints replace the tombstone on their next serial health probe;
/// this backstop covers endpoints that stop probing altogether.
const HEALTH_PROBE_TOMBSTONE_TTL: Duration = Duration::from_secs(5 * 60);

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

#[derive(Debug)]
pub(crate) struct RouteBinding {
    pub client_connection_id: ConnectionId,
    pub client_sink: FrameSink,
    pub client_negotiated_ver: u8,
    pub client_channel: u16,
    pub client_epoch: u32,
    pub module_id: String,
    pub module_endpoint: ModuleEndpointId,
    pub module_sink: FrameSink,
    pub module_negotiated_ver: u8,
    pub module_channel: u16,
    pub module_epoch: u32,
    pub principal: Principal,
    pub bound_at: Instant,
    pub flow: Arc<ChannelFlow>,
}

#[derive(Debug, Clone)]
pub(crate) enum DataRoute {
    Client(DataRouteState),
    Module(DataRouteState),
}

#[derive(Debug, Clone)]
pub(crate) enum DataRouteState {
    Bound(Arc<RouteBinding>),
    Reserved,
    EpochMismatch,
    Absent,
}

/// Which kind of peer a route GOODBYE is being delivered to. This decides what
/// happens when the GOODBYE cannot be enqueued (egress full/closed):
/// - `Client`: escalate to closing that client connection (a socket close is a
///   stronger teardown signal, and a full client egress means it is the slow
///   client we would drop anyway).
/// - `Module`: best-effort DROP, never close. A client-disconnect notifies the
///   SHARED module that one client's route is gone; closing the module on its
///   egress backpressure would tear down every co-tenant client (the exact
///   cross-tenant blast radius this never-close rule exists to prevent — observed when a
///   flooding dead client filled BOTH its own and the module's egress, so its
///   route-gone GOODBYE to the module failed and closed the shared connection).
///   subc has already removed the route from its forwarding state and drops
///   stale module frames for the released channel (see router.rs), so subc's
///   own routing is correct. The residual: under SUSTAINED module-egress
///   backpressure a module-targeted route-gone notification can be lost, which a
///   module using it for client-refcounting (e.g. AFT's session accounting)
///   would miss. This is INTENTIONALLY ACCEPTED, not a gap. A consuming module
///   must bound stale bindings with its own idle-activity reaper (last-touched
///   TTL) independent of route-gone signals — AFT does exactly this, so a lost
///   GOODBYE degrades to "the
///   binding stays warm until its idle TTL" (bounded wasted resources), never an
///   unbounded leak; disk-durable replay is unaffected. A dedicated reliable
///   module control lane was evaluated and deliberately NOT
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
    pub epoch: u32,
    pub kind: GoodbyeTargetKind,
    /// The receiving module when this is a module-targeted relay. Client targets
    /// have no module owner, while this identifier attributes a dropped relay.
    pub module_id: Option<String>,
}

impl GoodbyeTarget {
    /// True only when an undeliverable GOODBYE should escalate to closing the
    /// target connection. Never escalate for module recipients.
    pub(crate) fn close_on_delivery_failure(&self) -> bool {
        matches!(self.kind, GoodbyeTargetKind::Client)
    }
}

/// One route currently served by a module endpoint.
///
/// `goodbye_target` is deliberately retained alongside the census projection so
/// a draining caller can address exactly the same route set that this read
/// reports, without a second forwarding-table pass.
#[derive(Debug, Clone)]
pub(crate) struct EndpointRoute {
    pub goodbye_target: GoodbyeTarget,
    pub principal: Principal,
    pub bound_at: Instant,
    pub draining: bool,
    /// WHY the endpoint is draining, when it is. Carried per-route so the
    /// census can answer "closing because of what" without a second lookup;
    /// `None` exactly when `draining` is false (one source: the drain map).
    pub drain_reason: Option<RouteCloseReason>,
}

#[derive(Debug)]
pub(crate) struct PendingRouteBindRelay {
    pub endpoint: ModuleEndpointId,
    pub module_sink: FrameSink,
    pub negotiated_ver: u8,
    pub client_channel: u16,
    pub client_epoch: u32,
    pub module_channel: u16,
    pub module_epoch: u32,
    pub corr: u64,
    pub receiver: oneshot::Receiver<RouteBindRelayOutcome>,
}

#[derive(Debug, Clone)]
pub(crate) struct ModuleDrainTarget {
    pub endpoint: ModuleEndpointId,
    pub sink: FrameSink,
    pub negotiated_ver: u8,
    pub abandoned_bindings: Vec<GoodbyeTarget>,
}

#[derive(Debug, Clone)]
pub(crate) enum RouteBindRelayOutcome {
    Accepted,
    Rejected(ErrorBody),
    ModuleGone(String),
}

#[derive(Debug, Clone)]
pub(crate) struct PendingRelayCompletion {
    pub settled: bool,
    pub abandoned: Option<GoodbyeTarget>,
}

#[derive(Debug)]
pub(crate) struct PendingModuleControlRpc {
    pub endpoint: ModuleEndpointId,
    pub module_sink: FrameSink,
    pub negotiated_ver: u8,
    pub corr: u64,
    pub receiver: oneshot::Receiver<ModuleControlRpcOutcome>,
}

#[derive(Debug, Clone)]
pub(crate) enum ModuleControlRpcOutcome {
    Response(ModuleControlResponse),
    Rejected(ErrorBody),
    ModuleGone(String),
    MalformedResponse(String),
    UnexpectedOp { expected: String, actual: String },
    DeadlineElapsed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ModuleControlRpcCompletion {
    Unknown,
    Settled,
    LateHealthAnswer {
        module_id: String,
        latency: Duration,
    },
}

#[derive(Debug)]
struct PendingModuleControlRpcEntry {
    expected_op: String,
    deadline: Instant,
    health_probe_started_at: Option<Instant>,
    sender: oneshot::Sender<ModuleControlRpcOutcome>,
}

#[derive(Debug)]
struct HealthProbeTombstone {
    expected_op: String,
    module_id: String,
    probe_started_at: Instant,
    expires_at: Instant,
}

#[derive(Debug, Clone, Copy)]
struct RouteReservation {
    client_key: ClientRouteKey,
    module_key: ModuleRouteKey,
    client_epoch: u32,
    module_epoch: u32,
}

#[derive(Debug)]
struct PendingRouteBindRelayEntry {
    reservation: RouteReservation,
    client_sink: FrameSink,
    client_negotiated_ver: u8,
    client_permit: mpsc::OwnedPermit<crate::router::OutboundFrame>,
    route_open_frame: Frame,
    principal: Principal,
    deadline: Instant,
    relay_enqueued: bool,
    sender: oneshot::Sender<RouteBindRelayOutcome>,
}

#[derive(Debug, Clone)]
pub(crate) enum RouteRelease {
    Removed(GoodbyeTarget),
    Stale,
    Absent,
}

#[derive(Debug, Clone)]
pub(crate) enum RoutePollSnapshot {
    Bound {
        module_id: String,
        status: Option<String>,
    },
    Absent,
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
    /// Endpoints mid-drain, keyed to the reason the drain was begun with. The
    /// value serves the census ("draining because restart"); membership alone
    /// still answers every admission-gate check.
    draining_endpoints: HashMap<ModuleEndpointId, RouteCloseReason>,
    closing_connections: HashSet<ConnectionId>,
    next_generation: u64,
    reserved_client: HashMap<ClientRouteKey, ModuleRouteKey>,
    reserved_module: HashMap<ModuleRouteKey, ClientRouteKey>,
    next_client_channel: HashMap<ConnectionId, u16>,
    next_module_channel: HashMap<ModuleEndpointId, u16>,
    client_slot_epochs: HashMap<ClientRouteKey, u32>,
    module_slot_epochs: HashMap<ModuleRouteKey, u32>,
    last_published_epoch: HashMap<ClientRouteKey, u32>,
    client_to_module: HashMap<ClientRouteKey, Arc<RouteBinding>>,
    module_to_client: HashMap<ModuleRouteKey, Arc<RouteBinding>>,
    status: HashMap<(ClientRouteKey, u32), String>,
    pending_relays: HashMap<(ModuleEndpointId, u64), PendingRouteBindRelayEntry>,
    next_control_corr: HashMap<ModuleEndpointId, u64>,
    pending_control_rpcs: HashMap<(ModuleEndpointId, u64), PendingModuleControlRpcEntry>,
    health_probe_tombstones: HashMap<(ModuleEndpointId, u64), HealthProbeTombstone>,
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
    inner: Arc<RwLock<ForwardingInner>>,
    close_registry: Mutex<HashMap<ConnectionId, oneshot::Sender<CloseReason>>>,
    counters: DaemonCounters,
}

impl ForwardingTable {
    pub(crate) fn counters(&self) -> DaemonCounters {
        self.counters.clone()
    }

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
        let mut inner = self.write_inner()?;
        if inner.closing_connections.contains(&connection_id) {
            return Err(ForwardingError::ConnectionClosing { connection_id });
        }
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
        inner.next_control_corr.insert(endpoint, 1);
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

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn begin_route_bind_relay_for(
        &self,
        client_connection_id: ConnectionId,
        client_sink: FrameSink,
        client_negotiated_ver: u8,
        client_corr: u64,
        module_id: &str,
        principal: Principal,
        deadline: Instant,
    ) -> Result<PendingRouteBindRelay, ForwardingError> {
        // Reserve egress capacity before taking the forwarding lock. The permit is
        // held until the bind reaches one terminal state, so an accepted bind can
        // publish its table entry and RouteOpen response in one critical section.
        let client_permit =
            client_sink
                .reserve_owned()
                .await
                .map_err(|_| ForwardingError::ClientEgressClosed {
                    connection_id: client_connection_id,
                })?;
        self.begin_route_bind_relay_inner(
            client_connection_id,
            client_sink,
            client_negotiated_ver,
            client_corr,
            module_id,
            principal,
            deadline,
            client_permit,
        )
    }

    #[cfg(test)]
    pub(crate) fn begin_route_bind_relay_for_test(
        &self,
        client_connection_id: ConnectionId,
        client_sink: FrameSink,
        client_corr: u64,
        module_id: &str,
    ) -> Result<PendingRouteBindRelay, ForwardingError> {
        let permit =
            client_sink
                .try_reserve_owned()
                .map_err(|_| ForwardingError::ClientEgressClosed {
                    connection_id: client_connection_id,
                })?;
        self.begin_route_bind_relay_inner(
            client_connection_id,
            client_sink,
            subc_protocol::PROTOCOL_VERSION,
            client_corr,
            module_id,
            Principal::Direct,
            Instant::now() + std::time::Duration::from_secs(60),
            permit,
        )
    }

    pub(crate) fn begin_module_control_rpc_for(
        &self,
        module_id: &str,
        expected_op: &str,
        deadline: Instant,
    ) -> Result<PendingModuleControlRpc, ForwardingError> {
        self.begin_module_control_rpc_inner(module_id, expected_op, deadline, None)
    }

    pub(crate) fn begin_health_probe_rpc_for(
        &self,
        module_id: &str,
        expected_op: &str,
        probe_started_at: Instant,
        deadline: Instant,
    ) -> Result<PendingModuleControlRpc, ForwardingError> {
        self.begin_module_control_rpc_inner(
            module_id,
            expected_op,
            deadline,
            Some(probe_started_at),
        )
    }

    fn begin_module_control_rpc_inner(
        &self,
        module_id: &str,
        expected_op: &str,
        deadline: Instant,
        health_probe_started_at: Option<Instant>,
    ) -> Result<PendingModuleControlRpc, ForwardingError> {
        let mut inner = self.write_inner()?;
        let module = inner
            .modules_by_id
            .get(module_id)
            .cloned()
            .ok_or(ForwardingError::NoModuleConnection)?;
        if inner.draining_endpoints.contains_key(&module.endpoint) {
            return Err(ForwardingError::ModuleReloading {
                module_id: module_id.to_string(),
            });
        }
        if inner
            .closing_connections
            .contains(&module.endpoint.connection_id)
        {
            return Err(ForwardingError::ConnectionClosing {
                connection_id: module.endpoint.connection_id,
            });
        }
        if health_probe_started_at.is_some() {
            // Recurring health probes are serial per endpoint. Once the next one
            // starts, an older answer can no longer improve the current snapshot,
            // so retaining more than the newest unanswered probe has no value.
            inner
                .health_probe_tombstones
                .retain(|(endpoint, _), _| *endpoint != module.endpoint);
        }
        let corr = match inner.allocate_control_corr(module.endpoint) {
            Ok(corr) => corr,
            Err(err) => {
                drop(inner);
                self.request_connection_close(
                    module.endpoint.connection_id,
                    CloseReason::new(
                        "control_correlation_exhausted",
                        "daemon-originated channel-0 correlation space exhausted",
                    ),
                );
                return Err(err);
            }
        };
        let (sender, receiver) = oneshot::channel();
        inner.pending_control_rpcs.insert(
            (module.endpoint, corr),
            PendingModuleControlRpcEntry {
                expected_op: expected_op.to_string(),
                deadline,
                health_probe_started_at,
                sender,
            },
        );

        Ok(PendingModuleControlRpc {
            endpoint: module.endpoint,
            module_sink: module.sink,
            negotiated_ver: module.negotiated_ver,
            corr,
            receiver,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn begin_route_bind_relay_inner(
        &self,
        client_connection_id: ConnectionId,
        client_sink: FrameSink,
        client_negotiated_ver: u8,
        client_corr: u64,
        expected_module_id: &str,
        principal: Principal,
        deadline: Instant,
        client_permit: mpsc::OwnedPermit<crate::router::OutboundFrame>,
    ) -> Result<PendingRouteBindRelay, ForwardingError> {
        let mut inner = self.write_inner()?;
        if inner.closing_connections.contains(&client_connection_id) {
            return Err(ForwardingError::ConnectionClosing {
                connection_id: client_connection_id,
            });
        }
        let module = inner
            .modules_by_id
            .get(expected_module_id)
            .cloned()
            .ok_or(ForwardingError::NoModuleConnection)?;
        if inner.draining_endpoints.contains_key(&module.endpoint) {
            return Err(ForwardingError::ModuleReloading {
                module_id: expected_module_id.to_string(),
            });
        }
        if inner
            .closing_connections
            .contains(&module.endpoint.connection_id)
        {
            return Err(ForwardingError::ConnectionClosing {
                connection_id: module.endpoint.connection_id,
            });
        }

        let corr = match inner.allocate_control_corr(module.endpoint) {
            Ok(corr) => corr,
            Err(err) => {
                drop(inner);
                self.request_connection_close(
                    module.endpoint.connection_id,
                    CloseReason::new(
                        "control_correlation_exhausted",
                        "daemon-originated channel-0 correlation space exhausted",
                    ),
                );
                return Err(err);
            }
        };
        let (client_channel, client_epoch, module_channel, module_epoch) =
            inner.allocate_route_slots(client_connection_id, module.endpoint)?;
        let client_key = ClientRouteKey {
            connection_id: client_connection_id,
            channel: client_channel,
        };
        let module_key = ModuleRouteKey {
            endpoint: module.endpoint,
            channel: module_channel,
        };
        let reservation = RouteReservation {
            client_key,
            module_key,
            client_epoch,
            module_epoch,
        };
        let response_body = serde_json::to_vec(&ClientControlResponse::RouteOpen {
            route_channel: client_channel,
            route_epoch: client_epoch,
        })
        .map_err(|err| ForwardingError::RouteOpenBuild(err.to_string()))?;
        let route_open_frame = Frame::build_with_version(
            client_negotiated_ver,
            FrameType::Response,
            Flags::new(false, Priority::Passive, false),
            0,
            0,
            client_corr,
            response_body,
        )
        .map_err(|err| ForwardingError::RouteOpenBuild(err.to_string()))?;
        let (sender, receiver) = oneshot::channel();
        inner.reserved_client.insert(client_key, module_key);
        inner.reserved_module.insert(module_key, client_key);
        inner.pending_relays.insert(
            (module.endpoint, corr),
            PendingRouteBindRelayEntry {
                reservation,
                client_sink,
                client_negotiated_ver,
                client_permit,
                route_open_frame,
                principal,
                deadline,
                relay_enqueued: false,
                sender,
            },
        );

        Ok(PendingRouteBindRelay {
            endpoint: module.endpoint,
            module_sink: module.sink,
            negotiated_ver: module.negotiated_ver,
            client_channel,
            client_epoch,
            module_channel,
            module_epoch,
            corr,
            receiver,
        })
    }

    pub(crate) fn mark_route_bind_relay_enqueued(
        &self,
        endpoint: ModuleEndpointId,
        corr: u64,
    ) -> Result<bool, ForwardingError> {
        let mut inner = self.write_inner()?;
        let Some(pending) = inner.pending_relays.get_mut(&(endpoint, corr)) else {
            return Ok(false);
        };
        pending.relay_enqueued = true;
        Ok(true)
    }

    pub(crate) fn release_client_route(
        &self,
        client_connection_id: ConnectionId,
        client_channel: u16,
        expected_epoch: u32,
    ) -> Result<RouteRelease, ForwardingError> {
        let mut inner = self.write_inner()?;
        let release = release_client_route_locked(
            &mut inner,
            ClientRouteKey {
                connection_id: client_connection_id,
                channel: client_channel,
            },
            expected_epoch,
        );
        self.record_route_release(&release);
        Ok(release)
    }

    pub(crate) fn release_module_route(
        &self,
        module_connection_id: ConnectionId,
        module_channel: u16,
        expected_epoch: u32,
    ) -> Result<RouteRelease, ForwardingError> {
        let mut inner = self.write_inner()?;
        let Some(endpoint) = inner
            .endpoint_by_connection
            .get(&module_connection_id)
            .copied()
        else {
            return Ok(RouteRelease::Absent);
        };
        let release = release_module_route_locked(
            &mut inner,
            ModuleRouteKey {
                endpoint,
                channel: module_channel,
            },
            expected_epoch,
        );
        self.record_route_release(&release);
        Ok(release)
    }

    pub(crate) fn abort_pending_relay(
        &self,
        endpoint: ModuleEndpointId,
        corr: u64,
        outcome: RouteBindRelayOutcome,
    ) -> Result<Option<GoodbyeTarget>, ForwardingError> {
        let mut inner = self.write_inner()?;
        let Some(pending) = inner.pending_relays.remove(&(endpoint, corr)) else {
            return Ok(None);
        };
        release_reserved_route_locked(
            &mut inner,
            pending.reservation.client_key,
            pending.reservation.module_key,
        );
        let target = pending
            .relay_enqueued
            .then(|| abandoned_route_target(&inner, &pending.reservation));
        let _ = pending.sender.send(outcome);
        Ok(target.flatten())
    }

    pub(crate) fn cancel_module_control_rpc(
        &self,
        endpoint: ModuleEndpointId,
        corr: u64,
    ) -> Result<(), ForwardingError> {
        self.write_inner()?
            .pending_control_rpcs
            .remove(&(endpoint, corr));
        Ok(())
    }

    pub(crate) fn tombstone_health_probe_rpc(
        &self,
        endpoint: ModuleEndpointId,
        corr: u64,
    ) -> Result<bool, ForwardingError> {
        let key = (endpoint, corr);
        let expires_at = Instant::now() + HEALTH_PROBE_TOMBSTONE_TTL;
        {
            let mut inner = self.write_inner()?;
            let Some(pending) = inner.pending_control_rpcs.remove(&key) else {
                return Ok(false);
            };
            let Some(probe_started_at) = pending.health_probe_started_at else {
                inner.pending_control_rpcs.insert(key, pending);
                return Ok(false);
            };
            let module_id = inner
                .module_id_by_endpoint
                .get(&endpoint)
                .cloned()
                .unwrap_or_else(|| "unknown".to_string());
            inner.health_probe_tombstones.insert(
                key,
                HealthProbeTombstone {
                    expected_op: pending.expected_op,
                    module_id,
                    probe_started_at,
                    expires_at,
                },
            );
        }
        self.schedule_health_probe_tombstone_expiration(key, expires_at);
        Ok(true)
    }

    fn schedule_health_probe_tombstone_expiration(
        &self,
        key: (ModuleEndpointId, u64),
        expires_at: Instant,
    ) {
        let inner = Arc::downgrade(&self.inner);
        tokio::spawn(async move {
            tokio::time::sleep_until(expires_at).await;
            let Some(inner) = inner.upgrade() else {
                return;
            };
            let Ok(mut inner) = inner.write() else {
                return;
            };
            let expired = inner
                .health_probe_tombstones
                .get(&key)
                .is_some_and(|tombstone| tombstone.expires_at <= Instant::now());
            if expired {
                inner.health_probe_tombstones.remove(&key);
            }
        });
    }

    pub(crate) fn complete_pending_relay(
        &self,
        connection_id: ConnectionId,
        corr: u64,
        outcome: RouteBindRelayOutcome,
    ) -> Result<PendingRelayCompletion, ForwardingError> {
        let mut inner = self.write_inner()?;
        let Some(endpoint) = inner.endpoint_by_connection.get(&connection_id).copied() else {
            return Ok(PendingRelayCompletion {
                settled: false,
                abandoned: None,
            });
        };
        let Some(pending) = inner.pending_relays.remove(&(endpoint, corr)) else {
            return Ok(PendingRelayCompletion {
                settled: false,
                abandoned: None,
            });
        };

        if Instant::now() >= pending.deadline {
            release_reserved_route_locked(
                &mut inner,
                pending.reservation.client_key,
                pending.reservation.module_key,
            );
            let abandoned = matches!(outcome, RouteBindRelayOutcome::Accepted)
                .then(|| abandoned_route_target(&inner, &pending.reservation))
                .flatten();
            let _ = pending
                .sender
                .send(RouteBindRelayOutcome::Rejected(ErrorBody {
                    code: "module_timeout".to_string(),
                    message: "route.bind response arrived after its daemon deadline".to_string(),
                    detail: None,
                }));
            return Ok(PendingRelayCompletion {
                settled: true,
                abandoned,
            });
        }

        match outcome {
            RouteBindRelayOutcome::Accepted if pending.client_sink.is_closed() => {
                release_reserved_route_locked(
                    &mut inner,
                    pending.reservation.client_key,
                    pending.reservation.module_key,
                );
                let abandoned = pending
                    .relay_enqueued
                    .then(|| abandoned_route_target(&inner, &pending.reservation))
                    .flatten();
                let _ = pending.sender.send(RouteBindRelayOutcome::ModuleGone(
                    "client egress closed before route publication".to_string(),
                ));
                return Ok(PendingRelayCompletion {
                    settled: true,
                    abandoned,
                });
            }
            RouteBindRelayOutcome::Accepted => {
                let abandoned = commit_route_locked(&mut inner, pending)?;
                return Ok(PendingRelayCompletion {
                    settled: true,
                    abandoned,
                });
            }
            terminal => {
                release_reserved_route_locked(
                    &mut inner,
                    pending.reservation.client_key,
                    pending.reservation.module_key,
                );
                let _ = pending.sender.send(terminal);
            }
        }
        Ok(PendingRelayCompletion {
            settled: true,
            abandoned: None,
        })
    }

    pub(crate) fn pending_module_control_op(
        &self,
        connection_id: ConnectionId,
        corr: u64,
    ) -> Result<Option<String>, ForwardingError> {
        let inner = self.read_inner()?;
        let Some(endpoint) = inner.endpoint_by_connection.get(&connection_id).copied() else {
            return Ok(None);
        };
        let key = (endpoint, corr);
        Ok(inner
            .pending_control_rpcs
            .get(&key)
            .map(|pending| pending.expected_op.clone())
            .or_else(|| {
                inner
                    .health_probe_tombstones
                    .get(&key)
                    .filter(|tombstone| tombstone.expires_at > Instant::now())
                    .map(|tombstone| tombstone.expected_op.clone())
            }))
    }

    pub(crate) fn complete_module_control_rpc(
        &self,
        connection_id: ConnectionId,
        corr: u64,
        actual_op: Option<&str>,
        outcome: ModuleControlRpcOutcome,
    ) -> Result<ModuleControlRpcCompletion, ForwardingError> {
        let now = Instant::now();
        let mut inner = self.write_inner()?;
        let Some(endpoint) = inner.endpoint_by_connection.get(&connection_id).copied() else {
            return Ok(ModuleControlRpcCompletion::Unknown);
        };
        let key = (endpoint, corr);
        if let Some(pending) = inner.pending_control_rpcs.remove(&key) {
            if now >= pending.deadline {
                let late_health_answer = pending.health_probe_started_at.map(|probe_started_at| {
                    ModuleControlRpcCompletion::LateHealthAnswer {
                        module_id: inner
                            .module_id_by_endpoint
                            .get(&endpoint)
                            .cloned()
                            .unwrap_or_else(|| "unknown".to_string()),
                        latency: now.saturating_duration_since(probe_started_at),
                    }
                });
                let _ = pending
                    .sender
                    .send(ModuleControlRpcOutcome::DeadlineElapsed);
                return Ok(late_health_answer.unwrap_or(ModuleControlRpcCompletion::Settled));
            }
            let outcome = match actual_op {
                Some(actual) if actual != pending.expected_op => {
                    ModuleControlRpcOutcome::UnexpectedOp {
                        expected: pending.expected_op,
                        actual: actual.to_string(),
                    }
                }
                _ => outcome,
            };
            let _ = pending.sender.send(outcome);
            return Ok(ModuleControlRpcCompletion::Settled);
        }

        let Some(tombstone) = inner.health_probe_tombstones.remove(&key) else {
            return Ok(ModuleControlRpcCompletion::Unknown);
        };
        if tombstone.expires_at <= now {
            return Ok(ModuleControlRpcCompletion::Unknown);
        }
        Ok(ModuleControlRpcCompletion::LateHealthAnswer {
            module_id: tombstone.module_id,
            latency: now.saturating_duration_since(tombstone.probe_started_at),
        })
    }

    #[cfg(test)]
    pub(crate) fn health_probe_tombstone_count(&self) -> Result<usize, ForwardingError> {
        Ok(self.read_inner()?.health_probe_tombstones.len())
    }

    pub(crate) fn module_endpoint_for_connection(
        &self,
        connection_id: ConnectionId,
    ) -> Result<Option<ModuleEndpointId>, ForwardingError> {
        Ok(self
            .read_inner()?
            .endpoint_by_connection
            .get(&connection_id)
            .copied())
    }

    /// Looks up the module registered on a data-plane connection so route-drop
    /// diagnostics name the emitter instead of only reporting a daemon total.
    pub(crate) fn module_id_for_connection(
        &self,
        connection_id: ConnectionId,
    ) -> Result<Option<String>, ForwardingError> {
        let inner = self.read_inner()?;
        Ok(inner
            .endpoint_by_connection
            .get(&connection_id)
            .and_then(|endpoint| inner.module_id_by_endpoint.get(endpoint))
            .cloned())
    }

    pub(crate) fn has_live_module_connection(
        &self,
        module_id: &str,
    ) -> Result<bool, ForwardingError> {
        Ok(self.read_inner()?.modules_by_id.contains_key(module_id))
    }

    pub(crate) fn lookup_data_route(
        &self,
        connection_id: ConnectionId,
        channel: u16,
        epoch: u32,
    ) -> Result<DataRoute, ForwardingError> {
        let inner = self.read_inner()?;
        let state = if let Some(endpoint) =
            inner.endpoint_by_connection.get(&connection_id).copied()
        {
            let key = ModuleRouteKey { endpoint, channel };
            match inner.module_to_client.get(&key) {
                Some(route) if route.module_epoch == epoch => {
                    DataRouteState::Bound(Arc::clone(route))
                }
                Some(_) => DataRouteState::EpochMismatch,
                None if inner.reserved_module.contains_key(&key)
                    && inner.module_slot_epochs.get(&key).copied() == Some(epoch) =>
                {
                    DataRouteState::Reserved
                }
                None if inner.reserved_module.contains_key(&key) => DataRouteState::EpochMismatch,
                None => DataRouteState::Absent,
            }
        } else {
            let key = ClientRouteKey {
                connection_id,
                channel,
            };
            match inner.client_to_module.get(&key) {
                Some(route) if route.client_epoch == epoch => {
                    DataRouteState::Bound(Arc::clone(route))
                }
                Some(_) => DataRouteState::EpochMismatch,
                None if inner.reserved_client.contains_key(&key)
                    && inner.client_slot_epochs.get(&key).copied() == Some(epoch) =>
                {
                    DataRouteState::Reserved
                }
                None if inner.reserved_client.contains_key(&key) => DataRouteState::EpochMismatch,
                None => DataRouteState::Absent,
            }
        };
        Ok(
            if inner.endpoint_by_connection.contains_key(&connection_id) {
                DataRoute::Module(state)
            } else {
                DataRoute::Client(state)
            },
        )
    }

    #[cfg(test)]
    pub(crate) fn inject_client_slot_epoch(
        &self,
        connection_id: ConnectionId,
        channel: u16,
        last_epoch: u32,
    ) {
        let mut inner = self.write_inner().expect("forwarding lock");
        inner.client_slot_epochs.insert(
            ClientRouteKey {
                connection_id,
                channel,
            },
            last_epoch,
        );
        inner.next_client_channel.insert(connection_id, channel);
    }

    #[cfg(test)]
    pub(crate) fn inject_module_slot_epoch(
        &self,
        endpoint: ModuleEndpointId,
        channel: u16,
        last_epoch: u32,
    ) {
        let mut inner = self.write_inner().expect("forwarding lock");
        inner
            .module_slot_epochs
            .insert(ModuleRouteKey { endpoint, channel }, last_epoch);
        inner.next_module_channel.insert(endpoint, channel);
    }

    #[cfg(test)]
    pub(crate) fn inject_control_corr(&self, endpoint: ModuleEndpointId, next_corr: u64) {
        self.write_inner()
            .expect("forwarding lock")
            .next_control_corr
            .insert(endpoint, next_corr);
    }

    pub(crate) fn cache_status(
        &self,
        endpoint: ModuleEndpointId,
        module_channel: u16,
        module_epoch: u32,
        status: String,
    ) -> Result<bool, ForwardingError> {
        let mut inner = self.write_inner()?;
        if !inner.module_id_by_endpoint.contains_key(&endpoint) {
            return Err(ForwardingError::StaleModuleEndpoint);
        }

        let module_key = ModuleRouteKey {
            endpoint,
            channel: module_channel,
        };
        let handle = if let Some(route) = inner.module_to_client.get(&module_key) {
            (route.module_epoch == module_epoch).then_some((
                ClientRouteKey {
                    connection_id: route.client_connection_id,
                    channel: route.client_channel,
                },
                route.client_epoch,
            ))
        } else if let Some(client_key) = inner.reserved_module.get(&module_key).copied() {
            (inner.module_slot_epochs.get(&module_key).copied() == Some(module_epoch)).then_some((
                client_key,
                inner
                    .client_slot_epochs
                    .get(&client_key)
                    .copied()
                    .unwrap_or(0),
            ))
        } else {
            None
        };

        if let Some(handle) = handle {
            inner.status.insert(handle, status);
            Ok(true)
        } else {
            debug!(
                module_channel,
                module_epoch,
                generation = endpoint.generation,
                connection_id = endpoint.connection_id.get(),
                "dropping stale status update for module route handle"
            );
            Ok(false)
        }
    }

    pub(crate) fn route_poll_snapshot(
        &self,
        client_connection_id: ConnectionId,
        client_channel: u16,
        client_epoch: u32,
    ) -> Result<RoutePollSnapshot, ForwardingError> {
        let inner = self.read_inner()?;
        let client_key = ClientRouteKey {
            connection_id: client_connection_id,
            channel: client_channel,
        };
        let Some(route) = inner.client_to_module.get(&client_key) else {
            return Ok(RoutePollSnapshot::Absent);
        };
        if route.client_epoch != client_epoch
            || !inner
                .module_id_by_endpoint
                .contains_key(&route.module_endpoint)
        {
            return Ok(RoutePollSnapshot::Absent);
        }
        Ok(RoutePollSnapshot::Bound {
            module_id: route.module_id.clone(),
            status: inner.status.get(&(client_key, client_epoch)).cloned(),
        })
    }

    pub fn active_binding_count(&self) -> Result<usize, ForwardingError> {
        Ok(self.read_inner()?.client_to_module.len())
    }

    /// How many distinct client connections hold at least one committed route,
    /// alongside the largest number of routes any single connection holds.
    ///
    /// `connected_clients` alone cannot distinguish many clients with a route
    /// each from one client accumulating hundreds, and those have opposite
    /// causes. Reading it required an out-of-band `lsof` during a live
    /// investigation, and the count of connections was mistaken for a count of
    /// client processes — which sent two of us after cleanup paths that were
    /// working correctly.
    pub fn client_route_concentration(&self) -> Result<(usize, usize), ForwardingError> {
        let inner = self.read_inner()?;
        let mut per_connection: HashMap<ConnectionId, usize> = HashMap::new();
        for key in inner.client_to_module.keys() {
            *per_connection.entry(key.connection_id).or_insert(0) += 1;
        }
        let max = per_connection.values().copied().max().unwrap_or(0);
        Ok((per_connection.len(), max))
    }

    pub fn has_route_channel(&self, route_channel: u16) -> Result<bool, ForwardingError> {
        let inner = self.read_inner()?;
        Ok(inner
            .client_to_module
            .keys()
            .any(|key| key.channel == route_channel))
    }

    pub(crate) fn begin_module_drain(
        &self,
        module_id: &str,
        reason: RouteCloseReason,
    ) -> Result<Option<ModuleDrainTarget>, ForwardingError> {
        let mut inner = self.write_inner()?;
        let Some(module) = inner.modules_by_id.get(module_id).cloned() else {
            return Ok(None);
        };
        let endpoint = module.endpoint;
        inner.draining_endpoints.insert(endpoint, reason);

        let flows = inner
            .client_to_module
            .values()
            .filter(|route| route.module_endpoint == endpoint)
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
        let mut abandoned_bindings = Vec::new();
        for key in pending_keys {
            let Some(pending) = inner.pending_relays.remove(&key) else {
                continue;
            };
            release_reserved_route_locked(
                &mut inner,
                pending.reservation.client_key,
                pending.reservation.module_key,
            );
            if pending.relay_enqueued {
                if let Some(target) = abandoned_route_target(&inner, &pending.reservation) {
                    abandoned_bindings.push(target);
                }
            }
            let _ = pending
                .sender
                .send(RouteBindRelayOutcome::Rejected(ErrorBody::new(
                    "module_reloading",
                    format!("module_id '{module_id}' is reloading"),
                )));
        }

        let pending_control_keys = inner
            .pending_control_rpcs
            .keys()
            .filter(|(pending_endpoint, _)| *pending_endpoint == endpoint)
            .copied()
            .collect::<Vec<_>>();
        for key in pending_control_keys {
            if let Some(pending) = inner.pending_control_rpcs.remove(&key) {
                let _ = pending
                    .sender
                    .send(ModuleControlRpcOutcome::ModuleGone(format!(
                        "module '{module_id}' began draining during module-control RPC"
                    )));
            }
        }

        Ok(Some(ModuleDrainTarget {
            endpoint,
            sink: module.sink,
            negotiated_ver: module.negotiated_ver,
            abandoned_bindings,
        }))
    }

    pub(crate) fn endpoint_in_flight_count(
        &self,
        endpoint: ModuleEndpointId,
    ) -> Result<usize, ForwardingError> {
        let inner = self.read_inner()?;
        Ok(inner
            .client_to_module
            .values()
            .filter(|route| route.module_endpoint == endpoint)
            .map(|route| route.flow.in_flight())
            .sum())
    }

    pub(crate) fn endpoint_is_draining(
        &self,
        endpoint: ModuleEndpointId,
    ) -> Result<bool, ForwardingError> {
        Ok(self
            .read_inner()?
            .draining_endpoints
            .contains_key(&endpoint))
    }

    pub(crate) fn module_is_draining(&self, module_id: &str) -> Result<bool, ForwardingError> {
        let inner = self.read_inner()?;
        Ok(inner
            .modules_by_id
            .get(module_id)
            .is_some_and(|module| inner.draining_endpoints.contains_key(&module.endpoint)))
    }

    pub(crate) fn release_module_endpoint_routes(
        &self,
        endpoint: ModuleEndpointId,
    ) -> Result<Vec<GoodbyeTarget>, ForwardingError> {
        let mut inner = self.write_inner()?;
        let routes = inner
            .module_to_client
            .iter()
            .filter(|(module_key, _)| module_key.endpoint == endpoint)
            .map(|(module_key, route)| (*module_key, route.module_epoch))
            .collect::<Vec<_>>();
        let mut released = Vec::with_capacity(routes.len());
        for (module_key, epoch) in routes {
            if let RouteRelease::Removed(target) =
                release_module_route_locked(&mut inner, module_key, epoch)
            {
                released.push(target);
            }
        }
        Ok(released)
    }

    /// Enumerate one endpoint's current routes without contacting the module.
    ///
    /// The read lock makes this safe while the endpoint drains: the returned
    /// `draining` marker describes the same table state that owns the route,
    /// rather than inferring liveness from a module that may be stopping.
    pub(crate) fn endpoint_routes(
        &self,
        endpoint: ModuleEndpointId,
    ) -> Result<Vec<EndpointRoute>, ForwardingError> {
        let inner = self.read_inner()?;
        Ok(endpoint_routes_locked(&inner, endpoint))
    }

    /// Snapshot all live endpoint route sets under one forwarding-table read lock.
    pub(crate) fn route_census(
        &self,
        module_id: Option<&str>,
    ) -> Result<Vec<(String, Vec<EndpointRoute>)>, ForwardingError> {
        let inner = self.read_inner()?;
        let mut endpoints = inner
            .modules_by_id
            .iter()
            .filter(|(id, _)| module_id.is_none_or(|requested| requested == id.as_str()))
            .map(|(id, module)| (id.clone(), module.endpoint))
            .collect::<Vec<_>>();
        endpoints.sort_by(|left, right| left.0.cmp(&right.0));
        Ok(endpoints
            .into_iter()
            .map(|(id, endpoint)| (id, endpoint_routes_locked(&inner, endpoint)))
            .collect())
    }

    /// True if this connection already owns committed or reserved CLIENT routes.
    /// A module registers (HELLO) before serving and never opens client routes, so
    /// a connection that has client routes must not also become a module endpoint
    /// — otherwise one connection holds both client and module state and cleanup
    /// only releases one side.
    pub(crate) fn connection_has_client_routes(
        &self,
        connection_id: ConnectionId,
    ) -> Result<bool, ForwardingError> {
        let inner = self.read_inner()?;
        let has = inner
            .client_to_module
            .keys()
            .any(|key| key.connection_id == connection_id)
            || inner
                .reserved_client
                .keys()
                .any(|key| key.connection_id == connection_id);
        Ok(has)
    }

    pub(crate) fn cleanup_connection(
        &self,
        connection_id: ConnectionId,
    ) -> Result<Vec<GoodbyeTarget>, ForwardingError> {
        let mut inner = self.write_inner()?;
        inner.closing_connections.insert(connection_id);
        if let Some(endpoint) = inner.endpoint_by_connection.remove(&connection_id) {
            return Ok(remove_module_connection_locked(&mut inner, endpoint));
        }

        let routes = inner
            .client_to_module
            .iter()
            .filter(|(key, _)| key.connection_id == connection_id)
            .map(|(key, route)| (*key, route.client_epoch))
            .collect::<Vec<_>>();
        let mut released = Vec::with_capacity(routes.len());
        for (client_key, epoch) in routes {
            if let RouteRelease::Removed(target) =
                release_client_route_locked(&mut inner, client_key, epoch)
            {
                released.push(target);
            }
        }

        let pending_keys = inner
            .pending_relays
            .iter()
            .filter(|(_, pending)| pending.reservation.client_key.connection_id == connection_id)
            .map(|(key, _)| *key)
            .collect::<Vec<_>>();
        for key in pending_keys {
            let Some(pending) = inner.pending_relays.remove(&key) else {
                continue;
            };
            release_reserved_route_locked(
                &mut inner,
                pending.reservation.client_key,
                pending.reservation.module_key,
            );
            if pending.relay_enqueued {
                if let Some(target) = abandoned_route_target(&inner, &pending.reservation) {
                    released.push(target);
                }
            }
            let _ = pending.sender.send(RouteBindRelayOutcome::ModuleGone(
                "client connection closed during route.bind relay".to_string(),
            ));
        }

        let orphaned = inner
            .reserved_client
            .iter()
            .filter(|(key, _)| key.connection_id == connection_id)
            .map(|(client, module)| (*client, *module))
            .collect::<Vec<_>>();
        for (client_key, module_key) in orphaned {
            release_reserved_route_locked(&mut inner, client_key, module_key);
        }
        inner.next_client_channel.remove(&connection_id);
        inner
            .client_slot_epochs
            .retain(|key, _| key.connection_id != connection_id);
        inner
            .last_published_epoch
            .retain(|key, _| key.connection_id != connection_id);
        inner
            .status
            .retain(|(key, _), _| key.connection_id != connection_id);

        Ok(released)
    }

    pub(crate) fn escalate_client_delivery_failure(
        &self,
        connection_id: ConnectionId,
        channel: u16,
        expected_epoch: u32,
        reason: CloseReason,
    ) -> Result<bool, ForwardingError> {
        let should_close = {
            let mut inner = self.write_inner()?;
            let key = ClientRouteKey {
                connection_id,
                channel,
            };
            if inner.last_published_epoch.get(&key).copied() != Some(expected_epoch) {
                false
            } else {
                inner.closing_connections.insert(connection_id);
                true
            }
        };
        if should_close {
            self.request_connection_close(connection_id, reason);
        }
        Ok(should_close)
    }

    fn record_route_release(&self, release: &RouteRelease) {
        match release {
            RouteRelease::Removed(_) => self.counters.increment_route_released_epoch_fenced(),
            RouteRelease::Stale => self.counters.increment_route_release_stale_skipped(),
            RouteRelease::Absent => {}
        }
    }

    fn read_inner(&self) -> Result<RwLockReadGuard<'_, ForwardingInner>, ForwardingError> {
        self.inner.read().map_err(|_| ForwardingError::Poisoned)
    }

    fn write_inner(&self) -> Result<RwLockWriteGuard<'_, ForwardingInner>, ForwardingError> {
        self.inner.write().map_err(|_| ForwardingError::Poisoned)
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
    fn allocate_route_slots(
        &mut self,
        connection_id: ConnectionId,
        endpoint: ModuleEndpointId,
    ) -> Result<(u16, u32, u16, u32), ForwardingError> {
        let client_start = *self.next_client_channel.entry(connection_id).or_insert(1);
        let mut client_channel = client_start;
        let client_channel = loop {
            let key = ClientRouteKey {
                connection_id,
                channel: client_channel,
            };
            let eligible = !self.client_to_module.contains_key(&key)
                && !self.reserved_client.contains_key(&key)
                && self.client_slot_epochs.get(&key).copied().unwrap_or(0) < u32::MAX;
            if eligible {
                break client_channel;
            }
            client_channel = next_channel(client_channel);
            if client_channel == client_start {
                return Err(ForwardingError::ClientRouteChannelExhausted { connection_id });
            }
        };

        let module_start = *self.next_module_channel.entry(endpoint).or_insert(1);
        let mut module_channel = module_start;
        let module_channel = loop {
            let key = ModuleRouteKey {
                endpoint,
                channel: module_channel,
            };
            let eligible = !self.module_to_client.contains_key(&key)
                && !self.reserved_module.contains_key(&key)
                && self.module_slot_epochs.get(&key).copied().unwrap_or(0) < u32::MAX;
            if eligible {
                break module_channel;
            }
            module_channel = next_channel(module_channel);
            if module_channel == module_start {
                return Err(ForwardingError::ModuleRouteChannelExhausted { endpoint });
            }
        };

        let client_key = ClientRouteKey {
            connection_id,
            channel: client_channel,
        };
        let module_key = ModuleRouteKey {
            endpoint,
            channel: module_channel,
        };
        let client_epoch = self
            .client_slot_epochs
            .get(&client_key)
            .copied()
            .unwrap_or(0)
            + 1;
        let module_epoch = self
            .module_slot_epochs
            .get(&module_key)
            .copied()
            .unwrap_or(0)
            + 1;
        self.client_slot_epochs.insert(client_key, client_epoch);
        self.module_slot_epochs.insert(module_key, module_epoch);
        self.next_client_channel
            .insert(connection_id, next_channel(client_channel));
        self.next_module_channel
            .insert(endpoint, next_channel(module_channel));
        Ok((client_channel, client_epoch, module_channel, module_epoch))
    }

    fn allocate_control_corr(
        &mut self,
        endpoint: ModuleEndpointId,
    ) -> Result<u64, ForwardingError> {
        let candidate = self.next_control_corr.get(&endpoint).copied().unwrap_or(1);
        if candidate == 0 {
            self.closing_connections.insert(endpoint.connection_id);
            return Err(ForwardingError::RelayCorrelationExhausted);
        }
        self.next_control_corr.insert(
            endpoint,
            if candidate == u64::MAX {
                0
            } else {
                candidate + 1
            },
        );
        Ok(candidate)
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

fn endpoint_routes_locked(
    inner: &ForwardingInner,
    endpoint: ModuleEndpointId,
) -> Vec<EndpointRoute> {
    let drain_reason = inner.draining_endpoints.get(&endpoint).copied();
    let draining = drain_reason.is_some();
    let mut routes = inner
        .module_to_client
        .iter()
        .filter(|(module_key, _)| module_key.endpoint == endpoint)
        .map(|(_, route)| EndpointRoute {
            goodbye_target: GoodbyeTarget {
                connection_id: route.client_connection_id,
                sink: route.client_sink.clone(),
                negotiated_ver: route.client_negotiated_ver,
                channel: route.client_channel,
                epoch: route.client_epoch,
                kind: GoodbyeTargetKind::Client,
                module_id: None,
            },
            principal: route.principal.clone(),
            bound_at: route.bound_at,
            draining,
            drain_reason,
        })
        .collect::<Vec<_>>();
    routes.sort_by_key(|route| {
        (
            route.goodbye_target.connection_id.get(),
            route.goodbye_target.channel,
            route.goodbye_target.epoch,
        )
    });
    routes
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
    inner.status.retain(|(key, _), _| *key != client_key);
}

fn release_client_route_locked(
    inner: &mut ForwardingInner,
    client_key: ClientRouteKey,
    expected_epoch: u32,
) -> RouteRelease {
    let Some(route) = inner.client_to_module.get(&client_key) else {
        return RouteRelease::Absent;
    };
    if route.client_epoch != expected_epoch {
        return RouteRelease::Stale;
    }
    let route = inner
        .client_to_module
        .remove(&client_key)
        .expect("route checked under the same forwarding lock");
    route.flow.close();
    inner.module_to_client.remove(&ModuleRouteKey {
        endpoint: route.module_endpoint,
        channel: route.module_channel,
    });
    inner.status.remove(&(client_key, expected_epoch));
    RouteRelease::Removed(GoodbyeTarget {
        connection_id: route.module_endpoint.connection_id,
        sink: route.module_sink.clone(),
        negotiated_ver: route.module_negotiated_ver,
        channel: route.module_channel,
        epoch: route.module_epoch,
        kind: GoodbyeTargetKind::Module,
        module_id: Some(route.module_id.clone()),
    })
}

fn release_module_route_locked(
    inner: &mut ForwardingInner,
    module_key: ModuleRouteKey,
    expected_epoch: u32,
) -> RouteRelease {
    let Some(route) = inner.module_to_client.get(&module_key) else {
        return RouteRelease::Absent;
    };
    if route.module_epoch != expected_epoch {
        return RouteRelease::Stale;
    }
    let route = inner
        .module_to_client
        .remove(&module_key)
        .expect("route checked under the same forwarding lock");
    route.flow.close();
    let client_key = ClientRouteKey {
        connection_id: route.client_connection_id,
        channel: route.client_channel,
    };
    inner.client_to_module.remove(&client_key);
    inner.status.remove(&(client_key, route.client_epoch));
    RouteRelease::Removed(GoodbyeTarget {
        connection_id: route.client_connection_id,
        sink: route.client_sink.clone(),
        negotiated_ver: route.client_negotiated_ver,
        channel: route.client_channel,
        epoch: route.client_epoch,
        kind: GoodbyeTargetKind::Client,
        module_id: None,
    })
}

fn commit_route_locked(
    inner: &mut ForwardingInner,
    pending: PendingRouteBindRelayEntry,
) -> Result<Option<GoodbyeTarget>, ForwardingError> {
    let reservation = pending.reservation;
    if inner
        .closing_connections
        .contains(&reservation.client_key.connection_id)
    {
        return Err(ForwardingError::ConnectionClosing {
            connection_id: reservation.client_key.connection_id,
        });
    }
    let module_id = inner
        .module_id_by_endpoint
        .get(&reservation.module_key.endpoint)
        .cloned()
        .ok_or(ForwardingError::StaleModuleEndpoint)?;
    if inner
        .draining_endpoints
        .contains_key(&reservation.module_key.endpoint)
    {
        return Err(ForwardingError::ModuleReloading { module_id });
    }
    if inner.reserved_client.remove(&reservation.client_key) != Some(reservation.module_key)
        || inner.reserved_module.remove(&reservation.module_key) != Some(reservation.client_key)
    {
        return Err(ForwardingError::UnknownReservation {
            client_channel: reservation.client_key.channel,
            module_channel: reservation.module_key.channel,
        });
    }
    let module = inner
        .modules_by_id
        .get(&module_id)
        .filter(|module| module.endpoint == reservation.module_key.endpoint)
        .cloned()
        .ok_or(ForwardingError::StaleModuleEndpoint)?;
    let binding = Arc::new(RouteBinding {
        client_connection_id: reservation.client_key.connection_id,
        client_sink: pending.client_sink,
        client_negotiated_ver: pending.client_negotiated_ver,
        client_channel: reservation.client_key.channel,
        client_epoch: reservation.client_epoch,
        module_id,
        module_endpoint: reservation.module_key.endpoint,
        module_sink: module.sink,
        module_negotiated_ver: module.negotiated_ver,
        module_channel: reservation.module_key.channel,
        module_epoch: reservation.module_epoch,
        principal: pending.principal,
        bound_at: Instant::now(),
        flow: Arc::new(ChannelFlow::new(window_for(&module.concurrency))),
    });
    inner
        .client_to_module
        .insert(reservation.client_key, Arc::clone(&binding));
    inner
        .module_to_client
        .insert(reservation.module_key, binding);
    let previous_published = inner
        .last_published_epoch
        .insert(reservation.client_key, reservation.client_epoch);

    // OwnedPermit::send cannot fail, but its returned sender reveals a receiver
    // that closed after reservation and before this locked publication point.
    // Stamp at publication: the permit was reserved earlier, but queue residency
    // for the reply-write diagnosis starts when the frame actually enters the queue.
    let client_sender = pending.client_permit.send(crate::router::OutboundFrame {
        frame: pending.route_open_frame,
        enqueued_at: std::time::Instant::now(),
    });
    if client_sender.is_closed() {
        let abandoned = pending
            .relay_enqueued
            .then(|| abandoned_route_target(inner, &reservation))
            .flatten();
        if let Some(route) = inner.client_to_module.remove(&reservation.client_key) {
            route.flow.close();
        }
        inner.module_to_client.remove(&reservation.module_key);
        inner
            .status
            .remove(&(reservation.client_key, reservation.client_epoch));
        match previous_published {
            Some(epoch) => {
                inner
                    .last_published_epoch
                    .insert(reservation.client_key, epoch);
            }
            None => {
                inner.last_published_epoch.remove(&reservation.client_key);
            }
        }
        let _ = pending.sender.send(RouteBindRelayOutcome::ModuleGone(
            "client egress closed during route publication".to_string(),
        ));
        return Ok(abandoned);
    }

    let _ = pending.sender.send(RouteBindRelayOutcome::Accepted);
    Ok(None)
}

fn abandoned_route_target(
    inner: &ForwardingInner,
    reservation: &RouteReservation,
) -> Option<GoodbyeTarget> {
    let module_id = inner
        .module_id_by_endpoint
        .get(&reservation.module_key.endpoint)?;
    let module = inner.modules_by_id.get(module_id)?;
    (module.endpoint == reservation.module_key.endpoint).then(|| GoodbyeTarget {
        connection_id: module.endpoint.connection_id,
        sink: module.sink.clone(),
        negotiated_ver: module.negotiated_ver,
        channel: reservation.module_key.channel,
        epoch: reservation.module_epoch,
        kind: GoodbyeTargetKind::Module,
        module_id: Some(module_id.clone()),
    })
}

fn remove_module_connection_locked(
    inner: &mut ForwardingInner,
    endpoint: ModuleEndpointId,
) -> Vec<GoodbyeTarget> {
    inner.draining_endpoints.remove(&endpoint);
    let module_id = inner.module_id_by_endpoint.remove(&endpoint);
    if let Some(module_id) = module_id.as_ref() {
        if inner
            .modules_by_id
            .get(module_id)
            .is_some_and(|module| module.endpoint == endpoint)
        {
            inner.modules_by_id.remove(module_id);
        }
    }
    inner.endpoint_by_connection.remove(&endpoint.connection_id);
    inner.next_module_channel.remove(&endpoint);
    inner.next_control_corr.remove(&endpoint);
    inner
        .health_probe_tombstones
        .retain(|(pending_endpoint, _), _| *pending_endpoint != endpoint);
    inner
        .module_slot_epochs
        .retain(|key, _| key.endpoint != endpoint);
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
    for pending in pending {
        let module_label = module_id.as_deref().unwrap_or("unknown");
        let _ = pending
            .sender
            .send(RouteBindRelayOutcome::ModuleGone(format!(
                "module '{module_label}' connection closed during route.bind relay"
            )));
    }

    let pending_control_keys: Vec<_> = inner
        .pending_control_rpcs
        .keys()
        .filter(|(pending_endpoint, _)| *pending_endpoint == endpoint)
        .copied()
        .collect();
    let pending_control: Vec<_> = pending_control_keys
        .into_iter()
        .filter_map(|key| inner.pending_control_rpcs.remove(&key))
        .collect();
    for pending in pending_control {
        let module_label = module_id.as_deref().unwrap_or("unknown");
        let _ = pending
            .sender
            .send(ModuleControlRpcOutcome::ModuleGone(format!(
                "module '{module_label}' connection closed during module-control RPC"
            )));
    }

    let module_routes = inner
        .module_to_client
        .iter()
        .filter(|(module_key, _)| module_key.endpoint == endpoint)
        .map(|(module_key, route)| (*module_key, route.module_epoch))
        .collect::<Vec<_>>();
    let mut released = Vec::with_capacity(module_routes.len());
    for (module_key, epoch) in module_routes {
        if let RouteRelease::Removed(target) = release_module_route_locked(inner, module_key, epoch)
        {
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
    pub(crate) fn new(window: usize) -> Self {
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

    #[cfg(test)]
    pub(crate) fn available_permits(&self) -> usize {
        self.sem.available_permits()
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
    ConnectionClosing {
        connection_id: ConnectionId,
    },
    ClientEgressClosed {
        connection_id: ConnectionId,
    },
    RouteOpenBuild(String),
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
                write!(f, "module control correlation ids are exhausted")
            }
            Self::ConnectionClosing { connection_id } => write!(
                f,
                "connection {} is closing and cannot accept route allocation",
                connection_id.get()
            ),
            Self::ClientEgressClosed { connection_id } => write!(
                f,
                "client connection {} egress is closed",
                connection_id.get()
            ),
            Self::RouteOpenBuild(message) => {
                write!(f, "failed to prebuild route.open response: {message}")
            }
            Self::Poisoned => write!(f, "forwarding table lock was poisoned"),
        }
    }
}

impl Error for ForwardingError {}

#[cfg(test)]
mod tests {
    use std::time::Duration;

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
            let mut inner = forwarding.inner.write().unwrap();
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

        let (exhausted_tx, _exhausted_rx) = mpsc::channel(1);
        let err = forwarding
            .begin_route_bind_relay_for_test(
                exhausted_client,
                FrameSink::new(exhausted_tx),
                1,
                "route-limit-provider",
            )
            .unwrap_err();
        assert!(matches!(
            err,
            ForwardingError::ClientRouteChannelExhausted { connection_id }
                if connection_id == exhausted_client
        ));

        let (second_tx, _second_rx) = mpsc::channel(1);
        let pending = forwarding
            .begin_route_bind_relay_for_test(
                second_client,
                FrameSink::new(second_tx),
                2,
                "route-limit-provider",
            )
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

        let (client_tx, _client_rx) = mpsc::channel(1);
        let client_sink = FrameSink::new(client_tx);
        let mut wrapped_channel = None;
        for index in 0..=usize::from(u16::MAX) {
            let pending = forwarding
                .begin_route_bind_relay_for_test(
                    client,
                    client_sink.clone(),
                    index as u64 + 1,
                    "slot-reuse-provider",
                )
                .unwrap();
            if index == usize::from(u16::MAX) {
                wrapped_channel = Some(pending.module_channel);
            }
            forwarding
                .abort_pending_relay(
                    pending.endpoint,
                    pending.corr,
                    RouteBindRelayOutcome::ModuleGone("test abort".to_string()),
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
            .write()
            .unwrap()
            .next_client_channel
            .insert(client, 41);

        let released = forwarding.cleanup_connection(client).unwrap();

        assert!(released.is_empty());
        assert!(!forwarding
            .inner
            .read()
            .unwrap()
            .next_client_channel
            .contains_key(&client));
    }

    #[test]
    fn stale_module_cleanup_preserves_fast_reconnect_successor() {
        let forwarding = ForwardingTable::default();
        let module_id = "fast-reconnect-provider";
        let first_connection = ConnectionId::new(70);
        let second_connection = ConnectionId::new(80);
        let (first_tx, _first_rx) = mpsc::channel(1);
        let first_endpoint = forwarding
            .register_module_connection(
                first_connection,
                module_id.to_string(),
                1,
                Concurrency::ModuleManaged,
                FrameSink::new(first_tx),
            )
            .unwrap();
        let (second_tx, _second_rx) = mpsc::channel(1);
        let second_endpoint = forwarding
            .register_module_connection(
                second_connection,
                module_id.to_string(),
                1,
                Concurrency::ModuleManaged,
                FrameSink::new(second_tx),
            )
            .unwrap();
        assert_ne!(first_endpoint, second_endpoint);

        let released = forwarding.cleanup_connection(first_connection).unwrap();

        assert!(released.is_empty());
        assert_eq!(
            forwarding
                .inner
                .read()
                .unwrap()
                .modules_by_id
                .get(module_id)
                .map(|module| module.endpoint),
            Some(second_endpoint)
        );
        assert!(forwarding.has_live_module_connection(module_id).unwrap());
        let control_rpc = forwarding
            .begin_module_control_rpc_for(
                module_id,
                "health.check",
                Instant::now() + Duration::from_secs(1),
            )
            .unwrap();
        assert_eq!(control_rpc.endpoint, second_endpoint);
    }

    fn route_fixture(
        module_id: &str,
    ) -> (
        ForwardingTable,
        ConnectionId,
        ModuleEndpointId,
        ConnectionId,
        FrameSink,
        mpsc::Receiver<crate::router::OutboundFrame>,
    ) {
        let forwarding = ForwardingTable::default();
        let module_connection = ConnectionId::new(100);
        let client_connection = ConnectionId::new(200);
        let (module_tx, _module_rx) = mpsc::channel(8);
        let endpoint = forwarding
            .register_module_connection(
                module_connection,
                module_id.to_string(),
                2,
                Concurrency::ModuleManaged,
                FrameSink::new(module_tx),
            )
            .unwrap();
        let (client_tx, client_rx) = mpsc::channel(8);
        (
            forwarding,
            module_connection,
            endpoint,
            client_connection,
            FrameSink::new(client_tx),
            client_rx,
        )
    }

    fn test_ping(corr: u64) -> Frame {
        Frame::build(
            FrameType::Ping,
            Flags::new(false, Priority::Passive, false),
            0,
            0,
            corr,
            Vec::new(),
        )
        .unwrap()
    }

    fn begin_test_route(
        forwarding: &ForwardingTable,
        client_connection: ConnectionId,
        client_sink: FrameSink,
        corr: u64,
        module_id: &str,
    ) -> PendingRouteBindRelay {
        forwarding
            .begin_route_bind_relay_for_test(client_connection, client_sink, corr, module_id)
            .unwrap()
    }

    #[test]
    fn endpoint_routes_keep_goodbye_targets_and_mark_draining_routes() {
        let (forwarding, module_connection, endpoint, client, sink, _client_rx) =
            route_fixture("census");
        let pending = begin_test_route(&forwarding, client, sink, 1, "census");
        forwarding
            .complete_pending_relay(
                module_connection,
                pending.corr,
                RouteBindRelayOutcome::Accepted,
            )
            .unwrap();

        let routes = forwarding.endpoint_routes(endpoint).unwrap();
        assert_eq!(routes.len(), 1);
        assert!(matches!(routes[0].principal, Principal::Direct));
        assert_eq!(routes[0].goodbye_target.connection_id, client);
        assert_eq!(routes[0].goodbye_target.channel, pending.client_channel);
        assert_eq!(routes[0].goodbye_target.epoch, pending.client_epoch);
        assert!(!routes[0].draining);

        forwarding
            .begin_module_drain("census", RouteCloseReason::Restart)
            .unwrap();
        let draining_routes = forwarding.endpoint_routes(endpoint).unwrap();
        assert_eq!(draining_routes.len(), 1);
        assert!(draining_routes[0].draining);
    }

    #[test]
    fn aborted_reservation_consumes_both_epochs_and_reuse_advances_them() {
        let (forwarding, _, endpoint, client, sink, _client_rx) = route_fixture("epoch-abort");
        let first = begin_test_route(&forwarding, client, sink.clone(), 1, "epoch-abort");
        assert_eq!((first.client_epoch, first.module_epoch), (1, 1));
        forwarding
            .abort_pending_relay(
                first.endpoint,
                first.corr,
                RouteBindRelayOutcome::ModuleGone("abort".into()),
            )
            .unwrap();
        forwarding.inject_client_slot_epoch(client, first.client_channel, first.client_epoch);
        forwarding.inject_module_slot_epoch(endpoint, first.module_channel, first.module_epoch);

        let second = begin_test_route(&forwarding, client, sink, 2, "epoch-abort");
        assert_eq!(second.client_channel, first.client_channel);
        assert_eq!(second.module_channel, first.module_channel);
        assert_eq!((second.client_epoch, second.module_epoch), (2, 2));
    }

    #[test]
    fn stale_release_cannot_remove_reused_successor_and_status_is_epoch_fenced() {
        let (forwarding, module_connection, endpoint, client, sink, mut client_rx) =
            route_fixture("epoch-release");
        let first = begin_test_route(&forwarding, client, sink.clone(), 10, "epoch-release");
        forwarding
            .complete_pending_relay(
                module_connection,
                first.corr,
                RouteBindRelayOutcome::Accepted,
            )
            .unwrap();
        assert_eq!(client_rx.try_recv().unwrap().header.corr, 10);
        assert!(matches!(
            forwarding
                .release_client_route(client, first.client_channel, first.client_epoch)
                .unwrap(),
            RouteRelease::Removed(_)
        ));
        forwarding.inject_client_slot_epoch(client, first.client_channel, first.client_epoch);
        forwarding.inject_module_slot_epoch(endpoint, first.module_channel, first.module_epoch);

        let second = begin_test_route(&forwarding, client, sink, 11, "epoch-release");
        forwarding
            .complete_pending_relay(
                module_connection,
                second.corr,
                RouteBindRelayOutcome::Accepted,
            )
            .unwrap();
        assert_eq!(client_rx.try_recv().unwrap().header.corr, 11);
        assert!(matches!(
            forwarding
                .release_client_route(client, second.client_channel, first.client_epoch)
                .unwrap(),
            RouteRelease::Stale
        ));
        assert!(!forwarding
            .cache_status(
                endpoint,
                second.module_channel,
                first.module_epoch,
                "stale".into(),
            )
            .unwrap());
        assert!(forwarding
            .cache_status(
                endpoint,
                second.module_channel,
                second.module_epoch,
                "current".into(),
            )
            .unwrap());
        match forwarding
            .route_poll_snapshot(client, second.client_channel, second.client_epoch)
            .unwrap()
        {
            RoutePollSnapshot::Bound { status, .. } => {
                assert_eq!(status.as_deref(), Some("current"));
            }
            RoutePollSnapshot::Absent => panic!("successor binding was removed"),
        }
        let counters = forwarding.counters().snapshot();
        assert_eq!(counters["route_released_epoch_fenced"], 1);
        assert_eq!(counters["route_release_stale_skipped"], 1);
    }

    #[test]
    fn max_epoch_reservation_retires_only_that_slot() {
        let (forwarding, _, endpoint, client, sink, _client_rx) = route_fixture("epoch-max");
        forwarding.inject_client_slot_epoch(client, 7, u32::MAX - 1);
        forwarding.inject_module_slot_epoch(endpoint, 9, u32::MAX - 1);
        let final_use = begin_test_route(&forwarding, client, sink.clone(), 20, "epoch-max");
        assert_eq!(
            (final_use.client_channel, final_use.client_epoch),
            (7, u32::MAX)
        );
        assert_eq!(
            (final_use.module_channel, final_use.module_epoch),
            (9, u32::MAX)
        );
        forwarding
            .abort_pending_relay(
                endpoint,
                final_use.corr,
                RouteBindRelayOutcome::ModuleGone("abort".into()),
            )
            .unwrap();
        forwarding.inject_client_slot_epoch(client, 7, u32::MAX);
        forwarding.inject_module_slot_epoch(endpoint, 9, u32::MAX);
        let next = begin_test_route(&forwarding, client, sink, 21, "epoch-max");
        assert_ne!(next.client_channel, 7);
        assert_ne!(next.module_channel, 9);
        assert_eq!((next.client_epoch, next.module_epoch), (1, 1));
    }

    #[test]
    fn bind_and_module_control_share_monotonic_corr_and_deadline_arbitration() {
        let (forwarding, module_connection, endpoint, client, sink, _client_rx) =
            route_fixture("corr-shared");
        let bind = begin_test_route(&forwarding, client, sink, 30, "corr-shared");
        assert_eq!(bind.corr, 1);
        forwarding
            .abort_pending_relay(
                endpoint,
                bind.corr,
                RouteBindRelayOutcome::ModuleGone("abort".into()),
            )
            .unwrap();
        let rpc = forwarding
            .begin_module_control_rpc_for(
                "corr-shared",
                "health.check",
                Instant::now() - Duration::from_millis(1),
            )
            .unwrap();
        assert_eq!(rpc.corr, 2);
        assert_eq!(
            forwarding
                .complete_module_control_rpc(
                    module_connection,
                    rpc.corr,
                    Some("health.check"),
                    ModuleControlRpcOutcome::Response(ModuleControlResponse::HealthCheck {
                        status: subc_protocol::session::HealthStatus::Ok,
                        detail: None,
                        metrics: None,
                    }),
                )
                .unwrap(),
            ModuleControlRpcCompletion::Settled
        );
        assert!(matches!(
            rpc.receiver.blocking_recv().unwrap(),
            ModuleControlRpcOutcome::DeadlineElapsed
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn health_probe_tombstone_ttl_removes_an_endpoint_that_stops_probing() {
        let (forwarding, _, endpoint, _, _, _) = route_fixture("tombstone-ttl");
        let probe_started_at = Instant::now();
        let rpc = forwarding
            .begin_health_probe_rpc_for(
                "tombstone-ttl",
                "health.check",
                probe_started_at,
                probe_started_at + Duration::from_secs(5),
            )
            .unwrap();
        assert!(forwarding
            .tombstone_health_probe_rpc(endpoint, rpc.corr)
            .unwrap());
        assert_eq!(forwarding.health_probe_tombstone_count().unwrap(), 1);

        tokio::time::advance(HEALTH_PROBE_TOMBSTONE_TTL).await;
        tokio::task::yield_now().await;

        assert_eq!(forwarding.health_probe_tombstone_count().unwrap(), 0);
    }

    #[test]
    fn correlation_exhaustion_emits_max_once_then_closes_endpoint() {
        let (forwarding, _, endpoint, _, _, _) = route_fixture("corr-max");
        let mut close = forwarding.register_connection_close(endpoint.connection_id);
        forwarding.inject_control_corr(endpoint, u64::MAX);
        let final_rpc = forwarding
            .begin_module_control_rpc_for(
                "corr-max",
                "health.check",
                Instant::now() + Duration::from_secs(1),
            )
            .unwrap();
        assert_eq!(final_rpc.corr, u64::MAX);
        forwarding
            .cancel_module_control_rpc(endpoint, final_rpc.corr)
            .unwrap();
        assert!(matches!(
            forwarding.begin_module_control_rpc_for(
                "corr-max",
                "health.check",
                Instant::now() + Duration::from_secs(1),
            ),
            Err(ForwardingError::RelayCorrelationExhausted)
        ));
        assert!(close.try_recv().is_ok());
    }

    #[test]
    fn publication_epoch_controls_delivery_failure_escalation() {
        fn setup_successor(
            commit_successor: Option<bool>,
        ) -> (ForwardingTable, ConnectionId, u16, u32) {
            let (forwarding, module_connection, endpoint, client, sink, mut client_rx) =
                route_fixture("escalation");
            let first = begin_test_route(&forwarding, client, sink.clone(), 40, "escalation");
            forwarding
                .complete_pending_relay(
                    module_connection,
                    first.corr,
                    RouteBindRelayOutcome::Accepted,
                )
                .unwrap();
            client_rx.try_recv().unwrap();
            assert!(matches!(
                forwarding
                    .release_client_route(client, first.client_channel, first.client_epoch)
                    .unwrap(),
                RouteRelease::Removed(_)
            ));
            if let Some(commit_successor) = commit_successor {
                forwarding.inject_client_slot_epoch(
                    client,
                    first.client_channel,
                    first.client_epoch,
                );
                forwarding.inject_module_slot_epoch(
                    endpoint,
                    first.module_channel,
                    first.module_epoch,
                );
                let successor = begin_test_route(&forwarding, client, sink, 41, "escalation");
                if commit_successor {
                    forwarding
                        .complete_pending_relay(
                            module_connection,
                            successor.corr,
                            RouteBindRelayOutcome::Accepted,
                        )
                        .unwrap();
                    client_rx.try_recv().unwrap();
                } else {
                    forwarding
                        .abort_pending_relay(
                            endpoint,
                            successor.corr,
                            RouteBindRelayOutcome::ModuleGone("abort".into()),
                        )
                        .unwrap();
                }
            }
            (forwarding, client, first.client_channel, first.client_epoch)
        }

        let (no_successor, client, channel, epoch) = setup_successor(None);
        let mut close = no_successor.register_connection_close(client);
        assert!(no_successor
            .escalate_client_delivery_failure(
                client,
                channel,
                epoch,
                CloseReason::new("delivery", "failed"),
            )
            .unwrap());
        assert!(close.try_recv().is_ok());

        let (aborted, client, channel, epoch) = setup_successor(Some(false));
        let mut close = aborted.register_connection_close(client);
        assert!(aborted
            .escalate_client_delivery_failure(
                client,
                channel,
                epoch,
                CloseReason::new("delivery", "failed"),
            )
            .unwrap());
        assert!(close.try_recv().is_ok());

        let (published, client, channel, epoch) = setup_successor(Some(true));
        let mut close = published.register_connection_close(client);
        assert!(!published
            .escalate_client_delivery_failure(
                client,
                channel,
                epoch,
                CloseReason::new("delivery", "stale failure"),
            )
            .unwrap());
        assert!(close.try_recv().is_err());
    }

    #[test]
    fn route_concentration_separates_client_count_from_routes_per_client() {
        // The distinction this asserts is the one a bare connection count cannot
        // make: two connections holding one route each and one connection
        // holding two are the same total, and have opposite causes.
        let (forwarding, module_connection, _, client, sink, _client_rx) =
            route_fixture("concentration");
        assert_eq!(forwarding.client_route_concentration().unwrap(), (0, 0));

        for corr in [70_u64, 71] {
            let pending =
                begin_test_route(&forwarding, client, sink.clone(), corr, "concentration");
            forwarding
                .complete_pending_relay(
                    module_connection,
                    pending.corr,
                    RouteBindRelayOutcome::Accepted,
                )
                .unwrap();
        }

        // One connection, two routes — not two connections with a route each.
        assert_eq!(forwarding.active_binding_count().unwrap(), 2);
        assert_eq!(forwarding.client_route_concentration().unwrap(), (1, 2));
    }

    #[test]
    fn cleanup_and_accepted_resolution_have_one_lock_winner() {
        let (forwarding, module_connection, _, client, sink, mut client_rx) =
            route_fixture("cleanup-race");
        let pending = begin_test_route(&forwarding, client, sink, 45, "cleanup-race");
        forwarding
            .mark_route_bind_relay_enqueued(pending.endpoint, pending.corr)
            .unwrap();
        let released = forwarding.cleanup_connection(client).unwrap();
        assert_eq!(released.len(), 1);
        let completion = forwarding
            .complete_pending_relay(
                module_connection,
                pending.corr,
                RouteBindRelayOutcome::Accepted,
            )
            .unwrap();
        assert!(!completion.settled);
        assert!(client_rx.try_recv().is_err());
        assert_eq!(forwarding.active_binding_count().unwrap(), 0);

        let (forwarding, module_connection, _, client, sink, mut client_rx) =
            route_fixture("accepted-race");
        let pending = begin_test_route(&forwarding, client, sink, 46, "accepted-race");
        forwarding
            .complete_pending_relay(
                module_connection,
                pending.corr,
                RouteBindRelayOutcome::Accepted,
            )
            .unwrap();
        assert_eq!(client_rx.try_recv().unwrap().header.corr, 46);
        let released = forwarding.cleanup_connection(client).unwrap();
        assert_eq!(released.len(), 1);
        assert_eq!(forwarding.active_binding_count().unwrap(), 0);
    }

    #[test]
    fn drain_marks_block_reservation_commit_and_live_request_admission_until_phase_two() {
        let (forwarding, module_connection, _, client, sink, mut client_rx) =
            route_fixture("drain-gap");
        let live = begin_test_route(&forwarding, client, sink.clone(), 47, "drain-gap");
        forwarding
            .complete_pending_relay(
                module_connection,
                live.corr,
                RouteBindRelayOutcome::Accepted,
            )
            .unwrap();
        client_rx.try_recv().unwrap();
        let binding = match forwarding
            .lookup_data_route(client, live.client_channel, live.client_epoch)
            .unwrap()
        {
            DataRoute::Client(DataRouteState::Bound(binding)) => binding,
            other => panic!("expected live route, got {other:?}"),
        };

        let pending = begin_test_route(&forwarding, client, sink.clone(), 48, "drain-gap");
        forwarding
            .mark_route_bind_relay_enqueued(pending.endpoint, pending.corr)
            .unwrap();
        let control_rpc = forwarding
            .begin_module_control_rpc_for(
                "drain-gap",
                "health.check",
                Instant::now() + Duration::from_secs(1),
            )
            .unwrap();
        let target = forwarding
            .begin_module_drain("drain-gap", RouteCloseReason::Reload)
            .unwrap()
            .unwrap();
        assert!(matches!(
            control_rpc.receiver.blocking_recv().unwrap(),
            ModuleControlRpcOutcome::ModuleGone(_)
        ));
        assert_eq!(target.abandoned_bindings.len(), 1);
        assert!(binding.flow.sem.is_closed());
        assert!(
            !forwarding
                .complete_pending_relay(
                    module_connection,
                    pending.corr,
                    RouteBindRelayOutcome::Accepted,
                )
                .unwrap()
                .settled
        );
        assert!(matches!(
            forwarding.begin_route_bind_relay_for_test(client, sink, 49, "drain-gap"),
            Err(ForwardingError::ModuleReloading { .. })
        ));
        let released = forwarding
            .release_module_endpoint_routes(target.endpoint)
            .unwrap();
        assert_eq!(released.len(), 1);
        assert_eq!(forwarding.active_binding_count().unwrap(), 0);
    }

    #[test]
    fn pending_route_permit_is_released_on_rejection_and_abort() {
        let forwarding = ForwardingTable::default();
        let module_connection = ConnectionId::new(300);
        let client = ConnectionId::new(301);
        let (module_tx, _module_rx) = mpsc::channel(1);
        let endpoint = forwarding
            .register_module_connection(
                module_connection,
                "permit".into(),
                2,
                Concurrency::ModuleManaged,
                FrameSink::new(module_tx),
            )
            .unwrap();
        let (client_tx, mut client_rx) = mpsc::channel(1);
        let sink = FrameSink::new(client_tx);
        let rejected = begin_test_route(&forwarding, client, sink.clone(), 50, "permit");
        assert!(sink.try_send(test_ping(999)).is_err());
        forwarding
            .complete_pending_relay(
                module_connection,
                rejected.corr,
                RouteBindRelayOutcome::Rejected(ErrorBody {
                    code: "no".into(),
                    message: "rejected".into(),
                    detail: None,
                }),
            )
            .unwrap();
        sink.try_send(test_ping(1000)).unwrap();
        assert_eq!(client_rx.try_recv().unwrap().header.corr, 1000);

        let aborted = begin_test_route(&forwarding, client, sink.clone(), 51, "permit");
        assert!(sink.try_send(test_ping(1001)).is_err());
        forwarding
            .abort_pending_relay(
                endpoint,
                aborted.corr,
                RouteBindRelayOutcome::ModuleGone("abort".into()),
            )
            .unwrap();
        sink.try_send(test_ping(1002)).unwrap();
        assert_eq!(client_rx.try_recv().unwrap().header.corr, 1002);

        let receiver_closed = begin_test_route(&forwarding, client, sink, 52, "permit");
        forwarding
            .mark_route_bind_relay_enqueued(endpoint, receiver_closed.corr)
            .unwrap();
        drop(client_rx);
        let completion = forwarding
            .complete_pending_relay(
                module_connection,
                receiver_closed.corr,
                RouteBindRelayOutcome::Accepted,
            )
            .unwrap();
        assert!(completion.abandoned.is_some());
        assert_eq!(forwarding.active_binding_count().unwrap(), 0);
    }
}
