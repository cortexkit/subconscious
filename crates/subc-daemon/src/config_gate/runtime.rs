//! Serialized production owner for the approval engine and durable authority.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    sync::Mutex,
    time::{Duration, Instant},
};

use subc_config::{Changeset, Digest};
use subc_control::config::ConfigPlanView;
use subc_protocol::config::{
    ConfigDeclareRequest, ConfigLayer, ConfigLayerSnapshot, ConfigStatus, ConfigView,
};

use super::{
    capture::{CapturedConfiguration, ConfigCapturer},
    ApprovalChallenge, ApprovalFacts, ApprovalReply, ConfigGate, GateError, PlanKind, Result,
};
use crate::{
    config_store::{ConfigStore, DeliverySnapshot, StoreActivity, StoreState},
    ConnectionId,
};

pub struct RuntimeOwner {
    inner: Mutex<OwnerState>,
    capturer: ConfigCapturer,
    module_gates: Mutex<HashMap<String, std::sync::Arc<tokio::sync::Mutex<()>>>>,
}

pub use delivery::{AppliedDisposition, DeliverySpawn};

/// Supervisor callbacks for the configuration delivery lifecycle. The caller
/// supplies observed process events, never a module's claimed readiness.
/// Target checks and promotion callbacks run under [`RuntimeOwner::module_gate`]
/// across forwarding and registry changes. The spawn token binds a later
/// registration to the approved generation and its original warm-up deadline.
pub trait DeliveryObserver: Send + Sync {
    fn spawned(
        &self,
        module: &str,
        spawned_at: tokio::time::Instant,
        register_timeout: std::time::Duration,
        first_health_budget: std::time::Duration,
    ) -> Result<Option<DeliverySpawn>>;
    fn registered(&self, connection: ConnectionId, spawn: &DeliverySpawn) -> Result<()>;
    fn ready(
        &self,
        connection: ConnectionId,
        module: &str,
        generation: u64,
    ) -> Result<Option<AppliedDisposition>>;
    fn promoted(
        &self,
        connection: ConnectionId,
        module: &str,
        cutover_at: tokio::time::Instant,
    ) -> Result<Option<(u64, u64)>>;
    /// Retry only the retained, non-interactive backend when its backoff is due.
    /// `None` means there is no pending anchor and no timer should be armed.
    fn retry_anchor(&self) -> Result<Option<tokio::time::Instant>>;
}

impl DeliveryObserver for RuntimeOwner {
    fn spawned(
        &self,
        module: &str,
        spawned_at: tokio::time::Instant,
        register_timeout: std::time::Duration,
        first_health_budget: std::time::Duration,
    ) -> Result<Option<DeliverySpawn>> {
        self.delivery_spawned(module, spawned_at, register_timeout, first_health_budget)
    }

    fn registered(&self, connection: ConnectionId, spawn: &DeliverySpawn) -> Result<()> {
        self.delivery_child_registered(connection, spawn)
    }

    fn ready(
        &self,
        connection: ConnectionId,
        module: &str,
        generation: u64,
    ) -> Result<Option<AppliedDisposition>> {
        self.delivery_readiness_complete(connection, module, generation)
    }

    fn promoted(
        &self,
        connection: ConnectionId,
        module: &str,
        cutover_at: tokio::time::Instant,
    ) -> Result<Option<(u64, u64)>> {
        self.delivery_promoted(connection, module, cutover_at)
    }

    fn retry_anchor(&self) -> Result<Option<tokio::time::Instant>> {
        self.poll_anchor_retry()
    }
}

/// Keep a reviewed plan for one idle hour so the operator can read the full diff.
const PLAN_IDLE_TIMEOUT: Duration = Duration::from_secs(60 * 60);
/// One shared sweep reclaims idle plans without allocating a timer per plan.
pub(crate) const PLAN_SWEEP_INTERVAL: Duration = Duration::from_secs(60);
/// Remember recent expirations by digest without retaining unbounded plan bodies.
const EXPIRED_PLAN_IDS: usize = 64;

pub(crate) fn daemon_review(
    old: &super::GateSnapshot,
    new: &super::GateSnapshot,
) -> Result<Changeset> {
    let mut changeset = Changeset::new();
    for key in old.changed_keys(new) {
        match key {
            super::InputKey::Entry(module) | super::InputKey::Program { module, .. } => {
                let field = match key {
                    super::InputKey::Entry(_) => "entry".into(),
                    super::InputKey::Program { path, .. } => format!("program:{path}"),
                    _ => unreachable!(),
                };
                let manifest = changeset
                    .launch_manifests
                    .entry(module.clone())
                    .or_insert_with(|| serde_json::json!({}));
                manifest.as_object_mut().expect("launch manifest is an object").insert(field,serde_json::json!({"old":old.inputs.get(key).map(|v| &v.value),"new":new.inputs.get(key).map(|v| &v.value)}));
            }
            super::InputKey::Declaration(module) => {
                changeset.policy_changes.push(serde_json::json!({"module_id":module,"path":"declaration","old":old.inputs.get(key).map(|v| &v.value),"new":new.inputs.get(key).map(|v| &v.value)}));
            }
            super::InputKey::Daemon => {
                changeset.policy_changes.push(serde_json::json!({
                    "path":"subc.jsonc",
                    "old":old.inputs.get(key).map(|v| &v.value),
                    "new":new.inputs.get(key).map(|v| &v.value),
                }));
                changeset.apply_plan = serde_json::json!({"supervisor_reconcile":true});
            }
            _ => {}
        }
    }
    Ok(changeset)
}

struct OwnerState {
    gate: ConfigGate,
    store: ConfigStore,
    _writer_lock: Option<crate::config_store::StoreWriterLock>,
    plan_deadlines: BTreeMap<Digest, Instant>,
    // Only a successfully flushed complete plan on this connection can bind
    // its desktop approval. A later view replaces the connection's old view.
    desktop_views: BTreeMap<u64, (Digest, u64)>,
    delivery: delivery::DeliveryState,
    anchor_retry: super::anchor_retry::AnchorRetry,
    // Keep the original objects alive through approval so an unlinked file's
    // inode/file id cannot be recycled to impersonate the reviewed input.
    retained_plan_files: BTreeMap<Digest, Vec<std::fs::File>>,
    expired_plans: VecDeque<Digest>,
    declarations: HashMap<ConnectionId, (String, ConfigDeclareRequest)>,
    active_declarations: BTreeMap<String, ConnectionId>,
    activity: Option<crate::activity::ActivityLedger>,
}

impl OwnerState {
    fn retry_anchor(&mut self, operator_command: bool) -> Option<tokio::time::Instant> {
        let pending = self.store.status().anchor_pending;
        let events = self.anchor_retry.poll(
            pending,
            tokio::time::Instant::now(),
            operator_command,
            || {
                let error = self
                    .store
                    .retry_anchor()
                    .err()
                    .map(|error| error.to_string());
                (self.store.status().anchor_pending, error)
            },
        );
        for event in events {
            match event {
                super::anchor_retry::Event::Pending => {
                    tracing::info!("configuration anchor became pending")
                }
                super::anchor_retry::Event::Failed(reason) => {
                    tracing::warn!(%reason, "configuration anchor retry failed")
                }
                super::anchor_retry::Event::Recovered => {
                    tracing::info!("configuration anchor recovered")
                }
            }
        }
        self.anchor_retry.next()
    }
    fn with_desktop_view(&self, digest: &Digest, facts: &ApprovalFacts) -> ApprovalFacts {
        let mut checked = facts.clone();
        let bound = self
            .desktop_views
            .get(&facts.connection)
            .is_some_and(|(shown, revision)| {
                shown == digest
                    && self
                        .gate
                        .plans
                        .get(digest)
                        .is_some_and(|plan| plan.view.base_revision == *revision)
            });
        if !bound {
            checked.desktop_app = false;
            checked.attested_operator_app = false;
        }
        checked
    }
    fn expire_idle(&mut self, now: Instant) {
        let expired: Vec<_> = self
            .plan_deadlines
            .iter()
            .filter(|(digest, deadline)| **deadline <= now && !self.gate.has_open_approval(digest))
            .map(|(digest, _)| digest.clone())
            .collect();
        for digest in expired {
            self.plan_deadlines.remove(&digest);
            self.retained_plan_files.remove(&digest);
            self.desktop_views.retain(|_, (shown, _)| shown != &digest);
            if self.gate.expire_plan(&digest) {
                self.expired_plans.push_back(digest);
                if self.expired_plans.len() > EXPIRED_PLAN_IDS {
                    self.expired_plans.pop_front();
                }
            }
        }
    }
    fn reviewed(&mut self, digest: &Digest, now: Instant) {
        self.plan_deadlines
            .insert(digest.clone(), now + PLAN_IDLE_TIMEOUT);
        self.expired_plans.retain(|expired| expired != digest);
    }
}

struct OwnerGuard<'a>(std::sync::MutexGuard<'a, OwnerState>);
impl std::ops::Deref for OwnerGuard<'_> {
    type Target = OwnerState;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl std::ops::DerefMut for OwnerGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
impl Drop for OwnerGuard<'_> {
    fn drop(&mut self) {
        if let Some(ledger) = self.0.activity.clone() {
            for activity in self.0.store.take_activities() {
                ledger.config_store_activity(activity);
            }
        }
    }
}

pub struct PreparedPlan {
    pub candidate: CapturedConfiguration,
    pub bases: BTreeMap<String, ConfigView>,
    pub base_schemas: BTreeMap<String, subc_config::Schema>,
    pub kind: PlanKind,
    pub review_modules: std::collections::BTreeSet<String>,
    base_revision: u64,
    active_generations: BTreeMap<crate::config_store::Tuple, Option<u64>>,
    touched_modules: BTreeSet<String>,
    read_set: subc_config::InputVector,
}

#[cfg(test)]
#[path = "runtime_tests.rs"]
mod tests;

#[path = "delivery.rs"]
pub(crate) mod delivery;

impl RuntimeOwner {
    /// Record the exact full response only after its writer flush succeeds and
    /// the adapter revalidates the same live desktop connection.
    pub(crate) fn desktop_view_delivered(
        &self,
        connection: ConnectionId,
        view: &subc_control::config::ConfigPlanView,
    ) -> Result<()> {
        let mut state = self.lock()?;
        let plan = state
            .gate
            .plans
            .get(&view.digest)
            .ok_or_else(|| GateError::new("plan_unknown", "plan is absent or expired"))?;
        if plan.view != *view {
            return Err(GateError::new(
                "plan_stale",
                "delivered view differs from the immutable plan",
            ));
        }
        state
            .desktop_views
            .insert(connection.get(), (view.digest.clone(), view.base_revision));
        Ok(())
    }
    pub fn new(incarnation: String, mut store: ConfigStore, capturer: ConfigCapturer) -> Self {
        let initial_generation = store.body().map(|body| body.last_generation).unwrap_or(0);
        Self {
            inner: Mutex::new(OwnerState {
                gate: ConfigGate::new(incarnation),
                store,
                _writer_lock: None,
                plan_deadlines: BTreeMap::new(),
                desktop_views: BTreeMap::new(),
                delivery: delivery::DeliveryState::new(initial_generation),
                anchor_retry: super::anchor_retry::AnchorRetry::default(),
                retained_plan_files: BTreeMap::new(),
                expired_plans: VecDeque::new(),
                declarations: HashMap::new(),
                active_declarations: BTreeMap::new(),
                activity: None,
            }),
            capturer,
            module_gates: Mutex::new(HashMap::new()),
        }
    }

    pub fn module_gate(&self, module: &str) -> Result<std::sync::Arc<tokio::sync::Mutex<()>>> {
        let mut gates = self
            .module_gates
            .lock()
            .map_err(|_| GateError::new("config_unavailable", "module gate lock is poisoned"))?;
        Ok(gates.entry(module.into()).or_default().clone())
    }

    pub(crate) fn retain_writer_lock(
        self,
        lock: crate::config_store::StoreWriterLock,
    ) -> Result<Self> {
        self.lock()?._writer_lock = Some(lock);
        Ok(self)
    }

    fn lock(&self) -> Result<OwnerGuard<'_>> {
        let mut guard = self.inner.lock().map(OwnerGuard).map_err(|_| {
            GateError::new("config_unavailable", "configuration owner lock is poisoned")
        })?;
        guard.expire_idle(Instant::now());
        Ok(guard)
    }

    pub(crate) fn sweep_expired(&self) -> Result<()> {
        drop(self.lock()?);
        Ok(())
    }

    pub(crate) fn poll_anchor_retry(&self) -> Result<Option<tokio::time::Instant>> {
        Ok(self.lock()?.retry_anchor(false))
    }

    pub(crate) fn set_activity(
        &self,
        ledger: Option<crate::activity::ActivityLedger>,
    ) -> Result<()> {
        self.lock()?.activity = ledger;
        Ok(())
    }

    pub fn take_activities(&self) -> Vec<StoreActivity> {
        self.inner
            .lock()
            .map(|mut state| state.store.take_activities())
            .unwrap_or_default()
    }

    /// Declaration ownership comes from the registered transport connection.
    /// A candidate's proposal is retained without replacing the incumbent's.
    pub fn declare(
        &self,
        connection: ConnectionId,
        module: &str,
        declaration: ConfigDeclareRequest,
        active: bool,
    ) -> Result<()> {
        subc_config::Schema::new(declaration.schema.clone())
            .map_err(|r| GateError::new("schema_invalid", format!("{module}: {}", r.path)))?;
        let mut state = self.lock()?;
        if state.declarations.contains_key(&connection) {
            return Err(GateError::new(
                "declaration_order",
                "configuration is declared once per connection",
            ));
        }
        state
            .declarations
            .insert(connection, (module.into(), declaration));
        if active {
            state.active_declarations.insert(module.into(), connection);
        }
        Ok(())
    }

    pub fn disconnected(&self, connection: ConnectionId) {
        if let Ok(mut state) = self.inner.lock() {
            state.desktop_views.remove(&connection.get());
            state.delivery.disconnected(connection);
            for digest in state.gate.connection_approval_digests(connection.get()) {
                state.reviewed(&digest, Instant::now());
            }
            state.gate.cancel_connection_approvals(connection.get());
            state.declarations.remove(&connection);
            state
                .active_declarations
                .retain(|_, current| *current != connection);
        }
    }

    pub fn module_get(
        &self,
        connection: ConnectionId,
        module: &str,
        request: subc_protocol::config::ConfigGetRequest,
    ) -> Result<subc_protocol::config::ConfigGetReply> {
        let mut state = self.lock()?;
        let declaration = state
            .declarations
            .get(&connection)
            .filter(|(id, _)| id == module)
            .map(|(_, declaration)| declaration.clone())
            .ok_or_else(|| {
                GateError::new(
                    "declaration_order",
                    "config.declare must precede get or applied on this connection",
                )
            })?;
        if request.project_id.is_some() {
            return Err(GateError::new(
                "project_registry_unavailable",
                format!("{module}: project layer registry is not connected"),
            ));
        }
        state.store.check()?;
        if state.store.status().state == StoreState::Unstamped {
            if request.generation.is_some() || request.attempt.is_some() {
                return Err(GateError::new(
                    "config_gated",
                    "unstamped reads have no assigned generation or delivery attempt",
                ));
            }
            let mut declarations = Self::declarations(&mut state)?;
            declarations.insert(module.into(), declaration);
            let mut captured = self
                .capturer
                .capture_modules(&declarations, &BTreeSet::from([module.to_owned()]))?;
            let view = captured
                .modules
                .remove(module)
                .ok_or_else(|| GateError::new("config_gated", "module declaration is unavailable"))?
                .view;
            return Ok(subc_protocol::config::ConfigGetReply::new(Some(view), None));
        }
        state.store.delivery_ready()?;
        let body = state.store.body()?;
        let tuple = crate::config_store::Tuple {
            module: module.into(),
            project: None,
        };
        let versions = body
            .versions
            .iter()
            .find(|(entry, _)| entry == &tuple)
            .map(|(_, versions)| versions)
            .ok_or_else(|| {
                GateError::new("config_gated", "module is not in an approved generation")
            })?;
        let approved_declaration = [versions.pending, versions.active]
            .into_iter()
            .flatten()
            .any(|generation| {
                body.generations
                    .get(&generation)
                    .and_then(|entry| entry.delivery.as_ref())
                    .and_then(|snapshot| snapshot.declarations.get(module))
                    == Some(&declaration)
            });
        if !approved_declaration {
            return Err(GateError::new(
                "config_gated",
                "connection declaration differs from approved declaration",
            ));
        }
        let view = |generation: Option<u64>, status: ConfigStatus| -> Result<Option<ConfigView>> {
            let Some(generation) = generation else {
                return Ok(None);
            };
            let delivery = body
                .generations
                .get(&generation)
                .and_then(|entry| entry.delivery.as_ref())
                .ok_or_else(|| {
                    GateError::new("config_gated", "approved delivery bytes are absent")
                })?;
            let mut view =
                delivery.modules.get(module).cloned().ok_or_else(|| {
                    GateError::new("config_gated", "approved module view is absent")
                })?;
            view.generation = generation;
            view.status = status;
            Ok(Some(view))
        };
        let active = view(versions.active, ConfigStatus::Active)?;
        let pending = view(versions.pending, ConfigStatus::Pending)?;
        let transition = versions
            .pending
            .and_then(|id| body.generations.get(&id))
            .map(|entry| super::apply::Transition::for_module(&entry.generation.changeset, module))
            .transpose()?
            .unwrap_or(super::apply::Transition::Live);
        if active.is_none() && pending.is_none() {
            return Err(GateError::new(
                "config_gated",
                "module has no active or deliverable generation",
            ));
        }
        delivery::bind_get(
            &mut state,
            connection,
            module,
            &request,
            subc_protocol::config::ConfigGetReply::new(active, pending),
            transition,
        )
    }

    fn declarations(state: &mut OwnerState) -> Result<BTreeMap<String, ConfigDeclareRequest>> {
        let mut declarations = if state.store.status().state == StoreState::Stamped {
            state
                .store
                .body()?
                .generations
                .values()
                .next_back()
                .and_then(|entry| entry.delivery.as_ref())
                .map(|snapshot| snapshot.declarations.clone())
                .unwrap_or_default()
        } else {
            BTreeMap::new()
        };
        for (module, connection) in &state.active_declarations {
            if let Some((_, declaration)) = state.declarations.get(connection) {
                declarations.insert(module.clone(), declaration.clone());
            }
        }
        Ok(declarations)
    }

    fn capture(&self, state: &mut OwnerState) -> Result<CapturedConfiguration> {
        let declarations = Self::declarations(state)?;
        self.capturer.capture(&declarations)
    }

    fn capture_modules(
        &self,
        state: &mut OwnerState,
        modules: &BTreeSet<String>,
    ) -> Result<CapturedConfiguration> {
        self.capturer
            .capture_modules(&Self::declarations(state)?, modules)
    }

    fn check_authority(state: &mut OwnerState, recovery: bool) -> Result<()> {
        state.retry_anchor(true);
        match state.store.check() {
            Ok(()) => Ok(()),
            Err(_)
                if recovery
                    && matches!(
                        state.store.status().state,
                        StoreState::StoreUnverified | StoreState::StoreMissing
                    ) =>
            {
                Ok(())
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Run from a blocking worker: capture and store publication may touch disk
    /// and the platform keychain. No transport wait takes place under this lock.
    pub fn prepare(&self, kind: PlanKind) -> Result<PreparedPlan> {
        let mut state = self.lock()?;
        Self::check_authority(
            &mut state,
            matches!(kind, PlanKind::Stamp | PlanKind::Unstamp),
        )?;
        let candidate = self.capture(&mut state)?;
        if candidate.daemon.bytes.is_none() {
            return Err(GateError::new("config_missing", "subc.jsonc is absent"));
        }
        if matches!(kind, PlanKind::Rollback(_)) {
            return Err(GateError::new(
                "rollback_delivery_unavailable",
                "reverse delivery capture is not connected",
            ));
        }
        if state.store.status().state == StoreState::Stamped {
            state.store.observe(candidate.diagnostic.to_store()?)?;
        }
        let base_revision = state.store.status().base_revision;
        let old = if state.store.status().state == StoreState::Stamped {
            super::GateSnapshot::from_store(&state.store.body()?.approved)?
        } else {
            super::GateSnapshot::default()
        };
        let mut touched_modules: BTreeSet<_> = candidate
            .diagnostic
            .changed_keys(&old)
            .iter()
            .filter_map(|key| key.module().map(str::to_owned))
            .collect();
        if matches!(kind, PlanKind::Stamp | PlanKind::Unstamp) {
            touched_modules.extend(candidate.diagnostic.modules());
            touched_modules.extend(old.modules());
        }
        let review_modules: BTreeSet<String> = touched_modules
            .iter()
            .filter(|module| candidate.modules.contains_key(*module))
            .cloned()
            .collect();
        let read_set = candidate.diagnostic.read_set(&touched_modules)?;
        let mut bases = BTreeMap::new();
        let mut base_schemas = BTreeMap::new();
        let mut active_generations = BTreeMap::new();
        if state.store.status().state == StoreState::Stamped {
            let body = state.store.body()?;
            for (tuple, versions) in &body.versions {
                if touched_modules.contains(&tuple.module) {
                    active_generations.insert(tuple.clone(), versions.active);
                }
                if tuple.project.is_some() || !review_modules.contains(&tuple.module) {
                    continue;
                }
                if let Some(id) = versions.active {
                    let snapshot = body
                        .generations
                        .get(&id)
                        .and_then(|entry| entry.delivery.as_ref())
                        .ok_or_else(|| {
                            GateError::new(
                                "approved_schema_unavailable",
                                format!(
                                    "{}: approved old schema is missing or damaged",
                                    tuple.module
                                ),
                            )
                        })?;
                    let schema = snapshot
                        .declarations
                        .get(&tuple.module)
                        .and_then(|declaration| {
                            subc_config::Schema::new(declaration.schema.clone()).ok()
                        })
                        .ok_or_else(|| {
                            GateError::new(
                                "approved_schema_unavailable",
                                format!(
                                    "{}: approved old schema is missing or damaged",
                                    tuple.module
                                ),
                            )
                        })?;
                    base_schemas.insert(tuple.module.clone(), schema);
                    let view = snapshot.modules.get(&tuple.module).ok_or_else(|| {
                        GateError::new(
                            "config_gated",
                            format!("{}: approved active view is absent", tuple.module),
                        )
                    })?;
                    let mut view = view.clone();
                    view.generation = id;
                    view.status = ConfigStatus::Active;
                    bases.insert(tuple.module.clone(), view);
                }
            }
        }
        Ok(PreparedPlan {
            candidate,
            bases,
            base_schemas,
            kind,
            review_modules,
            base_revision,
            active_generations,
            touched_modules,
            read_set,
        })
    }

    pub fn finish_plan(
        &self,
        mut prepared: PreparedPlan,
        review: impl FnOnce(&super::GateSnapshot, &super::GateSnapshot) -> Result<Changeset>,
    ) -> Result<ConfigPlanView> {
        let mut state = self.lock()?;
        Self::check_authority(
            &mut state,
            matches!(prepared.kind, PlanKind::Stamp | PlanKind::Unstamp),
        )?;
        let active_generations = if state.store.status().state == StoreState::Stamped {
            state
                .store
                .body()?
                .versions
                .iter()
                .filter(|(tuple, _)| prepared.touched_modules.contains(&tuple.module))
                .map(|(tuple, versions)| (tuple.clone(), versions.active))
                .collect()
        } else {
            BTreeMap::new()
        };
        let current = self
            .capture_modules(&mut state, &prepared.touched_modules)
            .map_err(|_| GateError::stale())?;
        if state.store.status().base_revision != prepared.base_revision
            || active_generations != prepared.active_generations
            || !prepared
                .read_set
                .matches(&current.diagnostic.read_set(&prepared.touched_modules)?)
        {
            return Err(GateError::stale());
        }
        let delivery = DeliverySnapshot {
            daemon: Some(ConfigLayerSnapshot::file(
                "subc",
                ConfigLayer::User,
                prepared.candidate.daemon.path.to_string_lossy(),
                prepared.candidate.daemon.bytes.clone(),
            )),
            modules: prepared
                .candidate
                .modules
                .iter()
                .map(|(module, captured)| (module.clone(), captured.view.clone()))
                .collect(),
            declarations: prepared
                .candidate
                .modules
                .iter()
                .map(|(module, captured)| (module.clone(), captured.declaration.clone()))
                .collect(),
        };
        let handles = prepared.candidate.take_handles();
        let OwnerState { gate, store, .. } = &mut *state;
        let view = gate.plan(
            store,
            prepared.kind,
            prepared.candidate.diagnostic,
            &current.diagnostic,
            review,
        )?;
        gate.bind_delivery(&view.digest, delivery)?;
        state.reviewed(&view.digest, Instant::now());
        state
            .retained_plan_files
            .insert(view.digest.clone(), handles);
        Ok(view)
    }

    pub fn begin_approval(
        &self,
        digest: &Digest,
        facts: &ApprovalFacts,
    ) -> Result<ApprovalChallenge> {
        let mut state = self.lock()?;
        if state.expired_plans.contains(digest) {
            return Err(GateError::new(
                "plan_expired",
                "configuration plan exceeded its one-hour idle lifetime",
            ));
        }
        Self::check_authority(&mut state, true)?;
        let modules = state.gate.plan_modules(digest)?;
        let current = self
            .capture_modules(&mut state, &modules)
            .map_err(|_| GateError::stale())?;
        let facts = state.with_desktop_view(digest, facts);
        let OwnerState { gate, store, .. } = &mut *state;
        let challenge = gate.begin_approval(digest, store, &current.diagnostic, &facts)?;
        state.reviewed(digest, Instant::now());
        Ok(challenge)
    }

    pub fn finish_approval(
        &self,
        attempt: u64,
        facts: &ApprovalFacts,
        reply: ApprovalReply,
        now_ms: u64,
    ) -> Result<u64> {
        let mut state = self.lock()?;
        let digest = state.gate.approval_digest(attempt)?;
        state.reviewed(&digest, Instant::now());
        let modules = state.gate.approval_modules(attempt)?;
        // Reopen inputs after the operator's interactive confirmation, immediately
        // before freshness checking and atomic publication of the saved plan.
        let current = match Self::check_authority(&mut state, true).and_then(|_| {
            self.capture_modules(&mut state, &modules)
                .map_err(|_| GateError::stale())
        }) {
            Ok(current) => current,
            Err(error) => {
                state.gate.cancel_attempt(attempt);
                return Err(error);
            }
        };
        let facts = state.with_desktop_view(&digest, facts);
        let was_unstamped = state.store.status().state == StoreState::Unstamped;
        let OwnerState { gate, store, .. } = &mut *state;
        let result =
            gate.finish_approval(attempt, reply, store, &current.diagnostic, &facts, now_ms);
        let stamped_files = was_unstamped
            && state.store.body().is_ok_and(|body| {
                body.generations
                    .get(&body.last_generation)
                    .is_some_and(|entry| {
                        entry.generation.plan_digest == digest
                            && entry.kind == crate::config_store::CommitKind::Stamp
                    })
            });
        if stamped_files {
            let connections: Vec<_> = state.active_declarations.values().copied().collect();
            state.delivery.remember_stamp_connections(connections);
        }
        if result.is_ok() {
            state.plan_deadlines.remove(&digest);
            state.retained_plan_files.remove(&digest);
            state.desktop_views.retain(|_, (shown, _)| shown != &digest);
        }
        result
    }

    pub fn status(&self) -> Result<subc_control::config::ConfigStatus> {
        let mut state = self.lock()?;
        state.retry_anchor(true);
        let verified = state.store.check().is_ok();
        if verified
            && !state.store.status().anchor_pending
            && state.store.status().state == StoreState::Stamped
        {
            let captured = self.capture(&mut state)?;
            state.store.observe(captured.diagnostic.to_store()?)?;
        }
        let store_status = state.store.status();
        let mut status = subc_control::config::ConfigStatus::default();
        status.state = match store_status.state {
            StoreState::Stamped => "stamped",
            StoreState::Unstamped => "unstamped",
            StoreState::StoreMissing => "store_missing",
            StoreState::StoreUnverified => "store_unverified",
        }
        .into();
        status.integrity = match store_status.integrity {
            crate::config_store::IntegrityMode::DigestOnly => "digest_only",
            crate::config_store::IntegrityMode::Sealed => "sealed",
        }
        .into();
        status.revision = store_status.revision;
        status.base_revision = store_status.base_revision;
        status.anchor_pending = store_status.anchor_pending;
        status.warnings = store_status.warnings;
        if cfg!(feature = "dev-desktop-approval") {
            status
                .development_switches
                .push("dev-desktop-approval".into());
        }
        if let Ok(body) = state.store.body() {
            status.pending_proposals = u64::from(body.proposal.is_some());
            status.gated_modules = body
                .versions
                .iter()
                .map(|(tuple, _)| tuple.module.clone())
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect();
            for (tuple, versions) in &body.versions {
                if let Some(generation) = versions.pending {
                    let entry = body.generations.get(&generation);
                    let transition = entry.and_then(|entry| {
                        super::apply::Transition::for_module(
                            &entry.generation.changeset,
                            &tuple.module,
                        )
                        .ok()
                    });
                    status.approved_but_inactive.push(serde_json::json!({
                        "module_id":tuple.module,"project_id":tuple.project,"generation":generation,
                        "digest":entry.map(|entry| &entry.generation.plan_digest),
                        "waiting_for":match transition {
                            Some(super::apply::Transition::DaemonRestart) => "daemon_restart",
                            Some(super::apply::Transition::NextSupervision) => "next_supervision",
                            _ => "delivery",
                        },
                    }));
                }
            }
        }
        Ok(status)
    }

    pub fn history(&self) -> Result<(Vec<subc_config::Generation>, Vec<serde_json::Value>)> {
        let mut state = self.lock()?;
        state.retry_anchor(true);
        state.store.check()?;
        let body = state.store.body()?;
        Ok((
            body.generations
                .values()
                .map(|entry| entry.generation.clone())
                .collect(),
            body.pruned
                .values()
                .map(serde_json::to_value)
                .collect::<std::result::Result<_, _>>()
                .map_err(GateError::invalid)?,
        ))
    }

    /// Admission is owned by the supervisor adapter; until it is installed it
    /// must not infer executable authority from diagnostic snapshots.
    pub fn approved_snapshot(&self) -> Result<DeliverySnapshot> {
        let mut state = self.lock()?;
        state.store.delivery_ready()?;
        state
            .store
            .body()?
            .generations
            .values()
            .next_back()
            .and_then(|entry| entry.delivery.clone())
            .ok_or_else(|| GateError::new("config_gated", "approved delivery snapshot is absent"))
    }
}
