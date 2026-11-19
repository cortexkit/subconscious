//! Coalesce identical `route.open` refusal events without changing replies or metrics.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use tokio::task::AbortHandle;
use tokio::time::Instant;
use tracing::{info, instrument::WithSubscriber};

use super::RouteOpenCaller;

const WINDOW: Duration = Duration::from_secs(60);
const MAX_KEYS: usize = 256;
const MAX_TEXT_BYTES: usize = 1024 * 1024;

#[derive(Clone, Default)]
pub(super) struct RefusalLog(Arc<Mutex<State>>);

#[derive(Eq, Hash, PartialEq)]
struct Key {
    module_id: Box<str>,
    code: &'static str,
    reason: &'static str,
    principal: Box<str>,
    harness: Box<str>,
}

struct Entry {
    deadline: Instant,
    repeated: u64,
    // Sample attribution from the first refusal; neither field separates keys.
    connection_id: u64,
    scope: Box<str>,
}

impl Entry {
    fn text_bytes(&self, key: &Key) -> usize {
        key.module_id.len() + key.principal.len() + key.harness.len() + self.scope.len()
    }

    fn flush(self, key: &Key, now: Instant) {
        if self.repeated == 0 {
            return;
        }
        let window = now
            .saturating_duration_since(self.deadline - WINDOW)
            .min(WINDOW);
        info!(
            target: "control",
            code = key.code,
            reason = key.reason,
            module_id = ?key.module_id.as_ref(),
            connection_id = self.connection_id,
            principal = %key.principal,
            scope = %self.scope,
            harness = %key.harness,
            repeated = self.repeated,
            window_ms = window.as_millis() as u64,
            "route.open refused"
        );
    }
}

#[derive(Default)]
struct State {
    entries: HashMap<Arc<Key>, Entry>,
    // Fixed windows are ordered by insertion, both for expiry and for eviction.
    oldest_first: VecDeque<(Instant, Arc<Key>)>,
    text_bytes: usize,
    worker: Option<AbortHandle>,
}

impl State {
    fn next_deadline(&self) -> Option<Instant> {
        self.oldest_first.front().map(|(deadline, _)| *deadline)
    }

    fn pop_oldest(&mut self) -> Option<(Arc<Key>, Entry)> {
        let (_, key) = self.oldest_first.pop_front()?;
        let entry = self
            .entries
            .remove(&key)
            .expect("queued refusal has an entry");
        self.text_bytes -= entry.text_bytes(&key);
        Some((key, entry))
    }

    fn expire(&mut self, now: Instant, flushed: &mut Vec<(Arc<Key>, Entry)>) {
        while self.next_deadline().is_some_and(|deadline| deadline <= now) {
            flushed.extend(self.pop_oldest());
        }
    }
}

impl RefusalLog {
    pub(super) fn record(
        &self,
        module_id: &str,
        code: &'static str,
        reason: &'static str,
        caller: &RouteOpenCaller,
        connection_id: u64,
    ) {
        let key = Key {
            module_id: module_id.into(),
            code,
            reason,
            principal: caller.principal.as_str().into(),
            harness: caller.harness.as_str().into(),
        };
        let mut flushed = Vec::new();
        let (now, first) = {
            let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
            let now = Instant::now();
            state.expire(now, &mut flushed);
            let first = if let Some(entry) = state.entries.get_mut(&key) {
                entry.repeated = entry.repeated.saturating_add(1);
                false
            } else {
                let deadline = now + WINDOW;
                let entry = Entry {
                    deadline,
                    repeated: 0,
                    connection_id,
                    scope: caller.scope.as_str().into(),
                };
                let bytes = entry.text_bytes(&key);
                // An unknown module_id is arbitrary request text. Limit total
                // retained text, but keep one oversized key so it still coalesces.
                while state.entries.len() >= MAX_KEYS
                    || (!state.entries.is_empty() && state.text_bytes + bytes > MAX_TEXT_BYTES)
                {
                    flushed.extend(state.pop_oldest());
                }
                let key = Arc::new(key);
                state.oldest_first.push_back((deadline, Arc::clone(&key)));
                state.entries.insert(key, entry);
                state.text_bytes += bytes;
                true
            };
            if state.worker.as_ref().is_none_or(AbortHandle::is_finished) {
                let worker = flush_windows(Arc::downgrade(&self.0)).with_current_subscriber();
                state.worker = Some(tokio::spawn(worker).abort_handle());
            }
            (now, first)
        };

        for (key, entry) in flushed {
            entry.flush(&key, now);
        }
        if first {
            // Code and reason are daemon labels; escape the caller's target id.
            info!(
                target: "control",
                code,
                reason,
                module_id = ?module_id,
                connection_id,
                principal = %caller.principal,
                scope = %caller.scope,
                harness = %caller.harness,
                "route.open refused"
            );
        }
    }
}

async fn flush_windows(state: Weak<Mutex<State>>) {
    loop {
        let (next, now, flushed) = {
            let Some(state) = state.upgrade() else {
                return;
            };
            let mut state = state.lock().unwrap_or_else(|p| p.into_inner());
            let now = Instant::now();
            let mut flushed = Vec::new();
            state.expire(now, &mut flushed);
            let next = state.next_deadline();
            if next.is_none() {
                state.worker = None;
            }
            (next, now, flushed)
        };
        for (key, entry) in flushed {
            entry.flush(&key, now);
        }
        let Some(next) = next else {
            return;
        };
        // Inserts cannot move the earliest deadline earlier. Eviction may make
        // this wakeup early, at which point the worker reads the new deadline.
        tokio::time::sleep_until(next).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::test_log::{captured_logs, log_capture};

    #[tokio::test(start_paused = true)]
    async fn capacity_evicts_and_flushes_the_oldest_key_even_after_a_recent_repeat() {
        let log = RefusalLog::default();
        let caller = RouteOpenCaller::unknown();
        let record = |id: &str| log.record(id, "unknown_module", "not_registered", &caller, 1);
        let (output, _subscriber) = log_capture(tracing::Level::INFO);
        record("oldest");
        record("next-oldest");
        record("next-oldest");
        for index in 2..MAX_KEYS {
            record(&format!("target-{index}"));
        }
        tokio::time::advance(Duration::from_secs(5)).await;
        record("oldest");
        record("overflow");

        let logs = captured_logs(&output);
        let summaries: Vec<_> = logs
            .lines()
            .filter(|line| line.contains("repeated="))
            .collect();
        assert_eq!(summaries.len(), 1, "{summaries:?}");
        assert!(
            summaries[0].contains("module_id=\"oldest\""),
            "{summaries:?}"
        );
        assert!(
            summaries[0].contains("repeated=1 window_ms=5000"),
            "{summaries:?}"
        );
        assert_eq!(log.0.lock().unwrap().entries.len(), MAX_KEYS);
    }

    #[tokio::test(start_paused = true)]
    async fn retained_text_budget_still_allows_one_oversized_key_to_coalesce() {
        let log = RefusalLog::default();
        let caller = RouteOpenCaller::unknown();
        let record = |id: &str| log.record(id, "unknown_module", "not_registered", &caller, 1);
        let half_budget = "x".repeat(MAX_TEXT_BYTES / 2);
        record(&half_budget);
        record(&format!("{half_budget}y"));
        assert_eq!(log.0.lock().unwrap().entries.len(), 1);

        let oversized = "x".repeat(MAX_TEXT_BYTES + 1);
        record(&oversized);
        record(&oversized);
        let state = log.0.lock().unwrap();
        assert_eq!(state.entries.len(), 1);
        assert_eq!(state.entries.values().next().unwrap().repeated, 1);
    }
}
