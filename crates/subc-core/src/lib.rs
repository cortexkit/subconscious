//! subc daemon core.
//!
//! This crate owns the loopback TCP transport and the thin splice-router core:
//! routing decisions are made from the 17-byte envelope header, while message
//! bodies are carried as opaque bytes and are never deserialized by the router.

#![forbid(unsafe_code)]

pub mod auth;
pub mod bootstrap;
pub mod connection_file;
pub mod control;
pub mod forwarding;
mod frame;
pub mod frame_io;
pub mod identity;
pub mod registry;
pub mod router;
pub mod server;
pub mod status;
pub mod supervise;

pub use control::{ControlHandler, HelloAckBody, HelloBody, MIN_SUPPORTED_VERSION};
pub use forwarding::{
    AttachAck, AttachRelay, AttachRelayResponse, AttachRequest, ConfigTier, DetachRelay,
    ForwardingError, ForwardingTable, ModuleEndpointId,
};
pub use frame::{Frame, FrameBuildError};
pub use frame_io::{read_frame, write_frame, FrameIoError, ReadStage};
pub use identity::{IdentityError, ProjectRootId, RequestIdentity, SessionId};
pub use registry::{ChannelState, ConnectionId, ModuleRegistration, Registry, RegistryError};
pub use router::{
    Backend, EchoBackend, ForwardBackend, FrameSink, RouteCtx, Router, RouterConnection,
    RouterError,
};
pub use server::{
    handle_connection, serve_listener, serve_listeners, ConnectionError, ServerAuth, ServerError,
    DEFAULT_AUTH_DEADLINE, DEFAULT_MAX_UNAUTHENTICATED_CONNECTIONS,
};
pub use status::{LivenessReply, PassivePoll, PollOp, StatusReply, StatusUpdate};
pub use supervise::{
    ExitKind, ExitReport, ModuleSpec, ModuleState, ModuleStatus, RestartPolicy, SuperviseError,
    SupervisedModule, Supervisor, SUBC_ARG,
};
