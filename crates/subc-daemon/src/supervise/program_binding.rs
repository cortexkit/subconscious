use super::*;

pub(super) type ProgramBindings = Arc<Mutex<HashMap<(String, String), ProgramBindingEntry>>>;

#[derive(Clone, Debug)]
pub(super) struct ProgramBindingEntry {
    receiver: tokio::sync::watch::Receiver<Option<bool>>,
    live: Arc<Mutex<std::sync::Weak<content_binding::LiveProgramBinding>>>,
}

#[derive(Clone, Debug)]
pub(super) struct ProgramBindingPublication {
    signal: Arc<tokio::sync::watch::Sender<Option<bool>>>,
    live: Arc<Mutex<std::sync::Weak<content_binding::LiveProgramBinding>>>,
}

impl ProgramBindingPublication {
    pub(super) fn verify(&mut self) {
        self.signal.send_replace(Some(true));
    }
    #[cfg(target_os = "macos")]
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
            self.signal.send_replace(Some(false));
        }
    }
}

impl SupervisorHandle {
    #[cfg(windows)]
    pub(crate) fn program_hello_accepted(&self, module: &str, nonce: &str) {
        let live = self
            .program_bindings
            .lock()
            .ok()
            .and_then(|bindings| {
                bindings
                    .get(&(module.to_owned(), nonce.to_owned()))
                    .cloned()
            })
            .and_then(|entry| entry.live.lock().ok()?.upgrade());
        if let Some(live) = live {
            live.hello_accepted();
        }
    }
    pub(crate) fn has_program_binding(&self, module: &str, nonce: &str) -> bool {
        self.program_binding(module, nonce).is_some()
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
        if !self.spawned_consumer_authorized(module, nonce)
            || !self.program_binding_verified(module, nonce)
        {
            return Err(refused());
        }
        if self.has_program_binding(module, nonce) {
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
        if role == SpawnRole::Plain {
            bindings.retain(|(owner, _), _| owner != module);
        }
        let live = Arc::new(Mutex::new(std::sync::Weak::new()));
        bindings.insert(
            (module.to_owned(), nonce),
            ProgramBindingEntry {
                receiver,
                live: live.clone(),
            },
        );
        Some(ProgramBindingPublication {
            signal: Arc::new(sender),
            live,
        })
    }

    fn program_binding(
        &self,
        module: &str,
        nonce: &str,
    ) -> Option<tokio::sync::watch::Receiver<Option<bool>>> {
        self.program_bindings
            .lock()
            .ok()?
            .get(&(module.to_owned(), nonce.to_owned()))
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

    pub(crate) fn program_binding_verified(&self, module: &str, nonce: &str) -> bool {
        self.program_binding(module, nonce)
            .is_none_or(|binding| *binding.borrow() == Some(true))
    }
}
