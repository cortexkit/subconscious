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
    pub(super) fn pending_key(&self, root: AgentCommandRoot) -> Option<AgentCommandRoot> {
        self.pending.keys().copied().find(|known| {
            known.pid == root.pid
                && known.start_time_ms.abs_diff(root.start_time_ms)
                    < subc_os::process_start_time_precision_ms()
        })
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
        if self.pending_key(root).is_some()
            || self.trusted_key(root).is_some()
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
                module_pid: self.pid.expect("declaration module pid"),
                queued_at: self.clock.now(),
                declared_at: Instant::now(),
                sequence: table.revision(),
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
