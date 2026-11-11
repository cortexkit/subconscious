//! Spawn-time authority is a verified in-memory view, never a store read under a module gate.

use super::*;
use crate::config_gate::{program, CapturedInput, GateSnapshot, InputKey};
use crate::supervise::ModuleSpec;

// Copy of the supervisor's configured-ID set, not a read of live config files.
// A stamped supervisor derives that set from approved configuration. Readers
// reject an absent or poisoned source without locking its mutable set.
pub(super) struct ConfiguredModuleSnapshot {
    ids: std::collections::HashSet<String>,
    source: std::sync::Weak<Mutex<std::collections::HashSet<String>>>,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct AdmissionCache {
    state: StoreState,
    file_authoritative: bool,
    unstamp_exit_pending: bool,
    modules: BTreeMap<String, ModuleAdmission>,
    fenced: BTreeSet<String>,
}

#[derive(Clone, Debug, PartialEq)]
struct ModuleAdmission {
    program_base: Digest,
    has_active: bool,
    generation: u64,
    held: bool,
    snapshot: GateSnapshot,
    command: Option<ModuleSpec>,
}

impl AdmissionCache {
    pub(super) fn is_recovering(&self) -> bool {
        matches!(
            self.state,
            StoreState::StoreMissing | StoreState::StoreUnverified
        )
    }
    pub(super) fn held_modules(&self) -> Vec<String> {
        self.modules
            .iter()
            .filter(|(module, authority)| {
                authority.held || (!authority.has_active && self.fenced.contains(*module))
            })
            .map(|(module, _)| module.clone())
            .collect()
    }
    pub(super) fn transitions_from(&self, previous: &Self) -> (Vec<String>, Vec<String>) {
        let recovery = |state| {
            matches!(
                state,
                StoreState::StoreMissing | StoreState::StoreUnverified
            )
        };
        let held = |cache: &Self, module: &str| {
            recovery(cache.state)
                || cache
                    .modules
                    .get(module)
                    .is_some_and(|authority| authority.held)
        };
        let modules: BTreeSet<_> = self
            .modules
            .keys()
            .chain(previous.modules.keys())
            .cloned()
            .collect();
        let mut holds = Vec::new();
        let mut releases = Vec::new();
        for module in modules {
            match (held(previous, &module), held(self, &module)) {
                (false, true) => holds.push(module),
                (true, false) => releases.push(module),
                _ => {}
            }
        }
        (holds, releases)
    }
    pub(super) fn from_store(store: &ConfigStore) -> Self {
        Self::from_store_at(store, store.loaded_admission_state().1.last_generation)
    }

    fn from_store_at(store: &ConfigStore, started_at: u64) -> Self {
        let (state, body) = store.loaded_admission_state();
        let mut cache = Self {
            state,
            file_authoritative: state == StoreState::Unstamped,
            unstamp_exit_pending: false,
            modules: BTreeMap::new(),
            fenced: BTreeSet::new(),
        };
        if cache.state != StoreState::Stamped {
            return cache;
        }
        let Ok(approved) = GateSnapshot::from_store(&body.approved) else {
            cache.state = StoreState::StoreUnverified;
            return cache;
        };
        for module in approved.modules() {
            let versions = body
                .versions
                .iter()
                .find(|(tuple, _)| tuple.module == module && tuple.project.is_none())
                .map(|(_, versions)| versions);
            let pending = versions.and_then(|versions| versions.pending);
            let deferred = pending.is_some_and(|id| {
                id > started_at
                    && body.generations.get(&id).is_some_and(|entry| {
                        super::super::apply::Transition::for_module(
                            &entry.generation.changeset,
                            &module,
                        )
                        .is_ok_and(|transition| {
                            transition == super::super::apply::Transition::DaemonRestart
                        })
                    })
            });
            let generation = versions.and_then(|versions| {
                if deferred {
                    versions.active
                } else {
                    versions.pending.or(versions.active)
                }
            });
            let entry = body
                .generations
                .get(&generation.unwrap_or_else(|| placement::last_approved_generation(body)));
            let mut snapshot = entry
                .and_then(|entry| GateSnapshot::from_store(&entry.configuration).ok())
                .unwrap_or_else(|| approved.clone());
            if cfg!(feature = "dev-placement") {
                placement::overlay_programs(body, &module, &mut snapshot);
            }
            let command = entry
                .and_then(|entry| entry.delivery.as_ref())
                .and_then(|delivery| delivery.daemon.as_ref())
                .and_then(|layer| layer.bytes.as_ref().zip(layer.path.as_ref()))
                .and_then(|(bytes, path)| {
                    std::str::from_utf8(bytes)
                        .ok()
                        .and_then(|doc| crate::daemon_config::parse_with_activity(doc, path).ok())
                })
                .and_then(|(config, _)| {
                    config
                        .modules
                        .into_iter()
                        .find(|configured| configured.module_id == module)
                })
                .map(|configured| configured.module_spec());
            let program_base = program::approved_base(&snapshot, &module);
            let refused = body
                .program_refusals
                .iter()
                .any(|refusal| refusal.module == module && refusal.approved_base == program_base);
            let config_gated = snapshot
                .inputs
                .contains_key(&InputKey::Declaration(module.clone()));
            cache.modules.insert(
                module,
                ModuleAdmission {
                    program_base,
                    has_active: !config_gated
                        || versions.is_some_and(|versions| versions.active.is_some()),
                    generation: generation.unwrap_or(0),
                    held: refused
                        || (deferred && generation.is_none())
                        || (config_gated
                            && versions.is_some_and(|versions| {
                                versions.active.is_none() && versions.pending.is_none()
                            })),
                    snapshot,
                    command,
                },
            );
        }
        cache
    }

    pub(super) fn from_state(state: &OwnerState) -> Self {
        let mut cache = Self::from_store_at(&state.store, state.delivery.started_at_generation());
        cache.unstamp_exit_pending = cache.file_authoritative && state.authority_exit.is_some();
        if state.recovery_exit_hold {
            cache.state = StoreState::StoreUnverified;
        }
        for (module, program_base) in &state.refused_generations {
            if let Some(authority) = cache
                .modules
                .get_mut(module)
                .filter(|authority| authority.program_base == *program_base)
            {
                authority.held = true;
            }
        }
        cache.fenced.extend(state.delivery.fenced_modules());
        cache
    }

    fn decision(&self, module: &str) -> Result<Option<&ModuleAdmission>> {
        if matches!(
            self.state,
            StoreState::StoreMissing | StoreState::StoreUnverified
        ) {
            return Err(GateError::new(
                "config_gated",
                "configuration store is in recovery",
            ));
        }
        if self.state == StoreState::Unstamped {
            return Ok(None);
        }
        let authority = self
            .modules
            .get(module)
            .ok_or_else(|| GateError::new("config_gated", "module is not approved"))?;
        if authority.held {
            return Err(GateError::new(
                "config_gated",
                "module has no active or pending approved generation",
            ));
        }
        if self.fenced.contains(module) {
            return Err(GateError::new(
                "config_delivery_deferred",
                "exited delivery target is pending reconciliation",
            ));
        }
        Ok(Some(authority))
    }
}

pub(crate) struct SpawnAdmission {
    authority: Option<ModuleAdmission>,
    pub(crate) launch: Option<program::ProgramLaunch>,
}

impl SpawnAdmission {
    pub(crate) fn approved_base(&self) -> Option<&Digest> {
        self.authority
            .as_ref()
            .map(|authority| &authority.program_base)
    }
}

impl RuntimeOwner {
    /// Only a committed, still-pending unstamp may stage enabled state while held.
    pub(crate) fn can_stage_unstamp_enable(&self) -> bool {
        self.admission
            .read()
            .map(|cache| cache.unstamp_exit_pending)
            .unwrap_or(false)
    }
    /// A committed unstamp permits its definition reconciliation while the
    /// separate recovery admission hold still prevents every child spawn.
    pub(crate) fn file_configuration_is_gated(&self, module: &str) -> bool {
        self.admission
            .read()
            .map(|cache| {
                !cache.file_authoritative
                    && match cache.state {
                        StoreState::Stamped => cache.modules.contains_key(module),
                        _ => true,
                    }
            })
            .unwrap_or(true)
    }
    pub(crate) fn observe_supervisor_configured_ids(
        &self,
        ids: Option<&std::collections::HashSet<String>>,
        source: std::sync::Weak<Mutex<std::collections::HashSet<String>>>,
    ) {
        *self
            .configured_modules
            .write()
            .unwrap_or_else(|p| p.into_inner()) = ids.map(|ids| ConfiguredModuleSnapshot {
            ids: ids.clone(),
            source,
        });
    }

    pub(crate) fn module_is_gated(&self, module: &str) -> bool {
        {
            let Ok(cache) = self.admission.read() else {
                return true;
            };
            match cache.state {
                StoreState::Unstamped => return false,
                StoreState::Stamped if !cache.modules.contains_key(module) => {}
                _ => return true,
            }
        }
        self.configured_modules
            .read()
            .map(|snapshot| {
                snapshot.as_ref().is_none_or(|snapshot| {
                    snapshot
                        .source
                        .upgrade()
                        .is_none_or(|source| source.is_poisoned())
                        || snapshot.ids.contains(module)
                })
            })
            .unwrap_or(true)
    }

    pub(crate) fn with_legacy_nonce_authority<T>(
        &self,
        module: &str,
        commit: impl FnOnce() -> T,
    ) -> std::result::Result<Option<T>, ()> {
        // Stamping takes this same owner mutex and publishes admission before
        // releasing it. Keep it through the syscall-free accounting mutation,
        // without an OwnerGuard whose drop would wake the delivery worker.
        let _state = self.inner.try_lock().map_err(|_| ())?;
        Ok((!self.module_is_gated(module)).then(commit))
    }
    pub(crate) fn checked_admission_hold(&self, module: Option<&str>) -> Result<()> {
        {
            let mut state = self.lock()?;
            Self::check_authority(&mut state, false)?;
        }
        match module {
            Some(module) => self.check_module_hold(module),
            None => {
                if self
                    .admission
                    .read()
                    .is_ok_and(|cache| cache.state == StoreState::Stamped)
                {
                    let _ = self.status();
                }
                self.check_rescan_hold()
            }
        }
    }
    pub(crate) fn take_admission_transitions(&self) -> Result<(Vec<String>, Vec<String>)> {
        let mut state = self.lock()?;
        Ok((
            std::mem::take(&mut state.admission_holds),
            std::mem::take(&mut state.admission_releases),
        ))
    }
    #[cfg(all(test, unix))]
    pub(crate) fn set_spawn_exec_hook_for_test(&self, hook: impl FnOnce() + Send + 'static) {
        *self.spawn_exec_hook.lock().unwrap() = Some(Box::new(hook));
    }
    #[cfg(test)]
    pub(crate) fn before_spawn_exec_for_test(&self) {
        let hook = self.spawn_exec_hook.lock().unwrap().take();
        if let Some(hook) = hook {
            hook();
        }
    }
    pub(crate) fn exit_is_held(&self, module: &str) -> bool {
        self.admission
            .read()
            .map(|cache| {
                matches!(
                    cache.state,
                    StoreState::StoreMissing | StoreState::StoreUnverified
                ) || cache.modules.get(module).is_some_and(|authority| {
                    authority.held || (!authority.has_active && cache.fenced.contains(module))
                })
            })
            .unwrap_or(true)
    }
    fn refuse_program(
        &self,
        module: &str,
        base: &Digest,
        inputs: &BTreeMap<InputKey, CapturedInput>,
    ) -> Result<bool> {
        let identity = subc_config::raw_digest(
            &serde_json::to_vec(&inputs.iter().collect::<Vec<_>>()).map_err(GateError::invalid)?,
        );
        self.record_program_refusal(module, base, identity)
    }

    pub(crate) fn record_program_refusal(
        &self,
        module: &str,
        checked_base: &Digest,
        identity: Digest,
    ) -> Result<bool> {
        let mut state = self.lock()?;
        let base = AdmissionCache::from_state(&state)
            .modules
            .get(module)
            .map(|authority| authority.program_base.clone());
        if let Some(base) = base.filter(|current| current == checked_base) {
            state
                .refused_generations
                .insert(module.to_owned(), base.clone());
            let held_modules = AdmissionCache::from_state(&state)
                .held_modules()
                .into_iter()
                .collect();
            state
                .store
                .record_program_refusal(module, base, identity, &held_modules)?;
            return Ok(true);
        }
        Ok(false)
    }
    /// Run before taking a module gate; hashing and integrity checks may block.
    pub(crate) fn prepare_spawn(&self, spec: &ModuleSpec) -> Result<SpawnAdmission> {
        let authority = {
            let mut state = OwnerGuard(
                self.inner.try_lock().map_err(|_| {
                    GateError::new(
                        "config_delivery_deferred",
                        "configuration publication is busy",
                    )
                })?,
                &self.admission,
                &self.delivery_changed,
                &self.module_gates,
                &self.approved_daemon,
            );
            Self::check_authority(&mut state, false)?;
            AdmissionCache::from_state(&state)
                .decision(&spec.module_id)?
                .cloned()
        };
        let launch = if let Some(authority) = &authority {
            let entry = authority
                .snapshot
                .inputs
                .get(&InputKey::Entry(spec.module_id.clone()))
                .and_then(|input| match &input.value {
                    subc_config::RecordedValue::Value { value } => Some(value),
                    _ => None,
                })
                .ok_or_else(|| {
                    GateError::new("config_gated", "approved module entry is missing")
                })?;
            let covered = program::program_files(entry)?;
            let command = authority.command.as_ref().ok_or_else(|| {
                GateError::new("config_gated", "approved launch command is unavailable")
            })?;
            let approved: BTreeMap<_, _> = authority.snapshot.inputs.iter()
                .filter(|(key, _)| matches!(key, InputKey::Program { module, .. } if module == &spec.module_id))
                .map(|(key, input)| (key.clone(), input.clone())).collect();
            let launch = match program::capture(command, &covered) {
                Ok(launch) => launch,
                Err(error) => {
                    if program::definite_program_change(&approved) {
                        if !self.record_program_refusal(
                            &spec.module_id,
                            &authority.program_base,
                            subc_config::raw_digest(error.detail.as_bytes()),
                        )? {
                            return Err(GateError::new(
                                "config_delivery_deferred",
                                "program approval changed during capture",
                            ));
                        }
                        return Err(GateError::new("program_refused", error.detail));
                    }
                    return Err(GateError::new("config_delivery_deferred", error.detail));
                }
            };
            if launch.inputs != approved {
                if !self.refuse_program(&spec.module_id, &authority.program_base, &launch.inputs)? {
                    return Err(GateError::new(
                        "config_delivery_deferred",
                        "program approval changed during capture",
                    ));
                }
                return Err(GateError::new(
                    "program_refused",
                    format!(
                        "{}: covered program identity is not approved",
                        spec.module_id
                    ),
                ));
            }
            Some(launch)
        } else {
            None
        };
        Ok(SpawnAdmission { authority, launch })
    }

    /// Called under exactly one module gate. Only the verified memory copy is read.
    pub(crate) fn check_spawn(&self, module: &str, admission: &SpawnAdmission) -> Result<()> {
        let cache = self
            .admission
            .read()
            .map_err(|_| GateError::new("config_gated", "admission authority is unavailable"))?;
        let current = cache.decision(module)?;
        if current.map(|authority| (&authority.program_base, authority.generation))
            != admission
                .authority
                .as_ref()
                .map(|authority| (&authority.program_base, authority.generation))
        {
            return Err(GateError::new(
                "config_delivery_deferred",
                "configuration changed during spawn preparation",
            ));
        }
        Ok(())
    }

    pub(crate) fn check_module_hold(&self, module: &str) -> Result<()> {
        self.admission
            .read()
            .map_err(|_| GateError::new("config_gated", "admission authority is unavailable"))?
            .decision(module)
            .map(|_| ())
    }

    pub(crate) fn check_rescan_hold(&self) -> Result<()> {
        let cache = self
            .admission
            .read()
            .map_err(|_| GateError::new("config_gated", "admission authority is unavailable"))?;
        if matches!(
            cache.state,
            StoreState::StoreMissing | StoreState::StoreUnverified
        ) {
            return Err(GateError::new(
                "config_gated",
                "configuration store is in recovery",
            ));
        }
        if cache.state == StoreState::Stamped {
            return Err(GateError::new(
                "config_gated",
                "stamped configuration changes require plan and apply, not rescan",
            ));
        }
        Ok(())
    }
}
