//! The box account's live connections, learned from the server's `$SYS` connect and
//! disconnect events on ck-bus's system connection. The kick addresses a connection by
//! server id and client id, and those are known only from these events.
//!
//! A connection made before the watch started (before this ck-bus process was up) is not
//! known here until it disconnects. Revoking such a user kicks nothing: the pushed
//! revocation list is what closes its live connection and refuses its reconnect.

use std::{
    collections::{BTreeSet, HashMap},
    sync::Mutex,
};

use tokio::sync::mpsc;

use super::progress::KickTarget;
use crate::bootstrap::plane::ConnectionEvent;

#[derive(Debug, Default)]
pub struct Connections {
    live: Mutex<HashMap<String, BTreeSet<KickTarget>>>,
}

impl Connections {
    pub fn apply(&self, event: ConnectionEvent) {
        let mut live = self.lock();
        match event {
            ConnectionEvent::Connected {
                server_id,
                client_id,
                user,
            } => {
                live.entry(user).or_default().insert(KickTarget {
                    server_id,
                    client_id,
                });
            }
            ConnectionEvent::Disconnected {
                server_id,
                client_id,
                user,
                ..
            } => {
                if let Some(targets) = live.get_mut(&user) {
                    targets.remove(&KickTarget {
                        server_id,
                        client_id,
                    });
                    if targets.is_empty() {
                        live.remove(&user);
                    }
                }
            }
        }
    }

    /// The live connections of `user` this process has seen connect.
    pub fn targets(&self, user: &str) -> BTreeSet<KickTarget> {
        self.lock().get(user).cloned().unwrap_or_default()
    }

    /// Applies every event from `events` until the watch ends.
    pub async fn follow(&self, mut events: mpsc::UnboundedReceiver<ConnectionEvent>) {
        while let Some(event) = events.recv().await {
            self.apply(event);
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, BTreeSet<KickTarget>>> {
        // Each mutation is one insert or remove, so a poisoned map is still whole.
        self.live
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_connection_is_tracked_until_it_disconnects() {
        let connections = Connections::default();
        let connected = |client_id| ConnectionEvent::Connected {
            server_id: "S".to_string(),
            client_id,
            user: "UA".to_string(),
        };
        connections.apply(connected(1));
        connections.apply(connected(2));
        assert_eq!(connections.targets("UA").len(), 2);
        connections.apply(ConnectionEvent::Disconnected {
            server_id: "S".to_string(),
            client_id: 1,
            user: "UA".to_string(),
            reason: "Client Closed".to_string(),
        });
        assert_eq!(
            connections.targets("UA"),
            BTreeSet::from([KickTarget {
                server_id: "S".to_string(),
                client_id: 2,
            }])
        );
        assert!(connections.targets("UB").is_empty());
    }
}
