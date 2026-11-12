//! Which connections may use a module's registration operations.
//!
//! The operations below need "the module's registration connection". For a
//! supervised spawn that is its registration channel, and only while the spawn
//! is alive. Which of a module's live spawns may use each operation depends on
//! the spawn's role:
//!
//! | Operation | Current | Candidate | Draining |
//! |---|---|---|---|
//! | HELLO | once | once | no |
//! | `config.get`, `config.applied` | yes | yes | no |
//! | `catalog.update`, `activity.record` | yes | yes | yes |
//! | `claim.mint` | yes | yes | yes |
//! | `operator.confirm` | yes | yes | yes |
//! | `scope.sync`, `scope.apply` | yes | no | no |
//! | `scope.describe` | yes | yes | yes |
//! | `route.authorize` | yes | no | no |
//! | `supervisor.live_roots`, `scope.served_by` | yes | yes | yes |
//! | `supervisor.agent_commands` | yes | no | no |
//!
//! "Current" is the spawn serving the module, "candidate" a swap candidate
//! before cutover, and "draining" the incumbent after cutover until its drain
//! ends. The last two rows keep the requirement each operation had before
//! registration channels existed: any registration of the module for the
//! first, the module's current launch for the second. Each handler enforces
//! its own role rule: config delivery accepts only the active or candidate
//! target, `route.authorize` reads only active registrations, and scope sync
//! authority and agent command declarations go only to the current launch
//! ([`ControlHandler::connection_is_current_launch`]).
//!
//! This module adds the two rules every row shares. A connection bound by a
//! consumer claim (`self`, `helper` or `relay`) is never a registration, and a
//! registration channel whose spawn has been seen to exit carries no
//! authority, even while its registry entry still awaits cleanup. Connections
//! that registered over the launch nonce, or that no supervisor spawned, keep
//! the rules they had before.

use super::*;
use crate::scopes::LaunchStanding;
use crate::supervise::RegistrationSpawn;
pub(crate) use crate::supervise::REGISTRATION_CHANNEL_CLOSED;
use subc_control::ClaimPurpose;

/// The refusal for an operation a claim-bound connection may not use: any
/// registration operation on a `self`, `helper` or `relay` connection, and
/// anything outside a prefix registration's brokered set. Terminal.
pub(crate) const CLAIM_OPERATION_NOT_ALLOWED: &str = "claim_operation_not_allowed";

/// The operations that require the module's own registration.
pub(super) const REGISTRATION_OPERATIONS: &[&str] = &[
    "config.declare",
    "config.get",
    "config.applied",
    MODULE_TO_SUBC_OP_CATALOG_UPDATE,
    ops::ACTIVITY_RECORD,
    "claim.mint",
    "operator.confirm",
    SCOPE_SYNC_OP,
    SCOPE_APPLY_OP,
    SCOPE_DESCRIBE_OP,
    subc_protocol::scope::ROUTE_AUTHORIZE_OP,
    "supervisor.live_roots",
    "supervisor.agent_commands",
    subc_control::SCOPE_SERVED_BY_OP,
];

/// Whose authority an activity read carries, taken from the connection
/// itself before any request field is read.
pub(super) enum ActivityReader {
    /// Not a supervised registration channel or a claim-bound connection:
    /// the nonce-era rules apply, including `consumer_identity`.
    Legacy,
    /// A live registration channel or `self`-claim connection of this module,
    /// or a prefix registration. The spawn is carried so a subscription can
    /// end when the spawn's authority does; a prefix registration's spawn is
    /// its owner's.
    Module {
        module_id: String,
        spawn: Arc<RegistrationSpawn>,
    },
    /// Refused whatever the request says: a `helper` or `relay` connection,
    /// which acts for someone other than the module, or a connection whose
    /// spawn has died.
    Refused,
}

impl ControlHandler {
    /// Refuse a registration operation from a consumer claim connection or a
    /// dead spawn's registration channel. `claim.mint` is dispatched before
    /// this check and applies the same rules with its own refusal code.
    pub(super) fn registration_operation_refusal(
        &self,
        ctx: &RouteCtx,
        frame: &Frame,
        op: Option<&str>,
    ) -> Result<Option<Frame>, RouterError> {
        if !op.is_some_and(|op| REGISTRATION_OPERATIONS.contains(&op)) {
            return Ok(None);
        }
        let closed = || {
            control_error_frame(
                frame,
                REGISTRATION_CHANNEL_CLOSED,
                "the spawn that owned this registration has exited",
            )
            .map(Some)
        };
        if let Some(binding) = self.claims.binding(ctx.connection_id) {
            // Retirement marks the spawn dead before it fences the claim
            // connections, so a frame read in between still arrives here. A
            // prefix registration lives on its owner's spawn and dies with it.
            if !binding.registration.launch_ready() {
                return closed();
            }
            // A prefix-bound connection becomes the prefix child's
            // registration; ingress already limits it to brokered operations.
            if binding.prefix_module_id().is_some() {
                return Ok(None);
            }
            return control_error_frame(
                frame,
                CLAIM_OPERATION_NOT_ALLOWED,
                "a connection bound by a consumer claim has no registration operations",
            )
            .map(Some);
        }
        if self
            .supervisor
            .registration_spawn(ctx.connection_id)
            .is_some_and(|spawn| !spawn.launch_ready())
        {
            return closed();
        }
        Ok(None)
    }

    /// Whether `connection` belongs to `owner`'s current supervised launch, for
    /// scope sync authority and agent command declarations. A registration
    /// channel is current only while its spawn is the module's live serving
    /// spawn: a swap candidate is not current before cutover, and the incumbent
    /// is not current after it, even though both are alive. Any other
    /// connection is current when it presented the module's recorded launch
    /// nonce at HELLO.
    ///
    /// The registry slot decides a channel's role; the supervisor's launch
    /// record then confirms the spawn is that launch's process. When the record
    /// cannot confirm it, because it is briefly locked or because a cutover has
    /// promoted the registration and not yet rewritten the record, a live spawn
    /// the registry names as serving is [`LaunchStanding::Unsettled`], which
    /// callers refuse with a retryable code. A record showing the launch
    /// stopped or exited makes it not current: retrying would never succeed.
    ///
    /// This is an admission check. Scope sync evaluates it and publishes under
    /// one hold of the scope table's write lock, so a change admitted for the
    /// incumbent just before cutover can still publish just after it, as with
    /// nonces, but it cannot overtake a sync the promoted spawn has completed.
    /// Agent declarations repeat the check at commit, under the supervisor's
    /// launch locks.
    ///
    /// Scope sync calls this under the scope table's write lock. It takes the
    /// registry, the supervisor's channel index, roster and launch records, the
    /// gate runtime's admission state (to tell whether the module is gated),
    /// and the HELLO nonce map, each released before it returns; none of them
    /// is held while the scope table is taken.
    pub(super) fn connection_is_current_launch(
        &self,
        owner: &str,
        connection: ConnectionId,
        current_nonce: Option<&str>,
    ) -> LaunchStanding {
        if let Some(spawn) = self.supervisor.registration_spawn(connection) {
            if self.supervisor.registration_snapshot_is_current_live(
                &self.registry,
                owner,
                connection,
                None,
            ) {
                return LaunchStanding::Current;
            }
            let serving = spawn.module_id == owner
                && spawn.launch_ready()
                && (!self.supervisor.module_is_gated(owner) || spawn.has_program_binding())
                && self
                    .registry
                    .get_module(owner)
                    .ok()
                    .flatten()
                    .is_some_and(|registration| registration.connection_id == connection);
            if !serving {
                return LaunchStanding::NotCurrent;
            }
            let Some(module) = self.supervisor.get(owner) else {
                return LaunchStanding::NotCurrent;
            };
            return match module.resource_launch() {
                // The launch record is locked: nothing is known yet.
                None => LaunchStanding::Unsettled,
                // A stopping or exited launch will never be current again,
                // even before its spawn's retirement is observed.
                Some(launch) if !launch.enabled || !launch.alive => LaunchStanding::NotCurrent,
                // The record names another process, as during a cutover that
                // has promoted the registry but not rewritten the record, or it
                // lacks this process's start time.
                Some(_) => LaunchStanding::Unsettled,
            };
        }
        self.hello_launch_nonces
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .presented(connection, current_nonce)
            .into()
    }

    /// Whether a registration channel proves the module for
    /// `operator.confirm`, as the launch nonce does for older SDKs. Every live
    /// role proves it: current, candidate and draining. `None` when the
    /// connection is not a registration channel.
    pub(super) fn confirm_channel_proven(
        &self,
        connection: ConnectionId,
        module_id: &str,
    ) -> Option<bool> {
        self.supervisor
            .registration_spawn(connection)
            .map(|spawn| spawn.launch_ready() && spawn.module_id == module_id)
    }

    /// Resolve an activity reader from the connection's binding. A request
    /// field such as `consumer_identity` is consulted only for
    /// [`ActivityReader::Legacy`] connections.
    pub(super) fn activity_reader(&self, connection: ConnectionId) -> ActivityReader {
        if let Some(binding) = self.claims.binding(connection) {
            if !binding.registration.launch_ready() {
                return ActivityReader::Refused;
            }
            return match &binding.purpose {
                ClaimPurpose::SelfConnection => ActivityReader::Module {
                    module_id: binding.registration.module_id.clone(),
                    spawn: binding.registration.clone(),
                },
                ClaimPurpose::Prefix { .. } => match self
                    .forwarding
                    .module_id_for_connection(connection)
                    .ok()
                    .flatten()
                {
                    Some(module_id) => ActivityReader::Module {
                        module_id,
                        spawn: binding.registration.clone(),
                    },
                    None => ActivityReader::Refused,
                },
                // `helper`, `relay`, and any purpose added later, read nothing
                // until a rule for it exists.
                _ => ActivityReader::Refused,
            };
        }
        match self.supervisor.registration_spawn(connection) {
            Some(spawn) if spawn.launch_ready() => ActivityReader::Module {
                module_id: spawn.module_id.clone(),
                spawn,
            },
            Some(_) => ActivityReader::Refused,
            None => ActivityReader::Legacy,
        }
    }
}
