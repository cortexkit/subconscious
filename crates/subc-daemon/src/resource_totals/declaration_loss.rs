use super::*;

const MAX_LOST_ROOTS: usize = 1_024;
const LOST_WINDOW: Duration = Duration::from_secs(60 * 60);

/// Coverage metadata has a short update lock, separate from accounting and I/O.
#[derive(Debug, Default)]
pub(crate) struct Losses {
    state: Mutex<LossState>,
    clock: Clock,
}

#[derive(Debug, Default)]
struct LossState {
    roots: BTreeMap<AgentCommandRoot, Lost>,
    ever_lost: bool,
    overflow: u64,
    recovered: u64,
    overflow_at: RecentLoss,
    uncertainty_revision: u64,
}

#[derive(Debug)]
struct Lost {
    declared_at: Instant,
    gone_at: Option<Duration>,
}

impl Losses {
    #[cfg(test)]
    pub(crate) fn hold_for_test(&self, ready: std::sync::mpsc::Sender<()>, duration: Duration) {
        let _guard = self.state.lock().unwrap();
        ready.send(()).unwrap();
        std::thread::sleep(duration);
    }
    fn new(clock: Clock) -> Self {
        Self {
            clock,
            ..Self::default()
        }
    }
    pub(crate) fn mark(&self, roots: &[AgentCommandRoot]) {
        if roots.is_empty() {
            return;
        }
        let now = self.clock.now();
        // Contention is not a dropped identity: the lock protects only this
        // bounded metadata update and is never held across I/O or an await.
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.roots.retain(|_, loss| {
            loss.gone_at
                .is_none_or(|gone| now.saturating_sub(gone) < LOST_WINDOW)
        });
        state.ever_lost = true;
        for root in roots {
            if state.roots.contains_key(root) {
                continue;
            }
            state.uncertainty_revision = state.uncertainty_revision.saturating_add(1);
            if state.roots.len() < MAX_LOST_ROOTS {
                state.roots.insert(
                    *root,
                    Lost {
                        declared_at: Instant::now(),
                        gone_at: None,
                    },
                );
            } else {
                state.overflow = state.overflow.saturating_add(1);
                state.overflow_at.mark(now);
            }
        }
    }

    pub(crate) fn lower(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .ever_lost
    }

    pub(super) fn sample_uncertainty(&self) -> (u64, bool) {
        let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        (
            state.uncertainty_revision,
            state.roots.values().any(|loss| loss.gone_at.is_none())
                || state.overflow > state.recovered,
        )
    }

    pub(super) fn mark_overflow(&self) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.ever_lost = true;
        state.overflow = state.overflow.saturating_add(1);
        state.overflow_at.mark(self.clock.now());
        state.uncertainty_revision = state.uncertainty_revision.saturating_add(1);
    }

    pub(super) fn epoch(&self) -> u64 {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .overflow
    }
    fn overflowed(state: &LossState, now: Duration) -> bool {
        state.overflow != 0 && (state.overflow > state.recovered || state.overflow_at.active(now))
    }

    fn reconcile(&self, accepted: &[AgentCommandRoot], complete: Option<u64>) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let mut repaired = false;
        state.roots.retain(|root, _| {
            let matched = accepted.iter().any(|accepted| {
                accepted.pid == root.pid
                    && accepted.start_time_ms.abs_diff(root.start_time_ms)
                        < subc_os::process_start_time_precision_ms()
            });
            repaired |= matched;
            !matched
        });
        if repaired {
            state.uncertainty_revision = state.uncertainty_revision.saturating_add(1);
        }
        if let Some(through) = complete {
            // A resync restores current coverage, not the preceding hour.
            // It cannot acknowledge overflow that occurred after its receipt.
            if through > state.recovered {
                state.uncertainty_revision = state.uncertainty_revision.saturating_add(1);
                state.recovered = through;
            }
        }
    }

    #[cfg(test)]
    fn observe_gone(&self, now: Duration, gone: impl Fn(AgentCommandRoot) -> Option<bool>) {
        self.observe_after(now, None, gone);
    }

    fn observe_after(
        &self,
        now: Duration,
        began: Option<Instant>,
        gone: impl Fn(AgentCommandRoot) -> Option<bool>,
    ) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let mut changed = false;
        state.roots.retain(|root, loss| {
            // A pre-declaration cached walk cannot establish the root's exit.
            if began.is_some_and(|began| began <= loss.declared_at) {
                return true;
            }
            match gone(*root) {
                Some(true) => {
                    changed |= loss.gone_at.is_none();
                    now.saturating_sub(*loss.gone_at.get_or_insert(now)) < LOST_WINDOW
                }
                Some(false) => {
                    changed |= loss.gone_at.is_some();
                    loss.gone_at = None;
                    true
                }
                None => true,
            }
        });
        if changed {
            state.uncertainty_revision = state.uncertainty_revision.saturating_add(1);
        }
    }

    pub(super) fn observe(&self, table: &ProcessTable) {
        self.observe_after(self.clock.now(), table.started_at(), |root| {
            match table.process(root.pid) {
                Some(process) => Some(
                    process.start_time_ms.abs_diff(root.start_time_ms)
                        >= subc_os::process_start_time_precision_ms(),
                ),
                None => (table.identity_alive(root.pid, 0) == Some(false)).then_some(true),
            }
        });
    }

    fn diagnostics(&self) -> serde_json::Value {
        let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        serde_json::json!({
            "lost_identities": state.roots.len(),
            "overflow": Self::overflowed(&state, self.clock.now()),
        })
    }
}

impl Tracker {
    pub(crate) fn declaration_loss(&self, id: &str) -> Arc<Losses> {
        Arc::clone(
            self.declaration_losses
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .entry(id.to_owned())
                .or_insert_with(|| Arc::new(Losses::new(self.clock.clone()))),
        )
    }

    pub(crate) fn loss_diagnostics(&self) -> serde_json::Value {
        let losses = self
            .declaration_losses
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        serde_json::Value::Object(
            losses
                .iter()
                .filter(|(_, loss)| loss.lower())
                .map(|(id, loss)| (id.clone(), loss.diagnostics()))
                .collect(),
        )
    }
}

pub(super) fn recover(
    module: &Module,
    started: &[AgentCommandRoot],
    known: Option<&[AgentCommandRoot]>,
) -> Vec<AgentCommandRoot> {
    started
        .iter()
        .chain(known.into_iter().flatten())
        .filter(|root| {
            module
                .trusted_key(**root)
                .is_some_and(|key| module.roots.contains_key(&key))
        })
        .copied()
        .collect()
}

pub(super) fn commit(
    losses: &Losses,
    recovered: &[AgentCommandRoot],
    complete: Option<u64>,
    lost: &[AgentCommandRoot],
) {
    losses.reconcile(recovered, complete);
    losses.mark(lost);
}

#[cfg(test)]
mod tests {
    use super::*;
    fn root(pid: u32) -> AgentCommandRoot {
        AgentCommandRoot {
            pid,
            start_time_ms: 1,
        }
    }

    #[test]
    fn lock_contention_records_the_identity_without_fabricating_overflow() {
        let loss = Arc::new(Losses::default());
        let guard = loss.state.lock().unwrap();
        let worker = Arc::clone(&loss);
        let (done, result) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            worker.mark(&[root(2)]);
            done.send(()).unwrap();
        });
        assert!(
            result.recv_timeout(Duration::from_millis(20)).is_err(),
            "a contended update must wait, not invent overflow"
        );
        drop(guard);
        thread.join().unwrap();
        assert_eq!(loss.diagnostics()["lost_identities"], 1);
        assert_eq!(loss.diagnostics()["overflow"], false);
    }

    #[test]
    fn every_overflow_requires_a_resync_and_an_hour_since_its_latest_event() {
        let clock = Clock::manual(Duration::ZERO);
        let loss = Losses::new(clock.clone());
        let roots: Vec<_> = (1..=1_025).map(root).collect();
        loss.mark(&roots);
        let receipt = loss.epoch();
        clock.set(Duration::from_secs(3_600));
        assert!(
            loss.diagnostics()["overflow"].as_bool().unwrap(),
            "age alone cannot restore current coverage"
        );
        loss.mark(&[root(2_000)]);
        loss.reconcile(&roots, Some(receipt));
        clock.set(Duration::from_secs(7_200));
        assert!(
            loss.diagnostics()["overflow"].as_bool().unwrap(),
            "the old receipt cannot acknowledge newer overflow"
        );
        loss.reconcile(&[], Some(loss.epoch()));
        assert!(!loss.diagnostics()["overflow"].as_bool().unwrap());
        assert!(loss.lower(), "historical cumulative loss is never repaired");
    }

    #[test]
    fn empty_resync_qualifies_only_after_all_accepted_roots_are_verified_gone() {
        let clock = Clock::manual(Duration::ZERO);
        let tracker = Tracker {
            clock: clock.clone(),
            ..Tracker::default()
        };
        let losses = Arc::new(Losses::new(clock.clone()));
        losses.mark(&(1..=1_025).map(root).collect::<Vec<_>>());
        let live = ProcessTable::read().unwrap();
        let pid = std::process::id();
        let member = live.process(pid).unwrap();
        let accepted = AgentCommandRoot {
            pid,
            start_time_ms: member.start_time_ms,
        };
        let mut module = Module {
            declaration_loss: Arc::clone(&losses),
            ..Module::default()
        };
        module.roots.insert(
            accepted,
            Command {
                native_start: member.start_time,
                live: true,
                last_seen_ms: now_ms(),
                ..Command::default()
            },
        );
        let empty = |table| Prepared {
            scope_target: crate::subtree_resources::Target::Unavailable("no_container"),
            module_pid: pid,
            before_cpu: None,
            started: Vec::new(),
            pending: Vec::new(),
            finished: Vec::new(),
            known: Some(Vec::new()),
            table,
            at_ms: now_ms(),
            finish_until: u64::MAX,
            #[cfg(unix)]
            scope: None,
        };
        let epoch = losses.epoch();
        let first = tracker.apply_prepared(&mut module, 1, empty(live.clone()));
        assert!(!first.full_resync);
        assert_eq!(first.lost, vec![accepted]);
        commit(
            &losses,
            &[],
            first.full_resync.then_some(epoch),
            &first.lost,
        );
        let second = tracker.apply_prepared(&mut module, 1, empty(live));
        assert!(
            !second.full_resync,
            "retiring a live root cannot make the next empty resync complete"
        );
        clock.set(Duration::from_secs(3_600));
        assert!(losses.diagnostics()["overflow"].as_bool().unwrap());
        let gone = ProcessTable::from_test_members(u64::MAX, now_ms(), Vec::new(), Vec::new());
        let last = tracker.apply_prepared(&mut module, 1, empty(gone));
        assert!(last.full_resync);
        commit(
            &losses,
            &[],
            last.full_resync.then_some(losses.epoch()),
            &[],
        );
        assert_eq!(losses.diagnostics()["overflow"], false);
        assert!(losses.lower());
    }

    #[test]
    fn a_cached_walk_from_before_a_lost_start_cannot_start_its_gone_hour() {
        let loss = Losses::default();
        let before = ProcessTable::from_test_members(1, 1_000, Vec::new(), Vec::new());
        loss.mark(&[root(1)]);
        loss.observe(&before);
        assert!(loss.state.lock().unwrap().roots[&root(1)].gone_at.is_none());
        let after = ProcessTable::from_test_members(2, 2_000, Vec::new(), Vec::new());
        loss.observe(&after);
        assert!(loss.state.lock().unwrap().roots[&root(1)].gone_at.is_some());
        assert!(loss.lower());
    }

    #[test]
    fn refused_starts_remain_lower_until_the_matching_replay_or_the_gone_hour_expires() {
        let clock = Clock::manual(Duration::ZERO);
        let loss = Arc::new(Losses::new(clock.clone()));
        let module = Module {
            declaration_loss: Arc::clone(&loss),
            ..Module::default()
        };
        loss.mark(&[root(1), root(2)]);
        assert!(loss.lower());
        loss.reconcile(&[root(1)], None);
        assert_eq!(loss.diagnostics()["lost_identities"], 1);
        assert!(
            loss.lower(),
            "an unrelated replay cannot hide another lost command"
        );
        let now = clock.now();
        loss.observe_gone(now, |_| Some(true));
        loss.observe_gone(now + Duration::from_millis(3_599_999), |_| Some(true));
        assert!(
            loss.lower(),
            "exited command CPU still belongs to the covered hour"
        );
        assert_eq!(
            module.agent_bound(None),
            Some(subc_control::ResourceBound::Lower)
        );
        loss.observe_gone(now + Duration::from_secs(3_600), |_| Some(true));
        clock.set(Duration::from_secs(3_600));
        assert_eq!(loss.diagnostics()["lost_identities"], 0);
        assert!(loss.lower());
        assert_eq!(
            module.agent_bound(None),
            Some(subc_control::ResourceBound::Lower)
        );
        loss.mark(&[root(3)]);
        loss.reconcile(&[root(3)], None);
        assert!(
            loss.lower(),
            "accepted replay cannot repair historical cumulative loss"
        );
    }

    #[test]
    fn lost_identity_overflow_is_visible_and_requires_a_complete_reconciliation() {
        let clock = Clock::manual(Duration::ZERO);
        let loss = Losses::new(clock.clone());
        let roots: Vec<_> = (1..=1_025).map(root).collect();
        loss.mark(&roots);
        assert_eq!(loss.diagnostics()["lost_identities"], 1_024);
        assert_eq!(loss.diagnostics()["overflow"], true);
        loss.reconcile(&roots, None);
        assert_eq!(loss.diagnostics()["lost_identities"], 0);
        assert!(loss.lower());
        loss.reconcile(&[], Some(loss.epoch()));
        assert!(loss.diagnostics()["overflow"].as_bool().unwrap());
        clock.set(Duration::from_millis(3_599_999));
        assert!(loss.diagnostics()["overflow"].as_bool().unwrap());
        clock.set(Duration::from_secs(3_600));
        assert!(!loss.diagnostics()["overflow"].as_bool().unwrap());
        assert_eq!(loss.diagnostics()["overflow"], false);
        assert!(loss.lower());
    }

    #[test]
    fn a_resync_does_not_erase_overflow_that_arrived_after_its_receipt() {
        let loss = Losses::default();
        let receipt = loss.epoch();
        let roots: Vec<_> = (1..=1_025).map(root).collect();
        loss.mark(&roots);
        loss.reconcile(&roots, Some(receipt));
        assert!(loss.lower());
        assert_eq!(loss.diagnostics()["lost_identities"], 0);
        assert_eq!(loss.diagnostics()["overflow"], true);
    }
}
