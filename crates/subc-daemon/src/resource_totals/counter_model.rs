use super::{
    cpu_ledger::{Allocation, Identity},
    overlap::Overlap,
};
use std::collections::{BTreeMap, VecDeque};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    LinuxCgroup,
    WindowsJob,
}

/// One root's prospective progress and the native-counter interval bracketing
/// that progress. Roots are disjoint; one native CPU unit cannot cover two.
#[derive(Clone, Copy, Debug)]
pub(super) struct Demand {
    pub owner: Option<subc_protocol::session::AgentCommandRoot>,
    pub identity: Option<Identity>,
    pub cpu: u64,
    pub before: u64,
    pub inside_at_both_ends: bool,
}

#[derive(Clone, Debug, Default)]
pub(super) struct Model {
    clock: super::Clock,
    recent_loss: super::RecentLoss,
    allocation: Allocation,
    counter: Option<u128>,
    native: u64,
    lifetime: u64,
    overlap: Overlap,
    lower: bool,
    pending: VecDeque<(
        Option<Identity>,
        Option<subc_protocol::session::AgentCommandRoot>,
        u64,
    )>,
    commands: BTreeMap<subc_protocol::session::AgentCommandRoot, u64>,
}

impl Model {
    /// Process identities whose exit this model still waits to learn.
    pub(super) fn identities(&self) -> impl Iterator<Item = Identity> + '_ {
        self.pending.iter().filter_map(|(identity, _, _)| *identity)
    }
    pub(super) fn set_clock(&mut self, clock: super::Clock) {
        self.clock = clock;
    }
    pub(super) fn loss_revision(&self) -> u64 {
        self.recent_loss.revision()
    }
    #[cfg(unix)]
    pub(super) fn transfer(&mut self, cpu: u64) -> bool {
        let moved = self.allocation.transfer(cpu);
        if !moved {
            self.mark_lower();
        }
        moved
    }
    pub(super) fn command_funding(
        &mut self,
    ) -> BTreeMap<subc_protocol::session::AgentCommandRoot, u64> {
        std::mem::take(&mut self.commands)
    }
    #[cfg(windows)]
    pub(super) fn pending_for(&self, root: subc_protocol::session::AgentCommandRoot) -> u64 {
        self.pending
            .iter()
            .filter(|(_, owner, _)| *owner == Some(root))
            .map(|(_, _, cpu)| *cpu)
            .sum()
    }
    #[cfg(windows)]
    pub(super) fn transfer_pending(
        &mut self,
        root: subc_protocol::session::AgentCommandRoot,
        cpu: u64,
    ) -> bool {
        let mut candidate = self.clone();
        if candidate.pending_for(root) < cpu || !candidate.allocation.transfer_pending(cpu) {
            self.mark_lower();
            return false;
        }
        let mut remaining = cpu;
        for (_, owner, pending) in &mut candidate.pending {
            if *owner == Some(root) {
                let amount = remaining.min(*pending);
                *pending -= amount;
                remaining -= amount;
            }
        }
        candidate.pending.retain(|(_, _, cpu)| *cpu != 0);
        *self = candidate;
        true
    }
    pub(super) fn forget_command(&mut self, root: subc_protocol::session::AgentCommandRoot) {
        for (_, owner, _) in &mut self.pending {
            if *owner == Some(root) {
                *owner = None;
            }
        }
    }
    pub(super) fn observe(
        &mut self,
        kind: Kind,
        counter: u128,
        raw: u64,
        demands: &[Demand],
        earliest_baseline: u64,
    ) -> BTreeMap<Identity, u64> {
        self.commands.clear();
        let same = self.counter == Some(counter);
        if !same {
            if self.counter.is_some() && self.allocation.reset_pending() {
                self.mark_lower();
            }
            self.counter = Some(counter);
            self.pending.clear();
            self.native = 0;
            self.overlap = Overlap::default();
        }
        let before = self.native;
        self.native = self.native.max(raw);
        self.lifetime = self.lifetime.saturating_add(self.native - before);
        let mut inside = 0_u64;
        let mut outside = 0_u64;
        let mut funded = BTreeMap::<Identity, u64>::new();
        self.overlap.advance(self.native);
        // Endpoint-based inside attribution assigns the cgroup's CPU to a row;
        // it does not prove where that process spent an unobserved excursion.
        // Keep attribution separate from the physical overlap bound.
        for demand in demands.iter().filter(|demand| demand.inside_at_both_ends) {
            let credit = if kind == Kind::LinuxCgroup {
                demand
                    .cpu
                    .min((self.native - before).saturating_sub(inside))
            } else {
                demand.cpu
            };
            inside = inside.saturating_add(credit);
            if credit != 0 {
                self.pending
                    .push_back((demand.identity, demand.owner, credit));
            }
        }
        for demand in demands.iter().filter(|demand| !demand.inside_at_both_ends) {
            let overlap = match kind {
                Kind::LinuxCgroup => self.overlap.consume(demand.before, self.native, demand.cpu),
                Kind::WindowsJob => 0,
            };
            outside = outside.saturating_add(demand.cpu.saturating_sub(overlap));
            if let Some(identity) = demand.identity {
                *funded.entry(identity).or_default() += demand.cpu.saturating_sub(overlap);
            }
            if let Some(owner) = demand.owner {
                *self.commands.entry(owner).or_default() += demand.cpu.saturating_sub(overlap);
            }
            if kind == Kind::LinuxCgroup && demand.cpu != 0 && self.native > demand.before {
                self.mark_lower();
            }
        }
        let retained = self
            .pending
            .iter()
            .fold(0_u64, |sum, (_, _, cpu)| sum.saturating_add(*cpu));
        // Lost identity metadata cannot authorize a debit from the module.
        // Restrict the allocator before publishing either side of that debit.
        if self
            .allocation
            .bound_pending(retained.saturating_sub(inside))
        {
            self.mark_lower();
        }
        let mut credit = self.allocation.observe(self.lifetime, inside, outside);
        while credit != 0 {
            let Some((identity, owner, pending)) = self.pending.front_mut() else {
                self.mark_lower();
                break;
            };
            let amount = credit.min(*pending);
            credit -= amount;
            *pending -= amount;
            if let Some(identity) = identity {
                *funded.entry(*identity).or_default() += amount;
            }
            if let Some(owner) = owner {
                *self.commands.entry(*owner).or_default() += amount;
            }
            if *pending == 0 {
                self.pending.pop_front();
            }
        }
        self.overlap.discard_before(earliest_baseline);
        funded
    }

    /// Exited work still waits for its share of the measured native CPU total,
    /// but its retained increment no longer needs a PID.
    pub(super) fn retire(&mut self, gone: impl Fn(Identity) -> bool) {
        for (identity, _, _) in &mut self.pending {
            if identity.is_some_and(&gone) {
                *identity = None;
            }
        }
        let mut compact = BTreeMap::<
            (
                Option<Identity>,
                Option<subc_protocol::session::AgentCommandRoot>,
            ),
            u64,
        >::new();
        for (identity, owner, cpu) in self.pending.drain(..) {
            let amount = compact.entry((identity, owner)).or_default();
            *amount = amount.saturating_add(cpu);
        }
        self.pending = compact
            .into_iter()
            .map(|((identity, owner), cpu)| (identity, owner, cpu))
            .collect();
    }

    pub(super) fn rows(&self) -> (u64, u64, u64) {
        self.allocation.rows()
    }
    pub(super) fn lower(&self) -> bool {
        self.lower
    }
    pub(super) fn mark_lower(&mut self) {
        self.lower = true;
        self.recent_loss.mark(self.clock.now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_hourly_loss_expires_without_repairing_cumulative_loss() {
        let mut module = super::super::Module {
            declared_generation: Some(0),
            ..Default::default()
        };
        module.record_sample(0, Some(0), Some(0));
        module.cpu_model.mark_lower();
        module.record_sample(60_000, Some(0), Some(0));
        module.record_sample(120_000, Some(0), Some(0));
        assert_eq!(
            module.agents_last_hour(120_000).unwrap().bound,
            Some(subc_control::ResourceBound::Lower)
        );
        module.record_sample(3_720_000, Some(0), Some(0));
        assert_eq!(module.agents_last_hour(3_720_000).unwrap().bound, None);
        assert!(module.cpu_model.lower());
    }

    #[cfg(unix)]
    #[test]
    fn an_impossible_transfer_changes_neither_row_and_marks_the_model_lower() {
        let mut model = Model::default();
        model.observe(Kind::WindowsJob, 1, 10, &[], 0);
        assert!(!model.transfer(11));
        assert_eq!(model.rows(), (10, 0, 10));
        assert!(model.lower());
    }

    #[test]
    fn missing_pending_metadata_cannot_publish_unfunded_agents_cpu() {
        let mut model = Model::default();
        model.observe(Kind::WindowsJob, 1, 0, &[demand(10, 0, true)], 0);
        model.pending.clear();
        let funded = model.observe(Kind::WindowsJob, 1, 5, &[], 0);
        assert!(funded.is_empty());
        assert_eq!(model.rows(), (5, 0, 5));
        assert!(model.lower());
    }

    fn demand(cpu: u64, before: u64, inside: bool) -> Demand {
        Demand {
            owner: None,
            identity: None,
            cpu,
            before,
            inside_at_both_ends: inside,
        }
    }

    #[test]
    fn job_group_funding_keeps_pending_identity_credit_and_retires_only_its_label() {
        let mut job = Model::default();
        let mut command = demand(10, 0, true);
        command.identity = Some((2, 1));
        assert!(job
            .observe(Kind::WindowsJob, 1, 0, &[command], 0)
            .is_empty());
        let paid = job.observe(Kind::WindowsJob, 1, 4, &[], 4);
        assert_eq!(paid, BTreeMap::from([((2, 1), 4)]));
        assert_eq!(job.rows(), (0, 4, 4));
        job.retire(|identity| identity == (2, 1));
        assert!(
            job.observe(Kind::WindowsJob, 1, 10, &[], 10).is_empty(),
            "retired work funds the row, not a live group"
        );
        assert_eq!(job.rows(), (0, 10, 10));
        assert!(job.pending.is_empty());
        assert!(job.observe(Kind::WindowsJob, 1, 12, &[], 12).is_empty());
        assert_eq!(job.rows(), (2, 10, 12));
    }

    #[test]
    fn inside_attribution_does_not_prove_that_an_outside_interval_cannot_overlap_the_same_counter()
    {
        let mut model = Model::default();
        model.observe(Kind::LinuxCgroup, 1, 0, &[], 0);
        // The inside-endpoint command can have burned its 50ms during an
        // unobserved excursion. The outside-endpoint command can have burned
        // its 50ms inside. These snapshots prove only the cgroup's 50ms.
        model.observe(
            Kind::LinuxCgroup,
            1,
            50,
            &[demand(50, 0, true), demand(50, 0, false)],
            50,
        );
        assert_eq!(model.rows(), (0, 50, 50));
        assert!(model.lower());
    }

    #[test]
    fn late_acceptance_does_not_move_historical_cpu_or_create_an_hourly_delta() {
        let mut model = Model::default();
        model.observe(Kind::LinuxCgroup, 1, 100, &[], 100);
        assert_eq!(model.rows(), (100, 0, 100));
        model.observe(Kind::LinuxCgroup, 1, 100, &[demand(0, 100, true)], 100);
        assert_eq!(model.rows(), (100, 0, 100));
        model.observe(Kind::LinuxCgroup, 1, 105, &[demand(5, 100, true)], 105);
        assert_eq!(model.rows(), (100, 5, 105));
    }

    #[test]
    fn linux_endpoint_domain_and_outside_overlap_are_distinct() {
        let mut inside = Model::default();
        inside.observe(Kind::LinuxCgroup, 1, 0, &[], 0);
        inside.observe(Kind::LinuxCgroup, 1, 50, &[demand(50, 0, true)], 50);
        assert_eq!(inside.rows(), (0, 50, 50));
        assert!(!inside.lower());
        let mut outside = Model::default();
        outside.observe(Kind::LinuxCgroup, 1, 0, &[], 0);
        outside.observe(Kind::LinuxCgroup, 1, 50, &[demand(50, 0, false)], 50);
        assert_eq!(outside.rows(), (50, 0, 50));
        assert!(outside.lower());
        outside.observe(Kind::LinuxCgroup, 1, 55, &[demand(20, 50, false)], 55);
        assert_eq!(outside.rows(), (55, 15, 70));
    }

    #[test]
    fn job_pending_increment_survives_the_debit_cap_and_counter_carry() {
        let mut job = Model::default();
        job.observe(Kind::WindowsJob, 1, 0, &[demand(10, 0, true)], 0);
        assert_eq!(job.rows(), (0, 0, 0));
        job.observe(Kind::WindowsJob, 1, 5, &[], 5);
        assert_eq!(job.rows(), (0, 5, 5));
        job.observe(Kind::WindowsJob, 1, 15, &[], 15);
        assert_eq!(job.rows(), (5, 10, 15));
        job.observe(Kind::WindowsJob, 2, 4, &[], 4);
        assert_eq!(job.rows(), (9, 10, 19));
    }
}
