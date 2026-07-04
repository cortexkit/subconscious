use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

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
