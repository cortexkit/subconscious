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
use crate::supervise::RegistrationSpawn;
use subc_control::ClaimPurpose;

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
        if let Some(binding) = self.claims.binding(ctx.connection_id) {
            // A prefix-bound connection becomes the prefix child's
            // registration; ingress already limits it to brokered operations.
            if binding.prefix_module_id().is_some() {
                return Ok(None);
            }
            return control_error_frame(
                frame,
                "claim_operation_not_allowed",
                "a connection bound by a consumer claim has no registration operations",
            )
            .map(Some);
        }
        if self
            .supervisor
            .registration_spawn(ctx.connection_id)
            .is_some_and(|spawn| !spawn.launch_ready())
        {
            return control_error_frame(
                frame,
                "registration_channel_closed",
                "the spawn that owned this registration channel has exited",
            )
            .map(Some);
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
    /// Scope sync calls this under the scope table's write lock. It takes the
    /// registry, the supervisor's channel index and launch records, and the
    /// HELLO nonce map, each released before it returns; none of them is held
    /// while the scope table is taken.
    pub(super) fn connection_is_current_launch(
        &self,
        owner: &str,
        connection: ConnectionId,
        current_nonce: Option<&str>,
    ) -> bool {
        if self.supervisor.registration_spawn(connection).is_some() {
            return self.supervisor.registration_snapshot_is_current_live(
                &self.registry,
                owner,
                connection,
                None,
            );
        }
        self.hello_launch_nonces
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .presented(connection, current_nonce)
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
