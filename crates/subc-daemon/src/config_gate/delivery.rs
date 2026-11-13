//! Connection-bound delivery attempts; durable vectors remain in ConfigStore.

use std::collections::HashSet;
use subc_protocol::config::{ConfigAppliedRequest, ConfigGetReply, ConfigGetRequest};

use super::*;
use crate::config_gate::apply::Transition;
use crate::config_store::Tuple;

pub(crate) const CONFIG_LIVE_ACK_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const CONFIG_WARMUP_SLACK: Duration = Duration::from_secs(10);

fn allocate_spawn(
    state: &mut OwnerState,
    module: &str,
    generation: u64,
    at: tokio::time::Instant,
    register: Duration,
    health: Duration,
    authority: Option<(
        std::sync::Arc<crate::spawn_authority::SpawnAuthorityRecord>,
        bool,
    )>,
) -> Result<DeliverySpawn> {
    #[cfg(test)]
    if state.delivery.failed_spawn_module.as_deref() == Some(module) {
        return Err(GateError::new(
            "delivery_spawn_exhausted",
            "delivery spawn identifiers exhausted",
        ));
    }
    state.delivery.next_spawn = state.delivery.next_spawn.checked_add(1).ok_or_else(|| {
        GateError::new(
            "delivery_spawn_exhausted",
            "delivery spawn identifiers exhausted",
        )
    })?;
    let (authority, stamped) =
        authority.map_or((None, false), |(record, stamped)| (Some(record), stamped));
    Ok(DeliverySpawn {
        id: state.delivery.next_spawn,
        module: module.into(),
        generation,
        deadline: at + register + health + CONFIG_WARMUP_SLACK,
        authority,
        stamped,
    })
}

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
    policy_probe_requested: bool,
    /// The spawn this attempt was assigned to, when it carries an authority
    /// record. Its acknowledgement counts toward that spawn's first-party
    /// mark only, never another spawn's.
    spawn: Option<DeliverySpawn>,
    /// A catch-up assigned only so a fresh spawn acknowledges the active
    /// generation for its first-party mark. Its refusal or timeout leaves the
    /// spawn unmarked and starts no recovery.
    mark_only: bool,
}

#[derive(Clone, Debug)]
pub struct DeliverySpawn {
    id: u64,
    module: String,
    generation: u64,
    deadline: tokio::time::Instant,
    /// The launch's spawn authority record, when the supervisor launched it.
    authority: Option<std::sync::Arc<crate::spawn_authority::SpawnAuthorityRecord>>,
    /// The stamped store gated this module when the spawn was admitted. Only
    /// such a spawn can ever carry the first-party mark.
    stamped: bool,
}

impl DeliverySpawn {
    pub(crate) fn module(&self) -> &str {
        &self.module
    }
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }
    pub(crate) fn authority(
        &self,
    ) -> Option<&std::sync::Arc<crate::spawn_authority::SpawnAuthorityRecord>> {
        self.authority.as_ref()
    }
    pub(crate) fn stamped(&self) -> bool {
        self.stamped
    }
    /// Attach a launch's authority record, as the supervisor's admission path
    /// does, to a spawn a test allocated directly.
    #[cfg(test)]
    pub(crate) fn with_authority_for_test(
        mut self,
        record: std::sync::Arc<crate::spawn_authority::SpawnAuthorityRecord>,
        stamped: bool,
    ) -> Self {
        self.authority = Some(record);
        self.stamped = stamped;
        self
    }
}

#[derive(Default)]
pub(super) struct DeliveryState {
    #[cfg(test)]
    failed_spawn_module: Option<String>,
    next_spawn: u64,
    next_attempt: u64,
    policy_epochs: HashMap<String, u64>,
    attempts: HashMap<ConnectionId, Attempt>,
    served: HashMap<ConnectionId, (Option<u64>, Option<u64>)>,
    stamp_running: HashSet<ConnectionId>,
    initial_generation: u64,
    children: HashMap<ConnectionId, DeliverySpawn>,
    healthy_children: HashSet<(ConnectionId, u64)>,
    readiness_observed: HashSet<(ConnectionId, u64)>,
    unregistered_children: HashMap<(String, bool), DeliverySpawn>,
    recoveries: Vec<(ConnectionId, String, AppliedDisposition)>,
    exited: HashMap<String, ExitedDelivery>,
    // Transport cleanup may precede the process-exit observation. Retain the
    // last active child's assigned generation, not an unattempted deferred one.
    last_target_attempt: HashMap<String, (u64, u64)>,
    /// Every spawn admitted under the stamp, which carries or may still earn
    /// the first-party mark. Unlike the rest of this state it survives an
    /// authority exit, so a committed unstamp still finds the spawns to lower.
    mark_records: Vec<std::sync::Weak<crate::spawn_authority::SpawnAuthorityRecord>>,
}

impl DeliveryState {
    pub(super) fn reset_for_authority_exit(&mut self, generation: u64) {
        *self = Self {
            next_spawn: self.next_spawn,
            next_attempt: self.next_attempt,
            mark_records: std::mem::take(&mut self.mark_records),
            ..Self::new(generation)
        };
    }
    /// Remember `spawn` for the mark sweep when it was admitted under the
    /// stamp with an authority record.
    pub(super) fn track_mark(&mut self, spawn: &DeliverySpawn) {
        let Some(record) = spawn.authority.as_ref().filter(|_| spawn.stamped) else {
            return;
        };
        if !self
            .mark_records
            .iter()
            .any(|known| std::ptr::eq(known.as_ptr(), std::sync::Arc::as_ptr(record)))
        {
            self.mark_records.push(std::sync::Arc::downgrade(record));
        }
    }
    /// The live spawns remembered for the mark sweep; forgets dead ones.
    pub(super) fn mark_records(
        &mut self,
    ) -> Vec<std::sync::Arc<crate::spawn_authority::SpawnAuthorityRecord>> {
        self.mark_records
            .retain(|record| record.upgrade().is_some_and(|record| record.alive()));
        self.mark_records
            .iter()
            .filter_map(std::sync::Weak::upgrade)
            .collect()
    }
    pub(super) fn started_at_generation(&self) -> u64 {
        self.initial_generation
    }
    pub(super) fn policy_authorization(
        &self,
        module: &str,
        generation: u64,
        connection: ConnectionId,
    ) -> Option<(u64, tokio::time::Instant)> {
        let attempt = self.attempts.get(&connection)?;
        let deadline = attempt.deadline?;
        (attempt.module == module
            && attempt.generation == generation
            && attempt.delivered
            && attempt.acknowledged
            && tokio::time::Instant::now() < deadline)
            .then_some((attempt.id, deadline))
    }
    pub(super) fn fenced_modules(&self) -> impl Iterator<Item = String> + '_ {
        self.exited.keys().cloned()
    }
}

/// Captured before a health RPC; only the same child and installed policy may
/// consume its result. The fields are private so a completion cannot retarget it.
#[derive(Clone, Debug)]
pub struct DeliveryProbe {
    connection: ConnectionId,
    module: String,
    generation: u64,
    attempt: Option<u64>,
    policy_epoch: u64,
    route_readiness: bool,
}

impl DeliveryProbe {
    pub(crate) fn connection(&self) -> ConnectionId {
        self.connection
    }
    pub(crate) fn owns_failure(&self) -> bool {
        self.route_readiness
    }
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
    pub(super) fn invalidate_policy(&mut self, module: &str, generation: u64) {
        for attempt in self
            .attempts
            .values_mut()
            .filter(|attempt| attempt.module == module && attempt.generation == generation)
        {
            attempt.ready = false;
            attempt.policy_probe_requested = false;
        }
    }
    pub(super) fn policy_installed(
        &mut self,
        module: &str,
        generation: u64,
        target: Option<ConnectionId>,
    ) -> Result<()> {
        // The generation may be unchanged after a restore and retry. Use a
        // separate epoch so a probe begun before this installation cannot count.
        let epoch = self.policy_epochs.entry(module.into()).or_default();
        *epoch = epoch.checked_add(1).ok_or_else(|| {
            GateError::new(
                "delivery_probe_exhausted",
                "policy readiness epochs exhausted",
            )
        })?;
        self.invalidate_policy(module, generation);
        let connections: HashSet<_> = self
            .children
            .iter()
            .filter(|(_, spawn)| spawn.module == module)
            .map(|(connection, _)| *connection)
            .chain(
                self.attempts
                    .iter()
                    .filter(|(_, attempt)| attempt.module == module)
                    .map(|(connection, _)| *connection),
            )
            .chain(target)
            .collect();
        self.readiness_observed.retain(|(connection, observed)| {
            *observed != generation || !connections.contains(connection)
        });
        self.healthy_children.retain(|(connection, observed)| {
            *observed != generation || !connections.contains(connection)
        });
        Ok(())
    }
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
        self.readiness_observed
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
            policy_probe_requested: false,
            spawn: None,
            mark_only: false,
        })
    }
}

pub(super) fn visible_reply(
    state: &OwnerState,
    connection: ConnectionId,
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
    Ok(reply)
}

pub(super) fn bind_get(
    state: &mut OwnerState,
    connection: ConnectionId,
    module: &str,
    request: &ConfigGetRequest,
    reply: ConfigGetReply,
    transition: Transition,
) -> Result<ConfigGetReply> {
    let mut reply = visible_reply(state, connection, reply, transition)?;
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
    if reply.pending.is_none() && request.generation.is_none() {
        let generation = reply.active.as_ref().map(|view| view.generation);
        if let Some(attempt) = generation
            .map(|generation| mark_acknowledgement(state, connection, module, generation))
            .transpose()?
            .flatten()
        {
            state.delivery.attempts.insert(connection, attempt);
            reply.pending = reply.active.clone();
            if let Some(view) = &mut reply.pending {
                view.status = ConfigStatus::Pending;
            }
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
            attempt.ready &= super::route_policy::ready(state, module, pending.generation)?;
            attempt.spawn = state.delivery.children.get(&connection).cloned();
            state.delivery.attempts.insert(connection, attempt);
        }
        pending.attempt = state
            .delivery
            .attempts
            .get(&connection)
            .map(|bound| bound.id);
        let mark_only = state
            .delivery
            .attempts
            .get(&connection)
            .is_some_and(|bound| bound.mark_only);
        if state.active_declarations.get(module) == Some(&connection) && !mark_only {
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

/// The catch-up attempt that asks a fresh spawn to acknowledge its module's
/// active generation, so the spawn can earn the first-party mark (the
/// first-party mark design, §2: a spawn is marked only after it acknowledged
/// the active generation and reported ready, on every start path). Delivering
/// the active view at launch is not proof the spawn applied it, and without a
/// pending change a crash, health, manual or boot restart is otherwise never
/// asked for an acknowledgement.
///
/// Assigned only to the active target of a stamped, gated module whose spawn
/// carries an authority record that could still be marked, at most once per
/// active generation; never when a pending change already has an attempt.
/// Unstamped and ungated modules are never asked, so their delivery is
/// unchanged. A refusal or timeout leaves the spawn unmarked and starts no
/// recovery.
fn mark_acknowledgement(
    state: &mut OwnerState,
    connection: ConnectionId,
    module: &str,
    generation: u64,
) -> Result<Option<Attempt>> {
    if state.store.loaded_admission_state().0 != crate::config_store::StoreState::Stamped
        || state.active_declarations.get(module) != Some(&connection)
        || state.delivery.attempts.contains_key(&connection)
    {
        return Ok(None);
    }
    let Some(spawn) = state
        .delivery
        .children
        .get(&connection)
        .filter(|spawn| spawn.stamped && spawn.module == module)
        .cloned()
    else {
        return Ok(None);
    };
    let Some(record) = spawn.authority.as_ref() else {
        return Ok(None);
    };
    let facts = record.mark_facts();
    if !facts.unmarked || !facts.origin_qualifies || !record.request_acknowledgement(generation) {
        return Ok(None);
    }
    let mut attempt = state
        .delivery
        .allocate(module, generation, Transition::Live)?;
    attempt.catchup = true;
    attempt.mark_only = true;
    attempt.spawn = Some(spawn);
    Ok(Some(attempt))
}

impl RuntimeOwner {
    pub(crate) fn policy_probe_due(&self, connection: ConnectionId) -> Result<Option<(u64, u64)>> {
        let mut state = self.lock()?;
        let Some(bound) = state.delivery.attempts.get(&connection) else {
            return Ok(None);
        };
        let (module, generation, attempt) = (bound.module.clone(), bound.generation, bound.id);
        if bound.ready
            || bound.policy_probe_requested
            || state.active_declarations.get(&module) != Some(&connection)
            || !state
                .store
                .body()?
                .generations
                .get(&generation)
                .is_some_and(|entry| {
                    super::route_policy::required(&entry.generation.changeset, &module)
                })
            || !super::route_policy::ready(&mut state, &module, generation)?
        {
            return Ok(None);
        }
        state
            .delivery
            .attempts
            .get_mut(&connection)
            .unwrap()
            .policy_probe_requested = true;
        Ok(Some((generation, attempt)))
    }

    pub(crate) fn policy_probe_pending(
        &self,
        connection: ConnectionId,
        generation: u64,
        attempt: u64,
    ) -> Result<bool> {
        let mut state = self.lock()?;
        let Some(bound) = state.delivery.attempts.get(&connection) else {
            return Ok(false);
        };
        let module = bound.module.clone();
        Ok(bound.generation == generation
            && bound.id == attempt
            && !bound.ready
            && state.active_declarations.get(&module) == Some(&connection)
            && super::route_policy::ready(&mut state, &module, generation)?)
    }
    pub(crate) fn delivery_health_probe(
        &self,
        connection: ConnectionId,
        module: &str,
    ) -> Result<Option<DeliveryProbe>> {
        let mut state = self.lock()?;
        if state.active_declarations.get(module) != Some(&connection) {
            return Ok(None);
        }
        let attempt = state
            .delivery
            .attempts
            .get(&connection)
            .map(|attempt| (attempt.generation, attempt.id));
        let generation = attempt.map(|attempt| attempt.0).or_else(|| {
            state
                .delivery
                .children
                .get(&connection)
                .map(|child| child.generation)
        });
        let route_readiness = match generation {
            Some(generation) if attempt.is_some() => state
                .store
                .body()?
                .generations
                .get(&generation)
                .is_some_and(|entry| {
                    super::route_policy::required(&entry.generation.changeset, module)
                }),
            _ => false,
        };
        Ok(generation.map(|generation| DeliveryProbe {
            connection,
            module: module.into(),
            generation,
            attempt: attempt.map(|attempt| attempt.1),
            route_readiness,
            policy_epoch: state
                .delivery
                .policy_epochs
                .get(module)
                .copied()
                .unwrap_or_default(),
        }))
    }

    pub(crate) fn delivery_health_result(
        &self,
        probe: DeliveryProbe,
        healthy: bool,
    ) -> Result<Option<AppliedDisposition>> {
        let attempt = {
            let mut state = self.lock()?;
            if state.active_declarations.get(&probe.module) != Some(&probe.connection)
                || state
                    .delivery
                    .policy_epochs
                    .get(&probe.module)
                    .copied()
                    .unwrap_or_default()
                    != probe.policy_epoch
            {
                return Ok(None);
            }
            let current = state.delivery.attempts.get(&probe.connection);
            if probe
                .attempt
                .is_some_and(|id| current.is_none_or(|bound| bound.id != id))
                || current.is_some_and(|bound| bound.generation != probe.generation)
            {
                return Ok(None);
            }
            let attempt = current.map(|bound| bound.id);
            if !healthy
                && (!super::route_policy::ready(&mut state, &probe.module, probe.generation)?
                    || !state
                        .store
                        .body()?
                        .generations
                        .get(&probe.generation)
                        .is_some_and(|entry| {
                            super::route_policy::required(
                                &entry.generation.changeset,
                                &probe.module,
                            )
                        }))
            {
                return Ok(None);
            }
            attempt
        };
        if healthy {
            self.delivery_readiness_complete(probe.connection, &probe.module, probe.generation)
        } else if let Some(attempt) = attempt {
            self.module_outcome(
                probe.connection,
                &probe.module,
                ConfigAppliedRequest::new(
                    probe.generation,
                    attempt,
                    subc_protocol::config::ConfigApplyResult::Nack,
                    vec![],
                ),
                true,
            )
            .map(Some)
        } else {
            Ok(None)
        }
    }

    #[cfg(test)]
    pub(super) fn ready(
        &self,
        connection: ConnectionId,
        module: &str,
        generation: u64,
    ) -> Result<Option<AppliedDisposition>> {
        self.delivery_readiness_complete(connection, module, generation)
    }

    pub(crate) fn pending_config_restarts(&self) -> Result<Vec<(String, u64)>> {
        let mut state = self.lock()?;
        state.store.delivery_ready()?;
        let body = state.store.body()?;
        body.versions
            .iter()
            .filter(|(tuple, _)| tuple.project.is_none())
            .filter_map(|(tuple, versions)| {
                let generation = versions.pending?;
                let entry = body.generations.get(&generation)?;
                Some(
                    Transition::for_module(&entry.generation.changeset, &tuple.module).map(
                        |transition| {
                            (transition == Transition::ModuleRestart)
                                .then(|| (tuple.module.clone(), generation))
                        },
                    ),
                )
            })
            .collect::<Result<Vec<_>>>()
            .map(|entries| entries.into_iter().flatten().collect())
    }

    pub(crate) fn requires_fresher_spawn(
        &self,
        module: &str,
        spawn: Option<&DeliverySpawn>,
    ) -> Result<Option<u64>> {
        let mut state = self.lock()?;
        state.store.delivery_ready()?;
        Self::requires_fresher_spawn_loaded(&state, module, spawn)
    }

    pub(super) fn requires_fresher_spawn_loaded(
        state: &OwnerState,
        module: &str,
        spawn: Option<&DeliverySpawn>,
    ) -> Result<Option<u64>> {
        let body = state.store.loaded_admission_state().1;
        let pending = body
            .versions
            .iter()
            .find(|(tuple, _)| tuple.module == module && tuple.project.is_none())
            .and_then(|(_, versions)| versions.pending);
        let Some(generation) = pending else {
            return Ok(None);
        };
        let entry = body
            .generations
            .get(&generation)
            .ok_or_else(|| GateError::new("config_gated", "pending generation is absent"))?;
        let transition = Transition::for_module(&entry.generation.changeset, module)?;
        let deferred_daemon = transition == Transition::DaemonRestart
            && generation > state.delivery.initial_generation;
        Ok((transition.requires_spawn()
            && !deferred_daemon
            && spawn
                .is_none_or(|spawn| spawn.module != module || spawn.generation() != generation))
        .then_some(generation))
    }

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
        let pending = pending.filter(|generation| {
            state
                .store
                .loaded_admission_state()
                .1
                .generations
                .get(generation)
                .and_then(|entry| entry.delivery.as_ref())
                .is_some_and(|delivery| delivery.declarations.contains_key(&spawn.module))
        });
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
        #[cfg(test)]
        self.exit_record_attempts
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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
            state.restore_route_policy(&exited.module, exited.generation);
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

    #[cfg(all(test, unix))]
    pub(crate) fn exit_record_attempts_for_test(&self) -> usize {
        self.exit_record_attempts
            .load(std::sync::atomic::Ordering::SeqCst)
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
                // A prior release may have reached the worker while this fence
                // still blocked admission. Recheck it after the fence is gone;
                // the worker starts only enabled modules with no remaining hold.
                state.admission_releases.push(exited.module.clone());
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
    pub(crate) fn delivery_queue_abandoned_spawn(&self, spawn: DeliverySpawn, active: bool) {
        if let Ok(mut abandoned) = self.abandoned_spawns.lock() {
            abandoned.push((spawn, active));
        }
        self.notify_delivery_changed();
    }
    pub(super) fn drain_abandoned_spawns(&self, state: &mut OwnerState) {
        if let Ok(mut abandoned) = self.abandoned_spawns.lock() {
            for (spawn, active) in abandoned.drain(..) {
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
    }

    /// The full authority check and reservation ran on a blocking worker while
    /// the caller owned the module gate. Recheck only that exact witness here;
    /// contention asks the async caller to wait off the runtime thread.
    pub(crate) fn delivery_launch_current(
        &self,
        spawn: &DeliverySpawn,
        active: bool,
    ) -> Result<Option<bool>> {
        let state = match self.inner.try_lock() {
            Ok(state) => state,
            Err(std::sync::TryLockError::WouldBlock) => return Ok(None),
            Err(_) => {
                return Err(GateError::new(
                    "config_unavailable",
                    "configuration owner lock is poisoned",
                ))
            }
        };
        Ok(Some(
            super::admission::AdmissionCache::from_state(&state)
                .spawn_generation(&spawn.module)
                .is_ok_and(|generation| generation == spawn.generation)
                && state
                    .delivery
                    .unregistered_children
                    .get(&(spawn.module.clone(), active))
                    .is_some_and(|current| current.id == spawn.id)
                && !state.delivery.exited.contains_key(&spawn.module),
        ))
    }
    #[cfg(test)]
    pub(crate) fn pause_spawn_preflight_for_test(&self) {
        let hook = self.spawn_preflight_hook.lock().unwrap().take();
        if let Some(hook) = hook {
            hook();
        }
    }
    #[cfg(all(test, unix))]
    pub(crate) fn exhaust_delivery_spawns_for_test(&self) {
        self.lock().unwrap().delivery.next_spawn = u64::MAX;
    }
    #[cfg(all(test, unix))]
    pub(crate) fn restore_unused_delivery_spawns_for_test(&self) {
        self.lock().unwrap().delivery.next_spawn = 1000;
    }
    #[cfg(all(test, unix))]
    pub(crate) fn fail_delivery_spawns_for_module_for_test(&self, module: &str) {
        self.lock().unwrap().delivery.failed_spawn_module = Some(module.into());
    }
    #[cfg(all(test, unix))]
    pub(crate) fn clear_delivery_spawn_failure_for_test(&self) {
        self.lock().unwrap().delivery.failed_spawn_module = None;
    }
    #[cfg(all(test, unix))]
    pub(crate) fn set_spawn_preflight_hook_for_test(&self, hook: impl FnOnce() + Send + 'static) {
        *self.spawn_preflight_hook.lock().unwrap() = Some(Box::new(hook));
    }
    #[cfg(all(test, unix))]
    pub(crate) fn unregistered_spawn_count_for_test(&self) -> usize {
        self.lock().unwrap().delivery.unregistered_children.len()
    }
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn delivery_spawned_for_admission_in_slot(
        &self,
        module: &str,
        admission: &super::admission::SpawnAdmission,
        at: tokio::time::Instant,
        register: Duration,
        health: Duration,
        active: bool,
        authority: Option<std::sync::Arc<crate::spawn_authority::SpawnAuthorityRecord>>,
    ) -> Result<Option<DeliverySpawn>> {
        let mut state = OwnerGuard(
            self.inner.try_lock().map_err(|error| {
                if matches!(error, std::sync::TryLockError::Poisoned(_)) {
                    return GateError::new("config_unavailable", "configuration owner is poisoned");
                }
                GateError::new(
                    "config_delivery_deferred",
                    "configuration publication is busy",
                )
            })?,
            &self.published,
            &self.delivery_changed,
            &self.module_gates,
            self,
        );
        state.store.check()?;
        // Admission under the stamped gate is the only way to the mark: a
        // stamp never marks a child admitted before it.
        let stamped = admission.gated()
            && state.store.loaded_admission_state().0 == crate::config_store::StoreState::Stamped;
        let spawn = allocate_spawn(
            &mut state,
            module,
            admission.generation(),
            at,
            register,
            health,
            authority.map(|record| (record, stamped)),
        )?;
        state.delivery.track_mark(&spawn);
        state
            .delivery
            .unregistered_children
            .insert((module.into(), active), spawn.clone());
        Ok(Some(spawn))
    }

    #[cfg(test)]
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
        state.store.check()?;
        let generation =
            super::admission::AdmissionCache::from_state(&state).spawn_generation(module)?;
        allocate_spawn(
            &mut state,
            module,
            generation,
            spawned_at,
            register_timeout,
            first_health_budget,
            None,
        )
        .map(Some)
    }

    pub(crate) fn delivery_child_registered(
        &self,
        connection: ConnectionId,
        spawn: &DeliverySpawn,
    ) -> Result<()> {
        let mut state = self.lock()?;
        state.delivery.track_mark(spawn);
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
            let body = state.store.body()?;
            if body.stamped {
                let versions = body
                    .versions
                    .iter()
                    .find(|(tuple, _)| tuple.module == module && tuple.project.is_none())
                    .map(|(_, versions)| versions.clone())
                    .unwrap_or_default();
                if reply
                    .active
                    .as_ref()
                    .is_some_and(|view| versions.active != Some(view.generation))
                    || reply.pending.as_ref().is_some_and(|view| {
                        // A promoted child catches up to the durable active
                        // vector through its own pending, child-local attempt.
                        let catchup = versions.active == Some(view.generation)
                            && state
                                .delivery
                                .attempts
                                .get(&connection)
                                .is_some_and(|bound| {
                                    bound.catchup
                                        && bound.generation == view.generation
                                        && Some(bound.id) == view.attempt
                                });
                        versions.pending != Some(view.generation) && !catchup
                    })
                {
                    return Err(GateError::new(
                        "delivery_superseded",
                        "configuration changed while its view was being written",
                    ));
                }
            }
        }
        if active {
            if let Some(view) = &reply.pending {
                if let Some(attempt) = view.attempt {
                    self.delivery_written(connection, view.generation, attempt, written_at)?;
                }
            }
        }
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
            let policy_ready = super::route_policy::ready(&mut state, module, generation)?;
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
            bound.ready = policy_ready;
            let acknowledged = bound.ready && bound.acknowledged;
            let id = bound.id;
            state
                .delivery
                .readiness_observed
                .insert((connection, generation));
            acknowledged.then_some(id)
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
        Self::promote_loaded(&mut state, connection, module, cutover_at)
    }

    pub(super) fn promote_loaded(
        state: &mut OwnerState,
        connection: ConnectionId,
        module: &str,
        cutover_at: tokio::time::Instant,
    ) -> Result<Option<(u64, u64)>> {
        if state.store.loaded_admission_state().0 == crate::config_store::StoreState::Unstamped {
            if state
                .declarations
                .get(&connection)
                .is_some_and(|(id, _)| id == module)
            {
                state.phone_target_changed(module, connection);
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
        let body = state.store.loaded_admission_state().1;
        let versions = body
            .versions
            .iter()
            .find(|(key, _)| key == &tuple)
            .map(|(_, versions)| versions.clone())
            .unwrap_or_default();
        let transition = versions
            .pending
            .and_then(|id| body.generations.get(&id))
            .map(|entry| Transition::for_module(&entry.generation.changeset, module))
            .transpose()?
            .unwrap_or(Transition::Live);
        let pending_declaration = versions
            .pending
            .and_then(|id| body.generations.get(&id))
            .and_then(|entry| entry.delivery.as_ref())
            .and_then(|snapshot| snapshot.declarations.get(module))
            .cloned();
        let active_declaration = versions
            .active
            .and_then(|id| body.generations.get(&id))
            .and_then(|entry| entry.delivery.as_ref())
            .and_then(|snapshot| snapshot.declarations.get(module))
            .cloned();
        let pending = versions.pending.filter(|generation| {
            state
                .delivery
                .boundary_open(connection, *generation, transition)
        });
        if pending != versions.pending && transition != Transition::DaemonRestart {
            return Err(GateError::new(
                "config_gated",
                "candidate launch was superseded by a configuration requiring a fresh start",
            ));
        }
        let expected = if pending.is_some() {
            pending_declaration
        } else {
            active_declaration
        };
        if expected.as_ref() != Some(&declaration) {
            return Err(GateError::new(
                "config_gated",
                "candidate declaration differs from the approved declaration",
            ));
        }
        let transition = if pending.is_some() {
            transition
        } else {
            Transition::Live
        };
        let vector = (versions.active, pending);
        let catchup = pending.is_none()
            && versions.active.is_some()
            && state.delivery.served.get(&connection) != Some(&vector);
        state.phone_target_changed(module, connection);
        state.active_declarations.insert(id, connection);
        state.delivery.attempts.remove(&connection);
        let Some(generation) = pending.or_else(|| catchup.then_some(versions.active).flatten())
        else {
            return Ok(None);
        };
        let mut attempt = state.delivery.allocate(module, generation, transition)?;
        attempt.catchup = catchup;
        attempt.spawn = state.delivery.children.get(&connection).cloned();
        // Swap readiness completed before promotion. Its new target now gets
        // a full live ACK window, independently of the finished spawn budget.
        // A route obligation still needs its own post-install probe: the
        // candidate's earlier HEALTHY did not bind the installed policy epoch.
        attempt.ready = super::route_policy::ready_loaded(state, module, generation)
            && !state
                .store
                .loaded_admission_state()
                .1
                .generations
                .get(&generation)
                .is_some_and(|entry| {
                    super::route_policy::required(&entry.generation.changeset, module)
                });
        attempt.deadline = Some(cutover_at + CONFIG_LIVE_ACK_TIMEOUT);
        let assignment = (generation, attempt.id);
        state.delivery.attempts.insert(connection, attempt);
        state
            .delivery
            .readiness_observed
            .insert((connection, generation));
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
        let bound = state
            .delivery
            .attempts
            .get_mut(&connection)
            .filter(|bound| bound.generation == generation && bound.id == attempt)
            .ok_or_else(|| {
                GateError::new(
                    "delivery_superseded",
                    "writer receipt no longer names the current delivery attempt",
                )
            })?;
        if bound
            .write_deadline
            .is_some_and(|deadline| written_at >= deadline)
            || bound
                .deadline
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
            // A later approval waiting for a new daemon cannot replace the
            // active configuration this child is catching up to now.
            let pending_transition = versions
                .pending
                .map(|generation| {
                    let entry = state
                        .store
                        .body()?
                        .generations
                        .get(&generation)
                        .ok_or_else(|| {
                            GateError::new("config_gated", "pending generation is absent")
                        })?;
                    Transition::for_module(&entry.generation.changeset, module)
                })
                .transpose()?;
            let pending_is_deliverable =
                versions
                    .pending
                    .zip(pending_transition)
                    .is_some_and(|(generation, transition)| {
                        state
                            .delivery
                            .boundary_open(connection, generation, transition)
                    });
            if pending_is_deliverable || versions.active != Some(bound.generation) {
                return Ok(AppliedDisposition::Ignored);
            }
            state.delivery.attempts.remove(&connection);
            if bound.mark_only {
                // Asked only for the mark: a refusal or timeout leaves the
                // spawn unmarked and starts no recovery.
                return Ok(if ack {
                    super::mark::acknowledged(self, &state, bound.spawn.as_ref(), bound.generation);
                    AppliedDisposition::CatchupComplete
                } else {
                    super::mark::acknowledgement_refused(self, &state, bound.spawn.as_ref());
                    AppliedDisposition::Ignored
                });
            }
            if ack {
                super::mark::acknowledged(self, &state, bound.spawn.as_ref(), bound.generation);
            } else {
                super::mark::delivery_failed(self, bound.spawn.as_ref());
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
        if !ack {
            state.restore_route_policy(module, bound.generation);
        }
        let result = state.store.record_delivery(&tuple, bound.generation, ack);
        if result.is_ok() && ack {
            state.commit_route_policy(module, bound.generation);
        }
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
        if ack && result.is_ok() {
            super::mark::acknowledged(self, &state, bound.spawn.as_ref(), bound.generation);
        } else if !ack {
            super::mark::delivery_failed(self, bound.spawn.as_ref());
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
