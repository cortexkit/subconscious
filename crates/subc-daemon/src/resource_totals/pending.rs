use super::*;

// A permanently unreadable tree must not reserve live-root capacity forever.
const VALIDATION_LIMIT: Duration = Duration::from_secs(60);
pub(super) const ROOT_LIMIT: usize = 1_024;

#[derive(Clone, Debug)]
pub(super) struct PendingRoot {
    generation: u128,
    module_pid: u32,
    queued_at: Duration,
    declared_at: Instant,
    sequence: u64,
}

impl Module {
    pub(super) fn oldest_pending_declaration(&self) -> Option<Instant> {
        self.pending
            .values()
            .map(|pending| pending.declared_at)
            .min()
    }

    pub(super) fn pending_key(&self, root: AgentCommandRoot) -> Option<AgentCommandRoot> {
        self.pending.keys().copied().find(|known| {
            known.pid == root.pid
                && known.start_time_ms.abs_diff(root.start_time_ms)
                    < subc_os::process_start_time_precision_ms()
        })
    }

    /// Pending roots and the module processes that declared them.
    pub(super) fn pending_pids(&self, pids: &mut BTreeSet<u32>) {
        for (root, pending) in &self.pending {
            pids.insert(root.pid);
            pids.insert(pending.module_pid);
        }
    }

    pub(super) fn root_capacity_full(&self) -> bool {
        self.roots.len() + self.finished_live.len() + self.pending.len() >= ROOT_LIMIT
    }

    pub(super) fn pend(
        &mut self,
        generation: u128,
        root: AgentCommandRoot,
        table: &ProcessTable,
        now: u64,
    ) -> Acceptance {
        let already_pending = self.pending_key(root).is_some();
        let result = self.pend_at(
            generation,
            self.pid.expect("declaration module pid"),
            root,
            DeclarationArrival {
                queued_at: self.clock.now(),
                declared_at: Instant::now(),
            },
            now,
        );
        if !already_pending {
            if let Some(key) = self.pending_key(root) {
                let pending = self.pending.get_mut(&key).unwrap();
                pending.sequence = pending.sequence.max(table.revision());
            }
        }
        result
    }

    pub(super) fn pend_at(
        &mut self,
        generation: u128,
        module_pid: u32,
        root: AgentCommandRoot,
        arrival: DeclarationArrival,
        now: u64,
    ) -> Acceptance {
        if let Some(key) = self.pending_key(root) {
            let pending = &self.pending[&key];
            if pending.generation == generation && pending.module_pid == module_pid {
                return Acceptance::default();
            }
            self.pending.remove(&key);
        }
        if self.trusted_key(root).is_some()
            || self
                .finished
                .keys()
                .chain(self.finished_live.keys())
                .any(|known| {
                    known.pid == root.pid
                        && known.start_time_ms.abs_diff(root.start_time_ms)
                            < subc_os::process_start_time_precision_ms()
                })
        {
            return Acceptance::default();
        }
        if self.root_capacity_full() {
            let first = self.rejected.insert(root, now).is_none();
            trim_oldest(&mut self.rejected);
            return Acceptance {
                log_rejected: first,
                full: true,
            };
        }
        self.pending.insert(
            root,
            PendingRoot {
                generation,
                module_pid,
                queued_at: arrival.queued_at,
                declared_at: arrival.declared_at,
                sequence: self.table_revision,
            },
        );
        Acceptance::default()
    }

    pub(super) fn validate_pending(
        &mut self,
        module_id: &str,
        generation: u128,
        target: Option<&crate::subtree_resources::Target>,
        table: &ProcessTable,
        now: u64,
    ) {
        for (root, mut pending) in std::mem::take(&mut self.pending) {
            if generation != pending.generation {
                continue;
            }
            if self.clock.now().saturating_sub(pending.queued_at) >= VALIDATION_LIMIT {
                tracing::warn!(
                    module_id,
                    pid = root.pid,
                    "pending agent command validation expired"
                );
                continue;
            }
            let Some(target) = target else {
                self.pending.insert(root, pending);
                continue;
            };
            if table.revision() <= pending.sequence
                || table.revision() <= self.table_revision
                || table
                    .started_at()
                    .is_none_or(|began| began <= pending.declared_at)
                || self.observation_completed.is_some_and(|completed| {
                    table.started_at().is_none_or(|began| began <= completed)
                })
            {
                self.pending.insert(root, pending);
                continue;
            }
            pending.sequence = table.revision();
            if table.process(root.pid).is_none() && table.identity_alive(root.pid, 0) == Some(false)
            {
                tracing::debug!(
                    module_id,
                    pid = root.pid,
                    "pending agent command exited before validation"
                );
                continue;
            }
            match validation::validate_root(pending.module_pid, target, root, table, None) {
                validation::Validation::Incomplete => {
                    self.pending.insert(root, pending);
                }
                result => {
                    let verified = match result {
                        validation::Validation::Valid(verified) => Some(verified),
                        validation::Validation::Invalid => None,
                        validation::Validation::Incomplete => unreachable!(),
                    };
                    let acceptance = self.accept_checked(generation, root, verified, table, now);
                    if acceptance.log_rejected {
                        tracing::warn!(
                            module_id,
                            pid = root.pid,
                            start_time_ms = root.start_time_ms,
                            "agent command root was not admitted"
                        );
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
