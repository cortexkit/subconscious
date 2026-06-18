use std::sync::Arc;

use serde::{Deserialize, Serialize};
use subc_protocol::{
    manifest::ModuleManifest, ErrorBody, Flags, FrameType, Priority, PROTOCOL_VERSION,
};
use tracing::debug;

use crate::{
    registry::{ConnectionId, Registry, RegistryError},
    router::RouterError,
    Frame,
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
#[derive(Debug, Clone)]
pub struct ControlHandler {
    registry: Arc<Registry>,
    subc_capabilities: Arc<[String]>,
}

impl ControlHandler {
    pub fn new(registry: Arc<Registry>) -> Self {
        Self {
            registry,
            subc_capabilities: Arc::from([
                CAP_MANIFEST_REGISTRATION.to_string(),
                CAP_CHANNEL_LIFECYCLE.to_string(),
                CAP_PING_PONG.to_string(),
            ]),
        }
    }

    pub fn registry(&self) -> Arc<Registry> {
        Arc::clone(&self.registry)
    }

    pub fn handle_control(
        &self,
        connection_id: ConnectionId,
        frame: Frame,
    ) -> Result<Vec<Frame>, RouterError> {
        match frame.header.ty {
            FrameType::Ping => Ok(vec![pong(&frame)?]),
            FrameType::Hello => self.handle_hello(connection_id, frame),
            FrameType::Goodbye => self.handle_goodbye(connection_id),
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
        self.registry.deregister_connection(connection_id)
    }

    fn handle_hello(
        &self,
        connection_id: ConnectionId,
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

    fn handle_goodbye(&self, connection_id: ConnectionId) -> Result<Vec<Frame>, RouterError> {
        debug!(connection_id = connection_id.get(), "handling GOODBYE");
        self.registry
            .deregister_connection(connection_id)
            .map_err(|err| RouterError::backend(0, 0, err.to_string()))?;
        Ok(Vec::new())
    }
}

impl Default for ControlHandler {
    fn default() -> Self {
        Self::new(Arc::new(Registry::default()))
    }
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
    let body = serde_json::to_vec(&ErrorBody {
        code: code.to_string(),
        message: message.into(),
    })
    .map_err(|err| {
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
