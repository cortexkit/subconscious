use super::*;
use cpu_ledger::Identity;

#[derive(Clone, Debug, Default)]
pub(super) struct Credits {
    initialized: bool,
    module_row: u64,
    // When an identity moves between module and agents rows, its old row's
    // groups no longer show that identity's credit. Keep the credit in case
    // the same live identity later returns to that row. Only a
    // confirmed process exit retires identity-specific bookkeeping.
    module: BTreeMap<Identity, u64>,
    agents: BTreeMap<Identity, u64>,
    own: BTreeMap<Identity, u64>,
}

impl Credits {
    /// Identities whose credit lasts until a walk confirms their exit.
    pub(super) fn identities(&self) -> impl Iterator<Item = Identity> + '_ {
        self.module
            .keys()
            .chain(self.agents.keys())
            .chain(self.own.keys())
            .copied()
    }
    pub(super) fn module_credit(&self) -> u64 {
        self.module.values().copied().sum()
    }
    pub(super) fn transfer(&mut self, cpu: u64) -> bool {
        let Some(remaining) = self.module_row.checked_sub(cpu) else {
            return false;
        };
        self.module_row = remaining;
        true
    }
    pub(super) fn rebaseline_module(&mut self) {
        self.own.clear();
    }
    pub(super) fn observe(
        &mut self,
        table: &ProcessTable,
        own: &[subc_os::ProcessMember],
        module_row: u64,
        agents: BTreeMap<Identity, u64>,
    ) -> Option<u64> {
        for credits in [&mut self.module, &mut self.agents] {
            credits.retain(|(pid, start), _| table.identity_alive(*pid, *start) != Some(false));
        }
        for (identity, cpu) in agents {
            if table.identity_alive(identity.0, identity.1) != Some(false) {
                let credited = self.agents.entry(identity).or_default();
                *credited = credited.saturating_add(cpu);
            }
        }
        let mut candidates = Vec::new();
        let mut next = BTreeMap::new();
        for member in own {
            let identity = (member.pid, member.start_time);
            let cpu = member.own_cpu_ms?;
            // Newly discovered module-owned work is not an agent baseline:
            // its already consumed CPU belongs to the module too.
            let previous = self.own.get(&identity).copied().unwrap_or(0);
            let demand = if self.initialized {
                cpu.saturating_sub(previous)
            } else {
                cpu
            };
            candidates.push((identity, demand));
            next.insert(identity, cpu.max(previous));
        }
        // Initial module history already belongs to this row, so its live own
        // counters may subdivide it. Largest demand first, then native identity,
        // makes an overcommitted/skewed native snapshot deterministic.
        candidates.sort_by(|(a, ac), (b, bc)| bc.cmp(ac).then_with(|| a.cmp(b)));
        let mut available = module_row.saturating_sub(self.module_row);
        let mut newly_reserved = 0;
        for (identity, demand) in candidates {
            let funded = demand.min(available);
            available -= funded;
            newly_reserved += funded;
            let credit = self.module.entry(identity).or_default();
            *credit = credit.saturating_add(funded);
        }
        self.own = next;
        self.module_row = module_row;
        self.initialized = true;
        Some(newly_reserved)
    }

    pub(super) fn module_parts(
        &self,
        live: &[subc_os::ProcessMember],
        root: Option<Identity>,
        reading: &mut subc_control::TreeReading,
    ) -> Option<()> {
        let root_member = live
            .iter()
            .find(|member| Some((member.pid, member.start_time)) == root);
        reading.own = if let Some(member) = root_member {
            Some(
                subc_control::TreeOwn::new(
                    self.module
                        .get(&(member.pid, member.start_time))
                        .copied()
                        .unwrap_or(0)
                        .min(reading.cpu_ms),
                    member.resources?.memory_bytes.min(reading.memory_bytes),
                )
                .with_bound(reading.bound.clone()),
            )
        } else {
            None
        };
        let children: Vec<_> = live
            .iter()
            .filter(|member| Some((member.pid, member.start_time)) != root)
            .cloned()
            .collect();
        reading.groups = self.groups(&children, false, reading.bound.clone())?;
        let mut cpu = reading
            .cpu_ms
            .saturating_sub(reading.own.as_ref().map_or(0, |own| own.cpu_ms));
        let mut memory = reading
            .memory_bytes
            .saturating_sub(reading.own.as_ref().map_or(0, |own| own.memory_bytes));
        // Native container totals can be smaller than summed per-process RSS.
        // Reserve the root first, then fit child groups in memory rank order.
        // Apply the same row cap to CPU without changing funded identity credit.
        for group in &mut reading.groups {
            group.cpu_ms = group.cpu_ms.min(cpu);
            cpu -= group.cpu_ms;
            group.memory_bytes = group.memory_bytes.min(memory);
            memory -= group.memory_bytes;
        }
        Some(())
    }

    pub(super) fn groups(
        &self,
        live: &[subc_os::ProcessMember],
        agents: bool,
        bound: Option<subc_control::ResourceBound>,
    ) -> Option<Vec<subc_control::ProcessGroup>> {
        let credits = if agents { &self.agents } else { &self.module };
        let mut groups = BTreeMap::<String, subc_control::ProcessGroup>::new();
        let mut seen = BTreeSet::new();
        for member in live {
            let identity = (member.pid, member.start_time);
            if !seen.insert(identity) {
                continue;
            }
            let resource = member.resources?;
            let group = groups.entry(member.name.clone()).or_insert_with(|| {
                subc_control::ProcessGroup::new(member.name.clone(), 0, 0, 0)
                    .with_bound(bound.clone())
            });
            group.count += 1;
            group.cpu_ms = group
                .cpu_ms
                .saturating_add(credits.get(&identity).copied().unwrap_or(0));
            group.memory_bytes = group.memory_bytes.saturating_add(resource.memory_bytes);
        }
        let mut groups: Vec<_> = groups.into_values().collect();
        groups.sort_by(|a, b| {
            b.memory_bytes
                .cmp(&a.memory_bytes)
                .then_with(|| a.name.cmp(&b.name))
        });
        groups.truncate(8);
        Some(groups)
    }

    #[cfg(all(test, unix))]
    pub(super) fn totals(&self) -> (u64, u64) {
        (self.module.values().sum(), self.agents.values().sum())
    }
}
