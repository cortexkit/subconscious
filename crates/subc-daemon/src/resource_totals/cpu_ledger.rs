//! Membership-aware CPU floors when no cgroup or Windows job retains exited CPU.

use std::collections::{BTreeMap, BTreeSet};

pub(super) type Identity = (u32, u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Member {
    pub identity: Identity,
    pub parent: Option<Identity>,
    pub own_ms: u64,
    pub reaped_ms: u64,
    /// Initial CPU is eligible only if this identity was born after an accepted
    /// command's baseline. A later first sighting is not proof of recent work.
    pub new_since_baseline: bool,
}

#[derive(Clone, Copy, Debug)]
struct Previous {
    member: Member,
    reap_debt: u64,
}

/// Retired CPU is retained in `total`, not in a lifetime list of dead PIDs.
/// Reap debt belongs to a surviving ancestor and can only suppress its child
/// counter, never that ancestor's own subsequent CPU.
#[derive(Debug, Clone)]
pub(super) struct Ledger {
    members: BTreeMap<Identity, Previous>,
    outside: BTreeMap<Identity, Previous>,
    total: u64,
    baseline: bool,
    initialized: bool,
    revision: Option<u64>,
}

impl Default for Ledger {
    fn default() -> Self {
        Self::new(false)
    }
}

impl Ledger {
    pub(super) fn new(baseline: bool) -> Self {
        Self {
            members: BTreeMap::new(),
            outside: BTreeMap::new(),
            total: 0,
            baseline,
            initialized: false,
            revision: None,
        }
    }

    pub(super) fn total(&self) -> u64 {
        self.total
    }

    /// Identities whose liveness the next observation checks.
    pub(super) fn identities(&self) -> impl Iterator<Item = Identity> + '_ {
        self.members.keys().chain(self.outside.keys()).copied()
    }
    #[cfg(test)]
    pub(super) fn tracked(&self) -> usize {
        self.members.len() + self.outside.len()
    }

    #[cfg(windows)]
    pub(super) fn parent(&self, identity: Identity) -> Option<Identity> {
        self.members
            .get(&identity)
            .and_then(|previous| previous.member.parent)
    }

    #[cfg(windows)]
    pub(super) fn own(&self, identity: Identity) -> Option<u64> {
        self.members
            .get(&identity)
            .map(|previous| previous.member.own_ms)
    }
    #[cfg(windows)]
    pub(super) fn forget_own(&mut self, identity: Identity) {
        self.members.remove(&identity);
        self.outside.remove(&identity);
    }

    /// `alive` includes identities outside this scope. A confirmed live orphan
    /// cannot already have been reaped by its former ancestor.
    /// A stale snapshot makes no change and supplies no new CPU.
    pub(super) fn observe(
        &mut self,
        revision: u64,
        members: impl IntoIterator<Item = Member>,
        alive: &BTreeSet<Identity>,
    ) -> BTreeMap<Identity, u64> {
        if self.revision.is_some_and(|previous| revision <= previous) {
            return BTreeMap::new();
        }
        let current: BTreeMap<_, _> = members
            .into_iter()
            .map(|member| (member.identity, member))
            .collect();
        self.outside.retain(|identity, _| alive.contains(identity));
        let mut debt = BTreeMap::<Identity, u64>::new();
        for (identity, previous) in &self.members {
            if current.contains_key(identity) {
                continue;
            }
            if alive.contains(identity) {
                self.outside.insert(*identity, *previous);
                continue;
            }
            let mut ancestor = previous.member.parent;
            let mut visited = BTreeSet::new();
            while let Some(parent) = ancestor.filter(|parent| visited.insert(*parent)) {
                if current.contains_key(&parent) {
                    let amount = previous
                        .member
                        .own_ms
                        .saturating_add(previous.member.reaped_ms)
                        .saturating_add(previous.reap_debt);
                    let pending = debt.entry(parent).or_default();
                    *pending = pending.saturating_add(amount);
                    break;
                }
                ancestor = self.members.get(&parent).and_then(|old| old.member.parent);
            }
        }
        let mut next = BTreeMap::new();
        let mut increments = BTreeMap::new();
        for (identity, mut member) in current {
            let returning = self.outside.remove(&identity);
            let previous = self.members.get(&identity).or(returning.as_ref());
            // Walks can overlap even when their start revisions are ordered.
            // A per-identity counter read earlier cannot lower its baseline and
            // make the next fresh increment appear twice.
            if let Some(previous) = previous {
                member.own_ms = member.own_ms.max(previous.member.own_ms);
                member.reaped_ms = member.reaped_ms.max(previous.member.reaped_ms);
            }
            let own = member
                .own_ms
                .saturating_sub(previous.map_or(0, |old| old.member.own_ms));
            let reaped = member
                .reaped_ms
                .saturating_sub(previous.map_or(0, |old| old.member.reaped_ms));
            let pending = previous
                .map_or(0, |old| old.reap_debt)
                .saturating_add(debt.get(&identity).copied().unwrap_or(0));
            let absorbed = pending.min(reaped);
            let increase = if (self.baseline
                && (!self.initialized || (previous.is_none() && !member.new_since_baseline)))
                || returning.is_some()
            {
                0
            } else {
                own.saturating_add(reaped - absorbed)
            };
            self.total = self.total.saturating_add(increase);
            increments.insert(identity, increase);
            next.insert(
                identity,
                Previous {
                    member,
                    reap_debt: pending - absorbed,
                },
            );
        }
        self.members = next;
        self.initialized = true;
        self.revision = Some(revision);
        increments
    }
}

/// Allocate only new scope progress. Pending confirmed inside CPU is retained
/// across observations; a skewed sample cannot erase it or debit old module CPU.
#[derive(Debug, Default, Clone)]
pub(super) struct Allocation {
    scope: u64,
    module: u64,
    agents: u64,
    outside: u64,
    pending: u64,
}

impl Allocation {
    pub(super) fn transfer(&mut self, cpu: u64) -> bool {
        let (Some(module), Some(agents)) =
            (self.module.checked_sub(cpu), self.agents.checked_add(cpu))
        else {
            return false;
        };
        self.module = module;
        self.agents = agents;
        true
    }
    #[cfg(windows)]
    pub(super) fn transfer_pending(&mut self, cpu: u64) -> bool {
        let Some(pending) = self.pending.checked_sub(cpu) else {
            return false;
        };
        if !self.transfer(cpu) {
            return false;
        }
        self.pending = pending;
        true
    }
    pub(super) fn bound_pending(&mut self, maximum: u64) -> bool {
        let lost = self.pending > maximum;
        self.pending = self.pending.min(maximum);
        lost
    }
    pub(super) fn reset_pending(&mut self) -> bool {
        let had_pending = self.pending != 0;
        self.pending = 0;
        had_pending
    }
    pub(super) fn observe(&mut self, scope: u64, inside: u64, outside: u64) -> u64 {
        let progress = scope.saturating_sub(self.scope);
        self.scope = self.scope.max(scope);
        self.pending = self.pending.saturating_add(inside);
        let credited = progress.min(self.pending);
        self.pending -= credited;
        self.module = self.module.saturating_add(progress - credited);
        self.agents = self.agents.saturating_add(credited).saturating_add(outside);
        self.outside = self.outside.saturating_add(outside);
        credited
    }

    pub(super) fn rows(&self) -> (u64, u64, u64) {
        (
            self.module,
            self.agents,
            self.scope.saturating_add(self.outside),
        )
    }
}

#[cfg(test)]
#[path = "cpu_ledger/tests.rs"]
pub(super) mod tests;
