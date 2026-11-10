//! Shared resource snapshots, command attribution and minute CPU sampling.

mod accounting;
mod commit_fence;
mod committed;
mod counter_model;
pub(crate) mod declaration_loss;
mod finish_cpu;
mod funded_groups;
mod overlap;
mod pending;
pub(crate) use commit_fence::CommitFence;
#[cfg(test)]
mod audit_tests;
#[cfg(test)]
mod committed_tests;
mod cpu_ledger;
mod prospective;
mod time;
use self::time::{Clock, RecentLoss};
mod validation;
#[cfg(windows)]
mod windows;
use validation::{Prepared, VerifiedRoot};

type TableCache = Arc<Mutex<Option<(Instant, Arc<ProcessTable>)>>>;
#[cfg(test)]
type OwnProbe = Arc<(
    std::sync::mpsc::Sender<()>,
    Mutex<std::sync::mpsc::Receiver<()>>,
)>;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use subc_control::{AgentCommandResources, CpuLastHour, DaemonResources, TreeReading, TreeSource};
use subc_os::{ProcessTable, TreeUsage};
use subc_protocol::session::{AgentCommandFinished, AgentCommandRoot};

use crate::resource_history::History;
use crate::supervise::{SupervisedModule, SupervisorHandle};

#[derive(Debug, Default)]
pub(crate) struct Tracker {
    clock: Clock,
    daemon_version: String,
    cache: TableCache,
    refreshing: Arc<AtomicBool>,
    state: Mutex<State>,
    committed: Mutex<committed::View>,
    walk: tokio::sync::Mutex<()>,
    view_refreshing: Arc<AtomicBool>,
    #[cfg(test)]
    view_refreshes: std::sync::atomic::AtomicUsize,
    declaration_losses: Mutex<BTreeMap<String, Arc<declaration_loss::Losses>>>,
    wake: tokio::sync::Notify,
    #[cfg(test)]
    own_probe: Option<OwnProbe>,
    #[cfg(test)]
    revision_conflicts: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    validation_table: Mutex<Option<ValidationTable>>,
    #[cfg(test)]
    sampler_tables: Arc<Mutex<std::collections::VecDeque<ProcessTable>>>,
    #[cfg(test)]
    sampler_reads: Arc<std::sync::atomic::AtomicUsize>,
}

#[cfg(test)]
#[derive(Debug)]
struct ValidationTable {
    table: ProcessTable,
    first_delay: Option<Duration>,
    reads: Arc<std::sync::atomic::AtomicUsize>,
}

#[derive(Clone, Copy)]
pub(crate) struct DeclarationArrival {
    queued_at: Duration,
    declared_at: Instant,
}

#[derive(Debug, Default, Clone)]
pub(crate) struct Declaration {
    pub started: Vec<AgentCommandRoot>,
    pub finished: Vec<AgentCommandFinished>,
    pub known: Option<Vec<AgentCommandRoot>>,
}

#[derive(Default)]
pub(crate) struct Receipt {
    finish_until: Option<u64>,
    loss_epoch: Option<u64>,
    #[cfg(windows)]
    windows_stop_cpu: windows::StopBoundary,
}

#[derive(Debug, Default, Clone)]
struct State {
    revision: u64,
    modules: BTreeMap<String, Module>,
    daemon: History,
}

/// Blocking work prepares a candidate. Only an accepted, revision-fenced
/// result may publish measurements or append minute history.
struct Observation {
    expected_revision: u64,
    state: State,
    readings: Readings,
    generations: BTreeMap<String, u128>,
}

#[cfg(all(test, windows))]
type NativeCpuSamples = BTreeMap<(u32, u64), u64>;

#[derive(Debug, Default, Clone)]
struct Module {
    declared_generation: Option<u128>,
    sampled_model_loss: u64,
    sampled_agent_losses: [u64; 4],
    clock: Clock,
    recent_agent_loss: RecentLoss,
    last_source: Option<TreeSource>,
    finish_cpu: finish_cpu::FinishCpu,
    group_credit: funded_groups::Credits,
    cpu_floor: cpu_ledger::Ledger,
    cpu_model: counter_model::Model,
    table_revision: u64,
    observation_before: u64,
    last_readings: Option<(Option<TreeReading>, AgentCommandResources)>,
    observation_completed: Option<Instant>,
    generation: u128,
    counter: Option<u128>,
    counter_owner: Option<crate::subtree_resources::Target>,
    pid: Option<u32>,
    outside_only: bool,
    roots: BTreeMap<AgentCommandRoot, Command>,
    pending: BTreeMap<AgentCommandRoot, pending::PendingRoot>,
    finished: BTreeMap<AgentCommandRoot, u64>,
    finished_live: BTreeMap<AgentCommandRoot, (u64, Instant)>,
    omitted: BTreeMap<AgentCommandRoot, OmittedRoot>,
    rejected: BTreeMap<AgentCommandRoot, u64>,
    unmeasured_finished: u64,
    agents_lower: bool,
    #[cfg(all(test, windows))]
    finished_root_cpu: Option<u64>,
    #[cfg(all(test, windows))]
    finished_native_samples: Option<(NativeCpuSamples, NativeCpuSamples)>,
    #[cfg(all(test, windows))]
    finished_native_names: BTreeMap<(u32, u64), String>,
    #[cfg(all(test, windows))]
    native_job_samples: Option<(u64, u64)>,
    declaration_loss: Arc<declaration_loss::Losses>,
    #[cfg(windows)]
    retired_handles: Vec<Arc<subc_os::Process>>,
    history: History,
    agents_history: History,
}

#[derive(Debug, Default, Clone)]
struct Command {
    #[cfg(all(test, windows))]
    observed_root_cpu: Option<u64>,
    #[cfg(all(test, windows))]
    native_baseline_samples: BTreeMap<(u32, u64), u64>,
    #[cfg(all(test, windows))]
    native_latest_samples: BTreeMap<(u32, u64), u64>,
    #[cfg(all(test, windows))]
    native_sample_names: BTreeMap<(u32, u64), String>,
    acceptance_cpu: Option<u64>,
    credited_cpu: u64,
    last_cpu_sequence: u64,
    funding_sequence: u64,
    first_gone_sequence: Option<u64>,
    first_gone_at: Option<Duration>,
    measured_cpu: u64,
    accepted_at: Option<Instant>,
    #[cfg(windows)]
    finished_requested: bool,
    #[cfg(windows)]
    attribution_stopped: bool,
    rebaseline: bool,
    cpu_floor: cpu_ledger::Ledger,
    baseline_at_ms: u64,
    counter_before: u64,
    previous_inside: BTreeSet<(u32, u64)>,
    #[cfg(windows)]
    pinned_cpu: Vec<((u32, u64), Option<u64>)>,
    #[cfg(windows)]
    retired_cpu: [u64; 2],
    #[cfg(windows)]
    windows_unreadable: bool,
    #[cfg(windows)]
    windows_lost_final: bool,
    #[cfg(windows)]
    windows_loss_marked: bool,
    #[cfg(windows)]
    windows_exit_loss_marked: bool,
    #[cfg(windows)]
    fixed_inside: BTreeSet<(u32, u64)>,
    #[cfg(windows)]
    fixed_outside: BTreeSet<(u32, u64)>,
    measured: bool,
    accepted_generation: u128,
    native_start: u64,
    members: BTreeSet<(u32, u64)>,
    last_seen_ms: u64,
    live: bool,
    #[cfg(windows)]
    process: Option<Arc<subc_os::Process>>,
    #[cfg(windows)]
    member_handles: BTreeMap<(u32, u64), Arc<subc_os::Process>>,
    #[cfg(windows)]
    exited_at: BTreeMap<(u32, u64), Instant>,
    #[cfg(windows)]
    root_exited: bool,
}

#[derive(Debug, Default, Clone)]
pub(crate) struct Readings {
    pub modules: BTreeMap<String, (Option<TreeReading>, Option<CpuLastHour>)>,
    pub daemon: Option<DaemonResources>,
    pub agent_commands: Option<Vec<AgentCommandResources>>,
    pub reasons: BTreeMap<String, &'static str>,
}

/// A complete candidate computed without supervisor locks. Commit only swaps
/// it in if no accounting observation changed the state it was based on.
pub(crate) struct PreparedCommit {
    expected_revision: u64,
    module: Module,
    rejected: Vec<AgentCommandRoot>,
    recovered: Vec<AgentCommandRoot>,
    complete_recovery: Option<u64>,
    lost: Vec<AgentCommandRoot>,
    roots_full: bool,
    omitted_overflow: bool,
}

pub(crate) struct Committed {
    pub rejected: Vec<AgentCommandRoot>,
    pub roots_full: bool,
    _retired: Option<Module>,
}

#[derive(Default)]
struct Application {
    rejected: Vec<AgentCommandRoot>,
    lost: Vec<AgentCommandRoot>,
    roots_full: bool,
    full_resync: bool,
    omitted_overflow: bool,
}

#[derive(Default)]
struct Acceptance {
    log_rejected: bool,
    full: bool,
}

#[derive(Clone, Debug)]
struct OmittedRoot {
    native_start: u64,
    generation: u128,
    counter: Option<u128>,
    sequence: u64,
    accepted_at: Instant,
}

impl Tracker {
    pub(crate) fn declaration_arrival(&self) -> DeclarationArrival {
        DeclarationArrival {
            queued_at: self.clock.now(),
            declared_at: Instant::now(),
        }
    }

    pub(crate) fn retain_started(
        &self,
        module_id: &str,
        generation: u128,
        module_pid: u32,
        started: &[AgentCommandRoot],
        arrival: DeclarationArrival,
        fence: &CommitFence,
    ) -> Option<Committed> {
        let losses = self.declaration_loss(module_id);
        let mut state = self.state.try_lock().ok()?;
        if !fence.claim_commit() {
            return None;
        }
        let module = state.modules.entry(module_id.to_owned()).or_default();
        module.set_clock(self.clock.clone());
        module.declaration_loss = losses;
        let mut rejected = Vec::new();
        let mut lost = Vec::new();
        let now = now_ms();
        for &root in started {
            let result = module.pend_at(generation, module_pid, root, arrival, now);
            if result.log_rejected {
                rejected.push(root);
            }
            if result.full {
                lost.push(root);
            }
        }
        let recovered = declaration_loss::recover(module, started, None);
        declaration_loss::commit(&module.declaration_loss, &recovered, None, &lost);
        state.revision = state.revision.wrapping_add(1);
        self.wake.notify_one();
        Some(Committed {
            rejected,
            roots_full: !lost.is_empty(),
            _retired: None,
        })
    }

    #[cfg(test)]
    pub(crate) fn root_counts_for_test(&self, module_id: &str) -> (usize, usize, usize) {
        let state = self.state.lock().unwrap();
        let module = &state.modules[module_id];
        (
            module.roots.len(),
            module.pending.len(),
            module.finished.len(),
        )
    }

    #[cfg(test)]
    pub(crate) fn inject_model_loss_for_test(&self, id: &str) {
        self.state
            .lock()
            .unwrap()
            .modules
            .get_mut(id)
            .unwrap()
            .cpu_model
            .mark_lower();
    }
    #[cfg(test)]
    pub(crate) fn fill_root_capacity_for_test(&self, id: &str) {
        let mut state = self.state.lock().unwrap();
        let module = state.modules.entry(id.into()).or_default();
        for pid in 1_000_000..1_001_024 {
            module.roots.insert(
                AgentCommandRoot {
                    pid,
                    start_time_ms: 1,
                },
                Command {
                    native_start: 1,
                    live: true,
                    last_seen_ms: now_ms(),
                    ..Command::default()
                },
            );
        }
    }
    #[cfg(test)]
    pub(crate) fn inject_revision_conflicts(&self, count: usize) {
        self.revision_conflicts.store(count, Ordering::SeqCst);
    }

    #[cfg(test)]
    pub(crate) fn inject_validation_table_for_test(
        &self,
        table: ProcessTable,
        first_delay: Duration,
    ) -> Arc<std::sync::atomic::AtomicUsize> {
        let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        *self.validation_table.lock().unwrap() = Some(ValidationTable {
            table,
            first_delay: Some(first_delay),
            reads: Arc::clone(&reads),
        });
        reads
    }

    #[cfg(test)]
    pub(crate) fn clear_validation_table_for_test(&self) {
        *self.validation_table.lock().unwrap() = None;
    }

    #[cfg(test)]
    pub(crate) fn inject_sampler_tables_for_test(
        &self,
        tables: impl IntoIterator<Item = ProcessTable>,
    ) -> Arc<std::sync::atomic::AtomicUsize> {
        *self.sampler_tables.lock().unwrap() = tables.into_iter().collect();
        Arc::clone(&self.sampler_reads)
    }

    #[cfg(test)]
    pub(crate) async fn read_with_native_table_for_test(
        self: &Arc<Self>,
        modules: Vec<SupervisedModule>,
        sample: bool,
        caller: &'static str,
    ) -> Readings {
        // These fixtures check attribution, not native acquisition latency.
        // Collect a fresh real table outside the bounded sampler reader pool.
        self.inject_sampler_tables_for_test([ProcessTable::read().unwrap()]);
        self.read(modules, sample, caller).await
    }

    fn validation_table(&self) -> Option<ProcessTable> {
        #[cfg(test)]
        {
            let injected = self.validation_table.lock().unwrap().as_mut().map(|read| {
                read.reads.fetch_add(1, Ordering::SeqCst);
                (read.table.clone(), read.first_delay.take())
            });
            if let Some((table, delay)) = injected {
                if let Some(delay) = delay {
                    std::thread::sleep(delay);
                }
                return Some(table);
            }
        }
        ProcessTable::read().ok()
    }
    #[cfg(all(test, windows))]
    pub(crate) fn command_process(
        &self,
        id: &str,
        root: AgentCommandRoot,
    ) -> Option<Arc<subc_os::Process>> {
        let state = self.state.lock().ok()?;
        let module = state.modules.get(id)?;
        module
            .roots
            .get(&module.trusted_key(root)?)?
            .process
            .clone()
    }
    fn prune(
        &self,
        launches: &BTreeMap<String, Option<crate::supervise::ResourceLaunch>>,
        now: u64,
    ) {
        self.declaration_losses
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|id, _| launches.contains_key(id));
        #[cfg(windows)]
        let mut retired = Vec::new();
        let Ok(mut state) = self.state.try_lock() else {
            return;
        };
        state.revision = state.revision.wrapping_add(1);
        state.modules.retain(|id, module| {
            let Some(launch) = launches.get(id) else {
                #[cfg(windows)]
                retired.extend(module.take_handles());
                return false;
            };
            module.prune(now);
            let Some(launch) = launch else {
                return true;
            };
            if !launch.enabled {
                #[cfg(windows)]
                retired.extend(module.take_handles());
                return false;
            }
            if launch.generation() != module.generation
                && !module.roots.values().any(|root| root.live)
            {
                let roots: Vec<_> = module.roots.keys().copied().collect();
                for root in roots {
                    module.finish_at(root, None, now);
                }
                module.finished.clear();
                module.rejected.clear();
            }
            true
        });
        #[cfg(windows)]
        for module in state.modules.values_mut() {
            retired.append(&mut module.retired_handles);
        }
        drop(state);
        #[cfg(windows)]
        drop(retired);
    }
    pub(crate) fn new(daemon_version: String) -> Self {
        Self {
            daemon_version,
            ..Self::default()
        }
    }

    pub(crate) fn prepare(
        &self,
        module_id: &str,
        generation: u128,
        module_pid: u32,
        target: crate::subtree_resources::Target,
        batch: Declaration,
        receipt: &mut Receipt,
    ) -> Option<PreparedCommit> {
        let losses = self.declaration_loss(module_id);
        let epoch = *receipt.loss_epoch.get_or_insert(losses.epoch());
        // Take the paired native-counter/table reading before state contention
        // can cause a retry. Even a failed first read closes the finish window;
        // a later attempt cannot borrow subsequently observed CPU.
        let first_pass = receipt.finish_until.is_none();
        let snapshot = validation::Snapshot::read_with(&target, || self.validation_table());
        let finish_until = *receipt.finish_until.get_or_insert_with(|| {
            snapshot
                .as_ref()
                .map_or(0, |snapshot| snapshot.table.revision())
        });
        let snapshot = snapshot?;
        let counter = target
            .has_container()
            .then(|| target.counter_identity())
            .flatten();
        let (expected_revision, mut module) = {
            let state = self.state.try_lock().ok()?;
            (
                state.revision,
                state.modules.get(module_id).cloned().unwrap_or_default(),
            )
        };
        let trusted = {
            let carried = counter.is_some_and(|counter| module.counter == Some(counter));
            let mut trusted: BTreeMap<_, _> = module
                .roots
                .iter()
                .filter(|(_, command)| {
                    command.accepted_generation == generation
                        || (carried && command.accepted_generation == module.generation)
                })
                .map(|(root, command)| (*root, command.native_start))
                .collect();
            for (root, proof) in &module.omitted {
                if proof.generation == generation
                    || counter.is_some_and(|counter| proof.counter == Some(counter))
                {
                    trusted.insert(*root, proof.native_start);
                }
            }
            trusted
        };
        module.declaration_loss = losses;
        let started = batch.started.clone();
        let known = batch.known.clone();
        let prepared = validation::prepare_snapshot(
            module_pid,
            target,
            batch,
            &trusted,
            snapshot,
            finish_until,
        )?;
        let applied = self.apply_prepared_with_receipt(
            &mut module,
            generation,
            prepared,
            receipt,
            first_pass,
        );
        let recovered = declaration_loss::recover(&module, &started, known.as_deref());
        Some(PreparedCommit {
            expected_revision,
            module,
            rejected: applied.rejected,
            recovered,
            complete_recovery: applied.full_resync.then_some(epoch),
            lost: applied.lost,
            roots_full: applied.roots_full,
            omitted_overflow: applied.omitted_overflow,
        })
    }

    #[cfg(test)]
    fn commit(
        &self,
        module_id: &str,
        generation: u128,
        prepared: Prepared,
    ) -> Option<Vec<AgentCommandRoot>> {
        let (expected_revision, mut module) = {
            let state = self.state.try_lock().ok()?;
            (
                state.revision,
                state.modules.get(module_id).cloned().unwrap_or_default(),
            )
        };
        let applied = self.apply_prepared(&mut module, generation, prepared);
        self.commit_fenced(
            module_id,
            generation,
            PreparedCommit {
                expected_revision,
                module,
                rejected: applied.rejected,
                recovered: Vec::new(),
                complete_recovery: None,
                lost: applied.lost,
                roots_full: applied.roots_full,
                omitted_overflow: applied.omitted_overflow,
            },
            &CommitFence::default(),
        )
        .map(|committed| committed.rejected)
    }

    pub(crate) fn commit_fenced(
        &self,
        module_id: &str,
        _generation: u128,
        prepared: PreparedCommit,
        fence: &CommitFence,
    ) -> Option<Committed> {
        let mut state = self.state.try_lock().ok()?;
        #[cfg(test)]
        {
            let left = self.revision_conflicts.load(Ordering::SeqCst);
            if left != 0
                && self
                    .revision_conflicts
                    .compare_exchange(left, left - 1, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
            {
                state.revision = state.revision.wrapping_add(1);
            }
        }
        if state.revision != prepared.expected_revision {
            return None;
        }
        if !fence.claim_commit() {
            return None;
        }
        state.revision = state.revision.wrapping_add(1);
        declaration_loss::commit(
            &prepared.module.declaration_loss,
            &prepared.recovered,
            prepared.complete_recovery,
            &prepared.lost,
        );
        if prepared.omitted_overflow {
            prepared.module.declaration_loss.mark_overflow();
        }
        let old = state.modules.insert(module_id.to_owned(), prepared.module);
        self.wake.notify_one();
        drop(state);
        // The old state may own Windows handles. Return it with the reply so
        // the caller closes handles only after the supervisor locks that exclude
        // launch replacement and nonce rotation have been released.
        Some(Committed {
            rejected: prepared.rejected,
            roots_full: prepared.roots_full,
            _retired: old,
        })
    }

    #[cfg(test)]
    fn apply_prepared(
        &self,
        module: &mut Module,
        generation: u128,
        prepared: Prepared,
    ) -> Application {
        self.apply_prepared_with_receipt(
            module,
            generation,
            prepared,
            &mut Receipt::default(),
            true,
        )
    }

    fn apply_prepared_with_receipt(
        &self,
        module: &mut Module,
        generation: u128,
        prepared: Prepared,
        _receipt: &mut Receipt,
        _first_pass: bool,
    ) -> Application {
        module.set_clock(self.clock.clone());
        module.prune_members(&prepared.table);
        if module.generation == 0 {
            module.generation = generation;
        }
        let now = prepared.at_ms;
        module.pid = Some(prepared.module_pid);
        module.observe_reaped(&prepared.table, now);
        module.prune(now);
        module.observation_before = prepared.before_cpu.map_or(0, |(_, cpu)| cpu);
        let mut applied = Application::default();
        for (root, verified) in prepared.started {
            if let Some(key) = module.pending_key(root) {
                module.pending.remove(&key);
            }
            let result = module.accept_checked(generation, root, verified, &prepared.table, now);
            if result.log_rejected {
                applied.rejected.push(root);
            }
            if result.full {
                applied.roots_full = true;
                applied.lost.push(root);
            }
        }
        for root in prepared.pending {
            let result = module.pend(generation, root, &prepared.table, now);
            if result.log_rejected {
                applied.rejected.push(root);
            }
            if result.full {
                applied.roots_full = true;
                applied.lost.push(root);
            }
        }
        for finished in &prepared.finished {
            if let Some(key) = module.pending_key(AgentCommandRoot {
                pid: finished.pid,
                start_time_ms: finished.start_time_ms,
            }) {
                module.pending.remove(&key);
            }
        }
        // Retained identity-specific process handles can read final CPU after exit.
        // Read those counters before the job total so the job can cover their
        // inside increments. Apply both to one candidate revision, or neither.
        #[cfg(windows)]
        {
            let wanted = prepared.known.as_ref().map(|known| {
                known
                    .iter()
                    .filter_map(|(_, verified)| verified.as_ref())
                    .map(|verified| module.canonical_key(verified.root, verified.native_start))
                    .collect::<BTreeSet<_>>()
            });
            let mut stopping: BTreeSet<_> = module
                .roots
                .keys()
                .filter(|root| wanted.as_ref().is_some_and(|wanted| !wanted.contains(root)))
                .copied()
                .collect();
            stopping.extend(prepared.finished.iter().filter_map(|finished| {
                module.trusted_key(AgentCommandRoot {
                    pid: finished.pid,
                    start_time_ms: finished.start_time_ms,
                })
            }));
            module.refresh_windows_for_receipt(
                &prepared.table,
                &stopping,
                &mut _receipt.windows_stop_cpu,
                _first_pass,
            );
        }
        let scope = if prepared.scope_target.has_container() {
            #[cfg(unix)]
            {
                prepared.scope
            }
            #[cfg(windows)]
            {
                prepared.scope_target.tree(&prepared.table)
            }
        } else {
            prepared
                .table
                .process(prepared.module_pid)
                .and_then(|root| {
                    prepared
                        .table
                        .tree(root.pid, root.start_time, false)
                        .map(|usage| {
                            let identities = usage
                                .members
                                .iter()
                                .map(|member| (member.pid, member.start_time))
                                .collect();
                            (wire(usage, TreeSource::ProcessTree), identities)
                        })
                })
        };
        if let Some(counter) = prepared.scope_target.counter_identity() {
            module.bind_counter(
                generation,
                counter,
                Some(prepared.scope_target),
                Some(prepared.module_pid),
            );
        }
        let _ = module.prospective(
            generation,
            scope.clone(),
            &prepared.table,
            now,
            module.observation_before,
        );
        for root in prepared.finished {
            module.finish_at_observation(
                AgentCommandRoot {
                    pid: root.pid,
                    start_time_ms: root.start_time_ms,
                },
                root.cpu_ms,
                now,
                prepared.finish_until,
            );
        }
        if let Some(known) = prepared.known {
            module.pending.retain(|pending, _| {
                known.iter().any(|(root, _)| {
                    root.pid == pending.pid
                        && root.start_time_ms.abs_diff(pending.start_time_ms)
                            < subc_os::process_start_time_precision_ms()
                })
            });
            let mut required: BTreeSet<_> = module
                .roots
                .iter()
                .filter(|(root, command)| {
                    #[cfg(windows)]
                    if command.finished_requested || command.attribution_stopped {
                        return false;
                    }
                    prepared
                        .table
                        .identity_alive(root.pid, command.native_start)
                        != Some(false)
                })
                .map(|(root, _)| *root)
                .collect();
            module.prune_omitted(&prepared.table);
            required.extend(module.omitted.keys().copied());
            let verified_known = known.iter().all(|(_, verified)| verified.is_some());
            let wanted: BTreeSet<_> = known
                .iter()
                .filter(|(_, verified)| verified.is_some())
                .filter_map(|(_, verified)| {
                    verified
                        .as_ref()
                        .map(|verified| module.canonical_key(verified.root, verified.native_start))
                })
                .collect();
            // Check overlaps before replacing the accepted set: removing old
            // roots first would conceal an overlapping ancestor in the resync.
            for (root, verified) in known {
                if module.pending_key(root).is_some() {
                    continue;
                }
                let result =
                    module.accept_checked(generation, root, verified, &prepared.table, now);
                if result.log_rejected {
                    applied.rejected.push(root);
                }
                if result.full {
                    applied.roots_full = true;
                    applied.lost.push(root);
                }
            }
            applied.full_resync = verified_known
                && required.is_subset(&wanted)
                && wanted
                    .iter()
                    .all(|root| module.trusted_key(*root).is_some());
            let removed: Vec<_> = module
                .roots
                .keys()
                .filter(|root| !wanted.contains(root))
                .copied()
                .collect();
            for root in removed {
                let omitted = required
                    .contains(&root)
                    .then(|| {
                        module.roots.get(&root).map(|command| OmittedRoot {
                            native_start: command.native_start,
                            generation: command.accepted_generation,
                            counter: module.counter,
                            sequence: prepared.table.revision(),
                            accepted_at: command.accepted_at.unwrap_or_else(Instant::now),
                        })
                    })
                    .flatten();
                if omitted.is_some() {
                    applied.lost.push(root);
                }
                module.finish_at(root, None, now);
                if let Some(proof) = omitted {
                    if module.omitted.contains_key(&root) || module.omitted.len() < 1_024 {
                        module.omitted.insert(root, proof);
                    } else {
                        applied.omitted_overflow = true;
                    }
                }
            }
        }
        // Render the changed live set and any bounded finish transfer without
        // reading or charging the same physical CPU a second time.
        module.rerender(
            generation,
            scope,
            &prepared.table,
            now,
            module.observation_before,
        );
        applied.lost.sort();
        applied.lost.dedup();
        applied
    }

    fn cached_table_covers_pending(&self, table: &ProcessTable) -> bool {
        let Ok(state) = self.state.try_lock() else {
            return false;
        };
        let pending_after = state
            .modules
            .values()
            .filter_map(Module::oldest_pending_declaration)
            .min();
        pending_after
            .is_none_or(|declared| table.started_at().is_some_and(|began| began > declared))
    }

    async fn table(&self) -> Option<Arc<ProcessTable>> {
        {
            let Ok(cache) = self.cache.try_lock() else {
                tracing::debug!(
                    cause = "state_contention",
                    phase = "table_cache",
                    "resource pass incomplete"
                );
                return None;
            };
            if let Some((at, table)) = cache.as_ref() {
                if at.elapsed() < Duration::from_secs(1) && self.cached_table_covers_pending(table)
                {
                    return Some(Arc::clone(table));
                }
            }
        }
        if self.refreshing.swap(true, Ordering::AcqRel) {
            tracing::debug!(
                cause = "admission",
                phase = "table_refresh",
                "resource pass incomplete"
            );
            return None;
        }
        struct Refresh(Arc<AtomicBool>);
        impl Drop for Refresh {
            fn drop(&mut self) {
                self.0.store(false, Ordering::Release);
            }
        }
        let refresh = Refresh(Arc::clone(&self.refreshing));
        let cache = Arc::clone(&self.cache);
        #[cfg(test)]
        let supplied = Arc::clone(&self.sampler_tables);
        #[cfg(test)]
        let reads = Arc::clone(&self.sampler_reads);
        crate::subtree_resources::bounded(
            read_deadline(),
            move |_| {
                let _refresh = refresh;
                // Publish even a late result. No cache or attribution lock is
                // held while walking the host's process table.
                #[cfg(test)]
                let table = {
                    reads.fetch_add(1, Ordering::SeqCst);
                    let table = supplied.lock().unwrap().pop_front();
                    table.map_or_else(ProcessTable::read, Ok)
                };
                #[cfg(not(test))]
                let table = ProcessTable::read();
                let table = Arc::new(table.map_err(|error| {
                    tracing::debug!(cause = "unreadable", phase = "process_table", %error, "resource pass incomplete");
                }).ok()?);
                let Ok(mut cache) = cache.try_lock() else {
                    tracing::debug!(cause = "state_contention", phase = "table_publication", "resource pass incomplete");
                    return None;
                };
                *cache = Some((Instant::now(), Arc::clone(&table)));
                Some(table)
            },
            |_| None,
        )
        .await
    }

    #[cfg(all(test, windows))]
    fn declare(
        &self,
        module_id: &str,
        module_pid: u32,
        generation: u128,
        target: crate::subtree_resources::Target,
        batch: Declaration,
    ) -> Committed {
        let prepared = self
            .prepare(
                module_id,
                generation,
                module_pid,
                target,
                batch,
                &mut Receipt::default(),
            )
            .unwrap();
        self.commit_fenced(module_id, generation, prepared, &CommitFence::default())
            .unwrap()
    }

    pub(crate) async fn read(
        self: &Arc<Self>,
        modules: Vec<SupervisedModule>,
        sample: bool,
        _caller: &'static str,
    ) -> Readings {
        let _walk = self.walk.lock().await;
        let now = now_ms();
        let launches: BTreeMap<_, _> = modules
            .into_iter()
            .map(|module| (module.module_id().to_owned(), module.resource_launch()))
            .collect();
        if launches.values().any(Option::is_none) {
            tracing::debug!(
                cause = "state_contention",
                phase = "launch_snapshot",
                "resource pass incomplete"
            );
            return Readings::default();
        }
        self.prune(&launches, now);
        if sample {
            // A minute endpoint must come from this sampler's walk, not from
            // an interactive refresh that happened to finish a moment earlier.
            *self.cache.lock().unwrap_or_else(|p| p.into_inner()) = None;
        }
        let targets: Vec<_> = launches
            .iter()
            .filter_map(|(id, launch)| {
                launch
                    .as_ref()
                    .map(|launch| (id.clone(), launch.target.clone()))
            })
            .collect();
        let before = crate::subtree_resources::bounded(
            read_deadline(),
            move |_| {
                targets
                    .into_iter()
                    .filter_map(|(id, target)| target.counter_cpu().map(|cpu| (id, cpu)))
                    .collect()
            },
            |_| BTreeMap::new(),
        )
        .await;
        let Some(table) = self.table().await else {
            return Readings::default();
        };
        let tracker = Arc::clone(self);
        let observation = crate::subtree_resources::bounded(
            read_deadline(),
            move |deadline| {
                tracker.read_observed_before(launches, table, sample, now, deadline, before)
            },
            |_| None,
        )
        .await;
        observation.map_or_else(Readings::default, |observation| {
            self.commit_observation(observation)
        })
    }

    #[cfg(test)]
    fn read_observed(
        &self,
        launches: BTreeMap<String, Option<crate::supervise::ResourceLaunch>>,
        table: Arc<ProcessTable>,
        sample: bool,
        now: u64,
        deadline: Instant,
    ) -> Readings {
        let before = launches
            .iter()
            .filter_map(|(id, launch)| {
                launch
                    .as_ref()?
                    .target
                    .counter_cpu()
                    .map(|counter| (id.clone(), counter))
            })
            .collect();
        self.read_observed_before(launches, table, sample, now, deadline, before)
            .map_or_else(Readings::default, |observation| {
                self.commit_observation(observation)
            })
    }

    fn read_observed_before(
        &self,
        launches: BTreeMap<String, Option<crate::supervise::ResourceLaunch>>,
        table: Arc<ProcessTable>,
        sample: bool,
        now: u64,
        deadline: Instant,
        before: BTreeMap<String, (u128, u64)>,
    ) -> Option<Observation> {
        let Ok(state) = self.state.try_lock() else {
            tracing::debug!(
                cause = "state_contention",
                phase = "before_observation",
                "resource pass incomplete"
            );
            return None;
        };
        let expected_revision = state.revision;
        drop(state);
        #[cfg(windows)]
        let Some(commands) = self.observe_windows_commands(&table) else {
            tracing::debug!(
                cause = "unreadable",
                phase = "command_observation",
                "resource pass incomplete"
            );
            return None;
        };
        let mut observed = BTreeMap::new();
        for (id, launch) in launches {
            if Instant::now() >= deadline {
                tracing::debug!(
                    cause = "deadline",
                    phase = "module_observation",
                    "resource pass incomplete"
                );
                return None;
            }
            let Some(launch) = launch else {
                tracing::debug!(
                    cause = "state_contention",
                    phase = "launch_observation",
                    "resource pass incomplete"
                );
                return None;
            };
            if !launch.enabled {
                continue;
            }
            let generation = launch.generation();
            let tree = launch
                .pid
                .zip(launch.start)
                .and_then(|(pid, start)| table.tree(pid, start, false))
                .map(|usage| {
                    let identities = usage
                        .members
                        .iter()
                        .map(|member| (member.pid, member.start_time))
                        .collect();
                    (wire(usage, TreeSource::ProcessTree), identities)
                });
            let counter_before = launch.target.counter_identity();
            let mut reading = if launch.target.has_container() {
                launch.target.tree(&table)
            } else if matches!(
                launch.target,
                crate::subtree_resources::Target::Unavailable("read_failed")
            ) {
                None
            } else {
                tree
            };
            if let Some((reading, _)) = &mut reading {
                reading.measured_at_ms = Some(table.observed_at_ms());
            }
            let counter = if launch.target.has_container() {
                launch.target.counter_identity()
            } else {
                Some(generation)
            };
            if launch.target.has_container() && (counter.is_none() || counter != counter_before) {
                reading = None;
            }
            if launch.target.has_container()
                && before.get(&id).map(|(identity, _)| *identity) != counter
            {
                reading = None;
            }
            let outside_only = !matches!(
                launch.target,
                crate::subtree_resources::Target::Unavailable("read_failed")
            ) && !launch.target.has_container()
                && (!launch.alive || launch.pid.is_none());
            observed.insert(
                id,
                (
                    generation,
                    reading,
                    counter,
                    Some(launch.target),
                    launch.pid,
                    outside_only,
                ),
            );
        }
        // Process reads are completed before taking attribution state, including
        // the daemon's own counter (which excludes its reaped module children).
        let pid = std::process::id();
        #[cfg(test)]
        if let Some(probe) = &self.own_probe {
            let _ = probe.0.send(());
            let _ = probe.1.lock().unwrap().recv();
        }
        let own = subc_os::process_start_time_ms(pid)
            .map(|start| (start, crate::child_resources::read_own(pid)));
        if Instant::now() >= deadline {
            tracing::debug!(
                cause = "deadline",
                phase = "after_observation",
                "resource pass incomplete"
            );
            return None;
        }
        #[cfg(windows)]
        let mut retired = Vec::new();
        let Ok(state) = self.state.try_lock() else {
            tracing::debug!(
                cause = "state_contention",
                phase = "observation_commit",
                "resource pass incomplete"
            );
            return None;
        };
        if state.revision != expected_revision {
            tracing::debug!(
                cause = "state_contention",
                phase = "observation_revision",
                "resource pass incomplete"
            );
            return None;
        }
        let mut candidate = state.clone();
        drop(state);
        let state = &mut candidate;
        state.revision = state.revision.wrapping_add(1);
        #[cfg(windows)]
        {
            for (id, root, observation) in commands {
                retired.extend(observation.handles.values().cloned());
                if let Some(command) = state
                    .modules
                    .get_mut(&id)
                    .and_then(|module| module.roots.get_mut(&root))
                {
                    retired.extend(command.member_handles.values().cloned());
                    command.apply_windows(root, observation);
                }
            }
            for module in state.modules.values_mut() {
                module.trim_windows_pending();
            }
        }
        let mut readings = Readings {
            agent_commands: Some(Vec::new()),
            ..Readings::default()
        };
        let generations = observed
            .iter()
            .map(|(id, (generation, ..))| (id.clone(), *generation))
            .collect();
        let mut any_agents_known = false;
        let mut any_agents_unknown = false;
        for id in state.modules.keys() {
            observed
                .entry(id.clone())
                .or_insert((0, None, None, None, None, false));
        }
        for (id, (generation, tree, counter, owner, pid, outside_only)) in observed {
            let module = state.modules.entry(id.clone()).or_default();
            module.set_clock(self.clock.clone());
            module.declaration_loss = self.declaration_loss(&id);
            module.declaration_loss.observe(&table);
            module.observe_reaped(&table, now);
            module.prune_members(&table);
            module.outside_only = outside_only;
            let changed_domain =
                (module.generation != 0 && generation != 0 && module.generation != generation)
                    || module
                        .counter
                        .zip(counter)
                        .is_some_and(|(old, new)| old != new);
            let overlapping = module
                .observation_completed
                .is_some_and(|completed| table.started_at().is_none_or(|began| began <= completed));
            let stale_transition =
                changed_domain && (module.table_revision >= table.revision() || overlapping);
            if !stale_transition {
                if let (Some(_), Some(counter)) = (&tree, counter) {
                    module.bind_counter(generation, counter, owner.clone(), pid);
                }
                module.observation_before = before.get(&id).map_or(0, |(_, cpu)| *cpu);
                module.validate_pending(&id, generation, owner.as_ref(), &table, now);
            }
            let result = if stale_transition || (tree.is_none() && !module.outside_only) {
                None
            } else if module.table_revision >= table.revision() && module.generation == generation {
                module.last_readings.clone()
            } else {
                module.prospective(
                    generation,
                    tree,
                    &table,
                    now,
                    before.get(&id).map_or(0, |(_, cpu)| *cpu),
                )
            };
            let (tree, mut agents) =
                result.map_or((None, None), |(tree, agents)| (tree, Some(agents)));
            if stale_transition {
                readings.reasons.insert(id.clone(), "launch_changed");
            } else if tree.is_none() && !module.outside_only {
                readings.reasons.insert(id.clone(), "unreadable");
            }
            if module.declaration_loss.lower() {
                if let Some(agents) = &mut agents {
                    agents.bound = Some(subc_control::ResourceBound::Lower);
                    for group in &mut agents.groups {
                        group.bound = agents.bound.clone();
                    }
                }
            }
            any_agents_known |= agents.is_some();
            any_agents_unknown |= agents.is_none();
            if sample {
                module.record_sample(
                    now,
                    tree.as_ref().map(|tree| tree.cpu_ms),
                    agents.as_ref().map(|agents| agents.cpu_ms),
                );
            }
            readings
                .modules
                .insert(id.clone(), (tree, module.last_hour(now)));
            let Some(mut agents) = agents else {
                continue;
            };
            agents.module_id = id;
            agents.cpu_last_hour = module.agents_last_hour(now);
            if module.should_emit_agents(agents.cpu_last_hour.as_ref().map(|hour| hour.cpu_ms)) {
                if let Some(readings) = &mut readings.agent_commands {
                    readings.push(agents);
                }
            }
        }
        if any_agents_unknown && !any_agents_known {
            readings.agent_commands = None;
        }
        if let Some((start_time_ms, resources)) = own {
            if sample {
                if let subc_control::ChildResourceUsage::Measured(usage) = &resources {
                    state.daemon.record_observation(
                        table.revision(),
                        now,
                        u128::from(start_time_ms),
                        usage.cpu_user_ms.saturating_add(usage.cpu_system_ms),
                    );
                }
            }
            readings.daemon = Some(
                DaemonResources::new(pid, start_time_ms, self.daemon_version.clone())
                    .with_measured_at_ms(Some(now_ms()))
                    .with_resources(Some(resources))
                    .with_cpu_last_hour(state.daemon.last_hour_at(now)),
            );
        }
        #[cfg(windows)]
        for module in state.modules.values_mut() {
            retired.append(&mut module.retired_handles);
        }
        #[cfg(windows)]
        drop(retired);
        if Instant::now() >= deadline {
            tracing::debug!(
                cause = "deadline",
                phase = "attribution",
                "resource pass incomplete"
            );
            return None;
        }
        Some(Observation {
            expected_revision,
            state: candidate,
            readings,
            generations,
        })
    }

    fn commit_observation(&self, observation: Observation) -> Readings {
        let Ok(mut state) = self.state.try_lock() else {
            tracing::debug!(
                cause = "state_contention",
                phase = "accepted_commit",
                "resource pass incomplete"
            );
            return Readings::default();
        };
        if state.revision != observation.expected_revision {
            tracing::debug!(
                cause = "state_contention",
                phase = "accepted_revision",
                "resource pass incomplete"
            );
            return Readings::default();
        }
        let previous = std::mem::replace(&mut *state, observation.state);
        self.committed
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .publish(&observation.readings, observation.generations);
        drop(state);
        // Native process handles retired by the candidate are released outside
        // attribution state, including on Windows.
        drop(previous);
        observation.readings
    }
}

fn trim_oldest(entries: &mut BTreeMap<AgentCommandRoot, u64>) {
    while entries.len() > 1_024 {
        let Some(oldest) = entries
            .iter()
            .min_by_key(|(_, at)| *at)
            .map(|(root, _)| *root)
        else {
            break;
        };
        entries.remove(&oldest);
    }
}

impl Module {
    fn canonical_key(&self, requested: AgentCommandRoot, native_start: u64) -> AgentCommandRoot {
        self.roots
            .iter()
            .find(|(root, command)| {
                root.pid == requested.pid && command.native_start == native_start
            })
            .map(|(root, _)| *root)
            .or_else(|| {
                self.omitted
                    .iter()
                    .find(|(root, proof)| {
                        root.pid == requested.pid && proof.native_start == native_start
                    })
                    .map(|(root, _)| *root)
            })
            .unwrap_or(requested)
    }

    fn prune_omitted(&mut self, table: &ProcessTable) {
        self.omitted.retain(|root, proof| {
            table.revision() < proof.sequence
                || table
                    .started_at()
                    .is_none_or(|began| began <= proof.accepted_at)
                || table.identity_alive(root.pid, proof.native_start) != Some(false)
        });
    }
    fn set_clock(&mut self, clock: Clock) {
        self.clock = clock.clone();
        self.cpu_model.set_clock(clock);
    }

    fn should_emit_agents(&self, hourly_cpu: Option<u64>) -> bool {
        self.declared_generation == Some(self.generation)
            || self.roots.values().any(|command| command.live)
            || self.declaration_loss.lower()
            || hourly_cpu.map_or(self.cpu_model.rows().1 != 0, |cpu| cpu != 0)
    }
    fn record_sample(&mut self, now: u64, module_cpu: Option<u64>, agents_cpu: Option<u64>) {
        // A cached table may predate a declaration's direct reading. Cached
        // rows still belong to their committed observation, not that old table.
        let model_loss = self.cpu_model.loss_revision();
        let (declaration_loss, missing_live) = self.declaration_loss.sample_uncertainty();
        let agent_losses = [
            model_loss,
            self.recent_agent_loss.revision(),
            self.finish_cpu.loss_revision(),
            declaration_loss,
        ];
        let process_tree = self.last_source == Some(TreeSource::ProcessTree);
        let has_commands =
            self.declared_generation == Some(self.generation) || !self.roots.is_empty();
        if let Some(cpu) = module_cpu {
            if self.history.record_bound_observation(
                self.table_revision,
                now,
                0,
                cpu,
                process_tree || model_loss != self.sampled_model_loss,
            ) {
                self.sampled_model_loss = model_loss;
            }
        }
        if let Some(cpu) = agents_cpu {
            if self.agents_history.record_bound_observation(
                self.table_revision,
                now,
                0,
                cpu,
                missing_live
                    || declaration_loss != self.sampled_agent_losses[3]
                    || agent_losses[1] != self.sampled_agent_losses[1]
                    || agent_losses[2] != self.sampled_agent_losses[2]
                    || (has_commands
                        && (process_tree || model_loss != self.sampled_agent_losses[0])),
            ) {
                self.sampled_agent_losses = agent_losses;
            }
        }
    }
    fn agent_bound(
        &self,
        exact: Option<subc_control::ResourceBound>,
    ) -> Option<subc_control::ResourceBound> {
        #[cfg(unix)]
        let measured_lower = self.finish_cpu.lower();
        #[cfg(not(unix))]
        let measured_lower = false;
        if self.agents_lower || measured_lower || self.declaration_loss.lower() {
            Some(subc_control::ResourceBound::Lower)
        } else {
            exact
        }
    }
    #[cfg(windows)]
    fn take_handles(&mut self) -> Vec<Arc<subc_os::Process>> {
        let mut retired = std::mem::take(&mut self.retired_handles);
        for command in self.roots.values_mut() {
            retired.extend(command.process.take());
            retired.extend(std::mem::take(&mut command.member_handles).into_values());
        }
        retired
    }
    fn bind_counter(
        &mut self,
        generation: u128,
        counter: u128,
        owner: Option<crate::subtree_resources::Target>,
        pid: Option<u32>,
    ) {
        if self.counter.is_some_and(|previous| previous != counter) {
            self.group_credit.rebaseline_module();
            for command in self.roots.values_mut() {
                command.rebaseline = true;
                command.previous_inside.clear();
                #[cfg(windows)]
                {
                    command.fixed_inside.clear();
                    command.fixed_outside.clear();
                }
            }
        }
        if self.counter == Some(counter) && generation != self.generation {
            for command in self.roots.values_mut() {
                if command.accepted_generation == self.generation {
                    command.accepted_generation = generation;
                }
            }
        }
        self.counter = Some(counter);
        // Keep the weak job allocation alive so a new job's allocation cannot
        // reuse an old counter identity after its last strong owner exits.
        self.counter_owner = owner;
        self.pid = pid;
    }
    fn prune(&mut self, now: u64) {
        const RETAIN_MS: u64 = 10 * 60 * 1_000;
        self.finished
            .retain(|_, at| now.saturating_sub(*at) < RETAIN_MS);
        self.rejected
            .retain(|_, at| now.saturating_sub(*at) < RETAIN_MS);
        trim_oldest(&mut self.finished);
        trim_oldest(&mut self.rejected);
        let expired: Vec<_> = self
            .roots
            .iter()
            .filter(|(_, command)| {
                !command.live && now.saturating_sub(command.last_seen_ms) >= RETAIN_MS
            })
            .map(|(root, _)| *root)
            .collect();
        for root in expired {
            self.finish_at(root, None, now);
        }
    }

    fn prune_members(&mut self, table: &ProcessTable) {
        if table.revision() == 0
            || self
                .observation_completed
                .is_some_and(|completed| table.started_at().is_none_or(|began| began <= completed))
        {
            return;
        }
        self.finished_live.retain(|root, (start, accepted)| {
            table.started_at().is_none_or(|began| began <= *accepted)
                || table.identity_alive(root.pid, *start) != Some(false)
        });
        for command in self.roots.values_mut() {
            if command
                .accepted_at
                .is_some_and(|accepted| table.started_at().is_none_or(|began| began <= accepted))
            {
                continue;
            }
            command
                .members
                .retain(|(pid, start)| table.identity_alive(*pid, *start) != Some(false));
        }
    }
    fn trusted_key(&self, root: AgentCommandRoot) -> Option<AgentCommandRoot> {
        self.roots
            .keys()
            .find(|known| {
                known.pid == root.pid
                    && known.start_time_ms.abs_diff(root.start_time_ms)
                        < subc_os::process_start_time_precision_ms()
            })
            .copied()
    }

    #[cfg(test)]
    fn accept(
        &mut self,
        generation: u128,
        requested: AgentCommandRoot,
        verified: Option<VerifiedRoot>,
        table: &ProcessTable,
        now: u64,
    ) -> bool {
        self.accept_checked(generation, requested, verified, table, now)
            .log_rejected
    }

    fn accept_checked(
        &mut self,
        generation: u128,
        requested: AgentCommandRoot,
        verified: Option<VerifiedRoot>,
        table: &ProcessTable,
        now: u64,
    ) -> Acceptance {
        #[cfg(windows)]
        if let Some(process) = verified.as_ref().and_then(|root| root.process.as_ref()) {
            // Defer closing even a rejected handle until attribution unlocks.
            self.retired_handles.push(Arc::clone(process));
        }
        let resume = verified.as_ref().and_then(|verified| {
            self.omitted
                .iter()
                .find(|(root, proof)| {
                    root.pid == verified.root.pid
                        && proof.native_start == verified.native_start
                        && root.start_time_ms.abs_diff(verified.root.start_time_ms)
                            < subc_os::process_start_time_precision_ms()
                })
                .map(|(root, _)| *root)
        });
        if resume.is_none()
            && (self.trusted_key(requested).is_some()
                || self.finished_live.keys().any(|known| {
                    known.pid == requested.pid
                        && known.start_time_ms.abs_diff(requested.start_time_ms)
                            < subc_os::process_start_time_precision_ms()
                })
                || self.finished.keys().any(|known| {
                    known.pid == requested.pid
                        && known.start_time_ms.abs_diff(requested.start_time_ms)
                            < subc_os::process_start_time_precision_ms()
                }))
        {
            return Acceptance::default();
        }
        let retained = self.roots.len() + self.finished_live.len() + self.pending.len();
        let resumed = usize::from(resume.is_some_and(|root| {
            self.finished_live.contains_key(&root) || self.roots.contains_key(&root)
        }));
        let full = verified.is_some() && retained.saturating_sub(resumed) >= pending::ROOT_LIMIT;
        let verified = verified.filter(|_| !full).filter(|candidate| {
            !self.roots.iter().any(|(root, command)| {
                if resume == Some(*root) {
                    return false;
                }
                let identities = table
                    .tree_identities(root.pid, command.native_start, true)
                    .unwrap_or_else(|| command.members.clone());
                !candidate.members.is_disjoint(&identities)
            })
        });
        let Some(verified) = verified else {
            let first = self.rejected.insert(requested, now).is_none();
            trim_oldest(&mut self.rejected);
            return Acceptance {
                log_rejected: first,
                full,
            };
        };
        self.last_readings = None;
        if let Some(root) = resume {
            // Old pending CPU still settles, but cannot debit the resumed
            // command's new prospective baseline a second time.
            self.cpu_model.forget_command(root);
            self.omitted.remove(&root);
            self.finished_live.remove(&root);
            self.finished.remove(&root);
        }
        let root = resume.unwrap_or(verified.root);
        let mut command = Command {
            accepted_at: Some(Instant::now()),
            #[cfg(windows)]
            fixed_inside: if verified.inside {
                BTreeSet::from([(root.pid, verified.native_start)])
            } else {
                BTreeSet::new()
            },
            #[cfg(windows)]
            fixed_outside: if verified.inside {
                BTreeSet::new()
            } else {
                BTreeSet::from([(root.pid, verified.native_start)])
            },
            cpu_floor: cpu_ledger::Ledger::new(true),
            baseline_at_ms: table.observed_at_ms(),
            counter_before: self.observation_before,
            previous_inside: if verified.inside {
                verified.members.clone()
            } else {
                BTreeSet::new()
            },
            measured: table
                .tree(verified.root.pid, verified.native_start, true)
                .is_some(),
            accepted_generation: generation,
            native_start: verified.native_start,
            members: verified.members,
            last_seen_ms: now,
            funding_sequence: table.revision(),
            live: true,
            #[cfg(windows)]
            process: verified.process,
            ..Command::default()
        };
        command.baseline(root, table);
        self.roots.insert(root, command);
        self.declared_generation = Some(generation);
        Acceptance::default()
    }

    #[cfg(test)]
    fn start(
        &mut self,
        _module_id: &str,
        module_pid: u32,
        generation: u128,
        root: AgentCommandRoot,
        _container: &[u32],
        resync: bool,
    ) {
        let batch = Declaration {
            started: if resync { Vec::new() } else { vec![root] },
            known: resync.then_some(vec![root]),
            ..Declaration::default()
        };
        let prepared = validation::prepare(
            module_pid,
            crate::subtree_resources::Target::Unavailable("no_container"),
            batch,
            &self
                .roots
                .iter()
                .map(|(root, command)| (*root, command.native_start))
                .collect(),
            &mut None,
        )
        .unwrap();
        let (root, verified) = prepared
            .started
            .into_iter()
            .chain(prepared.known.into_iter().flatten())
            .next()
            .unwrap();
        self.accept(generation, root, verified, &prepared.table, prepared.at_ms);
    }

    fn finish_at(&mut self, root: AgentCommandRoot, reported: Option<u64>, now: u64) {
        self.finish_at_observation(root, reported, now, self.finish_cpu.sequence);
    }

    fn transfer_finish(&mut self, _root: AgentCommandRoot, sequence: u64, amount: u64) {
        let mut model = self.cpu_model.clone();
        let mut credit = self.group_credit.clone();
        let mut history = self.history.clone();
        let mut agents_history = self.agents_history.clone();
        #[cfg(unix)]
        let moved = model.transfer(amount);
        #[cfg(windows)]
        let moved = model.transfer_pending(_root, amount);
        if moved
            && credit.transfer(amount)
            && history.transfer_to(&mut agents_history, sequence, amount)
        {
            self.cpu_model = model;
            self.group_credit = credit;
            self.history = history;
            self.agents_history = agents_history;
        } else {
            self.agents_lower = true;
            self.recent_agent_loss.mark(self.clock.now());
        }
    }

    fn finish_at_observation(
        &mut self,
        root: AgentCommandRoot,
        reported: Option<u64>,
        now: u64,
        arrival_sequence: u64,
    ) {
        self.finish_cpu.expire(self.clock.now());
        let Some(root) = self.trusted_key(root) else {
            return;
        };
        self.omitted.remove(&root);
        self.last_readings = None;
        #[cfg(windows)]
        {
            let Some(command) = self.roots.get_mut(&root) else {
                self.agents_lower = true;
                self.recent_agent_loss.mark(self.clock.now());
                return;
            };
            if command.finished_requested {
                return;
            }
            // The first validation pass is the final live-attribution snapshot.
            // Later work stays in the measured module tree, not this command.
            command.attribution_stopped = true;
            command.live = false;
            if command.pinned_cpu.iter().any(|(_, cpu)| cpu.is_none()) {
                command.windows_lost_final = true;
                command.pinned_cpu.retain(|(_, cpu)| cpu.is_some());
                command.windows_unreadable = false;
            }
            if !command.root_exited && command.process.is_some() {
                command.finished_requested = true;
                return;
            }
            if command.measured
                && (command.retired_cpu.iter().any(|cpu| *cpu != 0)
                    || command.pinned_cpu.iter().any(|(identity, cpu)| {
                        cpu.is_some_and(|cpu| {
                            command.cpu_floor.own(*identity).is_none_or(|old| cpu > old)
                        })
                    }))
            {
                command.finished_requested = true;
                command.live = false;
                self.finished.insert(root, now);
                trim_oldest(&mut self.finished);
                return;
            }
        }
        let Some(command) = self.roots.remove(&root) else {
            return;
        };
        let loss_at = command.first_gone_at.unwrap_or_else(|| self.clock.now());
        #[cfg(all(test, windows))]
        {
            self.finished_root_cpu = command.observed_root_cpu;
            self.finished_native_samples = Some((
                command.native_baseline_samples.clone(),
                command.native_latest_samples.clone(),
            ));
            self.finished_native_names = command.native_sample_names.clone();
        }
        #[cfg(unix)]
        {
            if let Some(reported) =
                reported.filter(|_| self.finish_cpu.readable() && !command.rebaseline)
            {
                if let Some(baseline) = command.acceptance_cpu {
                    let requested = reported
                        .saturating_sub(baseline)
                        .saturating_sub(command.credited_cpu);
                    let available = self
                        .cpu_model
                        .rows()
                        .0
                        .saturating_sub(self.group_credit.module_credit());
                    let transfers = self.finish_cpu.claim(finish_cpu::Claim {
                        root,
                        requested,
                        measured_uncredited: command
                            .measured_cpu
                            .saturating_sub(command.credited_cpu),
                        since: command.last_cpu_sequence,
                        baseline_at: command.funding_sequence,
                        available,
                        // The receipt's first validation snapshot is eligible,
                        // but no retry or later sampler reading may enlarge it.
                        // An earlier confirmed root exit tightens that boundary.
                        // This is the first queued pass, not network arrival:
                        // admission allows four requests, and each validation
                        // and reply has its own 200 ms deadline.
                        until: command
                            .first_gone_sequence
                            .map_or(arrival_sequence, |gone| gone.min(arrival_sequence)),
                    });
                    let paid = transfers.iter().map(|(_, cpu)| *cpu).sum::<u64>();
                    if paid < requested {
                        self.agents_lower = true;
                        self.recent_agent_loss.mark(loss_at);
                    }
                    for (at, amount) in transfers {
                        self.transfer_finish(root, at, amount);
                    }
                } else {
                    self.agents_lower = true;
                    self.recent_agent_loss.mark(loss_at);
                }
            } else {
                self.agents_lower = true;
                self.recent_agent_loss.mark(loss_at);
            }
        }
        #[cfg(not(unix))]
        let _ = reported;
        #[cfg(windows)]
        {
            if !command.attribution_stopped || !command.measured || command.windows_lost_final {
                self.agents_lower = true;
                self.recent_agent_loss.mark(loss_at);
            }
            let requested = command
                .measured_cpu
                .saturating_sub(command.credited_cpu)
                .min(self.cpu_model.pending_for(root));
            let available = self
                .cpu_model
                .rows()
                .0
                .saturating_sub(self.group_credit.module_credit());
            for (at, amount) in self.finish_cpu.claim_funding(
                requested,
                command.funding_sequence,
                available,
                arrival_sequence,
            ) {
                self.transfer_finish(root, at, amount);
            }
        }
        #[cfg(unix)]
        self.finish_cpu.forget_measured(root);
        self.cpu_model.forget_command(root);
        if !command.measured {
            self.unmeasured_finished = self.unmeasured_finished.saturating_add(1);
        }
        #[cfg(unix)]
        if command.live {
            self.finished_live.insert(
                root,
                (
                    command.native_start,
                    command.accepted_at.unwrap_or_else(Instant::now),
                ),
            );
        }
        self.finished.insert(root, now);
        trim_oldest(&mut self.finished);
        #[cfg(windows)]
        {
            self.retired_handles.extend(command.process);
            self.retired_handles
                .extend(command.member_handles.into_values());
        }
    }

    #[cfg(test)]
    fn finish(&mut self, root: AgentCommandRoot, reported: Option<u64>) {
        self.finish_at(root, reported, now_ms());
    }
}

pub(crate) fn wire(usage: TreeUsage, source: TreeSource) -> TreeReading {
    TreeReading::new(
        usage.cpu_ms,
        usage.memory_bytes,
        match usage.memory_kind {
            subc_os::MemoryKind::PhysFootprint => subc_control::ChildMemoryKind::PhysFootprint,
            subc_os::MemoryKind::ResidentSet => subc_control::ChildMemoryKind::ResidentSet,
            subc_os::MemoryKind::WindowsWorkingSet => {
                subc_control::ChildMemoryKind::WindowsWorkingSet
            }
        },
        usage.process_count,
        source,
    )
    .with_groups(groups(usage.groups))
}

fn groups(groups: Vec<subc_os::ProcessGroup>) -> Vec<subc_control::ProcessGroup> {
    groups
        .into_iter()
        .map(|group| {
            subc_control::ProcessGroup::new(
                group.name,
                group.count,
                group.cpu_ms,
                group.memory_bytes,
            )
        })
        .collect()
}

#[cfg(test)]
#[cfg(all(test, unix))]
fn identity(pid: u32, start: u64) -> u128 {
    (u128::from(pid) << 64) | u128::from(start)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn read_deadline() -> Instant {
    let deadline = Instant::now() + Duration::from_secs(1);
    crate::control::list_diagnostics::deadline()
        .map_or(deadline, |list| deadline.min(list.into_std()))
}

#[cfg(test)]
fn cpu_ms(usage: subc_os::ResourceUsage) -> u64 {
    u64::try_from((usage.cpu_user + usage.cpu_system).as_millis()).unwrap_or(u64::MAX)
}

pub(crate) fn spawn(
    control: &Arc<crate::control::ControlHandler>,
    supervisor: SupervisorHandle,
    tracker: Arc<Tracker>,
) {
    let owner = Arc::downgrade(control);
    tokio::spawn(async move {
        let mut minute = tokio::time::interval(Duration::from_secs(60));
        minute.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            let sample = tracker.next_sample(&mut minute).await;
            tracker
                .read(
                    supervisor.list(),
                    sample,
                    if sample {
                        "resource_sampler"
                    } else {
                        "agent_commands"
                    },
                )
                .await;
            if owner.upgrade().is_none() {
                break;
            }
        }
    });
}

impl Tracker {
    async fn next_sample(&self, minute: &mut tokio::time::Interval) -> bool {
        tokio::select! {
            _ = minute.tick() => true,
            _ = self.wake.notified() => false,
            // macOS walks take 0.2–1.2 s, so declarations often miss the 200 ms
            // window and depend on fresh retries before pending starts expire.
            _ = tokio::time::sleep(Duration::from_secs(5)), if self.needs_retry() => false,
        }
    }

    fn needs_retry(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .modules
            .values()
            .any(|module| {
                !module.pending.is_empty()
                    || (cfg!(windows) && module.roots.values().any(|root| root.live))
            })
    }
}

#[cfg(test)]
mod tests;
