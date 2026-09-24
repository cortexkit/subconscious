//! subc daemon core.
//!
//! This crate owns the loopback TCP transport and the thin splice-router core:
//! routing decisions are made from the 21-byte envelope header, while message
//! bodies are carried as opaque bytes and are never deserialized by the router.

#![forbid(unsafe_code)]

pub mod bootstrap;
pub(crate) mod capability_requirements;
mod child_roster;
mod clock;
pub mod control;
pub mod daemon_config;
#[cfg(test)]
pub(crate) mod dispatch_spike;
pub mod fleet_lint;
pub mod forwarding;
pub mod identity;
mod live_children;
pub mod machine_id;
pub mod observability;
#[allow(dead_code)]
mod provenance;
pub mod registry;
pub mod router;
pub mod server;
pub mod stderr_tail;
pub mod supervise;
mod systemd_kill_mode;
mod terminal_journal;
pub mod terminal_ring;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
pub mod watchdog;

#[cfg(feature = "bench-harness")]
pub mod bench_harness;

pub use control::{ControlHandler, MIN_SUPPORTED_VERSION};
pub use forwarding::{ForwardingError, ForwardingTable, ModuleEndpointId};
// The frame codec now lives in its natural homes: the `Frame` data type in
// subc-protocol (pure, with the envelope it accompanies) and the async I/O loop
// in subc-transport (with the authenticated stream). Re-exported here for
// callers that use the daemon library to build and exchange frames.
pub use control::{
    DEFAULT_ROUTE_BIND_BREAKER_COOLDOWN, DEFAULT_ROUTE_BIND_BREAKER_THRESHOLD,
    DEFAULT_ROUTE_BIND_RELAY_TIMEOUT,
};
pub use identity::{IdentityError, ProjectRootId, RequestIdentity, SessionId};
pub use observability::{ConnectedClients, DaemonCounters};
pub use registry::{ChannelState, ConnectionId, ModuleRegistration, Registry, RegistryError};
pub use router::{
    Backend, EchoBackend, ForwardBackend, FrameSink, RouteCtx, Router, RouterConnection,
    RouterError,
};
pub use server::{
    handle_connection, serve_listener, serve_listeners, ConnectionError, ServerAuth, ServerError,
    DEFAULT_AUTH_DEADLINE, DEFAULT_MAX_UNAUTHENTICATED_CONNECTIONS,
};
pub use subc_protocol::{Frame, FrameBuildError};
pub use subc_transport::{read_frame, write_frame, FrameIoError, ReadStage};
pub use supervise::{
    ExitKind, ExitReport, HealthAction, HealthConfig, ModuleHealthStatus, ModuleOverlap,
    ModuleProcessLiveness, ModuleSpec, ModuleState, ModuleStatus, RestartPolicy, SuperviseError,
    SupervisedModule, Supervisor, SupervisorHandle, SupervisorProcessLiveness, SwapFailureArm,
    SwapRefusal, DEFAULT_DRAIN_TIMEOUT, DEFAULT_SWAP_READY_TIMEOUT, SPAWN_ROLE_SWAP_CANDIDATE,
    SUBC_ARG, SUBC_SPAWN_ROLE_ENV,
};
pub use watchdog::{
    DaemonSelfWatchdog, DaemonSelfWatchdogConfig, WatchdogStage, WatchdogTickError,
    DEFAULT_SELF_WATCHDOG_DEADLINE, DEFAULT_SELF_WATCHDOG_INTERVAL,
};
