use super::*;
use counter_model::{Demand, Kind};
use cpu_ledger::Member;

#[cfg(all(test, unix))]
#[path = "prospective/property.rs"]
mod property;

fn members(
    table: &ProcessTable,
    source: &[subc_os::ProcessMember],
    baseline: u64,
) -> Option<Vec<Member>> {
    source
        .iter()
        .map(|member| {
            Some(Member {
                identity: (member.pid, member.start_time),
                parent: table
                    .process(member.parent_pid)
                    .filter(|parent| parent.start_time <= member.start_time)
                    .map(|parent| (parent.pid, parent.start_time)),
                own_ms: member.own_cpu_ms?,
                reaped_ms: member.reaped_cpu_ms?,
                new_since_baseline: member.start_time_ms
                    > baseline.saturating_add(subc_os::process_start_time_precision_ms()),
            })
        })
        .collect()
}

impl Command {
    pub(super) fn live_members(
        &self,
        root: AgentCommandRoot,
        table: &ProcessTable,
    ) -> Option<Vec<subc_os::ProcessMember>> {
        #[cfg(windows)]
        if self.finished_requested || self.attribution_stopped {
            return Some(Vec::new());
        }
        match table.identity_alive(root.pid, self.native_start) {
            None => return None,
            Some(true) => return Some(table.tree(root.pid, self.native_start, true)?.members),
            Some(false) => {}
        }
        let mut live = BTreeMap::new();
        for &(pid, start) in &self.members {
            match table.identity_alive(pid, start) {
                None => return None,
                Some(false) => continue,
                Some(true) => {}
            }
            for member in table.tree(pid, start, false)?.members {
                live.insert((member.pid, member.start_time), member);
            }
        }
        Some(live.into_values().collect())
    }

    pub(super) fn baseline(&mut self, root: AgentCommandRoot, table: &ProcessTable) {
        let Some(live) = self.live_members(root, table) else {
            return;
        };
        if live.is_empty() {
            return;
        }
        let Some(observed) = members(table, &live, table.observed_at_ms()) else {
            return;
        };
        let alive = table
            .members()
            .map(|member| (member.pid, member.start_time))
            .collect();
        self.cpu_floor.observe(table.revision(), observed, &alive);
        #[cfg(all(test, windows))]
        {
            // Retain raw reader inputs so tests can independently check the ledger.
            self.native_baseline_samples = live
                .iter()
                .filter_map(|member| Some(((member.pid, member.start_time), member.own_cpu_ms?)))
                .collect();
            self.native_latest_samples = self.native_baseline_samples.clone();
            self.native_sample_names = live
                .iter()
                .map(|member| ((member.pid, member.start_time), member.name.clone()))
                .collect();
        }
        self.acceptance_cpu = ProcessTable::sum(live).map(|usage| usage.cpu_ms);
        self.measured = true;
        self.baseline_at_ms = table.observed_at_ms();
        self.last_cpu_sequence = table.revision();
    }
}

impl Module {
    /// Reuse only the exact native scope and process snapshot that completed
    /// accounting. A finish may reclassify funded CPU, but cannot measure more.
    pub(super) fn rerender(
        &mut self,
        generation: u128,
        tree: Option<(TreeReading, BTreeSet<(u32, u64)>)>,
        table: &ProcessTable,
        now: u64,
        native_before: u64,
    ) {
        if table.revision() != self.table_revision {
            return;
        }
        let mut candidate = self.clone();
        if let Some(result) =
            candidate.prospective_complete(generation, tree, table, now, native_before)
        {
            candidate.last_readings = Some(result);
            *self = candidate;
        }
    }

    /// Build a complete candidate first. If any required member is unreadable,
    /// retain the previous accounting state but publish no partial measurement.
    pub(super) fn prospective(
        &mut self,
        generation: u128,
        tree: Option<(TreeReading, BTreeSet<(u32, u64)>)>,
        table: &ProcessTable,
        now: u64,
        native_before: u64,
    ) -> Option<(Option<TreeReading>, AgentCommandResources)> {
        self.cpu_model.set_clock(self.clock.clone());
        if table.revision() < self.table_revision {
            return self.last_readings.clone();
        }
        if self
            .observation_completed
            .is_some_and(|completed| table.started_at().is_some_and(|began| began <= completed))
        {
            return self.last_readings.clone();
        }
        let mut candidate = self.clone();
        let Some(result) =
            candidate.prospective_complete(generation, tree, table, now, native_before)
        else {
            self.last_readings = None;
            return None;
        };
        candidate.last_readings = Some(result.clone());
        candidate.observation_completed = Some(Instant::now());
        *self = candidate;
        Some(result)
    }

    fn prospective_complete(
        &mut self,
        generation: u128,
        mut tree: Option<(TreeReading, BTreeSet<(u32, u64)>)>,
        table: &ProcessTable,
        now: u64,
        native_before: u64,
    ) -> Option<(Option<TreeReading>, AgentCommandResources)> {
        let module_before = self.cpu_model.rows().0;
        self.observe_reaped(table, now);
        let inside = tree
            .as_ref()
            .map(|(_, inside)| inside.clone())
            .unwrap_or_default();
        let alive = table
            .members()
            .map(|member| (member.pid, member.start_time))
            .collect();
        let source = tree.as_ref().map(|(tree, _)| tree.source.clone());
        let process_tree = source
            .as_ref()
            .is_none_or(|source| *source == TreeSource::ProcessTree);
        if tree.is_none() && !self.outside_only {
            return None;
        }
        if process_tree && self.generation != 0 && generation != 0 && self.generation != generation
        {
            for command in self.roots.values_mut() {
                command.rebaseline = true;
            }
        }
        let mut scope_increments = BTreeMap::new();
        let raw = if process_tree {
            let observed: Vec<_> = inside
                .iter()
                .map(|(pid, start)| {
                    table
                        .process(*pid)
                        .filter(|member| member.start_time == *start)
                        .cloned()
                })
                .collect::<Option<_>>()?;
            scope_increments =
                self.cpu_floor
                    .observe(table.revision(), members(table, &observed, 0)?, &alive);
            self.cpu_floor.total()
        } else {
            tree.as_ref()?.0.cpu_ms
        };
        let mut demands = Vec::new();
        let mut live_agents = Vec::new();
        let mut excluded = BTreeSet::new();
        for (root, command) in &mut self.roots {
            #[cfg(windows)]
            if command.windows_unreadable {
                return None;
            }
            #[cfg(windows)]
            let live = if command.finished_requested && command.root_exited {
                Vec::new()
            } else {
                command.live_members(*root, table)?
            };
            #[cfg(not(windows))]
            let live = command.live_members(*root, table)?;
            if !command.measured && !live.is_empty() {
                command.baseline_at_ms = table.observed_at_ms();
                command.funding_sequence = table.revision();
                command.acceptance_cpu = ProcessTable::sum(live.clone()).map(|usage| usage.cpu_ms);
            }
            #[cfg(windows)]
            if live.is_empty() && command.live && !command.finished_requested {
                return None;
            }
            #[cfg(windows)]
            if source == Some(TreeSource::Job) {
                for member in &live {
                    let identity = (member.pid, member.start_time);
                    if inside.contains(&identity) {
                        command.fixed_inside.insert(identity);
                    } else if command.member_handles.contains_key(&identity) {
                        command.fixed_outside.insert(identity);
                    }
                }
            }
            #[cfg(not(windows))]
            let observed = members(table, &live, command.baseline_at_ms)?;
            #[cfg(windows)]
            let observed: Vec<_> = command
                .pinned_cpu
                .iter()
                .map(|(identity, cpu)| {
                    let current = table
                        .process(identity.0)
                        .filter(|member| member.start_time == identity.1);
                    Some(Member {
                        identity: *identity,
                        parent: current
                            .and_then(|member| table.process(member.parent_pid))
                            .filter(|parent| parent.start_time <= identity.1)
                            .map(|parent| (parent.pid, parent.start_time))
                            .or_else(|| command.cpu_floor.parent(*identity)),
                        own_ms: (*cpu)?,
                        reaped_ms: 0,
                        new_since_baseline: current.is_some_and(|member| {
                            member.start_time_ms
                                > command
                                    .baseline_at_ms
                                    .saturating_add(subc_os::process_start_time_precision_ms())
                        }),
                    })
                })
                .collect::<Option<_>>()?;
            let increments = if !command.measured && live.is_empty() {
                BTreeMap::new()
            } else if command.rebaseline {
                let progress =
                    command
                        .cpu_floor
                        .observe(table.revision(), observed.clone(), &alive);
                if progress.values().any(|increase| *increase != 0) {
                    self.cpu_model.mark_lower();
                }
                command.cpu_floor = cpu_ledger::Ledger::new(true);
                command
                    .cpu_floor
                    .observe(table.revision(), observed, &alive);
                command.acceptance_cpu = (!live.is_empty())
                    .then(|| ProcessTable::sum(live.clone()).map(|usage| usage.cpu_ms))
                    .flatten();
                command.credited_cpu = 0;
                command.measured_cpu = 0;
                command.funding_sequence = table.revision();
                command.rebaseline = false;
                BTreeMap::new()
            } else {
                command
                    .cpu_floor
                    .observe(table.revision(), observed, &alive)
            };
            command.measured_cpu = command
                .measured_cpu
                .saturating_add(increments.values().sum::<u64>());
            #[cfg(unix)]
            self.finish_cpu.observe_measured(
                *root,
                increments
                    .values()
                    .copied()
                    .fold(0_u64, u64::saturating_add),
                self.clock.now(),
                table.revision(),
            );
            for (identity, increase) in increments {
                if increase == 0 {
                    continue;
                }
                let currently_inside = inside.contains(&identity);
                let previously_inside = command.previous_inside.contains(&identity);
                #[cfg(windows)]
                let (currently_inside, previously_inside) = if source == Some(TreeSource::Job) {
                    if command.fixed_inside.contains(&identity) {
                        (true, true)
                    } else if command.fixed_outside.contains(&identity) {
                        (false, false)
                    } else {
                        // CPU without a confirmed job location cannot be added
                        // as outside work or debited from an unrelated counter.
                        continue;
                    }
                } else {
                    (currently_inside, previously_inside)
                };
                if process_tree {
                    let counted = scope_increments
                        .get(&identity)
                        .copied()
                        .unwrap_or(0)
                        .min(increase);
                    if counted != 0 {
                        demands.push(Demand {
                            owner: Some(*root),
                            identity: Some(identity),
                            cpu: counted,
                            before: 0,
                            inside_at_both_ends: true,
                        });
                    }
                    if !currently_inside || !previously_inside {
                        demands.push(Demand {
                            owner: Some(*root),
                            identity: Some(identity),
                            cpu: increase - counted,
                            before: 0,
                            inside_at_both_ends: false,
                        });
                    }
                } else {
                    demands.push(Demand {
                        owner: Some(*root),
                        identity: Some(identity),
                        cpu: increase,
                        before: command.counter_before,
                        inside_at_both_ends: currently_inside
                            && (previously_inside
                                || live.iter().any(|member| {
                                    (member.pid, member.start_time) == identity
                                        && member.start_time_ms
                                            > command.baseline_at_ms.saturating_add(
                                                subc_os::process_start_time_precision_ms(),
                                            )
                                })),
                    });
                }
            }
            #[cfg(windows)]
            for (location, cpu) in std::mem::take(&mut command.retired_cpu)
                .into_iter()
                .enumerate()
            {
                command.measured_cpu = command.measured_cpu.saturating_add(cpu);
                if cpu != 0 {
                    demands.push(Demand {
                        owner: Some(*root),
                        identity: None,
                        cpu,
                        before: command.counter_before,
                        inside_at_both_ends: location == 0,
                    });
                }
            }
            command.previous_inside = live
                .iter()
                .filter_map(|member| {
                    let identity = (member.pid, member.start_time);
                    inside.contains(&identity).then_some(identity)
                })
                .collect();
            command.counter_before = native_before;
            command.members = live
                .iter()
                .map(|member| (member.pid, member.start_time))
                .collect();
            command.measured |= !live.is_empty();
            command.live = !live.is_empty();
            if table.identity_alive(root.pid, command.native_start) == Some(true) {
                command.last_seen_ms = now;
                command.last_cpu_sequence = table.revision();
            }
            #[cfg(windows)]
            {
                command
                    .pinned_cpu
                    .retain(|(identity, _)| command.member_handles.contains_key(identity));
                command
                    .fixed_inside
                    .retain(|identity| command.member_handles.contains_key(identity));
                command
                    .fixed_outside
                    .retain(|identity| command.member_handles.contains_key(identity));
            }
            excluded.extend(command.previous_inside.iter().copied());
            live_agents.extend(live);
        }
        let kind = if source == Some(TreeSource::Cgroup) {
            Kind::LinuxCgroup
        } else {
            Kind::WindowsJob
        };
        #[cfg(all(test, windows))]
        if source == Some(TreeSource::Job) {
            // Keep the actual kernel inputs independent of attributed rows.
            self.native_job_samples.get_or_insert((raw, raw)).1 = raw;
        }
        let baseline = self
            .roots
            .values()
            .map(|command| command.counter_before)
            .min()
            .unwrap_or(raw);
        self.cpu_model
            .retire(|(pid, start)| table.identity_alive(pid, start) == Some(false));
        let funded = self.cpu_model.observe(
            kind,
            if process_tree {
                0
            } else {
                self.counter.unwrap_or(generation)
            },
            raw,
            &demands,
            baseline,
        );
        for (root, cpu) in self.cpu_model.command_funding() {
            #[cfg(unix)]
            self.finish_cpu.credit_measured(root, cpu);
            if let Some(command) = self.roots.get_mut(&root) {
                command.credited_cpu = command.credited_cpu.saturating_add(cpu);
            }
        }
        let own: Vec<_> = inside
            .difference(&excluded)
            .map(|(pid, _)| table.process(*pid).cloned())
            .collect::<Option<_>>()?;
        let module_cpu = self.cpu_model.rows().0;
        let reserved = self.group_credit.observe(table, &own, module_cpu, funded)?;
        self.finish_cpu.fund(
            module_before.saturating_add(reserved),
            module_cpu,
            self.clock.now(),
            table.revision(),
        );
        #[cfg(windows)]
        {
            let settled: Vec<_> = self
                .roots
                .iter()
                .filter(|(_, command)| command.finished_requested && command.root_exited)
                .map(|(root, _)| *root)
                .collect();
            for root in settled {
                let Some(command) = self.roots.get_mut(&root) else {
                    self.agents_lower = true;
                    self.recent_agent_loss.mark(self.clock.now());
                    continue;
                };
                command.finished_requested = false;
                command.pinned_cpu.clear();
                self.finish_at(root, None, now);
            }
        }
        let (module_cpu, agent_cpu, _) = self.cpu_model.rows();
        if process_tree {
            self.cpu_model.mark_lower();
        }
        self.last_source = if process_tree {
            Some(TreeSource::ProcessTree)
        } else {
            source.clone()
        };
        let bound =
            (process_tree || self.cpu_model.lower()).then_some(subc_control::ResourceBound::Lower);
        let agent_bound = self.agent_bound(bound.clone());
        let agent_groups = self
            .group_credit
            .groups(&live_agents, true, agent_bound.clone())?;
        let agent_usage = if live_agents.is_empty() {
            None
        } else {
            Some(ProcessTable::sum(live_agents)?)
        };
        let agents = AgentCommandResources {
            module_id: String::new(),
            cpu_ms: agent_cpu,
            memory_bytes: agent_usage.as_ref().map_or(0, |usage| usage.memory_bytes),
            process_count: agent_usage.as_ref().map_or(0, |usage| usage.process_count),
            groups: agent_groups,
            bound: agent_bound,
            unmeasured_finished: (self.unmeasured_finished != 0)
                .then_some(self.unmeasured_finished),
            cpu_last_hour: None,
        };
        if let Some((reading, _)) = &mut tree {
            reading.cpu_ms = module_cpu;
            reading.bound = bound.clone();
            let excluded_members: Vec<_> = excluded
                .iter()
                .map(|(pid, _)| table.process(*pid).cloned())
                .collect::<Option<_>>()?;
            if !excluded_members.is_empty() {
                let excluded_usage = ProcessTable::sum(excluded_members)?;
                reading.memory_bytes = reading
                    .memory_bytes
                    .saturating_sub(excluded_usage.memory_bytes);
                reading.process_count = reading
                    .process_count
                    .saturating_sub(excluded_usage.process_count);
            }
            reading.groups = self.group_credit.groups(&own, false, bound)?;
        }
        self.table_revision = table.revision();
        if generation != 0 {
            self.generation = generation;
        }
        Some((tree.map(|(tree, _)| tree), agents))
    }
}
