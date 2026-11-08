//! Live phone bindings and cancellation belong to the serialized config owner.

use super::*;

pub(crate) struct PhoneWithdrawal {
    pub connection: ConnectionId,
    pub sink: crate::router::FrameSink,
    pub frame: subc_protocol::frame::Frame,
}

impl OwnerState {
    fn cancel_approval(&mut self, attempt: u64) {
        if let Ok(digest) = self.gate.approval_digest(attempt) {
            self.reviewed(&digest, Instant::now());
        }
        self.gate.cancel_approval(attempt);
        self.phone_waiters.remove(&attempt);
        self.approval_requests
            .retain(|_, current| *current != attempt);
    }

    pub(super) fn invalidate_phone(&mut self, connection: ConnectionId) {
        for digest in self.gate.phone_approval_digests(connection.get()) {
            self.reviewed(&digest, Instant::now());
        }
        for attempt in self.gate.invalidate_phone(connection.get()) {
            self.phone_waiters.remove(&attempt);
        }
    }

    pub(super) fn phone_target_changed(&mut self, module: &str, connection: ConnectionId) {
        if module == "callosum" {
            if let Some(previous) = self.active_declarations.get(module).copied() {
                if previous != connection {
                    self.invalidate_phone(previous);
                }
            }
        }
    }
}

impl RuntimeOwner {
    pub(super) fn drain_canceled_approvals(&self, state: &mut OwnerState) {
        if let Ok(mut pending) = self.canceled_approvals.lock() {
            for attempt in pending.drain(..) {
                state.cancel_approval(attempt);
            }
        }
    }

    /// The active generation's declaration and daemon entry, not a pending edit
    /// or the connection's claimed program. Program admission remains a separate
    /// supervisor check against the exact current registration.
    pub(crate) fn approved_phone_entry(
        &self,
        connection: ConnectionId,
    ) -> Result<Option<crate::daemon_config::ConfiguredModule>> {
        let mut state = self.lock()?;
        Self::check_authority(&mut state, false)?;
        state.store.delivery_ready()?;
        if state.active_declarations.get("callosum") != Some(&connection)
            || state.phone_withdrawing.contains_key(&connection)
        {
            return Ok(None);
        }
        let Some((module, declaration)) = state.declarations.get(&connection).cloned() else {
            return Ok(None);
        };
        let body = state.store.body()?;
        let tuple = crate::config_store::Tuple {
            module: "callosum".into(),
            project: None,
        };
        let Some(delivery) = body
            .versions
            .iter()
            .find(|(key, _)| key == &tuple)
            .and_then(|(_, versions)| versions.active)
            .and_then(|generation| body.generations.get(&generation))
            .and_then(|entry| entry.delivery.as_ref())
        else {
            return Ok(None);
        };
        if module != "callosum" || delivery.declarations.get("callosum") != Some(&declaration) {
            return Ok(None);
        }
        let Some(bytes) = delivery
            .daemon
            .as_ref()
            .and_then(|layer| layer.bytes.as_deref())
        else {
            return Ok(None);
        };
        let text = std::str::from_utf8(bytes).map_err(GateError::invalid)?;
        let (config, _) =
            crate::daemon_config::parse_with_activity(text, &self.capturer.daemon_path)
                .map_err(GateError::invalid)?;
        Ok(config
            .modules
            .into_iter()
            .find(|module| module.module_id == "callosum"))
    }

    pub(crate) fn bind_phone_rpc(
        &self,
        attempt: u64,
        connection: ConnectionId,
        correlation: u64,
    ) -> Result<tokio::sync::oneshot::Receiver<()>> {
        let mut state = self.lock()?;
        state
            .gate
            .bind_phone_rpc(attempt, connection.get(), correlation)?;
        let (sender, receiver) = tokio::sync::oneshot::channel();
        state.phone_waiters.insert(attempt, sender);
        Ok(receiver)
    }

    pub(crate) fn cancel_approval(&self, attempt: u64) {
        if let Ok(mut state) = self.inner.try_lock() {
            state.cancel_approval(attempt);
        } else if let Ok(mut pending) = self.canceled_approvals.lock() {
            pending.push(attempt);
        }
        self.notify_delivery_changed();
    }

    pub(crate) fn track_approval_request(
        &self,
        attempt: u64,
        caller: ConnectionId,
        correlation: u64,
    ) -> Result<()> {
        let mut state = self.lock()?;
        state.gate.approval_digest(attempt)?;
        if state.approval_requests.contains_key(&(caller, correlation)) {
            return Err(GateError::new(
                "config_busy",
                "approval request correlation is already pending",
            ));
        }
        state
            .approval_requests
            .insert((caller, correlation), attempt);
        Ok(())
    }

    pub(crate) fn cancel_approval_request(&self, caller: ConnectionId, correlation: u64) -> bool {
        let attempt = self
            .inner
            .lock()
            .ok()
            .and_then(|state| state.approval_requests.get(&(caller, correlation)).copied());
        if let Some(attempt) = attempt {
            self.cancel_approval(attempt);
            true
        } else {
            false
        }
    }

    /// A full writer queue must delay withdrawal, not discard the Cancel. The
    /// daemon-owned delivery coordinator sends it; no detached task retains the
    /// store. New cards on this endpoint wait until withdrawal leaves the writer.
    pub(crate) fn queue_phone_withdrawal(
        &self,
        connection: ConnectionId,
        sink: crate::router::FrameSink,
        frame: subc_protocol::frame::Frame,
    ) {
        if let Ok(mut state) = self.inner.lock() {
            *state.phone_withdrawing.entry(connection).or_default() += 1;
            state.phone_withdrawals.push_back(PhoneWithdrawal {
                connection,
                sink,
                frame,
            });
        }
        self.notify_delivery_changed();
    }

    pub(crate) fn take_phone_withdrawals(&self) -> Vec<PhoneWithdrawal> {
        self.inner
            .lock()
            .map(|mut state| state.phone_withdrawals.drain(..).collect())
            .unwrap_or_default()
    }

    pub(crate) fn phone_withdrawn(&self, connection: ConnectionId) {
        if let Ok(mut state) = self.inner.lock() {
            if let Some(count) = state.phone_withdrawing.get_mut(&connection) {
                *count -= 1;
                if *count == 0 {
                    state.phone_withdrawing.remove(&connection);
                }
            }
        }
    }
}
