use super::*;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
/// Internal channel identity, allocated before a process starts. Once launches
/// own their spawn-event generation before HELLO, this type will use that
/// generation instead of the private sequence without changing binding consumers.
pub(super) struct ProgramBindingKey {
    pub(super) incarnation: String,
    channel_launch_seq: u64,
}

impl ProgramBindingKey {
    pub(super) fn new(handle: &SupervisorHandle) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        Self {
            incarnation: handle
                .spawn_events
                .0
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .daemon_incarnation
                .clone(),
            channel_launch_seq: NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        }
    }

    #[cfg(test)]
    fn locator(&self) -> String {
        format!("channel:{}:{}", self.incarnation, self.channel_launch_seq)
    }
}

#[derive(Debug, Default)]
pub(super) struct ProgramBindingRecords {
    active: HashMap<(String, ProgramBindingKey), ProgramBindingEntry>,
    retired: HashMap<(String, ProgramBindingKey), ProgramBindingEntry>,
    acceptable: HashSet<(String, ProgramBindingKey)>,
    // Legacy nonce admission resolves an alias to the same launch record. A
    // channel never consults these aliases, even if its HELLO carries a nonce.
    aliases: HashMap<(String, String), ProgramBindingKey>,
    // Clearing active records does not destroy the child's publication. Track
    // ownership separately so alias reuse reclaims only unowned launch keys.
    publications: HashMap<
        (String, ProgramBindingKey),
        std::sync::Weak<tokio::sync::watch::Sender<Option<bool>>>,
    >,
    configured: HashSet<String>,
    #[cfg(all(test, unix))]
    exit_pause: Option<ProgramBindingExitPause>,
    #[cfg(all(test, target_os = "linux"))]
    review_after_image_pause: Option<Arc<(std::sync::Barrier, std::sync::Barrier)>>,
}
type ProgramBindingTable = Mutex<ProgramBindingRecords>;
pub(super) type ProgramBindings = Arc<ProgramBindingTable>;

impl ProgramBindingRecords {
    fn resolve(&self, module: &str, nonce: &str) -> Option<ProgramBindingKey> {
        self.aliases
            .get(&(module.to_owned(), nonce.to_owned()))
            .cloned()
    }

    #[cfg(test)]
    fn fixture_key(&self, module: &str, proof: &str) -> Option<ProgramBindingKey> {
        self.resolve(module, proof).or_else(|| {
            self.active
                .keys()
                .chain(self.retired.keys())
                .find(|(owner, key)| owner == module && key.locator() == proof)
                .map(|(_, key)| key.clone())
        })
    }

    fn entry(&self, module: &str, nonce: &str) -> Option<ProgramBindingEntry> {
        self.active
            .get(&(module.to_owned(), self.resolve(module, nonce)?))
            .cloned()
    }
}

#[cfg(all(test, unix))]
#[derive(Debug)]
struct ProgramBindingExitPause {
    key: (String, String),
    reached: tokio::sync::oneshot::Sender<()>,
    release: tokio::sync::oneshot::Receiver<()>,
}

// Refused program bindings stay in `retired` while HELLO, consumer or open-swap
// admission still accepts their nonce alias. Erasing a refusal earlier would let
// a dead child's nonce bypass image checks by looking like an unbound legacy launch.
#[derive(Clone, Debug)]
pub(super) struct ProgramBindingEntry {
    receiver: tokio::sync::watch::Receiver<Option<bool>>,
    live: Arc<Mutex<std::sync::Weak<content_binding::LiveProgramBinding>>>,
    #[cfg(all(test, unix))]
    before_witness_pause: Option<Arc<(std::sync::Barrier, std::sync::Barrier)>>,
}

#[derive(Clone, Debug)]
pub(super) struct ProgramBindingPublication {
    signal: Arc<tokio::sync::watch::Sender<Option<bool>>>,
    live: Arc<Mutex<std::sync::Weak<content_binding::LiveProgramBinding>>>,
    bindings: std::sync::Weak<ProgramBindingTable>,
    key: (String, ProgramBindingKey),
    retired: Arc<std::sync::atomic::AtomicBool>,
    registration: Arc<Mutex<std::sync::Weak<RegistrationSpawn>>>,
}

// A carried pin can reap independently of the actor. Its hook shares retirement
// state without keeping the publication alive or postponing last-owner Drop.
#[cfg(unix)]
#[derive(Clone, Debug)]
pub(super) struct ProgramBindingRetirement {
    signal: std::sync::Weak<tokio::sync::watch::Sender<Option<bool>>>,
    live: std::sync::Weak<Mutex<std::sync::Weak<content_binding::LiveProgramBinding>>>,
    bindings: std::sync::Weak<ProgramBindingTable>,
    key: (String, ProgramBindingKey),
    retired: Arc<std::sync::atomic::AtomicBool>,
    registration: Arc<Mutex<std::sync::Weak<RegistrationSpawn>>>,
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
            registration: self.registration.clone(),
        })
    }
}

impl ProgramBindingPublication {
    #[cfg(all(test, unix))]
    pub(super) fn set_content_probe(&self, probe: content_binding::ContentProbe) {
        self.live
            .lock()
            .unwrap()
            .upgrade()
            .unwrap()
            .set_content_probe(probe);
    }
    #[cfg(all(test, unix))]
    pub(super) fn defer_content_kill_for_test(&self) -> content_binding::ContentKillDeferral {
        self.live
            .lock()
            .unwrap()
            .upgrade()
            .unwrap()
            .defer_kill_for_test()
    }

    #[cfg(all(test, unix))]
    pub(super) fn content_was_refused(&self) -> bool {
        self.live
            .lock()
            .unwrap()
            .upgrade()
            .is_none_or(|live| live.is_refused())
    }
    pub(super) fn attach_registration(&self, spawn: &Arc<RegistrationSpawn>) {
        *self.registration.lock().unwrap_or_else(|p| p.into_inner()) = Arc::downgrade(spawn);
    }
    pub(super) fn channel_key(&self) -> ProgramBindingKey {
        self.key.1.clone()
    }
    #[cfg(test)]
    pub(super) fn key_for_test(&self) -> String {
        self.key.1.locator()
    }
    pub(super) fn check(&self) -> Result<(), crate::config_gate::GateError> {
        self.verified_live()?.check()
    }

    pub(super) fn verdict(&self) -> Result<(), crate::config_gate::GateError> {
        self.verified_live()?.verdict()
    }

    fn verified_live(
        &self,
    ) -> Result<Arc<content_binding::LiveProgramBinding>, crate::config_gate::GateError> {
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
            })
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
            registration: self.registration.clone(),
            #[cfg(test)]
            upgrade_pause: None,
        }
    }
    #[cfg(all(test, unix))]
    pub(super) async fn pause_after_wait_for_test(&self) {
        let pause = self.bindings.upgrade().and_then(|bindings| {
            let mut records = bindings.lock().unwrap();
            if records.exit_pause.as_ref().is_some_and(|pause| {
                pause.key.0 == self.key.0
                    && records.fixture_key(&pause.key.0, &pause.key.1).as_ref() == Some(&self.key.1)
            }) {
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
        let registration = self
            .registration
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .upgrade();
        if let Some(registration) = registration {
            registration.retire();
        }
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
                        #[cfg(all(test, unix))]
                        before_witness_pause: None,
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
            if let Some(bindings) = self.bindings.upgrade() {
                let mut records = bindings.lock().unwrap_or_else(|p| p.into_inner());
                if !records.aliases.values().any(|key| key == &self.key.1) {
                    records.active.remove(&self.key);
                    records.retired.remove(&self.key);
                    records.acceptable.remove(&self.key);
                    records.publications.remove(&self.key);
                }
            }
        }
    }
}

impl SupervisorHandle {
    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn hold_program_review_after_image_for_test(
        &self,
    ) -> Arc<(std::sync::Barrier, std::sync::Barrier)> {
        let pause = Arc::new((std::sync::Barrier::new(2), std::sync::Barrier::new(2)));
        self.program_bindings
            .lock()
            .unwrap()
            .review_after_image_pause = Some(pause.clone());
        pause
    }
    #[cfg(all(test, unix))]
    pub(crate) fn program_binding_fixture_nonce(&self, module: &str, proof: &str) -> String {
        let nonce = format!("fixture-nonce:{proof}");
        let mut records = self.program_bindings.lock().unwrap();
        let key = records
            .fixture_key(module, proof)
            .expect("fixture launch binding");
        records
            .aliases
            .insert((module.to_owned(), nonce.clone()), key);
        nonce
    }

    #[cfg(all(test, unix))]
    pub(crate) fn has_program_binding_record_for_test(&self, module: &str, proof: &str) -> bool {
        let records = self.program_bindings.lock().unwrap();
        records
            .fixture_key(module, proof)
            .is_some_and(|key| records.retired.contains_key(&(module.to_owned(), key)))
            || records.active.keys().any(|(owner, _)| owner == module)
    }

    #[cfg(test)]
    pub(crate) fn has_live_verified_program_binding_for_test(
        &self,
        module: &str,
        proof: &str,
    ) -> bool {
        let records = self.program_bindings.lock().unwrap();
        records
            .fixture_key(module, proof)
            .and_then(|key| records.active.get(&(module.to_owned(), key)))
            .is_some_and(|entry| *entry.receiver.borrow() == Some(true))
    }

    #[cfg(test)]
    pub(crate) async fn wait_program_binding_for_test(&self, module: &str, proof: &str) -> bool {
        let binding = {
            let records = self.program_bindings.lock().unwrap();
            records
                .fixture_key(module, proof)
                .and_then(|key| {
                    records
                        .active
                        .get(&(module.to_owned(), key.clone()))
                        .or_else(|| records.retired.get(&(module.to_owned(), key)))
                })
                .map(|entry| entry.receiver.clone())
        };
        let Some(mut binding) = binding else {
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

    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn supersede_review_launch_for_test(&self, module: &str) {
        let module = self.get(module).unwrap();
        let mut snapshot = module.inner.snapshot.lock().unwrap();
        snapshot.process_start_time = snapshot.process_start_time.map(|start| start + 1);
    }
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
        let removed: HashSet<_> = records
            .aliases
            .iter()
            .filter(|((owner, nonce), _)| owner == module && !accepted.contains(nonce))
            .map(|(_, key)| key.clone())
            .collect();
        records
            .aliases
            .retain(|(owner, nonce), _| owner != module || accepted.contains(nonce));
        records
            .active
            .retain(|(owner, key), _| owner != module || !removed.contains(key));
        records
            .retired
            .retain(|(owner, key), _| owner != module || !removed.contains(key));
        records
            .acceptable
            .retain(|(owner, key)| owner != module || !removed.contains(key));
        records.publications.retain(|(owner, key), publication| {
            owner != module || !removed.contains(key) || publication.strong_count() != 0
        });
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
        // An active record can be removed while the child still owns its
        // publication. Resolve the request at exit, after retirement has
        // published the refusal record, rather than requiring a record to arm.
        self.program_bindings.lock().unwrap().exit_pause = Some(ProgramBindingExitPause {
            key: (module.into(), nonce.into()),
            reached,
            release: waiting,
        });
        (observed, release)
    }
    pub(crate) fn install_binding_retirement(
        &self,
        owner: &Arc<crate::config_gate::runtime::RuntimeOwner>,
    ) {
        *self.gate_runtime.lock().unwrap_or_else(|p| p.into_inner()) = Some(Arc::downgrade(owner));
        let ids = self.configured_ids.lock();
        self.publish_configured_ids(ids.as_ref().ok().map(|ids| &**ids));
        drop(ids);
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
    #[cfg(windows)]
    pub(crate) fn program_hello_accepted(&self, module: &str, nonce: &str) {
        let Some(key) = self.binding_key(module, nonce) else {
            return;
        };
        let live = self
            .program_bindings
            .lock()
            .ok()
            .and_then(|bindings| bindings.active.get(&(module.to_owned(), key)).cloned())
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
                    .resolve(module, nonce)
                    .is_some_and(|key| records.retired.contains_key(&(module.to_owned(), key)))
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
                    .get(&(module.to_owned(), records.resolve(module, nonce)?))
                    .map(|entry| *entry.receiver.borrow() == Some(true))
            })
            .unwrap_or(false)
    }

    pub(crate) fn has_live_verified_channel_binding(&self, spawn: &RegistrationSpawn) -> bool {
        let entry = self.program_bindings.lock().ok().and_then(|records| {
            records
                .active
                .get(&(spawn.module_id.clone(), spawn.binding_key.clone()))
                .cloned()
        });
        entry
            .and_then(|entry| {
                #[cfg(all(test, unix))]
                if let Some(pause) = &entry.before_witness_pause {
                    pause.0.wait();
                    pause.1.wait();
                }
                Some(
                    *entry.receiver.borrow() == Some(true)
                        && entry.live.lock().ok()?.upgrade().is_some(),
                )
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
        records.aliases.retain(|(owner, _), _| owner != module);
        records.publications.retain(|(owner, _), _| owner != module);
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
            .entry(module, nonce)
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
        self.check_review_running_image(module, None, self.program_binding(module, nonce).is_some())
    }

    pub(crate) fn check_channel_program_review(
        &self,
        registry: &Registry,
        module: &str,
        connection: ConnectionId,
    ) -> Result<(), crate::config_gate::GateError> {
        let refused = || {
            crate::config_gate::GateError::new(
                "program_refused",
                "review requires the current admitted supervised channel",
            )
        };
        let spawn = self.registration_spawn(connection).ok_or_else(refused)?;
        if !self.registration_is_current_live(registry, module, connection, None) {
            return Err(refused());
        }
        let entry = self
            .program_bindings
            .lock()
            .map_err(|_| refused())?
            .active
            .get(&(module.to_owned(), spawn.binding_key.clone()))
            .cloned();
        if spawn.has_program_binding() && entry.is_none() && self.module_is_gated(module) {
            return Err(refused());
        }
        let mut expected = None;
        if let Some(entry) = entry {
            if *entry.receiver.borrow() != Some(true) {
                return Err(refused());
            }
            let live = entry
                .live
                .lock()
                .map_err(|_| refused())?
                .upgrade()
                .ok_or_else(refused)?;
            live.check()?;
            expected = Some(live.expected_image()?);
        }
        self.check_review_running_image(module, expected, false)?;
        #[cfg(all(test, target_os = "linux"))]
        {
            let pause = self
                .program_bindings
                .lock()
                .unwrap()
                .review_after_image_pause
                .take();
            if let Some(pause) = pause {
                pause.0.wait();
                pause.1.wait();
            }
        }
        if !self.registration_is_current_live(registry, module, connection, None) {
            return Err(refused());
        }
        Ok(())
    }

    fn check_review_running_image(
        &self,
        module: &str,
        expected_image: Option<subc_os::RetainedFileIdentity>,
        nonce_binding: bool,
    ) -> Result<(), crate::config_gate::GateError> {
        let refused = || {
            crate::config_gate::GateError::new(
                "program_refused",
                "review requires the current live supervised image",
            )
        };
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
            let expected = if let Some(expected) = expected_image {
                expected
            } else if nonce_binding {
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
            let _ = nonce_binding;
            if expected_image.is_some_and(|expected| {
                _identity
                    .as_ref()
                    .and_then(|identity| identity.windows_image.as_ref())
                    .is_none_or(|image| image.identity() != expected)
            }) {
                return Err(refused());
            }
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
            SpawnRole::SwapCandidate if !self.module_is_gated(module) => self
                .swaps
                .lock()
                .ok()?
                .get(module)
                .map(|swap| swap.candidate_nonce.clone()),
            SpawnRole::SwapCandidate => None,
        }
        .filter(|nonce| !nonce.is_empty());
        let (sender, receiver) = tokio::sync::watch::channel(None);
        let signal = Arc::new(sender);
        let channel_key = ProgramBindingKey::new(self);
        let mut bindings = self.program_bindings.lock().ok()?;
        bindings.configured.insert(module.to_owned());
        if let Some(nonce) = nonce {
            if let Some(previous) = bindings
                .aliases
                .insert((module.to_owned(), nonce), channel_key.clone())
            {
                let previous_key = (module.to_owned(), previous);
                let owned = bindings
                    .publications
                    .get(&previous_key)
                    .is_some_and(|publication| publication.strong_count() != 0);
                if !owned {
                    bindings.active.remove(&previous_key);
                    bindings.retired.remove(&previous_key);
                    bindings.acceptable.remove(&previous_key);
                    bindings.publications.remove(&previous_key);
                }
            }
        }
        if role == SpawnRole::Plain {
            bindings.active.retain(|(owner, _), _| owner != module);
        }
        let live = Arc::new(Mutex::new(std::sync::Weak::new()));
        let key = (module.to_owned(), channel_key);
        bindings.acceptable.insert(key.clone());
        bindings
            .publications
            .insert(key.clone(), Arc::downgrade(&signal));
        bindings.active.insert(
            key.clone(),
            ProgramBindingEntry {
                receiver,
                live: live.clone(),
                #[cfg(all(test, unix))]
                before_witness_pause: None,
            },
        );
        Some(ProgramBindingPublication {
            signal,
            live,
            bindings: Arc::downgrade(&self.program_bindings),
            key,
            retired: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            registration: Arc::new(Mutex::new(std::sync::Weak::new())),
        })
    }

    fn program_binding(
        &self,
        module: &str,
        nonce: &str,
    ) -> Option<tokio::sync::watch::Receiver<Option<bool>>> {
        let records = self.program_bindings.lock().ok()?;
        let key = records.resolve(module, nonce)?;
        records
            .active
            .get(&(module.to_owned(), key.clone()))
            .or_else(|| records.retired.get(&(module.to_owned(), key)))
            .map(|entry| entry.receiver.clone())
    }

    #[cfg(windows)]
    fn binding_key(&self, module: &str, nonce: &str) -> Option<ProgramBindingKey> {
        self.program_bindings.lock().ok()?.resolve(module, nonce)
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
