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
    capture_plan::{self, CapturePlan, ModuleRoots, ProjectCapture},
    ApprovalChallenge, ApprovalFacts, ApprovalReply, ConfigGate, GateError, PlanKind, Result,
};
use crate::{
    config_store::{ConfigStore, DeliverySnapshot, StoreActivity, StoreState},
    ConnectionId,
};

#[path = "edits.rs"]
pub(super) mod edits;

#[cfg(test)]
type UserFileTestIdle =
    tokio::sync::mpsc::UnboundedSender<(ConnectionId, tokio::sync::oneshot::Sender<()>)>;

#[cfg(test)]
pub(crate) type SettingsProjectionHook = Box<dyn FnMut() -> Result<()> + Send>;

pub struct RuntimeOwner {
    #[cfg(test)]
    pub(crate) user_file_test_idle: Mutex<Option<UserFileTestIdle>>,
    #[cfg(test)]
    config_park_hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    #[cfg(test)]
    pub(crate) content_kill_hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    #[cfg(test)]
    polled_capture_hook: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    #[cfg(test)]
    pub(crate) settings_projection_hook: Mutex<Option<SettingsProjectionHook>>,
    #[cfg(test)]
    pub(crate) settings_status_hook: Mutex<Option<SettingsProjectionHook>>,
    #[cfg(test)]
    pub(crate) settings_observation_hook: Mutex<Option<Box<dyn FnMut() + Send>>>,
    #[cfg(test)]
    polled_capture_count: std::sync::atomic::AtomicUsize,
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
    /// Set after state that `published` derives from changed without an
    /// `OwnerGuard` (a disconnect or cancellation taken with `try_lock`, or a
    /// phone withdrawal ending). The next guard release republishes.
    projection_stale: std::sync::atomic::AtomicBool,
    #[cfg(test)]
    projection_builds: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    owner_guards: std::sync::atomic::AtomicUsize,
    /// Connections with a running `config.review` delivery worker. Closing
    /// one must wake the delivery coordinator so it retires that worker.
    config_workers: Mutex<std::collections::HashSet<ConnectionId>>,
    configured_modules: std::sync::RwLock<Option<admission::ConfiguredModuleSnapshot>>,
    inner: Mutex<OwnerState>,
    capturer: ConfigCapturer,
    // Enable with the public project-settings capability. Until then an
    // unscoped config.plan must not return project targets or diagnostics.
    project_capture_enabled: bool,
    startup_overrides: Vec<String>,
    module_gates: Mutex<HashMap<String, std::sync::Arc<tokio::sync::Mutex<()>>>>,
    delivery_changed: std::sync::Arc<tokio::sync::Notify>,
    settings_changed: tokio::sync::watch::Sender<u64>,
    route_policy_authority: std::sync::OnceLock<std::sync::Arc<dyn RoutePolicyAuthority>>,
    mark_publisher: std::sync::OnceLock<std::sync::Arc<dyn mark::MarkPublisher>>,
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

fn layer_file_rows(
    changeset: &mut Changeset,
    old: &super::GateSnapshot,
    new: &super::GateSnapshot,
    previous: Option<&DeliverySnapshot>,
    candidate: &DeliverySnapshot,
) -> Result<()> {
    for key in old.changed_keys(new) {
        let super::InputKey::Layer { module, path } = key else {
            continue;
        };
        // A value or source row already explains the changed layer. Otherwise
        // expose the exact raw input without disclosing its contents.
        if changeset.fields.iter().any(|field| {
            field.module_id == *module
                && field.project_id.is_none()
                && (field.old_source == subc_config::Layer::User
                    || field.new_source == subc_config::Layer::User)
        }) {
            continue;
        }
        let describe = |snapshot: Option<&DeliverySnapshot>,
                        input: Option<&super::CapturedInput>|
         -> Result<serde_json::Value> {
            if input.is_none()
                || input.is_some_and(|input| input.state == subc_config::InputState::Absent)
            {
                return Ok(serde_json::json!({"absent":true}));
            }
            let bytes = snapshot
                .and_then(|snapshot| snapshot.modules.get(module))
                .and_then(|view| {
                    view.layers
                        .iter()
                        .find(|layer| layer.path.as_deref() == Some(path.as_str()))
                })
                .and_then(|layer| layer.bytes.as_deref())
                .ok_or_else(|| GateError::new("config_gated", "retained layer bytes are absent"))?;
            Ok(
                serde_json::json!({"sha256":subc_config::raw_digest(bytes).as_str().trim_start_matches("sha256:"), "bytes":bytes.len()}),
            )
        };
        changeset.policy_changes.push(serde_json::json!({
            "kind":"layer_file", "module_id":module, "layer":"user",
            "old":describe(previous, old.inputs.get(key))?,
            "new":describe(Some(candidate), new.inputs.get(key))?,
        }));
    }
    Ok(())
}

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
                    .or_insert_with(|| serde_json::json!({"origin": "other"}));
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
    authority_incarnation: std::sync::Arc<()>,
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
        self.retry_anchor_inner(operator_command, false)
    }

    fn retry_anchor_inner(
        &mut self,
        operator_command: bool,
        verified: bool,
    ) -> Option<tokio::time::Instant> {
        if !verified {
            let _ = self.store.check();
        }
        let pending = self.store.loaded_anchor_pending();
        let events = self.anchor_retry.poll(
            pending,
            tokio::time::Instant::now(),
            operator_command,
            || {
                let result = if verified {
                    self.store.retry_verified_anchor()
                } else {
                    let result = self.store.retry_anchor();
                    let _ = self.store.check();
                    result
                };
                (
                    self.store.loaded_anchor_pending(),
                    result.err().map(|error| error.to_string()),
                )
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
        if verified {
            self.commit_verified_route_policies();
        } else {
            self.commit_anchored_route_policies();
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
    /// Reclaim idle plans. Returns whether any plan expired.
    fn expire_idle(&mut self, now: Instant) -> bool {
        let expired: Vec<_> = self
            .plan_deadlines
            .iter()
            .filter(|(digest, deadline)| **deadline <= now && !self.gate.has_open_approval(digest))
            .map(|(digest, _)| digest.clone())
            .collect();
        let any = !expired.is_empty();
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
        any
    }
    fn reviewed(&mut self, digest: &Digest, now: Instant) {
        self.plan_deadlines
            .insert(digest.clone(), now + PLAN_IDLE_TIMEOUT);
        self.expired_plans.retain(|expired| expired != digest);
    }
}

/// The owner's state, held exclusively. Releasing it republishes the polled
/// projection, admission and approved-daemon views when anything they derive
/// from could have changed: any mutable access through this guard, a store
/// check that moved the store, or a change made outside a guard that set
/// `RuntimeOwner::projection_stale`. A guard used only to read publishes
/// nothing, so read-like authority checks do not rebuild the projection.
struct OwnerGuard<'a> {
    state: std::sync::MutexGuard<'a, OwnerState>,
    owner: &'a RuntimeOwner,
    changed: bool,
}
impl std::ops::Deref for OwnerGuard<'_> {
    type Target = OwnerState;
    fn deref(&self) -> &Self::Target {
        &self.state
    }
}
impl std::ops::DerefMut for OwnerGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // Any mutable access may change a published input. Read-only store
        // checks use `OwnerGuard::store_read` so they republish only when the
        // check itself changed the store.
        self.changed = true;
        &mut self.state
    }
}

/// The parts of a store that a read-only integrity check can change and that
/// feed the published views: its state (a failed check moves it into
/// recovery), its atomic publications, its pending anchor and its key status.
fn store_witness(store: &ConfigStore) -> (StoreState, u64, bool, u64) {
    (
        store.loaded_admission_state().0,
        store.publication_sequence(),
        store.loaded_anchor_pending(),
        store.key_status_revision(),
    )
}

impl<'a> OwnerGuard<'a> {
    fn new(state: std::sync::MutexGuard<'a, OwnerState>, owner: &'a RuntimeOwner) -> Self {
        #[cfg(test)]
        owner
            .owner_guards
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Self {
            state,
            owner,
            changed: false,
        }
    }

    /// Run a store integrity read (`check`, `status`, `delivery_ready`) and
    /// mark the guard changed only if the read changed what the store
    /// publishes. Every caller still checks authority on every call.
    fn store_read<T>(&mut self, read: impl FnOnce(&mut ConfigStore) -> T) -> T {
        let before = store_witness(&self.state.store);
        let result = read(&mut self.state.store);
        if store_witness(&self.state.store) != before {
            self.changed = true;
        }
        result
    }

    /// `ConfigStore::body` without marking the guard changed when the check
    /// it performs leaves the store as it was.
    fn checked_body(
        &mut self,
    ) -> std::result::Result<&crate::config_store::StoreBody, crate::config_store::StoreError> {
        self.store_read(ConfigStore::check)?;
        Ok(self.state.store.loaded_admission_state().1)
    }

    fn check_authority_for_read(&mut self) -> Result<()> {
        let retry = self.state.anchor_retry.next();
        let pending = self.state.store.loaded_anchor_pending();
        self.store_read(ConfigStore::check)?;
        let before = store_witness(&self.state.store);
        self.state.retry_anchor_inner(true, true);
        self.changed |= store_witness(&self.state.store) != before;
        // Verification can finish writing the committed store's integrity
        // anchor or schedule its first retry. Delivery waits for that write.
        if pending != self.state.store.loaded_anchor_pending()
            || retry != self.state.anchor_retry.next()
        {
            self.owner.notify_delivery_changed();
        }
        Ok(self.state.store.usable()?)
    }

    fn publish(&mut self) {
        #[cfg(test)]
        self.owner
            .projection_builds
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        // Project committed or invalidated authority before releasing its owner.
        // Parse changed generations outside the cheap reader lock, then publish
        // only the Arc; list polling never waits for store or keychain work.
        let previous = self
            .owner
            .published
            .read()
            .map(|view| view.approved_daemon.clone())
            .unwrap_or(ApprovedDaemonView::Unavailable);
        let approved_daemon = ApprovedDaemonView::from_store(
            &self.state.store,
            self.state.recovery_exit_hold,
            &previous,
        );
        // Volatile holds follow the same lifetime as their durable records.
        // Approving another program identity retires the previous identity's
        // volatile hold, even if that previous identity is later approved again.
        let (_, body) = self.state.store.loaded_admission_state();
        let valid: std::collections::BTreeSet<_> = body
            .program_refusals
            .iter()
            .map(|refusal| (refusal.module.clone(), refusal.approved_base.clone()))
            .collect();
        self.state
            .refused_generations
            .retain(|module, base| valid.contains(&(module.clone(), base.clone())));
        let next = admission::AdmissionCache::from_state(&self.state);
        // Lower the marks this state rules out (an unstamp, an ungated
        // module, an active generation the spawn never acknowledged) before
        // the owner is released: delivery, then record, then forwarding.
        mark::sweep(self.owner, &mut self.state, &next);
        let observed = self.state.observed_snapshot.clone();
        let projection = self
            .owner
            .status_projection(&mut self.state, observed, &next);
        let settings = settings::SettingsVersion::from_state(&self.state);
        if let Ok(mut cache) = self.owner.published.write() {
            let (holds, releases) = next.transitions_from(&cache);
            let changed = next.is_recovering() != cache.is_recovering()
                || !holds.is_empty()
                || !releases.is_empty();
            let settings_changed = cache.settings.as_ref() != Some(&settings)
                || projection.status.ungated_modules != cache.projection.status.ungated_modules;
            self.state.admission_holds.extend(holds);
            self.state.admission_releases.extend(releases);
            *cache = std::sync::Arc::new(Published {
                admission: next,
                approved_daemon,
                projection,
                settings: Some(settings),
            });
            if settings_changed {
                self.owner
                    .settings_changed
                    .send_modify(|revision| *revision = revision.wrapping_add(1));
            }
            if changed {
                self.owner.delivery_changed.notify_one();
            }
        }
    }
}
impl Drop for OwnerGuard<'_> {
    fn drop(&mut self) {
        // Clear the outside-change flag even when this guard publishes anyway.
        let stale = self
            .owner
            .projection_stale
            .swap(false, std::sync::atomic::Ordering::AcqRel);
        let published = (!(self.changed || stale))
            .then(|| {
                self.owner
                    .published
                    .read()
                    .ok()
                    .map(|view| std::sync::Arc::clone(&view))
            })
            .flatten();
        match published {
            // Nothing published here changed, so the published admission is
            // what a rebuild would produce. Spawn records still carry their
            // own mark facts, so sweep them against it on every release.
            Some(published) => mark::sweep(self.owner, &mut self.state, &published.admission),
            None => self.publish(),
        }
        let Some(ledger) = self.state.activity.clone() else {
            return;
        };
        let Ok(gates) = self.owner.module_gates.try_lock() else {
            return;
        };
        let held: Vec<_> = gates
            .values()
            .filter(|gate| gate.try_lock().is_err())
            .cloned()
            .collect();
        drop(gates);
        if held.is_empty() {
            for activity in self.state.store.take_activities() {
                ledger.config_store_activity(activity);
            }
        } else if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let activities = self.state.store.take_activities();
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
    edits: Option<edits::EditPlan>,
    /// Filled by project capture only after checking the module's reply. A
    /// user-only plan leaves this absent to preserve its digest and view.
    pub(crate) projects: Option<super::targets::ProjectReviews>,
    project_capture: Option<CapturePlan>,
    pub candidate: CapturedConfiguration,
    pub bases: BTreeMap<String, ConfigView>,
    pub base_schemas: BTreeMap<String, subc_config::Schema>,
    pub kind: PlanKind,
    pub review_modules: std::collections::BTreeSet<String>,
    /// Modules in `review_modules` only because their newly approved program
    /// is active and has not yet reviewed their approved layers. A missing
    /// reviewer drops such a module from this plan instead of failing it.
    pub rereview_modules: std::collections::BTreeSet<String>,
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
    approval_digest: Digest,
    authority_incarnation: std::sync::Arc<()>,
    daemon: std::sync::Arc<ConfigLayerSnapshot>,
    changed_entries: BTreeSet<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct ApprovalIdentity {
    generation: u64,
    approval_digest: Digest,
    // Rebuilding rejected store authority from unchanged files can repeat both
    // generation and digest. Retain this token in ApprovedDaemonConfiguration and
    // each actor's approved_start_failure so old failures cannot alias new authority.
    authority_incarnation: std::sync::Arc<()>,
}

impl PartialEq for ApprovalIdentity {
    fn eq(&self, other: &Self) -> bool {
        self.generation == other.generation
            && self.approval_digest == other.approval_digest
            && std::sync::Arc::ptr_eq(&self.authority_incarnation, &other.authority_incarnation)
    }
}

impl Eq for ApprovalIdentity {}

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
#[path = "mark.rs"]
pub(crate) mod mark;
#[path = "recovery.rs"]
mod recovery;
#[path = "rollback.rs"]
mod rollback;
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
#[path = "unstamped.rs"]
pub(crate) mod unstamped;
#[path = "widening.rs"]
mod widening;
pub(crate) use published::Published;
#[path = "settings.rs"]
pub(crate) mod settings;
#[path = "settings_observation.rs"]
pub(crate) mod settings_observation;
#[path = "status_inputs.rs"]
mod status_inputs;

#[path = "runtime_capture.rs"]
mod runtime_capture;
pub(crate) use approval_request::{ApprovalRequest, ApprovalRequestGuard, CancelApprovalRequest};
pub(crate) use runtime_capture::CaptureApply;

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
        // Daemon-restart generations newer than the booted one stay deferred.
        let initial_generation = match store.booted_generation() {
            Some(booted) => booted,
            None => store.body().map(|body| body.last_generation).unwrap_or(0),
        };
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
            settings: None,
        }));
        let delivery_changed = std::sync::Arc::new(tokio::sync::Notify::new());
        let owner = Self {
            #[cfg(test)]
            user_file_test_idle: Mutex::new(None),
            #[cfg(test)]
            polled_capture_hook: Mutex::new(None),
            #[cfg(test)]
            settings_projection_hook: Mutex::new(None),
            #[cfg(test)]
            settings_status_hook: Mutex::new(None),
            #[cfg(test)]
            settings_observation_hook: Mutex::new(None),
            #[cfg(test)]
            config_park_hook: Mutex::new(None),
            #[cfg(test)]
            content_kill_hook: Mutex::new(None),
            #[cfg(test)]
            polled_capture_count: std::sync::atomic::AtomicUsize::new(0),
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
            // The first guard release below builds the initial projection.
            projection_stale: std::sync::atomic::AtomicBool::new(true),
            #[cfg(test)]
            projection_builds: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            owner_guards: std::sync::atomic::AtomicUsize::new(0),
            config_workers: Mutex::new(std::collections::HashSet::new()),
            configured_modules: std::sync::RwLock::new(None),
            inner: Mutex::new(OwnerState {
                authority_incarnation: std::sync::Arc::new(()),
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
            project_capture_enabled: false,
            startup_overrides: status_inputs::present_overrides(|name| {
                std::env::var_os(name).is_some()
            }),
            module_gates: Mutex::new(HashMap::new()),
            delivery_changed,
            settings_changed: tokio::sync::watch::channel(0).0,
            route_policy_authority: std::sync::OnceLock::new(),
            mark_publisher: std::sync::OnceLock::new(),
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
        // The owner's state comes before the forwarding lock: lowering a
        // first-party mark takes the forwarding lock under it.
        crate::forwarding::assert_forwarding_unlocked("the configuration owner's state");
        let mut guard = self
            .inner
            .lock()
            .map(|state| OwnerGuard::new(state, self))
            .map_err(|_| {
                GateError::new("config_unavailable", "configuration owner lock is poisoned")
            })?;
        // The test seam only arms a backend failure; nothing published reads it.
        #[cfg(feature = "test-support")]
        guard.state.store.refresh_test_anchor_failure();
        // Deferred cancellations, disconnects and abandoned spawns, and idle
        // plan expiry, change the owner only when they had work. Each runs on
        // every acquisition, and work done here publishes on release.
        let drained = self.drain_canceled_approvals(&mut guard.state)
            | self.drain_disconnections(&mut guard.state)
            | self.drain_abandoned_spawns(&mut guard.state)
            | guard.state.expire_idle(Instant::now());
        guard.changed |= drained;
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
            state: OwnerGuard::new(state, self),
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
        super::strict_json::check_canonical(&declaration.schema, "/schema")
            .map_err(|error| GateError::new(&error.code, format!("{module}: {}", error.detail)))?;
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
        self.notify_delivery_changed();
        Ok(())
    }

    /// Clear everything `connection` held in the configuration owner. If another
    /// task holds the owner lock, the close is queued and the coordinator is
    /// always woken to drain it, because the connection can't be checked. When
    /// the lock is free, wake only if the connection had configuration state or
    /// ran a `config.review` worker.
    pub fn disconnected(&self, connection: ConnectionId) {
        let mut held = false;
        if let Ok(mut index) = self.approval_request_index.lock() {
            index.retain(|(caller, _), request| {
                if *caller == connection {
                    request.cancel();
                    held = true;
                    false
                } else {
                    true
                }
            });
        }
        match self.inner.try_lock() {
            Ok(mut state) => {
                if state.disconnected(connection) {
                    held = true;
                    // This change bypassed an OwnerGuard. Flag it before the
                    // owner is released so the next guard republishes it.
                    self.projection_stale
                        .store(true, std::sync::atomic::Ordering::Release);
                }
            }
            Err(_) => {
                if let Ok(mut pending) = self.disconnected_pending.lock() {
                    pending.push(connection);
                }
                // Without the owner lock we cannot rule out config state.
                // The coordinator's next acquisition applies the queued close.
                held = true;
            }
        }
        if held || self.runs_config_worker(connection) {
            self.notify_delivery_changed();
        }
    }

    /// The delivery coordinator started a `config.review` worker for
    /// `connection`.
    pub(crate) fn config_worker_started(&self, connection: ConnectionId) {
        self.config_workers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(connection);
    }

    /// The delivery coordinator retired `connection`'s worker.
    pub(crate) fn config_worker_stopped(&self, connection: ConnectionId) {
        self.config_workers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&connection);
    }

    fn runs_config_worker(&self, connection: ConnectionId) -> bool {
        self.config_workers
            .lock()
            .map_or(true, |workers| workers.contains(&connection))
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
        if let Some(project) = &request.project_id {
            if state.store.loaded_admission_state().0 == StoreState::Stamped {
                return self.stamped_project_get(
                    state,
                    module,
                    project,
                    &declaration,
                    &request,
                    check_store,
                );
            }
            return Err(GateError::new(
                "config_gated",
                format!("{module}: project-tuple delivery is not supported yet"),
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
        let approved_declaration = versions
            .pending
            .or(versions.active)
            .and_then(|generation| body.generations.get(&generation))
            .and_then(|entry| entry.delivery.as_ref())
            .and_then(|snapshot| snapshot.declarations.get(module))
            == Some(&declaration);
        if !approved_declaration && versions.active.is_none() {
            return Err(GateError::new(
                "config_gated",
                "module has no approved active configuration",
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
        if !approved_declaration {
            let mut reply = subc_protocol::config::ConfigGetReply::new(active, None);
            reply.declaration_pending = Self::declaration_pending(state, module, &declaration);
            return if check_store {
                delivery::bind_declaration_fallback(state, connection, module, &request, reply)
            } else {
                Ok(reply)
            };
        }
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

    fn declaration_pending(
        state: &OwnerState,
        module: &str,
        declaration: &ConfigDeclareRequest,
    ) -> Option<subc_protocol::config::ConfigDeclarationPending> {
        let body = state.store.loaded_admission_state().1;
        if !body.stamped {
            return None;
        }
        let versions = body
            .versions
            .iter()
            .find(|(tuple, _)| tuple.module == module && tuple.project.is_none())?
            .1
            .clone();
        if versions.active.is_none()
            || versions
                .pending
                .or(versions.active)
                .and_then(|id| body.generations.get(&id))
                .and_then(|entry| entry.delivery.as_ref())
                .and_then(|delivery| delivery.declarations.get(module))
                == Some(declaration)
        {
            return None;
        }
        let mut marker = subc_protocol::config::ConfigDeclarationPending::default();
        marker.plan_digest = state
            .gate
            .plans
            .values()
            .filter(|plan| {
                plan.modules.contains(module)
                    && plan
                        .delivery
                        .as_ref()
                        .and_then(|delivery| delivery.declarations.get(module))
                        == Some(declaration)
            })
            .max_by_key(|plan| plan.creation_sequence)
            .map(|plan| plan.view.digest.clone());
        Some(marker)
    }

    fn declarations(state: &mut OwnerState) -> Result<BTreeMap<String, ConfigDeclareRequest>> {
        if state.store.status().state == StoreState::Stamped {
            state.store.body()?;
        }
        Ok(Self::loaded_declarations(state))
    }

    fn loaded_declarations(state: &OwnerState) -> BTreeMap<String, ConfigDeclareRequest> {
        let (store, body) = state.store.loaded_admission_state();
        let mut declarations = if store == StoreState::Stamped {
            body.generations
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
        declarations
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
        self.prepare_inner(kind, None)
    }

    fn prepare_inner(
        &self,
        kind: PlanKind,
        registry: Option<(u64, Result<super::project_registry::RegistryBatch>)>,
    ) -> Result<PreparedPlan> {
        let mut state = self.lock()?;
        Self::check_authority(
            &mut state,
            matches!(kind, PlanKind::Stamp | PlanKind::Unstamp),
        )?;
        let registry = if let Some((base, reply)) = registry {
            if state.store.status().state != StoreState::Stamped
                || state.store.status().base_revision != base
            {
                return Err(capture_plan::changed());
            }
            let body = state.store.body()?;
            match reply {
                Ok(batch) => Some(batch),
                Err(error)
                    if error.code == "project_registry_unavailable"
                        && !capture_plan::known_projects(body) =>
                {
                    None
                }
                Err(error) => return Err(capture_plan::registry_retry(error)),
            }
        } else {
            None
        };
        #[cfg(test)]
        if let Some(hook) = self.prepare_capture_hook.lock().unwrap().take() {
            hook();
        }
        let (mut project_capture, observed) = if let Some((enumeration, revisions)) = registry {
            let declarations = Self::declarations(&mut state)?;
            if !declarations.keys().eq(revisions.keys()) {
                return Err(capture_plan::changed());
            }
            if let Some(enumeration) = &enumeration {
                state.store.replace_registry_membership(Ok(enumeration))?;
            }
            let mut project_plan = CapturePlan::default();
            let body = state.store.body()?;
            let approved = super::GateSnapshot::from_store(&body.approved)?;
            let roots = revisions
                .into_iter()
                .map(|(module, revision)| {
                    ModuleRoots::new(revision, &approved, &mut project_plan.skipped)
                        .map(|roots| (module, roots))
                })
                .collect::<Result<BTreeMap<_, _>>>()?;
            project_plan.context.held_user.extend(
                roots
                    .iter()
                    .filter(|(_, roots)| roots.holds_user())
                    .map(|(module, _)| module.clone()),
            );
            let held = capture_plan::retained_user_files(
                body,
                &approved,
                &declarations,
                &project_plan.context.held_user,
            );
            let captured = self
                .capturer
                .capture_with_retained_user(&declarations, None, held)?;
            let (project_plan, observed) = capture_plan::capture_projects(
                &self.capturer,
                body,
                &approved,
                &captured,
                &roots,
                project_plan,
            )?;
            state.store.observe(observed.to_store()?)?;
            state.observed_snapshot = Some(observed);
            (Some(project_plan), captured)
        } else {
            (None, self.capture(&mut state)?)
        };
        if observed.daemon.bytes.is_none() {
            return Err(GateError::new(
                "invalid_daemon_config",
                "subc.jsonc is absent; refusing to rescan",
            ));
        }
        if project_capture.is_none()
            && !state.recovery_exit_hold
            && state.store.status().state == StoreState::Stamped
        {
            let approved = super::GateSnapshot::from_store(&state.store.body()?.approved)?;
            state
                .store
                .observe(observed.proposal_snapshot(&approved).to_store()?)?;
        }
        let rollback = if let PlanKind::Rollback(id) = kind {
            Some(rollback::prepare(self, &mut state, id)?)
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
            .filter(|key| !key.is_project())
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
        let mut review_modules: BTreeSet<String> = touched_modules
            .iter()
            .filter(|module| candidate.modules.contains_key(*module))
            .filter(|module|matches!(kind,PlanKind::Stamp|PlanKind::Unstamp) || candidate.diagnostic.changed_keys(&old).iter().any(|key|matches!(key,super::InputKey::Layer {module:owner,..}|super::InputKey::Declaration(owner) if owner==*module)))
            .cloned()
            .collect();
        if let Some(projects) = &project_capture {
            touched_modules.extend(projects.reviews.keys().map(|tuple| tuple.module.clone()));
        }
        // A program change is approved launch-only: neither the build being
        // approved nor the one still running can review for it. Once the new
        // program is active, the next plan re-reviews the approved layers with
        // it. While it is not yet active the module waits, and the plan
        // carries its earlier reviewer forward.
        let mut rereview_modules = BTreeSet::new();
        if matches!(kind, PlanKind::Change | PlanKind::Rollback(_))
            && state.store.status().state == StoreState::Stamped
        {
            for (module, rereview) in
                super::reviewed_build::pending(state.store.body()?, candidate.modules.keys())?
            {
                if rereview == super::reviewed_build::Rereview::Ready
                    && !review_modules.contains(&module)
                {
                    touched_modules.insert(module.clone());
                    review_modules.insert(module.clone());
                    rereview_modules.insert(module);
                }
            }
        }
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
            let approved_id = placement::last_approved_generation(body);
            for (tuple, versions) in &body.versions {
                if touched_modules.contains(&tuple.module) {
                    active_generations.insert(tuple.clone(), versions.active);
                }
                if tuple.project.is_some() || !review_modules.contains(&tuple.module) {
                    continue;
                }
                // A plan replaces the last approval, even when that generation
                // has not activated. The reviewer and the card use that base.
                // A launch entry alone is not an approved configuration base;
                // only a retained declaration establishes one for review.
                if old
                    .inputs
                    .contains_key(&super::InputKey::Declaration(tuple.module.clone()))
                {
                    let id = approved_id;
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
                            format!("{}: approved view is absent", tuple.module),
                        )
                    })?;
                    let mut view = view.clone();
                    view.generation = id;
                    view.status = if versions.active == Some(id) {
                        ConfigStatus::Active
                    } else {
                        ConfigStatus::Pending
                    };
                    bases.insert(tuple.module.clone(), view);
                }
            }
        }
        Ok(PreparedPlan {
            edits: None,
            projects: None,
            project_capture: project_capture.take(),
            candidate,
            bases,
            base_schemas,
            kind,
            review_modules,
            rereview_modules,
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
        let project_plan = prepared.project_capture.take();
        if let Some(projects) = &project_plan {
            if !projects.reviews.is_empty() || !projects.refusals.is_empty() {
                return Err(GateError::new(
                    "module_review_invalid",
                    "project reviews were not accepted",
                ));
            }
        }
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
        let mut current = if prepared.edits.is_some() {
            self.edit_baseline(&mut state, Some(&prepared.touched_modules))
        } else if let Some(projects) = &project_plan {
            self.capture_with_project_context(
                &mut state,
                &prepared.touched_modules,
                &projects.context,
            )
        } else {
            self.capture_modules(&mut state, &prepared.touched_modules)
        }
        .map_err(|_| GateError::stale())?;
        current
            .capture_programs(&prepared.program_keys)
            .map_err(|_| GateError::stale())?;
        if (prepared.edits.is_none()
            && state.store.status().base_revision != prepared.base_revision)
            || active_generations != prepared.active_generations
            || !prepared
                .read_set
                .matches(&current.diagnostic.read_set(&prepared.touched_modules)?)
        {
            return Err(if prepared.edits.is_some() {
                GateError::new(
                    "config_changed_since_review",
                    "a touched module changed while the edits were reviewed",
                )
            } else {
                GateError::stale()
            });
        }
        if prepared.edits.is_some() {
            // Carry forward approvals for modules outside the edit set. Changes
            // to those approvals must neither refuse these edits nor be reverted by them.
            let touched = &prepared.touched_modules;
            for (key, input) in &current.diagnostic.inputs {
                if key.module().is_none_or(|module| !touched.contains(module)) {
                    prepared
                        .candidate
                        .diagnostic
                        .inputs
                        .insert(key.clone(), input.clone());
                }
            }
            prepared.candidate.diagnostic.inputs.retain(|key, _| {
                key.module().is_some_and(|module| touched.contains(module))
                    || current.diagnostic.inputs.contains_key(key)
            });
            prepared
                .candidate
                .modules
                .retain(|module, _| touched.contains(module));
            for (module, captured) in std::mem::take(&mut current.modules) {
                if !touched.contains(&module) {
                    prepared.candidate.modules.insert(module, captured);
                }
            }
        }
        let mut project_context = None;
        let mut project_skips = Vec::new();
        if let Some(projects) = project_plan {
            let CapturePlan {
                mut context,
                witnesses,
                skipped,
                files,
                ..
            } = projects;
            for (key, input) in &witnesses.inputs {
                if matches!(key, super::InputKey::ProjectTuple { .. }) {
                    context.witnesses.inputs.insert(key.clone(), input.clone());
                }
            }
            current
                .diagnostic
                .inputs
                .extend(context.witnesses.inputs.clone());
            prepared
                .candidate
                .diagnostic
                .inputs
                .extend(witnesses.inputs);
            if !files.is_empty() {
                prepared.projects.get_or_insert_with(Default::default).files = files;
            }
            project_skips = skipped;
            project_context = Some(context);
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
            ..DeliverySnapshot::default()
        };
        let mut handles = prepared.candidate.take_handles();
        handles.append(&mut prepared.observed_handles);
        let previous_delivery = if state.store.status().state == StoreState::Stamped {
            let body = state.store.body()?;
            body.generations
                .get(&placement::last_approved_generation(body))
                .and_then(|entry| entry.delivery.clone())
        } else if prepared.edits.as_ref().is_some_and(|edit| edit.unstamped) {
            Some(DeliverySnapshot {
                daemon: delivery.daemon.clone(),
                modules: prepared.bases.clone(),
                declarations: delivery.declarations.clone(),
                ..DeliverySnapshot::default()
            })
        } else {
            None
        };
        // Activation policy compares the candidate with what is running, not
        // with the last approval used by the visible field rows.
        let mut active_fields = Vec::new();
        for (module, captured) in prepared.candidate.modules.iter().filter(|(module, _)| {
            // Stamping records the inputs already running; unstamping returns
            // authority to files. Neither operation changes runtime values.
            matches!(prepared.kind, PlanKind::Change | PlanKind::Rollback(_))
                && prepared.touched_modules.contains(*module)
        }) {
            let active = if state.store.status().state == StoreState::Stamped {
                let body = state.store.body()?;
                body.versions
                    .iter()
                    .find(|(tuple, _)| tuple.module == *module && tuple.project.is_none())
                    .and_then(|(_, versions)| versions.active)
                    .and_then(|id| body.generations.get(&id))
                    .and_then(|entry| entry.delivery.as_ref())
            } else {
                None
            };
            let base = active.and_then(|snapshot| snapshot.modules.get(module));
            let schema = active
                .and_then(|snapshot| snapshot.declarations.get(module))
                .map(|declaration| subc_config::Schema::new(declaration.schema.clone()))
                .transpose()
                .map_err(GateError::invalid)?;
            active_fields.extend(
                super::review::expected_fields(
                    module,
                    &captured.schema,
                    base,
                    schema.as_ref(),
                    &captured.view,
                )?
                .into_values(),
            );
        }
        let OwnerState { gate, store, .. } = &mut *state;
        let mut view = gate.plan_reviewed_inner(
            store,
            prepared.kind,
            prepared.candidate.diagnostic,
            &current.diagnostic,
            super::ReviewRecord::Reviewed(&prepared.review_modules),
            prepared.edits,
            prepared.projects,
            |old, new| {
                let mut changeset = review(old, new)?;
                layer_file_rows(
                    &mut changeset,
                    old,
                    new,
                    previous_delivery.as_ref(),
                    &delivery,
                )?;
                let mut activation = changeset.clone();
                activation.fields = active_fields;
                super::apply::bind_plan(&mut activation)?;
                if !changeset.apply_plan.is_object() {
                    changeset.apply_plan = if changeset.apply_plan.is_null() {
                        serde_json::json!({})
                    } else {
                        serde_json::json!({"review": changeset.apply_plan})
                    };
                }
                changeset.apply_plan["module_transitions"] =
                    activation.apply_plan["module_transitions"].clone();
                Ok(changeset)
            },
        )?;
        view.skipped = project_skips;
        if let Some(plan) = gate.plans.get_mut(&view.digest) {
            plan.project_capture = project_context;
            plan.view.skipped.clone_from(&view.skipped);
        }
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
        let projects = state.gate.plans[digest].project_capture.clone();
        let mut current = if state
            .gate
            .plans
            .get(digest)
            .is_some_and(|plan| plan.edits.is_some())
        {
            self.edit_baseline(&mut state, Some(&modules))
        } else if let Some(capture) = &projects {
            self.capture_with_project_context(&mut state, &modules, capture)
        } else {
            self.capture_modules(&mut state, &modules)
        }
        .map_err(|_| GateError::stale())?;
        if let Some(capture) = projects {
            current.diagnostic.inputs.extend(capture.witnesses.inputs);
        }
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
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn finish_approval_with_commit_window(
        &self,
        attempt: u64,
        facts: &ApprovalFacts,
        reply: ApprovalReply,
        now_ms: u64,
        window: Option<std::sync::Arc<ApprovalCommitWindow>>,
        cancellation: std::sync::Arc<ApprovalRequest>,
        project_apply: Option<CaptureApply>,
    ) -> Result<u64> {
        if cancellation.attempt() != attempt {
            return Err(GateError::new(
                "config_unavailable",
                "approval request witness does not match its attempt",
            ));
        }
        let mut state = self.lock()?;
        let digest = state.gate.approval_digest(attempt)?;
        let project_capture = state.gate.plans[&digest].project_capture.clone();
        if let Some(capture) = &project_capture {
            if !capture.authority.is_empty()
                && project_apply
                    .as_ref()
                    .is_none_or(|apply| apply.digest != digest)
            {
                return Err(capture_plan::changed());
            }
            if state.store.status().base_revision
                != state.gate.plans[&digest].view.plan.base_revision
            {
                return Err(capture_plan::changed());
            }
        }
        if let Some(enumeration) = project_apply.and_then(|apply| apply.enumeration) {
            state.store.replace_registry_membership(Ok(&enumeration))?;
        }
        let declarations_only = state
            .gate
            .plans
            .get(&digest)
            .is_some_and(|plan| plan.development_approval.is_some());
        state.reviewed(&digest, Instant::now());
        let modules = state.gate.approval_modules(attempt)?;
        let user_modules = state.gate.plans[&digest]
            .targets()?
            .into_iter()
            .filter(|tuple| tuple.project.is_none())
            .map(|tuple| tuple.module)
            .collect();
        let program_keys = state.gate.plan_program_keys(&digest)?;
        // Reopen inputs after the operator's interactive confirmation, immediately
        // before freshness checking and atomic publication of the saved plan.
        let edit_plan = state
            .gate
            .plans
            .get(&digest)
            .and_then(|plan| plan.edits.clone());
        let current = match Self::check_authority(&mut state, true).and_then(|_| {
            let mut current = if edit_plan.is_some() {
                self.edit_baseline(&mut state, Some(&modules))
            } else if let Some(capture) = &project_capture {
                self.capture_with_project_context(&mut state, &modules, capture)
            } else {
                self.capture_modules(&mut state, &modules)
            }
            .map_err(|_| {
                if project_capture.is_some() {
                    capture_plan::changed()
                } else {
                    GateError::stale()
                }
            })?;
            if let Some(capture) = &project_capture {
                current
                    .diagnostic
                    .inputs
                    .extend(capture.witnesses.inputs.clone());
            }
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
        let publication_before = state.store.publication_sequence();
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
                if !declarations_only {
                    route_policy::restore_superseded(
                        route_policy_receipts,
                        installed_route_policies,
                        &user_modules,
                        delivery,
                    );
                }
                #[cfg(test)]
                if let Some(hook) = publication_hook {
                    hook();
                }
                Ok(())
            },
        );
        let committed_this_call = state.store.publication_sequence() != publication_before;
        if committed_this_call {
            if let Some(edit_plan) = edit_plan {
                for file in &edit_plan.files {
                    let _ = file.write_back();
                }
                // Publication already succeeded. Refresh drift after reconciliation
                // without turning a failed diagnostic capture into a failed approval.
                let _ = self.observe_proposal(&mut state);
            }
        }
        let stamped_files = committed_this_call
            && (was_unstamped || was_recovery)
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
                committed_this_call
                    && entry.generation.plan_digest == digest
                    && entry.kind != crate::config_store::CommitKind::DeclarationApproval
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
        if committed_this_call && was_recovery {
            state.authority_incarnation = std::sync::Arc::new(());
        }
        if let Some((generation, unstamp)) = committed_exit {
            state.authority_exit = Some(generation);
            state.recovery_exit_hold = was_recovery;
            if was_recovery || unstamp {
                state
                    .delivery
                    .reset_for_authority_exit(if unstamp { generation } else { 0 });
            }
            if unstamp {
                state.stamp_channel_cutover = false;
                state.reconciled_entries.clear();
                state.unstamp_hints.extend(unstamp_hints);
            }
        } else if committed_this_call {
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
        let mut observation = None;
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
            observation = Some(self.observe_proposal(&mut state)?);
            current_snapshot = state.observed_snapshot.clone();
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
        status.root_io = self.capturer.projects.root_io_health();
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
        #[cfg(feature = "dev-desktop-approval")]
        if let Some(floor) = crate::operator_apps::development_floor_diagnostic() {
            status.development_switches.push(floor);
        }
        if cfg!(feature = "dev-placement") {
            status.development_switches.push("dev-placement".into());
        }
        let declared: Vec<String> = Self::declarations(&mut state)
            .map(|declarations| declarations.into_keys().collect())
            .unwrap_or_default();
        if let Ok(body) = state.store.body() {
            if body.stamped {
                for (module, rereview) in
                    super::reviewed_build::pending(body, &declared).unwrap_or_default()
                {
                    status.warnings.push(match rereview {
                        super::reviewed_build::Rereview::WaitingForProgram => format!(
                            "module {module}: needs configuration re-review: its approved program has not reviewed its approved layers; the re-review waits until that program is running and active"
                        ),
                        super::reviewed_build::Rereview::Ready => format!(
                            "module {module}: needs configuration re-review: its approved program has not reviewed its approved layers; the next configuration plan re-reviews them with the running program"
                        ),
                    });
                }
            }
            if body.stamped {
                current_snapshot = super::GateSnapshot::from_store(&body.approved).ok();
            }
            for entry in body.generations.values().rev().take_while(|entry| {
                matches!(
                    entry.kind,
                    crate::config_store::CommitKind::Development
                        | crate::config_store::CommitKind::DeclarationApproval
                )
            }) {
                if entry.kind != crate::config_store::CommitKind::Development {
                    continue;
                }
                status.warnings.push(format!(
                    "development generation {}: development placement (no approval)",
                    entry.generation.id
                ));
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
        if let Some(captured) = observation {
            captured.report_errors(&mut status);
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
                let reply = self.module_get_locked(
                    &mut state,
                    connection,
                    &module,
                    subc_protocol::config::ConfigGetRequest::new(),
                    false,
                );
                if let Ok(reply) = &reply {
                    Self::report_declaration_pending(
                        &state,
                        connection,
                        &module,
                        reply,
                        &mut status,
                    );
                }
                if reply.is_err_and(|error| error.code == "config_gated") {
                    status.gated_modules.push(module);
                }
            }
            status.gated_modules.sort();
            status.gated_modules.dedup();
        }
        Ok(status)
    }

    fn report_declaration_pending(
        state: &OwnerState,
        connection: ConnectionId,
        module: &str,
        reply: &subc_protocol::config::ConfigGetReply,
        status: &mut subc_control::config::ConfigStatus,
    ) {
        if state.active_declarations.get(module) != Some(&connection) {
            return;
        }
        if let Some(marker) = &reply.declaration_pending {
            status
                .declaration_pending
                .insert(module.into(), marker.clone());
            if let Some(generation) = state.delivery.fallback_applied.get(&connection) {
                if reply
                    .active
                    .as_ref()
                    .is_some_and(|view| view.generation == *generation)
                {
                    status.warnings.push(format!("module {module}: running last approved settings (generation {generation}); declaration awaits approval"));
                }
            }
        }
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
