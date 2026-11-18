use super::*;
use crate::{
    forwarding::{ForwardingTable, ModuleControlRpcOutcome},
    registry::Registry,
};
use subc_protocol::{Frame, FrameType};
use tokio::time::{timeout_at, Duration, Instant};

/// Channel-0 access uses the same correlation and cancellation machinery as
/// config.review, but requires the reserved registry principal, not a reviewer.
pub struct EntorhinalProjectRegistry {
    registry: Arc<Registry>,
    forwarding: Arc<ForwardingTable>,
    root_io: std::io::Result<RootIoPool>,
}

impl EntorhinalProjectRegistry {
    pub(crate) fn new(registry: Arc<Registry>, forwarding: Arc<ForwardingTable>) -> Self {
        Self {
            registry,
            forwarding,
            root_io: RootIoPool::new(),
        }
    }

    fn target(&self) -> Result<crate::registry::ModuleRegistration> {
        let (_, modules) = self
            .registry
            .list_modules_with_nonce_verification()
            .map_err(|_| unavailable("module registry is unavailable"))?;
        let (registration, verified) = modules
            .into_iter()
            .find(|(module, _)| module.manifest.module_id == "entorhinal")
            .ok_or_else(|| unavailable("entorhinal is not registered"))?;
        if !verified {
            return Err(unavailable(
                "entorhinal is not nonce-verified under its reserved principal",
            ));
        }
        if !registration.ready {
            return Err(unavailable("entorhinal is not ready"));
        }
        if !registration.control_ops.iter().any(|op| op == ENUMERATE_OP) {
            return Err(unavailable(
                "entorhinal has no projects.enumerate control op",
            ));
        }
        Ok(registration)
    }

    async fn enumerate(&self) -> Result<EnumerateReply> {
        let registration = self.target()?;
        let deadline = Instant::now() + Duration::from_secs(10);
        let pending = self
            .forwarding
            .begin_module_control_rpc_for("entorhinal", ENUMERATE_OP, deadline)
            .map_err(|_| unavailable("entorhinal is unreachable"))?;
        let mut guard = RpcGuard {
            forwarding: self.forwarding.clone(),
            endpoint: pending.endpoint,
            corr: pending.corr,
            armed: true,
        };
        if pending.endpoint.connection_id != registration.connection_id
            || self.target()?.connection_id != registration.connection_id
        {
            return Err(unavailable("entorhinal changed its registered connection"));
        }
        let frame = Frame::build_with_version(
            pending.negotiated_ver,
            FrameType::Request,
            crate::control::control_flags(),
            0,
            0,
            pending.corr,
            serde_json::to_vec(&serde_json::json!({"op": ENUMERATE_OP}))
                .map_err(|_| unavailable("cannot encode projects.enumerate"))?,
        )
        .map_err(|_| unavailable("cannot frame projects.enumerate"))?;
        timeout_at(deadline, pending.module_sink.send(frame))
            .await
            .map_err(|_| unavailable("entorhinal projects.enumerate timed out"))?
            .map_err(|_| unavailable("entorhinal disconnected"))?;
        let outcome = timeout_at(deadline, pending.receiver)
            .await
            .map_err(|_| unavailable("entorhinal projects.enumerate timed out"))?
            .map_err(|_| unavailable("entorhinal disconnected"))?;
        guard.armed = false;
        if self.target()?.connection_id != registration.connection_id {
            return Err(unavailable("entorhinal changed its registered connection"));
        }
        let bytes = match outcome {
            ModuleControlRpcOutcome::RawResponse(bytes) => bytes,
            ModuleControlRpcOutcome::Rejected(error) => {
                return Err(unavailable(format!("entorhinal: {}", error.message)))
            }
            ModuleControlRpcOutcome::DeadlineElapsed => {
                return Err(unavailable("entorhinal projects.enumerate timed out"))
            }
            _ => return Err(unavailable("entorhinal did not answer projects.enumerate")),
        };
        #[derive(Deserialize)]
        struct Reply {
            op: String,
            result: EnumerateReply,
        }
        let reply: Reply = serde_json::from_slice(&bytes)
            .map_err(|_| unavailable("entorhinal returned an invalid projects.enumerate reply"))?;
        if reply.op != ENUMERATE_OP {
            return Err(unavailable("entorhinal returned the wrong control op"));
        }
        Ok(reply.result)
    }
}

impl ProjectRegistry for EntorhinalProjectRegistry {
    fn snapshot<'a>(
        &'a self,
        module: &'a str,
        affected: &'a BTreeSet<String>,
    ) -> RegistryFuture<'a> {
        Box::pin(async move {
            let reply = self.enumerate().await?;
            let pool = self
                .root_io
                .as_ref()
                .map_err(|_| unavailable("registry capture worker stopped"))?;
            RegistryRevision::from_enumerate(pool, &reply, module, affected).await
        })
    }

    fn root_io_health(&self) -> Option<subc_control::config::ConfigRootIo> {
        self.root_io.as_ref().ok().and_then(RootIoPool::health)
    }
}

struct RpcGuard {
    forwarding: Arc<ForwardingTable>,
    endpoint: crate::forwarding::ModuleEndpointId,
    corr: u64,
    armed: bool,
}
impl Drop for RpcGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = self
                .forwarding
                .cancel_module_control_rpc(self.endpoint, self.corr);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{control::ControlHandler, router::RouteCtx, ConnectionId};
    use subc_protocol::manifest::{Concurrency, ModuleManifest};

    fn rig(
        verified: bool,
        ready: bool,
        ops: Vec<String>,
    ) -> (
        EntorhinalProjectRegistry,
        ControlHandler,
        tokio::sync::mpsc::Receiver<crate::router::OutboundFrame>,
        RouteCtx,
    ) {
        let registry = Arc::new(Registry::default());
        let forwarding = Arc::new(ForwardingTable::default());
        let connection = ConnectionId::new(14);
        let mut manifest = ModuleManifest::builder("entorhinal", "1.0.0").build();
        manifest.ready = Some(ready);
        registry
            .register_unpublished_verified_with_control_ops(
                manifest,
                subc_protocol::PROTOCOL_VERSION,
                connection,
                ops,
                verified,
            )
            .unwrap();
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let sink = crate::FrameSink::new(tx);
        forwarding
            .register_module_connection(
                connection,
                "entorhinal".into(),
                subc_protocol::PROTOCOL_VERSION,
                Concurrency::ModuleManaged,
                sink.clone(),
            )
            .unwrap();
        registry.publish_registration(connection).unwrap();
        (
            EntorhinalProjectRegistry::new(registry.clone(), forwarding.clone()),
            ControlHandler::with_forwarding(registry, forwarding),
            rx,
            RouteCtx {
                connection_id: connection,
                egress: sink,
            },
        )
    }

    #[tokio::test]
    async fn registry_rpc_fake_module_serves_enumerate_once_on_channel_zero() {
        let (reader, handler, mut frames, ctx) = rig(true, true, vec![ENUMERATE_OP.into()]);
        let module = tokio::spawn(async move {
            let frame = frames.recv().await.unwrap().frame;
            assert_eq!(frame.header.channel, 0);
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&frame.body).unwrap(),
                serde_json::json!({"op":ENUMERATE_OP})
            );
            let body = serde_json::json!({"op":ENUMERATE_OP,"result":{"projects":[],"workspaces":[],"generation":1,"incarnation":"registry"}});
            let response = Frame::build(
                FrameType::Response,
                crate::control::control_flags(),
                0,
                0,
                frame.header.corr,
                serde_json::to_vec(&body).unwrap(),
            )
            .unwrap();
            assert!(handler
                .handle_control_frame(&ctx, response)
                .await
                .unwrap()
                .is_empty());
            assert!(frames.try_recv().is_err());
        });
        let revision = reader.snapshot("module", &BTreeSet::new()).await.unwrap();
        assert!(revision.projects.is_empty());
        module.await.unwrap();
    }

    #[tokio::test]
    async fn registry_rpc_requires_nonce_ready_and_advertised_op() {
        for (verified, ready, ops, reason) in [
            (false, true, vec![ENUMERATE_OP.into()], "nonce-verified"),
            (true, false, vec![ENUMERATE_OP.into()], "not ready"),
            (true, true, vec![], "no projects.enumerate"),
        ] {
            let (reader, _handler, mut frames, _ctx) = rig(verified, ready, ops);
            let error = reader
                .snapshot("module", &BTreeSet::new())
                .await
                .unwrap_err();
            assert_eq!(error.code, "project_registry_unavailable");
            assert!(error.detail.contains(reason));
            assert!(frames.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn registry_rpc_preserves_store_unavailable_refusal_message() {
        let (reader, handler, mut frames, ctx) = rig(true, true, vec![ENUMERATE_OP.into()]);
        let module = tokio::spawn(async move {
            let frame = frames.recv().await.unwrap().frame;
            let body = subc_protocol::ErrorBody::new("unavailable", "store not open yet");
            let response = Frame::build(
                FrameType::Error,
                crate::control::control_flags(),
                0,
                0,
                frame.header.corr,
                serde_json::to_vec(&body).unwrap(),
            )
            .unwrap();
            handler.handle_control_frame(&ctx, response).await.unwrap();
        });
        let error = reader
            .snapshot("module", &BTreeSet::new())
            .await
            .unwrap_err();
        assert_eq!(error.code, "project_registry_unavailable");
        assert!(error.detail.contains("store not open yet"));
        module.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn registry_rpc_timeout_is_bounded_and_refuses_the_plan() {
        let (reader, _handler, mut frames, _ctx) = rig(true, true, vec![ENUMERATE_OP.into()]);
        let reader = Arc::new(reader);
        let task_reader = reader.clone();
        let task =
            tokio::spawn(async move { task_reader.snapshot("module", &BTreeSet::new()).await });
        let frame = frames.recv().await.unwrap().frame;
        tokio::time::advance(Duration::from_secs(9)).await;
        assert!(!task.is_finished());
        tokio::time::advance(Duration::from_secs(1)).await;
        let error = task.await.unwrap().unwrap_err();
        assert_eq!(error.code, "project_registry_unavailable");
        assert!(error.detail.contains("timed out"));
        assert!(reader
            .forwarding
            .pending_module_control_op(ConnectionId::new(14), frame.header.corr)
            .unwrap()
            .is_none());
    }
}
