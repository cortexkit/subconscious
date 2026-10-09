use std::time::{Duration, SystemTime, UNIX_EPOCH};

const STEP_THRESHOLD_MS: i128 = 5_000;

pub(crate) fn wall_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

// All production monotonic readings pass through this binding, not Instant:
// Instant stops during sleep on macOS.
fn monotonic_now() -> Duration {
    subc_uptime::suspend_inclusive_now()
}

/// Suspend-inclusive startup anchor used to derive corrected wall-clock start time.
#[derive(Clone, Copy, Debug)]
pub(crate) struct StartClock {
    started_monotonic: Duration,
}

impl StartClock {
    /// Capture the current suspend-inclusive reading at daemon startup.
    pub(crate) fn capture() -> Self {
        Self {
            started_monotonic: monotonic_now(),
        }
    }

    /// Derive a wall-clock start timestamp using the latest wall reading.
    pub(crate) fn started_at_ms(self) -> u64 {
        self.derive(wall_now_ms(), monotonic_now())
    }

    fn derive(self, wall_ms: u64, monotonic: Duration) -> u64 {
        let elapsed_ms = monotonic.saturating_sub(self.started_monotonic).as_millis();
        wall_ms.saturating_sub(elapsed_ms.try_into().unwrap_or(u64::MAX))
    }
}

#[derive(Debug)]
pub(crate) struct ClockStepDetector {
    previous_offset_ms: Option<i128>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ClockStep {
    pub(crate) delta_ms: i128,
    pub(crate) old_offset_ms: i128,
    pub(crate) new_offset_ms: i128,
}

impl ClockStep {
    /// What the pre-step clock would read at `wall_now`, the corrected reading.
    ///
    /// Lines written before the step were stamped, and filed into day segments,
    /// by that clock, so a marker describing them has to be stamped by it too
    /// to land in their segment and sort beside them.
    pub(crate) fn old_clock_reading(&self, wall_now: SystemTime) -> SystemTime {
        let delta =
            Duration::from_millis(self.delta_ms.unsigned_abs().try_into().unwrap_or(u64::MAX));
        if self.delta_ms > 0 {
            // The clock jumped forward, so the old clock reads earlier.
            wall_now.checked_sub(delta).unwrap_or(UNIX_EPOCH)
        } else {
            wall_now.checked_add(delta).unwrap_or(wall_now)
        }
    }
}

impl ClockStepDetector {
    pub(crate) fn new() -> Self {
        Self {
            previous_offset_ms: None,
        }
    }

    pub(crate) fn check_now(&mut self) -> Option<ClockStep> {
        self.observe(wall_now_ms(), monotonic_now())
    }

    fn observe(&mut self, wall_ms: u64, monotonic: Duration) -> Option<ClockStep> {
        let offset = i128::from(wall_ms) - monotonic.as_millis() as i128;
        // At startup there is no prior offset: a pre-existing step cannot be detected.
        let previous = self.previous_offset_ms.replace(offset)?;
        let delta = offset - previous;
        (delta.abs() > STEP_THRESHOLD_MS).then_some(ClockStep {
            delta_ms: delta,
            old_offset_ms: previous,
            new_offset_ms: offset,
        })
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn old_clock_reading_undoes_the_step_in_both_directions() {
        let now = UNIX_EPOCH + Duration::from_millis(1_790_000_000_000);
        let backward = ClockStep {
            delta_ms: -7_200_000,
            old_offset_ms: 0,
            new_offset_ms: -7_200_000,
        };
        // A step BACKWARD (a fast RTC corrected) means the old clock read later.
        assert_eq!(
            backward.old_clock_reading(now),
            now + Duration::from_millis(7_200_000)
        );
        let forward = ClockStep {
            delta_ms: 9_000,
            old_offset_ms: 0,
            new_offset_ms: 9_000,
        };
        assert_eq!(
            forward.old_clock_reading(now),
            now - Duration::from_millis(9_000)
        );
    }

    use super::*;

    fn seconds(value: u64) -> Duration {
        Duration::from_secs(value)
    }

    #[test]
    fn backward_wall_step_moves_derived_start_by_exact_step() {
        let start = StartClock {
            started_monotonic: seconds(100),
        };
        assert_eq!(start.derive(1_000_000, seconds(140)), 960_000);
        assert_eq!(start.derive(992_800, seconds(140)), 952_800);
    }

    #[test]
    fn advancing_monotonic_and_wall_leaves_derived_start_stable() {
        let start = StartClock {
            started_monotonic: seconds(100),
        };
        assert_eq!(start.derive(1_000_000, seconds(140)), 960_000);
        assert_eq!(start.derive(1_020_000, seconds(160)), 960_000);
    }

    #[test]
    fn detector_ignores_noise_and_reports_each_step_only_once() {
        let mut detector = ClockStepDetector::new();
        assert_eq!(detector.observe(1_000_000, seconds(100)), None);
        assert_eq!(detector.observe(1_003_000, seconds(102)), None);
        assert_eq!(
            detector.observe(1_013_000, seconds(103)),
            Some(ClockStep {
                delta_ms: 9_000,
                old_offset_ms: 901_000,
                new_offset_ms: 910_000,
            })
        );
        assert_eq!(detector.observe(1_014_000, seconds(104)), None);
        assert_eq!(
            detector.observe(1_006_000, seconds(105)),
            Some(ClockStep {
                delta_ms: -9_000,
                old_offset_ms: 910_000,
                new_offset_ms: 901_000,
            })
        );
        assert_eq!(detector.observe(1_007_000, seconds(106)), None);
    }
}
