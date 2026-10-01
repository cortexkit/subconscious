use std::{
    collections::{HashMap, VecDeque},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, MutexGuard,
    },
    time::Duration,
};

use serde_json::{json, Value};
use tracing::debug;

use crate::registry::ConnectionId;

/// The only keys the route.open refusal counter may carry. A module's own
/// error code on a rejected bind is counted under `module_rejected` and rides
/// the log event as a separate field: if the module's string were the key, a
/// module could grow this map for the daemon's lifetime and push terminal
/// control sequences through `ck daemon` to an operator's screen. The
/// `&'static str` increment signature plus the debug assertion keep the set
/// closed at the call site, not just here.
const ROUTE_OPEN_REFUSAL_COUNTER_CODES: &[&str] = &[
    "module_warming",
    ROUTE_OPEN_REFUSED_DECLARED_NOT_READY,
    ROUTE_OPEN_REFUSED_REQUIRED_CAPABILITY_UNPROVIDED,
    "target_unavailable",
    "module_removed",
    "module_no_protocol",
    "unknown_module",
    "module_reloading",
    "op_not_allowed",
    "bad_consumer_identity",
    "capability_forbidden",
    "admission_facts_not_permitted",
    "admission_facts_target_not_allowed",
    "route_limit",
    "forwarding_error",
    "module_timeout",
    ROUTE_OPEN_REFUSED_BREAKER_OPEN,
    "module_rejected",
    subc_protocol::error_codes::SCOPE_EPOCH_REQUIRED,
    subc_protocol::error_codes::SCOPE_NOT_SYNCED,
    subc_protocol::error_codes::SCOPE_NOT_LIVE,
    subc_protocol::error_codes::SCOPE_ENDED,
    subc_protocol::error_codes::SCOPE_NOT_CARRIER,
    subc_protocol::error_codes::SCOPE_CHANGED,
    subc_protocol::error_codes::INVALID_REQUEST,
];

/// Counter key for a `route.open` refused by the per-module bind-relay breaker
/// before any relay was attempted.
///
/// The frame the caller receives carries `module_timeout`, because both SDKs
/// already classify that as retryable with capped backoff and inventing a new
/// wire code would need a change in each of them. The COUNTER is deliberately a
/// different key: "this module burned the full bind budget" and "this module is
/// being refused in microseconds because it already did that repeatedly" are
/// the two states an operator most needs to tell apart, and they are
/// indistinguishable from the client side, where both look like one retryable
/// error that the next attempt may well satisfy.
pub(crate) const ROUTE_OPEN_REFUSED_BREAKER_OPEN: &str = "module_timeout_breaker_open";

/// Counter key for a registered module that declared itself not ready.
///
/// The caller still receives `module_warming`, but operators must be able to
/// distinguish declared readiness from a supervised process that has not
/// registered yet.
pub(crate) const ROUTE_OPEN_REFUSED_DECLARED_NOT_READY: &str = "module_warming_declared_not_ready";

/// Counter key for a registered, declared-ready module held not-ready because
/// a capability it declares `need: required` has no registered provider.
///
/// The caller still receives `module_warming`; a separate key lets an operator
/// tell "the module says it is warming" from "the module is waiting on a
/// provider that is not running", which point at different fixes.
pub(crate) const ROUTE_OPEN_REFUSED_REQUIRED_CAPABILITY_UNPROVIDED: &str =
    "module_warming_required_capability_unprovided";

/// Shared count of authenticated socket connections accepted by the daemon.
#[derive(Debug, Clone, Default)]
pub struct ConnectedClients {
    count: Arc<AtomicU64>,
}

impl ConnectedClients {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn count(&self) -> u64 {
        self.count.load(Ordering::SeqCst)
    }

    pub(crate) fn open(&self, connection_id: ConnectionId) -> ConnectedClientGuard {
        let previous = self.count.fetch_add(1, Ordering::SeqCst);
        let current = previous + 1;
        // DEBUG, not INFO: this pair fired on every connect and disconnect and was
        // measured at 43-74% of the daemon's own log (issue #114), burying the
        // lines an operator opens the file for. The count itself is not lost: it
        // is served live as `connected_clients` on server.describe (`ck daemon`),
        // and a connection that opens a route still names its connection_id on the
        // `route.open accepted` line, so per-connection forensics survive where
        // they matter. Raise the filter (`CK_LOG=subc=debug`) to see every churn.
        debug!(
            connection_id = connection_id.get(),
            connected_clients = current,
            previous_connected_clients = previous,
            "authenticated connection count changed"
        );
        ConnectedClientGuard {
            clients: self.clone(),
            connection_id,
        }
    }
}

pub(crate) struct ConnectedClientGuard {
    clients: ConnectedClients,
    connection_id: ConnectionId,
}

impl Drop for ConnectedClientGuard {
    fn drop(&mut self) {
        let previous = self.clients.count.fetch_sub(1, Ordering::SeqCst);
        let current = previous.saturating_sub(1);
        // DEBUG for the same reason as the open side above.
        debug!(
            connection_id = self.connection_id.get(),
            connected_clients = current,
            previous_connected_clients = previous,
            "authenticated connection count changed"
        );
    }
}

/// Lock-free counters for route lifecycle drops and delivery failures.
#[derive(Debug, Clone, Default)]
pub struct DaemonCounters {
    /// Every non-request frame a module sent on a (channel, epoch) the daemon
    /// holds no bound route for, dropped. Counts both kinds below: orphan
    /// traffic on a route the daemon released, and frames on a (channel,
    /// epoch) that was never allocated on that module connection.
    module_frames_dropped_no_route: Arc<AtomicU64>,
    /// The orphan-traffic part of `module_frames_dropped_no_route`: the daemon
    /// had allocated that (channel, epoch) on the sending module connection and
    /// has since released it, so the module is still holding a route the
    /// daemon closed. `module_frames_dropped_no_route` minus this is the count
    /// of frames on a (channel, epoch) that never existed on the connection.
    module_frames_dropped_released_route: Arc<AtomicU64>,
    module_frames_dropped_released_route_by_module: Arc<Mutex<HashMap<String, u64>>>,
    /// Route GOODBYEs the daemon enqueued to a module in answer to its frames
    /// on a (channel, epoch) the daemon holds no route for, telling the module
    /// to drop the route. Rate-limited per module connection and channel, so
    /// this counts answers, not orphan frames.
    module_orphan_route_goodbyes_sent: Arc<AtomicU64>,
    // Per-module maps and the rate window are daemon-lifetime diagnostics only:
    // they deliberately reset on restart instead of becoming durable daemon state.
    module_frames_dropped_no_route_by_module: Arc<Mutex<HashMap<String, u64>>>,
    route_open_refused_by_code: Arc<Mutex<HashMap<String, u64>>>,
    route_open_accepted_by_principal: Arc<Mutex<HashMap<String, u64>>>,
    module_frames_dropped_no_route_window: Arc<Mutex<DropWindow>>,
    module_requests_dropped_stale_route: Arc<AtomicU64>,
    client_frames_dropped_stale_route: Arc<AtomicU64>,
    client_egress_close_delivery_failed: Arc<AtomicU64>,
    goodbye_relay_client_failed: Arc<AtomicU64>,
    goodbye_relay_module_dropped: Arc<AtomicU64>,
    goodbye_relay_module_dropped_by_module: Arc<Mutex<HashMap<String, u64>>>,
    route_released_epoch_fenced: Arc<AtomicU64>,
    route_release_stale_skipped: Arc<AtomicU64>,
    drains_with_undeclared_gauge: Arc<AtomicU64>,
}

/// Ten one-minute buckets make sustained module-to-client route drops visible
/// without retaining one record for every dropped frame.
#[derive(Debug)]
struct DropWindow {
    started_at: tokio::time::Instant,
    buckets: VecDeque<DropBucket>,
}

#[derive(Debug)]
struct DropBucket {
    minute: u64,
    count: u64,
}

impl Default for DropWindow {
    fn default() -> Self {
        Self {
            started_at: tokio::time::Instant::now(),
            buckets: VecDeque::new(),
        }
    }
}

impl DropWindow {
    const MINUTE: Duration = Duration::from_secs(60);
    const BUCKETS: u64 = 10;

    fn record(&mut self, now: tokio::time::Instant) {
        let minute = self.minute_at(now);
        self.prune_before(minute);
        match self.buckets.back_mut() {
            Some(bucket) if bucket.minute == minute => bucket.count += 1,
            _ => self.buckets.push_back(DropBucket { minute, count: 1 }),
        }
    }

    fn count_last_10m(&mut self, now: tokio::time::Instant) -> u64 {
        let minute = self.minute_at(now);
        self.prune_before(minute);
        self.buckets.iter().map(|bucket| bucket.count).sum()
    }

    fn nonzero_minutes_last_10m(&mut self, now: tokio::time::Instant) -> u64 {
        let minute = self.minute_at(now);
        self.prune_before(minute);
        self.buckets.len() as u64
    }

    fn minute_at(&self, now: tokio::time::Instant) -> u64 {
        now.saturating_duration_since(self.started_at).as_secs() / Self::MINUTE.as_secs()
    }

    fn prune_before(&mut self, current_minute: u64) {
        while self
            .buckets
            .front()
            .is_some_and(|bucket| current_minute.saturating_sub(bucket.minute) >= Self::BUCKETS)
        {
            self.buckets.pop_front();
        }
    }
}

impl DaemonCounters {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns a JSON snapshot whose stable, additive schema keeps the
    /// `server.describe` diagnostic endpoint backward-compatible.
    pub fn snapshot(&self) -> Value {
        let mut snapshot = serde_json::Map::new();
        snapshot.insert(
            "module_frames_dropped_no_route".into(),
            self.module_frames_dropped_no_route
                .load(Ordering::Relaxed)
                .into(),
        );
        let mut drop_window = self
            .module_frames_dropped_no_route_window
            .lock()
            .expect("drop-rate window mutex poisoned");
        let now = tokio::time::Instant::now();
        snapshot.insert(
            "module_frames_dropped_no_route_last_10m".into(),
            drop_window.count_last_10m(now).into(),
        );
        snapshot.insert(
            "module_frames_dropped_no_route_nonzero_minutes_last_10m".into(),
            drop_window.nonzero_minutes_last_10m(now).into(),
        );
        insert_nonempty_counts(
            &mut snapshot,
            "module_frames_dropped_no_route_by_module",
            &self.module_frames_dropped_no_route_by_module,
        );
        snapshot.insert(
            "module_frames_dropped_released_route".into(),
            self.module_frames_dropped_released_route
                .load(Ordering::Relaxed)
                .into(),
        );
        insert_nonempty_counts(
            &mut snapshot,
            "module_frames_dropped_released_route_by_module",
            &self.module_frames_dropped_released_route_by_module,
        );
        snapshot.insert(
            "module_orphan_route_goodbyes_sent".into(),
            self.module_orphan_route_goodbyes_sent
                .load(Ordering::Relaxed)
                .into(),
        );
        insert_nonempty_counts(
            &mut snapshot,
            "route_open_refused_by_code",
            &self.route_open_refused_by_code,
        );
        insert_nonempty_counts(
            &mut snapshot,
            "route_open_accepted_by_principal",
            &self.route_open_accepted_by_principal,
        );
        snapshot.insert(
            "module_requests_dropped_stale_route".into(),
            self.module_requests_dropped_stale_route
                .load(Ordering::Relaxed)
                .into(),
        );
        snapshot.insert(
            "client_frames_dropped_stale_route".into(),
            self.client_frames_dropped_stale_route
                .load(Ordering::Relaxed)
                .into(),
        );
        snapshot.insert(
            "client_egress_close_delivery_failed".into(),
            self.client_egress_close_delivery_failed
                .load(Ordering::Relaxed)
                .into(),
        );
        snapshot.insert(
            "goodbye_relay_client_failed".into(),
            self.goodbye_relay_client_failed
                .load(Ordering::Relaxed)
                .into(),
        );
        snapshot.insert(
            "goodbye_relay_module_dropped".into(),
            self.goodbye_relay_module_dropped
                .load(Ordering::Relaxed)
                .into(),
        );
        insert_nonempty_counts(
            &mut snapshot,
            "goodbye_relay_module_dropped_by_module",
            &self.goodbye_relay_module_dropped_by_module,
        );
        snapshot.insert(
            "route_released_epoch_fenced".into(),
            self.route_released_epoch_fenced
                .load(Ordering::Relaxed)
                .into(),
        );
        snapshot.insert(
            "route_release_stale_skipped".into(),
            self.route_release_stale_skipped
                .load(Ordering::Relaxed)
                .into(),
        );
        snapshot.insert(
            "drains_with_undeclared_gauge".into(),
            self.drains_with_undeclared_gauge
                .load(Ordering::Relaxed)
                .into(),
        );
        Value::Object(snapshot)
    }

    pub(crate) fn increment_module_frames_dropped_no_route(&self, module_id: Option<&str>) {
        self.module_frames_dropped_no_route
            .fetch_add(1, Ordering::Relaxed);
        if let Some(module_id) = module_id {
            increment_keyed_count(&self.module_frames_dropped_no_route_by_module, module_id);
        }
        self.module_frames_dropped_no_route_window
            .lock()
            .expect("drop-rate window mutex poisoned")
            .record(tokio::time::Instant::now());
    }

    /// Count a dropped module frame whose (channel, epoch) the daemon had
    /// allocated on that connection and since released. Called in addition to
    /// [`Self::increment_module_frames_dropped_no_route`], never instead of it.
    pub(crate) fn increment_module_frames_dropped_released_route(&self, module_id: Option<&str>) {
        self.module_frames_dropped_released_route
            .fetch_add(1, Ordering::Relaxed);
        if let Some(module_id) = module_id {
            increment_keyed_count(
                &self.module_frames_dropped_released_route_by_module,
                module_id,
            );
        }
    }

    pub(crate) fn increment_module_orphan_route_goodbyes_sent(&self) {
        self.module_orphan_route_goodbyes_sent
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn increment_route_open_refused(&self, code: &'static str) {
        debug_assert!(ROUTE_OPEN_REFUSAL_COUNTER_CODES.contains(&code));
        increment_keyed_count(&self.route_open_refused_by_code, code);
    }

    /// Count an accepted route.open by the principal the daemon stamped.
    ///
    /// THE KEY SPACE IS CLOSED BY CONSTRUCTION, unlike the refusal counter which
    /// needs an explicit allowlist: a principal is `direct` or
    /// `reserved:<module_id>`, and a module id was already refused at HELLO
    /// unless it is a single path component free of control characters. So an
    /// untrusted string cannot expand this map without first passing module-id
    /// validation, and the bound is the number of modules rather than the number
    /// of distinct strings a caller can invent.
    pub(crate) fn increment_route_open_accepted(&self, principal: &str) {
        increment_keyed_count(&self.route_open_accepted_by_principal, principal);
    }

    pub(crate) fn increment_module_requests_dropped_stale_route(&self) {
        self.module_requests_dropped_stale_route
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn increment_client_frames_dropped_stale_route(&self) {
        self.client_frames_dropped_stale_route
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn increment_client_egress_close_delivery_failed(&self) {
        self.client_egress_close_delivery_failed
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn increment_goodbye_relay_client_failed(&self) {
        self.goodbye_relay_client_failed
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn increment_goodbye_relay_module_dropped(&self, module_id: Option<&str>) {
        self.goodbye_relay_module_dropped
            .fetch_add(1, Ordering::Relaxed);
        if let Some(module_id) = module_id {
            increment_keyed_count(&self.goodbye_relay_module_dropped_by_module, module_id);
        }
    }

    pub(crate) fn increment_route_released_epoch_fenced(&self) {
        self.route_released_epoch_fenced
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn increment_route_release_stale_skipped(&self) {
        self.route_release_stale_skipped
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn increment_drains_with_undeclared_gauge(&self) {
        self.drains_with_undeclared_gauge
            .fetch_add(1, Ordering::Relaxed);
    }
}

fn increment_keyed_count(counts: &Mutex<HashMap<String, u64>>, key: &str) {
    *counts
        .lock()
        .expect("keyed counter mutex poisoned")
        .entry(key.to_string())
        .or_default() += 1;
}

fn insert_nonempty_counts(
    snapshot: &mut serde_json::Map<String, Value>,
    key: &str,
    counts: &Mutex<HashMap<String, u64>>,
) {
    let counts: MutexGuard<'_, HashMap<String, u64>> =
        counts.lock().expect("keyed counter mutex poisoned");
    if !counts.is_empty() {
        snapshot.insert(key.to_string(), json!(&*counts));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_snapshot_includes_zero_rate_and_omits_empty_module_maps() {
        let counters = DaemonCounters::new();
        let snapshot = counters.snapshot();

        assert_eq!(snapshot["module_frames_dropped_no_route_last_10m"], 0);
        assert_eq!(
            snapshot["module_frames_dropped_no_route_nonzero_minutes_last_10m"],
            0
        );
        assert!(snapshot
            .get("module_frames_dropped_no_route_by_module")
            .is_none());
        assert!(snapshot
            .get("goodbye_relay_module_dropped_by_module")
            .is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn module_frame_drops_are_attributed_to_the_emitting_module() {
        let counters = DaemonCounters::new();
        counters.increment_module_frames_dropped_no_route(Some("alpha"));
        counters.increment_module_frames_dropped_no_route(Some("alpha"));

        let snapshot = counters.snapshot();
        assert_eq!(snapshot["module_frames_dropped_no_route"], 2);
        assert_eq!(
            snapshot["module_frames_dropped_no_route_by_module"],
            json!({ "alpha": 2 })
        );
        assert_eq!(snapshot["module_frames_dropped_no_route_last_10m"], 2);
    }

    #[tokio::test(start_paused = true)]
    async fn frame_drop_rate_ages_out_after_ten_minute_buckets() {
        let counters = DaemonCounters::new();
        counters.increment_module_frames_dropped_no_route(Some("alpha"));

        tokio::time::advance(Duration::from_secs(9 * 60)).await;
        assert_eq!(
            counters.snapshot()["module_frames_dropped_no_route_last_10m"],
            1
        );

        tokio::time::advance(Duration::from_secs(60)).await;
        assert_eq!(
            counters.snapshot()["module_frames_dropped_no_route_last_10m"],
            0
        );
    }

    #[tokio::test(start_paused = true)]
    async fn frame_drop_window_counts_only_nonzero_minutes() {
        let counters = DaemonCounters::new();
        for minute in 0..10 {
            if minute > 0 {
                tokio::time::advance(Duration::from_secs(60)).await;
            }
            if minute != 4 {
                counters.increment_module_frames_dropped_no_route(Some("alpha"));
            }
        }

        assert_eq!(
            counters.snapshot()["module_frames_dropped_no_route_nonzero_minutes_last_10m"],
            9
        );
    }

    #[test]
    fn goodbye_relay_drops_are_attributed_to_the_target_module() {
        let counters = DaemonCounters::new();
        counters.increment_goodbye_relay_module_dropped(Some("alpha"));

        assert_eq!(
            counters.snapshot()["goodbye_relay_module_dropped_by_module"],
            json!({ "alpha": 1 })
        );
    }
}
