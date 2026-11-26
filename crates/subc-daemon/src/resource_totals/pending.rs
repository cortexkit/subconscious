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
    native_start: Option<u64>,
}

impl PendingRoot {
    pub(super) fn matches(
        &self,
        key: AgentCommandRoot,
        requested: AgentCommandRoot,
        table: &ProcessTable,
    ) -> bool {
        if key.pid != requested.pid {
            return false;
        }
        let Some(native) = self.native_start else {
            return same_epoch(key, requested);
        };
        match table.process(key.pid) {
            Some(member) => {
                member.start_time == native
                    && (same_epoch(key, requested)
                        || same_epoch(
                            AgentCommandRoot {
                                pid: key.pid,
                                start_time_ms: member.start_time_ms,
                            },
                            requested,
                        ))
            }
            None => {
                table.identity_alive(key.pid, native) == Some(false) && same_epoch(key, requested)
            }
        }
    }
}

impl Module {
    pub(super) fn oldest_pending_declaration(&self) -> Option<Instant> {
        self.pending
            .values()
            .map(|pending| pending.declared_at)
            .min()
    }

    pub(super) fn pending_key(&self, root: AgentCommandRoot) -> Option<AgentCommandRoot> {
        self.pending
            .keys()
            .copied()
            .find(|known| same_epoch(*known, root))
    }

    pub(super) fn pending_key_in(
        &self,
        root: AgentCommandRoot,
        table: &ProcessTable,
    ) -> Option<AgentCommandRoot> {
        self.pending
            .iter()
            .find_map(|(key, pending)| pending.matches(*key, root, table).then_some(*key))
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
        let native = table.process(root.pid).and_then(|member| {
            same_epoch(
                root,
                AgentCommandRoot {
                    pid: root.pid,
                    start_time_ms: member.start_time_ms,
                },
            )
            .then_some(member.start_time)
        });
        self.pend_with_table(
            generation,
            self.pid.expect("declaration module pid"),
            root,
            DeclarationArrival {
                queued_at: self.clock.now(),
                declared_at: Instant::now(),
            },
            now,
            Some((table, native)),
        )
    }

    pub(super) fn pend_at(
        &mut self,
        generation: u128,
        module_pid: u32,
        root: AgentCommandRoot,
        arrival: DeclarationArrival,
        now: u64,
    ) -> Acceptance {
        self.pend_with_table(generation, module_pid, root, arrival, now, None)
    }

    fn pend_with_table(
        &mut self,
        generation: u128,
        module_pid: u32,
        root: AgentCommandRoot,
        arrival: DeclarationArrival,
        now: u64,
        snapshot: Option<(&ProcessTable, Option<u64>)>,
    ) -> Acceptance {
        let key = if let Some((table, _)) = snapshot {
            self.pending_key_in(root, table)
        } else {
            self.pending_key(root)
        };
        if let Some(key) = key {
            let pending = self.pending.get_mut(&key).unwrap();
            if pending.generation == generation && pending.module_pid == module_pid {
                pending.native_start = pending
                    .native_start
                    .or(snapshot.and_then(|(_, native)| native));
                return Acceptance::default();
            }
            self.pending.remove(&key);
        }
        if self.pending.contains_key(&root)
            || snapshot
                .map_or_else(
                    || self.trusted_key(root),
                    |(table, _)| self.finish_key(root, table),
                )
                .is_some()
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
                sequence: snapshot.map_or(self.table_revision, |(table, _)| {
                    self.table_revision.max(table.revision())
                }),
                native_start: snapshot.and_then(|(_, native)| native),
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
            let result = if table.process(root.pid).is_none()
                && table.identity_alive(root.pid, 0) == Some(false)
            {
                validation::Validation::Exited
            } else {
                let trusted = pending
                    .native_start
                    .into_iter()
                    .map(|start| (root, start))
                    .collect();
                validation::validate_root(pending.module_pid, target, root, table, &trusted)
            };
            let verified = match result {
                validation::Validation::Exited => {
                    tracing::debug!(
                        module_id,
                        pid = root.pid,
                        "pending agent command exited before validation"
                    );
                    continue;
                }
                validation::Validation::Incomplete => {
                    self.pending.insert(root, pending);
                    continue;
                }
                validation::Validation::Valid(verified) => Some(verified),
                validation::Validation::Invalid => None,
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

#[cfg(test)]
mod tests;
