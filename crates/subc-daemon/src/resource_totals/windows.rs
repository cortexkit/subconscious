use super::*;

#[cfg(test)]
mod resumption_tests {
    use super::*;

    #[test]
    fn a_live_omitted_root_can_resume_without_overlapping_itself() {
        let pid = std::process::id();
        let native = subc_os::start_time(pid).unwrap();
        let root = AgentCommandRoot {
            pid,
            start_time_ms: subc_os::process_start_time_ms(pid).unwrap(),
        };
        let table = ProcessTable::read().unwrap();
        let members = BTreeSet::from([(pid, native)]);
        let mut module = Module::default();
        module.roots.insert(
            root,
            Command {
                native_start: native,
                accepted_generation: 1,
                finished_requested: true,
                attribution_stopped: true,
                members: members.clone(),
                ..Command::default()
            },
        );
        module.omitted.insert(
            root,
            OmittedRoot {
                native_start: native,
                generation: 1,
                counter: Some(1),
                sequence: 1,
                accepted_at: Instant::now(),
            },
        );
        module.cpu_model.observe(
            counter_model::Kind::WindowsJob,
            1,
            0,
            &[counter_model::Demand {
                owner: Some(root),
                identity: Some((pid, native)),
                cpu: 10,
                before: 0,
                inside_at_both_ends: true,
            }],
            0,
        );
        let accepted = module.accept_checked(
            1,
            root,
            Some(VerifiedRoot {
                root,
                native_start: native,
                inside: true,
                members,
                process: Some(Arc::new(subc_os::Process::open(pid).unwrap().unwrap())),
            }),
            &table,
            now_ms(),
        );
        assert!(!accepted.log_rejected && !accepted.full);
        assert!(!module.roots[&root].finished_requested);
        assert!(module.omitted.is_empty());
        module
            .cpu_model
            .observe(counter_model::Kind::WindowsJob, 1, 10, &[], 0);
        assert_eq!(
            module.cpu_model.rows(),
            (0, 10, 10),
            "old pending CPU still settles once"
        );
        assert!(
            !module.cpu_model.command_funding().contains_key(&root),
            "old pending credit is not new-period command credit"
        );
    }
}

pub(super) struct Observation {
    pub cpu: Vec<((u32, u64), Option<u64>)>,
    pub exited: BTreeSet<(u32, u64)>,
    pub handles: BTreeMap<(u32, u64), Arc<subc_os::Process>>,
    pub exited_at: BTreeMap<(u32, u64), Instant>,
}

pub(super) type StopBoundary = BTreeMap<AgentCommandRoot, BTreeMap<(u32, u64), Option<u64>>>;

#[cfg(test)]
mod stopped_group_tests {
    use super::*;

    #[test]
    fn stopped_windows_command_emits_no_agent_groups() {
        let table = ProcessTable::read().unwrap();
        let member = table.process(std::process::id()).unwrap();
        let root = AgentCommandRoot {
            pid: member.pid,
            start_time_ms: member.start_time_ms,
        };
        let mut command = Command {
            native_start: member.start_time,
            live: true,
            ..Command::default()
        };
        let module = Module::default();
        let live = command.live_members(root, &table).unwrap();
        assert!(!module
            .group_credit
            .groups(&live, true, None)
            .unwrap()
            .is_empty());
        command.attribution_stopped = true;
        command.live = false;
        let stopped = command.live_members(root, &table).unwrap();
        assert!(module
            .group_credit
            .groups(&stopped, true, None)
            .unwrap()
            .is_empty());
    }
}

impl Tracker {
    pub(super) fn observe_windows_commands(
        &self,
        table: &ProcessTable,
    ) -> Option<Vec<(String, AgentCommandRoot, Observation)>> {
        let roots: Vec<_> = {
            let state = self.state.try_lock().ok()?;
            state
                .modules
                .iter()
                .flat_map(|(id, module)| {
                    module
                        .roots
                        .iter()
                        .filter(|(_, command)| !command.finished_requested || !command.root_exited)
                        .map(|(root, command)| (id.clone(), *root, command.clone()))
                })
                .collect()
        };
        Some(
            roots
                .into_iter()
                .map(|(id, root, command)| (id, root, observe(root, &command, table)))
                .collect(),
        )
    }
}

fn observe(root: AgentCommandRoot, command: &Command, table: &ProcessTable) -> Observation {
    let mut handles = command.member_handles.clone();
    let mut exited_at = command.exited_at.clone();
    exited_at.retain(|_, exit| table.started_at().is_none_or(|began| began <= *exit));
    if !command.root_exited {
        if let Some(process) = &command.process {
            handles
                .entry((root.pid, command.native_start))
                .or_insert_with(|| Arc::clone(process));
        }
    }
    if let Some(members) = command.live_members(root, table) {
        for member in members {
            let identity = (member.pid, member.start_time);
            if !exited_at.contains_key(&identity) {
                handles.entry(identity).or_insert(member.process);
            }
        }
    }
    let mut cpu = Vec::new();
    let mut exited = BTreeSet::new();
    handles.retain(|identity, process| {
        let (measured, ended) = if command.attribution_stopped {
            (
                command
                    .pinned_cpu
                    .iter()
                    .find(|(member, _)| member == identity)
                    .and_then(|(_, cpu)| *cpu),
                process.wait_for_exit(Duration::ZERO).ok() == Some(true),
            )
        } else {
            sample_after_exit(
                || process.cumulative_cpu_ms(),
                || process.wait_for_exit(Duration::ZERO).ok(),
            )
        };
        if !command.attribution_stopped || measured.is_some() {
            cpu.push((*identity, measured));
        }
        if ended {
            exited.insert(*identity);
            exited_at.insert(*identity, Instant::now());
            false
        } else {
            true
        }
    });
    Observation {
        cpu,
        exited,
        handles,
        exited_at,
    }
}

fn sample_after_exit(
    mut cpu: impl FnMut() -> Option<u64>,
    mut exited: impl FnMut() -> Option<bool>,
) -> (Option<u64>, bool) {
    match exited() {
        Some(true) => (cpu(), true),
        Some(false) => {
            let reading = cpu();
            match exited() {
                Some(true) => (cpu(), true),
                Some(false) => (reading, false),
                None => (None, false),
            }
        }
        None => (None, false),
    }
}

#[cfg(test)]
mod final_sample_tests {
    #[test]
    fn exit_between_cpu_read_and_probe_requires_a_new_final_read() {
        let mut cpu = [Some(10), Some(15)].into_iter();
        let mut exited = [Some(false), Some(true)].into_iter();
        assert_eq!(
            super::sample_after_exit(|| cpu.next().unwrap(), || exited.next().unwrap()),
            (Some(15), true)
        );
    }
}

#[cfg(test)]
mod lost_final_tests {
    use super::*;

    #[test]
    fn a_failed_or_unclassified_final_sample_remains_lower_after_a_clean_pass() {
        for (classified, final_cpu) in [(true, None), (false, None), (false, Some(50))] {
            let root = AgentCommandRoot {
                pid: 2,
                start_time_ms: 1,
            };
            let child = (3, (11_644_473_600_000_u64 + 1_000) * 10_000);
            let mut module = Module::default();
            module.roots.insert(
                root,
                Command {
                    measured: true,
                    fixed_inside: if classified {
                        BTreeSet::from([child])
                    } else {
                        BTreeSet::new()
                    },
                    ..Command::default()
                },
            );
            for cpu in [vec![(child, final_cpu)], Vec::new()] {
                let exited = cpu.iter().map(|(identity, _)| *identity).collect();
                module.roots.get_mut(&root).unwrap().apply_windows(
                    root,
                    Observation {
                        cpu,
                        exited,
                        handles: BTreeMap::new(),
                        exited_at: BTreeMap::new(),
                    },
                );
                module.trim_windows_pending();
                assert_eq!(
                    module.agent_bound(None),
                    Some(subc_control::ResourceBound::Lower)
                );
            }
            assert!(
                !module.roots[&root].windows_unreadable,
                "the second pass is clean, but cannot recover lost final CPU"
            );
        }
    }
}

impl Command {
    pub(super) fn apply_windows(&mut self, root: AgentCommandRoot, observation: Observation) {
        self.windows_unreadable = observation.cpu.iter().any(|(_, cpu)| cpu.is_none());
        self.root_exited |= observation.exited.contains(&(root.pid, self.native_start));
        let mut pending: BTreeMap<_, _> =
            std::mem::take(&mut self.pinned_cpu).into_iter().collect();
        for (identity, cpu) in observation.cpu {
            #[cfg(test)]
            if let Some(cpu) = cpu {
                self.native_latest_samples
                    .entry(identity)
                    .and_modify(|old| *old = (*old).max(cpu))
                    .or_insert(cpu);
            }
            #[cfg(test)]
            if identity == (root.pid, self.native_start) {
                self.observed_root_cpu = cpu;
            }
            if observation.exited.contains(&identity) {
                let delta = cpu.and_then(|cpu| {
                    self.cpu_floor
                        .own(identity)
                        .map(|old| cpu.saturating_sub(old))
                        .or_else(|| {
                            let born = (identity.1 / 10_000).checked_sub(11_644_473_600_000)?;
                            (self.measured
                                && born
                                    > self
                                        .baseline_at_ms
                                        .saturating_add(subc_os::process_start_time_precision_ms()))
                            .then_some(cpu)
                        })
                });
                let location = if self.fixed_inside.contains(&identity) {
                    Some(0)
                } else if self.fixed_outside.contains(&identity) {
                    Some(1)
                } else {
                    None
                };
                if let (Some(delta), Some(location)) = (delta, location) {
                    self.retired_cpu[location] = self.retired_cpu[location].saturating_add(delta);
                } else {
                    // The identity will not be sampled again. A later clean
                    // pass cannot recover an unknown final counter/location.
                    self.windows_lost_final = true;
                }
                self.cpu_floor.forget_own(identity);
                pending.remove(&identity);
                continue;
            }
            pending
                .entry(identity)
                .and_modify(|previous| {
                    if let Some(cpu) = cpu {
                        *previous = Some(previous.unwrap_or(0).max(cpu));
                    }
                })
                .or_insert(cpu);
        }
        self.pinned_cpu = pending.into_iter().collect();
        self.member_handles = observation.handles;
        self.exited_at = observation.exited_at;
        self.live = !self.attribution_stopped && !self.member_handles.is_empty();
    }
}

impl Module {
    #[cfg(test)]
    pub(super) fn refresh_windows(&mut self, table: &ProcessTable) {
        self.refresh_windows_for_receipt(table, &BTreeSet::new(), &mut StopBoundary::new(), true);
    }

    pub(super) fn refresh_windows_for_receipt(
        &mut self,
        table: &ProcessTable,
        stopping: &BTreeSet<AgentCommandRoot>,
        boundary: &mut StopBoundary,
        first_pass: bool,
    ) {
        for (root, command) in &mut self.roots {
            if command.finished_requested && command.root_exited {
                continue;
            }
            let mut observation = observe(*root, command, table);
            if stopping.contains(root) {
                if first_pass {
                    boundary.insert(*root, observation.cpu.iter().copied().collect());
                } else {
                    // A module's agent_commands message can be retried when
                    // its commit is contended. Only those retries of the same
                    // message are capped to the counters its first attempt
                    // read. A sampler may legitimately credit the still-live
                    // root before the finish commits; never roll that back.
                    let limits = boundary.get(root);
                    if limits.is_none() {
                        // The first attempt recorded no counters for this
                        // root, so CPU after it is unknown: mark the final
                        // tail lost and keep only the earlier measurements.
                        command.windows_lost_final = true;
                    }
                    observation.cpu.retain_mut(|(identity, cpu)| {
                        let limit = limits
                            .and_then(|limits| limits.get(identity))
                            .copied()
                            .or_else(|| {
                                command
                                    .pinned_cpu
                                    .iter()
                                    .find(|(old, _)| old == identity)
                                    .map(|(_, cpu)| *cpu)
                            })
                            .or_else(|| command.cpu_floor.own(*identity).map(Some));
                        let Some(limit) = limit else {
                            return false;
                        };
                        *cpu = match (*cpu, limit) {
                            (Some(cpu), Some(limit)) => Some(cpu.min(limit)),
                            _ => None,
                        };
                        true
                    });
                }
            }
            command.apply_windows(*root, observation);
        }
        self.trim_windows_pending_for_stop(stopping);
    }

    pub(super) fn trim_windows_pending(&mut self) {
        self.trim_windows_pending_for_stop(&BTreeSet::new());
    }

    fn trim_windows_pending_for_stop(&mut self, stopping: &BTreeSet<AgentCommandRoot>) {
        // Final exited counters have already been reduced into aggregate
        // increments. Even an unavailable scope cannot grow a dead-PID list.
        let mut remaining = 1_024;
        for (root, command) in &mut self.roots {
            if (command.windows_lost_final && !command.windows_loss_marked)
                || (command.root_exited
                    && !command.finished_requested
                    && !command.attribution_stopped
                    // This candidate already has an explicit stop snapshot;
                    // a root that exited before a retry is not an unreported exit.
                    && !stopping.contains(root)
                    && !command.windows_exit_loss_marked)
                || (command.live && !command.fixed_outside.is_empty())
            {
                self.agents_lower = true;
                self.recent_agent_loss.mark(self.clock.now());
                command.windows_loss_marked |= command.windows_lost_final;
                command.windows_exit_loss_marked |= command.root_exited;
            }
            let before = command.pinned_cpu.len();
            command
                .pinned_cpu
                .retain(|(identity, _)| command.member_handles.contains_key(identity));
            if command.pinned_cpu.len() > remaining {
                command.pinned_cpu.truncate(remaining);
                command.windows_unreadable = true;
                self.agents_lower = true;
                self.recent_agent_loss.mark(self.clock.now());
            }
            remaining -= command.pinned_cpu.len();
            if before != command.pinned_cpu.len() {
                self.agents_lower = true;
                self.recent_agent_loss.mark(self.clock.now());
            }
            command
                .fixed_inside
                .retain(|identity| command.member_handles.contains_key(identity));
            command
                .fixed_outside
                .retain(|identity| command.member_handles.contains_key(identity));
        }
    }
}

#[cfg(test)]
mod pending_bounds_tests {
    use super::*;
    #[test]
    fn failed_scope_reads_still_prune_dead_pending_pids_and_cap_the_module() {
        let process = Arc::new(subc_os::Process::open(std::process::id()).unwrap().unwrap());
        let root = AgentCommandRoot {
            pid: 2,
            start_time_ms: 1,
        };
        let mut module = Module::default();
        module.roots.insert(root, Command::default());
        for pid in 100..1_200 {
            let identity = (pid, 1);
            module.roots.get_mut(&root).unwrap().apply_windows(
                root,
                Observation {
                    cpu: vec![(identity, Some(1))],
                    exited: BTreeSet::new(),
                    handles: BTreeMap::from([(identity, Arc::clone(&process))]),
                    exited_at: BTreeMap::new(),
                },
            );
            module.trim_windows_pending();
            assert!(module
                .prospective(1, None, &ProcessTable::default(), u64::from(pid), 0)
                .is_none());
            assert_eq!(
                module.roots[&root].pinned_cpu.len(),
                1,
                "an unavailable scope must not retain yesterday's PID list"
            );
        }
        let identities: Vec<_> = (2_000..4_100).map(|pid| (pid, 1)).collect();
        module.roots.get_mut(&root).unwrap().apply_windows(
            root,
            Observation {
                cpu: identities
                    .iter()
                    .map(|identity| (*identity, Some(1)))
                    .collect(),
                exited: BTreeSet::new(),
                handles: identities
                    .iter()
                    .map(|identity| (*identity, Arc::clone(&process)))
                    .collect(),
                exited_at: BTreeMap::new(),
            },
        );
        module.trim_windows_pending();
        assert_eq!(
            module
                .roots
                .values()
                .map(|command| command.pinned_cpu.len())
                .sum::<usize>(),
            1_024
        );
        assert!(module.agents_lower);
        assert!(
            module.roots[&root].windows_unreadable,
            "a cap cannot turn omitted live resources into a confident partial reading"
        );
    }
}

#[cfg(test)]
mod pending_exit_tests {
    use super::*;
    #[test]
    fn a_finish_cannot_retire_its_pinned_root_before_exit_is_confirmed() {
        let pid = std::process::id();
        let start = subc_os::start_time(pid).unwrap();
        let root = AgentCommandRoot {
            pid,
            start_time_ms: subc_os::process_start_time_ms(pid).unwrap(),
        };
        let mut module = Module::default();
        module.roots.insert(
            root,
            Command {
                measured: true,
                native_start: start,
                process: Some(Arc::new(subc_os::Process::open(pid).unwrap().unwrap())),
                ..Command::default()
            },
        );
        module.finish_at(root, None, 1);
        assert!(
            module.roots.contains_key(&root),
            "a finish notification is not native exit confirmation"
        );
        assert!(module.roots[&root].finished_requested);
        assert!(!module.roots[&root].root_exited);
        let table = ProcessTable::read().unwrap();
        module.refresh_windows(&table);
        assert!(!module.roots[&root].root_exited);
        assert!(
            !module.roots[&root].live,
            "retained identity handles are not live attribution"
        );
        assert!(
            module.roots[&root].process.is_some(),
            "the native identity handle remains pinned"
        );
        assert!(
            module.roots[&root].pinned_cpu.is_empty(),
            "a finish cannot acquire new CPU after its validation snapshot"
        );
    }
}
