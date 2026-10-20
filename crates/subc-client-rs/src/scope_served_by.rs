pub use subc_control::{ScopeServedByAnswer, ScopeServedByReply};
use subc_control::{ScopeServedByRequest, SCOPE_SERVED_BY_OP};
use subc_protocol::{scope::SCOPE_SERVED_BY_CAPABILITY, ErrorBody, Frame, FrameType};
use tokio::sync::oneshot;

use crate::{ModuleHandle, ScopeCallError, CATALOG_UPDATE_TIMEOUT};

impl ModuleHandle {
    /// Read acknowledged-bind history for an epoch owned by this module.
    /// Only `Served` is affirmative. Ended or absent epochs are `Unknown`, as are
    /// epochs not yet re-registered after a daemon restart.
    /// Without `scope-served-by/v1` in HELLO_ACK, nothing is sent.
    pub async fn scope_served_by(
        &self,
        scope_ref: String,
        scope_epoch: u64,
        module_id: String,
    ) -> Result<ScopeServedByReply, ScopeCallError> {
        if !self
            .shared
            .daemon_capabilities
            .iter()
            .any(|cap| cap == SCOPE_SERVED_BY_CAPABILITY)
        {
            return Err(ScopeCallError::NotSupported {
                op: SCOPE_SERVED_BY_OP,
            });
        }
        let body = serde_json::to_vec(&ScopeServedByRequest::new(
            scope_ref,
            scope_epoch,
            module_id,
        ))
        .map_err(|err| ScopeCallError::Protocol(err.to_string()))?;
        let (corr, writer, rx) = {
            let mut inner = self.shared.lock_inner();
            if inner.closed {
                return Err(ScopeCallError::ConnectionClosed);
            }
            let writer = inner
                .writer
                .clone()
                .ok_or(ScopeCallError::ConnectionClosed)?;
            let corr = crate::next_module_control_corr(&mut inner)
                .ok_or(ScopeCallError::ConnectionClosed)?;
            let (tx, rx) = oneshot::channel();
            inner.pending_scope_served_by.insert(corr, tx);
            (corr, writer, rx)
        };
        let _guard = ServedByWaiter {
            handle: self.clone(),
            corr,
        };
        let frame = Frame::build_with_version(
            self.shared.negotiated_ver,
            FrameType::Request,
            crate::control_flags(),
            0,
            0,
            corr,
            body,
        )
        .map_err(|err| ScopeCallError::Protocol(err.to_string()))?;
        let reply = tokio::time::timeout(CATALOG_UPDATE_TIMEOUT, async {
            writer
                .send(frame)
                .await
                .map_err(|_| ScopeCallError::ConnectionClosed)?;
            rx.await.map_err(|_| ScopeCallError::ConnectionClosed)
        })
        .await
        .map_err(|_| ScopeCallError::Timeout)??;
        match reply.header.ty {
            FrameType::Response => serde_json::from_slice(&reply.body)
                .map_err(|err| ScopeCallError::Protocol(err.to_string())),
            FrameType::Error => {
                let body: ErrorBody = serde_json::from_slice(&reply.body)
                    .map_err(|err| ScopeCallError::Protocol(err.to_string()))?;
                Err(ScopeCallError::Refused {
                    code: body.code,
                    message: body.message,
                })
            }
            other => Err(ScopeCallError::Protocol(format!(
                "unexpected {SCOPE_SERVED_BY_OP} terminal frame: {other:?}"
            ))),
        }
    }
}

struct ServedByWaiter {
    handle: ModuleHandle,
    corr: u64,
}
impl Drop for ServedByWaiter {
    fn drop(&mut self) {
        self.handle
            .shared
            .lock_inner()
            .pending_scope_served_by
            .remove(&self.corr);
    }
}
