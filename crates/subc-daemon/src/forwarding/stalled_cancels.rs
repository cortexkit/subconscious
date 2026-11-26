//! Diagnostics for forwarded CANCELs whose request credit is still held.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use tokio::{
    sync::Notify,
    task::JoinHandle,
    time::{self, Instant},
};
use tracing::warn;

use super::{ForwardingTable, RouteBinding};
use crate::observability::DaemonCounters;

const GRACE: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, Default)]
pub(crate) struct StalledCancels(Arc<Mutex<State>>);

#[derive(Debug, Default)]
struct State {
    changed: Arc<Notify>,
    next_id: u64,
    pending: BTreeMap<Key, Request>,
    stalled: u64,
}

// Order by cancellation time; the sequence number distinguishes CANCELs that
// get the same clock reading.
type Key = (Instant, u64);

#[derive(Debug)]
struct Request {
    module_id: String,
    module_channel: u16,
    module_epoch: u32,
    corr: u64,
    subscription: bool,
}

/// Dropping a held credit (or closing its route) ends the observation, whether
/// the CANCEL was still within the grace period or had already been reported.
#[derive(Debug)]
pub(super) struct CancelObservation {
    key: Key,
    monitor: StalledCancels,
}

impl Drop for CancelObservation {
    fn drop(&mut self) {
        let mut state = self.monitor.state();
        let was_first = state
            .pending
            .first_key_value()
            .is_some_and(|(key, _)| *key == self.key);
        if state.pending.remove(&self.key).is_none() {
            state.stalled -= 1;
        }
        if was_first {
            state.changed.notify_one();
        }
    }
}

impl StalledCancels {
    fn state(&self) -> MutexGuard<'_, State> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(super) fn record(
        &self,
        route: &RouteBinding,
        corr: u64,
        subscription: bool,
    ) -> CancelObservation {
        let mut state = self.state();
        let key = (Instant::now(), state.next_id);
        state.next_id += 1;
        let wakes_worker = state
            .pending
            .first_key_value()
            .is_none_or(|(first, _)| key < *first);
        state.pending.insert(
            key,
            Request {
                module_id: route.module_id.clone(),
                module_channel: route.module_channel,
                module_epoch: route.module_epoch,
                corr,
                subscription,
            },
        );
        if wakes_worker {
            state.changed.notify_one();
        }
        CancelObservation {
            key,
            monitor: self.clone(),
        }
    }

    pub(crate) fn count(&self) -> u64 {
        let state = self.state();
        let now = Instant::now();
        // Include expired entries even if the worker hasn't been scheduled yet.
        state.stalled
            + state
                .pending
                .keys()
                .take_while(|(at, _)| *at + GRACE <= now)
                .count() as u64
    }

    async fn run(self) {
        let changed = self.state().changed.clone();
        loop {
            let now = Instant::now();
            let (warnings, deadline) = {
                let mut state = self.state();
                let mut warnings = Vec::new();
                while state
                    .pending
                    .first_key_value()
                    .is_some_and(|((at, _), _)| *at + GRACE <= now)
                {
                    warnings.push(state.pending.pop_first().expect("due cancellation exists"));
                    state.stalled += 1;
                }
                (
                    warnings,
                    state
                        .pending
                        .first_key_value()
                        .map(|((at, _), _)| *at + GRACE),
                )
            };
            for ((at, _), request) in warnings {
                warn!(
                    module_id = %request.module_id,
                    module_channel = request.module_channel,
                    module_epoch = request.module_epoch,
                    corr = request.corr,
                    subscription = request.subscription,
                    seconds_since_cancel = now.duration_since(at).as_secs(),
                    "cancelled module request has no terminal"
                );
            }
            if let Some(deadline) = deadline {
                tokio::select! {
                    () = changed.notified() => {},
                    () = time::sleep_until(deadline) => {},
                }
            } else {
                changed.notified().await;
            }
        }
    }
}

impl RouteBinding {
    pub(crate) fn record_forwarded_cancel(&self, corr: u64, counters: &DaemonCounters) {
        self.flow
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .credits
            .record_cancel(self, corr, counters.stalled_cancels());
    }
}

impl ForwardingTable {
    pub(crate) fn spawn_stalled_cancel_monitor(&self) -> JoinHandle<()> {
        tokio::spawn(self.counters.stalled_cancels().clone().run())
    }
}
