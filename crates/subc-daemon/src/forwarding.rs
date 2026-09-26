use std::{
    collections::{BTreeMap, HashMap, HashSet},
    error::Error,
    fmt,
    sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard},
    time::Duration,
};

use subc_control::{ClientControlResponse, RouteCloseReason};
use subc_protocol::{
    manifest::Concurrency,
    session::{LiveRoot, ModuleControlResponse, ModuleControlResponseToModule},
    ErrorBody, Flags, FrameType, Principal, Priority,
};
use tokio::sync::{oneshot, Semaphore};
use tokio::time::Instant;
use tracing::{debug, info, warn};

use crate::{
    control::{RouteBindBreakers, RouteBindConcurrency},
    observability::DaemonCounters,
    registry::ConnectionId,
    router::FrameSink,
    Frame, ProjectRootId,
};

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
    pub project_root: Option<ProjectRootId>,
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
/// - `Module`: never close; deliver late instead (see
///   [`send_module_route_goodbye`]), and drop only if the module still has no
///   room after [`LATE_MODULE_GOODBYE_DEADLINE`]. A client-disconnect notifies the
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
    /// The module on the other end of the route: for a module-targeted relay
    /// the receiving module (attributing a dropped relay), for a client target
    /// the module whose route is going away (naming it when an undeliverable
    /// relay closes the client).
    pub module_id: Option<String>,
}

/// The frame a client connection's egress queue refused, for the diagnosis
/// logged when that refusal closes the connection.
#[derive(Debug, Clone, Copy)]
pub(crate) struct UndeliveredFrame<'a> {
    /// The module on the other end of the route the frame belonged to, when known.
    pub module_id: Option<&'a str>,
    /// The refusing connection's sink, read for what its queue held.
    pub sink: &'a FrameSink,
}

/// How a route's principal appears in logs: `direct`, or `reserved:<module>`.
fn principal_label(principal: &Principal) -> String {
    match principal {
        Principal::Reserved { module_id } => format!("reserved:{module_id}"),
        Principal::Direct => "direct".to_string(),
        other => format!("{other:?}"),
    }
}

/// The distinct principals of the routes bound on one client connection,
/// sorted and comma-joined, or `none` when it has no bound route.
fn connection_principals_locked(inner: &ForwardingInner, connection_id: ConnectionId) -> String {
    let labels = inner
        .client_to_module
        .iter()
        .filter(|(key, _)| key.connection_id == connection_id)
        .map(|(_, route)| principal_label(&route.principal))
        .collect::<std::collections::BTreeSet<_>>();
    if labels.is_empty() {
        "none".to_string()
    } else {
        labels.into_iter().collect::<Vec<_>>().join(",")
    }
}

impl GoodbyeTarget {
    /// True only when an undeliverable GOODBYE should escalate to closing the
    /// target connection. Never escalate for module recipients.
    pub(crate) fn close_on_delivery_failure(&self) -> bool {
        matches!(self.kind, GoodbyeTargetKind::Client)
    }
}

/// How long a route GOODBYE that a module's egress queue refused keeps waiting
/// for room before it is given up and counted as dropped.
///
/// A module that stops reading for a moment (a GC pause, a long synchronous
/// handler, a reconnect herd it is still working through) should still learn
/// that the route is gone, because a module that never hears the GOODBYE keeps
/// the route and keeps sending on it for as long as its connection lives. The
/// value is the default module drain timeout: the daemon already treats that as
/// the longest it is reasonable to wait on a module that is busy but alive, and
/// a module that cannot free 21 bytes of egress in that time is wedged rather
/// than slow. Resolving each module's own configured drain timeout here would
/// need a registry lookup on a path that holds only the module's sink.
pub(crate) const LATE_MODULE_GOODBYE_DEADLINE: Duration = crate::supervise::DEFAULT_DRAIN_TIMEOUT;

/// Send a route GOODBYE to a module without ever closing the module's shared
/// connection, which would sever every other route it serves.
///
/// The frame is enqueued at once when the module's egress queue has room. When
/// it does not, a detached task waits for room (bounded by
/// [`LATE_MODULE_GOODBYE_DEADLINE`]) and sends it then. Arriving late is safe:
/// the daemon has already released the route, and module SDKs tear a route
/// down only when both the channel AND the epoch match the route installed on
/// that channel, so a late GOODBYE for a channel since reused at a newer epoch
/// is ignored. The GOODBYE is counted in `goodbye_relay_module_dropped` only
/// when the connection is closed or the deadline passes.
pub(crate) fn send_module_route_goodbye(
    counters: &DaemonCounters,
    sink: &FrameSink,
    frame: Frame,
    module_id: Option<&str>,
    context: &'static str,
) {
    let channel = frame.header.channel;
    let epoch = frame.header.epoch;
    let Err(err) = sink.try_send(frame.clone()) else {
        return;
    };
    // Waiting is pointless once the module's writer is gone, and impossible
    // outside a Tokio runtime (cleanup that runs from a destructor at shutdown).
    let runtime = match tokio::runtime::Handle::try_current() {
        Ok(runtime) if !sink.is_closed() => runtime,
        _ => {
            counters.increment_goodbye_relay_module_dropped(module_id);
            warn!(
            module_id = module_id.unwrap_or("unknown"),
            route_channel = channel,
            route_epoch = epoch,
            error = %err,
            context,
            "route GOODBYE to module dropped: module connection is closed; not closing shared module connection"
            );
            return;
        }
    };
    debug!(
        module_id = module_id.unwrap_or("unknown"),
        route_channel = channel,
        route_epoch = epoch,
        error = %err,
        context,
        "module egress queue refused route GOODBYE; delivering it once the module frees room"
    );
    let counters = counters.clone();
    let sink = sink.clone();
    let module_id = module_id.map(str::to_string);
    runtime.spawn(async move {
        let outcome = tokio::time::timeout(LATE_MODULE_GOODBYE_DEADLINE, sink.send(frame)).await;
        let why = match outcome {
            Ok(Ok(())) => {
                debug!(
                    module_id = module_id.as_deref().unwrap_or("unknown"),
                    route_channel = channel,
                    route_epoch = epoch,
                    context,
                    "late route GOODBYE delivered to module"
                );
                return;
            }
            Ok(Err(err)) => err.to_string(),
            Err(_) => format!(
                "module egress queue had no room within {LATE_MODULE_GOODBYE_DEADLINE:?}"
            ),
        };
        counters.increment_goodbye_relay_module_dropped(module_id.as_deref());
        warn!(
            module_id = module_id.as_deref().unwrap_or("unknown"),
            route_channel = channel,
            route_epoch = epoch,
            error = %why,
            context,
            "route GOODBYE to module dropped under backpressure; not closing shared module connection"
        );
    });
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
    pub excluded_subscriptions: u32,
}

/// One registered module connection, as [`ForwardingTable::module_connections`]
/// reports it: enough to send it a frame and then close it.
#[cfg(unix)]
#[derive(Debug, Clone)]
pub(crate) struct ModuleConnectionTarget {
    pub module_id: String,
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

/// What [`ForwardingTable::cutover_candidate`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ForwardingCutover {
    /// The endpoint now in the active slot (the former candidate).
    pub promoted: ModuleEndpointId,
    /// The endpoint demoted out of the active slot, to be drained by endpoint.
    /// `None` when the incumbent's connection was already gone.
    pub incumbent: Option<ModuleEndpointId>,
}

/// What tearing down one connection released.
#[derive(Debug)]
pub(crate) struct ConnectionCleanup {
    /// Routes whose other end must be sent GOODBYE.
    pub released: Vec<GoodbyeTarget>,
    /// Pending route.bind relays to the closed module that were aborted: opens
    /// that had not been bound yet. Zero when a client connection closed.
    pub abandoned_relays: u32,
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

#[derive(Debug, Clone)]
struct RouteReservation {
    client_key: ClientRouteKey,
    module_key: ModuleRouteKey,
    client_epoch: u32,
    module_epoch: u32,
    project_root: Option<ProjectRootId>,
}

#[derive(Debug)]
struct PendingRouteBindRelayEntry {
    reservation: RouteReservation,
    client_sink: FrameSink,
    client_negotiated_ver: u8,
    client_permit: crate::router::EgressPermit,
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
    daemon_draining: bool,
    /// The ACTIVE slot: the one endpoint per module id that routing resolves.
    /// Every by-id lookup (relay reservation, drain-by-id, liveness, census,
    /// live roots, module-control RPCs) reads this map and nothing else.
    modules_by_id: HashMap<String, ModuleConnection>,
    /// The CANDIDATE slot of a blue/green swap: a second process registered
    /// under an id that already has an active endpoint. It has a full endpoint
    /// identity (so its own connection can be looked up and torn down) but no
    /// by-id lookup sees it, so nothing is routed to it until `cutover_candidate`
    /// promotes it.
    candidates_by_id: HashMap<String, ModuleConnection>,
    /// Former active endpoints demoted by `cutover_candidate`, until their
    /// connection is removed. Membership is what distinguishes an endpoint that
    /// was SUPERSEDED by a promotion (still alive, still carrying bound routes,
    /// about to be drained) from one that is merely STALE (replaced some other
    /// way, which is treated as a fault on the acking connection).
    superseded_endpoints: HashMap<ModuleEndpointId, ModuleConnection>,
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
    /// Per-target-module bind-relay breaker state. It lives here, beside the
    /// module connections it describes, because this is where a module
    /// connection's identity is established and therefore where a stale
    /// verdict has to be discarded.
    route_bind_breakers: RouteBindBreakers,
    /// Current route.bind relays keyed by target module. Admission is shared
    /// across every client connection that points at the same endpoint.
    route_bind_concurrency: RouteBindConcurrency,
    /// Start and end of each module's `route.open` outage. Held here for the
    /// same reason as the breakers: every handler built over this table must
    /// see one outage per module, or each would log its own opening line.
    route_outages: Arc<crate::route_outage::RouteOutageTracker>,
}

impl ForwardingTable {
    pub(crate) fn counters(&self) -> DaemonCounters {
        self.counters.clone()
    }

    pub(crate) fn route_bind_breakers(&self) -> RouteBindBreakers {
        self.route_bind_breakers.clone()
    }

    pub(crate) fn route_bind_concurrency(&self) -> RouteBindConcurrency {
        self.route_bind_concurrency.clone()
    }

    pub(crate) fn route_outages(&self) -> Arc<crate::route_outage::RouteOutageTracker> {
        Arc::clone(&self.route_outages)
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

    /// Close every established connection, modules and clients alike, as the
    /// last step of an announced daemon shutdown. By the time this runs each
    /// registered module has already been sent a module GOODBYE (see
    /// `Supervisor::end_children_for_daemon_shutdown`), so the EOF that follows
    /// is a planned stop; EOF with no GOODBYE before it stays reserved for a
    /// daemon that went away unannounced. Closing while the daemon is still
    /// alive lets it wait for the modules' own teardowns before it ends
    /// whatever is left.
    #[cfg(unix)]
    pub(crate) fn close_all_connections(&self, reason: &CloseReason) -> usize {
        let senders: Vec<_> = self.lock_close_registry().drain().collect();
        let count = senders.len();
        for (_, sender) in senders {
            let _ = sender.send(reason.clone());
        }
        count
    }

    /// Every registered module connection, whichever slot it occupies (active,
    /// swap candidate, or superseded incumbent), once each. Daemon shutdown
    /// uses this to tell each of them it is a planned stop before closing it.
    #[cfg(unix)]
    pub(crate) fn module_connections(
        &self,
    ) -> Result<Vec<ModuleConnectionTarget>, ForwardingError> {
        let inner = self.read_inner()?;
        let mut seen = HashSet::new();
        Ok(inner
            .modules_by_id
            .values()
            .chain(inner.candidates_by_id.values())
            .chain(inner.superseded_endpoints.values())
            .filter(|module| seen.insert(module.endpoint))
            .map(|module| ModuleConnectionTarget {
                module_id: inner
                    .module_id_by_endpoint
                    .get(&module.endpoint)
                    .cloned()
                    .unwrap_or_default(),
                endpoint: module.endpoint,
                sink: module.sink.clone(),
                negotiated_ver: module.negotiated_ver,
            })
            .collect())
    }

    /// Ask a registered connection to close. Returns true only for the request
    /// that actually reached it; later requests for the same connection, and
    /// requests for a connection that is not registered, return false.
    pub(crate) fn request_connection_close(
        &self,
        connection_id: ConnectionId,
        reason: CloseReason,
    ) -> bool {
        let sender = self.lock_close_registry().remove(&connection_id);
        if let Some(sender) = sender {
            debug!(
                connection_id = connection_id.get(),
                close_reason = %reason,
                "requesting connection close"
            );
            let _ = sender.send(reason);
            true
        } else {
            debug!(
                connection_id = connection_id.get(),
                close_reason = %reason,
                "connection close request ignored for inactive connection"
            );
            false
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
        self.register_module_connection_inner(
            connection_id,
            module_id,
            negotiated_ver,
            concurrency,
            sink,
            None,
        )
    }

    /// Register a module connection and queue its HELLO_ACK as the first frame
    /// on its sink, in the same write-lock critical section that makes the
    /// endpoint visible.
    ///
    /// A module reads HELLO_ACK as the first frame after its HELLO and exits on
    /// anything else. Once the endpoint is in `modules_by_id`, a `route.open`
    /// on any other connection can queue a `route.bind` request onto this sink,
    /// so the ack must already be queued by then. Every lookup that can queue a
    /// frame for the module takes this lock, so nothing can get in ahead of it.
    ///
    /// If the ack cannot be queued (the sink is closed or full), the
    /// registration fails with nothing inserted.
    pub(crate) fn register_module_connection_acked(
        &self,
        connection_id: ConnectionId,
        module_id: String,
        negotiated_ver: u8,
        concurrency: Concurrency,
        sink: FrameSink,
        hello_ack: Frame,
    ) -> Result<ModuleEndpointId, ForwardingError> {
        self.register_module_connection_inner(
            connection_id,
            module_id,
            negotiated_ver,
            concurrency,
            sink,
            Some(hello_ack),
        )
    }

    fn register_module_connection_inner(
        &self,
        connection_id: ConnectionId,
        module_id: String,
        negotiated_ver: u8,
        concurrency: Concurrency,
        sink: FrameSink,
        hello_ack: Option<Frame>,
    ) -> Result<ModuleEndpointId, ForwardingError> {
        let mut inner = self.write_inner()?;
        if inner.daemon_draining || inner.closing_connections.contains(&connection_id) {
            return Err(ForwardingError::ConnectionClosing { connection_id });
        }
        // Every refusal check has passed and nothing has been mutated yet, so
        // a failed enqueue leaves the table exactly as it was.
        enqueue_hello_ack_locked(&sink, connection_id, hello_ack)?;
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
        drop(inner);

        // A new module connection has arrived under this id, so anything the
        // bind-relay breaker learned was learned about a process that is no
        // longer the one behind this name. See
        // `RouteBindBreakers::reset_for_new_module_connection`.
        //
        // Keyed on ARRIVAL rather than on teardown deliberately: a connection
        // going away is not evidence about anything, and a module whose
        // connection drops without coming back should keep its verdict until
        // something actually registers in its place. This also covers the
        // unclean replacements -- a module killed mid-bind, or one whose
        // connection was closing -- because registration is the single path by
        // which any module connection becomes usable.
        if let Some(discarded) = self
            .route_bind_breakers
            .reset_for_new_module_connection(&module_id)
        {
            info!(
                module_id = %module_id,
                discarded_consecutive_timeouts = discarded,
                "route.bind breaker state discarded: a new module connection replaced the process it described"
            );
        }
        Ok(endpoint)
    }

    /// Register a blue/green swap candidate for `module_id` into the candidate
    /// slot, alongside whatever endpoint is active for the id.
    ///
    /// Unlike [`Self::register_module_connection`], this never touches the
    /// active slot and never resets the bind-relay breaker: the incumbent is
    /// still the process serving the id, so what the breaker learned about it
    /// is still true. The breaker is reset at cutover instead, when the process
    /// behind the name actually changes. A second candidate for the same id is
    /// refused.
    ///
    /// Production registers candidates through
    /// [`Self::register_candidate_module_connection_acked`]; this ack-less form
    /// is for tests that build forwarding state directly.
    #[cfg(test)]
    pub(crate) fn register_candidate_module_connection(
        &self,
        connection_id: ConnectionId,
        module_id: String,
        negotiated_ver: u8,
        concurrency: Concurrency,
        sink: FrameSink,
    ) -> Result<ModuleEndpointId, ForwardingError> {
        self.register_candidate_module_connection_inner(
            connection_id,
            module_id,
            negotiated_ver,
            concurrency,
            sink,
            None,
        )
    }

    /// The swap-candidate counterpart of
    /// [`Self::register_module_connection_acked`]: the HELLO_ACK is queued on
    /// the candidate's sink before its endpoint is inserted. No by-id lookup
    /// sees a candidate, but its own connection's endpoint does become
    /// resolvable here, and at cutover it becomes routable; the ack has to be
    /// ahead of anything either can queue.
    pub(crate) fn register_candidate_module_connection_acked(
        &self,
        connection_id: ConnectionId,
        module_id: String,
        negotiated_ver: u8,
        concurrency: Concurrency,
        sink: FrameSink,
        hello_ack: Frame,
    ) -> Result<ModuleEndpointId, ForwardingError> {
        self.register_candidate_module_connection_inner(
            connection_id,
            module_id,
            negotiated_ver,
            concurrency,
            sink,
            Some(hello_ack),
        )
    }

    fn register_candidate_module_connection_inner(
        &self,
        connection_id: ConnectionId,
        module_id: String,
        negotiated_ver: u8,
        concurrency: Concurrency,
        sink: FrameSink,
        hello_ack: Option<Frame>,
    ) -> Result<ModuleEndpointId, ForwardingError> {
        let mut inner = self.write_inner()?;
        if inner.daemon_draining || inner.closing_connections.contains(&connection_id) {
            return Err(ForwardingError::ConnectionClosing { connection_id });
        }
        if inner.candidates_by_id.contains_key(&module_id) {
            return Err(ForwardingError::CandidateSlotOccupied { module_id });
        }
        // As in `register_module_connection_inner`: after every refusal check,
        // before any mutation.
        enqueue_hello_ack_locked(&sink, connection_id, hello_ack)?;
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
        inner.candidates_by_id.insert(
            module_id,
            ModuleConnection {
                endpoint,
                sink,
                negotiated_ver,
                concurrency,
            },
        );
        Ok(endpoint)
    }

    /// Promote `module_id`'s candidate to the active slot, in ONE forwarding
    /// write-lock critical section.
    ///
    /// That single section is the linearization point of a swap. Every relay
    /// reservation resolves the active slot under the same lock, so each one
    /// lands wholly before cutover (on the incumbent) or wholly after it (on the
    /// promoted candidate), never on a mix. A relay reserved on the incumbent
    /// before cutover can no longer commit: `commit_route_locked` requires the
    /// reservation's endpoint to be the active one, and an ack for it arriving
    /// later is answered by the superseded-endpoint arm of
    /// `complete_pending_relay` instead of ending the incumbent's connection.
    ///
    /// Both endpoints keep their identities: nothing is re-keyed, so the
    /// incumbent's bound routes and its pending correlation keys stay exactly
    /// where they are until it is drained with [`Self::begin_endpoint_drain`],
    /// using the incumbent endpoint this returns. Returns `Ok(None)` when there
    /// is no candidate for the id.
    pub(crate) fn cutover_candidate(
        &self,
        module_id: &str,
    ) -> Result<Option<ForwardingCutover>, ForwardingError> {
        let mut inner = self.write_inner()?;
        if inner.daemon_draining {
            return Err(ForwardingError::ModuleReloading {
                module_id: module_id.to_string(),
            });
        }
        let Some(candidate) = inner.candidates_by_id.remove(module_id) else {
            return Ok(None);
        };
        let promoted = candidate.endpoint;
        let incumbent = inner.modules_by_id.insert(module_id.to_string(), candidate);
        let incumbent = incumbent.map(|incumbent| {
            let endpoint = incumbent.endpoint;
            inner.superseded_endpoints.insert(endpoint, incumbent);
            endpoint
        });
        drop(inner);

        // A different process now answers for this id; see the matching reset
        // in `register_module_connection` for why a verdict about the old one
        // must not carry over.
        if let Some(discarded) = self
            .route_bind_breakers
            .reset_for_new_module_connection(module_id)
        {
            info!(
                module_id = %module_id,
                discarded_consecutive_timeouts = discarded,
                "route.bind breaker state discarded: a swap candidate was promoted over the process it described"
            );
        }
        Ok(Some(ForwardingCutover {
            promoted,
            incumbent,
        }))
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
        project_root: Option<ProjectRootId>,
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
            project_root,
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
            None,
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
        self.begin_module_control_rpc_inner(module_id, expected_op, deadline, None, false)
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
            false,
        )
    }

    pub(crate) fn begin_drain_health_probe_rpc_for(
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
            true,
        )
    }

    /// A health probe addressed to one endpoint, in whichever slot it is.
    ///
    /// Every other control RPC resolves the id's ACTIVE endpoint, which is how
    /// a swap candidate (not active until cutover) and a superseded incumbent
    /// (not active after it) are unreachable by them. A swap needs to probe
    /// exactly those two: the candidate before promoting it, and the incumbent
    /// for its busy gauges while it drains.
    pub(crate) fn begin_endpoint_health_probe_rpc_for(
        &self,
        endpoint: ModuleEndpointId,
        expected_op: &str,
        probe_started_at: Instant,
        deadline: Instant,
    ) -> Result<PendingModuleControlRpc, ForwardingError> {
        let inner = self.write_inner()?;
        let module = module_connection_for_endpoint_locked(&inner, endpoint)
            .cloned()
            .ok_or(ForwardingError::NoModuleConnection)?;
        let module_id = inner
            .module_id_by_endpoint
            .get(&endpoint)
            .cloned()
            .unwrap_or_default();
        // A draining incumbent is exactly what this probes for busy gauges, so
        // draining is allowed, as it is for the by-id drain probe.
        self.begin_control_rpc_locked(
            inner,
            &module_id,
            module,
            expected_op,
            deadline,
            Some(probe_started_at),
            true,
        )
    }

    fn begin_module_control_rpc_inner(
        &self,
        module_id: &str,
        expected_op: &str,
        deadline: Instant,
        health_probe_started_at: Option<Instant>,
        allow_draining: bool,
    ) -> Result<PendingModuleControlRpc, ForwardingError> {
        let inner = self.write_inner()?;
        let module = inner
            .modules_by_id
            .get(module_id)
            .cloned()
            .ok_or(ForwardingError::NoModuleConnection)?;
        self.begin_control_rpc_locked(
            inner,
            module_id,
            module,
            expected_op,
            deadline,
            health_probe_started_at,
            allow_draining,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn begin_control_rpc_locked(
        &self,
        mut inner: RwLockWriteGuard<'_, ForwardingInner>,
        module_id: &str,
        module: ModuleConnection,
        expected_op: &str,
        deadline: Instant,
        health_probe_started_at: Option<Instant>,
        allow_draining: bool,
    ) -> Result<PendingModuleControlRpc, ForwardingError> {
        if !allow_draining && inner.draining_endpoints.contains_key(&module.endpoint) {
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
        project_root: Option<ProjectRootId>,
        deadline: Instant,
        client_permit: crate::router::EgressPermit,
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
            project_root,
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
            // Two shapes of "the client is not there to receive this route" that
            // must resolve identically: its egress is already closed, or it is
            // marked closing (its connection loop has been asked to end, but has
            // not drained yet, so the sink is still open).
            //
            // Only the first used to be caught here. The second fell through to
            // `commit_route_locked`, which refuses a closing client with
            // `ConnectionClosing` -- and this function is called from the MODULE
            // connection's frame handler, so that refusal ended the module's
            // connection instead of this one client's route. One dying client's
            // route.open then took down a connection carrying every other
            // client's routes to that module.
            //
            // The remedy for both is the same, which is why they share an arm:
            // give back the reserved handle pair, tell the waiting route.open the
            // route is gone, and report the module-side channel so the caller can
            // send a channel-scoped GOODBYE for the binding the module just
            // created. Nothing here touches the module connection.
            RouteBindRelayOutcome::Accepted
                if pending.client_sink.is_closed()
                    || inner
                        .closing_connections
                        .contains(&pending.reservation.client_key.connection_id) =>
            {
                let reason = if pending.client_sink.is_closed() {
                    "client egress closed before route publication"
                } else {
                    "client connection is closing before route publication"
                };
                release_reserved_route_locked(
                    &mut inner,
                    pending.reservation.client_key,
                    pending.reservation.module_key,
                );
                let abandoned = pending
                    .relay_enqueued
                    .then(|| abandoned_route_target(&inner, &pending.reservation))
                    .flatten();
                let _ = pending
                    .sender
                    .send(RouteBindRelayOutcome::ModuleGone(reason.to_string()));
                return Ok(PendingRelayCompletion {
                    settled: true,
                    abandoned,
                });
            }
            // The acking endpoint was the active one when this relay was
            // reserved, and a swap has since promoted a candidate over it. The
            // module did nothing wrong: it bound the route it was asked to bind,
            // and it is still carrying every other client's routes until it is
            // drained. So this is settled here, while the pending entry still
            // holds the client's sender and the reservation pair: release the
            // pair, tell the waiting route.open to retry (it will reserve on the
            // promoted endpoint), and hand back the module-side channel so the
            // caller sends one channel-scoped GOODBYE for the binding the module
            // just created. Nothing here touches the module connection.
            //
            // Only membership in `superseded_endpoints` takes this arm. An
            // endpoint that stopped being the active one for any other reason
            // is STALE, not superseded, and still falls through to
            // `commit_route_locked`, which refuses it with `StaleModuleEndpoint`
            // exactly as before swaps existed.
            RouteBindRelayOutcome::Accepted
                if inner.superseded_endpoints.contains_key(&endpoint) =>
            {
                release_reserved_route_locked(
                    &mut inner,
                    pending.reservation.client_key,
                    pending.reservation.module_key,
                );
                // Not gated on `relay_enqueued`: an ack proves the module
                // received the bind, whether or not the enqueue mark was set.
                let abandoned = abandoned_route_target(&inner, &pending.reservation);
                let module_id = inner
                    .module_id_by_endpoint
                    .get(&endpoint)
                    .cloned()
                    .unwrap_or_else(|| "unknown".to_string());
                let _ = pending
                    .sender
                    .send(RouteBindRelayOutcome::Rejected(ErrorBody::new(
                        "module_reloading",
                        format!("module_id '{module_id}' is reloading"),
                    )));
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

    #[cfg(test)]
    pub(crate) fn closing_connection_count(&self) -> Result<usize, ForwardingError> {
        Ok(self.read_inner()?.closing_connections.len())
    }

    /// Reserved-but-uncommitted route handles, counted on both index sides, so
    /// a test can assert a reservation pair was actually given back.
    #[cfg(test)]
    pub(crate) fn reserved_route_count(&self) -> Result<(usize, usize), ForwardingError> {
        let inner = self.read_inner()?;
        Ok((inner.reserved_client.len(), inner.reserved_module.len()))
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

    /// Whether the daemon ever allocated `(channel, epoch)` on this module
    /// connection. Epochs on a module channel are handed out as 1, 2, 3, ...
    /// and the last one handed out is remembered until the connection ends, so
    /// every epoch from 1 up to that one was a real route at some point. Used
    /// only on the drop path, to tell a module still sending on a route the
    /// daemon released from one sending on a route that never existed.
    pub(crate) fn module_route_epoch_was_allocated(
        &self,
        connection_id: ConnectionId,
        channel: u16,
        epoch: u32,
    ) -> Result<bool, ForwardingError> {
        let inner = self.read_inner()?;
        let Some(endpoint) = inner.endpoint_by_connection.get(&connection_id).copied() else {
            return Ok(false);
        };
        Ok(inner
            .module_slot_epochs
            .get(&ModuleRouteKey { endpoint, channel })
            .is_some_and(|last| epoch != 0 && epoch <= *last))
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

    /// Whether daemon-wide shutdown has begun draining all providers.
    pub(crate) fn is_daemon_draining(&self) -> Result<bool, ForwardingError> {
        Ok(self.read_inner()?.daemon_draining)
    }

    /// Gate every provider atomically, including registrations racing shutdown.
    /// No supervisor lock is held while taking the forwarding lock.
    #[cfg(unix)]
    pub(crate) fn begin_daemon_drain(&self) -> Result<Vec<String>, ForwardingError> {
        let mut inner = self.write_inner()?;
        inner.daemon_draining = true;
        let modules = inner
            .modules_by_id
            .iter()
            .map(|(id, module)| (id.clone(), module.endpoint))
            .collect::<Vec<_>>();
        for (_, endpoint) in &modules {
            inner
                .draining_endpoints
                .insert(*endpoint, RouteCloseReason::Restart);
        }
        // Swap candidates and superseded incumbents are not routable, but they
        // are live endpoints that could still be sent module-control work, so
        // the daemon-wide gate covers them too. The returned ids are unchanged:
        // they name modules for the supervisor to drain, one per id.
        let off_slot_endpoints = inner
            .candidates_by_id
            .values()
            .map(|module| module.endpoint)
            .chain(inner.superseded_endpoints.keys().copied())
            .collect::<Vec<_>>();
        for endpoint in off_slot_endpoints {
            inner
                .draining_endpoints
                .insert(endpoint, RouteCloseReason::Restart);
        }
        Ok(modules.into_iter().map(|(id, _)| id).collect())
    }

    /// Begin draining whatever endpoint is ACTIVE for `module_id`.
    ///
    /// After a swap's cutover the active endpoint is the promoted candidate, so
    /// this must not be used to drain the incumbent; use
    /// [`Self::begin_endpoint_drain`] with the endpoint `cutover_candidate`
    /// returned.
    pub(crate) fn begin_module_drain(
        &self,
        module_id: &str,
        reason: RouteCloseReason,
    ) -> Result<Option<ModuleDrainTarget>, ForwardingError> {
        let mut inner = self.write_inner()?;
        let Some(module) = inner.modules_by_id.get(module_id).cloned() else {
            return Ok(None);
        };
        Ok(Some(begin_drain_locked(
            &mut inner, module_id, module, reason,
        )))
    }

    /// Begin draining one specific endpoint, whichever slot it is in.
    ///
    /// This is how a swap drains its incumbent after cutover: the incumbent's
    /// endpoint is captured by `cutover_candidate`, and resolving it by module id
    /// instead would find the promoted candidate and leave neither process
    /// routable. Returns `Ok(None)` when the endpoint is no longer registered.
    pub(crate) fn begin_endpoint_drain(
        &self,
        endpoint: ModuleEndpointId,
        reason: RouteCloseReason,
    ) -> Result<Option<ModuleDrainTarget>, ForwardingError> {
        let mut inner = self.write_inner()?;
        let Some(module) = module_connection_for_endpoint_locked(&inner, endpoint).cloned() else {
            return Ok(None);
        };
        let module_id = inner
            .module_id_by_endpoint
            .get(&endpoint)
            .cloned()
            .expect("an endpoint resolved to a module connection has a module id");
        Ok(Some(begin_drain_locked(
            &mut inner, &module_id, module, reason,
        )))
    }
}

/// Mark `module`'s endpoint draining and settle everything still pending on it.
/// Shared by the by-id and by-endpoint drain entry points, which differ only in
/// how they find the endpoint.
fn begin_drain_locked(
    inner: &mut ForwardingInner,
    module_id: &str,
    module: ModuleConnection,
    reason: RouteCloseReason,
) -> ModuleDrainTarget {
    {
        let endpoint = module.endpoint;
        inner.draining_endpoints.insert(endpoint, reason);

        let flows = inner
            .client_to_module
            .values()
            .filter(|route| route.module_endpoint == endpoint)
            .map(|route| Arc::clone(&route.flow))
            .collect::<Vec<_>>();
        let excluded_subscriptions = flows
            .into_iter()
            .map(|flow| flow.begin_drain())
            .fold(0u32, u32::saturating_add);

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
                inner,
                pending.reservation.client_key,
                pending.reservation.module_key,
            );
            if pending.relay_enqueued {
                if let Some(target) = abandoned_route_target(inner, &pending.reservation) {
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

        ModuleDrainTarget {
            endpoint,
            sink: module.sink,
            negotiated_ver: module.negotiated_ver,
            abandoned_bindings,
            excluded_subscriptions,
        }
    }
}

/// What a drain that timed out was still waiting on, for the log line that
/// reports the timeout. Per-request ages are not tracked.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct DrainHoldouts {
    /// Requests still counted against the drain (subscriptions flagged as
    /// such are excluded from the drain and not counted here).
    pub(crate) requests: usize,
    /// Routes holding at least one of those requests.
    pub(crate) routes: usize,
    /// Every route on the endpoint, for scale.
    pub(crate) total_routes: usize,
    /// The client connections holding the most requests, largest first, at
    /// most three: enough to name the consumer without listing every route.
    pub(crate) top_connections: Vec<(u64, usize)>,
    /// The held requests themselves, as the module saw them, so its own log
    /// can say what each one was: `(module channel, corr)`, ordered by channel
    /// then corr, at most [`DRAIN_HELD_REQUESTS_LISTED`]. The daemon never reads
    /// request bodies, so it cannot name a request's method; the module can,
    /// from the channel and corr. A request is released only when the module
    /// sends a terminal frame (Response, Error or StreamEnd) with its corr on
    /// its route, so every pair listed here is one the module ended, if at all,
    /// without sending that frame.
    pub(crate) held: Vec<(u16, u64)>,
}

/// How many held requests the drain-timeout line lists by channel and corr.
pub(crate) const DRAIN_HELD_REQUESTS_LISTED: usize = 32;

impl ForwardingTable {
    /// Summarise the requests one endpoint's drain is still waiting on.
    pub(crate) fn endpoint_drain_holdouts(
        &self,
        endpoint: ModuleEndpointId,
    ) -> Result<DrainHoldouts, ForwardingError> {
        let inner = self.read_inner()?;
        let mut holdouts = DrainHoldouts::default();
        let mut by_connection: HashMap<u64, usize> = HashMap::new();
        for (key, route) in &inner.client_to_module {
            if route.module_endpoint != endpoint {
                continue;
            }
            holdouts.total_routes += 1;
            let held = route.flow.drain_in_flight();
            if held == 0 {
                continue;
            }
            holdouts.requests += held;
            holdouts.routes += 1;
            *by_connection.entry(key.connection_id.get()).or_default() += held;
            holdouts.held.extend(
                route
                    .flow
                    .drain_held_corrs()
                    .into_iter()
                    .map(|corr| (route.module_channel, corr)),
            );
        }
        holdouts.held.sort_unstable();
        holdouts.held.truncate(DRAIN_HELD_REQUESTS_LISTED);
        let mut connections = by_connection.into_iter().collect::<Vec<_>>();
        connections.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(&right.0)));
        connections.truncate(3);
        holdouts.top_connections = connections;
        Ok(holdouts)
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
            .map(|route| route.flow.drain_in_flight())
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

    /// Snapshot only the endpoint currently routable under this module id.
    pub(crate) fn live_roots(
        &self,
        module_id: &str,
    ) -> Result<ModuleControlResponseToModule, ForwardingError> {
        let inner = self.read_inner()?;
        let endpoint = inner
            .modules_by_id
            .get(module_id)
            .map(|module| module.endpoint);
        let mut roots = BTreeMap::new();
        let mut unknown_root_bindings = 0;
        let mut total_bindings = 0;
        if let Some(endpoint) = endpoint {
            for binding in inner
                .module_to_client
                .values()
                .filter(|binding| binding.module_endpoint == endpoint)
            {
                total_bindings += 1;
                if let Some(root) = &binding.project_root {
                    let entry = roots.entry(root.as_path().to_path_buf()).or_insert((0, 0));
                    entry.0 += 1;
                } else {
                    unknown_root_bindings += 1;
                }
            }
            for pending in inner
                .pending_relays
                .values()
                .filter(|pending| pending.reservation.module_key.endpoint == endpoint)
            {
                total_bindings += 1;
                if let Some(root) = &pending.reservation.project_root {
                    let entry = roots.entry(root.as_path().to_path_buf()).or_insert((0, 0));
                    entry.1 += 1;
                } else {
                    unknown_root_bindings += 1;
                }
            }
        }
        Ok(ModuleControlResponseToModule::LiveRoots {
            roots: roots
                .into_iter()
                .map(|(project_root, (bound, pending))| LiveRoot {
                    project_root,
                    bound,
                    pending,
                })
                .collect(),
            unknown_root_bindings,
            total_bindings,
        })
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
        self.cleanup_connection_counted(connection_id)
            .map(|cleanup| cleanup.released)
    }

    /// [`Self::cleanup_connection`], also reporting how many pending
    /// route.bind relays to the closed module were aborted. That count is the
    /// `abandoned` figure of the `route.closed` push sent for a lost module
    /// connection; it is always zero for a client connection.
    pub(crate) fn cleanup_connection_counted(
        &self,
        connection_id: ConnectionId,
    ) -> Result<ConnectionCleanup, ForwardingError> {
        let mut inner = self.write_inner()?;
        inner.closing_connections.insert(connection_id);
        let cleanup = if let Some(endpoint) = inner.endpoint_by_connection.remove(&connection_id) {
            remove_module_connection_locked(&mut inner, endpoint)
        } else {
            ConnectionCleanup {
                released: Self::cleanup_client_connection_locked(&mut inner, connection_id),
                abandoned_relays: 0,
            }
        };
        // The closing mark refuses new work for a connection whose teardown is
        // still pending. This is the latest point at which lifting it is safe:
        // teardown has just removed every per-connection entry above, under
        // this same write lock, so no lookup can still find live state for the
        // id; and ids come from a monotonic counter that never reissues one,
        // so the id can never name a future connection either. Keeping the
        // mark past this point only grew the set by one entry per connection
        // for the life of the daemon.
        inner.closing_connections.remove(&connection_id);
        Ok(cleanup)
    }

    fn cleanup_client_connection_locked(
        inner: &mut ForwardingInner,
        connection_id: ConnectionId,
    ) -> Vec<GoodbyeTarget> {
        let routes = inner
            .client_to_module
            .iter()
            .filter(|(key, _)| key.connection_id == connection_id)
            .map(|(key, route)| (*key, route.client_epoch))
            .collect::<Vec<_>>();
        let mut released = Vec::with_capacity(routes.len());
        for (client_key, epoch) in routes {
            if let RouteRelease::Removed(target) =
                release_client_route_locked(inner, client_key, epoch)
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
                inner,
                pending.reservation.client_key,
                pending.reservation.module_key,
            );
            if pending.relay_enqueued {
                if let Some(target) = abandoned_route_target(inner, &pending.reservation) {
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
            release_reserved_route_locked(inner, client_key, module_key);
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

        released
    }

    /// Close a client connection whose egress queue refused a frame for the
    /// route `(connection_id, channel)` at `expected_epoch`. The whole
    /// connection closes because its queue is shared by every route on it.
    ///
    /// The first request that actually closes the connection logs one WARN with
    /// the diagnosis: which principals the connection's routes belonged to,
    /// which module's frame did not fit and on which client channel, and what
    /// the queue held at that moment.
    pub(crate) fn escalate_client_delivery_failure(
        &self,
        connection_id: ConnectionId,
        channel: u16,
        expected_epoch: u32,
        reason: CloseReason,
        undelivered: UndeliveredFrame<'_>,
    ) -> Result<bool, ForwardingError> {
        let principals = {
            let mut inner = self.write_inner()?;
            let key = ClientRouteKey {
                connection_id,
                channel,
            };
            if inner.last_published_epoch.get(&key).copied() != Some(expected_epoch) {
                None
            } else {
                inner.closing_connections.insert(connection_id);
                Some(connection_principals_locked(&inner, connection_id))
            }
        };
        let Some(principals) = principals else {
            return Ok(false);
        };
        let backlog = undelivered.sink.backlog();
        let close_reason = reason.to_string();
        if self.request_connection_close(connection_id, reason) {
            warn!(
                connection_id = connection_id.get(),
                principals = %principals,
                module_id = undelivered.module_id.unwrap_or("unknown"),
                client_channel = channel,
                queued_bytes = backlog.queued_bytes,
                queued_frames = backlog.queued_frames,
                oldest_queued_ms = backlog
                    .oldest_age
                    .map(|age| age.as_millis() as u64)
                    .unwrap_or(0),
                close_reason = %close_reason,
                "closing client connection: its egress queue could not take a frame"
            );
        }
        Ok(true)
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
                module_id: Some(route.module_id.clone()),
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
        module_id: Some(route.module_id.clone()),
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
        project_root: reservation.project_root.clone(),
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

    // Sending through the reserved slot cannot fail, but it reports a receiver
    // that closed after reservation and before this locked publication point.
    // The frame is stamped and charged to the queued-byte count here, when it
    // actually enters the queue, not when the slot was reserved.
    let client_writer_closed = pending.client_permit.send(pending.route_open_frame);
    if client_writer_closed {
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

/// The live module connection registered under exactly `endpoint`, whichever
/// slot holds it: active, swap candidate, or superseded incumbent.
///
/// Resolving by endpoint rather than by module id is what keeps an incumbent
/// addressable after a swap promoted a candidate over its id. An endpoint that
/// is in none of the three (a stale one, replaced without a promotion) resolves
/// to nothing, as it always has.
fn module_connection_for_endpoint_locked(
    inner: &ForwardingInner,
    endpoint: ModuleEndpointId,
) -> Option<&ModuleConnection> {
    let module_id = inner.module_id_by_endpoint.get(&endpoint)?;
    inner
        .modules_by_id
        .get(module_id)
        .filter(|module| module.endpoint == endpoint)
        .or_else(|| {
            inner
                .candidates_by_id
                .get(module_id)
                .filter(|module| module.endpoint == endpoint)
        })
        .or_else(|| inner.superseded_endpoints.get(&endpoint))
}

fn abandoned_route_target(
    inner: &ForwardingInner,
    reservation: &RouteReservation,
) -> Option<GoodbyeTarget> {
    let module_id = inner
        .module_id_by_endpoint
        .get(&reservation.module_key.endpoint)?;
    let module = module_connection_for_endpoint_locked(inner, reservation.module_key.endpoint)?;
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

/// Queue a registering module's HELLO_ACK on its sink. Called with the
/// forwarding write lock held, before the endpoint is inserted, which is what
/// puts the ack ahead of any `route.bind` or control RPC routed to the module.
/// `try_send` never waits, so holding the lock across it is safe.
fn enqueue_hello_ack_locked(
    sink: &FrameSink,
    connection_id: ConnectionId,
    hello_ack: Option<Frame>,
) -> Result<(), ForwardingError> {
    let Some(hello_ack) = hello_ack else {
        return Ok(());
    };
    sink.try_send(hello_ack)
        .map_err(|_| ForwardingError::ModuleEgressUnavailable { connection_id })
}

fn remove_module_connection_locked(
    inner: &mut ForwardingInner,
    endpoint: ModuleEndpointId,
) -> ConnectionCleanup {
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
        if inner
            .candidates_by_id
            .get(module_id)
            .is_some_and(|module| module.endpoint == endpoint)
        {
            inner.candidates_by_id.remove(module_id);
        }
    }
    inner.superseded_endpoints.remove(&endpoint);
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
    let abandoned_relays = u32::try_from(pending.len()).unwrap_or(u32::MAX);
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
    ConnectionCleanup {
        released,
        abandoned_relays,
    }
}

#[derive(Debug, Clone, Copy)]
struct RequestCredit {
    subscription: bool,
    excluded_from_drain: bool,
}

#[derive(Debug, Default)]
struct CreditLedger {
    by_corr: HashMap<u64, Vec<RequestCredit>>,
}

impl CreditLedger {
    fn acquire(&mut self, corr: u64, subscription: bool) {
        self.by_corr.entry(corr).or_default().push(RequestCredit {
            subscription,
            excluded_from_drain: false,
        });
    }

    fn release(&mut self, corr: u64) -> bool {
        let Some(credits) = self.by_corr.get_mut(&corr) else {
            return false;
        };
        let released = credits.pop().is_some();
        if credits.is_empty() {
            self.by_corr.remove(&corr);
        }
        released
    }

    fn capture_subscription_exclusions(&mut self) -> u32 {
        let mut excluded = 0u32;
        for credit in self.by_corr.values_mut().flatten() {
            if credit.subscription && !credit.excluded_from_drain {
                credit.excluded_from_drain = true;
                excluded = excluded.saturating_add(1);
            }
        }
        excluded
    }

    #[cfg(test)]
    fn in_flight(&self) -> usize {
        self.by_corr.values().map(Vec::len).sum()
    }

    fn drain_in_flight(&self) -> usize {
        self.by_corr
            .values()
            .flatten()
            .filter(|credit| !credit.excluded_from_drain)
            .count()
    }

    /// The correlation ids of the requests a drain still waits on, one entry
    /// per held credit, in ascending order.
    fn drain_held_corrs(&self) -> Vec<u64> {
        let mut corrs = self
            .by_corr
            .iter()
            .flat_map(|(corr, credits)| {
                credits
                    .iter()
                    .filter(|credit| !credit.excluded_from_drain)
                    .map(move |_| *corr)
            })
            .collect::<Vec<_>>();
        corrs.sort_unstable();
        corrs
    }
}

#[derive(Debug, Default)]
struct ChannelFlowState {
    closed: bool,
    credits: CreditLedger,
}

/// Per-channel request-credit accounting shared by the client and module route halves.
#[derive(Debug)]
pub(crate) struct ChannelFlow {
    sem: Semaphore,
    window: usize,
    state: Mutex<ChannelFlowState>,
}

impl ChannelFlow {
    pub(crate) fn new(window: usize) -> Self {
        debug_assert!(window > 0, "flow-control window must be non-zero");
        Self {
            sem: Semaphore::new(window),
            window,
            state: Mutex::new(ChannelFlowState::default()),
        }
    }

    #[cfg(test)]
    pub(crate) async fn acquire(&self) -> Result<(), ChannelFlowClosed> {
        self.acquire_tagged(0, false).await
    }

    pub(crate) async fn acquire_tagged(
        &self,
        corr: u64,
        subscription: bool,
    ) -> Result<(), ChannelFlowClosed> {
        let permit = self.sem.acquire().await.map_err(|_| ChannelFlowClosed)?;
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.closed {
            return Err(ChannelFlowClosed);
        }
        state.credits.acquire(corr, subscription);
        permit.forget();
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn release(&self) {
        self.release_corr(0);
    }

    pub(crate) fn release_corr(&self, corr: u64) {
        let released = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .credits
            .release(corr);
        if !released {
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
        if !self.sem.is_closed() {
            self.sem.add_permits(1);
        }
    }

    #[cfg(test)]
    pub(crate) fn in_flight(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .credits
            .in_flight()
    }

    pub(crate) fn drain_in_flight(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .credits
            .drain_in_flight()
    }

    pub(crate) fn drain_held_corrs(&self) -> Vec<u64> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .credits
            .drain_held_corrs()
    }

    #[cfg(test)]
    pub(crate) fn available_permits(&self) -> usize {
        self.sem.available_permits()
    }

    pub(crate) fn begin_drain(&self) -> u32 {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.closed = true;
        self.sem.close();
        state.credits.capture_subscription_exclusions()
    }

    pub(crate) fn close(&self) {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .closed = true;
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
    /// A swap candidate is already registered for this module id.
    CandidateSlotOccupied {
        module_id: String,
    },
    /// A registering module's HELLO_ACK could not be queued because its
    /// outbound queue is closed or full.
    ModuleEgressUnavailable {
        connection_id: ConnectionId,
    },
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
            Self::CandidateSlotOccupied { module_id } => write!(
                f,
                "module_id '{module_id}' already has a swap candidate registered"
            ),
            Self::ModuleEgressUnavailable { connection_id } => write!(
                f,
                "module connection {} egress is unavailable; HELLO_ACK could not be queued",
                connection_id.get()
            ),
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
    fn ordinary_long_running_request_is_not_excluded_from_drain() {
        let mut ledger = CreditLedger::default();
        ledger.acquire(1, false);

        assert_eq!(ledger.capture_subscription_exclusions(), 0);
        assert_eq!(ledger.drain_in_flight(), 1);
    }

    #[test]
    fn bit_set_subscription_is_excluded_and_counted() {
        let mut ledger = CreditLedger::default();
        ledger.acquire(1, true);

        assert_eq!(ledger.capture_subscription_exclusions(), 1);
        assert_eq!(ledger.drain_in_flight(), 0);
    }

    #[test]
    fn subscription_opened_after_drain_snapshot_is_not_excluded() {
        let mut ledger = CreditLedger::default();
        ledger.acquire(1, true);
        assert_eq!(ledger.capture_subscription_exclusions(), 1);

        ledger.acquire(2, true);

        assert_eq!(ledger.drain_in_flight(), 1);
    }

    #[test]
    fn drain_with_no_subscriptions_reports_zero_excluded() {
        let mut ledger = CreditLedger::default();
        assert_eq!(ledger.capture_subscription_exclusions(), 0);
    }

    fn test_hello_ack(corr: u64) -> Frame {
        Frame::build(
            FrameType::HelloAck,
            Flags::new(false, Priority::Passive, false),
            0,
            0,
            corr,
            Vec::new(),
        )
        .unwrap()
    }

    /// An acked registration whose HELLO_ACK cannot be queued must leave no
    /// endpoint behind: a routable module that never got its ack would read a
    /// route.bind first and exit. A closed queue and a full one both refuse.
    #[test]
    fn acked_registration_that_cannot_queue_its_hello_ack_inserts_nothing() {
        let forwarding = ForwardingTable::default();

        let (closed_tx, closed_rx) = mpsc::channel(8);
        drop(closed_rx);
        let closed = ConnectionId::new(1);
        assert_eq!(
            forwarding.register_module_connection_acked(
                closed,
                "closed".to_string(),
                2,
                Concurrency::ModuleManaged,
                FrameSink::new(closed_tx),
                test_hello_ack(1),
            ),
            Err(ForwardingError::ModuleEgressUnavailable {
                connection_id: closed
            })
        );

        let (full_tx, _full_rx) = mpsc::channel(1);
        let full_sink = FrameSink::new(full_tx);
        full_sink.try_send(test_hello_ack(99)).unwrap();
        let full = ConnectionId::new(2);
        assert_eq!(
            forwarding.register_module_connection_acked(
                full,
                "full".to_string(),
                2,
                Concurrency::ModuleManaged,
                full_sink.clone(),
                test_hello_ack(2),
            ),
            Err(ForwardingError::ModuleEgressUnavailable {
                connection_id: full
            })
        );
        assert_eq!(
            forwarding.register_candidate_module_connection_acked(
                full,
                "full".to_string(),
                2,
                Concurrency::ModuleManaged,
                full_sink,
                test_hello_ack(3),
            ),
            Err(ForwardingError::ModuleEgressUnavailable {
                connection_id: full
            })
        );

        for (connection, module_id) in [(closed, "closed"), (full, "full")] {
            assert_eq!(
                forwarding
                    .module_endpoint_for_connection(connection)
                    .unwrap(),
                None
            );
            let (client_tx, _client_rx) = mpsc::channel(8);
            assert_eq!(
                forwarding
                    .begin_route_bind_relay_for_test(
                        ConnectionId::new(50),
                        FrameSink::new(client_tx),
                        1,
                        module_id,
                    )
                    .err(),
                Some(ForwardingError::NoModuleConnection)
            );
        }
        assert!(forwarding.read_inner().unwrap().candidates_by_id.is_empty());
    }

    /// Both acked registration forms put the HELLO_ACK on the module's queue
    /// by the time the endpoint can be resolved.
    #[test]
    fn acked_registration_queues_the_hello_ack_first() {
        let forwarding = ForwardingTable::default();
        let (active_tx, mut active_rx) = mpsc::channel(8);
        forwarding
            .register_module_connection_acked(
                ConnectionId::new(1),
                "acked".to_string(),
                2,
                Concurrency::ModuleManaged,
                FrameSink::new(active_tx),
                test_hello_ack(11),
            )
            .unwrap();
        let (candidate_tx, mut candidate_rx) = mpsc::channel(8);
        forwarding
            .register_candidate_module_connection_acked(
                ConnectionId::new(2),
                "acked".to_string(),
                2,
                Concurrency::ModuleManaged,
                FrameSink::new(candidate_tx),
                test_hello_ack(12),
            )
            .unwrap();

        let active_first = active_rx.try_recv().unwrap().frame;
        assert_eq!(active_first.header.ty, FrameType::HelloAck);
        assert_eq!(active_first.header.corr, 11);
        let candidate_first = candidate_rx.try_recv().unwrap().frame;
        assert_eq!(candidate_first.header.ty, FrameType::HelloAck);
        assert_eq!(candidate_first.header.corr, 12);
    }

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

    #[test]
    #[cfg(unix)]
    fn daemon_drain_gates_current_and_racing_provider_registrations() {
        let (forwarding, _, endpoint, _, sink, _) = route_fixture("provider");
        assert_eq!(forwarding.begin_daemon_drain().unwrap(), ["provider"]);
        assert!(forwarding.endpoint_is_draining(endpoint).unwrap());
        assert!(matches!(
            forwarding.register_module_connection(
                ConnectionId::new(300),
                "late-provider".into(),
                2,
                Concurrency::ModuleManaged,
                sink,
            ),
            Err(ForwardingError::ConnectionClosing { .. })
        ));
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

    /// A pending route.open reserves its queue slot with `reserve_owned` and
    /// sends its prebuilt response through that slot when the module accepts.
    /// A queue already holding many data frames (far more than the old 64-frame
    /// queue) must not stop the open from reserving or completing, and the
    /// response must be charged to the queue's byte count when it is sent.
    #[tokio::test]
    async fn pending_route_open_completes_behind_queued_data_frames() {
        assert_eq!(
            crate::server::MAX_PENDING_ROUTE_OPENS_PER_CONNECTION,
            8,
            "the per-connection pending route.open limit is its own constant"
        );
        let (forwarding, module_connection, _endpoint, client, _unused_sink, _unused_rx) =
            route_fixture("open-behind-data");
        let (sink, mut client_rx) = crate::server::connection_egress();
        const DATA_FRAMES: usize = 1_000;
        let data = |corr: u64| {
            Frame::build(
                FrameType::StreamData,
                Flags::new(false, Priority::Interactive, false),
                9,
                1,
                corr,
                vec![b'x'; 200],
            )
            .unwrap()
        };
        for corr in 0..DATA_FRAMES as u64 {
            sink.try_send(data(corr)).unwrap();
        }
        let data_bytes = DATA_FRAMES * (subc_protocol::HEADER_LEN + 200);
        assert_eq!(sink.backlog().queued_bytes, data_bytes);

        let pending = tokio::time::timeout(
            Duration::from_secs(5),
            forwarding.begin_route_bind_relay_for(
                client,
                sink.clone(),
                subc_protocol::PROTOCOL_VERSION,
                4_242,
                "open-behind-data",
                Principal::Direct,
                None,
                Instant::now() + Duration::from_secs(60),
            ),
        )
        .await
        .expect("reserving the route.open slot must not wait behind data frames")
        .unwrap();
        forwarding
            .complete_pending_relay(
                module_connection,
                pending.corr,
                RouteBindRelayOutcome::Accepted,
            )
            .unwrap();

        let backlog = sink.backlog();
        assert_eq!(backlog.queued_frames, DATA_FRAMES + 1);
        assert!(
            backlog.queued_bytes > data_bytes,
            "the route.open response must be counted in queued bytes: {backlog:?}"
        );
        for corr in 0..DATA_FRAMES as u64 {
            assert_eq!(client_rx.recv().await.unwrap().header.corr, corr);
        }
        let open = client_rx.recv().await.unwrap();
        assert_eq!(open.header.corr, 4_242);
        assert_eq!(open.header.ty, FrameType::Response);
        drop(open);
        assert_eq!(sink.backlog().queued_bytes, 0);
        assert_eq!(sink.backlog().queued_frames, 0);
    }

    /// The drain-timeout line reports these numbers, so they must count the
    /// requests the drain is waiting on and no others: a route with nothing in
    /// flight is not a holdout, and a flagged subscription is excluded from the
    /// drain and so from the count.
    #[tokio::test]
    async fn drain_holdouts_count_held_requests_and_name_the_connection() {
        let (forwarding, module_connection, endpoint, client, sink, mut client_rx) =
            route_fixture("holdouts");
        let mut bound = |corr| {
            let route = begin_test_route(&forwarding, client, sink.clone(), corr, "holdouts");
            forwarding
                .complete_pending_relay(
                    module_connection,
                    route.corr,
                    RouteBindRelayOutcome::Accepted,
                )
                .unwrap();
            client_rx.try_recv().unwrap();
            match forwarding
                .lookup_data_route(client, route.client_channel, route.client_epoch)
                .unwrap()
            {
                DataRoute::Client(DataRouteState::Bound(binding)) => binding,
                other => panic!("expected live route, got {other:?}"),
            }
        };
        let holding = bound(61);
        let _idle = bound(62);
        holding.flow.acquire_tagged(7, false).await.unwrap();
        holding.flow.acquire_tagged(2, false).await.unwrap();
        holding.flow.acquire_tagged(3, true).await.unwrap();
        forwarding
            .begin_module_drain("holdouts", RouteCloseReason::Restart)
            .unwrap();

        let holdouts = forwarding.endpoint_drain_holdouts(endpoint).unwrap();
        assert_eq!(
            holdouts,
            DrainHoldouts {
                requests: 2,
                routes: 1,
                total_routes: 2,
                top_connections: vec![(client.get(), 2)],
                // The held requests by the module's channel and corr, ascending,
                // without the excluded subscription (corr 3).
                held: vec![(holding.module_channel, 2), (holding.module_channel, 7)],
            }
        );
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

        let probe_sink = FrameSink::new(mpsc::channel(1).0);
        let (no_successor, client, channel, epoch) = setup_successor(None);
        let mut close = no_successor.register_connection_close(client);
        assert!(no_successor
            .escalate_client_delivery_failure(
                client,
                channel,
                epoch,
                CloseReason::new("delivery", "failed"),
                UndeliveredFrame {
                    module_id: None,
                    sink: &probe_sink,
                },
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
                UndeliveredFrame {
                    module_id: None,
                    sink: &probe_sink,
                },
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
                UndeliveredFrame {
                    module_id: None,
                    sink: &probe_sink,
                },
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

    /// A client can be marked closing while its egress is still open: the daemon
    /// asks a connection to close (here through the production path, a module
    /// frame that its egress refused) and the connection loop tears down a moment
    /// later. A route.bind ack that lands inside that window is answered on the
    /// MODULE connection's frame handler, so resolving it must not produce an
    /// error -- an error there ends the module connection, and that connection
    /// carries every other client's routes to the module.
    #[test]
    fn accepted_bind_for_a_closing_client_releases_the_route_instead_of_failing_the_module() {
        let (forwarding, module_connection, endpoint, client, sink, mut client_rx) =
            route_fixture("closing-client");

        // A published route on this client: escalate_client_delivery_failure only
        // marks a connection closing for a route it has already published.
        let live = begin_test_route(&forwarding, client, sink.clone(), 60, "closing-client");
        forwarding
            .complete_pending_relay(
                module_connection,
                live.corr,
                RouteBindRelayOutcome::Accepted,
            )
            .unwrap();
        client_rx.try_recv().unwrap();

        // A second route.open from the same client, relayed and awaiting its ack.
        let pending = begin_test_route(&forwarding, client, sink.clone(), 61, "closing-client");
        forwarding
            .mark_route_bind_relay_enqueued(pending.endpoint, pending.corr)
            .unwrap();

        // The window: closing, but the sink is still open.
        assert!(forwarding
            .escalate_client_delivery_failure(
                client,
                live.client_channel,
                live.client_epoch,
                CloseReason::new(
                    "module_to_client_delivery_failed",
                    "client egress refused a module frame",
                ),
                UndeliveredFrame {
                    module_id: None,
                    sink: &sink,
                },
            )
            .unwrap());
        assert!(!sink.is_closed());

        let completion = forwarding
            .complete_pending_relay(
                module_connection,
                pending.corr,
                RouteBindRelayOutcome::Accepted,
            )
            .expect("a closing client must not turn a module's ack into an error");

        assert!(completion.settled);
        let abandoned = completion
            .abandoned
            .expect("the module must be told to drop the binding it just created");
        assert_eq!(abandoned.connection_id, module_connection);
        assert_eq!(abandoned.channel, pending.module_channel);
        assert_eq!(abandoned.epoch, pending.module_epoch);
        assert!(matches!(abandoned.kind, GoodbyeTargetKind::Module));
        assert!(matches!(
            pending.receiver.blocking_recv().unwrap(),
            RouteBindRelayOutcome::ModuleGone(_)
        ));
        // No route was published to a client that is on its way out, and the
        // reserved handle pair went back.
        assert!(client_rx.try_recv().is_err());
        assert_eq!(forwarding.active_binding_count().unwrap(), 1);

        // The module endpoint is untouched: still live, and still able to take a
        // route from another client.
        assert!(forwarding
            .has_live_module_connection("closing-client")
            .unwrap());
        let cotenant = ConnectionId::new(201);
        let (cotenant_tx, mut cotenant_rx) = mpsc::channel(8);
        let cotenant_route = begin_test_route(
            &forwarding,
            cotenant,
            FrameSink::new(cotenant_tx),
            62,
            "closing-client",
        );
        assert_eq!(cotenant_route.endpoint, endpoint);
        forwarding
            .complete_pending_relay(
                module_connection,
                cotenant_route.corr,
                RouteBindRelayOutcome::Accepted,
            )
            .unwrap();
        assert_eq!(cotenant_rx.try_recv().unwrap().header.corr, 62);
        assert_eq!(forwarding.active_binding_count().unwrap(), 2);
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

    /// Every connection teardown passes through `cleanup_connection`, and
    /// connection ids come from a monotonic counter that never hands an id out
    /// twice. If the closing mark survives teardown, the set grows by one entry
    /// per connection for the life of the daemon -- the self-watchdog alone
    /// reconnects once a minute.
    #[test]
    fn cleaned_up_connections_do_not_stay_in_the_closing_set() {
        let (forwarding, module_connection, _endpoint, _fixture_client, _sink, _rx) =
            route_fixture("closing-set-leak");

        const CONNECTIONS: u64 = 32;
        for index in 0..CONNECTIONS {
            let client = ConnectionId::new(1000 + index);
            let (client_tx, _client_rx) = mpsc::channel(8);
            let route = begin_test_route(
                &forwarding,
                client,
                FrameSink::new(client_tx),
                index + 1,
                "closing-set-leak",
            );
            forwarding
                .complete_pending_relay(
                    module_connection,
                    route.corr,
                    RouteBindRelayOutcome::Accepted,
                )
                .unwrap();
            forwarding.cleanup_connection(client).unwrap();
        }
        forwarding.cleanup_connection(module_connection).unwrap();

        assert_eq!(forwarding.closing_connection_count().unwrap(), 0);
    }

    /// The closing mark exists to refuse new work for a connection that is on
    /// its way out but whose teardown has not run yet: the daemon asks the
    /// connection loop to end, and only when the loop reacts does
    /// `cleanup_connection` strip the connection's state. Inside that window an
    /// operation for the dying connection must still be refused; only after
    /// cleanup completes may the mark go.
    #[test]
    fn closing_connection_is_refused_new_work_until_cleanup_completes() {
        let (forwarding, module_connection, _endpoint, client, sink, mut client_rx) =
            route_fixture("closing-gate");

        // A published route: escalate_client_delivery_failure only marks a
        // connection closing for a route it has already published.
        let live = begin_test_route(&forwarding, client, sink.clone(), 80, "closing-gate");
        forwarding
            .complete_pending_relay(
                module_connection,
                live.corr,
                RouteBindRelayOutcome::Accepted,
            )
            .unwrap();
        client_rx.try_recv().unwrap();

        // Mark the connection closing through the production path without
        // running teardown, pinning the window open.
        assert!(forwarding
            .escalate_client_delivery_failure(
                client,
                live.client_channel,
                live.client_epoch,
                CloseReason::new(
                    "module_to_client_delivery_failed",
                    "client egress refused a module frame",
                ),
                UndeliveredFrame {
                    module_id: None,
                    sink: &sink,
                },
            )
            .unwrap());
        assert_eq!(forwarding.closing_connection_count().unwrap(), 1);

        // A late route.open for the closing client is refused ...
        assert!(matches!(
            forwarding.begin_route_bind_relay_for_test(client, sink, 81, "closing-gate"),
            Err(ForwardingError::ConnectionClosing { connection_id })
                if connection_id == client
        ));
        // ... and so is a late attempt to register the connection as a module.
        let (late_tx, _late_rx) = mpsc::channel(1);
        assert!(matches!(
            forwarding.register_module_connection(
                client,
                "late-module".into(),
                2,
                Concurrency::ModuleManaged,
                FrameSink::new(late_tx),
            ),
            Err(ForwardingError::ConnectionClosing { connection_id })
                if connection_id == client
        ));

        // Teardown is the point that lifts the mark: it has just removed every
        // per-connection entry under the same lock, so the gate has nothing
        // left to protect for this id.
        forwarding.cleanup_connection(client).unwrap();
        assert_eq!(forwarding.closing_connection_count().unwrap(), 0);
    }
}

/// Blue/green swap slots: a candidate registered beside the active endpoint,
/// promoted by `cutover_candidate`, with the old incumbent drained by endpoint.
#[cfg(test)]
mod swap_slot_tests {
    use std::time::Duration;

    use super::*;
    use tokio::sync::mpsc;

    const MODULE_ID: &str = "swapped";

    struct SwapFixture {
        forwarding: ForwardingTable,
        incumbent_connection: ConnectionId,
        incumbent: ModuleEndpointId,
        candidate_connection: ConnectionId,
        candidate: ModuleEndpointId,
        _module_rxs: Vec<mpsc::Receiver<crate::router::OutboundFrame>>,
    }

    fn swap_fixture() -> SwapFixture {
        let forwarding = ForwardingTable::default();
        let incumbent_connection = ConnectionId::new(100);
        let candidate_connection = ConnectionId::new(110);
        let (incumbent_tx, incumbent_rx) = mpsc::channel(8);
        let incumbent = forwarding
            .register_module_connection(
                incumbent_connection,
                MODULE_ID.to_string(),
                2,
                Concurrency::ModuleManaged,
                FrameSink::new(incumbent_tx),
            )
            .unwrap();
        let (candidate_tx, candidate_rx) = mpsc::channel(8);
        let candidate = forwarding
            .register_candidate_module_connection(
                candidate_connection,
                MODULE_ID.to_string(),
                2,
                Concurrency::ModuleManaged,
                FrameSink::new(candidate_tx),
            )
            .unwrap();
        SwapFixture {
            forwarding,
            incumbent_connection,
            incumbent,
            candidate_connection,
            candidate,
            _module_rxs: vec![incumbent_rx, candidate_rx],
        }
    }

    fn client(
        raw: u64,
    ) -> (
        ConnectionId,
        FrameSink,
        mpsc::Receiver<crate::router::OutboundFrame>,
    ) {
        let (tx, rx) = mpsc::channel(8);
        (ConnectionId::new(raw), FrameSink::new(tx), rx)
    }

    fn committed_endpoints(forwarding: &ForwardingTable) -> Vec<ModuleEndpointId> {
        forwarding
            .read_inner()
            .unwrap()
            .client_to_module
            .values()
            .map(|route| route.module_endpoint)
            .collect()
    }

    #[test]
    fn candidate_is_unroutable_until_cutover_and_by_id_lookups_resolve_the_active_slot() {
        let fixture = swap_fixture();
        let forwarding = &fixture.forwarding;
        assert_ne!(fixture.incumbent, fixture.candidate);

        // Every by-id consumer still resolves the incumbent.
        assert!(forwarding.has_live_module_connection(MODULE_ID).unwrap());
        assert!(!forwarding.module_is_draining(MODULE_ID).unwrap());
        let (client_connection, client_sink, _client_rx) = client(200);
        let pending = forwarding
            .begin_route_bind_relay_for_test(client_connection, client_sink, 1, MODULE_ID)
            .unwrap();
        assert_eq!(pending.endpoint, fixture.incumbent);
        let rpc = forwarding
            .begin_module_control_rpc_for(
                MODULE_ID,
                "health.check",
                Instant::now() + Duration::from_secs(1),
            )
            .unwrap();
        assert_eq!(rpc.endpoint, fixture.incumbent);
        let census = forwarding.route_census(Some(MODULE_ID)).unwrap();
        assert_eq!(census.len(), 1, "the census lists one endpoint per id");

        // Connection-keyed lookups see the candidate, so its own frames resolve.
        assert_eq!(
            forwarding
                .module_endpoint_for_connection(fixture.candidate_connection)
                .unwrap(),
            Some(fixture.candidate)
        );
        assert_eq!(
            forwarding
                .module_id_for_connection(fixture.candidate_connection)
                .unwrap()
                .as_deref(),
            Some(MODULE_ID)
        );

        // One candidate per id.
        let (other_tx, _other_rx) = mpsc::channel(1);
        assert_eq!(
            forwarding.register_candidate_module_connection(
                ConnectionId::new(120),
                MODULE_ID.to_string(),
                2,
                Concurrency::ModuleManaged,
                FrameSink::new(other_tx),
            ),
            Err(ForwardingError::CandidateSlotOccupied {
                module_id: MODULE_ID.to_string()
            })
        );
    }

    /// The cutover linearization point. A relay reserved on the incumbent before
    /// cutover must never become a route on the incumbent, and every relay
    /// reserved after it must land on the promoted candidate.
    #[test]
    fn relay_reserved_before_cutover_never_commits_and_later_relays_land_on_the_candidate() {
        let fixture = swap_fixture();
        let forwarding = &fixture.forwarding;
        let (early_client, early_sink, _early_rx) = client(200);
        let mut early = forwarding
            .begin_route_bind_relay_for_test(early_client, early_sink, 1, MODULE_ID)
            .unwrap();
        assert_eq!(early.endpoint, fixture.incumbent);
        assert!(forwarding
            .mark_route_bind_relay_enqueued(early.endpoint, early.corr)
            .unwrap());

        let cutover = forwarding.cutover_candidate(MODULE_ID).unwrap().unwrap();
        assert_eq!(
            cutover,
            ForwardingCutover {
                promoted: fixture.candidate,
                incumbent: Some(fixture.incumbent),
            }
        );

        // A route.open reserved after cutover goes to the promoted candidate.
        let (late_client, late_sink, _late_rx) = client(201);
        let late = forwarding
            .begin_route_bind_relay_for_test(late_client, late_sink, 2, MODULE_ID)
            .unwrap();
        assert_eq!(
            late.endpoint, fixture.candidate,
            "a route.open after cutover was reserved on the incumbent"
        );

        // The incumbent acks the early relay after cutover.
        let completion = forwarding
            .complete_pending_relay(
                fixture.incumbent_connection,
                early.corr,
                RouteBindRelayOutcome::Accepted,
            )
            .expect("a superseded endpoint's ack is not an error on its connection");
        assert!(completion.settled);
        assert!(
            !committed_endpoints(forwarding).contains(&fixture.incumbent),
            "a relay reserved before cutover committed a route on the incumbent"
        );
        let goodbye = completion
            .abandoned
            .expect("the incumbent is told to drop the binding it just created");
        assert_eq!(goodbye.connection_id, fixture.incumbent_connection);
        assert_eq!(goodbye.channel, early.module_channel);
        assert_eq!(goodbye.epoch, early.module_epoch);
        assert_eq!(goodbye.kind, GoodbyeTargetKind::Module);
        match early.receiver.try_recv() {
            Ok(RouteBindRelayOutcome::Rejected(body)) => assert_eq!(body.code, "module_reloading"),
            other => panic!("expected a retryable module_reloading answer, got {other:?}"),
        }
        assert!(matches!(
            forwarding
                .lookup_data_route(early_client, early.client_channel, early.client_epoch)
                .unwrap(),
            DataRoute::Client(DataRouteState::Absent)
        ));

        // The late relay commits on the candidate; only its pair stays reserved
        // until then.
        assert_eq!(forwarding.reserved_route_count().unwrap(), (1, 1));
        forwarding
            .complete_pending_relay(
                fixture.candidate_connection,
                late.corr,
                RouteBindRelayOutcome::Accepted,
            )
            .unwrap();
        assert_eq!(forwarding.reserved_route_count().unwrap(), (0, 0));
        assert_eq!(committed_endpoints(forwarding), vec![fixture.candidate]);
    }

    #[test]
    fn endpoint_drain_after_cutover_drains_the_incumbent_not_the_promoted_candidate() {
        let fixture = swap_fixture();
        let forwarding = &fixture.forwarding;
        // One bound route and one in-flight relay on the incumbent.
        let (bound_client, bound_sink, _bound_rx) = client(200);
        let bound = forwarding
            .begin_route_bind_relay_for_test(bound_client, bound_sink, 1, MODULE_ID)
            .unwrap();
        forwarding
            .complete_pending_relay(
                fixture.incumbent_connection,
                bound.corr,
                RouteBindRelayOutcome::Accepted,
            )
            .unwrap();
        let (pending_client, pending_sink, _pending_rx) = client(201);
        let mut in_flight = forwarding
            .begin_route_bind_relay_for_test(pending_client, pending_sink, 2, MODULE_ID)
            .unwrap();
        forwarding
            .mark_route_bind_relay_enqueued(in_flight.endpoint, in_flight.corr)
            .unwrap();

        let incumbent = forwarding
            .cutover_candidate(MODULE_ID)
            .unwrap()
            .unwrap()
            .incumbent
            .unwrap();
        let target = forwarding
            .begin_endpoint_drain(incumbent, RouteCloseReason::Restart)
            .unwrap()
            .expect("the superseded incumbent is still registered");

        assert_eq!(target.endpoint, fixture.incumbent);
        assert!(forwarding.endpoint_is_draining(fixture.incumbent).unwrap());
        assert!(!forwarding.endpoint_is_draining(fixture.candidate).unwrap());
        assert!(!forwarding.module_is_draining(MODULE_ID).unwrap());
        assert_eq!(target.abandoned_bindings.len(), 1);
        assert_eq!(
            target.abandoned_bindings[0].channel,
            in_flight.module_channel
        );
        assert!(matches!(
            in_flight.receiver.try_recv(),
            Ok(RouteBindRelayOutcome::Rejected(body)) if body.code == "module_reloading"
        ));
        assert_eq!(
            forwarding.endpoint_routes(fixture.incumbent).unwrap().len(),
            1,
            "the incumbent's bound route stays until its drain finishes"
        );

        let (next_client, next_sink, _next_rx) = client(202);
        let next = forwarding
            .begin_route_bind_relay_for_test(next_client, next_sink, 3, MODULE_ID)
            .expect("the promoted candidate keeps accepting routes");
        assert_eq!(next.endpoint, fixture.candidate);
    }

    /// An endpoint replaced WITHOUT a promotion (a successor registered over it
    /// as an ordinary active HELLO) is stale, not superseded, and its ack still
    /// fails the acking connection exactly as it did before swap slots existed:
    /// `StaleModuleEndpoint`, the reservation indexes already stripped, and the
    /// waiting client's sender dropped unanswered.
    #[test]
    fn stale_endpoint_ack_without_a_promotion_still_fails_as_before() {
        let forwarding = ForwardingTable::default();
        let first_connection = ConnectionId::new(70);
        let (first_tx, _first_rx) = mpsc::channel(8);
        forwarding
            .register_module_connection(
                first_connection,
                MODULE_ID.to_string(),
                2,
                Concurrency::ModuleManaged,
                FrameSink::new(first_tx),
            )
            .unwrap();
        let (client_connection, client_sink, _client_rx) = client(200);
        let mut pending = forwarding
            .begin_route_bind_relay_for_test(client_connection, client_sink, 1, MODULE_ID)
            .unwrap();
        let (second_tx, _second_rx) = mpsc::channel(8);
        forwarding
            .register_module_connection(
                ConnectionId::new(80),
                MODULE_ID.to_string(),
                2,
                Concurrency::ModuleManaged,
                FrameSink::new(second_tx),
            )
            .unwrap();

        assert_eq!(
            forwarding
                .complete_pending_relay(
                    first_connection,
                    pending.corr,
                    RouteBindRelayOutcome::Accepted
                )
                .unwrap_err(),
            ForwardingError::StaleModuleEndpoint
        );
        assert!(committed_endpoints(&forwarding).is_empty());
        assert_eq!(forwarding.reserved_route_count().unwrap(), (0, 0));
        assert!(matches!(
            pending.receiver.try_recv(),
            Err(oneshot::error::TryRecvError::Closed)
        ));
    }

    #[test]
    fn cleanup_releases_candidate_and_superseded_slots_without_touching_the_active_one() {
        // A candidate whose connection drops leaves the incumbent routable.
        let fixture = swap_fixture();
        let forwarding = &fixture.forwarding;
        assert!(forwarding
            .cleanup_connection(fixture.candidate_connection)
            .unwrap()
            .is_empty());
        assert_eq!(forwarding.cutover_candidate(MODULE_ID).unwrap(), None);
        let (client_connection, client_sink, _client_rx) = client(200);
        assert_eq!(
            forwarding
                .begin_route_bind_relay_for_test(client_connection, client_sink, 1, MODULE_ID)
                .unwrap()
                .endpoint,
            fixture.incumbent
        );

        // After a cutover, the incumbent's teardown releases its own routes and
        // leaves the promoted candidate in place.
        let fixture = swap_fixture();
        let forwarding = &fixture.forwarding;
        let (bound_client, bound_sink, _bound_rx) = client(200);
        let bound = forwarding
            .begin_route_bind_relay_for_test(bound_client, bound_sink, 1, MODULE_ID)
            .unwrap();
        forwarding
            .complete_pending_relay(
                fixture.incumbent_connection,
                bound.corr,
                RouteBindRelayOutcome::Accepted,
            )
            .unwrap();
        forwarding.cutover_candidate(MODULE_ID).unwrap().unwrap();
        let released = forwarding
            .cleanup_connection(fixture.incumbent_connection)
            .unwrap();
        assert_eq!(released.len(), 1);
        assert_eq!(released[0].connection_id, bound_client);
        assert!(forwarding
            .read_inner()
            .unwrap()
            .superseded_endpoints
            .is_empty());
        assert!(forwarding.has_live_module_connection(MODULE_ID).unwrap());
        let (next_client, next_sink, _next_rx) = client(201);
        assert_eq!(
            forwarding
                .begin_route_bind_relay_for_test(next_client, next_sink, 2, MODULE_ID)
                .unwrap()
                .endpoint,
            fixture.candidate
        );
    }
}
