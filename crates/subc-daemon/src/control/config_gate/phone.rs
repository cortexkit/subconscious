//! A phone card is one daemon-owned, endpoint-bound channel-zero request.

use super::*;

#[cfg(all(test, unix))]
#[path = "phone_tests.rs"]
mod tests;

pub(super) struct ApprovalAttemptGuard {
    owner: std::sync::Weak<crate::config_gate::runtime::RuntimeOwner>,
    attempt: u64,
    pub(super) commit_window: Option<Arc<crate::config_gate::runtime::ApprovalCommitWindow>>,
}

impl ApprovalAttemptGuard {
    pub(super) fn new(
        owner: &Arc<crate::config_gate::runtime::RuntimeOwner>,
        attempt: u64,
    ) -> Self {
        Self {
            owner: Arc::downgrade(owner),
            attempt,
            commit_window: None,
        }
    }
}

impl Drop for ApprovalAttemptGuard {
    fn drop(&mut self) {
        if let Some(window) = &self.commit_window {
            window.expire();
        }
        if let Some(owner) = self.owner.upgrade() {
            owner.cancel_approval(self.attempt);
        }
    }
}

struct PhoneCardGuard {
    owner: std::sync::Weak<crate::config_gate::runtime::RuntimeOwner>,
    forwarding: Arc<crate::forwarding::ForwardingTable>,
    endpoint: crate::forwarding::ModuleEndpointId,
    sink: crate::router::FrameSink,
    version: u8,
    correlation: u64,
    answered: bool,
    enqueued: bool,
}

impl Drop for PhoneCardGuard {
    fn drop(&mut self) {
        let _ = self
            .forwarding
            .cancel_module_control_rpc(self.endpoint, self.correlation);
        if self.answered || !self.enqueued {
            return;
        }
        let Ok(cancel) = Frame::build_with_version(
            self.version,
            FrameType::Cancel,
            control_flags(),
            0,
            0,
            self.correlation,
            Vec::new(),
        ) else {
            return;
        };
        if self.sink.try_send(cancel.clone()).is_err() {
            if let Some(owner) = self.owner.upgrade() {
                owner.queue_phone_withdrawal(
                    self.endpoint.connection_id,
                    self.sink.clone(),
                    cancel,
                );
            }
        }
    }
}

impl ControlHandler {
    pub(super) async fn config_phone_rpc(
        &self,
        runtime: &Arc<crate::config_gate::runtime::RuntimeOwner>,
        attempt: u64,
        connection: ConnectionId,
        deadline: Instant,
        request: &subc_protocol::consent::ConsentRequest,
    ) -> std::result::Result<ApprovalReply, GateError> {
        let unavailable = || {
            GateError::new(
                "approval_surface_unavailable",
                "phone approval surface is unavailable",
            )
        };
        let exchange = async {
            // Consent does not require config.review or a pending generation.
            // Observe the real process feed and active registration directly,
            // including deliberate exits and consent-only swap cutovers.
            let mut process_changes = self.supervisor.subscribe_process_changes("callosum");
            let mut registration_changes = self
                .registry
                .subscribe_module_changes("callosum")
                .map_err(|_| unavailable())?;
            let gate = runtime.module_gate("callosum")?;
            let (pending, invalidated, spawn, mut card) = {
                let _gate = gate.lock().await;
                if self
                    .config_phone_surface()
                    .await
                    .as_ref()
                    .map(|phone| phone.connection)
                    != Some(connection.get())
                {
                    return Err(unavailable());
                }
                let spawn = self
                    .supervisor
                    .current_spawn("callosum")
                    .ok_or_else(unavailable)?;
                let pending = self
                    .forwarding
                    .begin_module_control_rpc_for(
                        "callosum",
                        subc_protocol::consent::CONSENT_V1,
                        deadline,
                    )
                    .map_err(|_| unavailable())?;
                // Allocation already owns forwarding state. Arm cleanup before
                // the first bind await, even though no card has been sent yet.
                let card = PhoneCardGuard {
                    owner: Arc::downgrade(runtime),
                    forwarding: self.forwarding.clone(),
                    endpoint: pending.endpoint,
                    sink: pending.module_sink.clone(),
                    version: pending.negotiated_ver,
                    correlation: pending.corr,
                    answered: false,
                    enqueued: false,
                };
                let binding = if pending.endpoint.connection_id == connection {
                    let corr = pending.corr;
                    config_work(runtime.clone(), move |owner| {
                        owner.bind_phone_rpc(attempt, connection, corr)
                    })
                    .await
                } else {
                    Err(unavailable())
                };
                let invalidated = binding?;
                (pending, invalidated, spawn, card)
            };
            let PendingModuleControlRpc {
                endpoint,
                module_sink,
                negotiated_ver,
                corr,
                receiver,
            } = pending;
            let frame = Frame::build_with_version(
                negotiated_ver,
                FrameType::Request,
                control_flags(),
                0,
                0,
                corr,
                serde_json::to_vec(request).map_err(|_| unavailable())?,
            )
            .map_err(|_| unavailable())?;
            let response = async {
                module_sink
                    .send_flushed_notifying_enqueue(frame, || card.enqueued = true)
                    .await
                    .map_err(|_| unavailable())?;
                let outcome = receiver.await.map_err(|_| unavailable())?;
                let response = match outcome {
                    ModuleControlRpcOutcome::RawResponse(bytes) => {
                        serde_json::from_slice::<subc_protocol::consent::ConsentResponse>(&bytes)
                            .ok()
                    }
                    _ => None,
                };
                if response.is_some() {
                    card.answered = true;
                }
                Ok(ApprovalReply::Phone {
                    connection: connection.get(),
                    correlation: corr,
                    response,
                })
            };
            let lifecycle_invalidated = async {
                loop {
                    if self.supervisor.current_spawn("callosum").as_ref() != Some(&spawn)
                        || self
                            .registry
                            .get_module("callosum")
                            .ok()
                            .flatten()
                            .is_none_or(|registration| registration.connection_id != connection)
                        || self
                            .forwarding
                            .active_module_endpoint("callosum")
                            .ok()
                            .flatten()
                            .is_none_or(|current| current != endpoint)
                    {
                        return;
                    }
                    tokio::select! {
                        result = process_changes.changed() => { if result.is_err() { return; } },
                        result = registration_changes.changed() => { if result.is_err() { return; } },
                    }
                }
            };
            tokio::select! {
                biased;
                _ = invalidated => Err(unavailable()),
                _ = lifecycle_invalidated => Err(unavailable()),
                result = timeout_at(deadline, response) => match result {
                    Ok(reply) => reply,
                    Err(_) => Ok(ApprovalReply::Phone { connection: connection.get(), correlation: corr, response: None }),
                },
            }
        };
        // Allocation, module-gate contention and transport backpressure all spend
        // the same card budget. The final facts and commit locks are taken later.
        match timeout_at(deadline, exchange).await {
            Ok(result) => result,
            Err(_) => Err(GateError::new(
                "approval_declined",
                "phone approval was unanswered",
            )),
        }
    }
}
