use std::{fmt, sync::Arc};

use serde::{Deserialize, Serialize};
use subc_protocol::{
    manifest::{Concurrency, ModuleManifest, ProviderRole},
    ErrorBody, Flags, FrameType, Priority, PROTOCOL_VERSION,
};
use tracing::{debug, warn};

use crate::{
    forwarding::{
        AttachAck, AttachRelay, AttachRelayOutcome, AttachRelayResponse, AttachRequest,
        DetachRelay, ForwardingError, ForwardingTable, ModuleEndpointId, ReleasedRoute,
    },
    registry::{ConnectionId, Registry, RegistryError},
    router::{RouteCtx, RouterError},
    status::{LivenessReply, PassivePoll, PollOp, StatusReply, StatusUpdate},
    supervise::ModuleProcessLiveness,
    Frame, ProjectRootId,
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HelloBody {
    pub manifest: ModuleManifest,
    pub protocol_ver: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HelloAckBody {
    pub negotiated_ver: u8,
    pub channels: Vec<u16>,
    pub subc_capabilities: Vec<String>,
}

/// Real channel-0 control handler for subc itself.
#[derive(Clone)]
pub struct ControlHandler {
    registry: Arc<Registry>,
    forwarding: Arc<ForwardingTable>,
    process_liveness: Option<Arc<dyn ModuleProcessLiveness>>,
    subc_capabilities: Arc<[String]>,
}

impl fmt::Debug for ControlHandler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ControlHandler")
            .field("registry", &self.registry)
            .field("forwarding", &self.forwarding)
            .field("process_liveness", &self.process_liveness.is_some())
            .field("subc_capabilities", &self.subc_capabilities)
            .finish()
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
            subc_capabilities: Arc::from([
                CAP_MANIFEST_REGISTRATION.to_string(),
                CAP_CHANNEL_LIFECYCLE.to_string(),
                CAP_PING_PONG.to_string(),
                CAP_SESSION_ATTACH.to_string(),
            ]),
        }
    }

    pub fn with_process_liveness(
        mut self,
        process_liveness: Arc<dyn ModuleProcessLiveness>,
    ) -> Self {
        self.process_liveness = Some(process_liveness);
        self
    }

    pub fn registry(&self) -> Arc<Registry> {
        Arc::clone(&self.registry)
    }

    pub fn forwarding(&self) -> Arc<ForwardingTable> {
        Arc::clone(&self.forwarding)
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
                    return Ok(vec![control_error_frame(
                        &frame,
                        "unsupported_control_frame",
                        "module-originated channel-0 REQUEST is not supported",
                    )?]);
                }
                if let Ok(poll) = serde_json::from_slice::<PassivePoll>(&frame.body) {
                    return self.handle_passive_poll(ctx, frame, poll);
                }
                self.handle_attach(ctx, frame).await
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
        let registrations = self.registry.deregister_connection(connection_id);
        if let Ok(released_routes) = self.forwarding.cleanup_connection(connection_id) {
            self.emit_detach_relays(released_routes);
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
        self.emit_detach_relays(vec![released_route]);
        Ok(true)
    }

    fn emit_detach_relays(&self, released_routes: Vec<ReleasedRoute>) {
        for released in released_routes {
            let body = match serde_json::to_vec(&DetachRelay {
                route_channel: released.route_channel,
            }) {
                Ok(body) => body,
                Err(err) => {
                    warn!(
                        route_channel = released.route_channel,
                        error = %err,
                        "failed to encode detach-relay"
                    );
                    continue;
                }
            };
            let frame = match Frame::build_with_version(
                released.negotiated_ver,
                FrameType::Request,
                control_flags(),
                0,
                0,
                body,
            ) {
                Ok(frame) => frame,
                Err(err) => {
                    warn!(
                        route_channel = released.route_channel,
                        error = %err,
                        "failed to build detach-relay frame"
                    );
                    continue;
                }
            };
            if let Err(err) = released.module_sink.try_send(frame) {
                warn!(
                    route_channel = released.route_channel,
                    error = %err,
                    "best-effort detach-relay was not delivered"
                );
            }
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
        let hello = match serde_json::from_slice::<HelloBody>(&frame.body) {
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

        let registration =
            match self
                .registry
                .register(hello.manifest, negotiated_ver, connection_id)
            {
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

        if manifest_provides_tools(&registration.manifest) {
            if let Some(sink) = sink {
                let concurrency = manifest_concurrency(&registration.manifest);
                if let Err(err) = self.forwarding.register_module_connection(
                    connection_id,
                    registration.manifest.module_id.clone(),
                    negotiated_ver,
                    concurrency,
                    sink,
                ) {
                    let _ = self.registry.deregister_connection(connection_id);
                    return Ok(vec![control_error_frame(
                        &frame,
                        forwarding_error_code(&err),
                        err.to_string(),
                    )?]);
                }
            }
        }

        let ack = HelloAckBody {
            negotiated_ver,
            channels: registration.channels,
            subc_capabilities: self.subc_capabilities.as_ref().to_vec(),
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

    async fn handle_attach(&self, ctx: &RouteCtx, frame: Frame) -> Result<Vec<Frame>, RouterError> {
        debug!(
            connection_id = ctx.connection_id.get(),
            corr = frame.header.corr,
            "handling session attach"
        );
        let attach = match serde_json::from_slice::<AttachRequest>(&frame.body) {
            Ok(attach) => attach,
            Err(err) => {
                return Ok(vec![control_error_frame(
                    &frame,
                    "invalid_attach_request",
                    format!("malformed AttachRequest body: {err}"),
                )?])
            }
        };

        let project_root = match ProjectRootId::from_path(&attach.project_root) {
            Ok(project_root) => project_root,
            Err(err) => {
                return Ok(vec![control_error_frame(
                    &frame,
                    "invalid_project_root",
                    err.to_string(),
                )?])
            }
        };

        let pending = match self.forwarding.begin_attach_relay() {
            Ok(pending) => pending,
            Err(err) => {
                return Ok(vec![control_error_frame(
                    &frame,
                    forwarding_error_code(&err),
                    err.to_string(),
                )?])
            }
        };
        let endpoint = pending.endpoint;
        let route_channel = pending.route_channel;
        let relay_corr = pending.corr;

        let relay = AttachRelay {
            route_channel,
            project_root: project_root.as_path().to_path_buf(),
            harness: attach.harness,
            session: attach.session,
            config: attach.config,
        };
        let relay_body = serde_json::to_vec(&relay).map_err(|err| {
            RouterError::backend(
                0,
                frame.header.corr,
                format!("failed to encode AttachRelay: {err}"),
            )
        })?;
        let relay_frame = Frame::build_with_version(
            pending.negotiated_ver,
            FrameType::Request,
            control_flags(),
            0,
            relay_corr,
            relay_body,
        )
        .map_err(RouterError::FrameBuild)?;

        if let Err(err) = pending.module_sink.send(relay_frame).await {
            let _ = self.forwarding.cancel_pending_relay(endpoint, relay_corr);
            let _ = self
                .forwarding
                .release_reserved_route(endpoint, route_channel);
            return Ok(vec![control_error_frame(
                &frame,
                "module_unavailable",
                err.to_string(),
            )?]);
        }

        match pending.receiver.await {
            Ok(AttachRelayOutcome::Accepted) => {
                if let Err(err) = self.forwarding.commit_route(
                    ctx.connection_id,
                    ctx.egress.clone(),
                    endpoint,
                    route_channel,
                ) {
                    let _ = self
                        .forwarding
                        .release_reserved_route(endpoint, route_channel);
                    return Ok(vec![control_error_frame(
                        &frame,
                        forwarding_error_code(&err),
                        err.to_string(),
                    )?]);
                }
                let body = serde_json::to_vec(&AttachAck { route_channel }).map_err(|err| {
                    RouterError::backend(
                        0,
                        frame.header.corr,
                        format!("failed to encode AttachAck: {err}"),
                    )
                })?;
                Ok(vec![Frame::build_with_version(
                    response_version(&frame),
                    FrameType::Response,
                    control_flags(),
                    0,
                    frame.header.corr,
                    body,
                )
                .map_err(RouterError::FrameBuild)?])
            }
            Ok(AttachRelayOutcome::Rejected(body)) => {
                let _ = self
                    .forwarding
                    .release_reserved_route(endpoint, route_channel);
                Ok(vec![control_error_body_frame(&frame, body)?])
            }
            Ok(AttachRelayOutcome::ModuleGone(message)) => {
                let _ = self
                    .forwarding
                    .release_reserved_route(endpoint, route_channel);
                Ok(vec![control_error_frame(
                    &frame,
                    "module_unavailable",
                    message,
                )?])
            }
            Err(_) => {
                let _ = self
                    .forwarding
                    .release_reserved_route(endpoint, route_channel);
                Ok(vec![control_error_frame(
                    &frame,
                    "module_unavailable",
                    "attach relay waiter was canceled before the module responded",
                )?])
            }
        }
    }

    fn handle_status_update(
        &self,
        endpoint: ModuleEndpointId,
        frame: Frame,
    ) -> Result<Vec<Frame>, RouterError> {
        let update = match serde_json::from_slice::<StatusUpdate>(&frame.body) {
            Ok(update) => update,
            Err(err) => {
                return Ok(vec![control_error_frame(
                    &frame,
                    "invalid_status_update",
                    format!("malformed StatusUpdate body: {err}"),
                )?])
            }
        };

        self.forwarding
            .cache_status(endpoint, update.route_channel, update.status)
            .map_err(RouterError::Forwarding)?;
        Ok(Vec::new())
    }

    fn handle_passive_poll(
        &self,
        ctx: &RouteCtx,
        frame: Frame,
        poll: PassivePoll,
    ) -> Result<Vec<Frame>, RouterError> {
        match poll.op {
            PollOp::Liveness => {
                let route_bound_to_active_module = self
                    .forwarding
                    .client_route_is_bound_to_active_module(ctx.connection_id, poll.route_channel)
                    .map_err(RouterError::Forwarding)?;
                let process_running = if route_bound_to_active_module {
                    self.process_running_for_route(ctx.connection_id, poll.route_channel)
                        .map_err(|message| {
                            RouterError::backend(frame.header.channel, frame.header.corr, message)
                        })?
                } else {
                    false
                };
                let live = route_bound_to_active_module && process_running;
                Ok(vec![control_response_body_frame(
                    &frame,
                    &LivenessReply { live },
                    "LivenessReply",
                )?])
            }
            PollOp::Status => {
                let Some(endpoint) = self
                    .forwarding
                    .client_route_endpoint(ctx.connection_id, poll.route_channel)
                    .map_err(RouterError::Forwarding)?
                else {
                    return Ok(vec![status_unavailable_frame(&frame, poll.route_channel)?]);
                };

                let Some(status) = self
                    .forwarding
                    .get_status(endpoint, poll.route_channel)
                    .map_err(RouterError::Forwarding)?
                else {
                    return Ok(vec![status_unavailable_frame(&frame, poll.route_channel)?]);
                };

                Ok(vec![control_response_body_frame(
                    &frame,
                    &StatusReply { status },
                    "StatusReply",
                )?])
            }
        }
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
        if let Some(registration) =
            self.registry
                .module_for_channel(route_channel)
                .map_err(|err| {
                    format!(
                        "registry error resolving module for route channel {route_channel}: {err}"
                    )
                })?
        {
            return Ok(Some(registration.manifest.module_id));
        }

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
        let outcome = match frame.header.ty {
            FrameType::Response => {
                let body = match serde_json::from_slice::<AttachRelayResponse>(&frame.body) {
                    Ok(body) => body,
                    Err(err) => {
                        return Ok(vec![control_error_frame(
                            &frame,
                            "invalid_attach_relay_response",
                            format!("malformed AttachRelay response body: {err}"),
                        )?])
                    }
                };
                if body.accept {
                    AttachRelayOutcome::Accepted
                } else {
                    AttachRelayOutcome::Rejected(ErrorBody {
                        code: "config_divergence".to_string(),
                        message: "module rejected AttachRelay".to_string(),
                    })
                }
            }
            FrameType::Error => {
                let body = match serde_json::from_slice::<ErrorBody>(&frame.body) {
                    Ok(body) => body,
                    Err(err) => {
                        return Ok(vec![control_error_frame(
                            &frame,
                            "invalid_attach_relay_error",
                            format!("malformed AttachRelay ERROR body: {err}"),
                        )?])
                    }
                };
                AttachRelayOutcome::Rejected(body)
            }
            ty => {
                return Ok(vec![control_error_frame(
                    &frame,
                    "unsupported_control_frame",
                    format!("unsupported module channel-0 frame {ty:?}"),
                )?])
            }
        };

        let _ = self
            .forwarding
            .complete_pending_relay(connection_id, frame.header.corr, outcome)
            .map_err(RouterError::Forwarding)?;
        Ok(Vec::new())
    }

    fn handle_goodbye(&self, connection_id: ConnectionId) -> Result<Vec<Frame>, RouterError> {
        debug!(connection_id = connection_id.get(), "handling GOODBYE");
        self.registry
            .deregister_connection(connection_id)
            .map_err(|err| RouterError::backend(0, 0, err.to_string()))?;
        let released_routes = self
            .forwarding
            .cleanup_connection(connection_id)
            .map_err(RouterError::Forwarding)?;
        self.emit_detach_relays(released_routes);
        Ok(Vec::new())
    }
}

impl Default for ControlHandler {
    fn default() -> Self {
        Self::new(Arc::new(Registry::default()))
    }
}

fn manifest_provides_tools(manifest: &ModuleManifest) -> bool {
    manifest
        .provides
        .iter()
        .any(|role| matches!(role, ProviderRole::ToolProvider { .. }))
}

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

fn status_unavailable_frame(frame: &Frame, route_channel: u16) -> Result<Frame, RouterError> {
    control_error_frame(
        frame,
        "status_unavailable",
        format!("status unavailable for route channel {route_channel}"),
    )
}

fn forwarding_error_code(err: &ForwardingError) -> &'static str {
    match err {
        ForwardingError::NoModuleConnection => "module_unavailable",
        ForwardingError::StaleModuleEndpoint
        | ForwardingError::UnknownReservation { .. }
        | ForwardingError::RouteChannelExhausted
        | ForwardingError::RelayCorrelationExhausted
        | ForwardingError::Poisoned => "forwarding_error",
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
    use std::{collections::BTreeMap, sync::Arc};

    use serde_json::{json, Value};
    use subc_protocol::{
        manifest::{
            Bindings, CircuitBreaker, Concurrency, ConfigBinding, ConfigSource, IdentityBinding,
            IdentityScope, ModelPolicy, ProviderRole, ScheduledTask, StorageBinding, StorageKind,
            StorageScope, TaskEligibility, Tool,
        },
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
                    mutates: false,
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
                config: ConfigBinding {
                    source: ConfigSource::SubcMediated,
                    tiers: vec!["user".to_string(), "project".to_string()],
                    expansion: BTreeMap::new(),
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
        let body = serde_json::to_vec(&HelloBody {
            manifest: manifest(module_id, protocol_ver),
            protocol_ver,
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

    fn parse_ack(frame: &Frame) -> HelloAckBody {
        serde_json::from_slice(&frame.body).unwrap()
    }

    fn parse_error(frame: &Frame) -> Value {
        serde_json::from_slice(&frame.body).unwrap()
    }

    fn parse_liveness(frame: &Frame) -> LivenessReply {
        serde_json::from_slice(&frame.body).unwrap()
    }

    fn passive_poll_frame(corr: u64, op: PollOp, route_channel: u16) -> Frame {
        let body = serde_json::to_vec(&PassivePoll { op, route_channel }).unwrap();
        Frame::build(FrameType::Request, control_flags(), 0, corr, body).unwrap()
    }

    fn bind_liveness_route(
        registry: &Registry,
        forwarding: &ForwardingTable,
        module_id: &str,
    ) -> (RouteCtx, u16) {
        let module_connection = ConnectionId::new(101);
        let client_connection = ConnectionId::new(202);
        let registration = registry
            .register(
                manifest(module_id, PROTOCOL_VERSION),
                PROTOCOL_VERSION,
                module_connection,
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
        let pending = forwarding.begin_attach_relay().unwrap();
        assert_eq!(pending.endpoint, endpoint);
        forwarding
            .complete_pending_relay(
                module_connection,
                pending.corr,
                AttachRelayOutcome::Accepted,
            )
            .unwrap();
        let (client_ctx, _client_rx) = route_ctx(client_connection);
        forwarding
            .commit_route(
                client_connection,
                client_ctx.egress.clone(),
                endpoint,
                pending.route_channel,
            )
            .unwrap();
        (client_ctx, pending.route_channel)
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
    fn hello_registers_manifest_and_returns_ack_with_active_channel() {
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
        assert_eq!(ack.channels.len(), 1);
        assert!(ack.channels[0] > 0);
        assert!(ack
            .subc_capabilities
            .contains(&CAP_MANIFEST_REGISTRATION.to_string()));

        let registration = registry.get_module("aft").unwrap().unwrap();
        assert_eq!(registration.negotiated_ver, PROTOCOL_VERSION);
        assert_eq!(registration.channels, ack.channels);
        assert_eq!(registration.state, ChannelState::Active);
        assert_eq!(registration.connection_id, conn);
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
        assert_eq!(registry.active_module_count().unwrap(), 0);
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

        let first = handler
            .handle_control(
                ConnectionId::new(1),
                hello_frame("aft", PROTOCOL_VERSION, 1),
            )
            .unwrap();
        let first_ack = parse_ack(&first[0]);
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
        assert_eq!(registration.channels, first_ack.channels);
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
        let poll = PassivePoll {
            op: PollOp::Liveness,
            route_channel,
        };

        let responses = handler
            .handle_passive_poll(
                &ctx,
                passive_poll_frame(41, PollOp::Liveness, route_channel),
                poll,
            )
            .unwrap();

        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0].header.ty, FrameType::Response);
        assert!(!parse_liveness(&responses[0]).live);
    }

    #[test]
    fn liveness_poll_without_process_source_uses_bound_route() {
        let registry = Arc::new(Registry::default());
        let forwarding = Arc::new(ForwardingTable::default());
        let handler =
            ControlHandler::with_forwarding(Arc::clone(&registry), Arc::clone(&forwarding));
        let (ctx, route_channel) = bind_liveness_route(&registry, &forwarding, "aft-bound-only");
        let poll = PassivePoll {
            op: PollOp::Liveness,
            route_channel,
        };

        let responses = handler
            .handle_passive_poll(
                &ctx,
                passive_poll_frame(42, PollOp::Liveness, route_channel),
                poll,
            )
            .unwrap();

        assert!(parse_liveness(&responses[0]).live);
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
        let poll = PassivePoll {
            op: PollOp::Liveness,
            route_channel,
        };

        let responses = handler
            .handle_passive_poll(
                &ctx,
                passive_poll_frame(43, PollOp::Liveness, route_channel),
                poll,
            )
            .unwrap();

        assert!(parse_liveness(&responses[0]).live);
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
        let channel = parse_ack(&response).channels[0];
        assert!(registry.is_channel_active(channel).unwrap());

        let goodbye = Frame::build(FrameType::Goodbye, control_flags(), 0, 12, Vec::new()).unwrap();
        router.route_for_connection(&ctx, goodbye).await.unwrap();
        assert!(rx.try_recv().is_err());
        assert!(registry.get_module("aft").unwrap().is_none());
        assert!(!registry.is_channel_active(channel).unwrap());

        router
            .route_for_connection(&ctx, channel_request(channel, 13))
            .await
            .unwrap();
        let error_frame = rx.recv().await.unwrap();
        assert_eq!(error_frame.header.ty, FrameType::Error);
        assert_eq!(error_frame.header.channel, channel);
    }

    #[tokio::test]
    async fn dropping_router_connection_releases_orphaned_channels() {
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
        let channel = parse_ack(&response).channels[0];
        assert!(registry.is_channel_active(channel).unwrap());
        assert!(registry.get_module("aft").unwrap().is_some());

        drop(connection);

        assert!(registry.get_module("aft").unwrap().is_none());
        assert!(!registry.is_channel_active(channel).unwrap());
        assert_eq!(registry.active_module_count().unwrap(), 0);
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
