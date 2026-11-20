use super::*;
use crate::config_gate::GateError;
use crate::policy_root::{refuse, Key, Move, Section};
use subc_control::config::ConfigControlResponse;
use subc_control::policy_root::{RECOVER_OP, SET_OP};
use subc_protocol::policy_root::{GET_OP, ROTATE_OP};

const VAULT: &str = "claustrum";

pub(super) fn is_op(op: &str) -> bool {
    matches!(op, GET_OP | ROTATE_OP | SET_OP | RECOVER_OP)
}

impl ControlHandler {
    fn policy_root_operator(&self, connection: ConnectionId) -> bool {
        self.claims.binding(connection).is_none()
            && self
                .local_connections
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .contains(&connection)
            && self.config_owner_connection(connection)
            && self
                .registry
                .get_module_by_connection(connection)
                .is_ok_and(|registration| registration.is_none())
            && self.supervisor.registration_spawn(connection).is_none()
    }

    fn policy_root_vault(&self, connection: ConnectionId) -> bool {
        let nonce = self.supervisor.spawn_launch_nonce_for(VAULT);
        let launched = matches!(
            self.connection_is_current_launch(VAULT, connection, nonce.as_deref()),
            crate::scopes::LaunchStanding::Current
        );
        let reserved = if self.supervisor.registration_spawn(connection).is_some() {
            self.rescan.as_ref().is_some_and(|rescan| {
                rescan
                    .module_entries
                    .lock()
                    .is_ok_and(|entries| entries.reserved(VAULT))
            })
        } else {
            nonce.is_some()
                && self
                    .supervisor
                    .reserved_hello_verification(VAULT, nonce.as_deref())
                    .is_ok_and(|proof| proof.nonce_verified && proof.prefix_owner.is_none())
        };
        launched && reserved
    }

    pub(super) async fn handle_policy_root(
        &self,
        ctx: &RouteCtx,
        frame: Frame,
        op: &str,
    ) -> Result<Vec<Frame>, RouterError> {
        let connection = ctx.connection_id;
        let registration = self
            .registry
            .get_module_by_connection(connection)
            .map_err(|err| RouterError::backend(0, frame.header.corr, err.to_string()))?;
        let admitted = match op {
            GET_OP => registration.is_some() || self.policy_root_operator(connection),
            ROTATE_OP => registration
                .as_ref()
                .is_some_and(|module| module.manifest.module_id == VAULT),
            SET_OP | RECOVER_OP => self.policy_root_operator(connection),
            _ => false,
        };
        let result = if admitted {
            self.execute_policy_root(ctx, &frame, op).await
        } else {
            Err(refuse("policy_root_caller_not_permitted"))
        };
        Ok(vec![match result {
            Ok(response) => control_response_body_frame(&frame, &response, "policy root response")?,
            Err(error) => config_gate::config_error_frame(&frame, error)?,
        }])
    }

    async fn execute_policy_root(
        &self,
        ctx: &RouteCtx,
        frame: &Frame,
        op: &str,
    ) -> Result<serde_json::Value, GateError> {
        let malformed = |_| GateError::new("invalid_control_body", "invalid policy root request");
        let encode = |_| {
            GateError::new(
                "config_unavailable",
                "could not encode the policy root response",
            )
        };
        if op == GET_OP {
            let _: ClientControlRequest = serde_json::from_slice(&frame.body).map_err(malformed)?;
            let view = if let Some(owner) = &self.config_runtime {
                owner.on_blocking(|owner| owner.read_policy_root()).await?.0
            } else {
                subc_protocol::policy_root::PolicyRoot {
                    pin: None,
                    generation: 0,
                }
            };
            return serde_json::to_value(ClientControlResponse::PolicyRoot(view)).map_err(encode);
        }
        let owner = self
            .config_runtime
            .as_ref()
            .ok_or_else(|| GateError::new("config_gate_inactive", "no config store configured"))?;
        if op == ROTATE_OP {
            let request = serde_json::from_slice(&frame.body).map_err(malformed)?;
            let ModuleControlRequestFromModule::PolicyRootRotate(request) = request else {
                return Err(GateError::new(
                    "invalid_control_body",
                    "invalid policy root rotation",
                ));
            };
            let operation = self.supervisor.operation_lock();
            let _guard = operation.lock().await;
            if !self.policy_root_vault(ctx.connection_id) {
                return Err(refuse("policy_root_caller_not_permitted"));
            }
            let change = owner
                .on_blocking(move |owner| owner.plan_policy_root(Move::Rotate(request)))
                .await?;
            return serde_json::to_value(ModuleControlResponseToModule::PolicyRootRotate(change))
                .map_err(encode);
        }
        let request = serde_json::from_slice(&frame.body).map_err(malformed)?;
        let response = match request {
            ClientControlRequest::PolicyRootSet(key) => {
                let change = owner
                    .on_blocking(move |owner| {
                        owner.plan_policy_root(Move::Set(Key {
                            key_id: key.key_id,
                            public_key_hex: key.public_key_hex,
                        }))
                    })
                    .await?;
                ClientControlResponse::PolicyRootSet(change)
            }
            ClientControlRequest::PolicyRootRecover(request) => {
                let change = owner
                    .on_blocking(move |owner| owner.plan_policy_root(Move::Recover(request)))
                    .await?;
                ClientControlResponse::PolicyRootRecover(change)
            }
            _ => {
                return Err(GateError::new(
                    "invalid_control_body",
                    "invalid policy root move",
                ))
            }
        };
        serde_json::to_value(response).map_err(encode)
    }

    pub(super) async fn rescan_policy_root(&self, section: Section) {
        if let Some(owner) = &self.config_runtime {
            if let Err(error) = owner
                .on_blocking(move |owner| owner.rescan_policy_root(section))
                .await
            {
                tracing::warn!(%error, "agent policy trust root could not be reconciled");
            }
        }
    }

    pub(super) async fn apply_unstamped_policy_root(
        &self,
        ctx: &RouteCtx,
        frame: &Frame,
        owner: &Arc<crate::config_gate::runtime::RuntimeOwner>,
        digest: &subc_config::Digest,
    ) -> Result<Option<ConfigControlResponse>, GateError> {
        let query = digest.clone();
        let Some(review) = owner
            .on_blocking(move |owner| owner.unstamped_policy_root_review(&query))
            .await?
        else {
            return Ok(None);
        };
        if !self.policy_root_operator(ctx.connection_id) {
            return Err(refuse("policy_root_caller_not_permitted"));
        }
        if !self.forwarding.operator_confirms().config_gate_ready() {
            return Err(GateError::new(
                "approval_surface_unavailable",
                "policy root changes require interactive operator presence",
            ));
        }
        let request = owner.claim_approval_request(ctx.connection_id, frame.header.corr)?;
        let caller = self.resolve_caller(ctx.connection_id).await;
        request.request.ensure_live()?;
        let key = crate::operator_confirm::DeclineKey::for_caller(&caller, ctx.connection_id);
        let decision = self
            .forwarding
            .operator_confirms()
            .approve_config_gate(ctx, frame, review, caller.description(), key)
            .await;
        use crate::config_gate::PresenceDecision;
        match PresenceDecision::from_operator_outcome(&decision) {
            PresenceDecision::InteractiveApproval => {}
            PresenceDecision::Declined => {
                return Err(GateError::new(
                    "approval_declined",
                    "the operator declined the policy root change",
                ))
            }
            _ => {
                return Err(GateError::new(
                    "approval_surface_unavailable",
                    "interactive operator presence is unavailable",
                ))
            }
        }
        // A rescan that already read the old file must finish before this
        // apply publishes the new pin and its unstamped reader view.
        let operation = self.supervisor.operation_lock().lock_owned().await;
        let digest = digest.clone();
        let cancellation = request.request.clone();
        let generation = owner
            .on_blocking(move |owner| {
                let _operation = operation;
                owner.apply_unstamped_edits_after_presence(
                    &digest,
                    true,
                    config_gate::now_ms(),
                    || cancellation.claim_publication(),
                )
            })
            .await?
            .ok_or_else(|| {
                GateError::new("plan_stale", "the policy root plan is no longer pending")
            })?;
        Ok(Some(ConfigControlResponse::Applied {
            generation,
            restart_required: false,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn owner_runtime(
        root: &cortexkit_test_support::ScratchDir,
    ) -> Arc<crate::config_gate::runtime::RuntimeOwner> {
        use crate::config_gate::capture::{ConfigCapturer, OfflineProjectRegistry};
        use crate::config_store::{ConfigStore, StoreRoot};

        let settings = root.join("subc.jsonc");
        std::fs::write(&settings, br#"{"version":1}"#).unwrap();
        std::fs::create_dir(root.join("store")).unwrap();
        Arc::new(
            crate::config_gate::runtime::RuntimeOwner::new(
                "policy-root-caller-tests".into(),
                ConfigStore::open_digest_only_for_tests(
                    StoreRoot::from_service_definition(root.join("store")).unwrap(),
                ),
                ConfigCapturer {
                    daemon_path: settings,
                    config_home: root.to_path_buf(),
                    projects: Arc::new(OfflineProjectRegistry),
                },
            )
            .with_policy_root_data(std::fs::canonicalize(root).unwrap()),
        )
    }

    fn context(
        handler: &ControlHandler,
        local: bool,
        module: Option<&str>,
    ) -> (
        RouteCtx,
        tokio::sync::mpsc::Receiver<crate::router::OutboundFrame>,
    ) {
        let connection_id = ConnectionId::new(700);
        if local {
            handler.record_local_connection(connection_id);
        }
        #[cfg(unix)]
        let peer = ObservedPeer::observed_uid(subc_os::effective_uid());
        #[cfg(windows)]
        let peer =
            ObservedPeer::observed_sid(subc_os::named_pipe::current_user_sid_string().unwrap());
        #[cfg(not(any(unix, windows)))]
        let peer = ObservedPeer::unobserved();
        handler
            .observed_peers
            .lock()
            .unwrap()
            .insert(connection_id, peer);
        if let Some(module) = module {
            handler
                .registry
                .register_with_control_ops(
                    subc_protocol::manifest::ModuleManifest::builder(module, "1.0.0").build(),
                    PROTOCOL_VERSION,
                    connection_id,
                    vec![],
                )
                .unwrap();
        }
        let (send, recv) = tokio::sync::mpsc::channel(8);
        (
            RouteCtx {
                connection_id,
                egress: crate::FrameSink::new(send),
            },
            recv,
        )
    }

    async fn refusal(
        handler: &ControlHandler,
        ctx: &RouteCtx,
        body: serde_json::Value,
    ) -> serde_json::Value {
        let request = Frame::build(
            FrameType::Request,
            control_flags(),
            0,
            0,
            1,
            serde_json::to_vec(&body).unwrap(),
        )
        .unwrap();
        let replies = handler
            .handle_control_frame_ingress(ctx, request, None, false)
            .await
            .unwrap();
        assert_eq!(replies[0].header.ty, FrameType::Error);
        serde_json::from_slice::<serde_json::Value>(&replies[0].body).unwrap()["code"].clone()
    }

    #[tokio::test]
    async fn policy_root_review_rotate_requires_vault_id_and_a_verified_launch() {
        let root = cortexkit_test_support::ScratchDir::new("policy-root-rotate-callers");
        let runtime = owner_runtime(&root);
        for (module, body) in [
            ("sample-module", json!({"op":ROTATE_OP})),
            (
                VAULT,
                json!({"op":ROTATE_OP,"statement_b64":"e30=","signature_hex":"00"}),
            ),
        ] {
            let handler = ControlHandler::new(Arc::new(Registry::default()))
                .with_config_runtime(runtime.clone());
            let (ctx, _recv) = context(&handler, true, Some(module));
            assert_eq!(
                refusal(&handler, &ctx, body).await,
                "policy_root_caller_not_permitted"
            );
        }
    }

    #[tokio::test]
    async fn policy_root_review_set_recover_and_unstamped_apply_require_local_operator() {
        let root = cortexkit_test_support::ScratchDir::new("policy-root-operator-callers");
        let runtime = owner_runtime(&root);
        let raw = ed25519_dalek::SigningKey::from_bytes(&[23; 32])
            .verifying_key()
            .to_bytes();
        let key = Key {
            key_id: subc_protocol::policy_root::key_id(&raw),
            public_key_hex: raw.iter().map(|byte| format!("{byte:02x}")).collect(),
        };
        let change = runtime.plan_policy_root(Move::Set(key.clone())).unwrap();
        for (local, module) in [(true, Some("sample-module")), (false, None)] {
            let handler = ControlHandler::new(Arc::new(Registry::default()))
                .with_config_runtime(runtime.clone());
            let (ctx, _recv) = context(&handler, local, module);
            for request in [
                json!({"op":SET_OP,"key_id":key.key_id,"public_key_hex":key.public_key_hex}),
                json!({"op":RECOVER_OP,"body_b64":"e30="}),
                json!({"op":"config.apply","digest":change.digest}),
            ] {
                assert_eq!(
                    refusal(&handler, &ctx, request).await,
                    "policy_root_caller_not_permitted"
                );
            }
        }
        assert!(runtime.read_policy_root().unwrap().0.pin.is_none());
        assert!(!root.join("policy-root").exists());
    }

    #[tokio::test]
    async fn policy_root_local_get_returns_null_and_client_cannot_rotate() {
        let handler = ControlHandler::new(Arc::new(Registry::default()));
        let connection_id = ConnectionId::new(700);
        handler.record_local_connection(connection_id);
        #[cfg(unix)]
        let peer = ObservedPeer::observed_uid(subc_os::effective_uid());
        #[cfg(windows)]
        let peer =
            ObservedPeer::observed_sid(subc_os::named_pipe::current_user_sid_string().unwrap());
        #[cfg(not(any(unix, windows)))]
        let peer = ObservedPeer::unobserved();
        handler
            .observed_peers
            .lock()
            .unwrap()
            .insert(connection_id, peer);
        let (send, _recv) = tokio::sync::mpsc::channel(8);
        let ctx = RouteCtx {
            connection_id,
            egress: crate::FrameSink::new(send),
        };
        let call = |body: serde_json::Value| {
            Frame::build(
                FrameType::Request,
                control_flags(),
                0,
                0,
                1,
                serde_json::to_vec(&body).unwrap(),
            )
            .unwrap()
        };
        let reply = handler
            .handle_control_frame_ingress(&ctx, call(json!({"op":GET_OP})), None, false)
            .await
            .unwrap();
        assert_eq!(reply[0].header.ty, FrameType::Response);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&reply[0].body).unwrap(),
            json!({"op":GET_OP,"pin":null,"generation":0})
        );
        let denied = handler
            .handle_control_frame_ingress(
                &ctx,
                call(json!({"op":ROTATE_OP,"statement_b64":"e30=","signature_hex":"00"})),
                None,
                false,
            )
            .await
            .unwrap();
        assert_eq!(denied[0].header.ty, FrameType::Error);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&denied[0].body).unwrap()["code"],
            "policy_root_caller_not_permitted"
        );
    }
}
