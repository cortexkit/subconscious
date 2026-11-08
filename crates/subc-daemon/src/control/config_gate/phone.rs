//! A phone card is one daemon-owned, endpoint-bound channel-zero request.

use super::*;

#[cfg(all(test, unix))]
#[path = "phone_tests.rs"]
mod tests;

pub(super) struct ApprovalAttemptGuard {
    owner: std::sync::Weak<crate::config_gate::runtime::RuntimeOwner>,
    attempt: u64,
}

impl ApprovalAttemptGuard {
    pub(super) fn new(
        owner: &Arc<crate::config_gate::runtime::RuntimeOwner>,
        attempt: u64,
    ) -> Self {
        Self {
            owner: Arc::downgrade(owner),
            attempt,
        }
    }
}

impl Drop for ApprovalAttemptGuard {
    fn drop(&mut self) {
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
}

impl Drop for PhoneCardGuard {
    fn drop(&mut self) {
        let _ = self
            .forwarding
            .cancel_module_control_rpc(self.endpoint, self.correlation);
        if self.answered {
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
            let gate = runtime.module_gate("callosum")?;
            let (pending, invalidated) = {
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
                let pending = self
                    .forwarding
                    .begin_module_control_rpc_for(
                        "callosum",
                        subc_protocol::consent::CONSENT_V1,
                        deadline,
                    )
                    .map_err(|_| unavailable())?;
                let binding = if pending.endpoint.connection_id == connection {
                    runtime.bind_phone_rpc(attempt, connection, pending.corr)
                } else {
                    Err(unavailable())
                };
                let invalidated = match binding {
                    Ok(receiver) => receiver,
                    Err(error) => {
                        let _ = self
                            .forwarding
                            .cancel_module_control_rpc(pending.endpoint, pending.corr);
                        return Err(error);
                    }
                };
                (pending, invalidated)
            };
            let PendingModuleControlRpc {
                endpoint,
                module_sink,
                negotiated_ver,
                corr,
                receiver,
            } = pending;
            let mut card = PhoneCardGuard {
                owner: Arc::downgrade(runtime),
                forwarding: self.forwarding.clone(),
                endpoint,
                sink: module_sink.clone(),
                version: negotiated_ver,
                correlation: corr,
                answered: false,
            };
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
                    .send_flushed(frame)
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
            tokio::select! {
                biased;
                _ = invalidated => Err(unavailable()),
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
