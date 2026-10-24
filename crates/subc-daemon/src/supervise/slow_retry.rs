use super::*;

/// Recovery after a daemon-initiated failure. Deadlines use the daemon's
/// monotonic clock, which pauses during macOS sleep: delays count awake time.
/// Operator-owned operations and daemon shutdown do not arm this schedule.
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct FailedRetry {
    pub(super) attempt: u32,
    pub(super) pending: Option<SlowRetry>,
    pub(super) healthy_since: Option<Instant>,
    // Draining publishes Failed before waiting for registration release. Keep
    // that instant so a slow release does not postpone the recovery deadline.
    pub(super) failed_at: Option<Instant>,
    pub(super) running_since: Option<Instant>,
}

/// Remember whether the daemon started the operation across waits. Only those
/// failures may schedule a retry, and only while the daemon's live shutdown
/// flag is unset when the failure is reported.
#[derive(Debug, Clone)]
pub(super) struct FailureContext {
    pub(super) daemon_initiated: bool,
    pub(super) shutdown: crate::child_roster::DaemonShutdownFlag,
}

impl FailureContext {
    pub(super) fn operator() -> Self {
        Self {
            daemon_initiated: false,
            shutdown: Default::default(),
        }
    }

    pub(super) fn may_retry(&self) -> bool {
        self.daemon_initiated && !self.shutdown.is_set()
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct SlowRetry {
    pub(super) deadline: Instant,
    pub(super) delay: Duration,
    pub(super) attempt: u32,
}

impl FailedRetry {
    pub(super) fn schedule(&mut self, module_id: &str) {
        if self.pending.is_some() {
            return;
        }
        self.attempt = self.attempt.saturating_add(1);
        let delay = Duration::from_secs((600 * (1 << (self.attempt - 1).min(3))).min(3600));
        self.pending = Some(SlowRetry {
            deadline: self.failed_at.take().unwrap_or_else(Instant::now) + delay,
            delay,
            attempt: self.attempt,
        });
        self.healthy_since = None;
        info!(
            module_id,
            attempt = self.attempt,
            delay_secs = delay.as_secs(),
            "failed module retry scheduled"
        );
    }

    pub(super) fn cancel(&mut self) {
        self.pending = None;
        self.healthy_since = None;
        self.failed_at = None;
    }

    pub(super) fn observe_health(&mut self, healthy: bool, window: Duration) {
        if !healthy {
            self.healthy_since = None;
        } else if self.attempt > 0 {
            let since = self.healthy_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= window {
                self.attempt = 0;
                self.healthy_since = None;
            }
        }
    }

    pub(super) fn uptime_reset_at(&mut self, no_checks: bool, window: Duration) -> Option<Instant> {
        if !no_checks || self.attempt == 0 {
            return None;
        }
        let deadline = self.running_since?.checked_add(window)?;
        if Instant::now() >= deadline {
            self.attempt = 0;
            self.healthy_since = None;
            None
        } else {
            Some(deadline)
        }
    }
}
