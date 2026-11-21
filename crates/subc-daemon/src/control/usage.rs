//! Bounded, body-free attribution of channel-zero requests.

use std::collections::BTreeMap;

use super::{ConnectionId, ControlHandler};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const WINDOW: Duration = Duration::from_secs(60);
const MAX_PAIRS: usize = 256;
const TOP_PAIRS: usize = 32;
const MAX_LABEL_BYTES: usize = 256;

pub(super) struct ControlUsage {
    started: Instant,
    pairs: BTreeMap<(String, String), u64>,
    total: u64,
    overflow: u64,
}

impl Default for ControlUsage {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            pairs: BTreeMap::new(),
            total: 0,
            overflow: 0,
        }
    }
}

impl ControlUsage {
    fn advance(&mut self, now: Instant) {
        if now.duration_since(self.started) >= WINDOW {
            self.started = now;
            self.pairs.clear();
            self.total = 0;
            self.overflow = 0;
        }
    }

    pub(super) fn record(&mut self, op: &str, principal: &str) {
        self.record_at(op, principal, Instant::now());
    }

    fn record_at(&mut self, op: &str, principal: &str, now: Instant) {
        self.advance(now);
        self.total = self.total.saturating_add(1);
        // Unknown operations and principals cannot grow diagnostics without bound.
        if op.len() > MAX_LABEL_BYTES || principal.len() > MAX_LABEL_BYTES {
            self.overflow = self.overflow.saturating_add(1);
            return;
        }
        let key = (op.to_owned(), principal.to_owned());
        if let Some(count) = self.pairs.get_mut(&key) {
            *count = count.saturating_add(1);
        } else if self.pairs.len() < MAX_PAIRS {
            self.pairs.insert(key, 1);
        } else {
            self.overflow = self.overflow.saturating_add(1);
        }
    }

    pub(super) fn snapshot(&mut self) -> Value {
        self.snapshot_at(Instant::now())
    }

    fn snapshot_at(&mut self, now: Instant) -> Value {
        self.advance(now);
        let mut pairs: Vec<_> = self.pairs.iter().collect();
        pairs.sort_by(|(a_key, a_count), (b_key, b_count)| {
            b_count.cmp(a_count).then_with(|| a_key.cmp(b_key))
        });
        let top: Vec<_> = pairs
            .into_iter()
            .take(TOP_PAIRS)
            .map(|((op, principal), count)| {
                json!({"op": op, "principal": principal, "requests": count})
            })
            .collect();
        json!({
            "window_ms": WINDOW.as_millis() as u64,
            "elapsed_ms": now.duration_since(self.started).as_millis() as u64,
            "requests": self.total,
            "overflow_requests": self.overflow,
            "tracked_pairs": self.pairs.len(),
            "top": top,
        })
    }
}

impl ControlHandler {
    pub(super) fn record_control_usage(&self, connection: ConnectionId, op: &str) {
        let module_id = self
            .claims
            .binding(connection)
            .filter(|binding| {
                binding.consumer_live(connection) && binding.registration.authority.alive()
            })
            .map(|binding| binding.registration.module_id.clone())
            .or_else(|| {
                self.registry
                    .module_id_for_connection(connection)
                    .ok()
                    .flatten()
            });
        let principal = module_id
            .map(|module_id| format!("reserved:{module_id}"))
            .unwrap_or_else(|| "direct".to_owned());
        self.control_usage
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .record(op, &principal);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_usage_counts_principals_and_resets_without_a_timer() {
        let mut usage = ControlUsage::default();
        let now = usage.started;
        usage.record_at("catalog.list", "reserved:provider", now);
        usage.record_at("catalog.list", "direct", now);
        usage.record_at("catalog.list", "reserved:provider", now);
        usage.record_at("config.desktop_review", "direct", now);
        let snapshot = usage.snapshot_at(now + Duration::from_secs(2));
        assert_eq!(snapshot["requests"], 4);
        assert_eq!(snapshot["top"][0]["principal"], "reserved:provider");
        assert_eq!(snapshot["top"][0]["requests"], 2);
        assert_eq!(snapshot["elapsed_ms"], 2000);
        let reset = usage.snapshot_at(now + WINDOW);
        assert_eq!(reset["requests"], 0);
        assert_eq!(reset["top"], json!([]));
    }

    #[test]
    fn control_usage_bounds_storage_output_and_labels() {
        let mut usage = ControlUsage::default();
        let now = usage.started;
        for index in 0..MAX_PAIRS + 10 {
            usage.record_at("catalog.list", &format!("reserved:provider-{index}"), now);
        }
        usage.record_at(&"x".repeat(MAX_LABEL_BYTES + 1), "direct", now);
        let snapshot = usage.snapshot_at(now);
        assert_eq!(snapshot["tracked_pairs"], MAX_PAIRS);
        assert_eq!(snapshot["top"].as_array().unwrap().len(), TOP_PAIRS);
        assert_eq!(snapshot["overflow_requests"], 11);
        assert_eq!(snapshot["requests"], MAX_PAIRS + 11);
    }
}
