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

#[path = "phone_cancel_r2_tests.rs"]
mod cancel_r2_tests;
#[path = "phone_cancel_r3_tests.rs"]
mod cancel_r3_tests;
#[path = "phone_cancel_r4_tests.rs"]
mod cancel_r4_tests;
#[path = "phone_lifecycle_tests.rs"]
mod lifecycle_tests;

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
        Self::build(false).await
    }

    async fn build(lifecycle: bool) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/scratch")
            .join(format!(
                "config-phone-{}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
        std::fs::create_dir_all(path.join("store")).unwrap();
        std::fs::create_dir_all(path.join("cortexkit")).unwrap();
        std::fs::copy("/bin/sh", path.join("program")).unwrap();
        // macOS needs the signed system image. Run the dummy holder directly:
        // a shell may exec sleep before its own image can be confirmed.
        let phone_program = if cfg!(target_os = "macos") {
            std::path::PathBuf::from("/bin/sleep")
        } else {
            path.join("program")
        };
        std::fs::write(
            path.join("subc.jsonc"),
            serde_json::to_vec(&json!({"version":1,"modules":{
                "a":{"program":path.join("program")},
                "callosum":{"program":phone_program,"args":if cfg!(target_os = "macos") { json!(["600"]) } else { json!(["-c","sleep 600"]) },"reserved":true,
                    "overlap": if lifecycle { "safe" } else { "exclusive" }}
            }}))
            .unwrap(),
        )
        .unwrap();
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
        let supervisor = Supervisor::new_for_test(registry.clone(), RestartPolicy::default())
            .with_handle(handle.clone())
            .with_forwarding(forwarding.clone())
            .with_health_config(HealthConfig {
                cadence: std::time::Duration::from_secs(3600),
                ..Default::default()
            })
            .with_drain_timeout(std::time::Duration::from_millis(10));
        let supervisor = if lifecycle {
            supervisor.with_config_runtime(owner.clone())
        } else {
            supervisor
        };
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
        let module = if lifecycle {
            supervisor.spawn_async(spec).await.unwrap()
        } else {
            supervisor.spawn(spec).unwrap()
        };
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
        // Manual registration can precede confirmation that macOS has started
        // the module. Its reportable pid is published after that confirmation.
        let ready_deadline = std::time::Instant::now() + std::time::Duration::from_secs(6);
        loop {
            let status = module.status().unwrap();
            if status.enabled
                && status.process_alive
                && status.registration_active
                && status.pid.is_some()
            {
                break;
            }
            assert!(
                std::time::Instant::now() < ready_deadline,
                "phone fixture did not finish exec and registration: {status:?}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
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
        let mut outbound =
            tokio::time::timeout(std::time::Duration::from_secs(5), self.frames.recv())
                .await
                .expect("trusted active phone must receive the card")
                .expect("phone request");
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
        ConfigControlResponse::Applied { generation: 2, .. }
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

#[tokio::test]
async fn config_phone_answer_commit_timeout_records_nothing_and_preserves_the_plan() {
    let mut fixture = Fixture::new().await;
    tokio::time::pause();
    let operation = fixture.handler.supervisor.operation_lock();
    let held = operation.lock().await;
    let apply = fixture.apply();
    let (correlation, request) = fixture.request().await;
    fixture.answer(PHONE, correlation, &request);
    tokio::task::yield_now().await;
    tokio::time::advance(std::time::Duration::from_secs(30)).await;
    let error = tokio::time::timeout(std::time::Duration::from_millis(100), apply)
        .await
        .expect("an answered card must not wait indefinitely for the commit locks")
        .unwrap()
        .unwrap_err();
    assert_eq!(error.code, "config_commit_timeout");
    assert_eq!(fixture.owner.history().unwrap().0.len(), 1);
    drop(held);
    let facts = fixture
        .handler
        .config_approval_facts(&RouteCtx {
            connection_id: CALLER,
            egress: crate::FrameSink::new(mpsc::channel(1).0),
        })
        .await;
    assert!(fixture
        .owner
        .begin_approval(&fixture.plan.digest, &facts)
        .is_ok());
    fixture.stop().await;
}

#[tokio::test]
async fn config_phone_answer_expiring_just_before_publication_records_nothing() {
    let mut fixture = Fixture::new().await;
    tokio::time::pause();
    // Idle auto-advance must not outrun the blocking worker before it reaches
    // the controlled boundary; only the test advances this clock.
    let clock = tokio::spawn(async {
        loop {
            tokio::task::yield_now().await;
        }
    });
    let (started, at_commit_point) = tokio::sync::oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    fixture.owner.set_prepublication_hook(move || {
        let _ = started.send(());
        wait.recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
    });
    let apply = fixture.apply();
    let (correlation, request) = fixture.request().await;
    fixture.answer(PHONE, correlation, &request);
    at_commit_point.await.unwrap();
    tokio::time::advance(std::time::Duration::from_secs(30)).await;
    release.send(()).unwrap();
    let error = apply.await.unwrap().unwrap_err();
    assert_eq!(error.code, "config_commit_timeout");
    assert_eq!(fixture.owner.history().unwrap().0.len(), 1);
    clock.abort();
    fixture.stop().await;
}

#[tokio::test]
async fn config_phone_answer_publication_started_before_deadline_returns_its_committed_result() {
    let mut fixture = Fixture::new().await;
    tokio::time::pause();
    let clock = tokio::spawn(async {
        loop {
            tokio::task::yield_now().await;
        }
    });
    let (started, publication) = tokio::sync::oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    fixture.owner.set_publication_hook(move || {
        let _ = started.send(());
        wait.recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
    });
    let apply = fixture.apply();
    let (correlation, request) = fixture.request().await;
    fixture.answer(PHONE, correlation, &request);
    publication.await.unwrap();
    tokio::time::advance(std::time::Duration::from_secs(31)).await;
    tokio::task::yield_now().await;
    assert!(
        !apply.is_finished(),
        "a started publication must not report a false timeout"
    );
    release.send(()).unwrap();
    assert!(matches!(
        apply.await.unwrap().unwrap(),
        ConfigControlResponse::Applied { generation: 2, .. }
    ));
    assert_eq!(fixture.owner.history().unwrap().0.len(), 2);
    clock.abort();
    fixture.stop().await;
}

#[derive(Clone)]
struct PendingPresence(Arc<PendingPresenceState>);

struct PendingPresenceState {
    done: std::sync::Mutex<bool>,
    wake: std::sync::Condvar,
    events: mpsc::UnboundedSender<&'static str>,
}

impl crate::operator_confirm::OperatorWithdraw for PendingPresence {
    fn withdraw(&self, _: &str) {
        let mut done = self.0.done.lock().unwrap();
        if !*done {
            *done = true;
            let _ = self.0.events.send("withdrawn");
            self.0.wake.notify_all();
        }
    }
}

impl crate::operator_confirm::OperatorProvider for PendingPresence {
    fn config_gate_ready(&self) -> bool {
        true
    }

    fn prompt(
        &self,
        _: &str,
        _: Box<dyn FnOnce(Arc<dyn crate::operator_confirm::OperatorWithdraw>) + Send>,
    ) -> crate::operator_confirm::ProviderResult {
        let _ = self.0.events.send("unexpected generic prompt");
        crate::operator_confirm::ProviderResult::Unavailable
    }

    fn prompt_gate(
        &self,
        _: &str,
        started: Box<dyn FnOnce(Arc<dyn crate::operator_confirm::OperatorWithdraw>) + Send>,
    ) -> crate::operator_confirm::ProviderResult {
        started(Arc::new(self.clone()));
        let _ = self.0.events.send("shown");
        let mut done = self.0.done.lock().unwrap();
        while !*done {
            done = self.0.wake.wait(done).unwrap();
        }
        crate::operator_confirm::ProviderResult::Unavailable
    }
}

async fn connection_approval_withdrawal(phone: bool, disconnect: bool, early_cancel: bool) {
    use crate::operator_confirm::OperatorWithdraw;
    let mut fixture = Fixture::new().await;
    let (events, mut presence_events) = mpsc::unbounded_channel();
    let presence = PendingPresence(Arc::new(PendingPresenceState {
        done: std::sync::Mutex::new(false),
        wake: std::sync::Condvar::new(),
        events,
    }));
    if !phone {
        fixture.plan = fixture
            .owner
            .finish_plan(
                fixture.owner.prepare(PlanKind::Change).unwrap(),
                |old, new| {
                    let mut changes = daemon_review(old, new)?;
                    let mut field = subc_config::FieldChange::new(
                        "a",
                        "/limit",
                        subc_config::RecordedValue::Value { value: json!(1) },
                        subc_config::RecordedValue::Value { value: json!(2) },
                        subc_config::ApplyKind::Live,
                    );
                    field.old_source = subc_config::Layer::User;
                    field.new_source = subc_config::Layer::User;
                    changes.fields = vec![field];
                    Ok(changes)
                },
            )
            .unwrap();
        fixture.owner.set_os_prompt_policy_for_tests();
        fixture
            .handler
            .forwarding
            .operator_confirms()
            .configure(Arc::new(presence.clone()), Default::default());
    }
    let router = Arc::new(crate::Router::with_control_handler(fixture.handler.clone()));
    let (connection, socket, server) = crate::server::config_test_connection_loop(router);
    fixture.handler.observed_peers.lock().unwrap().insert(
        connection,
        subc_protocol::peer::ObservedPeer::observed_uid(subc_os::effective_uid()),
    );
    if !phone {
        let facts = fixture
            .handler
            .config_approval_facts(&RouteCtx {
                connection_id: connection,
                egress: crate::FrameSink::new(mpsc::channel(1).0),
            })
            .await;
        assert!(facts.os_prompt_available);
        let challenge = fixture
            .owner
            .begin_approval(&fixture.plan.digest, &facts)
            .unwrap();
        assert!(
            matches!(challenge, ApprovalChallenge::OsPrompt { .. }),
            "OS fixture must select the compact prompt"
        );
        fixture.owner.cancel_approval(challenge.attempt());
    }
    let mut socket = Some(socket);
    let apply = Frame::build(
        FrameType::Request,
        control_flags(),
        0,
        0,
        881,
        serde_json::to_vec(&ConfigControlRequest::Apply {
            digest: fixture.plan.digest.clone(),
        })
        .unwrap(),
    )
    .unwrap();
    crate::write_frame(socket.as_mut().unwrap(), &apply)
        .await
        .unwrap();
    if early_cancel {
        // Both frames enter the reader without waiting for any prompt or task
        // tracking. The reader must reserve cancellation before it defers Apply.
        let cancel = Frame::build(FrameType::Cancel, control_flags(), 0, 0, 881, vec![]).unwrap();
        crate::write_frame(socket.as_mut().unwrap(), &cancel)
            .await
            .unwrap();
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            crate::read_frame(socket.as_mut().unwrap()),
        )
        .await
        .unwrap()
        .unwrap()
        .unwrap();
        let error: subc_protocol::ErrorBody = serde_json::from_slice(&response.body).unwrap();
        presence.withdraw("test cleanup");
        drop(socket);
        tokio::time::timeout(std::time::Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let mut shown = false;
        while let Ok(event) = presence_events.try_recv() {
            shown |= event == "shown" || event == "unexpected generic prompt";
        }
        assert_eq!(fixture.owner.history().unwrap().0.len(), 1);
        fixture.stop().await;
        assert_eq!(
            error.code, "approval_surface_unavailable",
            "an immediate Cancel must cancel Apply, not become unknown_subscription"
        );
        assert!(
            !shown,
            "an Apply cancelled in the reader must never open a prompt"
        );
        return;
    }
    let card = if phone {
        Some(fixture.request().await)
    } else {
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(5), presence_events.recv())
                .await
                .unwrap(),
            Some("shown")
        );
        None
    };
    if disconnect {
        drop(socket.take());
    } else {
        let cancel = Frame::build(FrameType::Cancel, control_flags(), 0, 0, 881, vec![]).unwrap();
        crate::write_frame(socket.as_mut().unwrap(), &cancel)
            .await
            .unwrap();
    }
    let withdrawn = if let Some((correlation, _)) = &card {
        tokio::time::timeout(std::time::Duration::from_secs(5), fixture.frames.recv())
            .await
            .ok()
            .flatten()
            .is_some_and(|frame| {
                frame.frame.header.ty == FrameType::Cancel
                    && frame.frame.header.corr == *correlation
            })
    } else {
        matches!(
            tokio::time::timeout(std::time::Duration::from_secs(5), presence_events.recv()).await,
            Ok(Some("withdrawn"))
        )
    };
    // Observe withdrawal before cleanup: disconnect the phone if withdrawal
    // was absent, and always release the blocked OS provider. Neither cleanup
    // action can change the earlier wire observation into a passing result.
    if !withdrawn && phone {
        fixture.owner.disconnected(PHONE);
    }
    presence.withdraw("test cleanup");
    drop(socket);
    tokio::time::timeout(std::time::Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(fixture.owner.history().unwrap().0.len(), 1);
    fixture.stop().await;
    assert!(
        withdrawn,
        "connection_loop must withdraw a pending approval: phone={phone}, disconnect={disconnect}"
    );
}

#[tokio::test]
async fn config_connection_loop_os_cancel_withdraws_without_recording() {
    connection_approval_withdrawal(false, false, false).await;
}

#[tokio::test]
async fn config_connection_loop_os_disconnect_withdraws_without_recording() {
    connection_approval_withdrawal(false, true, false).await;
}

#[tokio::test]
async fn config_connection_loop_phone_cancel_withdraws_without_recording() {
    connection_approval_withdrawal(true, false, false).await;
}

#[tokio::test]
async fn config_connection_loop_phone_disconnect_withdraws_without_recording() {
    connection_approval_withdrawal(true, true, false).await;
}

#[tokio::test]
async fn config_connection_loop_cancel_before_tracking_never_prompts() {
    connection_approval_withdrawal(false, false, true).await;
}
