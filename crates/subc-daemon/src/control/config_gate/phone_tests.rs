use super::*;
use crate::config_gate::{
    capture::{ConfigCapturer, UnavailableProjectRegistry},
    runtime::{daemon_review, DeliveryObserver, RuntimeOwner},
    PlanKind,
};
use crate::config_store::{ConfigStore, StoreRoot};
use crate::supervise::{
    HealthConfig, RestartPolicy, SupervisedModule, Supervisor, SupervisorHandle,
};
use serde_json::json;
use std::sync::atomic::Ordering;
use subc_protocol::config::{
    ConfigAppliedRequest, ConfigApplyResult, ConfigDeclareRequest, ConfigGetRequest,
};
use subc_protocol::consent::{ConsentDecision, ConsentRequest, ConsentResponse};
use tokio::sync::mpsc;

const PHONE: ConnectionId = ConnectionId::new(90);
const CALLER: ConnectionId = ConnectionId::new(7);

struct Scratch(std::path::PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Admitted;
impl crate::config_gate::ReviewAdmission for Admitted {
    fn check(
        &self,
        registration: &crate::registry::ModuleRegistration,
    ) -> std::result::Result<(), GateError> {
        assert_eq!(registration.manifest.module_id, "callosum");
        assert_eq!(registration.connection_id, PHONE);
        Ok(())
    }
}

struct Fixture {
    scratch: Scratch,
    owner: Arc<RuntimeOwner>,
    handler: Arc<ControlHandler>,
    module: SupervisedModule,
    frames: mpsc::Receiver<crate::router::OutboundFrame>,
    sink: crate::FrameSink,
    plan: subc_control::config::ConfigPlanView,
}

impl Fixture {
    async fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/scratch")
            .join(format!(
                "config-phone-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        std::fs::create_dir_all(path.join("store")).unwrap();
        std::fs::create_dir_all(path.join("cortexkit")).unwrap();
        std::fs::copy("/bin/sh", path.join("program")).unwrap();
        std::fs::write(path.join("subc.jsonc"), serde_json::to_vec(&json!({"version":1,"modules":{
            "a":{"program":path.join("program")},
            "callosum":{"program":path.join("program"),"args":["-c","sleep 600"],"reserved":true}
        }})).unwrap()).unwrap();
        std::fs::write(path.join("cortexkit/a.jsonc"), b"{\"limit\":1}").unwrap();
        let owner = Arc::new(RuntimeOwner::new(
            "phone-adapter".into(),
            ConfigStore::open_digest_only_for_tests(
                StoreRoot::from_service_definition(path.join("store")).unwrap(),
            ),
            ConfigCapturer {
                daemon_path: path.join("subc.jsonc"),
                config_home: path.clone(),
                projects: Arc::new(UnavailableProjectRegistry),
            },
        ));
        for (id, connection) in [("a", ConnectionId::new(20)), ("callosum", PHONE)] {
            owner.declare(connection, id, ConfigDeclareRequest::new(json!({
                "$schema":"https://json-schema.org/draft/2020-12/schema","type":"object",
                "properties": if id == "a" { json!({"limit":{"type":"integer","description":"Limit","default":1}}) } else { json!({}) },
                "unevaluatedProperties":false,"x-ck-layers":["user"],"x-ck-apply":"live"
            }), path.join(format!("cortexkit/{id}.jsonc")).to_string_lossy(), vec![]), true).unwrap();
        }
        let view = owner
            .finish_plan(owner.prepare(PlanKind::Stamp).unwrap(), daemon_review)
            .unwrap();
        let facts = ApprovalFacts {
            connection: CALLER.get(),
            owner_uid: true,
            desktop_app: true,
            attested_operator_app: true,
            os_prompt_available: true,
            phone: None,
            caller: Default::default(),
        };
        owner.desktop_view_delivered(CALLER, &view).unwrap();
        let challenge = owner.begin_approval(&view.digest, &facts).unwrap();
        owner
            .finish_approval(
                challenge.attempt(),
                &facts,
                ApprovalReply::Presence(crate::config_gate::PresenceDecision::InteractiveApproval),
                1,
            )
            .unwrap();
        for (id, connection) in [("a", ConnectionId::new(20)), ("callosum", PHONE)] {
            let view = owner
                .module_get(connection, id, ConfigGetRequest::new())
                .unwrap();
            owner
                .module_view_delivered(connection, id, &view, Instant::now(), true)
                .unwrap();
            let probe = owner.health_probe(connection, id).unwrap().unwrap();
            owner.health_result(probe, true).unwrap();
            let pending = view.pending.unwrap();
            owner
                .module_applied(
                    connection,
                    id,
                    ConfigAppliedRequest::new(
                        pending.generation,
                        pending.attempt.unwrap(),
                        ConfigApplyResult::Ack,
                        vec![],
                    ),
                )
                .unwrap();
        }
        let registry = Arc::new(Registry::default());
        let forwarding = Arc::new(crate::ForwardingTable::default());
        let handle = SupervisorHandle::new();
        let supervisor = Supervisor::new(registry.clone(), RestartPolicy::default())
            .with_handle(handle.clone())
            .with_forwarding(forwarding.clone())
            .with_health_config(HealthConfig {
                cadence: std::time::Duration::from_secs(3600),
                ..Default::default()
            })
            .with_drain_timeout(std::time::Duration::from_millis(10));
        let (config, _) = crate::daemon_config::parse_with_activity(
            &std::fs::read_to_string(path.join("subc.jsonc")).unwrap(),
            path.join("subc.jsonc"),
        )
        .unwrap();
        let spec = config
            .modules
            .iter()
            .find(|entry| entry.module_id == "callosum")
            .unwrap()
            .module_spec();
        let module = supervisor.spawn(spec).unwrap();
        let nonce = handle.spawn_launch_nonce_for("callosum").unwrap();
        let handler = Arc::new(
            ControlHandler::with_forwarding(registry.clone(), forwarding.clone())
                .with_supervisor(handle)
                .with_supervisor_rescan(supervisor, path.join("subc.jsonc"), None)
                .with_config_runtime(owner.clone())
                .with_config_review_admission(Arc::new(Admitted))
                .with_test_phone_shortcut(true),
        );
        handler.observed_peers.lock().unwrap().insert(
            CALLER,
            subc_protocol::peer::ObservedPeer::observed_uid(subc_os::effective_uid()),
        );
        handler
            .hello_launch_nonces
            .lock()
            .unwrap()
            .record(PHONE, Some(&nonce));
        let mut manifest =
            subc_protocol::manifest::ModuleManifest::builder("callosum", "1.0.0").build();
        manifest.ready = Some(true);
        registry
            .register_unpublished_verified_with_control_ops(
                manifest,
                subc_protocol::PROTOCOL_VERSION,
                PHONE,
                vec![subc_protocol::consent::CONSENT_V1.into()],
                true,
            )
            .unwrap();
        let (sender, frames) = mpsc::channel(8);
        let sink = crate::FrameSink::new(sender);
        forwarding
            .register_module_connection(
                PHONE,
                "callosum".into(),
                subc_protocol::PROTOCOL_VERSION,
                subc_protocol::manifest::Concurrency::ModuleManaged,
                sink.clone(),
            )
            .unwrap();
        registry.publish_registration(PHONE).unwrap();
        std::fs::write(path.join("cortexkit/a.jsonc"), b"{\"limit\":2}").unwrap();
        let plan = owner
            .finish_plan(owner.prepare(PlanKind::Change).unwrap(), daemon_review)
            .unwrap();
        Self {
            scratch: Scratch(path),
            owner,
            handler,
            module,
            frames,
            sink,
            plan,
        }
    }

    fn apply(
        &self,
    ) -> tokio::task::JoinHandle<std::result::Result<ConfigControlResponse, GateError>> {
        let handler = self.handler.clone();
        let owner = self.owner.clone();
        let digest = self.plan.digest.clone();
        let (sender, _) = mpsc::channel(1);
        tokio::spawn(async move {
            let frame = Frame::build(
                FrameType::Request,
                control_flags(),
                0,
                0,
                881,
                serde_json::to_vec(&ConfigControlRequest::Apply {
                    digest: digest.clone(),
                })
                .unwrap(),
            )
            .unwrap();
            handler
                .execute_config_request(
                    &RouteCtx {
                        connection_id: CALLER,
                        egress: crate::FrameSink::new(sender),
                    },
                    &frame,
                    owner,
                    ConfigControlRequest::Apply { digest },
                )
                .await
        })
    }

    async fn request(&mut self) -> (u64, ConsentRequest) {
        let mut outbound = self.frames.recv().await.expect("phone request");
        assert_eq!(outbound.frame.header.ty, FrameType::Request);
        let request: ConsentRequest = serde_json::from_slice(&outbound.frame.body).unwrap();
        assert!(request.is_consistent());
        assert_eq!(request.plan_digest, self.plan.digest);
        assert_eq!(request.plan, self.plan.plan);
        if let Some(flushed) = outbound.flushed.take() {
            let _ = flushed.send(());
        }
        (outbound.frame.header.corr, request)
    }

    fn answer(&self, connection: ConnectionId, correlation: u64, request: &ConsentRequest) {
        let body =
            serde_json::to_vec(&ConsentResponse::new(request, ConsentDecision::Approved)).unwrap();
        assert!(!serde_json::from_slice::<serde_json::Value>(&body)
            .unwrap()
            .as_object()
            .unwrap()
            .contains_key("op"));
        let frame = Frame::build(
            FrameType::Response,
            control_flags(),
            0,
            0,
            correlation,
            body,
        )
        .unwrap();
        assert!(self
            .handler
            .handle_module_relay_response(connection, frame)
            .unwrap()
            .is_empty());
    }

    async fn stop(self) {
        self.handler.registry.deregister_connection(PHONE).unwrap();
        self.module.stop().await.unwrap();
        drop(self.scratch);
    }
}

#[tokio::test]
async fn config_phone_relay_approves_the_exact_plan_with_wire_correlation_and_records_channel() {
    let mut fixture = Fixture::new().await;
    let apply = fixture.apply();
    let (correlation, request) = fixture.request().await;
    assert_eq!(
        correlation, 1,
        "the endpoint's first wire correlation differs from approval attempt 2"
    );
    fixture.answer(ConnectionId::new(91), correlation, &request);
    fixture.answer(PHONE, correlation + 1, &request);
    assert!(!apply.is_finished());
    assert_eq!(fixture.owner.history().unwrap().0.len(), 1);
    let callosum_gate = fixture.owner.module_gate("callosum").unwrap();
    let held = callosum_gate.lock().await;
    fixture.answer(PHONE, correlation, &request);
    tokio::task::yield_now().await;
    assert!(
        !apply.is_finished(),
        "phone authority cannot change while approval commits"
    );
    drop(held);
    assert!(matches!(
        apply.await.unwrap().unwrap(),
        ConfigControlResponse::Applied { generation: 2 }
    ));
    let (history, _) = fixture.owner.history().unwrap();
    assert_eq!(
        history[1].approval_channel,
        subc_config::ApprovalChannel::PhoneCard
    );
    assert!(
        fixture.frames.try_recv().is_err(),
        "answered cards need no withdrawal"
    );
    fixture.stop().await;
}

#[tokio::test(start_paused = true)]
async fn config_phone_relay_deadline_withdraws_once_and_a_late_answer_changes_nothing() {
    let mut fixture = Fixture::new().await;
    let apply = fixture.apply();
    let mut outbound = fixture.frames.recv().await.unwrap();
    let correlation = outbound.frame.header.corr;
    let request: ConsentRequest = serde_json::from_slice(&outbound.frame.body).unwrap();
    tokio::time::advance(std::time::Duration::from_secs(299)).await;
    outbound.flushed.take().unwrap().send(()).unwrap();
    tokio::task::yield_now().await;
    tokio::time::advance(std::time::Duration::from_millis(999)).await;
    assert!(!apply.is_finished());
    assert!(fixture.frames.try_recv().is_err());
    tokio::time::advance(std::time::Duration::from_millis(1)).await;
    assert_eq!(apply.await.unwrap().unwrap_err().code, "approval_declined");
    let cancel = fixture.frames.recv().await.unwrap();
    assert_eq!(cancel.frame.header.ty, FrameType::Cancel);
    assert_eq!(cancel.frame.header.corr, correlation);
    assert!(cancel.frame.body.is_empty());
    fixture.answer(PHONE, correlation, &request);
    tokio::time::advance(crate::config_gate::CONFIG_PHONE_APPROVAL_TIMEOUT).await;
    assert!(
        fixture.frames.try_recv().is_err(),
        "the expired attempt withdraws only once"
    );
    assert_eq!(fixture.owner.history().unwrap().0.len(), 1);
    assert!(
        fixture
            .owner
            .begin_approval(
                &fixture.plan.digest,
                &fixture
                    .handler
                    .config_approval_facts(&RouteCtx {
                        connection_id: CALLER,
                        egress: crate::FrameSink::new(mpsc::channel(1).0),
                    })
                    .await
            )
            .is_ok(),
        "an unanswered card preserves the reviewed plan"
    );
    fixture.owner.disconnected(CALLER);
    fixture.stop().await;
}

#[tokio::test(start_paused = true)]
async fn config_phone_relay_cancel_disconnect_and_target_change_withdraw_without_waiting() {
    for event in 0..4 {
        let mut fixture = Fixture::new().await;
        let apply = fixture.apply();
        let (correlation, request) = fixture.request().await;
        match event {
            0 => {
                let (sender, _) = mpsc::channel(1);
                let ctx = RouteCtx {
                    connection_id: CALLER,
                    egress: crate::FrameSink::new(sender),
                };
                let cancel =
                    Frame::build(FrameType::Cancel, control_flags(), 0, 0, 881, vec![]).unwrap();
                assert!(fixture
                    .handler
                    .handle_control_frame(&ctx, cancel)
                    .await
                    .unwrap()
                    .is_empty());
            }
            1 => fixture.owner.disconnected(PHONE),
            2 => fixture.owner.disconnected(CALLER),
            _ => {
                let candidate = ConnectionId::new(91);
                fixture.owner.declare(candidate, "callosum", ConfigDeclareRequest::new(json!({
                    "$schema":"https://json-schema.org/draft/2020-12/schema","type":"object",
                    "properties":{},"unevaluatedProperties":false,"x-ck-layers":["user"],"x-ck-apply":"live"
                }), fixture.scratch.0.join("cortexkit/callosum.jsonc").to_string_lossy(), vec![]), false).unwrap();
                let gate = fixture.owner.module_gate("callosum").unwrap();
                let _gate = gate.lock().await;
                fixture
                    .owner
                    .promoted(candidate, "callosum", Instant::now())
                    .unwrap();
            }
        }
        assert_eq!(
            apply.await.unwrap().unwrap_err().code,
            "approval_surface_unavailable"
        );
        let cancel = fixture.frames.recv().await.unwrap();
        assert_eq!(cancel.frame.header.ty, FrameType::Cancel);
        assert_eq!(cancel.frame.header.corr, correlation);
        fixture.answer(PHONE, correlation, &request);
        assert_eq!(fixture.owner.history().unwrap().0.len(), 1);
        assert!(fixture.frames.try_recv().is_err());
        fixture.stop().await;
    }
}

#[tokio::test]
async fn config_phone_surface_uses_compiled_policy_admission_and_current_nonce_not_the_module_name()
{
    let mut fixture = Fixture::new().await;
    assert!(fixture.handler.config_phone_surface().await.is_some());
    Arc::get_mut(&mut fixture.handler)
        .unwrap()
        .operator_app_policy
        .phone_enabled = false;
    assert!(fixture.handler.config_phone_surface().await.is_none());
    let facts = fixture
        .handler
        .config_approval_facts(&RouteCtx {
            connection_id: CALLER,
            egress: crate::FrameSink::new(mpsc::channel(1).0),
        })
        .await;
    assert_eq!(
        fixture
            .owner
            .begin_approval(&fixture.plan.digest, &facts)
            .unwrap_err()
            .code,
        "approval_surface_unavailable"
    );
    Arc::get_mut(&mut fixture.handler)
        .unwrap()
        .operator_app_policy
        .phone_enabled = true;
    Arc::get_mut(&mut fixture.handler)
        .unwrap()
        .config_review_admission = Arc::new(crate::config_gate::MissingReviewAdmission);
    assert!(fixture.handler.config_phone_surface().await.is_none());
    Arc::get_mut(&mut fixture.handler)
        .unwrap()
        .config_review_admission = Arc::new(Admitted);
    fixture
        .handler
        .hello_launch_nonces
        .lock()
        .unwrap()
        .record(PHONE, Some("wrong-launch"));
    assert!(fixture.handler.config_phone_surface().await.is_none());
    fixture.stop().await;
}

#[tokio::test]
async fn config_phone_relay_full_writer_queue_preserves_owned_withdrawal_and_blocks_new_cards() {
    let mut fixture = Fixture::new().await;
    let worker = fixture.handler.spawn_config_delivery_worker().unwrap();
    let apply = fixture.apply();
    let (correlation, request) = fixture.request().await;
    for corr in 0..8 {
        fixture
            .sink
            .try_send(
                Frame::build(
                    FrameType::Push,
                    control_flags(),
                    0,
                    0,
                    corr,
                    br#"{"op":"test.backpressure"}"#.to_vec(),
                )
                .unwrap(),
            )
            .unwrap();
    }
    assert!(fixture.owner.cancel_approval_request(CALLER, 881));
    assert_eq!(
        apply.await.unwrap().unwrap_err().code,
        "approval_surface_unavailable"
    );
    assert!(
        fixture.handler.config_phone_surface().await.is_none(),
        "the old card must be withdrawn before another can be allocated on this endpoint"
    );
    for _ in 0..8 {
        assert_eq!(
            fixture.frames.recv().await.unwrap().frame.header.ty,
            FrameType::Push
        );
    }
    let mut cancel = fixture.frames.recv().await.unwrap();
    assert_eq!(cancel.frame.header.ty, FrameType::Cancel);
    assert_eq!(cancel.frame.header.corr, correlation);
    cancel.flushed.take().unwrap().send(()).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while fixture.handler.config_phone_surface().await.is_none() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    fixture.answer(PHONE, correlation, &request);
    assert_eq!(fixture.owner.history().unwrap().0.len(), 1);
    worker.abort();
    let _ = worker.await;
    fixture.stop().await;
}
