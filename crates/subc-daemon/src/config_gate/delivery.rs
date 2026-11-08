//! Connection-bound delivery attempts; durable vectors remain in ConfigStore.

use std::collections::HashSet;
use subc_protocol::config::{ConfigAppliedRequest, ConfigGetReply, ConfigGetRequest};

use super::*;
use crate::config_gate::apply::Transition;
use crate::config_store::Tuple;

pub(crate) const CONFIG_LIVE_ACK_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const CONFIG_WARMUP_SLACK: Duration = Duration::from_secs(10);

#[derive(Clone, Debug)]
struct Attempt {
    module: String,
    generation: u64,
    id: u64,
    transition: Transition,
    ready: bool,
    acknowledged: bool,
    delivered: bool,
    deadline: Option<tokio::time::Instant>,
    // Catch-up belongs to one promoted child, not to the durable generation.
    // A daemon restart drops it: every fresh launch receives the active vector.
    catchup: bool,
}

#[derive(Clone, Debug)]
pub struct DeliverySpawn {
    module: String,
    generation: u64,
    deadline: tokio::time::Instant,
}

#[derive(Default)]
pub(super) struct DeliveryState {
    next_attempt: u64,
    attempts: HashMap<ConnectionId, Attempt>,
    served: HashMap<ConnectionId, (Option<u64>, Option<u64>)>,
    stamp_running: HashSet<ConnectionId>,
    initial_generation: u64,
    children: HashMap<ConnectionId, DeliverySpawn>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AppliedDisposition {
    Ignored,
    WaitingForReadiness,
    Active,
    CatchupComplete,
    Failed {
        generation: u64,
        restore: Option<u64>,
        keep_running: bool,
    },
    CatchupFailed {
        active: u64,
    },
}

impl DeliveryState {
    pub(super) fn new(initial_generation: u64) -> Self {
        Self {
            initial_generation,
            ..Self::default()
        }
    }

    pub(super) fn disconnected(&mut self, connection: ConnectionId) {
        self.attempts.remove(&connection);
        self.served.remove(&connection);
        self.stamp_running.remove(&connection);
        self.children.remove(&connection);
    }

    pub(super) fn remember_stamp_connections(&mut self, connections: Vec<ConnectionId>) {
        self.stamp_running.extend(connections);
    }

    fn boundary_open(
        &self,
        connection: ConnectionId,
        generation: u64,
        transition: Transition,
    ) -> bool {
        if transition == Transition::DaemonRestart && generation > self.initial_generation {
            return false;
        }
        !transition.requires_spawn()
            || self
                .children
                .get(&connection)
                .is_some_and(|spawn| spawn.generation == generation)
    }

    fn allocate(
        &mut self,
        module: &str,
        generation: u64,
        transition: Transition,
    ) -> Result<Attempt> {
        self.next_attempt = self.next_attempt.checked_add(1).ok_or_else(|| {
            GateError::new(
                "delivery_attempt_exhausted",
                "delivery attempt ids exhausted",
            )
        })?;
        Ok(Attempt {
            module: module.into(),
            generation,
            id: self.next_attempt,
            transition,
            ready: transition == Transition::Live,
            acknowledged: false,
            delivered: false,
            deadline: None,
            catchup: false,
        })
    }
}

pub(super) fn bind_get(
    state: &mut OwnerState,
    connection: ConnectionId,
    module: &str,
    request: &ConfigGetRequest,
    mut reply: ConfigGetReply,
    transition: Transition,
) -> Result<ConfigGetReply> {
    if reply.pending.as_ref().is_some_and(|view| {
        !state
            .delivery
            .boundary_open(connection, view.generation, transition)
    }) {
        reply.pending = None;
    }
    if reply.active.is_none() && reply.pending.is_none() {
        return Err(GateError::new(
            "config_gated",
            "generation waits for its required supervision or daemon boundary",
        ));
    }
    match (request.generation, request.attempt) {
        (None, None) => {}
        (Some(generation), Some(attempt))
            if state
                .delivery
                .attempts
                .get(&connection)
                .is_some_and(|bound| {
                    bound.module == module && bound.generation == generation && bound.id == attempt
                }) => {}
        _ => {
            return Err(GateError::new(
                "config_gated",
                "requested generation and attempt are not assigned to this connection",
            ))
        }
    }
    if reply.pending.is_none()
        && state
            .delivery
            .attempts
            .get(&connection)
            .is_some_and(|bound| {
                bound.catchup
                    && reply
                        .active
                        .as_ref()
                        .is_some_and(|view| view.generation == bound.generation)
            })
    {
        reply.pending = reply.active.clone();
        if let Some(view) = &mut reply.pending {
            view.status = ConfigStatus::Pending;
        }
    }
    if let Some(pending) = &mut reply.pending {
        let matches = state
            .delivery
            .attempts
            .get(&connection)
            .is_some_and(|bound| bound.module == module && bound.generation == pending.generation);
        if !matches {
            let mut attempt = state
                .delivery
                .allocate(module, pending.generation, transition)?;
            if transition.requires_spawn() {
                attempt.deadline = state
                    .delivery
                    .children
                    .get(&connection)
                    .filter(|warmup| warmup.generation == pending.generation)
                    .map(|warmup| warmup.deadline);
            }
            state.delivery.attempts.insert(connection, attempt);
        }
        pending.attempt = state
            .delivery
            .attempts
            .get(&connection)
            .map(|bound| bound.id);
    } else {
        state.delivery.attempts.remove(&connection);
    }
    if request
        .generation
        .is_some_and(|id| reply.pending.as_ref().map(|view| view.generation) != Some(id))
    {
        return Err(GateError::new(
            "config_gated",
            "selected delivery has been superseded",
        ));
    }
    Ok(reply)
}

impl RuntimeOwner {
    /// Capture the common spawn instant, not the later HELLO instant, so a slow
    /// registration cannot extend the approved generation's warm-up deadline.
    pub(crate) fn delivery_spawned(
        &self,
        module: &str,
        spawned_at: tokio::time::Instant,
        register_timeout: Duration,
        first_health_budget: Duration,
    ) -> Result<Option<DeliverySpawn>> {
        let mut state = self.lock()?;
        let generation = state
            .store
            .body()?
            .versions
            .iter()
            .find(|(tuple, _)| tuple.module == module && tuple.project.is_none())
            .and_then(|(_, versions)| versions.pending);
        if let Some(generation) = generation {
            let deadline =
                spawned_at + register_timeout + first_health_budget + CONFIG_WARMUP_SLACK;
            Ok(Some(DeliverySpawn {
                module: module.into(),
                generation,
                deadline,
            }))
        } else {
            Ok(None)
        }
    }

    pub(crate) fn delivery_child_registered(
        &self,
        connection: ConnectionId,
        spawn: &DeliverySpawn,
    ) -> Result<()> {
        let mut state = self.lock()?;
        state.delivery.children.insert(connection, spawn.clone());
        if let Some(bound) = state
            .delivery
            .attempts
            .get_mut(&connection)
            .filter(|bound| bound.module == spawn.module && bound.generation == spawn.generation)
        {
            if bound.transition.requires_spawn() {
                bound.deadline = Some(spawn.deadline);
            }
        }
        Ok(())
    }

    pub(crate) fn module_view_delivered(
        &self,
        connection: ConnectionId,
        module: &str,
        reply: &ConfigGetReply,
        written_at: tokio::time::Instant,
        active: bool,
    ) -> Result<()> {
        {
            let mut state = self.lock()?;
            if !state
                .declarations
                .get(&connection)
                .is_some_and(|(id, _)| id == module)
            {
                return Err(GateError::new(
                    "not_delivery_target",
                    "configuration connection has ended",
                ));
            }
            state.delivery.served.insert(
                connection,
                (
                    reply.active.as_ref().map(|view| view.generation),
                    reply.pending.as_ref().map(|view| view.generation),
                ),
            );
        }
        if active {
            if let Some(view) = &reply.pending {
                if let Some(attempt) = view.attempt {
                    self.delivery_written(connection, view.generation, attempt, written_at)?;
                }
            }
        }
        Ok(())
    }
    /// Completion is supplied by the supervisor after registration, the first
    /// healthy probe and any required route teardown, never by a module body.
    pub(crate) fn delivery_readiness_complete(
        &self,
        connection: ConnectionId,
        module: &str,
        generation: u64,
    ) -> Result<Option<AppliedDisposition>> {
        let attempt = {
            let mut state = self.lock()?;
            if state.active_declarations.get(module) != Some(&connection) {
                return Err(GateError::new(
                    "not_delivery_target",
                    "readiness belongs to another connection",
                ));
            }
            let started_for_generation = state
                .delivery
                .children
                .get(&connection)
                .is_some_and(|warmup| warmup.generation == generation);
            let Some(bound) = state
                .delivery
                .attempts
                .get_mut(&connection)
                .filter(|bound| bound.module == module && bound.generation == generation)
            else {
                return Ok(None);
            };
            if bound.transition.requires_spawn() && !started_for_generation {
                return Ok(None);
            }
            bound.ready = true;
            bound.acknowledged.then_some(bound.id)
        };
        attempt
            .map(|attempt| {
                self.module_applied(
                    connection,
                    module,
                    ConfigAppliedRequest::new(
                        generation,
                        attempt,
                        subc_protocol::config::ConfigApplyResult::Ack,
                        vec![],
                    ),
                )
            })
            .transpose()
    }

    /// Both forwarding and registry promotion happen under the caller's module
    /// gate before this connection becomes an acknowledgement target.
    pub(crate) fn delivery_promoted(
        &self,
        connection: ConnectionId,
        module: &str,
        cutover_at: tokio::time::Instant,
    ) -> Result<Option<(u64, u64)>> {
        let mut state = self.lock()?;
        state.store.delivery_ready()?;
        let Some((id, declaration)) = state
            .declarations
            .get(&connection)
            .cloned()
            .filter(|(id, _)| id == module)
        else {
            return Err(GateError::new(
                "config_gated",
                "candidate has not declared its approved configuration",
            ));
        };
        let tuple = Tuple {
            module: module.into(),
            project: None,
        };
        let body = state.store.body()?;
        let versions = body
            .versions
            .iter()
            .find(|(key, _)| key == &tuple)
            .map(|(_, versions)| versions.clone())
            .unwrap_or_default();
        let expected = versions
            .pending
            .or(versions.active)
            .and_then(|id| body.generations.get(&id))
            .and_then(|entry| entry.delivery.as_ref())
            .and_then(|snapshot| snapshot.declarations.get(module));
        if expected != Some(&declaration) {
            return Err(GateError::new(
                "config_gated",
                "candidate declaration differs from the approved declaration",
            ));
        }
        let transition = versions
            .pending
            .and_then(|id| body.generations.get(&id))
            .map(|entry| Transition::for_module(&entry.generation.changeset, module))
            .transpose()?
            .unwrap_or(Transition::Live);
        let vector = (versions.active, versions.pending);
        let catchup = versions.pending.is_none()
            && versions.active.is_some()
            && state.delivery.served.get(&connection) != Some(&vector);
        state.active_declarations.insert(id, connection);
        state.delivery.attempts.remove(&connection);
        let Some(generation) = versions
            .pending
            .or_else(|| catchup.then_some(versions.active).flatten())
        else {
            return Ok(None);
        };
        let mut attempt = state.delivery.allocate(module, generation, transition)?;
        attempt.catchup = catchup;
        // Swap readiness completed before promotion. Its new target now gets
        // a full live ACK window, independently of the finished spawn budget.
        attempt.ready = true;
        attempt.deadline = Some(cutover_at + CONFIG_LIVE_ACK_TIMEOUT);
        let assignment = (generation, attempt.id);
        state.delivery.attempts.insert(connection, attempt);
        Ok(Some(assignment))
    }

    /// The control adapter calls this only for the active target while holding
    /// its module gate, after the delivery frame's writer flush succeeds.
    pub(crate) fn delivery_written(
        &self,
        connection: ConnectionId,
        generation: u64,
        attempt: u64,
        written_at: tokio::time::Instant,
    ) -> Result<()> {
        let mut state = self.lock()?;
        if let Some(bound) = state.delivery.attempts.get_mut(&connection) {
            if bound.generation == generation && bound.id == attempt {
                bound.delivered = true;
                if !bound.transition.requires_spawn() && bound.deadline.is_none() {
                    bound.deadline = Some(written_at + CONFIG_LIVE_ACK_TIMEOUT);
                }
            }
        }
        Ok(())
    }

    pub(crate) fn module_applied(
        &self,
        connection: ConnectionId,
        module: &str,
        request: ConfigAppliedRequest,
    ) -> Result<AppliedDisposition> {
        let mut state = self.lock()?;
        if !state
            .declarations
            .get(&connection)
            .is_some_and(|(id, _)| id == module)
        {
            return Err(GateError::new(
                "declaration_order",
                "config.declare must precede get or applied on this connection",
            ));
        }
        if request.project_id.is_some() {
            return Err(GateError::new(
                "project_registry_unavailable",
                "project delivery requires the authoritative project registry",
            ));
        }
        if state.active_declarations.get(module) != Some(&connection) {
            return Err(GateError::new(
                "not_delivery_target",
                "connection is not the active delivery target",
            ));
        }
        state.store.check()?;
        let Some(bound) = state
            .delivery
            .attempts
            .get(&connection)
            .cloned()
            .filter(|bound| {
                bound.module == module
                    && bound.generation == request.generation
                    && bound.id == request.attempt
            })
        else {
            return Ok(AppliedDisposition::Ignored);
        };
        if !bound.delivered {
            return Ok(AppliedDisposition::Ignored);
        }
        let tuple = Tuple {
            module: module.into(),
            project: None,
        };
        let versions = state
            .store
            .body()?
            .versions
            .iter()
            .find(|(key, _)| key == &tuple)
            .map(|(_, versions)| versions.clone())
            .unwrap_or_default();
        let ack = request.result.is_ack()
            && !bound
                .deadline
                .is_some_and(|deadline| tokio::time::Instant::now() >= deadline);
        if bound.catchup {
            if versions.pending.is_some() || versions.active != Some(bound.generation) {
                return Ok(AppliedDisposition::Ignored);
            }
            state.delivery.attempts.remove(&connection);
            return Ok(if ack {
                AppliedDisposition::CatchupComplete
            } else {
                AppliedDisposition::CatchupFailed {
                    active: bound.generation,
                }
            });
        }
        if versions.pending != Some(bound.generation) {
            return Ok(AppliedDisposition::Ignored);
        }
        if ack
            && (!bound.ready
                || (bound.transition == Transition::DaemonRestart
                    && bound.generation > state.delivery.initial_generation))
        {
            if let Some(bound) = state.delivery.attempts.get_mut(&connection) {
                bound.acknowledged = true;
            }
            return Ok(AppliedDisposition::WaitingForReadiness);
        }
        let result = state.store.record_delivery(&tuple, bound.generation, ack);
        // A pending-anchor NACK has already restored memory. Discard its attempt
        // even though the durable failure must wait for the anchor retry.
        if result.is_ok() || !ack {
            state.delivery.attempts.remove(&connection);
        }
        result?;
        Ok(if ack {
            AppliedDisposition::Active
        } else {
            AppliedDisposition::Failed {
                generation: bound.generation,
                restore: versions.active,
                keep_running: versions.active.is_none()
                    && state.delivery.stamp_running.contains(&connection),
            }
        })
    }
}
