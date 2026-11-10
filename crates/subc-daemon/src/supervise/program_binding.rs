use super::*;

#[derive(Debug, Default)]
pub(super) struct ProgramBindingRecords {
    active: HashMap<(String, String), ProgramBindingEntry>,
    retired: HashMap<(String, String), ProgramBindingEntry>,
    acceptable: HashSet<(String, String)>,
    configured: HashSet<String>,
    #[cfg(all(test, unix))]
    exit_pause: Option<ProgramBindingExitPause>,
}
type ProgramBindingTable = Mutex<ProgramBindingRecords>;
pub(super) type ProgramBindings = Arc<ProgramBindingTable>;

#[cfg(all(test, unix))]
#[derive(Debug)]
struct ProgramBindingExitPause {
    key: (String, String),
    reached: tokio::sync::oneshot::Sender<()>,
    release: tokio::sync::oneshot::Receiver<()>,
}

// A negative record must survive child death: no record means an ungated legacy
// launch, so erasing it would let the dead child's nonce bypass image checks.
// Retain refusals for every nonce still accepted by identity, consumer or swap
// admission. Keep a refusal for as long as any acceptance surface recognizes
// its nonce; rotation and swap closure prune only nonces outside that union.
// Configured-owner metadata survives unstamp so a later exit still records refusal.
// Retirement refuses only the dead nonce, not a later unstamped spawn's fresh nonce.
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
    retired: Arc<std::sync::atomic::AtomicBool>,
}

// A carried pin can reap independently of the actor. Its hook shares retirement
// state without keeping the publication alive or postponing last-owner Drop.
#[cfg(unix)]
#[derive(Clone, Debug)]
pub(super) struct ProgramBindingRetirement {
    signal: std::sync::Weak<tokio::sync::watch::Sender<Option<bool>>>,
    live: std::sync::Weak<Mutex<std::sync::Weak<content_binding::LiveProgramBinding>>>,
    bindings: std::sync::Weak<ProgramBindingTable>,
    key: (String, String),
    retired: Arc<std::sync::atomic::AtomicBool>,
    #[cfg(test)]
    upgrade_pause: Option<Arc<(std::sync::Barrier, std::sync::Barrier)>>,
}

#[cfg(unix)]
impl ProgramBindingRetirement {
    pub(super) fn upgrade(&self) -> Option<ProgramBindingPublication> {
        // Drop uses the strong signal count to identify the last publication.
        // Acquire live first so a failed live upgrade cannot make that final
        // Drop skip retirement because a temporary extra signal owner exists.
        let live = self.live.upgrade()?;
        #[cfg(test)]
        if let Some(pause) = &self.upgrade_pause {
            pause.0.wait();
            pause.1.wait();
        }
        let signal = self.signal.upgrade()?;
        Some(ProgramBindingPublication {
            signal,
            live,
            bindings: self.bindings.clone(),
            key: self.key.clone(),
            retired: self.retired.clone(),
        })
    }
}

impl ProgramBindingPublication {
    #[cfg(all(test, unix))]
    pub(super) fn key_for_test(&self) -> &str {
        &self.key.1
    }
    pub(super) fn check(&self) -> Result<(), crate::config_gate::GateError> {
        if *self.signal.borrow() != Some(true)
            || self.retired.load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(crate::config_gate::GateError::new(
                "program_refused",
                "launch has not been verified",
            ));
        }
        self.live
            .lock()
            .ok()
            .and_then(|live| live.upgrade())
            .ok_or_else(|| {
                crate::config_gate::GateError::new(
                    "program_refused",
                    "live launch witness is unavailable",
                )
            })?
            .check()
    }

    #[cfg(windows)]
    pub(super) fn hello_accepted(&self) {
        if let Some(live) = self.live.lock().ok().and_then(|live| live.upgrade()) {
            live.hello_accepted();
        }
    }
    #[cfg(unix)]
    pub(super) fn retirement_hook(&self) -> ProgramBindingRetirement {
        ProgramBindingRetirement {
            signal: Arc::downgrade(&self.signal),
            live: Arc::downgrade(&self.live),
            bindings: self.bindings.clone(),
            key: self.key.clone(),
            retired: self.retired.clone(),
            #[cfg(test)]
            upgrade_pause: None,
        }
    }
    #[cfg(all(test, unix))]
    pub(super) async fn pause_after_wait_for_test(&self) {
        let pause = self.bindings.upgrade().and_then(|bindings| {
            let mut records = bindings.lock().unwrap();
            if records
                .exit_pause
                .as_ref()
                .is_some_and(|pause| pause.key == self.key)
            {
                records.exit_pause.take()
            } else {
                None
            }
        });
        if let Some(pause) = pause {
            let _ = pause.reached.send(());
            let _ = pause.release.await;
        }
    }
    pub(super) fn retire(&self) {
        if self.retired.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        self.signal.send_replace(Some(false));
        if let Some(bindings) = self.bindings.upgrade() {
            let mut bindings = bindings
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if bindings.active.get(&self.key).map_or_else(
                || {
                    bindings.configured.contains(&self.key.0)
                        && bindings.acceptable.contains(&self.key)
                },
                |entry| Arc::ptr_eq(&entry.live, &self.live),
            ) {
                bindings.active.remove(&self.key);
                let (_, receiver) = tokio::sync::watch::channel(Some(false));
                bindings.retired.insert(
                    self.key.clone(),
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
            if self.key.1.starts_with("channel:") {
                if let Some(bindings) = self.bindings.upgrade() {
                    let mut records = bindings.lock().unwrap_or_else(|p| p.into_inner());
                    records.active.remove(&self.key);
                    records.retired.remove(&self.key);
                    records.acceptable.remove(&self.key);
                }
            }
        }
    }
}

impl SupervisorHandle {
    #[cfg(all(test, target_os = "linux"))]
    pub(super) fn release_program_binding_witnesses_for_test(&self) {
        self.program_bindings.lock().unwrap().active.clear();
    }
    #[cfg(all(test, target_os = "linux"))]
    pub(crate) async fn close_actor_commands_for_test(&self, module: &mut SupervisedModule) {
        // Preserve nonce authority while reproducing receiver closure, rather
        // than retiring the configured identity or aborting its monitor task.
        self.modules.lock().unwrap().remove(module.module_id());
        let inner =
            Arc::get_mut(&mut module.inner).expect("the test owns the remaining module handle");
        let (unrelated, _receiver) = mpsc::channel(1);
        drop(std::mem::replace(&mut inner.commands, unrelated));
        loop {
            if module
                .inner
                .monitor
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .is_finished()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
    pub(super) fn refresh_program_binding_nonces(&self, module: &str) {
        // Hold acceptance maps through publication so concurrent rotations cannot
        // publish an older accepted set after a newer one has pruned it.
        let current = self.spawn_nonces.lock().unwrap_or_else(|p| p.into_inner());
        let reserved = self
            .reserved_nonces
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let swaps = self.swaps.lock().unwrap_or_else(|p| p.into_inner());
        let mut accepted = HashSet::new();
        accepted.extend(current.get(module).cloned());
        accepted.extend(reserved.get(module).cloned().flatten());
        if let Some(swap) = swaps.get(module) {
            accepted.insert(swap.candidate_nonce.clone());
            accepted.extend(swap.incumbent_nonce.clone());
        }
        let mut records = self
            .program_bindings
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        records.active.retain(|(owner, nonce), _| {
            owner != module || nonce.starts_with("channel:") || accepted.contains(nonce)
        });
        records.retired.retain(|(owner, nonce), _| {
            owner != module || nonce.starts_with("channel:") || accepted.contains(nonce)
        });
        records
            .acceptable
            .retain(|(owner, key)| owner != module || key.starts_with("channel:"));
        records
            .acceptable
            .extend(accepted.into_iter().map(|nonce| (module.to_owned(), nonce)));
    }
    #[cfg(all(test, unix))]
    pub(crate) fn pause_program_binding_exit_for_test(
        &self,
        module: &str,
        nonce: &str,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        let (reached, observed) = tokio::sync::oneshot::channel();
        let (release, waiting) = tokio::sync::oneshot::channel();
        self.program_bindings.lock().unwrap().exit_pause = Some(ProgramBindingExitPause {
            key: (module.into(), nonce.into()),
            reached,
            release: waiting,
        });
        (observed, release)
    }
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
    #[cfg(all(test, windows))]
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
    pub(crate) fn has_program_binding_record(&self, module: &str, nonce: &str) -> bool {
        self.program_bindings
            .lock()
            .map(|records| {
                records
                    .retired
                    .contains_key(&(module.to_owned(), nonce.to_owned()))
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
        records.retired.retain(|(owner, _), _| owner != module);
        records.acceptable.retain(|(owner, _)| owner != module);
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
        if self.has_program_binding_record(module, nonce) {
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
        }
        .filter(|nonce| !nonce.is_empty());
        let (sender, receiver) = tokio::sync::watch::channel(None);
        // A channel-only launch has no nonce-table entry. Its owned publication
        // supplies a launch-verification signal without creating a bearer secret.
        let nonce = nonce.unwrap_or_else(|| {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            format!(
                "channel:{}",
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            )
        });
        let mut bindings = self.program_bindings.lock().ok()?;
        bindings.configured.insert(module.to_owned());
        if nonce.starts_with("channel:") {
            bindings
                .acceptable
                .insert((module.to_owned(), nonce.clone()));
        }
        if role == SpawnRole::Plain {
            bindings.active.retain(|(owner, _), _| owner != module);
        }
        let live = Arc::new(Mutex::new(std::sync::Weak::new()));
        let key = (module.to_owned(), nonce);
        bindings.retired.remove(&key);
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
            retired: Arc::new(std::sync::atomic::AtomicBool::new(false)),
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
            .or_else(|| records.retired.get(&(module.to_owned(), nonce.to_owned())))
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
