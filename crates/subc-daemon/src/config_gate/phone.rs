//! Live phone bindings and cancellation belong to the serialized config owner.

use super::*;

pub(crate) struct PhoneWithdrawal {
    pub connection: ConnectionId,
    pub sink: crate::router::FrameSink,
    pub frame: subc_protocol::frame::Frame,
}

#[derive(Default)]
pub(super) struct PhoneWithdrawals {
    queued: VecDeque<PhoneWithdrawal>,
    withdrawing: HashMap<ConnectionId, usize>,
}

impl OwnerState {
    pub(super) fn disconnected(&mut self, connection: ConnectionId) {
        self.invalidate_phone(connection);
        self.desktop_views.remove(&connection.get());
        self.delivery.disconnected(connection);
        for digest in self.gate.connection_approval_digests(connection.get()) {
            self.reviewed(&digest, Instant::now());
        }
        for attempt in self.gate.cancel_connection_approvals(connection.get()) {
            self.phone_waiters.remove(&attempt);
        }
        self.approval_requests
            .retain(|(caller, _), _| *caller != connection);
        self.declarations.remove(&connection);
        self.anchor_waiters.remove(&connection);
        self.anchor_hints.remove(&connection);
        self.active_declarations
            .retain(|_, current| *current != connection);
    }

    pub(super) fn cancel_approval(&mut self, attempt: u64) {
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
    pub(super) fn phone_is_withdrawing(&self, connection: ConnectionId) -> bool {
        self.phone_withdrawals
            .lock()
            .map(|queue| queue.withdrawing.contains_key(&connection))
            .unwrap_or(true)
    }
    #[cfg(all(test, feature = "dev-desktop-approval"))]
    pub(crate) fn set_desktop_receipt_hook(&self, hook: impl FnOnce() + Send + 'static) {
        *self.desktop_receipt_hook.lock().unwrap() = Some(Box::new(hook));
    }
    #[cfg(all(test, any(unix, feature = "dev-desktop-approval")))]
    pub(crate) fn with_owner_lock_for_test(&self, work: impl FnOnce()) {
        let _state = self.lock().unwrap();
        work();
    }
    #[cfg(all(test, unix))]
    pub(crate) fn set_begin_approval_hook(&self, hook: impl FnOnce() + Send + 'static) {
        *self.begin_approval_hook.lock().unwrap() = Some(Box::new(hook));
    }
    #[cfg(all(test, unix))]
    pub(crate) fn begin_approval_completions_for_test(&self) -> usize {
        self.begin_approval_completions
            .load(std::sync::atomic::Ordering::SeqCst)
    }
    #[cfg(all(test, unix))]
    pub(crate) fn set_phone_bind_hook(&self, hook: impl FnOnce() + Send + 'static) {
        *self.phone_bind_hook.lock().unwrap() = Some(Box::new(hook));
    }
    pub(super) fn drain_canceled_approvals(&self, state: &mut OwnerState) {
        if let Ok(mut pending) = self.canceled_approvals.lock() {
            for attempt in pending.drain(..) {
                state.cancel_approval(attempt);
            }
        }
        if let Ok(mut index) = self.approval_request_index.lock() {
            index.retain(|key, request| {
                request.attempt() == 0
                    || state.approval_requests.get(key) == Some(&request.attempt())
            });
        }
    }

    pub(super) fn drain_disconnections(&self, state: &mut OwnerState) {
        if let Ok(mut pending) = self.disconnected_pending.lock() {
            for connection in pending.drain(..) {
                state.disconnected(connection);
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
        self.phone_entry_from_state(&state, connection)
    }

    pub(super) fn phone_entry_from_state(
        &self,
        state: &OwnerState,
        connection: ConnectionId,
    ) -> Result<Option<crate::daemon_config::ConfiguredModule>> {
        if state.active_declarations.get("callosum") != Some(&connection)
            || self
                .phone_withdrawals
                .lock()
                .map(|queue| queue.withdrawing.contains_key(&connection))
                .unwrap_or(true)
        {
            return Ok(None);
        }
        let Some((module, declaration)) = state.declarations.get(&connection).cloned() else {
            return Ok(None);
        };
        let (_, body) = state.store.loaded_admission_state();
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
        #[cfg(test)]
        if let Some(hook) = self.phone_bind_hook.lock().unwrap().take() {
            hook();
        }
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

    #[cfg(test)]
    pub(crate) fn cancel_approval_request(&self, caller: ConnectionId, correlation: u64) -> bool {
        self.cancel_approval_request_outcome(caller, correlation)
            != super::CancelApprovalRequest::Unknown
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
        if let Ok(mut queue) = self.phone_withdrawals.lock() {
            *queue.withdrawing.entry(connection).or_default() += 1;
            queue.queued.push_back(PhoneWithdrawal {
                connection,
                sink,
                frame,
            });
        }
        self.notify_delivery_changed();
    }

    pub(crate) fn take_phone_withdrawals(&self) -> Vec<PhoneWithdrawal> {
        self.phone_withdrawals
            .lock()
            .map(|mut queue| queue.queued.drain(..).collect())
            .unwrap_or_default()
    }

    pub(crate) fn phone_withdrawn(&self, connection: ConnectionId) {
        if let Ok(mut queue) = self.phone_withdrawals.lock() {
            if let Some(count) = queue.withdrawing.get_mut(&connection) {
                *count -= 1;
                if *count == 0 {
                    queue.withdrawing.remove(&connection);
                }
            }
        }
    }
}
