use super::*;

type Launches = BTreeMap<String, Option<crate::supervise::ResourceLaunch>>;

/// A failed observation changes the reason, never the last successful numbers
/// or their measurement time. Launch identities fence reuse of those numbers.
#[derive(Debug, Default)]
pub(super) struct View {
    pub(super) checked_at: Option<Instant>,
    generations: BTreeMap<String, u128>,
    readings: Readings,
}

impl View {
    pub(super) fn publish(&mut self, incoming: &Readings, generations: BTreeMap<String, u128>) {
        self.checked_at = Some(Instant::now());
        self.readings
            .modules
            .retain(|id, _| generations.contains_key(id));
        self.readings
            .reasons
            .retain(|id, _| generations.contains_key(id));
        let mut agents: BTreeMap<_, _> = self
            .readings
            .agent_commands
            .take()
            .unwrap_or_default()
            .into_iter()
            .map(|row| (row.module_id.clone(), row))
            .collect();
        agents.retain(|id, _| generations.contains_key(id));
        for (id, (tree, hour)) in &incoming.modules {
            let Some(generation) = generations.get(id).copied() else {
                continue;
            };
            if self
                .generations
                .get(id)
                .is_some_and(|previous| *previous > generation)
            {
                // A late old-launch walk cannot replace a newer committed view.
                if incoming.reasons.get(id) == Some(&"launch_changed") {
                    self.readings.reasons.insert(id.clone(), "launch_changed");
                }
                continue;
            }
            if self.generations.get(id) != Some(&generation) {
                self.readings.modules.remove(id);
                agents.remove(id);
            }
            self.generations.insert(id.clone(), generation);
            let previous = self.readings.modules.entry(id.clone()).or_default();
            previous.1 = hour.clone();
            if incoming.reasons.contains_key(id) {
                self.readings.reasons.insert(
                    id.clone(),
                    if previous.0.is_some() {
                        incoming.reasons[id]
                    } else {
                        "not_yet_measured"
                    },
                );
            } else {
                previous.0 = tree.clone();
                self.readings.reasons.remove(id);
                agents.remove(id);
            }
        }
        for row in incoming.agent_commands.iter().flatten() {
            if generations.get(&row.module_id) == self.generations.get(&row.module_id) {
                agents.insert(row.module_id.clone(), row.clone());
            }
        }
        self.generations
            .retain(|id, _| generations.contains_key(id));
        self.readings.agent_commands = Some(agents.into_values().collect());
        if let Some(daemon) = &incoming.daemon {
            if matches!(
                daemon.resources,
                Some(subc_control::ChildResourceUsage::Measured(_))
            ) {
                self.readings.daemon = Some(daemon.clone());
            }
        }
    }
}

impl Tracker {
    #[cfg(test)]
    pub(crate) fn seed_committed_for_test(
        &self,
        modules: &[SupervisedModule],
        readings: Readings,
        now: u64,
    ) {
        fn seed(history: &mut History, hour: &CpuLastHour, now: u64) {
            history.record_observation(1, now - hour.covered_ms, 0, 0);
            history.record_observation(2, now, 0, hour.cpu_ms);
        }
        let generations = modules
            .iter()
            .map(|module| {
                (
                    module.module_id().to_owned(),
                    module.resource_launch().unwrap().generation(),
                )
            })
            .collect();
        let mut state = self.state.lock().unwrap();
        for (id, (_, hour)) in &readings.modules {
            if let Some(hour) = hour {
                let module = state.modules.entry(id.clone()).or_default();
                module.last_source = Some(TreeSource::ProcessTree);
                seed(&mut module.history, hour, now);
            }
        }
        for row in readings.agent_commands.iter().flatten() {
            if let Some(hour) = &row.cpu_last_hour {
                seed(
                    &mut state
                        .modules
                        .entry(row.module_id.clone())
                        .or_default()
                        .agents_history,
                    hour,
                    now,
                );
            }
        }
        if let Some(hour) = readings
            .daemon
            .as_ref()
            .and_then(|row| row.cpu_last_hour.as_ref())
        {
            seed(&mut state.daemon, hour, now);
        }
        self.committed
            .lock()
            .unwrap()
            .publish(&readings, generations);
    }

    pub(crate) fn list(self: &Arc<Self>, modules: Vec<SupervisedModule>) -> Readings {
        let launches = modules
            .iter()
            .map(|module| (module.module_id().to_owned(), module.resource_launch()))
            .collect();
        self.list_for_launches(modules, &launches)
    }

    pub(super) fn list_for_launches(
        self: &Arc<Self>,
        modules: Vec<SupervisedModule>,
        launches: &Launches,
    ) -> Readings {
        let readings = self.committed_view(launches, now_ms());
        let stale = {
            let view = self.committed.lock().unwrap_or_else(|p| p.into_inner());
            view.checked_at
                .is_none_or(|at| at.elapsed() > Duration::from_secs(2))
                || launches.iter().any(|(id, launch)| {
                    launch.as_ref().is_some_and(|launch| {
                        view.generations.get(id) != Some(&launch.generation())
                    })
                })
        };
        if stale
            && self
                .view_refreshing
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            #[cfg(test)]
            self.view_refreshes.fetch_add(1, Ordering::Relaxed);
            struct Refresh(Arc<AtomicBool>);
            impl Drop for Refresh {
                fn drop(&mut self) {
                    self.0.store(false, Ordering::Release);
                }
            }
            let refresh = Refresh(Arc::clone(&self.view_refreshing));
            let tracker = Arc::clone(self);
            // A new task has no list-diagnostics task-local deadline. Lists do
            // not wait for admission, the process walk, or attribution work.
            tokio::spawn(async move {
                let _refresh = refresh;
                tracker.read(modules, false, "list_refresh").await;
            });
        }
        readings
    }

    pub(super) fn committed_view(&self, launches: &Launches, now: u64) -> Readings {
        let mut readings = {
            let view = self.committed.lock().unwrap_or_else(|p| p.into_inner());
            let mut readings = view.readings.clone();
            readings.modules.retain(|id, _| launches.contains_key(id));
            readings.reasons.retain(|id, _| launches.contains_key(id));
            if let Some(agents) = &mut readings.agent_commands {
                agents.retain(|row| {
                    launches
                        .get(&row.module_id)
                        .and_then(Option::as_ref)
                        .is_some_and(|launch| {
                            view.generations.get(&row.module_id) == Some(&launch.generation())
                        })
                });
            }
            for (id, launch) in launches {
                let current = launch
                    .as_ref()
                    .is_some_and(|launch| view.generations.get(id) == Some(&launch.generation()));
                if !current {
                    readings.modules.insert(id.clone(), (None, None));
                    readings.reasons.insert(id.clone(), "not_yet_measured");
                } else if readings.modules.get(id).is_none_or(|row| row.0.is_none()) {
                    readings.reasons.insert(id.clone(), "not_yet_measured");
                }
                if launch
                    .as_ref()
                    .is_some_and(|launch| !launch.enabled || !launch.alive)
                {
                    readings.reasons.insert(id.clone(), "not_running");
                }
            }
            readings
        };
        // Reading history is independent of measuring a current tree. A busy
        // attribution commit may delay this update, not erase the cached hour.
        if let Ok(state) = self.state.try_lock() {
            for (id, (_, hour)) in &mut readings.modules {
                if let Some(module) = state.modules.get(id) {
                    *hour = module.last_hour(now);
                }
            }
            for row in readings.agent_commands.iter_mut().flatten() {
                if let Some(module) = state.modules.get(&row.module_id) {
                    row.cpu_last_hour = module.agents_last_hour(now);
                }
            }
            if let Some(daemon) = &mut readings.daemon {
                daemon.cpu_last_hour = state.daemon.last_hour_at(now);
            }
        }
        readings
    }
}
