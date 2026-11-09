//! Wall-clock windows for cumulative resource counters.

use std::collections::VecDeque;

const HOUR_MS: u64 = 60 * 60 * 1_000;
const MAX_SAMPLES: usize = 61;

#[derive(Debug, Clone, Copy)]
struct Sample {
    sequence: u64,
    at_ms: u64,
    generation: u128,
    cpu_ms: u64,
    uncertain: bool,
}

#[derive(Debug, Default, Clone)]
pub(crate) struct History {
    samples: VecDeque<Sample>,
}

impl History {
    /// Change ownership at consumption time, not at the later declaration.
    /// The two cumulative baselines move together, so an expired interval
    /// cannot become current-hour CPU or look like a counter restart.
    pub(crate) fn transfer_to(&mut self, agents: &mut Self, sequence: u64, cpu: u64) -> bool {
        let module = self
            .samples
            .iter()
            .filter(|sample| sample.sequence >= sequence);
        let recipients = agents
            .samples
            .iter()
            .filter(|sample| sample.sequence >= sequence);
        // A transfer is one paired reclassification. Check the entire suffix
        // before touching either row; wall-clock ties or corrections do not
        // identify which observations contain the CPU that funded it.
        if !module
            .clone()
            .map(|sample| sample.sequence)
            .eq(recipients.clone().map(|sample| sample.sequence))
            || module
                .clone()
                .any(|sample| sample.cpu_ms.checked_sub(cpu).is_none())
            || recipients
                .clone()
                .any(|sample| sample.cpu_ms.checked_add(cpu).is_none())
        {
            return false;
        }
        for sample in self
            .samples
            .iter_mut()
            .filter(|sample| sample.sequence >= sequence)
        {
            sample.cpu_ms -= cpu;
        }
        for sample in agents
            .samples
            .iter_mut()
            .filter(|sample| sample.sequence >= sequence)
        {
            sample.cpu_ms += cpu;
        }
        true
    }
    #[cfg(test)]
    pub(crate) fn record(&mut self, at_ms: u64, generation: u128, cpu_ms: u64) {
        self.record_observation(at_ms, at_ms, generation, cpu_ms);
    }
    pub(crate) fn record_observation(
        &mut self,
        sequence: u64,
        at_ms: u64,
        generation: u128,
        cpu_ms: u64,
    ) {
        self.record_bound_observation(sequence, at_ms, generation, cpu_ms, false);
    }

    pub(crate) fn record_bound_observation(
        &mut self,
        sequence: u64,
        at_ms: u64,
        generation: u128,
        cpu_ms: u64,
        uncertain: bool,
    ) -> bool {
        // A wall-clock correction must not turn a duration into an underflow.
        if self.samples.back().is_some_and(|last| at_ms <= last.at_ms) {
            return false;
        }
        let cpu_ms = self
            .samples
            .back()
            .filter(|last| last.generation == generation)
            .map_or(cpu_ms, |last| last.cpu_ms.max(cpu_ms));
        self.samples.push_back(Sample {
            sequence,
            at_ms,
            generation,
            cpu_ms,
            uncertain,
        });
        let cutoff = at_ms.saturating_sub(HOUR_MS);
        while self.samples.len() > MAX_SAMPLES
            || self
                .samples
                .front()
                .is_some_and(|first| first.at_ms < cutoff)
        {
            self.samples.pop_front();
        }
        true
    }

    pub(crate) fn last_hour_at(&self, now_ms: u64) -> Option<subc_control::CpuLastHour> {
        let samples: Vec<_> = self
            .samples
            .iter()
            .filter(|sample| sample.at_ms >= now_ms.saturating_sub(HOUR_MS))
            .collect();
        let first = samples.first()?;
        let last = samples.last()?;
        let covered_ms = last.at_ms.checked_sub(first.at_ms)?;
        if covered_ms == 0 {
            return None;
        }
        let mut cpu_ms = 0_u64;
        for (previous, current) in samples.iter().zip(samples.iter().skip(1)) {
            let delta = if previous.generation != current.generation {
                // A replacement process starts a new counter, even when it has
                // already accumulated more CPU than its predecessor.
                current.cpu_ms
            } else {
                // A detached, unreaped descendant can leave a process tree.
                current.cpu_ms.saturating_sub(previous.cpu_ms)
            };
            cpu_ms = cpu_ms.saturating_add(delta);
        }
        Some(
            subc_control::CpuLastHour::new(cpu_ms, covered_ms).with_bound(
                samples
                    .iter()
                    .any(|sample| sample.uncertain)
                    .then_some(subc_control::ResourceBound::Lower),
            ),
        )
    }

    #[cfg(test)]
    fn last_hour(&self) -> Option<subc_control::CpuLastHour> {
        self.last_hour_at(self.samples.back()?.at_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_launch_recovery_credits_only_above_the_high_water_mark() {
        let mut history = History::default();
        for (minute, cpu) in [100, 50, 100, 150].into_iter().enumerate() {
            history.record(minute as u64 * 60_000, 7, cpu);
        }
        assert_eq!(history.last_hour().unwrap().cpu_ms, 50);
    }

    #[test]
    fn stopped_history_expires_without_another_sample() {
        let mut history = History::default();
        history.record(0, 7, 100);
        history.record(60_000, 7, 150);
        assert_eq!(history.last_hour_at(120_000).unwrap().cpu_ms, 50);
        assert!(history.last_hour_at(HOUR_MS + 60_001).is_none());
    }

    #[test]
    fn coverage_under_an_hour_uses_elapsed_wall_time() {
        let mut history = History::default();
        history.record(0, 1, 100);
        assert!(history.last_hour().is_none());
        history.record(60_000, 1, 340);
        let hour = history.last_hour().unwrap();
        assert_eq!(hour.cpu_ms, 240);
        assert_eq!(hour.covered_ms, 60_000);
    }

    #[test]
    fn restart_counts_the_new_process_total_and_tree_loss_is_clamped() {
        let mut history = History::default();
        history.record(0, 1, 100);
        history.record(60_000, 1, 160);
        history.record(120_000, 2, 20);
        history.record(180_000, 2, 10);
        history.record(240_000, 3, 200);
        let hour = history.last_hour().unwrap();
        assert_eq!(hour.cpu_ms, 280);
        assert_eq!(hour.covered_ms, 240_000);
    }

    #[test]
    fn sixty_intervals_cover_an_hour_and_evict_older_cpu() {
        let mut history = History::default();
        for minute in 0..=70 {
            history.record(minute * 60_000, 1, minute * 10);
        }
        let hour = history.last_hour().unwrap();
        assert_eq!(history.samples.len(), 61);
        assert_eq!(hour.cpu_ms, 600);
        assert_eq!(hour.covered_ms, HOUR_MS);
        history.record(200 * 60_000, 1, 2_000);
        assert!(history.last_hour().is_none());
    }
}

#[cfg(test)]
mod transfer_tests {
    use super::*;

    #[test]
    fn same_millisecond_transfer_never_reclassifies_the_earlier_observation() {
        let mut module = History::default();
        let mut agents = History::default();
        module.record_observation(1, 60_000, 0, 10);
        agents.record_observation(1, 60_000, 0, 0);
        // The funding was read later, although the wall clock did not tick.
        module.record_observation(2, 60_000, 0, 110);
        agents.record_observation(2, 60_000, 0, 0);
        assert!(module.transfer_to(&mut agents, 2, 100));
        assert_eq!(module.samples[0].cpu_ms, 10);
        assert_eq!(agents.samples[0].cpu_ms, 0);
    }

    #[test]
    fn a_backward_clock_step_cannot_move_cpu_out_of_an_earlier_observation() {
        let mut module = History::default();
        let mut agents = History::default();
        for (sequence, at, cpu) in [(1, 60_000, 10), (2, 30_000, 110), (3, 120_000, 110)] {
            module.record_observation(sequence, at, 0, cpu);
            agents.record_observation(sequence, at, 0, 0);
        }
        assert!(module.transfer_to(&mut agents, 2, 100));
        assert_eq!(module.last_hour_at(120_000).unwrap().cpu_ms, 0);
        assert_eq!(agents.last_hour_at(120_000).unwrap().cpu_ms, 100);
        assert_eq!(module.samples[0].cpu_ms, 10);
    }

    #[test]
    fn transfers_conserve_every_observation_or_change_neither_row() {
        let mut module = History::default();
        let mut agents = History::default();
        for sequence in 1..=100_u64 {
            let at = (sequence / 3) * 30_000;
            module.record_observation(sequence, at, 0, 1_000 + sequence);
            agents.record_observation(sequence, at, 0, 0);
        }
        let before: Vec<_> = module
            .samples
            .iter()
            .map(|sample| (sample.sequence, sample.cpu_ms))
            .collect();
        for sequence in 1..=100 {
            assert!(module.transfer_to(&mut agents, sequence, 1));
            for ((module, agent), (original_sequence, total)) in
                module.samples.iter().zip(&agents.samples).zip(&before)
            {
                assert_eq!(
                    (module.sequence, agent.sequence),
                    (*original_sequence, *original_sequence)
                );
                assert_eq!(module.cpu_ms + agent.cpu_ms, *total);
            }
        }
        let before_module: Vec<_> = module.samples.iter().map(|sample| sample.cpu_ms).collect();
        let before_agents: Vec<_> = agents.samples.iter().map(|sample| sample.cpu_ms).collect();
        assert!(!module.transfer_to(&mut agents, 1, u64::MAX));
        assert_eq!(
            before_module,
            module
                .samples
                .iter()
                .map(|sample| sample.cpu_ms)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            before_agents,
            agents
                .samples
                .iter()
                .map(|sample| sample.cpu_ms)
                .collect::<Vec<_>>()
        );
    }
    #[test]
    fn expired_finish_funding_changes_baselines_without_creating_current_hour_cpu() {
        let mut module = History::default();
        let mut agents = History::default();
        for (at, cpu) in [(0, 0), (60_000, 100), (3_660_000, 120), (3_720_000, 130)] {
            module.record(at, 0, cpu);
            agents.record(at, 0, 0);
        }
        let before = module.last_hour_at(3_720_000).unwrap().cpu_ms
            + agents.last_hour_at(3_720_000).unwrap().cpu_ms;
        module.transfer_to(&mut agents, 60_000, 100);
        assert_eq!(module.last_hour_at(3_720_000).unwrap().cpu_ms, 10);
        assert_eq!(agents.last_hour_at(3_720_000).unwrap().cpu_ms, 0);
        assert_eq!(
            module.last_hour_at(3_720_000).unwrap().cpu_ms
                + agents.last_hour_at(3_720_000).unwrap().cpu_ms,
            before
        );
        module.record(3_780_000, 0, 40);
        agents.record(3_780_000, 0, 100);
        assert_eq!(module.last_hour_at(3_780_000).unwrap().cpu_ms, 20);
        assert_eq!(
            agents.last_hour_at(3_780_000).unwrap().cpu_ms,
            0,
            "the next sample cannot replay the expired transfer as new agent work"
        );
    }
    #[test]
    fn a_recent_transfer_reclassifies_the_funded_minute_not_the_reply_minute() {
        let mut module = History::default();
        let mut agents = History::default();
        for (at, cpu) in [(0, 0), (60_000, 100), (120_000, 110)] {
            module.record(at, 0, cpu);
            agents.record(at, 0, 0);
        }
        module.transfer_to(&mut agents, 60_000, 40);
        assert_eq!(module.last_hour_at(120_000).unwrap().cpu_ms, 70);
        assert_eq!(agents.last_hour_at(120_000).unwrap().cpu_ms, 40);
        module.record(180_000, 0, 80);
        agents.record(180_000, 0, 40);
        assert_eq!(
            module.last_hour_at(180_000).unwrap().cpu_ms
                + agents.last_hour_at(180_000).unwrap().cpu_ms,
            120
        );
    }
}
