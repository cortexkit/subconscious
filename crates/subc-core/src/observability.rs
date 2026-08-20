use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
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
    module_requests_dropped_stale_route: Arc<AtomicU64>,
    client_frames_dropped_stale_route: Arc<AtomicU64>,
    client_egress_close_delivery_failed: Arc<AtomicU64>,
    goodbye_relay_client_failed: Arc<AtomicU64>,
    goodbye_relay_module_dropped: Arc<AtomicU64>,
    route_released_epoch_fenced: Arc<AtomicU64>,
    route_release_stale_skipped: Arc<AtomicU64>,
}

impl DaemonCounters {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns a JSON snapshot whose stable, additive schema keeps the
    /// `server.describe` diagnostic endpoint backward-compatible.
    pub fn snapshot(&self) -> Value {
        json!({
            "module_frames_dropped_no_route": self.module_frames_dropped_no_route.load(Ordering::Relaxed),
            "module_requests_dropped_stale_route": self.module_requests_dropped_stale_route.load(Ordering::Relaxed),
            "client_frames_dropped_stale_route": self.client_frames_dropped_stale_route.load(Ordering::Relaxed),
            "client_egress_close_delivery_failed": self.client_egress_close_delivery_failed.load(Ordering::Relaxed),
            "goodbye_relay_client_failed": self.goodbye_relay_client_failed.load(Ordering::Relaxed),
            "goodbye_relay_module_dropped": self.goodbye_relay_module_dropped.load(Ordering::Relaxed),
            "route_released_epoch_fenced": self.route_released_epoch_fenced.load(Ordering::Relaxed),
            "route_release_stale_skipped": self.route_release_stale_skipped.load(Ordering::Relaxed),
        })
    }

    pub(crate) fn increment_module_frames_dropped_no_route(&self) {
        self.module_frames_dropped_no_route
            .fetch_add(1, Ordering::Relaxed);
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

    pub(crate) fn increment_goodbye_relay_module_dropped(&self) {
        self.goodbye_relay_module_dropped
            .fetch_add(1, Ordering::Relaxed);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_snapshot_uses_stable_keys() {
        let counters = DaemonCounters::new();
        counters.increment_module_frames_dropped_no_route();
        counters.increment_module_requests_dropped_stale_route();
        counters.increment_route_release_stale_skipped();

        assert_eq!(
            counters.snapshot(),
            json!({
                "module_frames_dropped_no_route": 1,
                "module_requests_dropped_stale_route": 1,
                "client_frames_dropped_stale_route": 0,
                "client_egress_close_delivery_failed": 0,
                "goodbye_relay_client_failed": 0,
                "goodbye_relay_module_dropped": 0,
                "route_released_epoch_fenced": 0,
                "route_release_stale_skipped": 1,
            })
        );
    }
}
