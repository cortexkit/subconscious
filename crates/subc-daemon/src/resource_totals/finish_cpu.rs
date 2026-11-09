use super::*;
use std::collections::VecDeque;

pub(super) const REAP_POOL_RETENTION_MS: u64 = 5 * 60 * 1_000;
const MAX_REAP_INCREMENTS: usize = 256;

#[derive(Clone, Debug)]
struct Bucket {
    observed_at: Duration,
    sequence: u64,
    cpu: u64,
}

#[derive(Clone, Debug, Default)]
pub(super) struct FinishCpu {
    recent_loss: RecentLoss,
    pub(super) sequence: u64,
    reaped: Option<(u64, u64)>,
    readable: bool,
    pool: VecDeque<Bucket>,
    funding: VecDeque<Bucket>,
    #[cfg(unix)]
    measured: VecDeque<(AgentCommandRoot, Bucket)>,
    #[cfg(unix)]
    lost_measured: bool,
}

#[cfg(unix)]
pub(super) struct Claim {
    pub root: AgentCommandRoot,
    pub requested: u64,
    pub measured_uncredited: u64,
    pub since: u64,
    pub baseline_at: u64,
    pub available: u64,
    pub until: u64,
}

fn push(buckets: &mut VecDeque<Bucket>, observed_at: Duration, sequence: u64, cpu: u64) {
    if cpu == 0 {
        return;
    }
    buckets.push_back(Bucket {
        observed_at,
        sequence,
        cpu,
    });
    // If observations outrun the retained attribution metadata, leave those
    // units in the module row. A later claim cannot invent their timestamp.
    while buckets.len() > MAX_REAP_INCREMENTS {
        buckets.pop_front();
    }
}

impl FinishCpu {
    pub(super) fn expire(&mut self, observed_at: Duration) {
        for buckets in [&mut self.pool, &mut self.funding] {
            buckets.retain(|bucket| {
                observed_at.saturating_sub(bucket.observed_at)
                    < Duration::from_millis(REAP_POOL_RETENTION_MS)
            });
        }
        #[cfg(unix)]
        {
            let recent = &mut self.recent_loss;
            let lost = &mut self.lost_measured;
            self.measured.retain(|(_, bucket)| {
                let retained = observed_at.saturating_sub(bucket.observed_at)
                    < Duration::from_millis(REAP_POOL_RETENTION_MS);
                if !retained {
                    *lost = true;
                    recent.mark(bucket.observed_at);
                }
                retained
            });
        }
    }
    pub(super) fn loss_revision(&self) -> u64 {
        self.recent_loss.revision()
    }

    #[cfg(unix)]
    pub(super) fn lower(&self) -> bool {
        self.lost_measured
    }

    #[cfg(unix)]
    pub(super) fn observe_measured(
        &mut self,
        root: AgentCommandRoot,
        cpu: u64,
        observed_at: Duration,
        sequence: u64,
    ) {
        self.expire(observed_at);
        if cpu == 0 {
            return;
        }
        self.measured.push_back((
            root,
            Bucket {
                observed_at,
                sequence,
                cpu,
            },
        ));
        while self.measured.len() > MAX_REAP_INCREMENTS {
            if let Some((_, lost)) = self.measured.pop_front() {
                self.lost_measured = true;
                self.recent_loss.mark(lost.observed_at);
            }
        }
    }
    #[cfg(unix)]
    pub(super) fn credit_measured(&mut self, root: AgentCommandRoot, mut cpu: u64) {
        for (owner, bucket) in &mut self.measured {
            if *owner == root {
                let paid = cpu.min(bucket.cpu);
                cpu -= paid;
                bucket.cpu -= paid;
            }
        }
        self.measured.retain(|(_, bucket)| bucket.cpu != 0);
    }
    #[cfg(unix)]
    pub(super) fn forget_measured(&mut self, root: AgentCommandRoot) {
        let lost = &mut self.lost_measured;
        let recent = &mut self.recent_loss;
        self.measured.retain(|(owner, bucket)| {
            if *owner == root {
                *lost = true;
                recent.mark(bucket.observed_at);
                false
            } else {
                true
            }
        });
    }
    #[cfg(unix)]
    pub(super) fn readable(&self) -> bool {
        self.readable
    }
    pub(super) fn observe_reaped(
        &mut self,
        reading: Option<(u64, u64)>,
        observed_at: Duration,
        sequence: u64,
    ) {
        self.sequence = self.sequence.max(sequence);
        self.expire(observed_at);
        self.readable = reading.is_some();
        let Some((identity, cpu)) = reading else {
            return;
        };
        match self.reaped {
            Some((old, previous)) if old == identity => {
                push(
                    &mut self.pool,
                    observed_at,
                    sequence,
                    cpu.saturating_sub(previous),
                );
                self.reaped = Some((identity, cpu.max(previous)));
            }
            _ => {
                self.pool.clear();
                self.reaped = Some((identity, cpu));
            }
        }
    }
    pub(super) fn fund(&mut self, before: u64, after: u64, observed_at: Duration, sequence: u64) {
        self.expire(observed_at);
        push(
            &mut self.funding,
            observed_at,
            sequence,
            after.saturating_sub(before),
        );
    }
    #[cfg(unix)]
    pub(super) fn claim(&mut self, claim: Claim) -> Vec<(u64, u64)> {
        let Claim {
            root,
            requested,
            measured_uncredited,
            since,
            baseline_at,
            available,
            until,
        } = claim;
        let pool = self
            .pool
            .iter()
            .filter(|bucket| bucket.sequence > since && bucket.sequence <= until)
            .map(|bucket| bucket.cpu)
            .fold(0_u64, u64::saturating_add);
        // Previously measured, uncredited work keeps its exact original
        // observation. It cannot justify an older unrelated funding bucket.
        let mut transfers = self.claim_measured(
            root,
            requested.min(measured_uncredited).min(pool),
            baseline_at,
            available,
            until,
        );
        let paid = transfers.iter().map(|(_, cpu)| *cpu).sum::<u64>();
        // A missing/expired measured entry is not a new tail. Only a report's
        // excess above all measured-uncredited CPU may use post-measurement work.
        transfers.extend(
            self.claim_funding(
                requested
                    .saturating_sub(measured_uncredited)
                    .min(pool.saturating_sub(paid)),
                since.max(baseline_at),
                available.saturating_sub(paid),
                until,
            ),
        );
        let mut spent = transfers.iter().map(|(_, cpu)| *cpu).sum::<u64>();
        for bucket in self
            .pool
            .iter_mut()
            .filter(|bucket| bucket.sequence > since && bucket.sequence <= until)
        {
            let cpu = spent.min(bucket.cpu);
            bucket.cpu -= cpu;
            spent -= cpu;
        }
        transfers
    }
    #[cfg(unix)]
    fn claim_measured(
        &mut self,
        root: AgentCommandRoot,
        requested: u64,
        since: u64,
        available: u64,
        until: u64,
    ) -> Vec<(u64, u64)> {
        let mut remaining = requested.min(available);
        let mut transfers = Vec::new();
        for (owner, measured) in &mut self.measured {
            if *owner != root || measured.sequence <= since || measured.sequence > until {
                continue;
            }
            for funding in self
                .funding
                .iter_mut()
                .filter(|funding| funding.sequence == measured.sequence)
            {
                let paid = remaining.min(measured.cpu).min(funding.cpu);
                if paid != 0 {
                    funding.cpu -= paid;
                    measured.cpu -= paid;
                    remaining -= paid;
                    transfers.push((measured.sequence, paid));
                }
            }
        }
        self.measured.retain(|(_, bucket)| bucket.cpu != 0);
        transfers
    }
    #[cfg(all(test, unix))]
    fn claim_tail(
        &mut self,
        requested: u64,
        since: u64,
        baseline_at: u64,
        available: u64,
        until: u64,
    ) -> Vec<(u64, u64)> {
        self.claim(Claim {
            root: AgentCommandRoot {
                pid: 0,
                start_time_ms: 0,
            },
            requested,
            measured_uncredited: 0,
            since,
            baseline_at,
            available,
            until,
        })
    }
    pub(super) fn claim_funding(
        &mut self,
        requested: u64,
        since: u64,
        available: u64,
        until: u64,
    ) -> Vec<(u64, u64)> {
        let mut remaining = requested.min(available);
        let mut result = Vec::new();
        for bucket in self
            .funding
            .iter_mut()
            .filter(|bucket| bucket.sequence > since && bucket.sequence <= until)
        {
            let cpu = remaining.min(bucket.cpu);
            if cpu != 0 {
                bucket.cpu -= cpu;
                remaining -= cpu;
                result.push((bucket.sequence, cpu));
            }
            if remaining == 0 {
                break;
            }
        }
        result
    }
}

impl Module {
    pub(super) fn observe_reaped(&mut self, table: &ProcessTable, _now: u64) {
        if table.revision() < self.finish_cpu.sequence {
            return;
        }
        self.prune_omitted(table);
        let reading = self
            .pid
            .and_then(|pid| table.process(pid))
            .and_then(|member| Some((member.start_time, member.reaped_cpu_ms?)));
        self.finish_cpu
            .observe_reaped(reading, self.clock.now(), table.revision());
        for (root, command) in &mut self.roots {
            // Revisions order walk starts, not their completion. An overlapping
            // walk begun before acceptance cannot prove this root disappeared.
            if command
                .accepted_at
                .is_some_and(|accepted| table.started_at().is_none_or(|began| began <= accepted))
            {
                continue;
            }
            if table.identity_alive(root.pid, command.native_start) == Some(false) {
                command.first_gone_sequence.get_or_insert(table.revision());
                command.first_gone_at.get_or_insert(self.clock.now());
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    #[test]
    fn missing_measurement_is_cumulative_lower_but_only_one_hour_is_affected() {
        let mut module = Module {
            declared_generation: Some(0),
            ..Default::default()
        };
        module.record_sample(0, Some(0), Some(0));
        module.finish_cpu.observe_measured(
            AgentCommandRoot {
                pid: 1,
                start_time_ms: 1,
            },
            10,
            ms(10),
            1,
        );
        module.finish_cpu.expire(ms(300_010));
        module.record_sample(60_000, Some(0), Some(0));
        module.record_sample(120_000, Some(0), Some(0));
        assert!(module.finish_cpu.lower());
        assert_eq!(
            module.agents_last_hour(120_000).unwrap().bound,
            Some(subc_control::ResourceBound::Lower)
        );
        module.record_sample(3_720_000, Some(0), Some(0));
        assert_eq!(module.agents_last_hour(3_720_000).unwrap().bound, None);
        assert!(module.finish_cpu.lower());
    }

    #[test]
    fn measured_uncredited_entries_are_bounded_and_expiry_marks_a_minimum() {
        let mut pool = FinishCpu::default();
        for pid in 1..=257 {
            pool.observe_measured(
                AgentCommandRoot {
                    pid,
                    start_time_ms: 1,
                },
                1,
                ms(60_000),
                2,
            );
        }
        assert_eq!(pool.measured.len(), 256);
        assert_eq!(pool.measured.front().unwrap().0.pid, 2);
        assert!(pool.lower());
        let mut pool = FinishCpu::default();
        pool.observe_measured(
            AgentCommandRoot {
                pid: 1,
                start_time_ms: 1,
            },
            100,
            ms(60_000),
            2,
        );
        pool.expire(ms(359_999));
        assert_eq!(pool.measured.len(), 1);
        assert!(!pool.lower());
        pool.expire(ms(360_000));
        assert!(pool.measured.is_empty());
        assert!(pool.lower());
    }

    #[test]
    fn staggered_baselines_keep_the_later_commands_reaped_cpu_available() {
        let mut pool = FinishCpu::default();
        pool.observe_reaped(Some((1, 0)), ms(60_000), 1);
        pool.observe_reaped(Some((1, 10)), ms(120_000), 2);
        pool.fund(0, 10, ms(120_000), 2);
        pool.observe_reaped(Some((1, 20)), ms(180_000), 3);
        pool.fund(10, 20, ms(180_000), 3);
        assert_eq!(pool.claim_tail(10, 1, 1, 20, 3), vec![(2, 10)]);
        assert_eq!(
            pool.claim_tail(10, 2, 2, 10, 3),
            vec![(3, 10)],
            "the delayed older finish cannot consume the later command's pool"
        );
    }
    #[test]
    fn finished_claims_cannot_exceed_unclaimed_daemon_reaped_cpu() {
        let mut pool = FinishCpu::default();
        pool.observe_reaped(Some((1, 0)), ms(0), 0);
        pool.fund(0, 100, ms(1), 1);
        pool.observe_reaped(Some((1, 7)), ms(2), 2);
        assert_eq!(
            pool.claim_tail(90, 0, 0, 100, u64::MAX)
                .iter()
                .map(|(_, cpu)| cpu)
                .sum::<u64>(),
            7
        );
        assert!(
            pool.claim_tail(90, 0, 0, 93, u64::MAX).is_empty(),
            "the next finish cannot spend the same reaped units twice"
        );
    }
    #[test]
    fn unclassified_funding_uses_the_oldest_eligible_minute() {
        let mut pool = FinishCpu::default();
        pool.fund(0, 100, ms(60_000), 60_000);
        pool.fund(100, 110, ms(120_000), 120_000);
        assert_eq!(pool.claim_funding(10, 0, 110, u64::MAX), vec![(60_000, 10)]);
    }
    #[test]
    fn reaped_increment_retention_evicts_the_oldest_at_the_hard_module_cap() {
        let mut pool = FinishCpu::default();
        pool.observe_reaped(Some((1, 0)), ms(0), 0);
        for at in 1..=300 {
            pool.observe_reaped(Some((1, at)), ms(at), at);
        }
        assert_eq!(pool.pool.len(), 256);
        assert_eq!(pool.pool.front().unwrap().observed_at, ms(45));
        assert_eq!(pool.pool.back().unwrap().observed_at, ms(300));
        pool.fund(0, 300, ms(301), 301);
        let paid = pool
            .claim_tail(300, 0, 0, 300, u64::MAX)
            .iter()
            .map(|(_, cpu)| cpu)
            .sum::<u64>();
        assert_eq!(
            paid, 256,
            "evicted increments cannot be spent from another pool"
        );
        let mut funding = FinishCpu::default();
        for at in 1..=300 {
            funding.fund(at - 1, at, ms(at), at);
        }
        assert_eq!(funding.funding.len(), 256);
        assert_eq!(funding.funding.front().unwrap().observed_at, ms(45));
        funding.expire(ms(300 + REAP_POOL_RETENTION_MS));
        assert!(
            funding.funding.is_empty(),
            "funding timestamps have the same finite retention as reap increments"
        );
    }
}
