use super::*;

/// Narrow process-liveness signal published by supervisors and consumed by passive liveness polls.
pub trait ModuleProcessLiveness: Send + Sync {
    fn process_live(&self, module_id: &str) -> Option<bool>;

    /// Whether the supervisor is replacing this module's process right now: an
    /// operator restart or reload, a health restart, or a crash respawn whose
    /// backoff is running. A module in that state is not live, but a consumer
    /// refused now should retry shortly rather than treat the target as gone.
    /// Stopped, failed, and disabled modules are not replacing.
    fn process_replacing(&self, _module_id: &str) -> bool {
        false
    }
}

/// Shared process-liveness registry keyed by supervised `module_id`.
#[derive(Debug, Clone, Default)]
pub struct SupervisorProcessLiveness {
    snapshots: Arc<Mutex<HashMap<String, SharedSnapshot>>>,
}

impl SupervisorProcessLiveness {
    pub fn new() -> Self {
        Self::default()
    }

    pub(super) fn track(&self, module_id: String, snapshot: SharedSnapshot) {
        let mut snapshots = self
            .snapshots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        snapshots.insert(module_id, snapshot);
    }

    pub(super) fn untrack_if_current(&self, module_id: &str, snapshot: &SharedSnapshot) {
        let mut snapshots = self
            .snapshots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let is_current = snapshots
            .get(module_id)
            .map(|tracked| Arc::ptr_eq(tracked, snapshot))
            .unwrap_or(false);
        if is_current {
            snapshots.remove(module_id);
        }
    }
}

impl ModuleProcessLiveness for SupervisorProcessLiveness {
    fn process_live(&self, module_id: &str) -> Option<bool> {
        let snapshot = {
            let snapshots = self
                .snapshots
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            snapshots.get(module_id).cloned()
        }?;
        let snapshot = snapshot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Some(snapshot.state == ModuleState::Running && snapshot.process_alive)
    }

    fn process_replacing(&self, module_id: &str) -> bool {
        let Some(snapshot) = self
            .snapshots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(module_id)
            .cloned()
        else {
            return false;
        };
        let snapshot = snapshot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        snapshot.enabled
            && match snapshot.state {
                ModuleState::Restarting => true,
                ModuleState::Draining => snapshot.draining_to_replace,
                ModuleState::Starting
                | ModuleState::Running
                | ModuleState::Unresponsive
                | ModuleState::Stopped
                | ModuleState::Failed
                | ModuleState::Disabled => false,
            }
    }
}

impl SupervisorHandle {
    pub fn insert(&self, module: SupervisedModule) -> Option<SupervisedModule> {
        // Normally already marked before the process was spawned; marking here
        // too keeps `configured_ids` a superset of the roster for any caller
        // that inserts a module directly.
        self.mark_configured(module.module_id());
        let mut modules = self
            .modules
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        modules.insert(module.module_id().to_string(), module)
    }

    /// Record that the supervisor has taken on `module_id`. Called before the
    /// module's first process is spawned, so that by the time that process can
    /// register, [`Self::is_configured`] already answers true.
    pub(super) fn mark_configured(&self, module_id: &str) {
        self.update_configured_ids(|ids| {
            ids.insert(module_id.to_string());
        });
    }

    pub(super) fn publish_configured_ids(&self, ids: Option<&HashSet<String>>) {
        let owner = self
            .gate_runtime
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
            .and_then(|owner| owner.upgrade());
        if let Some(owner) = owner {
            owner.observe_supervisor_configured_ids(ids, Arc::downgrade(&self.configured_ids));
        }
    }

    fn update_configured_ids(&self, update: impl FnOnce(&mut HashSet<String>)) {
        match self.configured_ids.lock() {
            Ok(mut ids) => {
                update(&mut ids);
                // Publish while this roster mutation is serialized, so an
                // older snapshot can never overwrite a later configured id.
                self.publish_configured_ids(Some(&ids));
            }
            Err(poisoned) => {
                update(&mut poisoned.into_inner());
                self.publish_configured_ids(None);
            }
        }
    }

    /// Undo [`Self::mark_configured`] for a module whose first spawn failed
    /// before it was ever put on the roster. A module already on the roster
    /// keeps its mark: only [`Self::retire`] takes a rostered module off.
    pub(super) fn unmark_configured_unless_rostered(&self, module_id: &str) {
        let modules = self
            .modules
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !modules.contains_key(module_id) {
            self.update_configured_ids(|ids| {
                ids.remove(module_id);
            });
        }
    }

    /// Whether `module_id` is a module this daemon supervises: on the roster,
    /// or about to be (its process is being spawned right now).
    ///
    /// This, not `get(..).is_some()`, is what "the owner is configured" means
    /// for scopes: a supervised module's process can register and sync before
    /// [`Self::get`] can return it, and in that window it is still a module
    /// that will sync, not one that never will.
    pub(crate) fn is_configured(&self, module_id: &str) -> bool {
        self.configured_ids
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains(module_id)
    }

    pub fn get(&self, module_id: &str) -> Option<SupervisedModule> {
        let modules = self
            .modules
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        modules.get(module_id).cloned()
    }
}

impl SupervisorHandle {
    pub fn list(&self) -> Vec<SupervisedModule> {
        let modules = self
            .modules
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut modules = modules.values().cloned().collect::<Vec<_>>();
        modules.sort_by(|left, right| left.module_id().cmp(right.module_id()));
        modules
    }

    pub(crate) fn retire(&self, module_id: &str) -> Option<SupervisedModule> {
        self.activity_images
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(module_id);
        self.spawn_nonces
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(module_id);
        self.close_swap(module_id);
        let mut reserved_nonces = self
            .reserved_nonces
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if reserved_nonces.contains_key(module_id) {
            // The old nonce must die with the removed process, but the exact-id
            // gate remains until an operator explicitly releases it.
            reserved_nonces.insert(module_id.to_string(), None);
        }
        drop(reserved_nonces);
        self.reserved_prefix_owners
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|_, owner| owner != module_id);
        let removed = self
            .modules
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(module_id);
        self.update_configured_ids(|ids| {
            ids.remove(module_id);
        });
        self.remove_program_binding_records(module_id);
        removed
    }

    /// Remember a module removed by a non-preview rescan so route.open can
    /// distinguish that intentional removal from an unknown id.
    pub(crate) fn record_rescan_removal(&self, module_id: &str) {
        self.removal_tombstones
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(module_id.to_string(), unix_ms_now());
    }

    /// Return how long ago a rescan removed this module in milliseconds.
    pub(crate) fn removal_tombstone_age_ms(&self, module_id: &str) -> Option<u64> {
        self.removal_tombstones
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(module_id)
            .copied()
            .map(|removed_at_ms| unix_ms_now().saturating_sub(removed_at_ms))
    }

    /// Retire a reserved-id gate only after its module has left supervision.
    ///
    /// A retained gate has no live nonce (`None`), so releasing any other entry
    /// would weaken a currently configured or otherwise active reservation.
    pub(crate) fn release_retained_reserved_gate(&self, module_id: &str) -> bool {
        if self.get(module_id).is_some() {
            return false;
        }
        let mut reserved_nonces = self
            .reserved_nonces
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !matches!(reserved_nonces.get(module_id), Some(None)) {
            return false;
        }
        reserved_nonces.remove(module_id);
        self.implicit_protocol_none
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(module_id);
        true
    }
}
