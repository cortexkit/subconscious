use std::{
    collections::{BTreeSet, HashSet},
    fmt,
    sync::Arc,
    time::Duration,
};

use serde::{Deserialize, Serialize};
use subc_control::{
    ops, CatalogEntry, ClientControlRequest, ClientControlResponse, ConsumerIdentity, PollKind,
    SupervisorEntry, SupervisorHealthEntry,
};
use subc_protocol::{
    manifest::{Concurrency, ModuleManifest, ProviderRole},
    session::{
        HealthReport, ModuleControlPush, ModuleControlRequest, ModuleControlRequestFromModule,
        ModuleControlResponse, ModuleControlResponseToModule, MODULE_CONTROL_OP_HEALTH_CHECK,
        MODULE_TO_SUBC_OP_CATALOG_UPDATE,
    },
    BindIdentity, ErrorBody, Flags, FrameType, ModuleHelloAckBody, ModuleHelloBody, Principal,
    Priority, RouteTarget, PROTOCOL_VERSION,
};
use tokio::time::timeout;
use tokio::time::{timeout_at, Instant};
use tracing::{debug, info, warn};

use crate::{
    forwarding::{
        CloseReason, ForwardingError, ForwardingTable, GoodbyeTarget, ModuleControlRpcOutcome,
        ModuleEndpointId, PendingModuleControlRpc, RouteBindRelayOutcome,
    },
    registry::{ChannelState, ConnectionId, Registry, RegistryError},
    router::{RouteCtx, RouterError},
    supervise::{ModuleProcessLiveness, ReservedHelloRejection, SupervisorHandle},
    ConnectedClients, Frame, ProjectRootId,
};

/// Lowest envelope version this subc build will negotiate.
///
/// The v1 hardening floor is intentionally equal to the current locked envelope
/// version. A HELLO below this floor receives a typed `version_unsupported`
/// ERROR and is not registered.
pub const MIN_SUPPORTED_VERSION: u8 = 1;

const CAP_MANIFEST_REGISTRATION: &str = "manifest_registration_v1";
const CAP_CHANNEL_LIFECYCLE: &str = "channel_lifecycle_v1";
const CAP_PING_PONG: &str = "ping_pong_v1";
const CAP_SESSION_ATTACH: &str = "session_attach_v1";

const SUBC_CONTROL_OPS: &[&str] = &[
    ops::SERVER_DESCRIBE,
    ops::CATALOG_LIST,
    ops::ROUTE_OPEN,
    ops::ROUTE_POLL,
    ops::SUPERVISOR_LIST,
    ops::SUPERVISOR_RESTART,
    ops::SUPERVISOR_RELOAD,
    ops::SUPERVISOR_SET_ENABLED,
    ops::SUPERVISOR_HEALTH_PROBE,
    ops::SUPERVISOR_HEALTH,
];

const MODULE_TO_SUBC_CONTROL_OPS: &[&str] = &[MODULE_TO_SUBC_OP_CATALOG_UPDATE];

const MODULE_BASELINE_CONTROL_OPS: &[&str] = &["route.bind", "route.status"];

/// How long subc waits for a module to ack a relayed route.bind before returning
/// `module_timeout`. The ack waits on the module's own configure, which for AFT
/// includes a synchronous bounded project walk (up to ~20k files) plus gitignore
/// and DB-open work — on a cold page cache or a large repo that legitimately
/// exceeds a couple of seconds. The default is generous because rejecting a VALID
/// bind is far worse than waiting on a slow one; a consumer that wants a tighter
/// bound retries the bind itself (the sanctioned warm-bind-retry pattern).
const DEFAULT_ROUTE_BIND_RELAY_TIMEOUT: Duration = Duration::from_secs(12);
const DEFAULT_HEALTH_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Real channel-0 control handler for subc itself.
#[derive(Clone)]
pub struct ControlHandler {
    registry: Arc<Registry>,
    forwarding: Arc<ForwardingTable>,
    process_liveness: Option<Arc<dyn ModuleProcessLiveness>>,
    supervisor: SupervisorHandle,
    subc_capabilities: Arc<[String]>,
    route_bind_relay_timeout: Duration,
    health_probe_timeout: Duration,
    /// Central storage policy. When set, each registering module receives its
    /// resolved storage descriptor in HELLO_ACK; `None` leaves the field absent.
    storage_config: Option<crate::daemon_config::StorageConfig>,
    connected_clients: ConnectedClients,
}

impl fmt::Debug for ControlHandler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ControlHandler")
            .field("registry", &self.registry)
            .field("forwarding", &self.forwarding)
            .field("process_liveness", &self.process_liveness.is_some())
            .field("supervisor", &self.supervisor)
            .field("subc_capabilities", &self.subc_capabilities)
            .finish()
    }
}

struct RouteBindReservationGuard {
    forwarding: Arc<ForwardingTable>,
    endpoint: ModuleEndpointId,
    client_connection_id: ConnectionId,
    client_channel: u16,
    module_channel: u16,
    relay_corr: u64,
    armed: bool,
}

struct ModuleControlRpcGuard {
    forwarding: Arc<ForwardingTable>,
    endpoint: ModuleEndpointId,
    corr: u64,
    armed: bool,
}

impl ModuleControlRpcGuard {
    fn new(forwarding: Arc<ForwardingTable>, endpoint: ModuleEndpointId, corr: u64) -> Self {
        Self {
            forwarding,
            endpoint,
            corr,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ModuleControlRpcGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = self
                .forwarding
                .cancel_module_control_rpc(self.endpoint, self.corr);
        }
    }
}

impl RouteBindReservationGuard {
    fn new(
        forwarding: Arc<ForwardingTable>,
        endpoint: ModuleEndpointId,
        client_connection_id: ConnectionId,
        client_channel: u16,
        module_channel: u16,
        relay_corr: u64,
    ) -> Self {
        Self {
            forwarding,
            endpoint,
            client_connection_id,
            client_channel,
            module_channel,
            relay_corr,
            armed: true,
        }
    }

    fn release_and_disarm(&mut self) {
        if !self.armed {
            return;
        }
        let _ = self
            .forwarding
            .cancel_pending_relay(self.endpoint, self.relay_corr);
        let _ = self.forwarding.release_reserved_route(
            self.client_connection_id,
            self.client_channel,
            self.endpoint,
            self.module_channel,
        );
        self.armed = false;
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for RouteBindReservationGuard {
    fn drop(&mut self) {
        self.release_and_disarm();
    }
}

impl ControlHandler {
    pub fn new(registry: Arc<Registry>) -> Self {
        Self::with_forwarding(registry, Arc::new(ForwardingTable::default()))
    }

    pub fn with_forwarding(registry: Arc<Registry>, forwarding: Arc<ForwardingTable>) -> Self {
        Self {
            registry,
            forwarding,
            process_liveness: None,
            supervisor: SupervisorHandle::new(),
            subc_capabilities: Arc::from([
                CAP_MANIFEST_REGISTRATION.to_string(),
                CAP_CHANNEL_LIFECYCLE.to_string(),
                CAP_PING_PONG.to_string(),
                CAP_SESSION_ATTACH.to_string(),
            ]),
            route_bind_relay_timeout: DEFAULT_ROUTE_BIND_RELAY_TIMEOUT,
            health_probe_timeout: DEFAULT_HEALTH_PROBE_TIMEOUT,
            storage_config: None,
            connected_clients: ConnectedClients::new(),
        }
    }

    /// Set the central storage policy: registering modules then receive their
    /// resolved storage descriptor in HELLO_ACK.
    pub fn with_storage_config(
        mut self,
        storage_config: Option<crate::daemon_config::StorageConfig>,
    ) -> Self {
        self.storage_config = storage_config;
        self
    }

    /// Override the route.bind relay timeout. Used by tests that assert the
    /// timeout path so they don't block on the production-safe default.
    pub fn with_route_bind_relay_timeout(mut self, timeout: Duration) -> Self {
        self.route_bind_relay_timeout = timeout;
        self
    }

    #[cfg(test)]
    pub(crate) fn with_health_probe_timeout(mut self, timeout: Duration) -> Self {
        self.health_probe_timeout = timeout;
        self
    }

    pub fn with_process_liveness(
        mut self,
        process_liveness: Arc<dyn ModuleProcessLiveness>,
    ) -> Self {
        self.process_liveness = Some(process_liveness);
        self
    }

    pub fn with_supervisor(mut self, supervisor: SupervisorHandle) -> Self {
        self.supervisor = supervisor;
        self
    }

    pub fn with_connected_clients(mut self, connected_clients: ConnectedClients) -> Self {
        self.connected_clients = connected_clients;
        self
    }

    pub fn forwarding(&self) -> Arc<ForwardingTable> {
        Arc::clone(&self.forwarding)
    }

    /// Remove a connection's registry entries WITHOUT signalling the supervisor's
    /// registration-release watch. The signal is what the supervisor waits on
    /// before spawning a replacement, so it must only fire once forwarding
    /// teardown is also done (see [`Self::cleanup_connection`] /
    /// [`Self::handle_goodbye`]). Used directly only where there is no forwarding
    /// state to tear down (a HELLO that failed before module registration).
    fn deregister_connection(
        &self,
        connection_id: ConnectionId,
    ) -> Result<Vec<crate::registry::ModuleRegistration>, RegistryError> {
        self.registry.deregister_connection(connection_id)
    }

    /// Test-only compatibility entry point for unit control handling that does not have a socket sink.
    ///
    /// The real server path uses [`Self::handle_control_frame`] so module HELLO registration can
    /// record the module connection's [`crate::FrameSink`] and session attach can await the module
    /// relay response. This seam stays cfg(test) so production has only one channel-0 path.
    #[cfg(test)]
    pub fn handle_control(
        &self,
        connection_id: ConnectionId,
        frame: Frame,
    ) -> Result<Vec<Frame>, RouterError> {
        match frame.header.ty {
            FrameType::Ping => Ok(vec![pong(&frame)?]),
            FrameType::Hello => self.handle_hello(connection_id, None, frame),
            FrameType::Goodbye => self.handle_goodbye(connection_id),
            ty => Ok(vec![control_error_frame(
                &frame,
                "unsupported_control_frame",
                format!("unsupported channel-0 frame {ty:?}"),
            )?]),
        }
    }

    pub async fn handle_control_frame(
        &self,
        ctx: &RouteCtx,
        frame: Frame,
    ) -> Result<Vec<Frame>, RouterError> {
        match frame.header.ty {
            FrameType::Ping => Ok(vec![pong(&frame)?]),
            FrameType::Hello => {
                self.handle_hello(ctx.connection_id, Some(ctx.egress.clone()), frame)
            }
            FrameType::Goodbye => self.handle_goodbye(ctx.connection_id),
            FrameType::Request => {
                if self
                    .forwarding
                    .module_endpoint_for_connection(ctx.connection_id)
                    .map_err(RouterError::Forwarding)?
                    .is_some()
                {
                    if !is_known_module_request_op(&frame.body) {
                        return Ok(vec![control_error_frame(
                            &frame,
                            "unsupported_control_frame",
                            "module-originated channel-0 REQUEST is not supported",
                        )?]);
                    }
                    let request = match parse_module_control_request_from_module(&frame.body) {
                        Ok(request) => request,
                        Err((err, ControlRequestBodyError::UnknownOp)) => {
                            return Ok(vec![control_error_frame(
                                &frame,
                                "unsupported_control_frame",
                                format!("unsupported module-originated channel-0 REQUEST: {err}"),
                            )?])
                        }
                        Err((err, ControlRequestBodyError::InvalidBody)) => {
                            return Ok(vec![control_error_frame(
                                &frame,
                                "invalid_control_body",
                                format!("malformed module control body: {err}"),
                            )?])
                        }
                    };
                    return self.handle_module_control_request(ctx.connection_id, frame, request);
                }

                if is_known_module_request_op(&frame.body) {
                    return Ok(vec![control_error_frame(
                        &frame,
                        "not_registered",
                        "catalog.update requires an active module registration owned by this connection",
                    )?]);
                }

                let request = match parse_client_control_request(&frame.body) {
                    Ok(request) => request,
                    Err((err, ControlRequestBodyError::UnknownOp)) => {
                        return Ok(vec![control_error_frame(
                            &frame,
                            "unknown_control_op",
                            format!("unknown client control op: {err}"),
                        )?])
                    }
                    Err((err, ControlRequestBodyError::InvalidBody)) => {
                        return Ok(vec![control_error_frame(
                            &frame,
                            "invalid_control_body",
                            format!("malformed client control body: {err}"),
                        )?])
                    }
                };
                self.handle_client_control_request(ctx, frame, request)
                    .await
            }
            FrameType::Push => {
                let Some(endpoint) = self
                    .forwarding
                    .module_endpoint_for_connection(ctx.connection_id)
                    .map_err(RouterError::Forwarding)?
                else {
                    return Ok(vec![control_error_frame(
                        &frame,
                        "unsupported_control_frame",
                        "client-originated channel-0 PUSH is not supported",
                    )?]);
                };
                self.handle_status_update(endpoint, frame)
            }
            FrameType::Response | FrameType::Error
                if self
                    .forwarding
                    .module_endpoint_for_connection(ctx.connection_id)
                    .map_err(RouterError::Forwarding)?
                    .is_some() =>
            {
                self.handle_module_relay_response(ctx.connection_id, frame)
            }
            ty => Ok(vec![control_error_frame(
                &frame,
                "unsupported_control_frame",
                format!("unsupported channel-0 frame {ty:?}"),
            )?]),
        }
    }

    pub fn cleanup_connection(
        &self,
        connection_id: ConnectionId,
    ) -> Result<Vec<crate::registry::ModuleRegistration>, RegistryError> {
        let registrations = self.deregister_connection(connection_id);
        if let Ok(released_routes) = self.forwarding.cleanup_connection(connection_id) {
            self.emit_route_goodbyes(released_routes);
        }
        // Signal the registration-release watch only now that BOTH registry and
        // forwarding teardown are done, so a supervisor waiting to spawn a
        // replacement never observes release while old routes still exist.
        if matches!(&registrations, Ok(r) if !r.is_empty()) {
            crate::supervise::notify_registration_release();
        }
        registrations
    }

    pub(crate) fn handle_route_goodbye(
        &self,
        connection_id: ConnectionId,
        route_channel: u16,
    ) -> Result<bool, RouterError> {
        debug!(
            connection_id = connection_id.get(),
            route_channel, "handling route GOODBYE"
        );
        let Some(released_route) = self
            .forwarding
            .release_client_route(connection_id, route_channel)
            .map_err(RouterError::Forwarding)?
        else {
            return Ok(false);
        };
        self.emit_route_goodbyes(vec![released_route]);
        Ok(true)
    }

    fn emit_route_goodbyes(&self, released_routes: Vec<GoodbyeTarget>) {
        for released in released_routes {
            let frame = match Frame::build_with_version(
                released.negotiated_ver,
                FrameType::Goodbye,
                control_flags(),
                released.channel,
                0,
                Vec::new(),
            ) {
                Ok(frame) => frame,
                Err(err) => {
                    warn!(
                        route_channel = released.channel,
                        error = %err,
                        "failed to build route GOODBYE frame"
                    );
                    continue;
                }
            };
            if let Err(err) = released.sink.try_send(frame) {
                if released.close_on_delivery_failure() {
                    warn!(
                        target_connection_id = released.connection_id.get(),
                        route_channel = released.channel,
                        error = %err,
                        "route GOODBYE was not delivered to client; closing target connection"
                    );
                    self.forwarding.request_connection_close(
                        released.connection_id,
                        CloseReason::new(
                            "route_goodbye_delivery_failed",
                            format!(
                                "failed to enqueue route GOODBYE for channel {}: {err}",
                                released.channel
                            ),
                        ),
                    );
                } else {
                    warn!(
                        target_connection_id = released.connection_id.get(),
                        route_channel = released.channel,
                        error = %err,
                        "route GOODBYE to module dropped under backpressure; not closing shared module connection"
                    );
                }
            }
        }
    }

    /// Best-effort GOODBYE to a module for a route channel subc reserved but then
    /// abandoned (route.bind relay timed out, its waiter was cancelled, or subc's
    /// own commit failed after the module had already accepted). Without this, a
    /// module that accepts late keeps a binding subc has torn down, so a later
    /// frame on that module channel could misdeliver if the channel is reused.
    ///
    /// Never closes the shared module connection on failure: a dropped notification
    /// only wastes a bounded amount of warm module-side state, which the module's
    /// own idle reaper reclaims. Only call this once the route.bind relay was
    /// actually enqueued to the module — if the relay send itself failed, the
    /// module never created a binding and there is nothing to tear down.
    fn send_abandoned_route_bind_goodbye(
        &self,
        module_sink: &crate::FrameSink,
        negotiated_ver: u8,
        module_channel: u16,
    ) {
        let frame = match Frame::build_with_version(
            negotiated_ver,
            FrameType::Goodbye,
            control_flags(),
            module_channel,
            0,
            Vec::new(),
        ) {
            Ok(frame) => frame,
            Err(err) => {
                warn!(
                    route_channel = module_channel,
                    error = %err,
                    "failed to build GOODBYE for abandoned route.bind"
                );
                return;
            }
        };
        if let Err(err) = module_sink.try_send(frame) {
            warn!(
                route_channel = module_channel,
                error = %err,
                "GOODBYE for abandoned route.bind dropped; module idle reaper will reclaim the binding"
            );
        }
    }

    fn handle_hello(
        &self,
        connection_id: ConnectionId,
        sink: Option<crate::FrameSink>,
        frame: Frame,
    ) -> Result<Vec<Frame>, RouterError> {
        debug!(
            connection_id = connection_id.get(),
            corr = frame.header.corr,
            "handling HELLO"
        );
        let hello = match serde_json::from_slice::<ModuleHelloBody>(&frame.body) {
            Ok(hello) => hello,
            Err(err) => {
                return Ok(vec![control_error_frame(
                    &frame,
                    "invalid_hello",
                    format!("malformed HELLO body: {err}"),
                )?])
            }
        };

        if hello.protocol_ver != hello.manifest.protocol_ver {
            return Ok(vec![control_error_frame(
                &frame,
                "invalid_manifest",
                format!(
                    "HELLO protocol_ver {} does not match manifest protocol_ver {}",
                    hello.protocol_ver, hello.manifest.protocol_ver
                ),
            )?]);
        }

        if hello.manifest.module_id.trim().is_empty() {
            return Ok(vec![control_error_frame(
                &frame,
                "invalid_manifest",
                "manifest module_id must not be empty",
            )?]);
        }

        let negotiated_ver = match negotiate_version(hello.protocol_ver) {
            Ok(negotiated_ver) => negotiated_ver,
            Err(message) => {
                return Ok(vec![control_error_frame(
                    &frame,
                    "version_unsupported",
                    message,
                )?])
            }
        };

        // Reserved-module identity gate: a module_id configured `reserved` may be
        // registered ONLY by the process subc spawned for it, proven by echoing the
        // one-time launch nonce subc injected. A non-reserved id has no recorded
        // nonce and always passes. This blocks a key-holder from impersonating a
        // security-boundary module (e.g. the credential vault) while the real one is
        // down/restarting and its registration slot is momentarily free.
        if let Some(rejection) = self
            .supervisor
            .reserved_hello_rejection(&hello.manifest.module_id, hello.launch_nonce.as_deref())
        {
            let message = match rejection {
                ReservedHelloRejection::Exact { module_id } => format!(
                    "module_id '{module_id}' is reserved; HELLO without a valid launch nonce is rejected"
                ),
                ReservedHelloRejection::Prefix {
                    prefix,
                    owner_module_id,
                } => format!(
                    "module_id '{}' matches reserved prefix '{prefix}' owned by '{owner_module_id}'; HELLO without the owner launch nonce is rejected",
                    hello.manifest.module_id
                ),
            };
            return Ok(vec![control_error_frame(
                &frame,
                "reserved_module",
                message,
            )?]);
        }

        // A connection that already opened client routes must not also register as
        // a module: cleanup would then release only one side and leak the other.
        if self
            .forwarding
            .connection_has_client_routes(connection_id)
            .map_err(RouterError::Forwarding)?
        {
            return Ok(vec![control_error_frame(
                &frame,
                "invalid_hello",
                "connection has open client routes and cannot also register as a module",
            )?]);
        }

        let control_ops = effective_module_control_ops(hello.control_ops);
        let registration = match self.registry.register_with_control_ops(
            hello.manifest,
            negotiated_ver,
            connection_id,
            control_ops,
        ) {
            Ok(registration) => registration,
            Err(RegistryError::DuplicateModuleId { module_id }) => {
                return Ok(vec![control_error_frame(
                    &frame,
                    "duplicate_module_id",
                    format!(
                        "module_id '{module_id}' is already registered; duplicate HELLO rejected"
                    ),
                )?])
            }
            Err(err) => {
                return Ok(vec![control_error_frame(
                    &frame,
                    "registry_error",
                    err.to_string(),
                )?])
            }
        };

        if let Some(sink) = sink {
            // The forwarding table's module store is also the daemon-to-module
            // control-RPC lane, so every HELLO gets a live endpoint even when the
            // manifest has no routable provider role. Non-routable modules still
            // cannot receive route.bind in production: `handle_route_open` checks
            // the registry manifest with `target_has_required_role` before the
            // only production call to `begin_route_bind_relay_for` below that
            // route.open path. The remaining direct relay callers are unit tests
            // and benchmark harnesses that construct forwarding state explicitly.
            let concurrency = manifest_concurrency(&registration.manifest);
            if let Err(err) = self.forwarding.register_module_connection(
                connection_id,
                registration.manifest.module_id.clone(),
                negotiated_ver,
                concurrency,
                sink,
            ) {
                // Forwarding registration failed, so there is no forwarding
                // state to tear down. Remove the registry entry and signal the
                // release watch directly.
                if matches!(self.deregister_connection(connection_id), Ok(r) if !r.is_empty()) {
                    crate::supervise::notify_registration_release();
                }
                return Ok(vec![control_error_frame(
                    &frame,
                    forwarding_error_code(&err),
                    err.to_string(),
                )?]);
            }
        }

        info!(
            module_id = %registration.manifest.module_id,
            module_version = %registration.manifest.module_version,
            negotiated_ver,
            routable_provider = manifest_provides_routable_role(&registration.manifest),
            connection_id = connection_id.get(),
            "module registered"
        );

        let ack = ModuleHelloAckBody {
            negotiated_ver,
            subc_ops: module_subc_ops(),
            subc_capabilities: self.subc_capabilities.as_ref().to_vec(),
            storage: self
                .storage_config
                .as_ref()
                .map(|cfg| cfg.descriptor_for(&registration.manifest.module_id)),
        };
        let body = serde_json::to_vec(&ack).map_err(|err| {
            RouterError::backend(
                0,
                frame.header.corr,
                format!("failed to encode HELLO_ACK: {err}"),
            )
        })?;

        Ok(vec![Frame::build_with_version(
            negotiated_ver,
            FrameType::HelloAck,
            control_flags(),
            0,
            frame.header.corr,
            body,
        )
        .map_err(RouterError::FrameBuild)?])
    }

    async fn handle_client_control_request(
        &self,
        ctx: &RouteCtx,
        frame: Frame,
        request: ClientControlRequest,
    ) -> Result<Vec<Frame>, RouterError> {
        match request {
            ClientControlRequest::ServerDescribe {} => self.handle_server_describe(frame),
            ClientControlRequest::CatalogList { module_id } => {
                self.handle_catalog_list(frame, module_id)
            }
            ClientControlRequest::RouteOpen {
                target,
                identity,
                consumer_identity,
                consumer_capabilities,
            } => {
                self.handle_route_open(
                    ctx,
                    frame,
                    target,
                    identity,
                    consumer_identity,
                    consumer_capabilities,
                )
                .await
            }
            ClientControlRequest::RoutePoll {
                route_channel,
                kind,
            } => self.handle_route_poll(ctx, frame, route_channel, kind),
            ClientControlRequest::SupervisorList {} => self.handle_supervisor_list(frame),
            ClientControlRequest::SupervisorRestart { module_id } => {
                self.handle_supervisor_restart(frame, module_id).await
            }
            ClientControlRequest::SupervisorReload { module_id } => {
                self.handle_supervisor_reload(frame, module_id).await
            }
            ClientControlRequest::SupervisorSetEnabled { module_id, enabled } => {
                self.handle_supervisor_set_enabled(frame, module_id, enabled)
                    .await
            }
            ClientControlRequest::SupervisorHealthProbe { module_id } => {
                self.handle_supervisor_health_probe(frame, module_id).await
            }
            ClientControlRequest::SupervisorHealth {} => self.handle_supervisor_health(frame),
        }
    }

    fn handle_module_control_request(
        &self,
        connection_id: ConnectionId,
        frame: Frame,
        request: ModuleControlRequestFromModule,
    ) -> Result<Vec<Frame>, RouterError> {
        match request {
            ModuleControlRequestFromModule::CatalogUpdate { provides } => {
                self.handle_catalog_update(connection_id, frame, provides)
            }
        }
    }

    fn handle_catalog_update(
        &self,
        connection_id: ConnectionId,
        frame: Frame,
        provides: Vec<ProviderRole>,
    ) -> Result<Vec<Frame>, RouterError> {
        let Some(registration) = self
            .registry
            .get_module_by_connection(connection_id)
            .map_err(|err| RouterError::backend(0, frame.header.corr, err.to_string()))?
        else {
            return Ok(vec![control_error_frame(
                &frame,
                "not_registered",
                "catalog.update requires an active module registration owned by this connection",
            )?]);
        };

        if let Some(message) =
            catalog_update_frozen_field_message(&registration.manifest, &provides)
        {
            return Ok(vec![control_error_frame(
                &frame,
                "catalog_update_frozen_field",
                message,
            )?]);
        }

        let updated = self
            .registry
            .replace_provides_for_connection(connection_id, provides)
            .map_err(|err| RouterError::backend(0, frame.header.corr, err.to_string()))?;
        if updated.is_none() {
            return Ok(vec![control_error_frame(
                &frame,
                "not_registered",
                "catalog.update requires an active module registration owned by this connection",
            )?]);
        }

        let response = ModuleControlResponseToModule::CatalogUpdate {};
        control_response_body_frame(
            &frame,
            &response,
            "ModuleControlResponseToModule::CatalogUpdate",
        )
        .map(|frame| vec![frame])
    }

    fn handle_server_describe(&self, frame: Frame) -> Result<Vec<Frame>, RouterError> {
        let response = ClientControlResponse::ServerDescribe {
            protocol_ver: PROTOCOL_VERSION,
            subc_ops: subc_ops(),
            capabilities: self.subc_capabilities.as_ref().to_vec(),
            connected_clients: self.connected_clients.count(),
        };
        Ok(vec![control_response_body_frame(
            &frame,
            &response,
            "ClientControlResponse::ServerDescribe",
        )?])
    }

    fn handle_catalog_list(
        &self,
        frame: Frame,
        module_id: Option<String>,
    ) -> Result<Vec<Frame>, RouterError> {
        let (generation, modules) = self.registry.list_modules().map_err(|err| {
            RouterError::backend(0, frame.header.corr, format!("registry error: {err}"))
        })?;
        let entries = modules
            .into_iter()
            .filter(|registration| {
                module_id
                    .as_deref()
                    .map(|wanted| registration.manifest.module_id == wanted)
                    .unwrap_or(true)
            })
            .map(|registration| {
                let roles = registration.manifest.provides;
                CatalogEntry {
                    module_id: registration.manifest.module_id,
                    roles,
                    control_ops: registration.control_ops,
                }
            })
            .collect();
        let response = ClientControlResponse::CatalogList {
            generation,
            modules: entries,
            subc_ops: subc_ops(),
        };
        Ok(vec![control_response_body_frame(
            &frame,
            &response,
            "ClientControlResponse::CatalogList",
        )?])
    }

    fn route_open_principal(
        &self,
        frame: &Frame,
        consumer_identity: Option<ConsumerIdentity>,
    ) -> Result<Result<Principal, Frame>, RouterError> {
        let Some(consumer_identity) = consumer_identity else {
            return Ok(Ok(Principal::Direct));
        };

        if self.supervisor.spawned_consumer_authorized(
            &consumer_identity.module_id,
            &consumer_identity.launch_nonce,
        ) {
            return Ok(Ok(Principal::Reserved {
                module_id: consumer_identity.module_id,
            }));
        }

        Ok(Err(control_error_frame(
            frame,
            "bad_consumer_identity",
            format!(
                "consumer_identity for module_id '{}' did not match a supervised launch nonce",
                consumer_identity.module_id
            ),
        )?))
    }

    async fn handle_route_open(
        &self,
        ctx: &RouteCtx,
        frame: Frame,
        target: RouteTarget,
        mut identity: BindIdentity,
        consumer_identity: Option<ConsumerIdentity>,
        consumer_capabilities: Option<Vec<String>>,
    ) -> Result<Vec<Frame>, RouterError> {
        let target_module_id = target_module_id(&target).to_string();
        debug!(
            connection_id = ctx.connection_id.get(),
            corr = frame.header.corr,
            module_id = %target_module_id,
            "handling route.open"
        );

        let Some(registration) = self
            .registry
            .get_module(&target_module_id)
            .map_err(|err| RouterError::backend(0, frame.header.corr, err.to_string()))?
        else {
            if let Some(status) = self.supervisor_status(&target_module_id, frame.header.corr)? {
                return Ok(vec![control_error_frame(
                    &frame,
                    "target_unavailable",
                    format!(
                        "module_id '{target_module_id}' is supervised but not available (state={}, enabled={}, live={})",
                        status.state, status.enabled, status.live
                    ),
                )?]);
            }
            return Ok(vec![control_error_frame(
                &frame,
                "unknown_module",
                format!("module_id '{target_module_id}' is not registered"),
            )?]);
        };

        if !target_has_required_role(&target, &registration.manifest.provides) {
            return Ok(vec![control_error_frame(
                &frame,
                "target_unavailable",
                format!("module_id '{target_module_id}' does not provide the requested target"),
            )?]);
        }

        if registration.state != ChannelState::Active {
            return Ok(vec![control_error_frame(
                &frame,
                "target_unavailable",
                format!("module_id '{target_module_id}' is not active"),
            )?]);
        }

        if self
            .forwarding
            .module_is_draining(&target_module_id)
            .map_err(RouterError::Forwarding)?
        {
            return Ok(vec![control_error_frame(
                &frame,
                "module_reloading",
                format!("module_id '{target_module_id}' is reloading"),
            )?]);
        }

        if self
            .process_liveness
            .as_ref()
            .and_then(|process_liveness| process_liveness.process_live(&target_module_id))
            == Some(false)
        {
            return Ok(vec![control_error_frame(
                &frame,
                "target_unavailable",
                format!("module_id '{target_module_id}' is not live"),
            )?]);
        }

        if !self
            .forwarding
            .has_live_module_connection(&target_module_id)
            .map_err(RouterError::Forwarding)?
        {
            return Ok(vec![control_error_frame(
                &frame,
                "target_unavailable",
                format!("module_id '{target_module_id}' has no live forwarding connection"),
            )?]);
        }

        if let Some(error) =
            self.guard_module_control_op(&frame, &target_module_id, "route.bind")?
        {
            return Ok(vec![error]);
        }

        let principal = match self.route_open_principal(&frame, consumer_identity)? {
            Ok(principal) => principal,
            Err(error) => return Ok(vec![error]),
        };

        let project_root = match ProjectRootId::from_path(&identity.project_root) {
            Ok(project_root) => project_root,
            Err(err) => {
                return Ok(vec![control_error_frame(
                    &frame,
                    "invalid_project_root",
                    err.to_string(),
                )?])
            }
        };
        identity.project_root = project_root.as_path().to_path_buf();

        let pending = match self
            .forwarding
            .begin_route_bind_relay_for(ctx.connection_id, &target_module_id)
        {
            Ok(pending) => pending,
            Err(err) => {
                return Ok(vec![control_error_frame(
                    &frame,
                    forwarding_error_code(&err),
                    err.to_string(),
                )?])
            }
        };
        let crate::forwarding::PendingRouteBindRelay {
            endpoint,
            module_sink,
            negotiated_ver,
            client_connection_id,
            client_channel,
            module_channel,
            corr: relay_corr,
            receiver,
        } = pending;
        let mut reservation = RouteBindReservationGuard::new(
            Arc::clone(&self.forwarding),
            endpoint,
            client_connection_id,
            client_channel,
            module_channel,
            relay_corr,
        );

        let relay = ModuleControlRequest::RouteBind {
            route_channel: module_channel,
            target,
            identity,
            principal: Some(principal),
            consumer_capabilities,
        };
        let relay_body = serde_json::to_vec(&relay).map_err(|err| {
            RouterError::backend(
                0,
                frame.header.corr,
                format!("failed to encode route.bind request: {err}"),
            )
        })?;
        let relay_frame = Frame::build_with_version(
            negotiated_ver,
            FrameType::Request,
            control_flags(),
            0,
            relay_corr,
            relay_body,
        )
        .map_err(RouterError::FrameBuild)?;

        if let Err(err) = module_sink.send(relay_frame).await {
            reservation.release_and_disarm();
            return Ok(vec![control_error_frame(
                &frame,
                "target_unavailable",
                err.to_string(),
            )?]);
        }

        match timeout(self.route_bind_relay_timeout, receiver).await {
            Ok(Ok(RouteBindRelayOutcome::Accepted)) => {
                if let Err(err) = self.forwarding.commit_route(
                    ctx.connection_id,
                    ctx.egress.clone(),
                    response_version(&frame),
                    endpoint,
                    client_channel,
                    module_channel,
                ) {
                    // The module already accepted (it holds a binding), but subc
                    // could not commit locally. Tell the module to drop it.
                    self.send_abandoned_route_bind_goodbye(
                        &module_sink,
                        negotiated_ver,
                        module_channel,
                    );
                    reservation.release_and_disarm();
                    return Ok(vec![control_error_frame(
                        &frame,
                        forwarding_error_code(&err),
                        err.to_string(),
                    )?]);
                }
                reservation.disarm();
                let response = ClientControlResponse::RouteOpen {
                    route_channel: client_channel,
                };
                Ok(vec![control_response_body_frame(
                    &frame,
                    &response,
                    "ClientControlResponse::RouteOpen",
                )?])
            }
            Ok(Ok(RouteBindRelayOutcome::Rejected(body))) => {
                reservation.release_and_disarm();
                Ok(vec![control_error_body_frame(&frame, body)?])
            }
            Ok(Ok(RouteBindRelayOutcome::ModuleGone(message))) => {
                reservation.release_and_disarm();
                Ok(vec![control_error_frame(
                    &frame,
                    "target_unavailable",
                    message,
                )?])
            }
            Ok(Err(_)) => {
                // The relay was enqueued but its waiter was cancelled before the
                // module answered; the module may still accept late, so tell it to
                // drop any binding for this channel.
                self.send_abandoned_route_bind_goodbye(
                    &module_sink,
                    negotiated_ver,
                    module_channel,
                );
                reservation.release_and_disarm();
                Ok(vec![control_error_frame(
                    &frame,
                    "target_unavailable",
                    "route.bind relay waiter was canceled before the module responded",
                )?])
            }
            Err(_) => {
                // Relay was enqueued but the module did not answer in time; it may
                // still accept late, so tell it to drop any binding for this channel.
                self.send_abandoned_route_bind_goodbye(
                    &module_sink,
                    negotiated_ver,
                    module_channel,
                );
                reservation.release_and_disarm();
                Ok(vec![control_error_frame(
                    &frame,
                    "module_timeout",
                    format!(
                        "module_id '{target_module_id}' did not answer route.bind within {:?}",
                        self.route_bind_relay_timeout
                    ),
                )?])
            }
        }
    }

    fn handle_supervisor_list(&self, frame: Frame) -> Result<Vec<Frame>, RouterError> {
        let generation = self
            .registry
            .generation()
            .map_err(|err| RouterError::backend(0, frame.header.corr, err.to_string()))?;
        let modules = self
            .supervisor
            .list()
            .into_iter()
            .map(|module| {
                let status = module.status().map_err(|err| {
                    RouterError::backend(
                        0,
                        frame.header.corr,
                        format!("failed to read supervisor status: {err}"),
                    )
                })?;
                Ok(SupervisorEntry {
                    module_id: status.module_id,
                    state: status.state.to_string(),
                    enabled: status.enabled,
                    live: status.live,
                    health: status.health.status,
                    last_probe_ms: status.health.last_probe_ms,
                })
            })
            .collect::<Result<Vec<_>, RouterError>>()?;
        let response = ClientControlResponse::SupervisorList {
            generation,
            modules,
        };
        Ok(vec![control_response_body_frame(
            &frame,
            &response,
            "ClientControlResponse::SupervisorList",
        )?])
    }

    fn handle_supervisor_health(&self, frame: Frame) -> Result<Vec<Frame>, RouterError> {
        let generation = self
            .registry
            .generation()
            .map_err(|err| RouterError::backend(0, frame.header.corr, err.to_string()))?;
        let modules = self
            .supervisor
            .list()
            .into_iter()
            .map(|module| {
                let status = module.status().map_err(|err| {
                    RouterError::backend(
                        0,
                        frame.header.corr,
                        format!("failed to read supervisor health: {err}"),
                    )
                })?;
                Ok(SupervisorHealthEntry {
                    module_id: status.module_id,
                    status: status.health.status,
                    detail: status.health.detail,
                    metrics: status.health.metrics,
                    consecutive_failures: status.health.consecutive_failures,
                    last_action: status.health.last_action,
                    last_action_ms: status.health.last_action_ms,
                })
            })
            .collect::<Result<Vec<_>, RouterError>>()?;
        let response = ClientControlResponse::SupervisorHealth {
            generation,
            modules,
        };
        Ok(vec![control_response_body_frame(
            &frame,
            &response,
            "ClientControlResponse::SupervisorHealth",
        )?])
    }

    async fn handle_supervisor_restart(
        &self,
        frame: Frame,
        module_id: String,
    ) -> Result<Vec<Frame>, RouterError> {
        let Some(module) = self.supervisor.get(&module_id) else {
            return Ok(vec![control_error_frame(
                &frame,
                "unknown_module",
                format!("module_id '{module_id}' is not supervised"),
            )?]);
        };

        if let Err(err) = module.restart().await {
            let (code, message) = match err {
                crate::supervise::SuperviseError::Disabled { .. } => {
                    ("module_disabled", err.to_string())
                }
                _ => (
                    "target_unavailable",
                    format!("failed to restart module_id '{module_id}': {err}"),
                ),
            };
            return Ok(vec![control_error_frame(&frame, code, message)?]);
        }

        let response = ClientControlResponse::SupervisorAck {
            module_id,
            applied: true,
        };
        Ok(vec![control_response_body_frame(
            &frame,
            &response,
            "ClientControlResponse::SupervisorAck",
        )?])
    }

    async fn handle_supervisor_reload(
        &self,
        frame: Frame,
        module_id: String,
    ) -> Result<Vec<Frame>, RouterError> {
        let Some(module) = self.supervisor.get(&module_id) else {
            return Ok(vec![control_error_frame(
                &frame,
                "unknown_module",
                format!("module_id '{module_id}' is not supervised"),
            )?]);
        };

        if let Err(err) = module.reload().await {
            let (code, message) = match err {
                crate::supervise::SuperviseError::Disabled { .. } => {
                    ("module_disabled", err.to_string())
                }
                _ => (
                    "reload_failed",
                    format!("failed to reload module_id '{module_id}': {err}"),
                ),
            };
            return Ok(vec![control_error_frame(&frame, code, message)?]);
        }

        let response = ClientControlResponse::SupervisorAck {
            module_id,
            applied: true,
        };
        Ok(vec![control_response_body_frame(
            &frame,
            &response,
            "ClientControlResponse::SupervisorAck",
        )?])
    }

    async fn handle_supervisor_set_enabled(
        &self,
        frame: Frame,
        module_id: String,
        enabled: bool,
    ) -> Result<Vec<Frame>, RouterError> {
        let Some(module) = self.supervisor.get(&module_id) else {
            return Ok(vec![control_error_frame(
                &frame,
                "unknown_module",
                format!("module_id '{module_id}' is not supervised"),
            )?]);
        };

        let applied = match module.set_enabled(enabled).await {
            Ok(applied) => applied,
            Err(err) => {
                return Ok(vec![control_error_frame(
                    &frame,
                    "target_unavailable",
                    format!("failed to set module_id '{module_id}' enabled={enabled}: {err}"),
                )?])
            }
        };

        let response = ClientControlResponse::SupervisorAck { module_id, applied };
        Ok(vec![control_response_body_frame(
            &frame,
            &response,
            "ClientControlResponse::SupervisorAck",
        )?])
    }

    async fn handle_supervisor_health_probe(
        &self,
        frame: Frame,
        module_id: String,
    ) -> Result<Vec<Frame>, RouterError> {
        let Some(registration) = self
            .registry
            .get_module(&module_id)
            .map_err(|err| RouterError::backend(0, frame.header.corr, err.to_string()))?
        else {
            return Ok(vec![control_error_frame(
                &frame,
                "unknown_module",
                format!("module_id '{module_id}' is not registered"),
            )?]);
        };

        if !module_registration_grants_op(&registration.control_ops, MODULE_CONTROL_OP_HEALTH_CHECK)
        {
            return Ok(vec![control_error_frame(
                &frame,
                "health_not_advertised",
                format!("module_id '{module_id}' did not advertise health.check"),
            )?]);
        }

        let deadline = Instant::now() + self.health_probe_timeout;
        let pending = match self.forwarding.begin_module_control_rpc_for(
            &module_id,
            MODULE_CONTROL_OP_HEALTH_CHECK,
            deadline,
        ) {
            Ok(pending) => pending,
            Err(err) => {
                return Ok(vec![control_error_frame(
                    &frame,
                    forwarding_error_code(&err),
                    err.to_string(),
                )?])
            }
        };

        let PendingModuleControlRpc {
            endpoint,
            module_sink,
            negotiated_ver,
            corr: probe_corr,
            receiver,
        } = pending;
        let mut guard =
            ModuleControlRpcGuard::new(Arc::clone(&self.forwarding), endpoint, probe_corr);
        let probe_body =
            serde_json::to_vec(&ModuleControlRequest::HealthCheck {}).map_err(|err| {
                RouterError::backend(
                    0,
                    frame.header.corr,
                    format!("failed to encode health.check request: {err}"),
                )
            })?;
        let probe_frame = Frame::build_with_version(
            negotiated_ver,
            FrameType::Request,
            control_flags(),
            0,
            probe_corr,
            probe_body,
        )
        .map_err(RouterError::FrameBuild)?;

        if let Err(err) = module_sink.send(probe_frame).await {
            return Ok(vec![control_error_frame(
                &frame,
                "target_unavailable",
                err.to_string(),
            )?]);
        }

        match timeout_at(deadline, receiver).await {
            Ok(Ok(ModuleControlRpcOutcome::Response(response))) => {
                guard.disarm();
                let Some(report) = response.health_report() else {
                    return Ok(vec![control_error_frame(
                        &frame,
                        "invalid_control_body",
                        "health.check RPC returned a non-health response",
                    )?]);
                };
                let HealthReport {
                    status,
                    detail,
                    metrics,
                } = report;
                let response = ClientControlResponse::SupervisorHealthProbe {
                    module_id,
                    status,
                    detail,
                    metrics,
                };
                Ok(vec![control_response_body_frame(
                    &frame,
                    &response,
                    "ClientControlResponse::SupervisorHealthProbe",
                )?])
            }
            Ok(Ok(ModuleControlRpcOutcome::Rejected(body))) => {
                guard.disarm();
                Ok(vec![control_error_body_frame(&frame, body)?])
            }
            Ok(Ok(ModuleControlRpcOutcome::ModuleGone(message))) => {
                guard.disarm();
                Ok(vec![control_error_frame(
                    &frame,
                    "target_unavailable",
                    message,
                )?])
            }
            Ok(Ok(ModuleControlRpcOutcome::MalformedResponse(message))) => {
                guard.disarm();
                Ok(vec![control_error_frame(
                    &frame,
                    "invalid_control_body",
                    message,
                )?])
            }
            Ok(Ok(ModuleControlRpcOutcome::UnexpectedOp { expected, actual })) => {
                guard.disarm();
                Ok(vec![control_error_frame(
                    &frame,
                    "invalid_control_body",
                    format!("expected module-control op '{expected}', got '{actual}'"),
                )?])
            }
            Ok(Err(_)) => Ok(vec![control_error_frame(
                &frame,
                "target_unavailable",
                "health.check waiter was canceled before the module responded",
            )?]),
            Err(_) => Ok(vec![control_error_frame(
                &frame,
                "module_timeout",
                format!(
                    "module_id '{module_id}' did not answer health.check within {:?}",
                    self.health_probe_timeout
                ),
            )?]),
        }
    }

    fn supervisor_status(
        &self,
        module_id: &str,
        corr: u64,
    ) -> Result<Option<crate::supervise::ModuleStatus>, RouterError> {
        self.supervisor
            .get(module_id)
            .map(|module| {
                module.status().map_err(|err| {
                    RouterError::backend(
                        0,
                        corr,
                        format!(
                            "failed to read supervisor status for module_id '{module_id}': {err}"
                        ),
                    )
                })
            })
            .transpose()
    }

    fn guard_module_control_op(
        &self,
        frame: &Frame,
        module_id: &str,
        op: &str,
    ) -> Result<Option<Frame>, RouterError> {
        if self.module_grants_op(module_id, op, frame.header.corr)? {
            return Ok(None);
        }

        Ok(Some(control_error_frame(
            frame,
            "op_not_allowed",
            format!("module_id '{module_id}' did not grant control op '{op}'"),
        )?))
    }

    fn module_grants_op(&self, module_id: &str, op: &str, corr: u64) -> Result<bool, RouterError> {
        let Some(registration) = self
            .registry
            .get_module(module_id)
            .map_err(|err| RouterError::backend(0, corr, err.to_string()))?
        else {
            return Ok(false);
        };
        Ok(module_registration_grants_op(&registration.control_ops, op))
    }

    fn handle_status_update(
        &self,
        endpoint: ModuleEndpointId,
        frame: Frame,
    ) -> Result<Vec<Frame>, RouterError> {
        let update = match serde_json::from_slice::<ModuleControlPush>(&frame.body) {
            Ok(update) => update,
            Err(err) => {
                // Forward-compat: a newer module may push a channel-0 op this subc
                // version doesn't know. The control contract says unknown push ops
                // are IGNORED, never answered with an error. Only a malformed body
                // for an op we DO know is a real error worth surfacing.
                if is_known_module_push_op(&frame.body) {
                    return Ok(vec![control_error_frame(
                        &frame,
                        "invalid_control_body",
                        format!("malformed module control push body: {err}"),
                    )?]);
                }
                return Ok(Vec::new());
            }
        };

        match update {
            ModuleControlPush::RouteStatus {
                route_channel,
                status,
            } => {
                self.forwarding
                    .cache_status(endpoint, route_channel, status)
                    .map_err(RouterError::Forwarding)?;
            }
        }
        Ok(Vec::new())
    }

    fn handle_route_poll(
        &self,
        ctx: &RouteCtx,
        frame: Frame,
        route_channel: u16,
        kind: PollKind,
    ) -> Result<Vec<Frame>, RouterError> {
        let response = match kind {
            PollKind::Liveness => {
                let route_bound_to_live_module = self
                    .forwarding
                    .client_route_is_bound_to_live_module(ctx.connection_id, route_channel)
                    .map_err(RouterError::Forwarding)?;
                let process_running = if route_bound_to_live_module {
                    self.process_running_for_route(ctx.connection_id, route_channel)
                        .map_err(|message| {
                            RouterError::backend(frame.header.channel, frame.header.corr, message)
                        })?
                } else {
                    false
                };
                let live = route_bound_to_live_module && process_running;
                ClientControlResponse::RoutePoll {
                    status: None,
                    live: Some(live),
                }
            }
            PollKind::Status => {
                let status = self
                    .forwarding
                    .get_status(ctx.connection_id, route_channel)
                    .map_err(RouterError::Forwarding)?;

                ClientControlResponse::RoutePoll { status, live: None }
            }
        };

        Ok(vec![control_response_body_frame(
            &frame,
            &response,
            "ClientControlResponse::RoutePoll",
        )?])
    }

    fn process_running_for_route(
        &self,
        connection_id: ConnectionId,
        route_channel: u16,
    ) -> Result<bool, String> {
        let Some(process_liveness) = &self.process_liveness else {
            return Ok(true);
        };
        let Some(module_id) = self.module_id_for_route(connection_id, route_channel)? else {
            return Ok(true);
        };
        Ok(process_liveness.process_live(&module_id).unwrap_or(true))
    }

    fn module_id_for_route(
        &self,
        connection_id: ConnectionId,
        route_channel: u16,
    ) -> Result<Option<String>, String> {
        self.forwarding
            .client_route_module_id(connection_id, route_channel)
            .map_err(|err| {
                format!(
                    "forwarding error resolving module for route channel {route_channel}: {err}"
                )
            })
    }

    fn handle_module_relay_response(
        &self,
        connection_id: ConnectionId,
        frame: Frame,
    ) -> Result<Vec<Frame>, RouterError> {
        let mut secondary_error = None;
        let outcome = match frame.header.ty {
            FrameType::Response => match serde_json::from_slice::<ControlOpProbe>(&frame.body) {
                Ok(probe) if probe.op == "route.bind" => {
                    match serde_json::from_slice::<ModuleControlResponse>(&frame.body) {
                        Ok(ModuleControlResponse::RouteBindAck {}) => {
                            RouteBindRelayOutcome::Accepted
                        }
                        Ok(other) => {
                            let message =
                                format!("route.bind response carried unexpected body: {other:?}");
                            secondary_error = Some(control_error_frame(
                                &frame,
                                "invalid_control_body",
                                message.clone(),
                            )?);
                            RouteBindRelayOutcome::ModuleGone(message)
                        }
                        Err(err) => {
                            let message = format!("malformed route.bind response body: {err}");
                            secondary_error = Some(control_error_frame(
                                &frame,
                                "invalid_control_body",
                                message.clone(),
                            )?);
                            RouteBindRelayOutcome::ModuleGone(message)
                        }
                    }
                }
                Ok(probe) => {
                    let outcome = match serde_json::from_slice::<ModuleControlResponse>(&frame.body)
                    {
                        Ok(response) => ModuleControlRpcOutcome::Response(response),
                        Err(err) => ModuleControlRpcOutcome::MalformedResponse(format!(
                            "malformed {} response body: {err}",
                            probe.op
                        )),
                    };
                    let settled = self
                        .forwarding
                        .complete_module_control_rpc(
                            connection_id,
                            frame.header.corr,
                            Some(&probe.op),
                            outcome,
                        )
                        .map_err(RouterError::Forwarding)?;
                    if !settled {
                        debug!(
                            connection_id = connection_id.get(),
                            corr = frame.header.corr,
                            op = %probe.op,
                            "dropping late or unknown module-control RPC response"
                        );
                    }
                    return Ok(Vec::new());
                }
                Err(err) => {
                    if let Some(expected_op) = self
                        .forwarding
                        .pending_module_control_op(connection_id, frame.header.corr)
                        .map_err(RouterError::Forwarding)?
                    {
                        let settled = self
                            .forwarding
                            .complete_module_control_rpc(
                                connection_id,
                                frame.header.corr,
                                None,
                                ModuleControlRpcOutcome::MalformedResponse(format!(
                                    "malformed {expected_op} response body: {err}"
                                )),
                            )
                            .map_err(RouterError::Forwarding)?;
                        if !settled {
                            debug!(
                                connection_id = connection_id.get(),
                                corr = frame.header.corr,
                                "dropping late malformed module-control RPC response"
                            );
                        }
                        return Ok(Vec::new());
                    }
                    let message = format!("malformed route.bind response body: {err}");
                    secondary_error = Some(control_error_frame(
                        &frame,
                        "invalid_control_body",
                        message.clone(),
                    )?);
                    RouteBindRelayOutcome::ModuleGone(message)
                }
            },
            FrameType::Error => {
                if self
                    .forwarding
                    .pending_module_control_op(connection_id, frame.header.corr)
                    .map_err(RouterError::Forwarding)?
                    .is_some()
                {
                    let outcome = match serde_json::from_slice::<ErrorBody>(&frame.body) {
                        Ok(body) => ModuleControlRpcOutcome::Rejected(body),
                        Err(err) => ModuleControlRpcOutcome::MalformedResponse(format!(
                            "malformed module-control ERROR body: {err}"
                        )),
                    };
                    let settled = self
                        .forwarding
                        .complete_module_control_rpc(
                            connection_id,
                            frame.header.corr,
                            None,
                            outcome,
                        )
                        .map_err(RouterError::Forwarding)?;
                    if !settled {
                        debug!(
                            connection_id = connection_id.get(),
                            corr = frame.header.corr,
                            "dropping late or unknown module-control RPC error"
                        );
                    }
                    return Ok(Vec::new());
                }
                match serde_json::from_slice::<ErrorBody>(&frame.body) {
                    Ok(body) => RouteBindRelayOutcome::Rejected(body),
                    Err(err) => {
                        let message = format!("malformed route.bind ERROR body: {err}");
                        secondary_error = Some(control_error_frame(
                            &frame,
                            "invalid_control_body",
                            message.clone(),
                        )?);
                        RouteBindRelayOutcome::ModuleGone(message)
                    }
                }
            }
            ty => {
                return Ok(vec![control_error_frame(
                    &frame,
                    "unsupported_control_frame",
                    format!("unsupported module channel-0 frame {ty:?}"),
                )?])
            }
        };

        let settled = self
            .forwarding
            .complete_pending_relay(connection_id, frame.header.corr, outcome)
            .map_err(RouterError::Forwarding)?;
        if !settled {
            debug!(
                connection_id = connection_id.get(),
                corr = frame.header.corr,
                frame_type = ?frame.header.ty,
                "dropping late or unknown route.bind relay response"
            );
        }
        Ok(secondary_error.into_iter().collect())
    }

    fn handle_goodbye(&self, connection_id: ConnectionId) -> Result<Vec<Frame>, RouterError> {
        debug!(connection_id = connection_id.get(), "handling GOODBYE");
        let registrations = self
            .deregister_connection(connection_id)
            .map_err(|err| RouterError::backend(0, 0, err.to_string()))?;
        let released_routes = self
            .forwarding
            .cleanup_connection(connection_id)
            .map_err(RouterError::Forwarding)?;
        self.emit_route_goodbyes(released_routes);
        // Notify only after forwarding teardown completes (see cleanup_connection).
        if !registrations.is_empty() {
            crate::supervise::notify_registration_release();
        }
        Ok(Vec::new())
    }
}

impl Default for ControlHandler {
    fn default() -> Self {
        Self::new(Arc::new(Registry::default()))
    }
}

fn subc_ops() -> Vec<String> {
    SUBC_CONTROL_OPS
        .iter()
        .map(|op| (*op).to_string())
        .collect()
}

fn module_subc_ops() -> Vec<String> {
    SUBC_CONTROL_OPS
        .iter()
        .chain(MODULE_TO_SUBC_CONTROL_OPS.iter())
        .map(|op| (*op).to_string())
        .collect()
}

#[cfg(test)]
fn module_baseline_control_ops() -> Vec<String> {
    MODULE_BASELINE_CONTROL_OPS
        .iter()
        .map(|op| (*op).to_string())
        .collect()
}

fn effective_module_control_ops(declared: Option<Vec<String>>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut effective = Vec::new();
    for op in MODULE_BASELINE_CONTROL_OPS {
        if seen.insert((*op).to_string()) {
            effective.push((*op).to_string());
        }
    }
    for op in declared.unwrap_or_default() {
        if seen.insert(op.clone()) {
            effective.push(op);
        }
    }
    effective
}

fn module_registration_grants_op(control_ops: &[String], op: &str) -> bool {
    MODULE_BASELINE_CONTROL_OPS.contains(&op) || control_ops.iter().any(|granted| granted == op)
}

fn target_module_id(target: &RouteTarget) -> &str {
    match target {
        RouteTarget::ToolProvider { module_id }
        | RouteTarget::ManagementSurface { module_id }
        | RouteTarget::InternalService { module_id, .. } => module_id,
    }
}

fn target_has_required_role(target: &RouteTarget, roles: &[ProviderRole]) -> bool {
    roles.iter().any(|role| match (target, role) {
        (RouteTarget::ToolProvider { .. }, ProviderRole::ToolProvider { .. }) => true,
        (RouteTarget::ManagementSurface { .. }, ProviderRole::ManagementSurface { .. }) => true,
        (
            RouteTarget::InternalService { service_id, .. },
            ProviderRole::InternalService {
                service_id: provided,
                ..
            },
        ) => service_id == provided,
        _ => false,
    })
}

fn is_routable_role(role: &ProviderRole) -> bool {
    matches!(
        role,
        ProviderRole::ToolProvider { .. }
            | ProviderRole::ManagementSurface { .. }
            | ProviderRole::InternalService { .. }
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ControlRequestBodyError {
    UnknownOp,
    InvalidBody,
}

#[derive(Debug, Deserialize)]
struct ControlOpProbe {
    op: String,
}

/// Channel-0 push ops this subc version understands. A push whose `op` is not in
/// this set is treated as a forward-compat unknown and ignored rather than errored.
const MODULE_PUSH_OPS: &[&str] = &["route.status"];

fn is_known_module_push_op(body: &[u8]) -> bool {
    serde_json::from_slice::<ControlOpProbe>(body)
        .map(|probe| MODULE_PUSH_OPS.contains(&probe.op.as_str()))
        .unwrap_or(false)
}

fn is_known_module_request_op(body: &[u8]) -> bool {
    serde_json::from_slice::<ControlOpProbe>(body)
        .map(|probe| MODULE_TO_SUBC_CONTROL_OPS.contains(&probe.op.as_str()))
        .unwrap_or(false)
}

fn parse_client_control_request(
    body: &[u8],
) -> Result<ClientControlRequest, (serde_json::Error, ControlRequestBodyError)> {
    serde_json::from_slice::<ClientControlRequest>(body).map_err(|err| {
        let classification = match serde_json::from_slice::<ControlOpProbe>(body) {
            Ok(probe) if SUBC_CONTROL_OPS.contains(&probe.op.as_str()) => {
                ControlRequestBodyError::InvalidBody
            }
            Ok(_) => ControlRequestBodyError::UnknownOp,
            Err(_) => ControlRequestBodyError::InvalidBody,
        };
        (err, classification)
    })
}

fn parse_module_control_request_from_module(
    body: &[u8],
) -> Result<ModuleControlRequestFromModule, (serde_json::Error, ControlRequestBodyError)> {
    serde_json::from_slice::<ModuleControlRequestFromModule>(body).map_err(|err| {
        let classification = match serde_json::from_slice::<ControlOpProbe>(body) {
            Ok(probe) if MODULE_TO_SUBC_CONTROL_OPS.contains(&probe.op.as_str()) => {
                ControlRequestBodyError::InvalidBody
            }
            Ok(_) => ControlRequestBodyError::UnknownOp,
            Err(_) => ControlRequestBodyError::InvalidBody,
        };
        (err, classification)
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ProviderRoleKind {
    ToolProvider,
    PipelineStage,
    ManagementSurface,
    InternalService,
}

fn provider_role_kind(role: &ProviderRole) -> ProviderRoleKind {
    match role {
        ProviderRole::ToolProvider { .. } => ProviderRoleKind::ToolProvider,
        ProviderRole::PipelineStage { .. } => ProviderRoleKind::PipelineStage,
        ProviderRole::ManagementSurface { .. } => ProviderRoleKind::ManagementSurface,
        ProviderRole::InternalService { .. } => ProviderRoleKind::InternalService,
    }
}

fn provider_role_kind_set(roles: &[ProviderRole]) -> BTreeSet<ProviderRoleKind> {
    roles.iter().map(provider_role_kind).collect()
}

fn catalog_update_frozen_field_message(
    registered: &ModuleManifest,
    provides: &[ProviderRole],
) -> Option<String> {
    let old_has_provides = !registered.provides.is_empty();
    let new_has_provides = !provides.is_empty();
    if old_has_provides != new_has_provides {
        return Some(format!(
            "catalog.update cannot change module '{}' between supervision-only and routable; routability is fixed at HELLO",
            registered.module_id
        ));
    }

    if provider_role_kind_set(&registered.provides) != provider_role_kind_set(provides) {
        return Some(format!(
            "catalog.update cannot change provider role kinds for module '{}'; role kinds are fixed at HELLO",
            registered.module_id
        ));
    }

    let registered_concurrency = manifest_concurrency(registered);
    let candidate = ModuleManifest {
        provides: provides.to_vec(),
        ..registered.clone()
    };
    let candidate_concurrency = manifest_concurrency(&candidate);
    if candidate_concurrency != registered_concurrency {
        return Some(format!(
            "catalog.update cannot change module '{}' concurrency from {:?} to {:?}; concurrency is fixed at HELLO",
            registered.module_id, registered_concurrency, candidate_concurrency
        ));
    }

    // control_ops live beside the manifest in the HELLO body, not inside
    // ModuleManifest, so a provides-only catalog.update cannot change them.
    None
}

fn manifest_provides_routable_role(manifest: &ModuleManifest) -> bool {
    manifest.provides.iter().any(is_routable_role)
}

/// Returns the routable-provider concurrency subc should enforce for this manifest.
///
/// Today only `ProviderRole::ToolProvider` carries an explicit manifest
/// concurrency. `ManagementSurface` and `InternalService` modules therefore
/// still fall back to `Concurrency::ModuleManaged`, which maps to subc's
/// current default 32-credit per-route window, and there is no manifest-level
/// override for those roles yet. A per-role concurrency field is deferred until
/// such a module exists.
fn manifest_concurrency(manifest: &ModuleManifest) -> Concurrency {
    manifest
        .provides
        .iter()
        .find_map(|provider| match provider {
            ProviderRole::ToolProvider { concurrency, .. } => Some(concurrency.clone()),
            ProviderRole::PipelineStage { .. }
            | ProviderRole::ManagementSurface { .. }
            | ProviderRole::InternalService { .. } => None,
        })
        .unwrap_or(Concurrency::ModuleManaged)
}

fn negotiate_version(peer_version: u8) -> Result<u8, String> {
    if peer_version < MIN_SUPPORTED_VERSION {
        return Err(format!(
            "protocol_ver {peer_version} is below minimum supported version {MIN_SUPPORTED_VERSION}"
        ));
    }
    Ok(peer_version.min(PROTOCOL_VERSION))
}

fn pong(frame: &Frame) -> Result<Frame, RouterError> {
    Frame::build_with_version(
        response_version(frame),
        FrameType::Pong,
        frame.header.flags,
        0,
        frame.header.corr,
        Vec::new(),
    )
    .map_err(RouterError::FrameBuild)
}

fn control_error_frame(
    frame: &Frame,
    code: &'static str,
    message: impl Into<String>,
) -> Result<Frame, RouterError> {
    control_error_body_frame(
        frame,
        ErrorBody {
            code: code.to_string(),
            message: message.into(),
        },
    )
}

fn control_error_body_frame(frame: &Frame, error: ErrorBody) -> Result<Frame, RouterError> {
    let body = serde_json::to_vec(&error).map_err(|err| {
        RouterError::backend(
            0,
            frame.header.corr,
            format!("failed to encode control ERROR: {err}"),
        )
    })?;

    Frame::build_with_version(
        response_version(frame),
        FrameType::Error,
        control_flags(),
        0,
        frame.header.corr,
        body,
    )
    .map_err(RouterError::FrameBuild)
}

fn control_response_body_frame<T: Serialize>(
    frame: &Frame,
    reply: &T,
    label: &'static str,
) -> Result<Frame, RouterError> {
    let body = serde_json::to_vec(reply).map_err(|err| {
        RouterError::backend(
            0,
            frame.header.corr,
            format!("failed to encode {label}: {err}"),
        )
    })?;

    Frame::build_with_version(
        response_version(frame),
        FrameType::Response,
        control_flags(),
        0,
        frame.header.corr,
        body,
    )
    .map_err(RouterError::FrameBuild)
}

fn forwarding_error_code(err: &ForwardingError) -> &'static str {
    match err {
        ForwardingError::NoModuleConnection => "target_unavailable",
        ForwardingError::ModuleReloading { .. } => "module_reloading",
        ForwardingError::ClientRouteChannelExhausted { .. }
        | ForwardingError::ModuleRouteChannelExhausted { .. } => "route_limit",
        ForwardingError::StaleModuleEndpoint | ForwardingError::UnknownReservation { .. } => {
            "target_unavailable"
        }
        ForwardingError::RelayCorrelationExhausted | ForwardingError::Poisoned => {
            "forwarding_error"
        }
    }
}

fn response_version(frame: &Frame) -> u8 {
    if (MIN_SUPPORTED_VERSION..=PROTOCOL_VERSION).contains(&frame.header.ver) {
        frame.header.ver
    } else {
        PROTOCOL_VERSION
    }
}

fn control_flags() -> Flags {
    Flags::new(false, Priority::Passive, false)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::{json, Value};
    use subc_protocol::{
        manifest::{
            Bindings, CircuitBreaker, Concurrency, ExecutionMode, IdentityBinding, IdentityScope,
            ModelPolicy, ProviderRole, ScheduledTask, StorageBinding, StorageKind, StorageScope,
            TaskEligibility, Tool,
        },
        session::HealthStatus,
        FrameType,
    };

    use super::*;
    use crate::{registry::ChannelState, router::FrameSink, RouteCtx, Router};
    use tokio::sync::mpsc;

    fn manifest(module_id: &str, protocol_ver: u8) -> ModuleManifest {
        ModuleManifest {
            module_id: module_id.to_string(),
            module_version: "0.1.0".to_string(),
            protocol_ver,
            trust_tier: subc_protocol::manifest::TrustTier::FirstParty,
            provides: vec![ProviderRole::ToolProvider {
                tools: vec![Tool {
                    name: "read".to_string(),
                    description: None,
                    execution_mode: ExecutionMode::Pure,
                    schema: json!({"type": "object"}),
                }],
                identity_scope: vec![IdentityScope::Project, IdentityScope::Session],
                concurrency: Concurrency::ModuleManaged,
                emits_push: true,
                sub_supervises: true,
            }],
            consumes: Vec::new(),
            scheduled_tasks: vec![ScheduledTask {
                task_id: "aft.dreamer".to_string(),
                eligibility: TaskEligibility {
                    cooldown: "1h".to_string(),
                    window: "always".to_string(),
                },
                lease_scope: subc_protocol::manifest::LeaseScope::Project,
                renews_during_calls: true,
                toolset: vec!["read".to_string()],
                model_policy: ModelPolicy {
                    tier: "cheap".to_string(),
                    fallback_chain: vec!["fallback".to_string()],
                },
                step_cap: 10,
                circuit_breaker: CircuitBreaker {
                    identical_failures: 3,
                },
            }],
            bindings: Bindings {
                storage: StorageBinding {
                    kind: StorageKind::Sqlite,
                    scope: StorageScope::Project,
                    owns_schema: true,
                },
                vault_grants: Vec::new(),
                identity: IdentityBinding {
                    requires: vec![IdentityScope::Project],
                    optional: vec![IdentityScope::Session],
                },
            },
        }
    }

    fn hello_frame(module_id: &str, protocol_ver: u8, corr: u64) -> Frame {
        hello_frame_with_control_ops(module_id, protocol_ver, corr, None)
    }

    fn hello_frame_with_control_ops(
        module_id: &str,
        protocol_ver: u8,
        corr: u64,
        control_ops: Option<Vec<String>>,
    ) -> Frame {
        hello_frame_full(module_id, protocol_ver, corr, control_ops, None)
    }

    fn hello_frame_with_nonce(
        module_id: &str,
        protocol_ver: u8,
        corr: u64,
        launch_nonce: Option<&str>,
    ) -> Frame {
        hello_frame_full(
            module_id,
            protocol_ver,
            corr,
            None,
            launch_nonce.map(ToOwned::to_owned),
        )
    }

    fn hello_frame_full(
        module_id: &str,
        protocol_ver: u8,
        corr: u64,
        control_ops: Option<Vec<String>>,
        launch_nonce: Option<String>,
    ) -> Frame {
        let body = serde_json::to_vec(&ModuleHelloBody {
            manifest: manifest(module_id, protocol_ver),
            protocol_ver,
            control_ops,
            launch_nonce,
        })
        .unwrap();
        Frame::build(FrameType::Hello, control_flags(), 0, corr, body).unwrap()
    }

    fn non_routable_hello_frame_with_control_ops(
        module_id: &str,
        corr: u64,
        control_ops: Option<Vec<String>>,
    ) -> Frame {
        let mut manifest = manifest(module_id, PROTOCOL_VERSION);
        manifest.provides.clear();
        let body = serde_json::to_vec(&ModuleHelloBody {
            manifest,
            protocol_ver: PROTOCOL_VERSION,
            control_ops,
            launch_nonce: None,
        })
        .unwrap();
        Frame::build(FrameType::Hello, control_flags(), 0, corr, body).unwrap()
    }

    fn channel_request(channel: u16, corr: u64) -> Frame {
        Frame::build(
            FrameType::Request,
            Flags::new(true, Priority::Interactive, false),
            channel,
            corr,
            b"opaque".to_vec(),
        )
        .unwrap()
    }

    fn route_ctx(connection_id: ConnectionId) -> (RouteCtx, mpsc::Receiver<Frame>) {
        let (tx, rx) = mpsc::channel(8);
        (
            RouteCtx {
                connection_id,
                egress: FrameSink::new(tx),
            },
            rx,
        )
    }

    fn parse_ack(frame: &Frame) -> ModuleHelloAckBody {
        serde_json::from_slice(&frame.body).unwrap()
    }

    fn parse_error(frame: &Frame) -> Value {
        serde_json::from_slice(&frame.body).unwrap()
    }

    fn parse_route_poll(frame: &Frame) -> ClientControlResponse {
        serde_json::from_slice(&frame.body).unwrap()
    }

    fn route_poll_frame(corr: u64, kind: PollKind, route_channel: u16) -> Frame {
        let body = serde_json::to_vec(&ClientControlRequest::RoutePoll {
            route_channel,
            kind,
        })
        .unwrap();
        Frame::build(FrameType::Request, control_flags(), 0, corr, body).unwrap()
    }

    fn supervisor_health_probe_frame(corr: u64, module_id: &str) -> Frame {
        let body = serde_json::to_vec(&ClientControlRequest::SupervisorHealthProbe {
            module_id: module_id.to_string(),
        })
        .unwrap();
        Frame::build(FrameType::Request, control_flags(), 0, corr, body).unwrap()
    }

    fn route_open_frame(corr: u64, module_id: &str, project_root: std::path::PathBuf) -> Frame {
        route_open_frame_with_consumer_capabilities(corr, module_id, project_root, None)
    }

    fn route_open_frame_with_consumer_capabilities(
        corr: u64,
        module_id: &str,
        project_root: std::path::PathBuf,
        consumer_capabilities: Option<Vec<String>>,
    ) -> Frame {
        let body = serde_json::to_vec(&ClientControlRequest::RouteOpen {
            target: RouteTarget::ToolProvider {
                module_id: module_id.to_string(),
            },
            identity: BindIdentity {
                project_root,
                harness: "unit".to_string(),
                session: "session".to_string(),
            },
            consumer_identity: None,
            consumer_capabilities,
        })
        .unwrap();
        Frame::build(FrameType::Request, control_flags(), 0, corr, body).unwrap()
    }

    fn health_response(corr: u64, status: HealthStatus) -> Frame {
        let body = serde_json::to_vec(&ModuleControlResponse::HealthCheck {
            status,
            detail: Some("warming".to_string()),
            metrics: Some(json!({"queue_depth": 3})),
        })
        .unwrap();
        Frame::build(FrameType::Response, control_flags(), 0, corr, body).unwrap()
    }

    fn route_bind_ack(corr: u64) -> Frame {
        let body = serde_json::to_vec(&ModuleControlResponse::RouteBindAck {}).unwrap();
        Frame::build(FrameType::Response, control_flags(), 0, corr, body).unwrap()
    }

    fn unique_project_root(label: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "subc-control-{label}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn assert_route_poll_liveness(frame: &Frame, expected_live: bool) {
        match parse_route_poll(frame) {
            ClientControlResponse::RoutePoll {
                status: None,
                live: Some(live),
            } => assert_eq!(live, expected_live),
            other => panic!("unexpected route.poll response: {other:?}"),
        }
    }

    fn bind_liveness_route(
        registry: &Registry,
        forwarding: &ForwardingTable,
        module_id: &str,
    ) -> (RouteCtx, u16) {
        let module_connection = ConnectionId::new(101);
        let client_connection = ConnectionId::new(202);
        let registration = registry
            .register_with_control_ops(
                manifest(module_id, PROTOCOL_VERSION),
                PROTOCOL_VERSION,
                module_connection,
                module_baseline_control_ops(),
            )
            .unwrap();
        let (module_tx, _module_rx) = mpsc::channel(8);
        let endpoint = forwarding
            .register_module_connection(
                module_connection,
                module_id.to_string(),
                PROTOCOL_VERSION,
                manifest_concurrency(&registration.manifest),
                FrameSink::new(module_tx),
            )
            .unwrap();
        let pending = forwarding
            .begin_route_bind_relay_for(client_connection, module_id)
            .unwrap();
        assert_eq!(pending.endpoint, endpoint);
        forwarding
            .complete_pending_relay(
                module_connection,
                pending.corr,
                RouteBindRelayOutcome::Accepted,
            )
            .unwrap();
        let (client_ctx, _client_rx) = route_ctx(client_connection);
        forwarding
            .commit_route(
                client_connection,
                client_ctx.egress.clone(),
                PROTOCOL_VERSION,
                endpoint,
                pending.client_channel,
                pending.module_channel,
            )
            .unwrap();
        (client_ctx, pending.client_channel)
    }

    struct FakeProcessLiveness {
        live: Option<bool>,
    }

    impl ModuleProcessLiveness for FakeProcessLiveness {
        fn process_live(&self, _module_id: &str) -> Option<bool> {
            self.live
        }
    }

    #[test]
    fn hello_registers_manifest_and_returns_ack() {
        let registry = Arc::new(Registry::default());
        let handler = ControlHandler::new(Arc::clone(&registry));
        let conn = ConnectionId::new(1);

        let responses = handler
            .handle_control(conn, hello_frame("aft", PROTOCOL_VERSION, 7))
            .unwrap();

        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0].header.ty, FrameType::HelloAck);
        assert_eq!(responses[0].header.channel, 0);
        assert_eq!(responses[0].header.corr, 7);
        let ack = parse_ack(&responses[0]);
        assert_eq!(ack.negotiated_ver, PROTOCOL_VERSION);
        assert!(ack
            .subc_capabilities
            .contains(&CAP_MANIFEST_REGISTRATION.to_string()));
        assert!(ack.subc_ops.contains(&ops::SUPERVISOR_LIST.to_string()));
        assert!(ack.subc_ops.contains(&ops::SUPERVISOR_RESTART.to_string()));
        assert!(ack
            .subc_ops
            .contains(&ops::SUPERVISOR_SET_ENABLED.to_string()));
        assert!(ack
            .subc_ops
            .contains(&MODULE_TO_SUBC_OP_CATALOG_UPDATE.to_string()));

        let registration = registry.get_module("aft").unwrap().unwrap();
        assert_eq!(registration.negotiated_ver, PROTOCOL_VERSION);
        assert_eq!(registration.state, ChannelState::Active);
        assert_eq!(registration.connection_id, conn);
        assert_eq!(registration.control_ops, module_baseline_control_ops());
    }

    #[test]
    fn hello_ack_omits_storage_when_no_storage_config() {
        let registry = Arc::new(Registry::default());
        let handler = ControlHandler::new(Arc::clone(&registry));
        let responses = handler
            .handle_control(
                ConnectionId::new(1),
                hello_frame("aft", PROTOCOL_VERSION, 7),
            )
            .unwrap();
        let ack = parse_ack(&responses[0]);
        assert_eq!(ack.storage, None, "no storage config -> no descriptor");
    }

    #[test]
    fn hello_ack_delivers_resolved_storage_descriptor_per_module() {
        // With a central sqlite storage policy, each registering module gets its
        // own resolved descriptor in HELLO_ACK, keyed by its module id.
        let registry = Arc::new(Registry::default());
        let handler = ControlHandler::new(Arc::clone(&registry)).with_storage_config(Some(
            crate::daemon_config::StorageConfig::Sqlite {
                data_home: std::path::PathBuf::from("/data"),
            },
        ));

        let responses = handler
            .handle_control(
                ConnectionId::new(1),
                hello_frame("alfonso-routing", PROTOCOL_VERSION, 7),
            )
            .unwrap();
        let ack = parse_ack(&responses[0]);
        assert_eq!(
            ack.storage,
            Some(serde_json::json!({
                "module_id": "alfonso-routing",
                "storage_namespace": "default",
                "isolation": { "kind": "module" },
                "backend": {
                    "backend": "sqlite",
                    "path": "/data/cortexkit/alfonso-routing/store.db"
                }
            })),
            "the delivered descriptor is the module's own sqlite store path"
        );
    }

    #[test]
    fn hello_control_ops_none_is_baseline_and_guard_rejects_synthetic_gated_op() {
        let registry = Arc::new(Registry::default());
        let handler = ControlHandler::new(Arc::clone(&registry));
        let conn = ConnectionId::new(1);
        let responses = handler
            .handle_control(
                conn,
                hello_frame_with_control_ops("aft", PROTOCOL_VERSION, 7, None),
            )
            .unwrap();
        assert_eq!(responses[0].header.ty, FrameType::HelloAck);
        let registration = registry.get_module("aft").unwrap().unwrap();
        assert_eq!(registration.control_ops, module_baseline_control_ops());

        let frame = Frame::build(FrameType::Request, control_flags(), 0, 77, Vec::new()).unwrap();
        assert!(handler
            .guard_module_control_op(&frame, "aft", "route.bind")
            .unwrap()
            .is_none());
        let error = handler
            .guard_module_control_op(&frame, "aft", "test.synthetic")
            .unwrap()
            .expect("synthetic ungranted op should be rejected");
        assert_eq!(error.header.ty, FrameType::Error);
        assert_eq!(parse_error(&error)["code"], "op_not_allowed");
    }

    #[test]
    fn hello_control_ops_some_adds_optional_grants() {
        let registry = Arc::new(Registry::default());
        let handler = ControlHandler::new(Arc::clone(&registry));
        handler
            .handle_control(
                ConnectionId::new(1),
                hello_frame_with_control_ops(
                    "aft",
                    PROTOCOL_VERSION,
                    7,
                    Some(vec![
                        "future.synthetic".to_string(),
                        "route.bind".to_string(),
                    ]),
                ),
            )
            .unwrap();
        let registration = registry.get_module("aft").unwrap().unwrap();
        assert_eq!(
            registration.control_ops,
            vec![
                "route.bind".to_string(),
                "route.status".to_string(),
                "future.synthetic".to_string(),
            ]
        );
        let frame = Frame::build(FrameType::Request, control_flags(), 0, 78, Vec::new()).unwrap();
        assert!(handler
            .guard_module_control_op(&frame, "aft", "future.synthetic")
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn health_probe_refuses_unadvertised_module_without_sending_frame() {
        let registry = Arc::new(Registry::default());
        let forwarding = Arc::new(ForwardingTable::default());
        let handler =
            ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding));
        let (module_ctx, mut module_rx) = route_ctx(ConnectionId::new(10));
        let responses = handler
            .handle_control_frame(
                &module_ctx,
                hello_frame_with_control_ops("aft", PROTOCOL_VERSION, 7, None),
            )
            .await
            .unwrap();
        assert_eq!(responses[0].header.ty, FrameType::HelloAck);

        let (client_ctx, _client_rx) = route_ctx(ConnectionId::new(20));
        let responses = handler
            .handle_control_frame(&client_ctx, supervisor_health_probe_frame(77, "aft"))
            .await
            .unwrap();
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0].header.ty, FrameType::Error);
        assert_eq!(parse_error(&responses[0])["code"], "health_not_advertised");
        assert!(module_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn health_probe_demuxes_while_route_bind_relay_is_in_flight() {
        let registry = Arc::new(Registry::default());
        let forwarding = Arc::new(ForwardingTable::default());
        let handler =
            ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding));
        let (module_ctx, mut module_rx) = route_ctx(ConnectionId::new(30));
        handler
            .handle_control_frame(
                &module_ctx,
                hello_frame_with_control_ops(
                    "aft",
                    PROTOCOL_VERSION,
                    7,
                    Some(vec![MODULE_CONTROL_OP_HEALTH_CHECK.to_string()]),
                ),
            )
            .await
            .unwrap();

        let project_root = unique_project_root("demux");
        let (route_client_ctx, _route_client_rx) = route_ctx(ConnectionId::new(31));
        let route_handler = handler.clone();
        let route_task = tokio::spawn(async move {
            route_handler
                .handle_control_frame(
                    &route_client_ctx,
                    route_open_frame(100, "aft", project_root),
                )
                .await
                .unwrap()
        });
        let bind_frame = tokio::time::timeout(Duration::from_secs(1), module_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            serde_json::from_slice::<ModuleControlRequest>(&bind_frame.body).unwrap(),
            ModuleControlRequest::RouteBind { .. }
        ));

        let (health_client_ctx, _health_client_rx) = route_ctx(ConnectionId::new(32));
        let health_handler = handler.clone();
        let health_task = tokio::spawn(async move {
            health_handler
                .handle_control_frame(
                    &health_client_ctx,
                    supervisor_health_probe_frame(101, "aft"),
                )
                .await
                .unwrap()
        });
        let health_frame = tokio::time::timeout(Duration::from_secs(1), module_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<ModuleControlRequest>(&health_frame.body).unwrap(),
            ModuleControlRequest::HealthCheck {}
        );

        handler
            .handle_control_frame(
                &module_ctx,
                health_response(health_frame.header.corr, HealthStatus::Degraded),
            )
            .await
            .unwrap();
        let health_response = health_task.await.unwrap();
        assert_eq!(health_response.len(), 1);
        match serde_json::from_slice::<ClientControlResponse>(&health_response[0].body).unwrap() {
            ClientControlResponse::SupervisorHealthProbe {
                module_id,
                status,
                detail,
                metrics,
            } => {
                assert_eq!(module_id, "aft");
                assert_eq!(status, HealthStatus::Degraded);
                assert_eq!(detail.as_deref(), Some("warming"));
                assert_eq!(metrics, Some(json!({"queue_depth": 3})));
            }
            other => panic!("unexpected health response: {other:?}"),
        }

        handler
            .handle_control_frame(&module_ctx, route_bind_ack(bind_frame.header.corr))
            .await
            .unwrap();
        let route_response = route_task.await.unwrap();
        assert_eq!(route_response.len(), 1);
        assert!(matches!(
            serde_json::from_slice::<ClientControlResponse>(&route_response[0].body).unwrap(),
            ClientControlResponse::RouteOpen { .. }
        ));
    }

    #[tokio::test]
    async fn route_open_relays_consumer_capabilities_verbatim() {
        let registry = Arc::new(Registry::default());
        let forwarding = Arc::new(ForwardingTable::default());
        let handler =
            ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding));
        let (module_ctx, mut module_rx) = route_ctx(ConnectionId::new(37));
        handler
            .handle_control_frame(&module_ctx, hello_frame("aft", PROTOCOL_VERSION, 7))
            .await
            .unwrap();

        let expected = vec!["elicitation".to_string(), "roots".to_string()];
        let expected_for_request = expected.clone();
        let project_root = unique_project_root("consumer-capabilities-present");
        let (client_ctx, _client_rx) = route_ctx(ConnectionId::new(38));
        let route_handler = handler.clone();
        let route_task = tokio::spawn(async move {
            route_handler
                .handle_control_frame(
                    &client_ctx,
                    route_open_frame_with_consumer_capabilities(
                        401,
                        "aft",
                        project_root,
                        Some(expected_for_request),
                    ),
                )
                .await
                .unwrap()
        });
        let bind_frame = tokio::time::timeout(Duration::from_secs(1), module_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let bind: ModuleControlRequest = serde_json::from_slice(&bind_frame.body).unwrap();
        let ModuleControlRequest::RouteBind {
            consumer_capabilities,
            ..
        } = bind
        else {
            panic!("expected route.bind request, got {bind:?}");
        };
        assert_eq!(consumer_capabilities, Some(expected.clone()));

        handler
            .handle_control_frame(&module_ctx, route_bind_ack(bind_frame.header.corr))
            .await
            .unwrap();
        let route_response = route_task.await.unwrap();
        assert!(matches!(
            serde_json::from_slice::<ClientControlResponse>(&route_response[0].body).unwrap(),
            ClientControlResponse::RouteOpen { .. }
        ));
    }

    #[tokio::test]
    async fn route_open_without_consumer_capabilities_relays_none() {
        let registry = Arc::new(Registry::default());
        let forwarding = Arc::new(ForwardingTable::default());
        let handler =
            ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding));
        let (module_ctx, mut module_rx) = route_ctx(ConnectionId::new(39));
        handler
            .handle_control_frame(&module_ctx, hello_frame("aft", PROTOCOL_VERSION, 7))
            .await
            .unwrap();

        let project_root = unique_project_root("consumer-capabilities-absent");
        let (client_ctx, _client_rx) = route_ctx(ConnectionId::new(40));
        let route_handler = handler.clone();
        let route_task = tokio::spawn(async move {
            route_handler
                .handle_control_frame(&client_ctx, route_open_frame(402, "aft", project_root))
                .await
                .unwrap()
        });
        let bind_frame = tokio::time::timeout(Duration::from_secs(1), module_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let bind: ModuleControlRequest = serde_json::from_slice(&bind_frame.body).unwrap();
        let ModuleControlRequest::RouteBind {
            consumer_capabilities,
            ..
        } = bind
        else {
            panic!("expected route.bind request, got {bind:?}");
        };
        assert_eq!(consumer_capabilities, None);

        handler
            .handle_control_frame(&module_ctx, route_bind_ack(bind_frame.header.corr))
            .await
            .unwrap();
        let route_response = route_task.await.unwrap();
        assert!(matches!(
            serde_json::from_slice::<ClientControlResponse>(&route_response[0].body).unwrap(),
            ClientControlResponse::RouteOpen { .. }
        ));
    }

    #[tokio::test]
    async fn supervision_only_module_health_probe_does_not_enable_route_open_and_cleans_up() {
        let registry = Arc::new(Registry::default());
        let forwarding = Arc::new(ForwardingTable::default());
        let handler =
            ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding))
                .with_health_probe_timeout(Duration::from_secs(5));
        let (module_ctx, mut module_rx) = route_ctx(ConnectionId::new(35));
        let responses = handler
            .handle_control_frame(
                &module_ctx,
                non_routable_hello_frame_with_control_ops(
                    "mcp",
                    300,
                    Some(vec![MODULE_CONTROL_OP_HEALTH_CHECK.to_string()]),
                ),
            )
            .await
            .unwrap();
        assert_eq!(responses[0].header.ty, FrameType::HelloAck);
        assert!(registry
            .get_module("mcp")
            .unwrap()
            .unwrap()
            .manifest
            .provides
            .is_empty());

        let (route_client_ctx, _route_client_rx) = route_ctx(ConnectionId::new(36));
        let route_response = handler
            .handle_control_frame(
                &route_client_ctx,
                route_open_frame(301, "mcp", unique_project_root("non-routable-mcp")),
            )
            .await
            .unwrap();
        assert_eq!(route_response[0].header.ty, FrameType::Error);
        assert_eq!(
            parse_error(&route_response[0])["code"],
            "target_unavailable"
        );
        assert!(parse_error(&route_response[0])["message"]
            .as_str()
            .unwrap()
            .contains("does not provide the requested target"));
        assert!(module_rx.try_recv().is_err());

        let (health_client_ctx, _health_client_rx) = route_ctx(ConnectionId::new(37));
        let health_handler = handler.clone();
        let health_task = tokio::spawn(async move {
            health_handler
                .handle_control_frame(
                    &health_client_ctx,
                    supervisor_health_probe_frame(302, "mcp"),
                )
                .await
                .unwrap()
        });
        let health_frame = tokio::time::timeout(Duration::from_secs(1), module_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<ModuleControlRequest>(&health_frame.body).unwrap(),
            ModuleControlRequest::HealthCheck {}
        );
        handler
            .handle_control_frame(
                &module_ctx,
                health_response(health_frame.header.corr, HealthStatus::Ok),
            )
            .await
            .unwrap();
        let health_response = health_task.await.unwrap();
        assert_eq!(health_response[0].header.ty, FrameType::Response);
        match serde_json::from_slice::<ClientControlResponse>(&health_response[0].body).unwrap() {
            ClientControlResponse::SupervisorHealthProbe {
                module_id, status, ..
            } => {
                assert_eq!(module_id, "mcp");
                assert_eq!(status, HealthStatus::Ok);
            }
            other => panic!("unexpected health response: {other:?}"),
        }

        // Exercise the forwarding cleanup path directly while leaving the registry
        // advertisement in place. If cleanup leaves a stale control sink behind,
        // the next probe will enqueue onto it and wait for the long probe timeout
        // instead of returning an immediate no-connection error.
        forwarding
            .cleanup_connection(module_ctx.connection_id)
            .unwrap();
        let (cleanup_probe_ctx, _cleanup_probe_rx) = route_ctx(ConnectionId::new(38));
        let cleanup_response = tokio::time::timeout(
            Duration::from_millis(200),
            handler.handle_control_frame(
                &cleanup_probe_ctx,
                supervisor_health_probe_frame(303, "mcp"),
            ),
        )
        .await
        .expect("probe should fail immediately when the control lane is gone")
        .unwrap();
        assert_eq!(cleanup_response[0].header.ty, FrameType::Error);
        assert_eq!(
            parse_error(&cleanup_response[0])["code"],
            "target_unavailable"
        );
        assert!(parse_error(&cleanup_response[0])["message"]
            .as_str()
            .unwrap()
            .contains("no module connection"));

        handler
            .cleanup_connection(module_ctx.connection_id)
            .unwrap();
    }

    #[tokio::test]
    async fn health_probe_timeout_and_module_death_are_typed() {
        let registry = Arc::new(Registry::default());
        let forwarding = Arc::new(ForwardingTable::default());
        let handler =
            ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding))
                .with_health_probe_timeout(Duration::from_millis(50));
        let (module_ctx, mut module_rx) = route_ctx(ConnectionId::new(40));
        handler
            .handle_control_frame(
                &module_ctx,
                hello_frame_with_control_ops(
                    "aft",
                    PROTOCOL_VERSION,
                    7,
                    Some(vec![MODULE_CONTROL_OP_HEALTH_CHECK.to_string()]),
                ),
            )
            .await
            .unwrap();

        let (client_ctx, _client_rx) = route_ctx(ConnectionId::new(41));
        let responses = handler
            .handle_control_frame(&client_ctx, supervisor_health_probe_frame(201, "aft"))
            .await
            .unwrap();
        assert_eq!(responses[0].header.ty, FrameType::Error);
        assert_eq!(parse_error(&responses[0])["code"], "module_timeout");
        let _ = module_rx.try_recv();

        let (client_ctx, _client_rx) = route_ctx(ConnectionId::new(42));
        let health_handler = handler.clone();
        let death_task = tokio::spawn(async move {
            health_handler
                .handle_control_frame(&client_ctx, supervisor_health_probe_frame(202, "aft"))
                .await
                .unwrap()
        });
        tokio::time::timeout(Duration::from_secs(1), module_rx.recv())
            .await
            .unwrap()
            .unwrap();
        handler
            .cleanup_connection(module_ctx.connection_id)
            .unwrap();
        let responses = death_task.await.unwrap();
        assert_eq!(responses[0].header.ty, FrameType::Error);
        assert_eq!(parse_error(&responses[0])["code"], "target_unavailable");
    }

    #[test]
    fn hello_below_version_floor_returns_error_without_registration() {
        let registry = Arc::new(Registry::default());
        let handler = ControlHandler::new(Arc::clone(&registry));

        let responses = handler
            .handle_control(ConnectionId::new(1), hello_frame("aft", 0, 9))
            .unwrap();

        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0].header.ty, FrameType::Error);
        let error = parse_error(&responses[0]);
        assert_eq!(error["code"], "version_unsupported");
        assert!(registry.get_module("aft").unwrap().is_none());
        assert_eq!(registry.active_registration_count().unwrap(), 0);
    }

    #[test]
    fn unknown_module_push_op_is_ignored_but_malformed_known_op_errors() {
        let registry = Arc::new(Registry::default());
        let forwarding = Arc::new(ForwardingTable::default());
        let handler =
            ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding));
        let module_connection = ConnectionId::new(301);
        let registration = registry
            .register_with_control_ops(
                manifest("aft-push", PROTOCOL_VERSION),
                PROTOCOL_VERSION,
                module_connection,
                module_baseline_control_ops(),
            )
            .unwrap();
        let (module_tx, _module_rx) = mpsc::channel(8);
        let endpoint = forwarding
            .register_module_connection(
                module_connection,
                "aft-push".to_string(),
                PROTOCOL_VERSION,
                manifest_concurrency(&registration.manifest),
                FrameSink::new(module_tx),
            )
            .unwrap();

        // A push op this version does not know is ignored (forward-compat), not errored.
        let unknown = Frame::build(
            FrameType::Push,
            control_flags(),
            0,
            5,
            serde_json::to_vec(&json!({"op": "route.future.v2", "extra": 1})).unwrap(),
        )
        .unwrap();
        let out = handler.handle_status_update(endpoint, unknown).unwrap();
        assert!(
            out.is_empty(),
            "unknown push op must be ignored, got {out:?}"
        );

        // A malformed body for a KNOWN op is a real error worth surfacing.
        let malformed = Frame::build(
            FrameType::Push,
            control_flags(),
            0,
            6,
            serde_json::to_vec(&json!({"op": "route.status"})).unwrap(),
        )
        .unwrap();
        let out = handler.handle_status_update(endpoint, malformed).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].header.ty, FrameType::Error);
        assert_eq!(parse_error(&out[0])["code"], "invalid_control_body");
    }

    #[test]
    fn hello_rejected_when_connection_already_owns_client_routes() {
        let registry = Arc::new(Registry::default());
        let forwarding = Arc::new(ForwardingTable::default());
        let handler =
            ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding));
        // Commits a client route on connection 202 (bound to a module on conn 101).
        let _ = bind_liveness_route(&registry, &forwarding, "aft-module");
        let client_connection = ConnectionId::new(202);

        // That same connection now tries to register as a module: rejected, so one
        // connection never holds both client-route and module-endpoint state.
        let responses = handler
            .handle_control(
                client_connection,
                hello_frame("aft-second", PROTOCOL_VERSION, 9),
            )
            .unwrap();
        assert_eq!(responses[0].header.ty, FrameType::Error);
        assert_eq!(parse_error(&responses[0])["code"], "invalid_hello");
        assert!(registry.get_module("aft-second").unwrap().is_none());
    }

    #[test]
    fn reserved_module_hello_requires_matching_launch_nonce() {
        let registry = Arc::new(Registry::default());
        let supervisor = SupervisorHandle::new();
        // The supervisor recorded the nonce it injected when it spawned the reserved
        // module; the HELLO verifier checks against the same shared handle.
        supervisor.set_reserved_nonce("vault", "the-real-nonce".to_string());
        let handler = ControlHandler::new(Arc::clone(&registry)).with_supervisor(supervisor);

        // A HELLO with NO nonce is rejected.
        let no_nonce = handler
            .handle_control(
                ConnectionId::new(1),
                hello_frame("vault", PROTOCOL_VERSION, 1),
            )
            .unwrap();
        assert_eq!(no_nonce[0].header.ty, FrameType::Error);
        assert_eq!(parse_error(&no_nonce[0])["code"], "reserved_module");
        assert!(registry.get_module("vault").unwrap().is_none());

        // A HELLO with the WRONG nonce is rejected.
        let wrong = handler
            .handle_control(
                ConnectionId::new(2),
                hello_frame_with_nonce("vault", PROTOCOL_VERSION, 2, Some("forged")),
            )
            .unwrap();
        assert_eq!(wrong[0].header.ty, FrameType::Error);
        assert_eq!(parse_error(&wrong[0])["code"], "reserved_module");
        assert!(registry.get_module("vault").unwrap().is_none());

        // A HELLO with the CORRECT nonce registers.
        let ok = handler
            .handle_control(
                ConnectionId::new(3),
                hello_frame_with_nonce("vault", PROTOCOL_VERSION, 3, Some("the-real-nonce")),
            )
            .unwrap();
        assert_eq!(ok[0].header.ty, FrameType::HelloAck);
        assert!(registry.get_module("vault").unwrap().is_some());
    }

    #[test]
    fn reserved_prefix_hello_uses_delimiter_sensitive_owner_nonce() {
        let registry = Arc::new(Registry::default());
        let supervisor = SupervisorHandle::new();
        supervisor.set_spawn_nonce("federation", "owner-nonce".to_string());
        supervisor.set_reserved_prefixes("federation", &["fed:".to_string()]);
        let handler = ControlHandler::new(Arc::clone(&registry)).with_supervisor(supervisor);

        let squat = handler
            .handle_control(
                ConnectionId::new(1),
                hello_frame("fed:peerA:tool", PROTOCOL_VERSION, 1),
            )
            .unwrap();
        assert_eq!(squat[0].header.ty, FrameType::Error);
        assert_eq!(parse_error(&squat[0])["code"], "reserved_module");
        assert!(parse_error(&squat[0])["message"]
            .as_str()
            .unwrap()
            .contains("fed:"));

        let accepted_peer = handler
            .handle_control(
                ConnectionId::new(2),
                hello_frame_with_nonce("fed:peerA:tool", PROTOCOL_VERSION, 2, Some("owner-nonce")),
            )
            .unwrap();
        assert_eq!(accepted_peer[0].header.ty, FrameType::HelloAck);

        let accepted_short = handler
            .handle_control(
                ConnectionId::new(3),
                hello_frame_with_nonce("fed:x", PROTOCOL_VERSION, 3, Some("owner-nonce")),
            )
            .unwrap();
        assert_eq!(accepted_short[0].header.ty, FrameType::HelloAck);

        for (conn, module_id) in [(4, "fedx:tool"), (5, "fed"), (6, "FED:x")] {
            let response = handler
                .handle_control(
                    ConnectionId::new(conn),
                    hello_frame(module_id, PROTOCOL_VERSION, conn),
                )
                .unwrap();
            assert_eq!(response[0].header.ty, FrameType::HelloAck, "{module_id}");
        }
    }

    #[test]
    fn exact_reserved_module_takes_precedence_over_reserved_prefix() {
        let registry = Arc::new(Registry::default());
        let supervisor = SupervisorHandle::new();
        supervisor.set_spawn_nonce("federation", "owner-nonce".to_string());
        supervisor.set_reserved_prefixes("federation", &["fed:".to_string()]);
        supervisor.set_reserved_nonce("fed:special", "exact-nonce".to_string());
        let handler = ControlHandler::new(Arc::clone(&registry)).with_supervisor(supervisor);

        let owner_nonce = handler
            .handle_control(
                ConnectionId::new(1),
                hello_frame_with_nonce("fed:special", PROTOCOL_VERSION, 1, Some("owner-nonce")),
            )
            .unwrap();
        assert_eq!(owner_nonce[0].header.ty, FrameType::Error);
        assert_eq!(parse_error(&owner_nonce[0])["code"], "reserved_module");
        assert!(registry.get_module("fed:special").unwrap().is_none());

        let exact_nonce = handler
            .handle_control(
                ConnectionId::new(2),
                hello_frame_with_nonce("fed:special", PROTOCOL_VERSION, 2, Some("exact-nonce")),
            )
            .unwrap();
        assert_eq!(exact_nonce[0].header.ty, FrameType::HelloAck);
        assert!(registry.get_module("fed:special").unwrap().is_some());
    }

    #[test]
    fn non_reserved_module_ignores_launch_nonce() {
        let registry = Arc::new(Registry::default());
        // No reserved nonce recorded for these ids: they are not reserved, so HELLO
        // registration succeeds whether a spawned process echoes a nonce or not.
        let handler = ControlHandler::new(Arc::clone(&registry));
        let no_nonce = handler
            .handle_control(
                ConnectionId::new(1),
                hello_frame("aft-no-nonce", PROTOCOL_VERSION, 1),
            )
            .unwrap();
        assert_eq!(no_nonce[0].header.ty, FrameType::HelloAck);
        assert!(registry.get_module("aft-no-nonce").unwrap().is_some());

        let echoed_nonce = handler
            .handle_control(
                ConnectionId::new(2),
                hello_frame_with_nonce("aft-with-nonce", PROTOCOL_VERSION, 2, Some("spawn-nonce")),
            )
            .unwrap();
        assert_eq!(echoed_nonce[0].header.ty, FrameType::HelloAck);
        assert!(registry.get_module("aft-with-nonce").unwrap().is_some());
    }

    #[test]
    fn malformed_hello_returns_error_and_handler_still_answers_ping() {
        let handler = ControlHandler::default();
        let conn = ConnectionId::new(1);
        let malformed = Frame::build(
            FrameType::Hello,
            control_flags(),
            0,
            3,
            b"{not json".to_vec(),
        )
        .unwrap();

        let error = handler.handle_control(conn, malformed).unwrap();
        assert_eq!(error[0].header.ty, FrameType::Error);
        assert_eq!(parse_error(&error[0])["code"], "invalid_hello");

        let ping = Frame::build(FrameType::Ping, control_flags(), 0, 4, Vec::new()).unwrap();
        let pong = handler.handle_control(conn, ping).unwrap();
        assert_eq!(pong[0].header.ty, FrameType::Pong);
        assert_eq!(pong[0].header.corr, 4);
    }

    #[test]
    fn duplicate_module_id_is_rejected_without_replacing_active_registration() {
        let registry = Arc::new(Registry::default());
        let handler = ControlHandler::new(Arc::clone(&registry));

        handler
            .handle_control(
                ConnectionId::new(1),
                hello_frame("aft", PROTOCOL_VERSION, 1),
            )
            .unwrap();
        let duplicate = handler
            .handle_control(
                ConnectionId::new(2),
                hello_frame("aft", PROTOCOL_VERSION, 2),
            )
            .unwrap();

        assert_eq!(duplicate[0].header.ty, FrameType::Error);
        assert_eq!(parse_error(&duplicate[0])["code"], "duplicate_module_id");
        let registration = registry.get_module("aft").unwrap().unwrap();
        assert_eq!(registration.connection_id, ConnectionId::new(1));
    }

    #[test]
    fn liveness_poll_reports_false_when_process_liveness_reports_dead() {
        let registry = Arc::new(Registry::default());
        let forwarding = Arc::new(ForwardingTable::default());
        let process_liveness = Arc::new(FakeProcessLiveness { live: Some(false) });
        let handler =
            ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding))
                .with_process_liveness(process_liveness);
        let (ctx, route_channel) = bind_liveness_route(&registry, &forwarding, "aft-dead");
        let responses = handler
            .handle_route_poll(
                &ctx,
                route_poll_frame(41, PollKind::Liveness, route_channel),
                route_channel,
                PollKind::Liveness,
            )
            .unwrap();

        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0].header.ty, FrameType::Response);
        assert_route_poll_liveness(&responses[0], false);
    }

    #[test]
    fn liveness_poll_without_process_source_uses_bound_route() {
        let registry = Arc::new(Registry::default());
        let forwarding = Arc::new(ForwardingTable::default());
        let handler =
            ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding));
        let (ctx, route_channel) = bind_liveness_route(&registry, &forwarding, "aft-bound-only");
        let responses = handler
            .handle_route_poll(
                &ctx,
                route_poll_frame(42, PollKind::Liveness, route_channel),
                route_channel,
                PollKind::Liveness,
            )
            .unwrap();

        assert_route_poll_liveness(&responses[0], true);
    }

    #[test]
    fn liveness_poll_untracked_process_source_uses_bound_route() {
        let registry = Arc::new(Registry::default());
        let forwarding = Arc::new(ForwardingTable::default());
        let process_liveness = Arc::new(FakeProcessLiveness { live: None });
        let handler =
            ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding))
                .with_process_liveness(process_liveness);
        let (ctx, route_channel) = bind_liveness_route(&registry, &forwarding, "aft-untracked");
        let responses = handler
            .handle_route_poll(
                &ctx,
                route_poll_frame(43, PollKind::Liveness, route_channel),
                route_channel,
                PollKind::Liveness,
            )
            .unwrap();

        assert_route_poll_liveness(&responses[0], true);
    }

    #[tokio::test]
    async fn unknown_op_returns_unknown_control_op() {
        let handler = ControlHandler::default();
        let (ctx, _rx) = route_ctx(ConnectionId::new(77));
        let request = Frame::build(
            FrameType::Request,
            control_flags(),
            0,
            55,
            br#"{"op":"route.nope","route_channel":1}"#.to_vec(),
        )
        .unwrap();

        let response = handler.handle_control_frame(&ctx, request).await.unwrap();

        assert_eq!(response.len(), 1);
        assert_eq!(response[0].header.ty, FrameType::Error);
        assert_eq!(response[0].header.corr, 55);
        assert_eq!(parse_error(&response[0])["code"], "unknown_control_op");
    }

    #[tokio::test]
    async fn malformed_control_bodies_return_invalid_control_body() {
        let handler = ControlHandler::default();
        let (ctx, _rx) = route_ctx(ConnectionId::new(78));

        for (corr, body) in [
            (56, br#"{"route_channel":1}"#.as_slice()),
            (57, br#"{"op":17,"route_channel":1}"#.as_slice()),
            (
                58,
                br#"{"op":"route.poll","route_channel":"bad","kind":"status"}"#.as_slice(),
            ),
        ] {
            let request =
                Frame::build(FrameType::Request, control_flags(), 0, corr, body.to_vec()).unwrap();
            let response = handler.handle_control_frame(&ctx, request).await.unwrap();

            assert_eq!(response.len(), 1);
            assert_eq!(response[0].header.ty, FrameType::Error);
            assert_eq!(response[0].header.corr, corr);
            assert_eq!(parse_error(&response[0])["code"], "invalid_control_body");
        }
    }

    #[tokio::test]
    async fn goodbye_tears_down_registration_and_later_channel_is_unknown() {
        let registry = Arc::new(Registry::default());
        let control = Arc::new(ControlHandler::new(Arc::clone(&registry)));
        let router = Router::with_control_handler(Arc::clone(&control));
        let connection = router.begin_connection();
        let (ctx, mut rx) = route_ctx(connection.id());

        router
            .route_for_connection(&ctx, hello_frame("aft", PROTOCOL_VERSION, 11))
            .await
            .unwrap();
        let response = rx.recv().await.unwrap();
        let ack = parse_ack(&response);
        assert_eq!(ack.negotiated_ver, PROTOCOL_VERSION);
        let channel = 1;

        let goodbye = Frame::build(FrameType::Goodbye, control_flags(), 0, 12, Vec::new()).unwrap();
        router.route_for_connection(&ctx, goodbye).await.unwrap();
        assert!(rx.try_recv().is_err());
        assert!(registry.get_module("aft").unwrap().is_none());

        router
            .route_for_connection(&ctx, channel_request(channel, 13))
            .await
            .unwrap();
        let error_frame = rx.recv().await.unwrap();
        assert_eq!(error_frame.header.ty, FrameType::Error);
        assert_eq!(error_frame.header.channel, channel);
    }

    #[tokio::test]
    async fn dropping_router_connection_releases_registration() {
        let registry = Arc::new(Registry::default());
        let control = Arc::new(ControlHandler::new(Arc::clone(&registry)));
        let router = Router::with_control_handler(control);
        let connection = router.begin_connection();
        let (ctx, mut rx) = route_ctx(connection.id());

        router
            .route_for_connection(&ctx, hello_frame("aft", PROTOCOL_VERSION, 31))
            .await
            .unwrap();
        let response = rx.recv().await.unwrap();
        let ack = parse_ack(&response);
        assert_eq!(ack.negotiated_ver, PROTOCOL_VERSION);
        assert!(registry.get_module("aft").unwrap().is_some());

        drop(connection);

        assert!(registry.get_module("aft").unwrap().is_none());
        assert_eq!(registry.active_registration_count().unwrap(), 0);
    }

    #[test]
    fn unsupported_channel_zero_frame_returns_error() {
        let handler = ControlHandler::default();
        let request = Frame::build(
            FrameType::Request,
            control_flags(),
            0,
            21,
            b"opaque".to_vec(),
        )
        .unwrap();

        let response = handler
            .handle_control(ConnectionId::new(1), request)
            .unwrap();

        assert_eq!(response[0].header.ty, FrameType::Error);
        assert_eq!(
            parse_error(&response[0])["code"],
            "unsupported_control_frame"
        );
    }
}
