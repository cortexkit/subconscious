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
    write_deadline: Option<tokio::time::Instant>,
}

#[derive(Clone, Debug)]
pub struct DeliverySpawn {
    id: u64,
    module: String,
    generation: u64,
    deadline: tokio::time::Instant,
}

#[derive(Default)]
pub(super) struct DeliveryState {
    next_spawn: u64,
    next_attempt: u64,
    attempts: HashMap<ConnectionId, Attempt>,
    served: HashMap<ConnectionId, (Option<u64>, Option<u64>)>,
    stamp_running: HashSet<ConnectionId>,
    initial_generation: u64,
    children: HashMap<ConnectionId, DeliverySpawn>,
    healthy_children: HashSet<(ConnectionId, u64)>,
    unregistered_children: HashMap<(String, bool), DeliverySpawn>,
    recoveries: Vec<(ConnectionId, String, AppliedDisposition)>,
    exited: HashMap<String, ExitedDelivery>,
    // Transport cleanup may precede the process-exit observation. Retain the
    // last active child's assigned generation, not an unattempted deferred one.
    last_target_attempt: HashMap<String, (u64, u64)>,
}

#[derive(Clone)]
pub(crate) struct ExitedDelivery {
    pub module: String,
    spawn: u64,
    generation: u64,
    changed: tokio::sync::watch::Sender<bool>,
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

#[derive(Clone, Copy)]
pub(crate) struct DeliveryWindow {
    pub generation: u64,
    pub attempt: u64,
    pub delivered: bool,
    pub deadline: Option<tokio::time::Instant>,
}

impl DeliveryState {
    pub(super) fn declared(&mut self, connection: ConnectionId, module: &str, active: bool) {
        if let Some(spawn) = self.unregistered_children.remove(&(module.into(), active)) {
            self.children.insert(connection, spawn);
        }
    }
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
        self.healthy_children
            .retain(|(child, _)| *child != connection);
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
            write_deadline: None,
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
                attempt.ready = state
                    .delivery
                    .healthy_children
                    .contains(&(connection, pending.generation));
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
        if state.active_declarations.get(module) == Some(&connection) {
            if let Some(spawn) = state.delivery.children.get(&connection) {
                let id = spawn.id;
                state
                    .delivery
                    .last_target_attempt
                    .insert(module.into(), (id, pending.generation));
            }
        }
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
    /// A supervised process exit, not a transport disconnect, fails warm-up.
    /// The synchronous fence closes the gap before the async failure commit.
    pub(crate) fn delivery_child_exited(&self, spawn: &DeliverySpawn) -> Result<()> {
        let mut state = self.lock()?;
        let pending = state
            .store
            .body()?
            .versions
            .iter()
            .find(|(tuple, _)| tuple.module == spawn.module && tuple.project.is_none())
            .and_then(|(_, versions)| versions.pending);
        if let Some(generation) = pending.filter(|generation| {
            *generation == spawn.generation
                || state.delivery.last_target_attempt.get(&spawn.module)
                    == Some(&(spawn.id, *generation))
        }) {
            state
                .delivery
                .exited
                .entry(spawn.module.clone())
                .or_insert_with(|| ExitedDelivery {
                    module: spawn.module.clone(),
                    spawn: spawn.id,
                    generation,
                    changed: tokio::sync::watch::channel(true).0,
                });
        }
        self.notify_delivery_changed();
        Ok(())
    }

    pub(crate) fn delivery_exit_fence(
        &self,
        module: &str,
    ) -> Result<Option<tokio::sync::watch::Receiver<bool>>> {
        Ok(self
            .lock()?
            .delivery
            .exited
            .get(module)
            .map(|entry| entry.changed.subscribe()))
    }

    pub(crate) fn delivery_exits(&self) -> Result<Vec<ExitedDelivery>> {
        Ok(self.lock()?.delivery.exited.values().cloned().collect())
    }

    /// The delivery worker holds this module's asynchronous configuration lock
    /// while recording the failure. Respawn stays blocked until the failure has
    /// restored the previous active generation or, if none exists, the supervisor
    /// has canceled respawn and stopped the module.
    /// `AnchorPending` still restores the previous active generation in memory;
    /// its durable failure record is retried before delivery can resume.
    pub(crate) fn delivery_record_exit(&self, exited: &ExitedDelivery) -> Result<bool> {
        let mut state = self.lock()?;
        if !state
            .delivery
            .exited
            .get(&exited.module)
            .is_some_and(|entry| entry.spawn == exited.spawn)
        {
            return Ok(false);
        }
        let tuple = Tuple {
            module: exited.module.clone(),
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
        if versions
            .pending
            .is_some_and(|generation| generation != exited.generation)
        {
            return Ok(false);
        }
        if versions.pending == Some(exited.generation) {
            let result = state
                .store
                .record_delivery(&tuple, exited.generation, false);
            if let Err(error) = result {
                if error.code != crate::config_store::StoreErrorCode::AnchorPending {
                    return Err(error.into());
                }
            }
            state.delivery.attempts.retain(|_, attempt| {
                attempt.module != exited.module || attempt.generation != exited.generation
            });
        }
        Ok(versions.active.is_none())
    }

    pub(crate) fn delivery_release_exit(&self, exited: &ExitedDelivery) -> Result<()> {
        let mut state = self.lock()?;
        if state
            .delivery
            .exited
            .get(&exited.module)
            .is_some_and(|entry| entry.spawn == exited.spawn)
        {
            if let Some(entry) = state.delivery.exited.remove(&exited.module) {
                entry.changed.send_replace(false);
            }
        }
        self.notify_delivery_changed();
        Ok(())
    }
    pub(crate) fn delivery_abandon_spawn(&self, spawn: &DeliverySpawn, active: bool) {
        if let Ok(mut state) = self.lock() {
            let key = (spawn.module.clone(), active);
            if state
                .delivery
                .unregistered_children
                .get(&key)
                .is_some_and(|current| current.id == spawn.id)
            {
                state.delivery.unregistered_children.remove(&key);
            }
        }
    }
    pub(crate) fn delivery_spawned_in_slot(
        &self,
        module: &str,
        at: tokio::time::Instant,
        register: Duration,
        health: Duration,
        active: bool,
    ) -> Result<Option<DeliverySpawn>> {
        let spawn = self.delivery_spawned(module, at, register, health)?;
        if let Some(spawn) = &spawn {
            self.lock()?
                .delivery
                .unregistered_children
                .insert((module.into(), active), spawn.clone());
        }
        Ok(spawn)
    }
    pub(crate) fn delivery_take_recoveries(
        &self,
    ) -> Result<Vec<(ConnectionId, String, AppliedDisposition)>> {
        Ok(std::mem::take(&mut self.lock()?.delivery.recoveries))
    }

    pub(crate) fn delivery_window(
        &self,
        connection: ConnectionId,
    ) -> Result<Option<DeliveryWindow>> {
        Ok(self
            .lock()?
            .delivery
            .attempts
            .get(&connection)
            .map(|bound| DeliveryWindow {
                generation: bound.generation,
                attempt: bound.id,
                delivered: bound.delivered,
                deadline: bound.deadline,
            }))
    }

    pub(crate) fn delivery_write_started(
        &self,
        connection: ConnectionId,
        generation: u64,
        attempt: u64,
        now: tokio::time::Instant,
    ) -> Result<()> {
        if let Some(bound) = self
            .lock()?
            .delivery
            .attempts
            .get_mut(&connection)
            .filter(|bound| {
                bound.generation == generation && bound.id == attempt && !bound.delivered
            })
        {
            bound
                .write_deadline
                .get_or_insert(now + CONFIG_LIVE_ACK_TIMEOUT);
        }
        Ok(())
    }

    pub(crate) fn delivery_timeout(
        &self,
        connection: ConnectionId,
        module: &str,
        generation: u64,
        attempt: u64,
    ) -> Result<AppliedDisposition> {
        self.module_outcome(
            connection,
            module,
            ConfigAppliedRequest::new(
                generation,
                attempt,
                subc_protocol::config::ConfigApplyResult::Nack,
                vec![],
            ),
            true,
        )
    }
    pub(crate) fn delivery_current_attempt(
        &self,
        connection: ConnectionId,
    ) -> Result<Option<(u64, u64)>> {
        Ok(self
            .lock()?
            .delivery
            .attempts
            .get(&connection)
            .map(|attempt| (attempt.generation, attempt.id)))
    }

    pub(crate) fn delivery_child_generation(
        &self,
        connection: ConnectionId,
    ) -> Result<Option<u64>> {
        Ok(self
            .lock()?
            .delivery
            .children
            .get(&connection)
            .map(|spawn| spawn.generation))
    }
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
            .and_then(|(_, versions)| versions.pending.or(versions.active))
            .unwrap_or(0);
        let deadline = spawned_at + register_timeout + first_health_budget + CONFIG_WARMUP_SLACK;
        state.delivery.next_spawn = state.delivery.next_spawn.checked_add(1).ok_or_else(|| {
            GateError::new(
                "delivery_spawn_exhausted",
                "delivery spawn identifiers exhausted",
            )
        })?;
        Ok(Some(DeliverySpawn {
            id: state.delivery.next_spawn,
            module: module.into(),
            generation,
            deadline,
        }))
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
            if started_for_generation {
                state
                    .delivery
                    .healthy_children
                    .insert((connection, generation));
            }
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
        if state.store.status().state == crate::config_store::StoreState::Unstamped {
            if state
                .declarations
                .get(&connection)
                .is_some_and(|(id, _)| id == module)
            {
                state.active_declarations.insert(module.into(), connection);
            }
            return Ok(None);
        }
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
                if bound
                    .write_deadline
                    .is_some_and(|deadline| written_at >= deadline)
                {
                    return Err(GateError::new(
                        "delivery_expired",
                        "configuration delivery timed out",
                    ));
                }
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
        self.module_outcome(connection, module, request, false)
    }

    fn module_outcome(
        &self,
        connection: ConnectionId,
        module: &str,
        request: ConfigAppliedRequest,
        timed_out: bool,
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
        if !bound.delivered && !timed_out {
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
            if !ack {
                state.delivery.recoveries.push((
                    connection,
                    module.into(),
                    AppliedDisposition::CatchupFailed {
                        active: bound.generation,
                    },
                ));
            }
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
        if !ack {
            let keep_running =
                versions.active.is_none() && state.delivery.stamp_running.contains(&connection);
            state.delivery.recoveries.push((
                connection,
                module.into(),
                AppliedDisposition::Failed {
                    generation: bound.generation,
                    restore: versions.active,
                    keep_running,
                },
            ));
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
