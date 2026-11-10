//! Module declarations of agent-requested command roots.

use crate::{AgentCommandFinished, AgentCommandRoot, ModuleHandle};
use subc_protocol::{session::ModuleControlRequestFromModule, ErrorBody, Frame, FrameType};
use tokio::sync::oneshot;

/// A refused or incomplete agent-command declaration never closes the module's connection.
#[derive(Debug)]
#[non_exhaustive]
pub enum AgentCommandsError {
    Unsupported,
    NotSupervised,
    Refused(Box<ErrorBody>),
    ConnectionClosed,
    Timeout,
    Protocol(String),
}

impl AgentCommandsError {
    /// Retryable refusals need a fresh correlation; a closed declaration queue
    /// additionally needs a current supervised connection. This does not retry automatically.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Refused(body) => {
                subc_protocol::error_codes::is_retryable_agent_commands(&body.code)
            }
            Self::Timeout | Self::ConnectionClosed => true,
            Self::Unsupported | Self::NotSupervised | Self::Protocol(_) => false,
        }
    }
}

impl std::fmt::Display for AgentCommandsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported => f.write_str("daemon does not support agent command declarations"),
            Self::NotSupervised => {
                f.write_str("agent commands require this module's supervised connection")
            }
            Self::Refused(body) => write!(
                f,
                "agent command declaration refused: {} ({})",
                body.message, body.code
            ),
            Self::ConnectionClosed => {
                f.write_str("connection closed before agent command acknowledgement")
            }
            Self::Timeout => f.write_str("agent command acknowledgement timed out"),
            Self::Protocol(message) => write!(f, "invalid agent command reply: {message}"),
        }
    }
}
impl std::error::Error for AgentCommandsError {}

impl ModuleHandle {
    /// Attribute commands to agents rather than to this module. Start times must
    /// come from `subc_os::process_start_time_ms`. `known` replaces the whole live
    /// set; omitting it only applies starts and finishes. An older daemon returns
    /// [`AgentCommandsError::Unsupported`], which callers may safely ignore.
    pub async fn agent_commands(
        &self,
        started: Vec<AgentCommandRoot>,
        finished: Vec<AgentCommandFinished>,
        known: Option<Vec<AgentCommandRoot>>,
    ) -> Result<(), AgentCommandsError> {
        if !self.shared.supports_agent_commands {
            return Err(AgentCommandsError::Unsupported);
        }
        let body = serde_json::to_vec(&ModuleControlRequestFromModule::AgentCommands {
            started,
            finished,
            known,
        })
        .map_err(|err| AgentCommandsError::Protocol(err.to_string()))?;
        let (corr, writer, rx) = {
            let mut inner = self.shared.lock_inner();
            if inner.closed {
                return Err(AgentCommandsError::ConnectionClosed);
            }
            let writer = inner
                .writer
                .clone()
                .ok_or(AgentCommandsError::ConnectionClosed)?;
            let corr = crate::next_module_control_corr(&mut inner)
                .ok_or(AgentCommandsError::ConnectionClosed)?;
            let (tx, rx) = oneshot::channel();
            inner.pending_agent_commands.insert(corr, tx);
            (corr, writer, rx)
        };
        let _guard = Waiter {
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
        .map_err(|err| AgentCommandsError::Protocol(err.to_string()))?;
        let reply = tokio::time::timeout(crate::CATALOG_UPDATE_TIMEOUT, async {
            writer
                .send(frame)
                .await
                .map_err(|_| AgentCommandsError::ConnectionClosed)?;
            rx.await.map_err(|_| AgentCommandsError::ConnectionClosed)
        })
        .await
        .map_err(|_| AgentCommandsError::Timeout)??;
        match reply.header.ty {
            FrameType::Response => {
                let value: serde_json::Value = serde_json::from_slice(&reply.body)
                    .map_err(|err| AgentCommandsError::Protocol(err.to_string()))?;
                if value.is_object() {
                    Ok(())
                } else {
                    Err(AgentCommandsError::Protocol("expected an object".into()))
                }
            }
            FrameType::Error => {
                let body: ErrorBody = serde_json::from_slice(&reply.body)
                    .map_err(|err| AgentCommandsError::Protocol(err.to_string()))?;
                Err(match body.code.as_str() {
                    "unsupported_control_frame" | "unknown_control_op" => {
                        AgentCommandsError::Unsupported
                    }
                    "not_supervised" | "not_registered" => AgentCommandsError::NotSupervised,
                    _ => AgentCommandsError::Refused(Box::new(body)),
                })
            }
            other => Err(AgentCommandsError::Protocol(format!(
                "unexpected terminal frame: {other:?}"
            ))),
        }
    }
}

struct Waiter {
    handle: ModuleHandle,
    corr: u64,
}
impl Drop for Waiter {
    fn drop(&mut self) {
        self.handle
            .shared
            .lock_inner()
            .pending_agent_commands
            .remove(&self.corr);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declaration_retry_mapping_uses_the_shared_protocol_classes() {
        for (code, retry) in [
            ("agent_roots_full", true),
            ("busy", true),
            ("duplicate_correlation", true),
            ("declaration_closed", true),
            ("invalid_request", false),
            ("future_code", false),
        ] {
            let error = AgentCommandsError::Refused(Box::new(ErrorBody::new(code, "refused")));
            assert_eq!(error.is_retryable(), retry, "{code}");
        }
        assert!(!AgentCommandsError::NotSupervised.is_retryable());
        assert!(!AgentCommandsError::Unsupported.is_retryable());
        assert!(AgentCommandsError::Timeout.is_retryable());
        assert!(AgentCommandsError::ConnectionClosed.is_retryable());
    }
    use subc_protocol::{ModuleHelloAckBody, PROTOCOL_VERSION};
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    fn handle(supported: bool) -> (ModuleHandle, mpsc::Receiver<Frame>) {
        let (writer, receiver) = mpsc::channel(4);
        let ack = ModuleHelloAckBody {
            negotiated_ver: PROTOCOL_VERSION,
            subc_ops: if supported {
                vec!["supervisor.agent_commands".into()]
            } else {
                Vec::new()
            },
            subc_capabilities: Vec::new(),
            storage: None,
            machine_id: None,
        };
        (
            ModuleHandle::new(
                &ack,
                writer,
                1,
                CancellationToken::new(),
                tokio::runtime::Handle::current(),
            ),
            receiver,
        )
    }

    #[tokio::test]
    async fn agent_commands_unsupported_and_ack_keep_connection_open() {
        let (old, mut wire) = handle(false);
        assert!(matches!(
            old.agent_commands(Vec::new(), Vec::new(), None).await,
            Err(AgentCommandsError::Unsupported)
        ));
        assert!(wire.try_recv().is_err());
        assert!(!old.is_closed());
        let (handle, mut wire) = handle(true);
        for code in [
            Some("unsupported_control_frame"),
            Some("unknown_control_op"),
            Some("not_supervised"),
            Some("agent_roots_full"),
            Some("declaration_closed"),
            Some("duplicate_correlation"),
            Some("busy"),
            None,
        ] {
            let caller = handle.clone();
            let root = AgentCommandRoot {
                pid: 42,
                start_time_ms: 1_700_000_000_000,
            };
            let call = tokio::spawn(async move {
                caller
                    .agent_commands(vec![root], Vec::new(), Some(vec![root]))
                    .await
            });
            let request = wire.recv().await.unwrap();
            assert_eq!(request.header.channel, 0);
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            assert_eq!(body["op"], "supervisor.agent_commands");
            assert_eq!(body["started"][0]["start_time_ms"], root.start_time_ms);
            assert_eq!(body["known"][0]["pid"], root.pid);
            let (ty, body) = match code {
                Some(code) => (
                    FrameType::Error,
                    serde_json::json!({"code":code,"message":"refused"}),
                ),
                None => (FrameType::Response, serde_json::json!({})),
            };
            let reply = Frame::build(
                ty,
                crate::control_flags(),
                0,
                0,
                request.header.corr,
                serde_json::to_vec(&body).unwrap(),
            )
            .unwrap();
            assert!(handle.handle_control_reply(reply));
            let result = call.await.unwrap();
            match code {
                Some("not_supervised") => {
                    assert!(matches!(result, Err(AgentCommandsError::NotSupervised)))
                }
                Some(
                    "agent_roots_full" | "declaration_closed" | "duplicate_correlation" | "busy",
                ) => {
                    assert!(result.unwrap_err().is_retryable());
                }
                Some(_) => assert!(matches!(result, Err(AgentCommandsError::Unsupported))),
                None => assert!(result.is_ok()),
            }
            assert!(
                !handle.is_closed(),
                "a refused declaration must not tear down the serving connection"
            );
            assert!(handle.shared.lock_inner().pending_agent_commands.is_empty());
        }
    }
}
