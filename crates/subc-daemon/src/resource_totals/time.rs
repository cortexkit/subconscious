use std::time::Duration;

pub(super) const HOUR: Duration = Duration::from_secs(60 * 60);

/// Resource ages use the suspend-inclusive clock, never wall-clock labels.
#[derive(Clone, Debug, Default)]
pub(crate) struct Clock {
    #[cfg(test)]
    controlled: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
}

impl Clock {
    pub(crate) fn now(&self) -> Duration {
        #[cfg(test)]
        if let Some(controlled) = &self.controlled {
            return Duration::from_millis(controlled.load(std::sync::atomic::Ordering::Acquire));
        }
        crate::clock::monotonic_now()
    }

    #[cfg(test)]
    pub(crate) fn manual(now: Duration) -> Self {
        let clock = Self {
            controlled: Some(std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0))),
        };
        clock.set(now);
        clock
    }

    #[cfg(test)]
    pub(crate) fn set(&self, now: Duration) {
        self.controlled
            .as_ref()
            .expect("injected resource clock")
            .store(
                now.as_millis().try_into().unwrap_or(u64::MAX),
                std::sync::atomic::Ordering::Release,
            );
    }
}

/// Loss event identity and elapsed age. Hourly bounds live on history samples.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct RecentLoss {
    last: Option<Duration>,
    revision: u64,
}

impl RecentLoss {
    pub(super) fn mark(&mut self, at: Duration) {
        self.last = Some(self.last.map_or(at, |last| last.max(at)));
        self.revision = self.revision.saturating_add(1);
    }

    pub(super) fn revision(&self) -> u64 {
        self.revision
    }

    pub(super) fn active(&self, now: Duration) -> bool {
        self.last
            .is_some_and(|last| now.saturating_sub(last) < HOUR)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_recent_loss_holds_for_the_full_hour_without_following_wall_clock() {
        let clock = Clock::manual(Duration::from_secs(10));
        let mut loss = RecentLoss::default();
        loss.mark(clock.now());
        assert!(loss.active(clock.now()));
        clock.set(Duration::from_millis(3_609_999));
        assert!(loss.active(clock.now()));
        clock.set(Duration::from_secs(3_610));
        assert!(!loss.active(clock.now()));
    }
}
