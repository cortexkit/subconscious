//! The box account's live connections, learned from the server's `$SYS` connect and
//! disconnect events on ck-bus's system connection. The kick addresses a connection by
//! server id and client id, and those are known only from these events.
//!
//! A connection made before the watch started (before this ck-bus process was up) is not
//! known here until it disconnects. Revoking such a user kicks nothing: the pushed
//! revocation list is what closes its live connection and refuses its reconnect.

use std::{
    collections::{BTreeSet, HashMap},
    sync::{Arc, Mutex},
    time::Duration,
};

use tokio::sync::mpsc;

use super::{log_event, progress::KickTarget};
use crate::bootstrap::plane::{ConnectionEvent, SystemPlane};
use serde_json::json;

#[derive(Debug, Default)]
pub struct Connections {
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    server_id: Option<String>,
    live: HashMap<String, BTreeSet<KickTarget>>,
}

impl State {
    fn apply(&mut self, event: ConnectionEvent) {
        match event {
            ConnectionEvent::Connected {
                server_id,
                client_id,
                user,
            } => {
                if self
                    .server_id
                    .as_ref()
                    .is_none_or(|current| *current == server_id)
                {
                    self.live.entry(user).or_default().insert(KickTarget {
                        server_id,
                        client_id,
                    });
                }
            }
            ConnectionEvent::Disconnected {
                server_id,
                client_id,
                user,
                ..
            } => {
                if let Some(targets) = self.live.get_mut(&user) {
                    targets.remove(&KickTarget {
                        server_id,
                        client_id,
                    });
                    if targets.is_empty() {
                        self.live.remove(&user);
                    }
                }
            }
        }
    }

    fn refresh_server(&mut self, system: &dyn SystemPlane) {
        let Some(current) = system.server_id() else {
            return;
        };
        if self.server_id.as_ref() == Some(&current) {
            return;
        }
        let mut dropped = 0;
        self.live.retain(|_, targets| {
            let before = targets.len();
            targets.retain(|target| target.server_id == current);
            dropped += before - targets.len();
            !targets.is_empty()
        });
        let previous = self.server_id.replace(current);
        if previous.is_some() || dropped != 0 {
            log_event(
                "ckbus.revocation.connection_targets_pruned",
                json!({
                    "server_id": self.server_id, "previous_server_id": previous, "dropped": dropped,
                }),
            );
        }
    }
}

impl Connections {
    /// The live connections of `user` this process has seen connect.
    pub fn targets(&self, user: &str) -> BTreeSet<KickTarget> {
        self.lock().live.get(user).cloned().unwrap_or_default()
    }

    /// Check the system connection even when the event stream is idle. A server
    /// restart can leave no disconnect events and no new module connections yet.
    pub async fn follow(
        &self,
        system: Arc<dyn SystemPlane>,
        mut events: mpsc::UnboundedReceiver<ConnectionEvent>,
    ) {
        let mut recheck = tokio::time::interval(Duration::from_secs(1));
        recheck.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            let event = tokio::select! {
                event = events.recv() => match event {
                    Some(event) => Some(event),
                    None => return,
                },
                _ = recheck.tick() => None,
            };
            let mut state = self.lock();
            state.refresh_server(system.as_ref());
            if let Some(event) = event {
                state.apply(event);
            }
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
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
        connections.lock().apply(connected(1));
        connections.lock().apply(connected(2));
        assert_eq!(connections.targets("UA").len(), 2);
        connections.lock().apply(ConnectionEvent::Disconnected {
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
