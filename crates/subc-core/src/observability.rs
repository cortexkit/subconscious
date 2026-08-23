use std::{
    collections::{HashMap, VecDeque},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, MutexGuard,
    },
    time::Duration,
};

use serde_json::{json, Value};
use tracing::info;

use crate::registry::ConnectionId;

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
        info!(
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
        info!(
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
    module_frames_dropped_no_route: Arc<AtomicU64>,
    // Per-module maps and the rate window are daemon-lifetime diagnostics only:
    // they deliberately reset on restart instead of becoming durable daemon state.
    module_frames_dropped_no_route_by_module: Arc<Mutex<HashMap<String, u64>>>,
    module_frames_dropped_no_route_window: Arc<Mutex<DropWindow>>,
    module_requests_dropped_stale_route: Arc<AtomicU64>,
    client_frames_dropped_stale_route: Arc<AtomicU64>,
    client_egress_close_delivery_failed: Arc<AtomicU64>,
    goodbye_relay_client_failed: Arc<AtomicU64>,
    goodbye_relay_module_dropped: Arc<AtomicU64>,
    goodbye_relay_module_dropped_by_module: Arc<Mutex<HashMap<String, u64>>>,
    route_released_epoch_fenced: Arc<AtomicU64>,
    route_release_stale_skipped: Arc<AtomicU64>,
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
        insert_nonempty_module_counts(
            &mut snapshot,
            "module_frames_dropped_no_route_by_module",
            &self.module_frames_dropped_no_route_by_module,
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
        insert_nonempty_module_counts(
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
        Value::Object(snapshot)
    }

    pub(crate) fn increment_module_frames_dropped_no_route(&self, module_id: Option<&str>) {
        self.module_frames_dropped_no_route
            .fetch_add(1, Ordering::Relaxed);
        if let Some(module_id) = module_id {
            increment_module_count(&self.module_frames_dropped_no_route_by_module, module_id);
        }
        self.module_frames_dropped_no_route_window
            .lock()
            .expect("drop-rate window mutex poisoned")
            .record(tokio::time::Instant::now());
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
            increment_module_count(&self.goodbye_relay_module_dropped_by_module, module_id);
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
}

fn increment_module_count(counts: &Mutex<HashMap<String, u64>>, module_id: &str) {
    *counts
        .lock()
        .expect("module drop-count mutex poisoned")
        .entry(module_id.to_string())
        .or_default() += 1;
}

fn insert_nonempty_module_counts(
    snapshot: &mut serde_json::Map<String, Value>,
    key: &str,
    counts: &Mutex<HashMap<String, u64>>,
) {
    let counts: MutexGuard<'_, HashMap<String, u64>> =
        counts.lock().expect("module drop-count mutex poisoned");
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
