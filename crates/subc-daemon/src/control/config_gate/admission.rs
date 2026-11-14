use super::*;

pub(super) struct SupervisedReviewAdmission(pub(super) ControlHandler);

impl crate::config_gate::ReviewAdmission for SupervisedReviewAdmission {
    fn check(
        &self,
        registration: &crate::registry::ModuleRegistration,
    ) -> std::result::Result<(), GateError> {
        let module = &registration.manifest.module_id;
        let current = self
            .0
            .registry
            .get_module(module)
            .ok()
            .flatten()
            .is_some_and(|current| current.connection_id == registration.connection_id)
            && self
                .0
                .forwarding
                .active_module_endpoint(module)
                .ok()
                .flatten()
                .is_some_and(|endpoint| endpoint.connection_id == registration.connection_id);
        if !current {
            return Err(GateError::new(
                "program_refused",
                "reviewer is not the active delivery target",
            ));
        }
        // Channel authority comes only from its live launch, never a bearer
        // nonce that happened to be included in the same HELLO.
        if self
            .0
            .supervisor
            .registration_spawn(registration.connection_id)
            .is_some()
        {
            return self.0.supervisor.check_channel_program_review(
                &self.0.registry,
                module,
                registration.connection_id,
            );
        }
        let nonce = self
            .0
            .hello_launch_nonces
            .lock()
            .map_err(|_| GateError::new("program_refused", "HELLO provenance is unavailable"))?
            .nonce(registration.connection_id)
            .map(str::to_owned);
        self.0
            .supervisor
            .check_program_review(module, nonce.as_deref().unwrap_or_default())
    }
}

#[cfg(all(test, target_os = "linux"))]
mod dual_path_tests {
    use super::*;
    use crate::config_gate::{
        capture::{ConfigCapturer, UnavailableProjectRegistry},
        runtime::RuntimeOwner,
        ApprovalFacts, ApprovalReply, PlanKind, PresenceDecision, ReviewAdmission,
    };
    use crate::supervise::{ModuleSpec, RestartPolicy, SupervisedModule, Supervisor};
    use serde_json::json;
    use tokio::sync::mpsc;

    const PRIMARY: ConnectionId = ConnectionId::new(700);

    struct Fixture {
        root: cortexkit_test_support::ScratchDir,
        owner: Arc<RuntimeOwner>,
        _supervisor: Supervisor,
        module: SupervisedModule,
        handler: ControlHandler,
        sink: crate::FrameSink,
        frames: Mutex<mpsc::Receiver<crate::router::OutboundFrame>>,
    }

    impl Fixture {
        async fn new(gated: bool, protocol: &str) -> Self {
            let root = cortexkit_test_support::ScratchDir::new("dual-path-admission");
            std::fs::create_dir_all(root.join("store")).unwrap();
            let path = root.join("subc.jsonc");
            std::fs::write(&path, serde_json::to_vec(&json!({
                "version": 1, "modules": {"ckbus": {
                    "program": "/bin/sleep", "args": ["600"], "protocol": protocol, "registration_channel": true,
                    "env": {"HOME": root.to_string_lossy(), "XDG_DATA_HOME": root.to_string_lossy(),
                        "XDG_CONFIG_HOME": root.to_string_lossy(), "XDG_RUNTIME_DIR": root.to_string_lossy()}
                }}
            })).unwrap()).unwrap();
            let owner = Arc::new(RuntimeOwner::new(
                "dual-path-test".into(),
                crate::config_store::ConfigStore::open_digest_only_for_tests(
                    crate::config_store::StoreRoot::from_service_definition(root.join("store"))
                        .unwrap(),
                ),
                ConfigCapturer {
                    daemon_path: path.clone(),
                    config_home: root.to_path_buf(),
                    projects: Arc::new(UnavailableProjectRegistry),
                },
            ));
            if gated {
                let prepared = owner.prepare(PlanKind::Stamp).unwrap();
                let plan = owner
                    .finish_plan(prepared, crate::config_gate::runtime::daemon_review)
                    .unwrap();
                let facts = ApprovalFacts {
                    connection: 7,
                    owner_uid: true,
                    desktop_app: true,
                    attested_operator_app: true,
                    os_prompt_available: true,
                    phone: None,
                    caller: crate::caller_identity::CallerIdentity::default(),
                };
                owner
                    .desktop_view_delivered(ConnectionId::new(7), &plan)
                    .unwrap();
                let challenge = owner.begin_approval(&plan.digest, &facts).unwrap();
                owner
                    .finish_approval(
                        challenge.attempt(),
                        &facts,
                        ApprovalReply::Presence(PresenceDecision::InteractiveApproval),
                        1,
                    )
                    .unwrap();
            }
            let handle = SupervisorHandle::new();
            handle.capture_registration_channels_for_test();
            let registry = Arc::new(Registry::default());
            let forwarding = Arc::new(ForwardingTable::default());
            let supervisor = Supervisor::new_for_test(registry.clone(), RestartPolicy::default())
                .with_handle(handle.clone())
                .with_forwarding(forwarding.clone())
                .with_config_runtime(owner.clone());
            let spec = Self::spec(&path);
            let module = supervisor.spawn_async(spec).await.unwrap();
            if protocol == "subc" {
                let launch = handle.registration_for_test("ckbus").unwrap();
                tokio::time::timeout(std::time::Duration::from_secs(6), launch.wait_launch())
                    .await
                    .expect("registration launch must settle");
            }
            let handler = ControlHandler::with_forwarding(registry, forwarding)
                .with_supervisor(handle)
                .with_config_runtime(owner.clone())
                .with_activity_ledger(&root, "dual-path-test".into());
            let (tx, frames) = mpsc::channel(32);
            Self {
                root,
                owner,
                _supervisor: supervisor,
                module,
                handler,
                sink: crate::FrameSink::new(tx),
                frames: Mutex::new(frames),
            }
        }

        fn spec(path: &std::path::Path) -> ModuleSpec {
            crate::daemon_config::parse_with_activity(&std::fs::read_to_string(path).unwrap(), path)
                .unwrap()
                .0
                .modules[0]
                .module_spec()
        }

        fn recovery(&self) {
            std::fs::write(
                self.root
                    .join("store")
                    .join(crate::config_store::STORE_FILE),
                b"tampered",
            )
            .unwrap();
            assert!(self.owner.check_recovery().unwrap());
        }

        fn approve(&self, kind: PlanKind) {
            let prepared = self.owner.prepare(kind).unwrap();
            let plan = self
                .owner
                .finish_plan(prepared, crate::config_gate::runtime::daemon_review)
                .unwrap();
            let facts = ApprovalFacts {
                connection: 7,
                owner_uid: true,
                desktop_app: true,
                attested_operator_app: true,
                os_prompt_available: true,
                phone: None,
                caller: crate::caller_identity::CallerIdentity::default(),
            };
            self.owner
                .desktop_view_delivered(ConnectionId::new(7), &plan)
                .unwrap();
            let challenge = self.owner.begin_approval(&plan.digest, &facts).unwrap();
            self.owner
                .finish_approval(
                    challenge.attempt(),
                    &facts,
                    ApprovalReply::Presence(PresenceDecision::InteractiveApproval),
                    1,
                )
                .unwrap();
        }

        fn hello(&self, channel: bool, nonce: Option<&str>) -> Frame {
            if channel {
                self.handler
                    .supervisor
                    .registration_for_test("ckbus")
                    .unwrap()
                    .bind(PRIMARY);
            }
            let mut replies = self
                .handler
                .handle_hello(PRIMARY, Some(self.sink.clone()), hello(nonce))
                .unwrap();
            if replies.is_empty() {
                self.frames.lock().unwrap().try_recv().unwrap().frame
            } else {
                replies.remove(0)
            }
        }

        fn review(&self) -> Result<(), GateError> {
            let registration = self.handler.registry.get_module("ckbus").unwrap().unwrap();
            SupervisedReviewAdmission(self.handler.clone()).check(&registration)
        }

        async fn stop(&self) {
            self.handler.cleanup_connection(PRIMARY).unwrap();
            self.module.stop().await.unwrap();
        }
    }

    fn hello(nonce: Option<&str>) -> Frame {
        Frame::build(
            FrameType::Hello,
            control_flags(),
            0,
            0,
            1,
            serde_json::to_vec(&subc_protocol::ModuleHelloBody {
                manifest: subc_protocol::manifest::ModuleManifest::builder("ckbus", "1.0.0")
                    .build(),
                protocol_ver: subc_protocol::PROTOCOL_VERSION,
                control_ops: None,
                launch_nonce: nonce.map(str::to_owned),
            })
            .unwrap(),
        )
        .unwrap()
    }

    fn activity(nonce: &str) -> Frame {
        Frame::build(
            FrameType::Request,
            control_flags(),
            0,
            0,
            2,
            serde_json::to_vec(&json!({"op":"activity.query", "limit":1,
                "consumer_identity":{"module_id":"ckbus", "launch_nonce":nonce}}))
            .unwrap(),
        )
        .unwrap()
    }

    fn consumer(f: &Fixture) -> RouteCtx {
        RouteCtx {
            connection_id: ConnectionId::new(701),
            egress: f.sink.clone(),
        }
    }

    #[tokio::test]
    async fn dual_path_gated_channel_hello_without_nonce_and_reviewer_are_admitted() {
        for recovery in [false, true] {
            let f = Fixture::new(true, "subc").await;
            assert!(f.handler.supervisor.spawn_nonce("ckbus").is_none());
            if recovery {
                f.recovery();
            }
            assert_eq!(f.hello(true, None).header.ty, FrameType::HelloAck);
            f.review()
                .expect("the live channel binding authorizes review without a nonce");
            f.stop().await;
        }
    }

    #[tokio::test]
    async fn dual_path_ungated_channel_reviewer_is_admitted() {
        let f = Fixture::new(false, "subc").await;
        assert_eq!(f.hello(true, None).header.ty, FrameType::HelloAck);
        f.review()
            .expect("an ungated channel retains supervised review authority");
        f.stop().await;
    }

    #[tokio::test]
    async fn audit2_channel_reviewer_remains_available_after_unstamp() {
        let f = Fixture::new(true, "subc").await;
        assert_eq!(f.hello(true, None).header.ty, FrameType::HelloAck);
        f.review().unwrap();
        f.approve(PlanKind::Unstamp);
        assert!(!f.owner.module_is_gated("ckbus"));
        f.review()
            .expect("an unstamped live channel uses the ungated image checks");
        f.stop().await;
    }

    #[tokio::test]
    async fn audit2_channel_reviewer_without_binding_is_refused_after_restamp() {
        let f = Fixture::new(true, "subc").await;
        assert_eq!(f.hello(true, None).header.ty, FrameType::HelloAck);
        f.approve(PlanKind::Unstamp);
        f.review().expect("review is available while ungated");
        f.approve(PlanKind::Stamp);
        assert!(f.owner.module_is_gated("ckbus"));
        assert_eq!(f.review().unwrap_err().code, "program_refused");
        f.stop().await;
    }

    #[tokio::test]
    async fn audit2_recovery_revokes_matching_ungated_nonce_authority() {
        for missing in [false, true] {
            let f = Fixture::new(true, "subc").await;
            let legacy = "legacy-ungated";
            let nonce = "matching-legacy-nonce";
            f.handler.supervisor.set_spawn_nonce(legacy, nonce.into());
            assert!(!f.owner.module_is_gated(legacy));
            assert!(f
                .handler
                .supervisor
                .spawned_consumer_authorized(legacy, nonce));
            if missing {
                std::fs::remove_file(f.root.join("store").join(crate::config_store::STORE_FILE))
                    .unwrap();
                assert!(f.owner.check_recovery().unwrap());
            } else {
                f.recovery();
            }
            assert!(
                !f.handler
                    .supervisor
                    .spawned_consumer_authorized(legacy, nonce),
                "neither a missing nor an unverified store permits nonce authority"
            );
            f.stop().await;
        }
    }

    #[tokio::test]
    async fn audit2_recovering_channel_cannot_fall_back_to_its_matching_nonce() {
        let f = Fixture::new(true, "subc").await;
        let spawn = f.handler.supervisor.registration_for_test("ckbus").unwrap();
        assert!(f
            .handler
            .supervisor
            .has_live_verified_channel_binding(&spawn));
        f.approve(PlanKind::Unstamp);
        let nonce = "matching-legacy-nonce";
        f.handler.supervisor.set_spawn_nonce("ckbus", nonce.into());
        assert!(f
            .handler
            .supervisor
            .spawned_consumer_authorized("ckbus", nonce));
        f.approve(PlanKind::Stamp);
        f.recovery();
        assert!(spawn.has_program_binding());
        assert!(!f
            .handler
            .supervisor
            .has_live_verified_channel_binding(&spawn));
        spawn
            .check()
            .expect("the retained publication is live; only its active table binding was removed");
        let refused = f.hello(true, Some(nonce));
        assert_eq!(refused.header.ty, FrameType::Error);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&refused.body).unwrap()["code"],
            "config_gated"
        );
        f.stop().await;
    }

    async fn delayed_plain_hello_after_replacement(recovery: bool) {
        let f = Fixture::new(true, "subc").await;
        let old = f.handler.supervisor.registration_for_test("ckbus").unwrap();
        let replacement = f
            ._supervisor
            .spawn_async(Fixture::spec(&f.root.join("subc.jsonc")))
            .await
            .unwrap();
        let current = f.handler.supervisor.registration_for_test("ckbus").unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(6), current.wait_launch())
            .await
            .expect("replacement registration launch must settle");
        assert!(!Arc::ptr_eq(&old, &current));
        assert!(f.module.status().unwrap().process_alive);
        assert!(old.has_program_binding());
        old.check()
            .expect("the older process remains alive with its verified publication");
        if recovery {
            f.recovery();
        }
        old.bind(PRIMARY);
        let refused = f.hello(false, None);
        assert_eq!(
            refused.header.ty,
            FrameType::Error,
            "a delayed first HELLO cannot publish a superseded Plain launch"
        );
        f.handler.cleanup_connection(PRIMARY).unwrap();
        assert_eq!(
            f.hello(true, None).header.ty,
            FrameType::HelloAck,
            "the current Plain launch keeps the id in normal admission and recovery"
        );
        f.handler.cleanup_connection(PRIMARY).unwrap();
        replacement.stop().await.unwrap();
        f.stop().await;
    }

    #[tokio::test]
    async fn audit3_delayed_plain_hello_is_refused_after_replacement() {
        delayed_plain_hello_after_replacement(false).await;
    }

    #[tokio::test]
    async fn audit3_delayed_plain_hello_is_refused_after_replacement_in_recovery() {
        delayed_plain_hello_after_replacement(true).await;
    }

    #[tokio::test]
    async fn audit2_channel_review_rechecks_current_launch_after_image_verification() {
        let f = Fixture::new(true, "subc").await;
        assert_eq!(f.hello(true, None).header.ty, FrameType::HelloAck);
        f.review().unwrap();
        let pause = f
            .handler
            .supervisor
            .hold_program_review_after_image_for_test();
        let registration = f.handler.registry.get_module("ckbus").unwrap().unwrap();
        let admission = SupervisedReviewAdmission(f.handler.clone());
        let reviewing = tokio::task::spawn_blocking(move || admission.check(&registration));
        pause.0.wait();
        let replacement = f
            ._supervisor
            .spawn_async(Fixture::spec(&f.root.join("subc.jsonc")))
            .await
            .unwrap();
        assert_ne!(
            replacement.status().unwrap().pid,
            f.module.status().unwrap().pid
        );
        pause.1.wait();
        let result = reviewing.await.unwrap();
        f.handler.cleanup_connection(PRIMARY).unwrap();
        replacement.stop().await.unwrap();
        f.stop().await;
        assert_eq!(
            result.unwrap_err().code,
            "program_refused",
            "a same-image Plain relaunch cannot inherit an in-flight channel review"
        );
    }

    #[tokio::test]
    async fn dual_path_channel_reviewer_retired_or_superseded_spawn_is_refused() {
        for retired in [false, true] {
            let f = Fixture::new(false, "subc").await;
            assert_eq!(f.hello(true, None).header.ty, FrameType::HelloAck);
            f.review()
                .expect("verify live authority before testing its revocation");
            let spawn = f.handler.supervisor.registration_for_test("ckbus").unwrap();
            if retired {
                spawn.retire();
            } else {
                f.handler
                    .supervisor
                    .supersede_review_launch_for_test("ckbus");
            }
            assert_eq!(f.review().unwrap_err().code, "program_refused");
            f.stop().await;
        }
    }

    #[tokio::test]
    async fn dual_path_channel_body_nonce_never_grants_review_authority() {
        let f = Fixture::new(false, "subc").await;
        let nonce = f.handler.supervisor.spawn_nonce("ckbus").unwrap();
        assert_eq!(f.hello(true, Some(&nonce)).header.ty, FrameType::HelloAck);
        f.review()
            .expect("channel authority works independently of the body nonce");
        assert!(f
            .handler
            .hello_launch_nonces
            .lock()
            .unwrap()
            .nonce(PRIMARY)
            .is_none());
        f.handler
            .supervisor
            .registration_for_test("ckbus")
            .unwrap()
            .retire();
        assert_eq!(f.review().unwrap_err().code, "program_refused");
        f.stop().await;
    }

    /// The supervisor's swap gate refuses a gated id on its own, behind the
    /// handler's earlier reserved-module check. A handler without the
    /// configuration runtime reaches it directly.
    #[tokio::test]
    async fn dual_path_gated_hello_without_its_channel_names_the_channel_requirement() {
        let f = Fixture::new(true, "subc").await;
        assert!(f.handler.supervisor.module_is_gated("ckbus"));
        let handler = ControlHandler::with_forwarding(
            Arc::new(Registry::default()),
            Arc::new(ForwardingTable::default()),
        )
        .with_supervisor(f.handler.supervisor.clone());
        let mut replies = handler
            .handle_hello(PRIMARY, Some(f.sink.clone()), hello(None))
            .unwrap();
        let reply = if replies.is_empty() {
            f.frames.lock().unwrap().try_recv().unwrap().frame
        } else {
            replies.remove(0)
        };
        assert_eq!(reply.header.ty, FrameType::Error);
        let error = serde_json::from_slice::<serde_json::Value>(&reply.body).unwrap();
        assert_eq!(error["code"], "swap_token_invalid");
        let message = error["message"].as_str().unwrap();
        assert!(
            message.contains("must register over its supervised registration channel")
                && !message.contains("swap"),
            "the refusal names the channel requirement, not a swap: {message}"
        );
        f.stop().await;
    }

    #[tokio::test]
    async fn dual_path_protocol_none_gets_no_channel_or_nonce_and_refuses_hello() {
        let f = Fixture::new(false, "none").await;
        assert!(f.handler.supervisor.spawn_nonce("ckbus").is_none());
        assert!(f
            .handler
            .supervisor
            .registration_for_test("ckbus")
            .is_none());
        let reply = f.hello(false, None);
        assert_eq!(
            reply.header.ty,
            FrameType::Error,
            "protocol:none has no legitimate HELLO holder"
        );
        let error = serde_json::from_slice::<serde_json::Value>(&reply.body).unwrap();
        assert_eq!(error["code"], "swap_token_invalid");
        let message = error["message"].as_str().unwrap();
        assert!(
            message.contains("a protocol:none module may not register")
                && !message.contains("swap"),
            "the refusal names the protocol:none reason, not a swap: {message}"
        );
        f.stop().await;
        f.handler.supervisor.retire("ckbus");
        let reply = f.hello(false, Some("stale"));
        assert_eq!(
            reply.header.ty,
            FrameType::Error,
            "the no-holder reservation survives retirement"
        );
    }

    #[tokio::test]
    async fn dual_path_gated_nonce_guards_precede_legacy_admission_without_roster() {
        for recovery in [false, true] {
            let f = Fixture::new(true, "subc").await;
            f.stop().await;
            if recovery {
                f.recovery();
            }
            for rostered in [true, false] {
                if !rostered {
                    f.handler.supervisor.retire("ckbus");
                }
                let handle = &f.handler.supervisor;
                handle.set_spawn_nonce("ckbus", "same-body-nonce".into());
                handle.set_reserved_nonce("ckbus", "same-body-nonce".into());
                handle.open_swap("ckbus", "same-body-nonce".into());
                handle.set_reserved_prefixes("ckbus", &["child:".into()]);
                assert!(!handle.reserved_hello_authorized("ckbus", Some("same-body-nonce")));
                assert_eq!(
                    handle.swap_hello_admission("ckbus", Some("same-body-nonce")),
                    SwapHelloAdmission::ChannelRequired
                );
                assert!(!handle.reserved_hello_authorized("child:tool", Some("same-body-nonce")));
                assert!(!handle.spawned_consumer_authorized("ckbus", "same-body-nonce"));
                let identity = ConsumerIdentity {
                    module_id: "ckbus".into(),
                    launch_nonce: "same-body-nonce".into(),
                };
                let refused = f
                    .handler
                    .route_open_principal(ConnectionId::new(9999), &hello(None), Some(identity))
                    .unwrap()
                    .unwrap_err();
                assert_eq!(
                    serde_json::from_slice::<serde_json::Value>(&refused.body).unwrap()["code"],
                    "bad_consumer_identity"
                );
                assert_eq!(
                    f.hello(false, Some("same-body-nonce")).header.ty,
                    FrameType::Error
                );
                let refused = f
                    .handler
                    .handle_activity_read(&consumer(&f), activity("same-body-nonce"))
                    .await
                    .unwrap();
                assert_eq!(
                    serde_json::from_slice::<serde_json::Value>(&refused[0].body).unwrap()["code"],
                    "activity_read_forbidden"
                );
            }
            assert!(
                f.owner.module_is_gated("ckbus"),
                "admission records, not the roster, require a channel"
            );
        }
    }

    #[tokio::test]
    async fn dual_path_ungated_nonce_hello_swap_and_consumer_remain_compatible() {
        let f = Fixture::new(false, "subc").await;
        let handle = &f.handler.supervisor;
        let nonce = handle.spawn_nonce("ckbus").unwrap();
        assert_eq!(f.hello(false, Some(&nonce)).header.ty, FrameType::HelloAck);
        f.review()
            .expect("nonce-registered reviewers retain legacy review authority");
        let identity = ConsumerIdentity {
            module_id: "ckbus".into(),
            launch_nonce: nonce.clone(),
        };
        assert!(
            matches!(f.handler.route_open_principal(ConnectionId::new(9999), &hello(None), Some(identity)).unwrap(),
            Ok(Principal::Reserved { module_id }) if module_id == "ckbus")
        );
        let reads = f
            .handler
            .handle_activity_read(&consumer(&f), activity(&nonce))
            .await
            .unwrap();
        assert_eq!(
            reads[0].header.ty,
            FrameType::Response,
            "a legacy nonce consumer keeps its activity reads"
        );
        assert_eq!(
            f.handler.connection_is_current_launch(
                "ckbus",
                PRIMARY,
                handle.spawn_nonce("ckbus").as_deref()
            ),
            crate::scopes::LaunchStanding::Current
        );
        handle.open_swap("ckbus", "candidate-token".into());
        assert_eq!(
            handle.swap_hello_admission("ckbus", Some("candidate-token")),
            SwapHelloAdmission::Candidate
        );
        assert_eq!(
            handle.swap_hello_admission("ckbus", Some(&nonce)),
            SwapHelloAdmission::NotSwapping
        );
        assert_eq!(
            handle.swap_hello_admission("ckbus", Some("wrong")),
            SwapHelloAdmission::Refused
        );
        f.stop().await;
    }
}
