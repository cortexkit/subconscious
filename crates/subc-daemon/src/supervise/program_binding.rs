use super::*;

#[derive(Debug, Default)]
pub(super) struct ProgramBindingRecords {
    active: HashMap<(String, String), ProgramBindingEntry>,
    retired: HashMap<String, ProgramBindingEntry>,
    configured: HashSet<String>,
}
type ProgramBindingTable = Mutex<ProgramBindingRecords>;
pub(super) type ProgramBindings = Arc<ProgramBindingTable>;

// A negative record must survive child death: no record means an ungated legacy
// launch, so erasing it would let the dead child's nonce bypass image checks.
// Keep one retired record per module until its next spawn or removal. During
// swap warm-up, the running child and its replacement keep separate active bindings.
// Configured-owner metadata survives unstamp so a later exit still records refusal.
#[derive(Clone, Debug)]
pub(super) struct ProgramBindingEntry {
    receiver: tokio::sync::watch::Receiver<Option<bool>>,
    live: Arc<Mutex<std::sync::Weak<content_binding::LiveProgramBinding>>>,
}

#[derive(Clone, Debug)]
pub(super) struct ProgramBindingPublication {
    signal: Arc<tokio::sync::watch::Sender<Option<bool>>>,
    live: Arc<Mutex<std::sync::Weak<content_binding::LiveProgramBinding>>>,
    bindings: std::sync::Weak<ProgramBindingTable>,
    key: (String, String),
}

impl ProgramBindingPublication {
    pub(super) fn retire(&self) {
        self.signal.send_replace(Some(false));
        if let Some(bindings) = self.bindings.upgrade() {
            let mut bindings = bindings
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if bindings.active.get(&self.key).map_or_else(
                || bindings.configured.contains(&self.key.0),
                |entry| Arc::ptr_eq(&entry.live, &self.live),
            ) {
                bindings.active.remove(&self.key);
                let (_, receiver) = tokio::sync::watch::channel(Some(false));
                bindings.retired.insert(
                    self.key.0.clone(),
                    ProgramBindingEntry {
                        receiver,
                        live: Arc::new(Mutex::new(std::sync::Weak::new())),
                    },
                );
            }
        }
    }
    pub(super) fn verify(&self) -> Result<(), crate::config_gate::GateError> {
        let result = self
            .live
            .lock()
            .ok()
            .and_then(|binding| binding.upgrade())
            .ok_or_else(|| {
                crate::config_gate::GateError::new(
                    "config_delivery_deferred",
                    "live program witness is unavailable",
                )
            })
            .and_then(|live| live.check());
        self.signal.send_replace(Some(result.is_ok()));
        result
    }
    pub(super) fn reject(&mut self) {
        self.signal.send_replace(Some(false));
    }
    pub(super) fn attach(&self, live: &Arc<content_binding::LiveProgramBinding>) {
        if let Ok(mut binding) = self.live.lock() {
            *binding = Arc::downgrade(live);
        }
    }
}

#[cfg(test)]
#[path = "program_binding_latch_tests.rs"]
mod tests;
impl Drop for ProgramBindingPublication {
    fn drop(&mut self) {
        if Arc::strong_count(&self.signal) == 1 {
            self.retire();
        }
    }
}

impl SupervisorHandle {
    pub(crate) fn install_binding_retirement(
        &self,
        owner: &crate::config_gate::runtime::RuntimeOwner,
    ) {
        let key = Arc::as_ptr(&self.program_bindings) as usize;
        let bindings = Arc::downgrade(&self.program_bindings);
        owner.install_binding_retirement(key, move || {
            if let Some(bindings) = bindings.upgrade() {
                let retired = std::mem::take(
                    &mut bindings
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .active,
                );
                #[cfg(windows)]
                for entry in retired.values() {
                    if let Some(live) = entry.live.lock().ok().and_then(|live| live.upgrade()) {
                        live.hello_accepted();
                    }
                }
                drop(retired);
            }
        });
    }
    #[cfg(all(test, any(windows, target_os = "linux")))]
    pub(crate) fn candidate_launch_nonce_for_test(&self, module: &str) -> Option<String> {
        self.swaps
            .lock()
            .ok()?
            .get(module)
            .map(|swap| swap.candidate_nonce.clone())
    }
    #[cfg(windows)]
    pub(crate) fn program_hello_accepted(&self, module: &str, nonce: &str) {
        let live = self
            .program_bindings
            .lock()
            .ok()
            .and_then(|bindings| {
                bindings
                    .active
                    .get(&(module.to_owned(), nonce.to_owned()))
                    .cloned()
            })
            .and_then(|entry| entry.live.lock().ok()?.upgrade());
        if let Some(live) = live {
            live.hello_accepted();
        }
    }
    pub(crate) fn has_program_binding_record(&self, module: &str) -> bool {
        self.program_bindings
            .lock()
            .map(|records| {
                records.retired.contains_key(module)
                    || records.active.keys().any(|(owner, _)| owner == module)
            })
            .unwrap_or(true)
    }
    pub(crate) fn has_live_verified_program_binding(&self, module: &str, nonce: &str) -> bool {
        self.program_bindings
            .lock()
            .ok()
            .and_then(|records| {
                records
                    .active
                    .get(&(module.to_owned(), nonce.to_owned()))
                    .map(|entry| *entry.receiver.borrow() == Some(true))
            })
            .unwrap_or(false)
    }
    pub(super) fn remove_program_binding_records(&self, module: &str) {
        let mut records = self
            .program_bindings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        records.active.retain(|(owner, _), _| owner != module);
        records.retired.remove(module);
        records.configured.remove(module);
    }
    pub(crate) fn check_current_program_binding(
        &self,
        module: &str,
        nonce: &str,
    ) -> Result<(), crate::config_gate::GateError> {
        let refused = || {
            crate::config_gate::GateError::new(
                "program_refused",
                "no current supervised image is bound to this HELLO",
            )
        };
        let entry = self
            .program_bindings
            .lock()
            .map_err(|_| refused())?
            .active
            .get(&(module.to_owned(), nonce.to_owned()))
            .cloned()
            .ok_or_else(refused)?;
        if *entry.receiver.borrow() != Some(true) {
            return Err(refused());
        }
        let live = entry
            .live
            .lock()
            .map_err(|_| refused())?
            .upgrade()
            .ok_or_else(refused)?;
        live.check()
    }
    pub(crate) fn check_program_review(
        &self,
        module: &str,
        nonce: &str,
    ) -> Result<(), crate::config_gate::GateError> {
        let refused = || {
            crate::config_gate::GateError::new(
                "program_refused",
                "review requires the current admitted supervised child",
            )
        };
        if !self.spawned_consumer_authorized(module, nonce) {
            return Err(refused());
        }
        if self.has_program_binding_record(module) {
            self.check_current_program_binding(module, nonce)?;
        }
        let child = self.get(module).ok_or_else(refused)?;
        let (status, _identity) = child
            .status_with_snapshot_lock(&child.inner.snapshot, None)
            .map_err(|_| refused())?;
        if !status.process_alive {
            return Err(refused());
        }
        #[cfg(unix)]
        {
            let observed = status
                .pid
                .and_then(observe_spawned_image)
                .ok_or_else(refused)?;
            if Some(observed.start_time) != status.process_start_time {
                return Err(refused());
            }
            let expected = if self.program_binding(module, nonce).is_some() {
                _identity
                    .map(|identity| subc_os::FileIdentity {
                        device: identity.device,
                        inode: identity.inode,
                    })
                    .ok_or_else(refused)?
            } else {
                let (spec, _) = child.configuration().map_err(|_| refused())?;
                crate::config_gate::program::capture(&spec, &[])?.image
            };
            if observed.executable != Some(expected) {
                return Err(refused());
            }
        }
        #[cfg(windows)]
        {
            let (_, image) = tokio::runtime::Handle::current()
                .block_on(child.status_and_running_image_agreement())
                .map_err(|_| refused())?;
            if !matches!(image, subc_control::RunningImageAgreement::Match { .. }) {
                return Err(refused());
            }
        }
        Ok(())
    }
    pub(super) fn begin_program_binding(
        &self,
        module: &str,
        role: SpawnRole,
    ) -> Option<ProgramBindingPublication> {
        let nonce = match role {
            SpawnRole::Plain => self.spawn_nonce(module),
            SpawnRole::SwapCandidate => self
                .swaps
                .lock()
                .ok()?
                .get(module)
                .map(|swap| swap.candidate_nonce.clone()),
        }?;
        let (sender, receiver) = tokio::sync::watch::channel(None);
        let mut bindings = self.program_bindings.lock().ok()?;
        bindings.configured.insert(module.to_owned());
        bindings.retired.remove(module);
        if role == SpawnRole::Plain {
            bindings.active.retain(|(owner, _), _| owner != module);
        }
        let live = Arc::new(Mutex::new(std::sync::Weak::new()));
        let key = (module.to_owned(), nonce);
        bindings.active.insert(
            key.clone(),
            ProgramBindingEntry {
                receiver,
                live: live.clone(),
            },
        );
        Some(ProgramBindingPublication {
            signal: Arc::new(sender),
            live,
            bindings: Arc::downgrade(&self.program_bindings),
            key,
        })
    }

    fn program_binding(
        &self,
        module: &str,
        nonce: &str,
    ) -> Option<tokio::sync::watch::Receiver<Option<bool>>> {
        let records = self.program_bindings.lock().ok()?;
        records
            .active
            .get(&(module.to_owned(), nonce.to_owned()))
            .or_else(|| records.retired.get(module))
            .map(|entry| entry.receiver.clone())
    }

    pub(crate) async fn wait_program_binding(&self, module: &str, nonce: &str) -> bool {
        let Some(mut binding) = self.program_binding(module, nonce) else {
            return true;
        };
        loop {
            if let Some(verified) = *binding.borrow_and_update() {
                return verified;
            }
            if binding.changed().await.is_err() {
                return false;
            }
        }
    }
}
