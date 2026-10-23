//! Typed channel-0 calls for the supervisor's read and lifecycle operations:
//! cached health, a fresh health probe, provenance, routes, retained stderr
//! and exits, the server description, restart, stop and start, and the
//! checked rescan that previews or applies a config.
//!
//! Each attempt sends one `ClientControlRequest` and decodes the matching
//! `ClientControlResponse` variant. A daemon refusal comes back as
//! [`CallError::Module`], so its code is readable through [`CallError::code`].
//!
//! Reads are retried on transport failures until the consumer's call deadline,
//! like [`SubcConsumer::supervisor_list`]: sending a read twice changes
//! nothing. Restart and enable/disable are sent once and never retried, because
//! a request whose outcome is unknown may already have restarted or stopped the
//! module, and repeating it would do so a second time.

use subc_control::{
    CapabilityRequirementStatus, ClientControlRequest, ClientControlResponse, RescanExpect,
    SupervisorDaemonProvenance, SupervisorHealthEntry, SupervisorModuleProvenance,
    SupervisorRescanResult, SupervisorRouteModule,
};
use tokio::time::Instant;

use super::{is_retryable_catalog_transport_error, CallError, SubcConsumer, TerminalFrame};

/// The stored health records `supervisor.health` answers with.
#[derive(Debug, Clone, PartialEq)]
pub struct SupervisorHealthResponse {
    pub generation: u64,
    pub modules: Vec<SupervisorHealthEntry>,
}

/// Daemon and module provenance from `supervisor.provenance`.
#[derive(Debug, Clone, PartialEq)]
pub struct SupervisorProvenanceResponse {
    pub daemon: SupervisorDaemonProvenance,
    pub modules: Vec<SupervisorModuleProvenance>,
}

/// The daemon's description of itself from `server.describe`.
#[derive(Debug, Clone, PartialEq)]
pub struct ServerDescription {
    pub protocol_ver: u8,
    pub subc_ops: Vec<String>,
    pub capabilities: Vec<String>,
    pub connected_clients: u64,
    pub counters: Option<serde_json::Value>,
    pub build_git_sha: Option<String>,
    pub build_lock_digest: Option<String>,
    pub capability_requirements: Vec<CapabilityRequirementStatus>,
    /// Absent from daemons that predate the field.
    pub machine_id: Option<String>,
    /// The configuration file the daemon reads, as the daemon names it (never
    /// canonicalized). Absent when the daemon predates the field, was started
    /// without a reloadable config file, or reads a relative path.
    pub config_path: Option<std::path::PathBuf>,
}

/// A module's retained stderr from `supervisor.stderr_tail`.
#[derive(Debug, Clone, PartialEq)]
pub struct StderrTailResponse {
    pub module_id: String,
    pub tail: subc_control::StderrTail,
}

/// One module's fresh health probe from `supervisor.health_probe`: the
/// module's whole metrics object, which the cached `supervisor.health` view
/// may have replaced with a size marker.
#[derive(Debug, Clone, PartialEq)]
pub struct HealthProbeResponse {
    pub module_id: String,
    pub status: subc_protocol::session::HealthStatus,
    pub detail: Option<String>,
    pub metrics: Option<serde_json::Value>,
}

/// The daemon's acknowledgement of a lifecycle request: whether it changed
/// anything. `applied: false` means the module was already in the requested
/// state, not that the request failed (a failure is an error).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupervisorAck {
    pub module_id: String,
    pub applied: bool,
}

impl SubcConsumer {
    /// Send one control request and return the daemon's decoded reply.
    /// `retry` repeats transport failures until the call deadline; only reads
    /// pass it.
    async fn control_request(
        &self,
        request: &ClientControlRequest,
        op: &str,
        retry: bool,
    ) -> Result<ClientControlResponse, CallError> {
        let deadline = Instant::now() + self.shared.opts.call_timeout;
        let body = serde_json::to_vec(request)
            .map_err(|err| CallError::not_sent(format!("failed to encode {op}: {err}")))?;
        let mut attempt = 0usize;
        loop {
            attempt = attempt.saturating_add(1);
            match self
                .shared
                .control_call(body.clone(), deadline, false, None)
                .await
            {
                Ok(TerminalFrame::Response { body, .. }) => {
                    return serde_json::from_slice::<ClientControlResponse>(&body).map_err(|err| {
                        CallError::not_sent(format!("failed to decode {op} response: {err}"))
                    });
                }
                Ok(TerminalFrame::Error { body, .. }) => return Err(CallError::Module(body)),
                Ok(TerminalFrame::StreamEnd) => {
                    return Err(CallError::not_sent(format!("{op} returned StreamEnd")));
                }
                Err(err)
                    if retry
                        && is_retryable_catalog_transport_error(&err)
                        && Instant::now() < deadline =>
                {
                    let delay = self.shared.jittered(
                        self.shared
                            .opts
                            .reconnect_backoff
                            .delay_after_attempt(attempt),
                    );
                    self.shared.sleep_until_retry(deadline, delay).await?;
                }
                Err(err) => return Err(err),
            }
        }
    }

    /// The supervisor's stored health record for every module
    /// (`supervisor.health`). Answered from the daemon's cache; nothing is
    /// probed.
    pub async fn supervisor_health(&self) -> Result<SupervisorHealthResponse, CallError> {
        const OP: &str = "supervisor.health";
        match self
            .control_request(&ClientControlRequest::SupervisorHealth {}, OP, true)
            .await?
        {
            ClientControlResponse::SupervisorHealth {
                generation,
                modules,
            } => Ok(SupervisorHealthResponse {
                generation,
                modules,
            }),
            _ => Err(unexpected(OP)),
        }
    }

    /// Daemon and module provenance (`supervisor.provenance`), for every
    /// module or only `module_id`.
    pub async fn supervisor_provenance(
        &self,
        module_id: Option<String>,
    ) -> Result<SupervisorProvenanceResponse, CallError> {
        const OP: &str = "supervisor.provenance";
        match self
            .control_request(
                &ClientControlRequest::SupervisorProvenance { module_id },
                OP,
                true,
            )
            .await?
        {
            ClientControlResponse::SupervisorProvenance { daemon, modules } => {
                Ok(SupervisorProvenanceResponse { daemon, modules })
            }
            _ => Err(unexpected(OP)),
        }
    }

    /// The live routes each module serves (`supervisor.routes`): every
    /// module's when `module_id` is `None`, otherwise only that module's.
    pub async fn supervisor_routes(
        &self,
        module_id: Option<String>,
    ) -> Result<Vec<SupervisorRouteModule>, CallError> {
        const OP: &str = "supervisor.routes";
        match self
            .control_request(
                &ClientControlRequest::SupervisorRoutes { module_id },
                OP,
                true,
            )
            .await?
        {
            ClientControlResponse::SupervisorRoutes { modules } => Ok(modules),
            _ => Err(unexpected(OP)),
        }
    }

    /// The module's retained stderr (`supervisor.stderr_tail`), oldest entry
    /// first, capped by `max_lines` and `max_bytes` when given.
    pub async fn supervisor_stderr_tail(
        &self,
        module_id: impl Into<String>,
        max_lines: Option<u32>,
        max_bytes: Option<u32>,
    ) -> Result<StderrTailResponse, CallError> {
        const OP: &str = "supervisor.stderr_tail";
        match self
            .control_request(
                &ClientControlRequest::SupervisorStderrTail {
                    module_id: module_id.into(),
                    max_lines,
                    max_bytes,
                },
                OP,
                true,
            )
            .await?
        {
            ClientControlResponse::SupervisorStderrTail { module_id, tail } => {
                Ok(StderrTailResponse { module_id, tail })
            }
            _ => Err(unexpected(OP)),
        }
    }

    /// Asks one module for its health now (`supervisor.health_probe`), the
    /// probe `ck health <id>` sends. Unlike the cached view, the metrics are
    /// complete.
    pub async fn supervisor_health_probe(
        &self,
        module_id: impl Into<String>,
    ) -> Result<HealthProbeResponse, CallError> {
        const OP: &str = "supervisor.health_probe";
        match self
            .control_request(
                &ClientControlRequest::SupervisorHealthProbe {
                    module_id: module_id.into(),
                },
                OP,
                true,
            )
            .await?
        {
            ClientControlResponse::SupervisorHealthProbe {
                module_id,
                status,
                detail,
                metrics,
            } => Ok(HealthProbeResponse {
                module_id,
                status,
                detail,
                metrics,
            }),
            _ => Err(unexpected(OP)),
        }
    }

    /// The module's retained process exits (`supervisor.terminals`), with
    /// what the supervisor did after each.
    pub async fn supervisor_terminals(
        &self,
        module_id: impl Into<String>,
    ) -> Result<subc_control::TerminalHistory, CallError> {
        const OP: &str = "supervisor.terminals";
        match self
            .control_request(
                &ClientControlRequest::SupervisorTerminals {
                    module_id: module_id.into(),
                },
                OP,
                true,
            )
            .await?
        {
            ClientControlResponse::SupervisorTerminals { terminals, .. } => Ok(terminals),
            _ => Err(unexpected(OP)),
        }
    }

    /// The daemon's description of itself (`server.describe`).
    pub async fn server_describe(&self) -> Result<ServerDescription, CallError> {
        const OP: &str = "server.describe";
        match self
            .control_request(&ClientControlRequest::ServerDescribe {}, OP, true)
            .await?
        {
            ClientControlResponse::ServerDescribe {
                protocol_ver,
                subc_ops,
                capabilities,
                connected_clients,
                counters,
                build_git_sha,
                build_lock_digest,
                capability_requirements,
                machine_id,
                config_path,
            } => Ok(ServerDescription {
                protocol_ver,
                subc_ops,
                capabilities,
                connected_clients,
                counters,
                build_git_sha,
                build_lock_digest,
                capability_requirements,
                machine_id,
                config_path,
            }),
            _ => Err(unexpected(OP)),
        }
    }

    /// Restart one module (`supervisor.restart`), the request `ck module
    /// restart` sends. `drain_timeout_ms` overrides the module's drain budget
    /// for this restart; `None` keeps the configured one. Sent once, never
    /// retried.
    pub async fn supervisor_restart(
        &self,
        module_id: impl Into<String>,
        drain_timeout_ms: Option<u64>,
    ) -> Result<SupervisorAck, CallError> {
        let request = ClientControlRequest::SupervisorRestart {
            module_id: module_id.into(),
            drain_timeout_ms,
        };
        self.lifecycle(&request, "supervisor.restart").await
    }

    /// Sends `supervisor.set_enabled` for one module: `enabled: true` starts
    /// it and `false` stops it, the same request `ck module start` and
    /// `ck module stop` send. This method sends the request once and never
    /// retries it.
    pub async fn supervisor_set_enabled(
        &self,
        module_id: impl Into<String>,
        enabled: bool,
    ) -> Result<SupervisorAck, CallError> {
        let request = ClientControlRequest::SupervisorSetEnabled {
            module_id: module_id.into(),
            enabled,
        };
        self.lifecycle(&request, "supervisor.set_enabled").await
    }

    /// Sends `supervisor.rescan_checked` and returns the daemon's rescan
    /// result. With `preview: true` and the full candidate config text, the
    /// daemon previews that text and writes nothing; the result carries what
    /// each module would do and the `expect` value an apply must send. With
    /// `preview: false` and that `expect`, the daemon applies its config file
    /// only if the file and its staged state still match the preview. Any
    /// other combination is refused by the daemon. A refusal, such as
    /// `invalid_daemon_config` for a candidate the daemon can't load, comes
    /// back as [`CallError::Module`].
    ///
    /// Send this only to a daemon whose `server.describe` lists
    /// `rescan-candidate-preview/v1`. A preview is retried on transport
    /// failures like the other reads; an apply is sent once and never retried,
    /// because one whose outcome is unknown may already have applied.
    pub async fn supervisor_rescan_checked(
        &self,
        preview: bool,
        candidate: Option<String>,
        expect: Option<RescanExpect>,
    ) -> Result<SupervisorRescanResult, CallError> {
        const OP: &str = "supervisor.rescan_checked";
        let request = ClientControlRequest::SupervisorRescanChecked {
            preview,
            candidate,
            expect,
        };
        match self.control_request(&request, OP, preview).await? {
            ClientControlResponse::SupervisorRescan { result } => Ok(result),
            _ => Err(unexpected(OP)),
        }
    }

    async fn lifecycle(
        &self,
        request: &ClientControlRequest,
        op: &str,
    ) -> Result<SupervisorAck, CallError> {
        match self.control_request(request, op, false).await? {
            ClientControlResponse::SupervisorAck { module_id, applied } => {
                Ok(SupervisorAck { module_id, applied })
            }
            _ => Err(unexpected(op)),
        }
    }
}

fn unexpected(op: &str) -> CallError {
    CallError::not_sent(format!("{op} returned an unexpected control response"))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use subc_control::{SupervisorHealthStatus, SupervisorRouteModule};
    use subc_protocol::{ErrorBody, Flags, Frame, FrameType, Priority};
    use tokio::sync::mpsc;

    use super::super::{dispatch_frame, ConsumerOptions, Shared};
    use super::*;

    fn consumer() -> (
        SubcConsumer,
        Arc<Shared>,
        mpsc::Receiver<super::super::WriteCommand>,
    ) {
        let shared = Arc::new(Shared::new(
            PathBuf::from("/tmp/does-not-exist"),
            ConsumerOptions::default(),
        ));
        let (writer, rx) = mpsc::channel(1);
        shared.lock_inner().writer = Some(writer);
        (
            SubcConsumer {
                shared: Arc::clone(&shared),
            },
            shared,
            rx,
        )
    }

    fn frame(kind: FrameType, corr: u64, body: Vec<u8>) -> Frame {
        Frame::build(
            kind,
            Flags::new(false, Priority::Interactive, false),
            0,
            0,
            corr,
            body,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn health_sends_its_op_and_returns_the_entries() {
        let (consumer, shared, mut rx) = consumer();
        let call = tokio::spawn(async move { consumer.supervisor_health().await });
        let command = rx.recv().await.expect("a channel-0 request");
        assert_eq!(command.frame.header.channel, 0);
        let body: serde_json::Value = serde_json::from_slice(&command.frame.body).unwrap();
        assert_eq!(body["op"], "supervisor.health");
        let reply = serde_json::json!({
            "op": "supervisor.health",
            "generation": 7,
            "modules": [{
                "module_id": "broca",
                "status": "degraded",
                "detail": "provider throttled",
                "metrics": {"headline": ["runs"], "runs": 3},
                "consecutive_failures": 0
            }]
        });
        let corr = command.frame.header.corr;
        let reply = serde_json::to_vec(&reply).unwrap();
        assert!(dispatch_frame(&shared, 1, frame(FrameType::Response, corr, reply)).await);
        let health = call.await.unwrap().unwrap();
        assert_eq!(health.generation, 7);
        assert_eq!(health.modules[0].status, SupervisorHealthStatus::Degraded);
        assert_eq!(
            health.modules[0].detail.as_deref(),
            Some("provider throttled")
        );
    }

    #[tokio::test]
    async fn routes_and_describe_decode_their_replies() {
        let (consumer, shared, mut rx) = consumer();
        let consumer = Arc::new(consumer);
        let c = Arc::clone(&consumer);
        let call = tokio::spawn(async move { c.supervisor_routes(Some("broca".into())).await });
        let command = rx.recv().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&command.frame.body).unwrap();
        assert_eq!(body["op"], "supervisor.routes");
        assert_eq!(body["module_id"], "broca");
        let reply = serde_json::to_vec(&ClientControlResponse::SupervisorRoutes {
            modules: vec![SupervisorRouteModule {
                module_id: "broca".into(),
                routes: vec![],
            }],
        })
        .unwrap();
        let corr = command.frame.header.corr;
        assert!(dispatch_frame(&shared, 1, frame(FrameType::Response, corr, reply)).await);
        assert_eq!(call.await.unwrap().unwrap()[0].module_id, "broca");

        let c = Arc::clone(&consumer);
        let call = tokio::spawn(async move { c.server_describe().await });
        let command = rx.recv().await.unwrap();
        let reply = serde_json::json!({
            "op": "server.describe",
            "protocol_ver": 1,
            "subc_ops": [],
            "capabilities": [],
            "connected_clients": 12,
            "machine_id": "4f1a9c2e7b3d40c8"
        });
        let corr = command.frame.header.corr;
        let reply = serde_json::to_vec(&reply).unwrap();
        assert!(dispatch_frame(&shared, 1, frame(FrameType::Response, corr, reply)).await);
        let described = call.await.unwrap().unwrap();
        assert_eq!(described.connected_clients, 12);
        assert_eq!(described.machine_id.as_deref(), Some("4f1a9c2e7b3d40c8"));
        assert_eq!(described.config_path, None, "an older daemon sends no path");

        let c = Arc::clone(&consumer);
        let call = tokio::spawn(async move { c.server_describe().await });
        let command = rx.recv().await.unwrap();
        let reply = serde_json::json!({
            "op": "server.describe",
            "protocol_ver": 1,
            "subc_ops": [],
            "capabilities": ["rescan-candidate-preview/v1"],
            "connected_clients": 1,
            "config_path": "/Users/alex/.config/cortexkit/subc.jsonc"
        });
        let corr = command.frame.header.corr;
        let reply = serde_json::to_vec(&reply).unwrap();
        assert!(dispatch_frame(&shared, 1, frame(FrameType::Response, corr, reply)).await);
        let described = call.await.unwrap().unwrap();
        assert_eq!(
            described.config_path.as_deref(),
            Some(std::path::Path::new(
                "/Users/alex/.config/cortexkit/subc.jsonc"
            ))
        );
    }

    #[tokio::test]
    async fn restart_returns_the_ack_and_a_refusal_keeps_its_code() {
        let (consumer, shared, mut rx) = consumer();
        let consumer = Arc::new(consumer);
        let c = Arc::clone(&consumer);
        let call = tokio::spawn(async move { c.supervisor_restart("broca", None).await });
        let command = rx.recv().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&command.frame.body).unwrap();
        assert_eq!(body["op"], "supervisor.restart");
        assert_eq!(body["module_id"], "broca");
        assert!(body.get("drain_timeout_ms").is_none());
        let reply = serde_json::to_vec(&ClientControlResponse::SupervisorAck {
            module_id: "broca".into(),
            applied: true,
        })
        .unwrap();
        let corr = command.frame.header.corr;
        assert!(dispatch_frame(&shared, 1, frame(FrameType::Response, corr, reply)).await);
        assert_eq!(
            call.await.unwrap().unwrap(),
            SupervisorAck {
                module_id: "broca".into(),
                applied: true
            }
        );

        let c = Arc::clone(&consumer);
        let call = tokio::spawn(async move { c.supervisor_set_enabled("broca", false).await });
        let command = rx.recv().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&command.frame.body).unwrap();
        assert_eq!(body["op"], "supervisor.set_enabled");
        assert_eq!(body["enabled"], false);
        let refusal = serde_json::to_vec(&ErrorBody {
            code: "module_reserved".into(),
            message: "refused by the test".into(),
            detail: None,
        })
        .unwrap();
        let corr = command.frame.header.corr;
        assert!(dispatch_frame(&shared, 1, frame(FrameType::Error, corr, refusal)).await);
        assert_eq!(
            call.await.unwrap().unwrap_err().code(),
            Some("module_reserved")
        );
    }

    #[tokio::test]
    async fn health_probe_and_terminals_decode_their_replies() {
        let (consumer, shared, mut rx) = consumer();
        let consumer = Arc::new(consumer);
        let c = Arc::clone(&consumer);
        let call = tokio::spawn(async move { c.supervisor_health_probe("aft").await });
        let command = rx.recv().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&command.frame.body).unwrap();
        assert_eq!(body["op"], "supervisor.health_probe");
        assert_eq!(body["module_id"], "aft");
        let reply = serde_json::json!({
            "op": "supervisor.health_probe", "module_id": "aft", "status": "ok",
            "metrics": {"headline": ["files"], "files": 12}
        });
        let corr = command.frame.header.corr;
        let reply = serde_json::to_vec(&reply).unwrap();
        assert!(dispatch_frame(&shared, 1, frame(FrameType::Response, corr, reply)).await);
        let probe = call.await.unwrap().unwrap();
        assert_eq!(probe.metrics.unwrap()["files"], 12);

        let c = Arc::clone(&consumer);
        let call = tokio::spawn(async move { c.supervisor_terminals("aft").await });
        let command = rx.recv().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&command.frame.body).unwrap();
        assert_eq!(body["op"], "supervisor.terminals");
        let reply = serde_json::json!({
            "op": "supervisor.terminals", "module_id": "aft",
            "daemon_started_at_ms": 1000,
            "entries": [{"exit_code": 0, "at_ms": 2000, "disposition": "restarting"}]
        });
        let corr = command.frame.header.corr;
        let reply = serde_json::to_vec(&reply).unwrap();
        assert!(dispatch_frame(&shared, 1, frame(FrameType::Response, corr, reply)).await);
        let history = call.await.unwrap().unwrap();
        assert_eq!(history.entries.len(), 1);
        assert_eq!(history.entries[0].at_ms, 2000);
    }

    #[tokio::test]
    async fn stderr_tail_carries_its_caps_and_decodes_lines() {
        let (consumer, shared, mut rx) = consumer();
        let call = tokio::spawn(async move {
            consumer
                .supervisor_stderr_tail("broca", Some(200), None)
                .await
        });
        let command = rx.recv().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&command.frame.body).unwrap();
        assert_eq!(body["op"], "supervisor.stderr_tail");
        assert_eq!(body["max_lines"], 200);
        let reply = serde_json::json!({
            "op": "supervisor.stderr_tail",
            "module_id": "broca",
            "capture": {"state": "captured"},
            "entries": [
                {"kind": "line", "text": "hello", "truncated": false, "at_ms": 1000}
            ]
        });
        let corr = command.frame.header.corr;
        let reply = serde_json::to_vec(&reply).unwrap();
        assert!(dispatch_frame(&shared, 1, frame(FrameType::Response, corr, reply)).await);
        let tail = call.await.unwrap().unwrap();
        assert_eq!(tail.module_id, "broca");
        assert_eq!(tail.tail.entries.len(), 1);
    }

    fn expect() -> RescanExpect {
        RescanExpect {
            config_sha256: "ab".repeat(32),
            staged_generation: subc_control::StagedGeneration {
                incarnation: "incarnation-1".into(),
                counter: 4,
            },
        }
    }

    /// The request on the wire is exactly subc-control's
    /// `SupervisorRescanChecked` variant with the arguments given, for both
    /// the preview and the apply form.
    #[tokio::test]
    async fn rescan_checked_sends_the_checked_variant_with_its_arguments() {
        let (consumer, shared, mut rx) = consumer();
        let consumer = Arc::new(consumer);
        for (preview, candidate, expect) in [
            (true, Some("{\"modules\": {}}".to_string()), None),
            (false, None, Some(expect())),
        ] {
            let c = Arc::clone(&consumer);
            let (sent_candidate, sent_expect) = (candidate.clone(), expect.clone());
            let call = tokio::spawn(async move {
                c.supervisor_rescan_checked(preview, sent_candidate, sent_expect)
                    .await
            });
            let command = rx.recv().await.unwrap();
            assert_eq!(command.frame.header.channel, 0);
            let sent: ClientControlRequest = serde_json::from_slice(&command.frame.body).unwrap();
            assert_eq!(
                sent,
                ClientControlRequest::SupervisorRescanChecked {
                    preview,
                    candidate,
                    expect,
                }
            );
            let body: serde_json::Value = serde_json::from_slice(&command.frame.body).unwrap();
            assert_eq!(body["op"], "supervisor.rescan_checked");
            let reply = serde_json::to_vec(&serde_json::json!({
                "op": "supervisor.rescan",
                "added": [], "removed": [], "changed_pending_reload": [], "unchanged": 0
            }))
            .unwrap();
            let corr = command.frame.header.corr;
            assert!(dispatch_frame(&shared, 1, frame(FrameType::Response, corr, reply)).await);
            call.await.unwrap().unwrap();
        }
    }

    /// Drops the waiting caller's reply channel for `command`, as a transport
    /// failure before any reply does: the caller sees a retryable `NotSent`.
    fn lose_reply(shared: &Shared, command: &super::super::WriteCommand) {
        let key = command.pending.expect("a request waits for its reply");
        assert!(shared.lock_inner().pending.remove(&key).is_some());
    }

    /// A preview changes nothing, so a transport failure is retried with a
    /// second request. An apply whose outcome is unknown may already have
    /// applied, so it is sent once and the failure is returned.
    #[tokio::test]
    async fn rescan_checked_retries_a_preview_but_sends_an_apply_once() {
        let (consumer, shared, mut rx) = consumer();
        let consumer = Arc::new(consumer);
        let c = Arc::clone(&consumer);
        let call = tokio::spawn(async move {
            c.supervisor_rescan_checked(true, Some("{}".into()), None)
                .await
        });
        let first = rx.recv().await.unwrap();
        lose_reply(&shared, &first);
        let second = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("a preview is sent again after a transport failure")
            .unwrap();
        assert_ne!(second.frame.header.corr, first.frame.header.corr);
        assert_eq!(second.frame.body, first.frame.body);
        let reply = serde_json::to_vec(&serde_json::json!({
            "op": "supervisor.rescan",
            "added": [], "removed": [], "changed_pending_reload": [], "unchanged": 0,
            "preview": true
        }))
        .unwrap();
        let corr = second.frame.header.corr;
        assert!(dispatch_frame(&shared, 1, frame(FrameType::Response, corr, reply)).await);
        assert!(call.await.unwrap().unwrap().preview);

        let c = Arc::clone(&consumer);
        let call = tokio::spawn(async move {
            c.supervisor_rescan_checked(false, None, Some(expect()))
                .await
        });
        let only = rx.recv().await.unwrap();
        lose_reply(&shared, &only);
        let err = call.await.unwrap().unwrap_err();
        assert!(matches!(err, CallError::NotSent(_)), "{err:?}");
        assert!(
            rx.try_recv().is_err(),
            "an apply is never sent a second time"
        );
    }

    /// A preview reply decodes with its module actions, earlier-pending list,
    /// diagnostics and `expect`; a refusal keeps its code and message.
    #[tokio::test]
    async fn rescan_checked_decodes_the_preview_and_keeps_a_refusal_typed() {
        let (consumer, shared, mut rx) = consumer();
        let consumer = Arc::new(consumer);
        let c = Arc::clone(&consumer);
        let call = tokio::spawn(async move {
            c.supervisor_rescan_checked(true, Some("{}".into()), None)
                .await
        });
        let command = rx.recv().await.unwrap();
        let reply = serde_json::json!({
            "op": "supervisor.rescan",
            "added": [], "removed": [], "changed_pending_reload": ["broca"],
            "unchanged": 3, "preview": true,
            "restart_required": ["storage"],
            "capability_warnings": ["memory.read has no provider"],
            "module_actions": [{
                "module_id": "broca", "action": "restart", "changed": ["environment"],
                "drain_timeout_ms": 30000,
                "busy_gauges": [{"gauge": "runs_in_flight", "value": 3}]
            }],
            "earlier_pending": ["wernicke"],
            "expect": serde_json::to_value(expect()).unwrap()
        });
        let corr = command.frame.header.corr;
        let reply = serde_json::to_vec(&reply).unwrap();
        assert!(dispatch_frame(&shared, 1, frame(FrameType::Response, corr, reply)).await);
        let result = call.await.unwrap().unwrap();
        assert!(result.preview);
        assert_eq!(result.restart_required, vec!["storage"]);
        assert_eq!(
            result.capability_warnings,
            vec!["memory.read has no provider"]
        );
        assert_eq!(result.earlier_pending, vec!["wernicke"]);
        assert_eq!(result.expect, Some(expect()));
        let action = &result.module_actions[0];
        assert_eq!(action.module_id, "broca");
        assert_eq!(action.action, subc_control::RescanModuleActionKind::Restart);
        assert_eq!(action.changed, vec!["environment"]);
        assert_eq!(action.drain_timeout_ms, Some(30000));
        assert_eq!(action.busy_gauges[0].gauge, "runs_in_flight");
        assert_eq!(action.busy_gauges[0].value, Some(serde_json::json!(3)));

        let c = Arc::clone(&consumer);
        let call = tokio::spawn(async move {
            c.supervisor_rescan_checked(true, Some("{".into()), None)
                .await
        });
        let command = rx.recv().await.unwrap();
        let refusal = serde_json::to_vec(&ErrorBody {
            code: "invalid_daemon_config".into(),
            message: "supervisor rescan rejected daemon config: unexpected end".into(),
            detail: None,
        })
        .unwrap();
        let corr = command.frame.header.corr;
        assert!(dispatch_frame(&shared, 1, frame(FrameType::Error, corr, refusal)).await);
        match call.await.unwrap().unwrap_err() {
            CallError::Module(body) => {
                assert_eq!(body.code, "invalid_daemon_config");
                assert_eq!(
                    body.message,
                    "supervisor rescan rejected daemon config: unexpected end"
                );
            }
            other => panic!("expected a typed refusal, got {other:?}"),
        }
    }
}
