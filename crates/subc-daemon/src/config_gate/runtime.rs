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
    #[cfg(test)]
    polled_capture_hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    #[cfg(test)]
    census_probe: Mutex<Option<std::sync::Arc<dyn Fn() + Send + Sync>>>,
    #[cfg(test)]
    agent_validation_window: Mutex<Option<Duration>>,
    #[cfg(test)]
    pub(crate) warm_restart_pause: Mutex<Option<WarmRestartPause>>,
    #[cfg(test)]
    pub(crate) warm_restart_busy: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    #[cfg(test)]
    pub(crate) swap_preflight_attempts: std::sync::atomic::AtomicUsize,
    #[cfg(feature = "test-support")]
    test_hooks: Option<crate::config_store::testing::TestSupport>,
    #[cfg(test)]
    spawn_exec_hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    published: std::sync::RwLock<std::sync::Arc<Published>>,
    configured_modules: std::sync::RwLock<Option<admission::ConfiguredModuleSnapshot>>,
    inner: Mutex<OwnerState>,
    capturer: ConfigCapturer,
    startup_overrides: Vec<String>,
    module_gates: Mutex<HashMap<String, std::sync::Arc<tokio::sync::Mutex<()>>>>,
    delivery_changed: std::sync::Arc<tokio::sync::Notify>,
    route_policy_authority: std::sync::OnceLock<std::sync::Arc<dyn RoutePolicyAuthority>>,
    binding_retirements: Mutex<BTreeMap<usize, Box<dyn Fn() + Send + Sync>>>,
    canceled_approvals: Mutex<Vec<u64>>,
    // Reader and Drop callbacks must not wait behind a keychain or disk check.
    // These short queues/indexes never perform store I/O while locked.
    approval_request_index: Mutex<HashMap<(ConnectionId, u64), std::sync::Arc<ApprovalRequest>>>,
    disconnected_pending: Mutex<Vec<ConnectionId>>,
    abandoned_spawns: Mutex<Vec<(DeliverySpawn, bool)>>,
    phone_withdrawals: Mutex<phone::PhoneWithdrawals>,
    #[cfg(any(test, feature = "test-support"))]
    os_prompt_policy: std::sync::atomic::AtomicBool,
    #[cfg(test)]
    spawn_preflight_hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    #[cfg(test)]
    exit_record_attempts: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    phone_bind_hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    #[cfg(test)]
    begin_approval_hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    #[cfg(test)]
    begin_approval_completions: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    desktop_receipt_hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    #[cfg(test)]
    prepare_capture_hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    #[cfg(all(test, unix))]
    approval_claim_pause: Mutex<Option<ApprovalClaimPause>>,
}

/// An in-memory cutover transaction. It is borrowed only for synchronous
/// forwarding, registry and delivery-target publication under one module gate.
pub(crate) struct SwapConfiguration<'a> {
    owner: &'a RuntimeOwner,
    state: OwnerGuard<'a>,
}

#[cfg(test)]
pub(crate) struct WarmRestartPause {
    pub(crate) reached: tokio::sync::oneshot::Sender<()>,
    pub(crate) resume: tokio::sync::oneshot::Receiver<()>,
}

impl SwapConfiguration<'_> {
    pub(crate) fn before_cutover(
        &mut self,
        module: &str,
        connection: ConnectionId,
        spawn: Option<&DeliverySpawn>,
    ) -> Result<Option<u64>> {
        match self.state.store.loaded_admission_state().0 {
            StoreState::Unstamped => return Ok(None),
            StoreState::Stamped => {}
            _ => {
                return Err(GateError::new(
                    "config_gated",
                    "configuration store is in recovery",
                ))
            }
        }
        let newer = RuntimeOwner::requires_fresher_spawn_loaded(&self.state, module, spawn)?;
        if newer.is_none() {
            self.owner.module_get_locked(
                &mut self.state,
                connection,
                module,
                subc_protocol::config::ConfigGetRequest::new(),
                false,
            )?;
        }
        Ok(newer)
    }
    pub(crate) fn promoted(
        &mut self,
        module: &str,
        connection: ConnectionId,
        at: tokio::time::Instant,
    ) -> Result<()> {
        RuntimeOwner::promote_loaded(&mut self.state, connection, module, at).map(|_| ())
    }
}

#[cfg(all(test, unix))]
struct ApprovalClaimPause {
    reached: tokio::sync::oneshot::Sender<()>,
    release: tokio::sync::oneshot::Receiver<()>,
}

/// The post-answer deadline can withdraw queued work, but never a publication
/// that has crossed the final commit point while holding its owned gates.
pub(crate) struct ApprovalCommitWindow {
    pub deadline: tokio::time::Instant,
    phase: std::sync::atomic::AtomicU8,
}

impl ApprovalCommitWindow {
    pub(crate) fn new(deadline: tokio::time::Instant) -> Self {
        Self {
            deadline,
            phase: std::sync::atomic::AtomicU8::new(0),
        }
    }

    pub(crate) fn expire(&self) -> bool {
        self.phase
            .compare_exchange(
                0,
                2,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_ok()
    }

    fn begin_publication(&self) -> Result<()> {
        if tokio::time::Instant::now() >= self.deadline {
            self.expire();
            return Err(GateError::commit_timeout());
        }
        self.phase
            .compare_exchange(
                0,
                1,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .map(|_| ())
            .map_err(|_| GateError::commit_timeout())
    }
}

pub use route_policy::{
    ApprovedRoutePolicy, RoutePolicyAuthority, RoutePolicyInstall, RoutePolicyInstallation,
    RoutePolicyReceipt,
};

pub use delivery::{AppliedDisposition, DeliveryProbe, DeliverySpawn};

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
    fn health_probe(&self, connection: ConnectionId, module: &str)
        -> Result<Option<DeliveryProbe>>;
    fn health_result(
        &self,
        probe: DeliveryProbe,
        healthy: bool,
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

    /// Capture before dispatching the ordinary health RPC. A later result must
    /// use this witness, not look up whichever target is current on completion.
    fn health_probe(
        &self,
        connection: ConnectionId,
        module: &str,
    ) -> Result<Option<DeliveryProbe>> {
        self.delivery_health_probe(connection, module)
    }

    /// Complete under the module gate. A new target, attempt or policy install
    /// invalidates an older witness rather than lending its health to new bytes.
    fn health_result(
        &self,
        probe: DeliveryProbe,
        healthy: bool,
    ) -> Result<Option<AppliedDisposition>> {
        self.delivery_health_result(probe, healthy)
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
    observed_snapshot: Option<super::GateSnapshot>,
    stamp_channel_cutover: bool,
    gate: ConfigGate,
    store: ConfigStore,
    _writer_lock: Option<crate::config_store::StoreWriterLock>,
    plan_deadlines: BTreeMap<Digest, Instant>,
    // Only a successfully flushed complete plan on this connection can bind
    // its desktop approval. A later view replaces the connection's old view.
    desktop_views: BTreeMap<u64, (Digest, u64)>,
    phone_waiters: BTreeMap<u64, tokio::sync::oneshot::Sender<()>>,
    approval_requests: HashMap<(ConnectionId, u64), u64>,
    delivery: delivery::DeliveryState,
    anchor_retry: super::anchor_retry::AnchorRetry,
    // Keep the original objects alive through approval so an unlinked file's
    // inode/file id cannot be recycled to impersonate the reviewed input.
    retained_plan_files: BTreeMap<Digest, Vec<std::fs::File>>,
    expired_plans: VecDeque<Digest>,
    declarations: HashMap<ConnectionId, (String, ConfigDeclareRequest)>,
    active_declarations: BTreeMap<String, ConnectionId>,
    activity: Option<crate::activity::ActivityLedger>,
    refused_generations: BTreeMap<String, Digest>,
    admission_holds: Vec<String>,
    admission_releases: Vec<String>,
    authority_exit: Option<u64>,
    // Last reconciled daemon entries, independent of module-layer activation.
    reconciled_entries: BTreeMap<String, crate::daemon_config::ConfiguredModule>,
    recovery_exit_hold: bool,
    unstamp_hints: std::collections::HashSet<ConnectionId>,
    anchor_waiters: std::collections::HashSet<ConnectionId>,
    anchor_hints: std::collections::HashSet<ConnectionId>,
    anchor_notify: std::sync::Arc<tokio::sync::Notify>,
    installed_route_policies: BTreeMap<String, u64>,
    route_policy_receipts: BTreeMap<(String, u64), route_policy::PendingRoutePolicy>,
    #[cfg(test)]
    publication_hook: Option<Box<dyn FnOnce() + Send>>,
    #[cfg(test)]
    prepublication_hook: Option<Box<dyn FnOnce() + Send>>,
    #[cfg(test)]
    after_approval_drain_hook: Option<Box<dyn FnOnce() + Send>>,
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
                    tracing::info!("configuration anchor recovered");
                    self.queue_anchor_hints();
                }
            }
        }
        self.commit_anchored_route_policies();
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

struct OwnerGuard<'a>(
    std::sync::MutexGuard<'a, OwnerState>,
    &'a std::sync::RwLock<std::sync::Arc<Published>>,
    &'a tokio::sync::Notify,
    &'a Mutex<HashMap<String, std::sync::Arc<tokio::sync::Mutex<()>>>>,
    &'a RuntimeOwner,
);
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
        // Project committed or invalidated authority before releasing its owner.
        // Parse changed generations outside the cheap reader lock, then publish
        // only the Arc; list polling never waits for store or keychain work.
        let previous = self
            .1
            .read()
            .map(|view| view.approved_daemon.clone())
            .unwrap_or(ApprovedDaemonView::Unavailable);
        let approved_daemon =
            ApprovedDaemonView::from_store(&self.0.store, self.0.recovery_exit_hold, &previous);
        // Volatile holds follow the same lifetime as their durable records.
        // Approving another program identity retires the previous identity's
        // volatile hold, even if that previous identity is later approved again.
        let (_, body) = self.0.store.loaded_admission_state();
        let valid: std::collections::BTreeSet<_> = body
            .program_refusals
            .iter()
            .map(|refusal| (refusal.module.clone(), refusal.approved_base.clone()))
            .collect();
        self.0
            .refused_generations
            .retain(|module, base| valid.contains(&(module.clone(), base.clone())));
        let next = admission::AdmissionCache::from_state(&self.0);
        let observed = self.0.observed_snapshot.clone();
        let projection = self.4.status_projection(&mut self.0, observed, &next);
        if let Ok(mut cache) = self.1.write() {
            let (holds, releases) = next.transitions_from(&cache);
            let changed = next.is_recovering() != cache.is_recovering()
                || !holds.is_empty()
                || !releases.is_empty();
            self.0.admission_holds.extend(holds);
            self.0.admission_releases.extend(releases);
            *cache = std::sync::Arc::new(Published {
                admission: next,
                approved_daemon,
                projection,
            });
            if changed {
                self.2.notify_one();
            }
        }
        let Some(ledger) = self.0.activity.clone() else {
            return;
        };
        let Ok(gates) = self.3.try_lock() else {
            return;
        };
        let held: Vec<_> = gates
            .values()
            .filter(|gate| gate.try_lock().is_err())
            .cloned()
            .collect();
        drop(gates);
        if held.is_empty() {
            for activity in self.0.store.take_activities() {
                ledger.config_store_activity(activity);
            }
        } else if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let activities = self.0.store.take_activities();
            if !activities.is_empty() {
                runtime.spawn(async move {
                    // No store mutex follows the activity into this task. Drop
                    // each gate before waiting for another, then publish the log.
                    for gate in held {
                        drop(gate.lock_owned().await);
                    }
                    for activity in activities {
                        ledger.config_store_activity(activity);
                    }
                });
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
    program_keys: Vec<super::InputKey>,
    observed_handles: Vec<std::fs::File>,
}

/// Immutable command inputs for the supervisor's approved rollback admission.
pub struct ApprovedRollbackCommands {
    pub generation: u64,
    pub daemon: ConfigLayerSnapshot,
    pub programs: BTreeMap<String, ApprovedRollbackProgram>,
}

/// A supervisor reconciliation witness minted only from a verified committed
/// generation. Keeping the raw layer prevents a caller from substituting a
/// freshly parsed live file for the approved command inputs.
#[derive(Clone, Debug)]
pub(crate) struct ApprovedDaemonConfiguration {
    generation: u64,
    daemon: std::sync::Arc<ConfigLayerSnapshot>,
    changed_entries: BTreeSet<String>,
}

pub struct ApprovedRollbackProgram {
    pub path: String,
    pub identity: super::CapturedInput,
}

#[cfg(test)]
#[path = "runtime_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "commit_pause.rs"]
pub(crate) mod commit_pause;

#[path = "delivery.rs"]
pub(crate) mod delivery;

#[path = "admission.rs"]
mod admission;
#[path = "recovery.rs"]
mod recovery;
#[path = "route_policy.rs"]
mod route_policy;
pub(crate) use admission::SpawnAdmission;

#[path = "approval_request.rs"]
mod approval_request;
#[path = "daemon_readers.rs"]
mod daemon_readers;
pub(crate) use daemon_readers::ApprovedDaemonView;
#[path = "phone.rs"]
mod phone;
#[path = "placement.rs"]
mod placement;
#[path = "polled.rs"]
mod polled;
#[path = "published.rs"]
mod published;
pub(crate) use published::Published;
#[path = "status_inputs.rs"]
mod status_inputs;
pub(crate) use approval_request::{ApprovalRequest, ApprovalRequestGuard, CancelApprovalRequest};

impl RuntimeOwner {
    /// Keep store I/O and waits for its synchronous owner lock off runtime threads.
    pub(crate) async fn on_blocking<T: Send + 'static>(
        self: &std::sync::Arc<Self>,
        work: impl FnOnce(std::sync::Arc<Self>) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let owner = self.clone();
        tokio::task::spawn_blocking(move || work(owner))
            .await
            .map_err(|_| GateError::new("config_unavailable", "configuration worker stopped"))?
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn pause_spawn_before_exec(&self) -> Result<()> {
        if let Some(hooks) = &self.test_hooks {
            hooks
                .pause_spawn_before_exec()
                .map_err(|error| GateError::new("config_test_pause", error.to_string()))?;
        }
        Ok(())
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn pause_swap_before_promotion(&self) -> Result<()> {
        if let Some(hooks) = &self.test_hooks {
            hooks
                .pause_swap_before_promotion()
                .map_err(|error| GateError::new("config_test_pause", error.to_string()))?;
        }
        Ok(())
    }
    /// Record the exact full response only after its writer flush succeeds and
    /// the adapter revalidates the same live desktop connection.
    #[cfg(test)]
    pub(crate) fn desktop_view_delivered(
        &self,
        connection: ConnectionId,
        view: &subc_control::config::ConfigPlanView,
    ) -> Result<()> {
        self.desktop_view_delivered_when(connection, view, || true)
    }

    pub(crate) fn desktop_view_delivered_when(
        &self,
        connection: ConnectionId,
        view: &subc_control::config::ConfigPlanView,
        is_live: impl FnOnce() -> bool,
    ) -> Result<()> {
        #[cfg(test)]
        if let Some(hook) = self.desktop_receipt_hook.lock().unwrap().take() {
            hook();
        }
        let mut state = self.lock()?;
        // Check the live peer after acquiring the owner, without retaining the
        // peer mutex during an owner wait or any configuration/store work.
        if !is_live() {
            return Ok(());
        }
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
        let admission = admission::AdmissionCache::from_store(&store);
        let approved_daemon =
            ApprovedDaemonView::from_store(&store, false, &ApprovedDaemonView::Unavailable);
        let reconciled_entries = match &approved_daemon {
            ApprovedDaemonView::Approved { config, .. } => config
                .modules
                .iter()
                .map(|module| (module.module_id.clone(), module.clone()))
                .collect(),
            _ => BTreeMap::new(),
        };
        let published = std::sync::RwLock::new(std::sync::Arc::new(Published {
            admission,
            approved_daemon,
            projection: polled::Projection::default(),
        }));
        let delivery_changed = std::sync::Arc::new(tokio::sync::Notify::new());
        let owner = Self {
            #[cfg(test)]
            polled_capture_hook: Mutex::new(None),
            #[cfg(test)]
            census_probe: Mutex::new(None),
            #[cfg(test)]
            agent_validation_window: Mutex::new(None),
            #[cfg(test)]
            warm_restart_pause: Mutex::new(None),
            #[cfg(test)]
            warm_restart_busy: Mutex::new(None),
            #[cfg(test)]
            swap_preflight_attempts: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(feature = "test-support")]
            test_hooks: store.test_support().cloned(),
            #[cfg(test)]
            spawn_exec_hook: Mutex::new(None),
            published,
            configured_modules: std::sync::RwLock::new(None),
            inner: Mutex::new(OwnerState {
                observed_snapshot: None,
                stamp_channel_cutover: false,
                gate: ConfigGate::new(incarnation),
                store,
                _writer_lock: None,
                plan_deadlines: BTreeMap::new(),
                desktop_views: BTreeMap::new(),
                phone_waiters: BTreeMap::new(),
                approval_requests: HashMap::new(),
                delivery: delivery::DeliveryState::new(initial_generation),
                anchor_retry: super::anchor_retry::AnchorRetry::default(),
                retained_plan_files: BTreeMap::new(),
                expired_plans: VecDeque::new(),
                declarations: HashMap::new(),
                active_declarations: BTreeMap::new(),
                activity: None,
                refused_generations: BTreeMap::new(),
                admission_holds: Vec::new(),
                admission_releases: Vec::new(),
                authority_exit: None,
                reconciled_entries,
                recovery_exit_hold: false,
                unstamp_hints: std::collections::HashSet::new(),
                anchor_waiters: std::collections::HashSet::new(),
                anchor_hints: std::collections::HashSet::new(),
                anchor_notify: delivery_changed.clone(),
                installed_route_policies: BTreeMap::new(),
                route_policy_receipts: BTreeMap::new(),
                #[cfg(test)]
                publication_hook: None,
                #[cfg(test)]
                prepublication_hook: None,
                #[cfg(test)]
                after_approval_drain_hook: None,
            }),
            capturer,
            startup_overrides: status_inputs::present_overrides(|name| {
                std::env::var_os(name).is_some()
            }),
            module_gates: Mutex::new(HashMap::new()),
            delivery_changed,
            route_policy_authority: std::sync::OnceLock::new(),
            binding_retirements: Mutex::new(BTreeMap::new()),
            canceled_approvals: Mutex::new(Vec::new()),
            approval_request_index: Mutex::new(HashMap::new()),
            disconnected_pending: Mutex::new(Vec::new()),
            abandoned_spawns: Mutex::new(Vec::new()),
            phone_withdrawals: Mutex::new(phone::PhoneWithdrawals::default()),
            #[cfg(any(test, feature = "test-support"))]
            os_prompt_policy: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            spawn_preflight_hook: Mutex::new(None),
            #[cfg(test)]
            exit_record_attempts: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            phone_bind_hook: Mutex::new(None),
            #[cfg(test)]
            begin_approval_hook: Mutex::new(None),
            #[cfg(test)]
            begin_approval_completions: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            desktop_receipt_hook: Mutex::new(None),
            #[cfg(test)]
            prepare_capture_hook: Mutex::new(None),
            #[cfg(all(test, unix))]
            approval_claim_pause: Mutex::new(None),
        };
        if let Ok(state) = owner.lock() {
            drop(state);
        }
        owner
    }

    pub fn module_gate(&self, module: &str) -> Result<std::sync::Arc<tokio::sync::Mutex<()>>> {
        let mut gates = self
            .module_gates
            .lock()
            .map_err(|_| GateError::new("config_unavailable", "module gate lock is poisoned"))?;
        Ok(gates.entry(module.into()).or_default().clone())
    }

    #[cfg(all(test, unix))]
    pub(crate) fn set_agent_validation_window_for_test(&self, window: Duration) {
        *self.agent_validation_window.lock().unwrap() = Some(window);
    }

    #[cfg(test)]
    pub(crate) fn agent_validation_window_for_test(&self) -> Option<Duration> {
        *self.agent_validation_window.lock().unwrap()
    }

    pub(crate) fn install_binding_retirement(
        &self,
        key: usize,
        retire: impl Fn() + Send + Sync + 'static,
    ) {
        self.binding_retirements
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(key, Box::new(retire));
    }

    pub(crate) fn delivery_notifications(&self) -> std::sync::Arc<tokio::sync::Notify> {
        self.delivery_changed.clone()
    }

    pub(crate) fn notify_delivery_changed(&self) {
        self.delivery_changed.notify_one();
    }

    pub(crate) fn retain_writer_lock(
        self,
        lock: crate::config_store::StoreWriterLock,
    ) -> Result<Self> {
        self.lock()?._writer_lock = Some(lock);
        Ok(self)
    }

    fn lock(&self) -> Result<OwnerGuard<'_>> {
        let mut guard = self
            .inner
            .lock()
            .map(|guard| {
                OwnerGuard(
                    guard,
                    &self.published,
                    &self.delivery_changed,
                    &self.module_gates,
                    self,
                )
            })
            .map_err(|_| {
                GateError::new("config_unavailable", "configuration owner lock is poisoned")
            })?;
        #[cfg(feature = "test-support")]
        guard.store.refresh_test_anchor_failure();
        self.drain_canceled_approvals(&mut guard);
        self.drain_disconnections(&mut guard);
        self.drain_abandoned_spawns(&mut guard);
        guard.expire_idle(Instant::now());
        Ok(guard)
    }

    pub(crate) fn try_swap_configuration(&self) -> Result<SwapConfiguration<'_>> {
        let state = self.inner.try_lock().map_err(|_| {
            GateError::new(
                "config_delivery_deferred",
                "configuration publication is busy or unavailable",
            )
        })?;
        Ok(SwapConfiguration {
            owner: self,
            state: OwnerGuard(
                state,
                &self.published,
                &self.delivery_changed,
                &self.module_gates,
                self,
            ),
        })
    }

    pub(crate) fn try_requires_fresher_spawn(
        &self,
        module: &str,
        spawn: Option<&DeliverySpawn>,
    ) -> Result<Option<u64>> {
        let state = self.inner.try_lock().map_err(|_| {
            GateError::new(
                "config_delivery_deferred",
                "configuration publication is busy or unavailable",
            )
        })?;
        Self::requires_fresher_spawn_loaded(&state, module, spawn)
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
        if state.store.status().state == StoreState::StoreMissing
            || state.store.status().state == StoreState::StoreUnverified
        {
            return Err(GateError::new(
                "config_gated",
                "configuration store is in recovery",
            ));
        }
        if state.declarations.contains_key(&connection) {
            return Err(GateError::new(
                "declaration_order",
                "configuration is declared once per connection",
            ));
        }
        state
            .declarations
            .insert(connection, (module.into(), declaration));
        state.delivery.declared(connection, module, active);
        if active {
            state.active_declarations.insert(module.into(), connection);
        }
        Ok(())
    }

    pub fn disconnected(&self, connection: ConnectionId) {
        if let Ok(mut index) = self.approval_request_index.lock() {
            index.retain(|(caller, _), request| {
                if *caller == connection {
                    request.cancel();
                    false
                } else {
                    true
                }
            });
        }
        self.notify_delivery_changed();
        if let Ok(mut state) = self.inner.try_lock() {
            state.disconnected(connection);
        } else if let Ok(mut pending) = self.disconnected_pending.lock() {
            pending.push(connection);
        }
    }

    pub fn module_get(
        &self,
        connection: ConnectionId,
        module: &str,
        request: subc_protocol::config::ConfigGetRequest,
    ) -> Result<subc_protocol::config::ConfigGetReply> {
        let mut state = self.lock()?;
        self.module_get_locked(&mut state, connection, module, request, true)
    }

    fn module_get_locked(
        &self,
        state: &mut OwnerState,
        connection: ConnectionId,
        module: &str,
        request: subc_protocol::config::ConfigGetRequest,
        check_store: bool,
    ) -> Result<subc_protocol::config::ConfigGetReply> {
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
        if check_store {
            let checked = state.store.check();
            if matches!(
                state.store.loaded_admission_state().0,
                StoreState::StoreMissing | StoreState::StoreUnverified
            ) {
                return recovery::active_view(state, module, &declaration, &request);
            }
            checked?;
        }
        if state.store.loaded_admission_state().0 == StoreState::Unstamped {
            if request.generation.is_some() || request.attempt.is_some() {
                return Err(GateError::new(
                    "config_gated",
                    "unstamped reads have no assigned generation or delivery attempt",
                ));
            }
            let mut declarations = Self::declarations(state)?;
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
        if check_store {
            state.store.delivery_ready()?;
        }
        let body = if check_store {
            state.store.body()?
        } else {
            state.store.loaded_admission_state().1
        };
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
        if !check_store {
            return delivery::visible_reply(
                state,
                connection,
                subc_protocol::config::ConfigGetReply::new(active, pending),
                transition,
            );
        }
        delivery::bind_get(
            state,
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
        let captured = self.capturer.capture(&declarations)?;
        state.observed_snapshot = Some(captured.diagnostic.clone());
        Ok(captured)
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
        if let Err(error) = state.store.check() {
            return if recovery
                && matches!(
                    state.store.loaded_admission_state().0,
                    StoreState::StoreUnverified | StoreState::StoreMissing
                ) {
                Ok(())
            } else {
                Err(error.into())
            };
        }
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
    /// Preflight may record the proposal observed on disk. Refusals for missing
    /// authoritative project membership, active-scope usage or approved-program
    /// launch selection happen before any plan, approval or generation is
    /// written. A proposal neither approves nor records the refused plan.
    pub fn prepare(&self, kind: PlanKind) -> Result<PreparedPlan> {
        let mut state = self.lock()?;
        Self::check_authority(
            &mut state,
            matches!(kind, PlanKind::Stamp | PlanKind::Unstamp),
        )?;
        #[cfg(test)]
        if let Some(hook) = self.prepare_capture_hook.lock().unwrap().take() {
            hook();
        }
        let observed = self.capture(&mut state)?;
        if observed.daemon.bytes.is_none() {
            return Err(GateError::new(
                "invalid_daemon_config",
                "subc.jsonc is absent; refusing to rescan",
            ));
        }
        if !state.recovery_exit_hold && state.store.status().state == StoreState::Stamped {
            state.store.observe(observed.diagnostic.to_store()?)?;
        }
        let rollback = if let PlanKind::Rollback(id) = kind {
            let target = super::GateSnapshot::from_store(&state.store.rollback_configuration(id)?)?;
            let delivery = state
                .store
                .body()?
                .generations
                .get(&id)
                .and_then(|entry| entry.delivery.as_ref())
                .ok_or_else(|| {
                    GateError::new(
                        "rollback_snapshot_unavailable",
                        "retained raw configuration is absent",
                    )
                })?;
            Some((self.capturer.capture_retained(&target, delivery)?, target))
        } else {
            None
        };
        let (candidate, mut observed, target) = if let Some((candidate, target)) = rollback {
            (candidate, Some(observed), Some(target))
        } else {
            (observed, None, None)
        };
        if kind == PlanKind::Stamp {
            super::registration::check_stamp(&candidate.diagnostic)?;
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
        if let Some(target) = target {
            touched_modules.extend(
                candidate
                    .diagnostic
                    .inputs
                    .iter()
                    .filter(|(key, value)| {
                        key.is_program() && target.inputs.get(*key) != Some(*value)
                    })
                    .filter_map(|(key, _)| key.module().map(str::to_owned)),
            );
        }
        let review_modules: BTreeSet<String> = touched_modules
            .iter()
            .filter(|module| candidate.modules.contains_key(*module))
            .filter(|module|matches!(kind,PlanKind::Stamp|PlanKind::Unstamp) || candidate.diagnostic.changed_keys(&old).iter().any(|key|matches!(key,super::InputKey::Layer {module:owner,..}|super::InputKey::Declaration(owner) if owner==*module)))
            .cloned()
            .collect();
        let program_keys: Vec<_> = candidate
            .diagnostic
            .inputs
            .keys()
            .filter(|key| {
                key.is_program()
                    && key
                        .module()
                        .is_some_and(|module| touched_modules.contains(module))
            })
            .cloned()
            .collect();
        let read_set = if let Some(observed) = &mut observed {
            observed.capture_programs(&program_keys)?;
            if program_keys.iter().any(|key| {
                observed.diagnostic.inputs.get(key) != candidate.diagnostic.inputs.get(key)
            }) {
                return Err(GateError::stale());
            }
            observed.diagnostic.read_set(&touched_modules)?
        } else {
            candidate.diagnostic.read_set(&touched_modules)?
        };
        let observed_handles = observed
            .as_mut()
            .map(CapturedConfiguration::take_handles)
            .unwrap_or_default();
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
            program_keys,
            observed_handles,
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
        let mut current = self
            .capture_modules(&mut state, &prepared.touched_modules)
            .map_err(|_| GateError::stale())?;
        current
            .capture_programs(&prepared.program_keys)
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
        let mut handles = prepared.candidate.take_handles();
        handles.append(&mut prepared.observed_handles);
        let OwnerState { gate, store, .. } = &mut *state;
        let view = gate.plan(
            store,
            prepared.kind,
            prepared.candidate.diagnostic,
            &current.diagnostic,
            review,
        )?;
        if let Err(error) = self.check_route_policy_authority(&view.plan.changeset) {
            gate.expire_plan(&view.digest);
            return Err(error);
        }
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
        self.begin_approval_for_request(digest, facts, None)
    }

    pub(crate) fn begin_approval_for_request(
        &self,
        digest: &Digest,
        facts: &ApprovalFacts,
        request: Option<&ApprovalRequest>,
    ) -> Result<ApprovalChallenge> {
        #[cfg(test)]
        struct Completion<'a>(&'a std::sync::atomic::AtomicUsize);
        #[cfg(test)]
        impl Drop for Completion<'_> {
            fn drop(&mut self) {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
        #[cfg(test)]
        let _completed = Completion(&self.begin_approval_completions);
        #[cfg(test)]
        if let Some(hook) = self.begin_approval_hook.lock().unwrap().take() {
            hook();
        }
        let mut state = self.lock()?;
        if let Some(request) = request {
            request.ensure_live()?;
        }
        if state.expired_plans.contains(digest) {
            return Err(GateError::new(
                "plan_expired",
                "configuration plan exceeded its one-hour idle lifetime",
            ));
        }
        Self::check_authority(&mut state, true)?;
        let modules = state.gate.plan_modules(digest)?;
        let program_keys = state.gate.plan_program_keys(digest)?;
        let mut current = self
            .capture_modules(&mut state, &modules)
            .map_err(|_| GateError::stale())?;
        current
            .capture_programs(&program_keys)
            .map_err(|_| GateError::stale())?;
        let facts = state.with_desktop_view(digest, facts);
        let OwnerState { gate, store, .. } = &mut *state;
        if let Some(request) = request {
            request.ensure_live()?;
        }
        let challenge = gate.begin_approval(digest, store, &current.diagnostic, &facts)?;
        state.reviewed(digest, Instant::now());
        Ok(challenge)
    }

    /// Acquire only after interactive approval. Sorted module locks serialize
    /// the final freshness check and publication with target changes at cutover.
    pub(crate) async fn approval_commit_gates(
        self: &std::sync::Arc<Self>,
        attempt: u64,
    ) -> Result<Vec<tokio::sync::OwnedMutexGuard<()>>> {
        let modules = self
            .on_blocking(move |owner| owner.lock()?.gate.approval_gate_modules(attempt))
            .await?;
        let mut guards = Vec::with_capacity(modules.len());
        for module in modules {
            guards.push(self.module_gate(&module)?.lock_owned().await);
        }
        Ok(guards)
    }

    pub fn finish_approval(
        &self,
        attempt: u64,
        facts: &ApprovalFacts,
        reply: ApprovalReply,
        now_ms: u64,
    ) -> Result<u64> {
        // In-process approvals have no reader reservation. Wire approvals pass
        // their exact request token into the publication job instead.
        self.finish_approval_with_commit_window(
            attempt,
            facts,
            reply,
            now_ms,
            None,
            ApprovalRequest::for_attempt(attempt),
        )
    }

    pub(crate) fn finish_approval_with_commit_window(
        &self,
        attempt: u64,
        facts: &ApprovalFacts,
        reply: ApprovalReply,
        now_ms: u64,
        window: Option<std::sync::Arc<ApprovalCommitWindow>>,
        cancellation: std::sync::Arc<ApprovalRequest>,
    ) -> Result<u64> {
        if cancellation.attempt() != attempt {
            return Err(GateError::new(
                "config_unavailable",
                "approval request witness does not match its attempt",
            ));
        }
        let mut state = self.lock()?;
        let digest = state.gate.approval_digest(attempt)?;
        state.reviewed(&digest, Instant::now());
        let modules = state.gate.approval_modules(attempt)?;
        let program_keys = state.gate.plan_program_keys(&digest)?;
        // Reopen inputs after the operator's interactive confirmation, immediately
        // before freshness checking and atomic publication of the saved plan.
        let current = match Self::check_authority(&mut state, true).and_then(|_| {
            let mut current = self
                .capture_modules(&mut state, &modules)
                .map_err(|_| GateError::stale())?;
            current
                .capture_programs(&program_keys)
                .map_err(|_| GateError::stale())?;
            Ok(current)
        }) {
            Ok(current) => current,
            Err(error) => {
                state.gate.cancel_attempt(attempt);
                return Err(error);
            }
        };
        let facts = state.with_desktop_view(&digest, facts);
        self.drain_canceled_approvals(&mut state);
        self.drain_disconnections(&mut state);
        #[cfg(test)]
        if let Some(hook) = state.after_approval_drain_hook.take() {
            hook();
        }
        let was_unstamped = state.store.status().state == StoreState::Unstamped;
        let was_recovery = state.recovery_exit_hold
            || matches!(
                state.store.status().state,
                StoreState::StoreMissing | StoreState::StoreUnverified
            );
        let unstamp_hints = recovery::changed_file_views(self, &mut state, &current);
        #[cfg(test)]
        let publication_hook = state.publication_hook.take();
        #[cfg(test)]
        let prepublication_hook = state.prepublication_hook.take();
        let OwnerState {
            gate,
            store,
            route_policy_receipts,
            installed_route_policies,
            delivery,
            ..
        } = &mut *state;
        let result = gate.finish_approval_with_precommit(
            attempt,
            reply,
            store,
            &current.diagnostic,
            &facts,
            now_ms,
            || {
                #[cfg(test)]
                if let Some(hook) = prepublication_hook {
                    hook();
                }
                if let Some(window) = window {
                    window.begin_publication()?;
                }
                {
                    // Serialize the publication claim with cancellation-index
                    // health, then release the index before any store work.
                    let _index = self.approval_request_index.lock().map_err(|_| {
                        GateError::new(
                            "config_unavailable",
                            "approval cancellation index is poisoned",
                        )
                    })?;
                    cancellation.claim_publication()?;
                }
                route_policy::restore_superseded(
                    route_policy_receipts,
                    installed_route_policies,
                    &modules,
                    delivery,
                );
                #[cfg(test)]
                if let Some(hook) = publication_hook {
                    hook();
                }
                Ok(())
            },
        );
        let stamped_files = (was_unstamped || was_recovery)
            && state.store.body().is_ok_and(|body| {
                body.generations
                    .get(&body.last_generation)
                    .is_some_and(|entry| {
                        entry.generation.plan_digest == digest
                            && entry.kind == crate::config_store::CommitKind::Stamp
                    })
            });
        if result.is_ok() {
            state.plan_deadlines.remove(&digest);
            state.retained_plan_files.remove(&digest);
            state.desktop_views.retain(|_, (shown, _)| shown != &digest);
        }
        let unstamped = result.is_ok() && state.store.status().state == StoreState::Unstamped;
        let committed_exit = state
            .store
            .loaded_admission_state()
            .1
            .generations
            .iter()
            .next_back()
            .filter(|(_, entry)| {
                entry.generation.plan_digest == digest
                    && (was_recovery
                        || state.store.loaded_admission_state().1.stamped
                        || entry.kind == crate::config_store::CommitKind::Unstamp)
            })
            .map(|(generation, entry)| {
                (
                    *generation,
                    entry.kind == crate::config_store::CommitKind::Unstamp,
                )
            });
        if let Some((generation, unstamp)) = committed_exit {
            state.authority_exit = Some(generation);
            state.recovery_exit_hold = was_recovery;
            if was_recovery || unstamp {
                state
                    .delivery
                    .reset_for_authority_exit(if unstamp { generation } else { 0 });
            }
            if unstamp {
                state.reconciled_entries.clear();
                state.unstamp_hints.extend(unstamp_hints);
            }
        } else if result.is_ok() {
            state.authority_exit = None;
        }
        if stamped_files {
            state.stamp_channel_cutover = true;
            let connections: Vec<_> = state.active_declarations.values().copied().collect();
            state.delivery.remember_stamp_connections(connections);
        }
        drop(state);
        if unstamped {
            self.retire_unstamped_bindings();
        }
        result
    }

    #[cfg(all(test, unix))]
    pub(crate) fn set_publication_hook(&self, hook: impl FnOnce() + Send + 'static) {
        self.inner.lock().unwrap().publication_hook = Some(Box::new(hook));
    }

    #[cfg(all(test, unix))]
    pub(crate) fn set_prepublication_hook(&self, hook: impl FnOnce() + Send + 'static) {
        self.inner.lock().unwrap().prepublication_hook = Some(Box::new(hook));
    }

    #[cfg(all(test, unix))]
    pub(crate) fn set_after_approval_drain_hook(&self, hook: impl FnOnce() + Send + 'static) {
        self.inner.lock().unwrap().after_approval_drain_hook = Some(Box::new(hook));
    }

    #[cfg(all(test, unix))]
    pub(crate) fn approval_request_count_for_test(&self) -> usize {
        self.approval_request_index.lock().unwrap().len()
    }

    #[cfg(all(test, unix))]
    pub(crate) fn poison_approval_request_index_for_test(&self) {
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = self.approval_request_index.lock().unwrap();
            panic!("injected approval index poison");
        }));
        assert!(poisoned.is_err());
    }

    #[cfg(all(test, unix))]
    pub(crate) fn pause_next_approval_claim_for_test(
        &self,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        let (reached, observed) = tokio::sync::oneshot::channel();
        let (release, wait) = tokio::sync::oneshot::channel();
        *self.approval_claim_pause.lock().unwrap() = Some(ApprovalClaimPause {
            reached,
            release: wait,
        });
        (observed, release)
    }

    #[cfg(all(test, unix))]
    pub(crate) async fn pause_approval_claim_for_test(&self) {
        let pause = self.approval_claim_pause.lock().unwrap().take();
        if let Some(pause) = pause {
            let _ = pause.reached.send(());
            pause.release.await.unwrap();
        }
    }

    // Flipping a flag is platform-neutral. Its callers are the unix-only phone
    // tests and, under test-support, the ckdev config test seam and its tests.
    #[cfg(any(all(test, unix), feature = "test-support"))]
    pub(crate) fn set_os_prompt_policy_for_tests(&self) {
        self.os_prompt_policy
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn os_prompt_policy_for_tests(&self) -> bool {
        self.os_prompt_policy
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn status(&self) -> Result<subc_control::config::ConfigStatus> {
        let mut state = self.lock()?;
        let mut current_snapshot = None;
        state.retry_anchor(true);
        let verified = state.store.check().is_ok();
        if verified
            && !state.recovery_exit_hold
            && !state.store.status().anchor_pending
            && matches!(
                state.store.status().state,
                StoreState::Stamped | StoreState::Unstamped
            )
        {
            let captured = self.capture(&mut state)?;
            current_snapshot = Some(captured.diagnostic.clone());
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
        if state.recovery_exit_hold {
            if store_status.state != StoreState::Stamped {
                status.state = "store_unverified".into();
            }
            status
                .warnings
                .push("approved recovery exit is waiting for reconciliation".into());
        }
        status.integrity = match store_status.integrity {
            crate::config_store::IntegrityMode::DigestOnly => "digest_only",
            crate::config_store::IntegrityMode::Sealed => "sealed",
        }
        .into();
        status.revision = store_status.revision;
        status.base_revision = store_status.base_revision;
        status.anchor_pending = store_status.anchor_pending;
        status.ungated_inputs = self.startup_overrides.clone();
        #[cfg(feature = "test-support")]
        if state.store.uses_config_gate_test_seam() {
            status
                .development_switches
                .push("SUBC_TEST_CONFIG_GATE".into());
        }
        status.warnings.extend(store_status.warnings);
        if cfg!(feature = "dev-desktop-approval") {
            status
                .development_switches
                .push("dev-desktop-approval".into());
        }
        if cfg!(feature = "dev-placement") {
            status.development_switches.push("dev-placement".into());
        }
        if let Ok(body) = state.store.body() {
            if body.stamped {
                current_snapshot = super::GateSnapshot::from_store(&body.approved).ok();
            }
            for entry in body
                .generations
                .values()
                .rev()
                .take_while(|entry| entry.kind == crate::config_store::CommitKind::Development)
            {
                status.warnings.push(format!(
                    "development generation {}: development placement (no approval)",
                    entry.generation.id
                ));
            }
            if let Ok(snapshot) = super::GateSnapshot::from_store(&body.approved) {
                for (key, input) in &snapshot.inputs {
                    if let (
                        super::InputKey::Program { module, .. },
                        subc_config::RecordedValue::Value { value },
                    ) = (key, &input.value)
                    {
                        if value.get("coverage").and_then(serde_json::Value::as_str)
                            == Some("interpreter only covered")
                        {
                            status
                                .warnings
                                .push(format!("{module}: interpreter only covered"));
                        }
                    }
                }
            }
            status.approval_channels = body
                .generations
                .iter()
                .map(|(id, entry)| {
                    subc_control::config::ConfigApprovalRecord::new(
                        *id,
                        entry.generation.approval_channel,
                    )
                })
                .chain(body.pruned.iter().map(|(id, entry)| {
                    subc_control::config::ConfigApprovalRecord::new(*id, entry.approval_channel)
                }))
                .collect();
            status
                .approval_channels
                .sort_by_key(|record| record.generation);
            status.pending_proposals = u64::from(body.proposal.is_some());
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
        if let Some(snapshot) = current_snapshot {
            status_inputs::report_snapshot(&snapshot, &mut status);
        }
        status.gated_modules = admission::AdmissionCache::from_state(&state).held_modules();
        if state.store.loaded_admission_state().0 == StoreState::Stamped {
            let declarations: Vec<_> = state
                .declarations
                .iter()
                .map(|(connection, (module, _))| (*connection, module.clone()))
                .collect();
            for (connection, module) in declarations {
                // Use get's approved-declaration/view checks without opening the
                // store or binding a delivery attempt merely to display status.
                if self
                    .module_get_locked(
                        &mut state,
                        connection,
                        &module,
                        subc_protocol::config::ConfigGetRequest::new(),
                        false,
                    )
                    .is_err_and(|error| error.code == "config_gated")
                {
                    status.gated_modules.push(module);
                }
            }
            status.gated_modules.sort();
            status.gated_modules.dedup();
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

    /// Read the saved rollback, never today's edited files. This is not spawn
    /// authorization: admission must select an eligible module generation and
    /// launch exactly the returned identity under that module's gate lock.
    /// Re-check the file identity at spawn and refuse `plan_stale` if it changed
    /// since approval. Path-changing rollback plans remain refused until the
    /// supervisor uses these approved inputs to select and launch programs at
    /// the restored paths.
    pub fn approved_rollback_commands(&self, generation: u64) -> Result<ApprovedRollbackCommands> {
        let mut state = self.lock()?;
        state.store.delivery_ready()?;
        let configuration = state.store.rollback_configuration(generation)?;
        let entry = &state.store.body()?.generations[&generation];
        if entry.kind != crate::config_store::CommitKind::Rollback {
            return Err(GateError::new(
                "rollback_generation_required",
                "generation is not an approved rollback",
            ));
        }
        let daemon = entry
            .delivery
            .as_ref()
            .and_then(|snapshot| snapshot.daemon.clone())
            .ok_or_else(|| {
                GateError::new(
                    "rollback_snapshot_unavailable",
                    "approved rollback command input is absent",
                )
            })?;
        let snapshot = super::GateSnapshot::from_store(&configuration)?;
        let mut programs = BTreeMap::new();
        for (key, identity) in snapshot.inputs {
            if let super::InputKey::Program { module, path } = key {
                if programs
                    .insert(module, ApprovedRollbackProgram { path, identity })
                    .is_some()
                {
                    return Err(GateError::new(
                        "rollback_snapshot_unavailable",
                        "rollback contains conflicting program identities",
                    ));
                }
            }
        }
        Ok(ApprovedRollbackCommands {
            generation,
            daemon,
            programs,
        })
    }
}
