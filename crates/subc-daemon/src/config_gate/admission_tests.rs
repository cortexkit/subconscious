use super::*;
use crate::supervise::{ModuleSpec, ModuleState, RestartPolicy, Supervisor, SupervisorHandle};
use std::{os::unix::fs::PermissionsExt, sync::Arc, time::Duration};

fn channel_handle() -> SupervisorHandle {
    let handle = SupervisorHandle::new();
    handle.capture_registration_channels_for_test();
    handle
}

struct GatedClaimFixture {
    rig: Rig,
    owner: Arc<RuntimeOwner>,
    supervisor: Supervisor,
    handle: SupervisorHandle,
    module: crate::supervise::SupervisedModule,
    spawn: Arc<crate::supervise::RegistrationSpawn>,
    router: Arc<crate::Router>,
    handler: Arc<crate::ControlHandler>,
    primary: crate::control::claims::tests::Net,
}

async fn gated_claim_fixture() -> GatedClaimFixture {
    use crate::control::claims::tests::Net;
    let rig = rig();
    wire_fixture_document(&rig, json!(["600"]));
    let owner = Arc::new(rig.owner("claims-recovery"));
    rig.stamp(&owner);
    let handle = channel_handle();
    let registry = Arc::new(crate::Registry::default());
    let forwarding = Arc::new(crate::ForwardingTable::default());
    let supervisor = Supervisor::new_for_test(registry.clone(), RestartPolicy::default())
        .with_handle(handle.clone())
        .with_forwarding(forwarding.clone())
        .with_config_runtime(owner.clone());
    let module = supervisor.spawn_async(spec(&rig)).await.unwrap();
    let proof = handle.registration_proof_for_test("a").unwrap();
    assert!(handle.wait_program_binding("a", &proof).await);
    let spawn = handle.registration_for_test("a").unwrap();
    spawn.wait_launch().await;
    let control = Arc::new(
        crate::ControlHandler::with_forwarding(registry, forwarding)
            .with_supervisor(handle.clone())
            .with_config_runtime(owner.clone()),
    );
    let router = Arc::new(crate::Router::with_control_handler(control.clone()));
    let mut primary = Net::new(router.clone()).await;
    let ack = primary
        .call(module_hello(Some(proof), ConnectionId::new(1)))
        .await;
    assert_eq!(
        ack.header.ty,
        subc_protocol::FrameType::HelloAck,
        "{}",
        String::from_utf8_lossy(&ack.body)
    );
    GatedClaimFixture {
        rig,
        owner,
        supervisor,
        handle,
        module,
        spawn,
        router,
        handler: control,
        primary,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn claims_mint_obeys_recovery_and_resumes_after_approved_exit() {
    use crate::control::claims::tests::{error, request, Net};
    use subc_control::{ClaimMintRequest, ClaimPurpose};
    let mut fixture = gated_claim_fixture().await;
    let token = gated_claim_mint(&mut fixture.primary).await;
    std::fs::write(
        fixture
            .rig
            .path
            .join("store")
            .join(crate::config_store::STORE_FILE),
        b"tampered",
    )
    .unwrap();
    assert!(fixture.owner.check_recovery().unwrap());
    error(
        &fixture
            .primary
            .call(request(ClaimMintRequest::new(ClaimPurpose::SelfConnection)))
            .await,
        "config_gated",
    );
    let checks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = checks.clone();
    fixture.spawn.set_content_probe_for_test(Arc::new(move || {
        observed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }));
    for _ in 0..2 {
        let mut consumer = Net::new(fixture.router.clone()).await;
        error(
            &consumer
                .call(request(subc_control::ClaimPresentRequest::new(
                    token.clone(),
                )))
                .await,
            "config_gated",
        );
        assert!(consumer.recv().await.is_none());
        assert!(
            fixture.handler.claim_pending_for_test(&token),
            "recovery refusal must preserve an unexpired bearer"
        );
        assert!(!fixture.handler.claim_reserved_for_test(&token));
    }
    assert_eq!(
        checks.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "recovery refusals must not verify executable content"
    );
    fixture.handler.expire_claim_for_test(&token);
    let mut expired = Net::new(fixture.router.clone()).await;
    error(
        &expired
            .call(request(subc_control::ClaimPresentRequest::new(
                token.clone(),
            )))
            .await,
        "config_gated",
    );
    assert!(expired.recv().await.is_none());
    assert!(
        !fixture.handler.claim_pending_for_test(&token),
        "recovery refusal must reclaim expired tokens"
    );
    assert_eq!(checks.load(std::sync::atomic::Ordering::Relaxed), 0);
    fixture
        .spawn
        .set_content_probe_for_test(Arc::new(|| Ok(())));
    let generation = approve(&fixture.rig, &fixture.owner, PlanKind::Unstamp);
    fixture.owner.complete_authority_exit(generation).unwrap();
    assert!(!fixture.owner.check_recovery().unwrap());
    drop(fixture.primary);
    fixture.module.stop().await.unwrap();
    let replacement = fixture
        .supervisor
        .spawn_async(spec(&fixture.rig))
        .await
        .unwrap();
    let proof = fixture.handle.registration_proof_for_test("a").unwrap();
    fixture
        .handle
        .registration_for_test("a")
        .unwrap()
        .wait_launch()
        .await;
    let mut primary = Net::new(fixture.router.clone()).await;
    let ping = subc_protocol::Frame::build(
        subc_protocol::FrameType::Ping,
        subc_protocol::Flags::new(false, subc_protocol::Priority::Passive, false),
        0,
        0,
        2,
        Vec::new(),
    )
    .unwrap();
    assert_eq!(
        primary.call(ping).await.header.ty,
        subc_protocol::FrameType::Pong
    );
    let connection = primary.connection_id(&fixture.handler);
    assert_eq!(
        primary
            .call(module_hello(Some(proof), connection))
            .await
            .header
            .ty,
        subc_protocol::FrameType::HelloAck
    );
    assert_eq!(
        primary
            .call(request(ClaimMintRequest::new(ClaimPurpose::Relay)))
            .await
            .header
            .ty,
        subc_protocol::FrameType::Response
    );
    drop(primary);
    replacement.stop().await.unwrap();
}

async fn recovery_during_claim_verification(present: bool) {
    use crate::control::claims::tests::{error, request, Net};
    type Gate = Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>;
    struct Release(Gate);
    impl Drop for Release {
        fn drop(&mut self) {
            *self.0 .0.lock().unwrap() = true;
            self.0 .1.notify_all();
        }
    }
    let mut fixture = gated_claim_fixture().await;
    let token = if present {
        Some(gated_claim_mint(&mut fixture.primary).await)
    } else {
        None
    };
    let gate: Gate = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    let release = Release(gate.clone());
    let reached = Arc::new(tokio::sync::Notify::new());
    let observed = reached.clone();
    fixture.spawn.set_content_probe_for_test(Arc::new(move || {
        observed.notify_one();
        let _ = gate
            .1
            .wait_timeout_while(
                gate.0.lock().unwrap(),
                Duration::from_secs(10),
                |released| !*released,
            )
            .unwrap();
        Ok(())
    }));
    let mut primary = Some(fixture.primary);
    let mut peer = if present {
        Net::new(fixture.router.clone()).await
    } else {
        primary.take().unwrap()
    };
    let frame = if let Some(token) = &token {
        request(subc_control::ClaimPresentRequest::new(token.clone()))
    } else {
        request(subc_control::ClaimMintRequest::new(
            subc_control::ClaimPurpose::SelfConnection,
        ))
    };
    peer.send(frame).await;
    tokio::time::timeout(Duration::from_secs(2), reached.notified())
        .await
        .expect("claim must reach its held verification after initial admission");
    std::fs::write(
        fixture
            .rig
            .path
            .join("store")
            .join(crate::config_store::STORE_FILE),
        b"tampered",
    )
    .unwrap();
    assert!(fixture.owner.check_recovery().unwrap());
    drop(release);
    error(&peer.recv().await.unwrap(), "config_gated");
    if let Some(token) = token {
        assert!(peer.recv().await.is_none());
        assert!(
            fixture.handler.claim_pending_for_test(&token),
            "recovery after verification must not consume the bearer"
        );
        assert!(!fixture.handler.claim_reserved_for_test(&token));
    } else {
        assert_eq!(
            fixture.spawn.claims.get().unwrap().pending_count_for_test(),
            0
        );
    }
    drop(peer);
    drop(primary);
    fixture.module.stop().await.unwrap();
}

#[tokio::test]
async fn claims_present_rechecks_recovery_after_content_verification() {
    recovery_during_claim_verification(true).await;
}

#[tokio::test]
async fn claims_mint_rechecks_recovery_after_content_verification() {
    recovery_during_claim_verification(false).await;
}

async fn gated_claim_mint(primary: &mut crate::control::claims::tests::Net) -> subc_control::Claim {
    use crate::control::claims::tests::request;
    let reply = primary
        .call(request(subc_control::ClaimMintRequest::new(
            subc_control::ClaimPurpose::SelfConnection,
        )))
        .await;
    assert_eq!(reply.header.ty, subc_protocol::FrameType::Response);
    serde_json::from_slice::<subc_control::ClaimMintReply>(&reply.body)
        .unwrap()
        .claim
}

#[tokio::test]
async fn claims_expired_representations_are_removed_without_content_checks() {
    use crate::control::claims::tests::{error, request, Net};
    let mut fixture = gated_claim_fixture().await;
    let token = gated_claim_mint(&mut fixture.primary).await;
    let checks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = checks.clone();
    fixture.spawn.set_content_probe_for_test(Arc::new(move || {
        observed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }));
    // Move only the stored monotonic expiry; the production lifetime and the
    // informational wall-clock timestamp remain unchanged.
    fixture.handler.expire_claim_for_test(&token);
    for _ in 0..2 {
        let mut consumer = Net::new(fixture.router.clone()).await;
        error(
            &consumer
                .call(request(subc_control::ClaimPresentRequest::new(
                    token.clone(),
                )))
                .await,
            "claim_invalid",
        );
        assert!(consumer.recv().await.is_none());
    }
    assert_eq!(
        checks.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "expired tokens must be rejected before executable hashing"
    );
    assert!(
        !fixture.handler.claim_pending_for_test(&token),
        "expired token must leave the global index"
    );
    assert_eq!(
        fixture.spawn.claims.get().unwrap().pending_count_for_test(),
        0
    );
    drop(fixture.primary);
    fixture.module.stop().await.unwrap();
}

#[tokio::test]
async fn claims_rate_refusal_precedes_content_checks() {
    use crate::control::claims::tests::{error, request};
    let mut fixture = gated_claim_fixture().await;
    gated_claim_mint(&mut fixture.primary).await;
    let checks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = checks.clone();
    fixture.spawn.set_content_probe_for_test(Arc::new(move || {
        observed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }));
    fixture
        .handler
        .exhaust_claim_rate_for_test(ConnectionId::new(1));
    error(
        &fixture
            .primary
            .call(request(subc_control::ClaimMintRequest::new(
                subc_control::ClaimPurpose::SelfConnection,
            )))
            .await,
        "claim_rate_limited",
    );
    assert_eq!(
        checks.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "rate-limited mint must not hash the executable"
    );
    drop(fixture.primary);
    fixture.module.stop().await.unwrap();
}

#[tokio::test]
async fn claims_mint_and_present_content_checks_use_blocking_workers() {
    use crate::control::claims::tests::{request, Net};
    let mut fixture = gated_claim_fixture().await;
    let threads = Arc::new(std::sync::Mutex::new(Vec::new()));
    let observed = threads.clone();
    // The current-thread runtime runs the test driver and socket readers on
    // one thread, so a different ID identifies an actual blocking worker.
    let reader = std::thread::current().id();
    fixture.spawn.set_content_probe_for_test(Arc::new(move || {
        observed.lock().unwrap().push(std::thread::current().id());
        Ok(())
    }));
    let token = gated_claim_mint(&mut fixture.primary).await;
    let mut consumer = Net::new(fixture.router.clone()).await;
    assert_eq!(
        consumer
            .call(request(subc_control::ClaimPresentRequest::new(token)))
            .await
            .header
            .ty,
        subc_protocol::FrameType::Response
    );
    let checked = threads.lock().unwrap().clone();
    assert!(
        checked.len() >= 2,
        "both mint and present must verify live content"
    );
    assert!(
        checked.iter().all(|thread| *thread != reader),
        "content verification must run off the async reader"
    );
    drop(consumer);
    drop(fixture.primary);
    fixture.module.stop().await.unwrap();
}

#[tokio::test]
async fn claims_cancelled_presentation_keeps_reservation_until_content_worker_finishes() {
    use crate::control::claims::tests::{error, request, Net};
    type Gate = Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>;
    struct Release(Gate);
    impl Drop for Release {
        fn drop(&mut self) {
            *self.0 .0.lock().unwrap() = true;
            self.0 .1.notify_all();
        }
    }
    let mut fixture = gated_claim_fixture().await;
    let token = gated_claim_mint(&mut fixture.primary).await;
    let gate: Gate = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    let release = Release(gate.clone());
    let reached = Arc::new(tokio::sync::Notify::new());
    let observed = reached.clone();
    let hold = gate.clone();
    fixture.spawn.set_content_probe_for_test(Arc::new(move || {
        observed.notify_one();
        let _ = hold
            .1
            .wait_timeout_while(
                hold.0.lock().unwrap(),
                Duration::from_secs(10),
                |released| !*released,
            )
            .unwrap();
        Ok(())
    }));
    let mut first = Net::new(fixture.router.clone()).await;
    first
        .send(request(subc_control::ClaimPresentRequest::new(
            token.clone(),
        )))
        .await;
    tokio::time::timeout(Duration::from_secs(2), reached.notified())
        .await
        .expect("content worker must reach the held check");
    fixture.handler.forwarding().request_connection_close(
        ConnectionId::new(2),
        crate::forwarding::CloseReason::new("test_cancel", "cancel pending claim check"),
    );
    assert!(first.recv().await.is_none());
    assert!(
        fixture.handler.claim_reserved_for_test(&token),
        "disconnect must not release a running content worker's reservation"
    );
    let mut duplicate = Net::new(fixture.router.clone()).await;
    error(
        &duplicate
            .call(request(subc_control::ClaimPresentRequest::new(
                token.clone(),
            )))
            .await,
        "claim_invalid",
    );
    assert!(duplicate.recv().await.is_none());
    drop(release);
    tokio::time::timeout(Duration::from_secs(2), async {
        while fixture.handler.claim_reserved_for_test(&token) {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("finished worker must release its reservation");
    fixture
        .spawn
        .set_content_probe_for_test(Arc::new(|| Ok(())));
    let mut retry = Net::new(fixture.router.clone()).await;
    assert_eq!(
        retry
            .call(request(subc_control::ClaimPresentRequest::new(token)))
            .await
            .header
            .ty,
        subc_protocol::FrameType::Response
    );
    drop(retry);
    drop(fixture.primary);
    fixture.module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn claims_live_program_refusal_revokes_before_kill_or_exit_observation() {
    use crate::control::claims::tests::{error, request, Net};
    let mut fixture = gated_claim_fixture().await;
    let token = gated_claim_mint(&mut fixture.primary).await;
    let pending = gated_claim_mint(&mut fixture.primary).await;
    let mut consumer = Net::new(fixture.router.clone()).await;
    assert_eq!(
        consumer
            .call(request(subc_control::ClaimPresentRequest::new(token)))
            .await
            .header
            .ty,
        subc_protocol::FrameType::Response
    );
    // Only the test build can postpone the kill. Dropping this guard signals the
    // original container even if an assertion panics, before fixture teardown.
    let kill = fixture.spawn.defer_content_kill_for_test();
    fixture.spawn.set_content_probe_for_test(Arc::new(|| {
        Err(GateError::new("program_refused", "content witness changed"))
    }));
    error(
        &fixture
            .primary
            .call(request(subc_control::ClaimMintRequest::new(
                subc_control::ClaimPurpose::Helper,
            )))
            .await,
        "claim_mint_not_allowed",
    );
    assert!(
        !fixture.spawn.launch_ready(),
        "permanent refusal must retire registration before killing the process"
    );
    assert!(
        fixture.module.status().unwrap().process_alive,
        "the child must still be alive while claims are revoked"
    );
    assert!(
        consumer.recv().await.is_none(),
        "content refusal must close claim-bound connections before exit"
    );
    let mut stale = Net::new(fixture.router.clone()).await;
    error(
        &stale
            .call(request(subc_control::ClaimPresentRequest::new(pending)))
            .await,
        "claim_invalid",
    );
    assert!(stale.recv().await.is_none());
    assert!(fixture.module.status().unwrap().process_alive);
    drop(kill);
    drop(fixture.primary);
    fixture.module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn claims_transient_content_deferral_does_not_retire_live_registration() {
    use crate::control::claims::tests::{error, request, Net};
    let mut fixture = gated_claim_fixture().await;
    let token = gated_claim_mint(&mut fixture.primary).await;
    let mut consumer = Net::new(fixture.router.clone()).await;
    assert_eq!(
        consumer
            .call(request(subc_control::ClaimPresentRequest::new(token)))
            .await
            .header
            .ty,
        subc_protocol::FrameType::Response
    );
    fixture.spawn.set_content_probe_for_test(Arc::new(|| {
        Err(GateError::new(
            "config_delivery_deferred",
            "content witness temporarily busy",
        ))
    }));
    error(
        &fixture
            .primary
            .call(request(subc_control::ClaimMintRequest::new(
                subc_control::ClaimPurpose::Helper,
            )))
            .await,
        "claim_mint_not_allowed",
    );
    assert!(
        fixture.spawn.launch_ready(),
        "transient content deferral must not retire registration"
    );
    assert!(!fixture.spawn.content_was_refused_for_test());
    assert!(fixture.module.status().unwrap().process_alive);
    let ping = subc_protocol::Frame::build(
        subc_protocol::FrameType::Ping,
        subc_protocol::Flags::new(false, subc_protocol::Priority::Passive, false),
        0,
        0,
        9,
        Vec::new(),
    )
    .unwrap();
    assert_eq!(
        consumer.call(ping).await.header.ty,
        subc_protocol::FrameType::Pong
    );
    fixture
        .spawn
        .set_content_probe_for_test(Arc::new(|| Ok(())));
    drop(consumer);
    drop(fixture.primary);
    fixture.module.stop().await.unwrap();
}

fn rig() -> Rig {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/scratch")
        .join(format!(
            "admission-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
    std::fs::create_dir_all(path.join("store")).unwrap();
    std::fs::create_dir_all(path.join("cortexkit")).unwrap();
    std::fs::copy("/bin/sleep", path.join("program")).unwrap();
    #[cfg(target_os = "macos")]
    assert!(
        std::process::Command::new("/usr/bin/codesign")
            .args(["--force", "--sign", "-"])
            .arg(path.join("program"))
            .status()
            .unwrap()
            .success(),
        "a copied Apple platform binary needs a local test signature before exec"
    );
    std::fs::write(path.join("subc.jsonc"), serde_json::to_vec(&json!({"version":1,"modules":{"a":{"program":path.join("program"),"args":["600"],"protocol":"none"}}})).unwrap()).unwrap();
    let declaration = ConfigDeclareRequest::new(
        json!({"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","unevaluatedProperties":false,"properties":{"limit":{"type":"integer","description":"Limit","default":1}},"x-ck-layers":["user"],"x-ck-apply":"live"}),
        path.join("cortexkit/a.jsonc").to_string_lossy(),
        vec![],
    );
    Rig { path, declaration }
}

fn spec(rig: &Rig) -> ModuleSpec {
    crate::daemon_config::parse_with_activity(
        &std::fs::read_to_string(rig.path.join("subc.jsonc")).unwrap(),
        rig.path.join("subc.jsonc"),
    )
    .unwrap()
    .0
    .modules[0]
        .module_spec()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gated_nonce_policy_stamp_between_agent_validation_and_commit_refuses_wire_declaration() {
    let rig = rig();
    std::fs::write(
        rig.path.join("subc.jsonc"),
        serde_json::to_vec(&json!({"version":1,"modules":{"a":{
        "program":rig.path.join("program"),"args":["600"],"protocol":"subc"}}}))
        .unwrap(),
    )
    .unwrap();
    let owner = Arc::new(rig.owner("resource-stamp-race"));
    let registry = Arc::new(crate::Registry::default());
    let handle = SupervisorHandle::new();
    let supervisor = Supervisor::new_for_test(registry.clone(), RestartPolicy::default())
        .with_handle(handle.clone())
        .with_config_runtime(owner.clone());
    let module = supervisor.supervise_configured(spec(&rig), true).unwrap();
    let nonce = handle.spawn_launch_nonce_for("a").unwrap();
    let control = crate::ControlHandler::new(registry)
        .with_supervisor(handle.clone())
        .with_config_runtime(owner.clone());
    let router = Arc::new(crate::Router::with_control_handler(Arc::new(control)));
    let info = subc_transport::ConnectionInfo {
        schema: subc_transport::SCHEMA_VERSION,
        wire_version: None,
        endpoints: vec![],
        key: vec![0x42; 32],
        daemon_id: [0x24; 16],
        pid: std::process::id(),
        daemon_ver: "test".into(),
    };
    let auth = crate::server::ServerAuth::new(info.key.clone(), info.daemon_id, "test");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(crate::server::serve_listener(listener, router, auth));
    let mut peer = tokio::net::TcpStream::connect(address).await.unwrap();
    subc_transport::authenticate_client(&mut peer, &info, Duration::from_secs(2))
        .await
        .unwrap();
    let hello = subc_protocol::ModuleHelloBody {
        manifest: subc_protocol::manifest::ModuleManifest::builder("a", "1.0.0").build(),
        protocol_ver: subc_protocol::PROTOCOL_VERSION,
        control_ops: None,
        launch_nonce: Some(nonce),
    };
    let frame = |ty, corr, body| {
        subc_protocol::Frame::build(
            ty,
            subc_protocol::Flags::new(false, subc_protocol::Priority::Interactive, false),
            0,
            0,
            corr,
            body,
        )
        .unwrap()
    };
    subc_transport::write_frame(
        &mut peer,
        &frame(
            subc_protocol::FrameType::Hello,
            1,
            serde_json::to_vec(&hello).unwrap(),
        ),
    )
    .await
    .unwrap();
    assert_eq!(
        subc_transport::read_frame(&mut peer)
            .await
            .unwrap()
            .unwrap()
            .header
            .ty,
        subc_protocol::FrameType::HelloAck
    );
    tokio::time::timeout(Duration::from_secs(2), async {
        while !module
            .resource_launch()
            .is_some_and(|launch| launch.alive && launch.enabled && launch.pid.is_some())
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("module must publish its confirmed live launch before declaring commands");
    let (reached, resume) = handle.pause_resource_commit_for_test();
    subc_transport::write_frame(
        &mut peer,
        &frame(
            subc_protocol::FrameType::Request,
            2,
            serde_json::to_vec(
                &json!({"op":"supervisor.agent_commands", "started":[], "finished":[]}),
            )
            .unwrap(),
        ),
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(2), reached)
        .await
        .expect("declaration must reach the commit fence")
        .unwrap();
    rig.stamp(&owner);
    resume.send(()).unwrap();
    let reply = tokio::time::timeout(
        Duration::from_secs(2),
        subc_transport::read_frame(&mut peer),
    )
    .await
    .unwrap()
    .unwrap()
    .unwrap();
    drop(peer);
    module.stop().await.unwrap();
    server.abort();
    assert_eq!(
        reply.header.ty,
        subc_protocol::FrameType::Error,
        "a new stamp must fence a validated but uncommitted nonce declaration"
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&reply.body).unwrap()["code"],
        "not_supervised"
    );
}

async fn stamped_nonce_hello(
    owner: Arc<RuntimeOwner>,
    handle: SupervisorHandle,
    module: &str,
    nonce: Option<&str>,
) -> subc_protocol::Frame {
    let control = crate::ControlHandler::new(Arc::new(crate::Registry::default()))
        .with_supervisor(handle)
        .with_config_runtime(owner);
    let router = Arc::new(crate::Router::with_control_handler(Arc::new(control)));
    let info = subc_transport::ConnectionInfo {
        schema: subc_transport::SCHEMA_VERSION,
        wire_version: None,
        endpoints: vec![],
        key: vec![0x42; 32],
        daemon_id: [0x24; 16],
        pid: std::process::id(),
        daemon_ver: "test".into(),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(crate::server::serve_listener(
        listener,
        router,
        crate::server::ServerAuth::new(info.key.clone(), info.daemon_id, "test"),
    ));
    let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
    subc_transport::authenticate_client(&mut client, &info, Duration::from_secs(2))
        .await
        .unwrap();
    let body = subc_protocol::ModuleHelloBody {
        manifest: subc_protocol::manifest::ModuleManifest::builder(module, "1.0.0").build(),
        protocol_ver: subc_protocol::PROTOCOL_VERSION,
        control_ops: None,
        launch_nonce: nonce.map(str::to_owned),
    };
    let frame = subc_protocol::Frame::build(
        subc_protocol::FrameType::Hello,
        subc_protocol::Flags::new(false, subc_protocol::Priority::Interactive, false),
        0,
        0,
        1,
        serde_json::to_vec(&body).unwrap(),
    )
    .unwrap();
    subc_transport::write_frame(&mut client, &frame)
        .await
        .unwrap();
    let reply = subc_transport::read_frame(&mut client)
        .await
        .unwrap()
        .unwrap();
    server.abort();
    reply
}

#[tokio::test]
async fn gated_nonce_policy_unconfigured_standalone_hello_remains_legacy() {
    let rig = rig();
    let owner = Arc::new(rig.owner("standalone-nonce-policy"));
    rig.stamp(&owner);
    let handle = SupervisorHandle::new();
    let _supervisor = Supervisor::new_for_test(
        Arc::new(crate::Registry::default()),
        RestartPolicy::default(),
    )
    .with_handle(handle.clone())
    .with_config_runtime(owner.clone());
    let reply = stamped_nonce_hello(owner, handle, "standalone", None).await;
    assert_eq!(
        reply.header.ty,
        subc_protocol::FrameType::HelloAck,
        "an id absent from both configured authority and approval cache stays unreserved legacy"
    );
}

#[tokio::test]
async fn gated_nonce_policy_configured_uncached_stopped_nonce_hello_is_refused() {
    let rig = rig();
    let owner = Arc::new(rig.owner("stopped-nonce-policy"));
    rig.stamp(&owner);
    let handle = SupervisorHandle::new();
    let supervisor = Supervisor::new_for_test(
        Arc::new(crate::Registry::default()),
        RestartPolicy::default(),
    )
    .with_handle(handle.clone())
    .with_config_runtime(owner.clone());
    let mut missing = spec(&rig);
    missing.module_id = "stopped-uncached".into();
    missing.protocol = subc_control::ModuleProtocol::Subc;
    missing.reserved = true;
    let _configured = supervisor
        .register_configured_with_health(
            missing,
            false,
            crate::supervise::HealthConfig::default(),
            None,
            RestartPolicy::default(),
            Duration::from_secs(5),
        )
        .unwrap();
    handle.set_spawn_nonce("stopped-uncached", "old-nonce".into());
    handle.set_reserved_nonce("stopped-uncached", "old-nonce".into());
    let reply = stamped_nonce_hello(owner, handle, "stopped-uncached", Some("old-nonce")).await;
    assert_eq!(
        reply.header.ty,
        subc_protocol::FrameType::Error,
        "absence from approval cache must not ungate a configured stopped id"
    );
}

#[tokio::test]
async fn gated_nonce_policy_refuses_stopped_uncached_identity_over_real_server() {
    let rig = rig();
    let owner = Arc::new(rig.owner("nonce-policy"));
    rig.stamp(&owner);
    let registry = Arc::new(crate::Registry::default());
    let handle = SupervisorHandle::new();
    let supervisor = Supervisor::new_for_test(registry.clone(), RestartPolicy::default())
        .with_handle(handle.clone())
        .with_config_runtime(owner.clone());
    let mut missing = spec(&rig);
    missing.module_id = "callosum".into();
    missing.protocol = subc_control::ModuleProtocol::Subc;
    missing.reserved = true;
    let _configured = supervisor
        .register_configured_with_health(
            missing,
            false,
            crate::supervise::HealthConfig::default(),
            None,
            RestartPolicy::default(),
            Duration::from_secs(5),
        )
        .unwrap();
    handle.set_spawn_nonce("callosum", "stale-before-stamp".into());
    handle.set_reserved_nonce("callosum", "stale-before-stamp".into());
    let control = crate::ControlHandler::new(registry)
        .with_supervisor(handle.clone())
        .with_config_runtime(owner.clone())
        .with_activity_ledger(&rig.path, "nonce-policy".into());
    let router = Arc::new(crate::Router::with_control_handler(Arc::new(control)));
    let info = subc_transport::ConnectionInfo {
        schema: subc_transport::SCHEMA_VERSION,
        wire_version: None,
        endpoints: vec![],
        key: vec![0x42; 32],
        daemon_id: [0x24; 16],
        pid: std::process::id(),
        daemon_ver: "test".into(),
    };
    let auth = crate::server::ServerAuth::new(info.key.clone(), info.daemon_id, "test");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(crate::server::serve_listener(listener, router, auth));
    let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
    subc_transport::authenticate_client(&mut client, &info, Duration::from_secs(2))
        .await
        .unwrap();
    let query = subc_protocol::Frame::build(
        subc_protocol::FrameType::Request,
        subc_protocol::Flags::new(false, subc_protocol::Priority::Interactive, false),
        0,
        0,
        1,
        serde_json::to_vec(&json!({"op":"activity.query", "limit":1,
            "consumer_identity":{"module_id":"callosum", "launch_nonce":"stale-before-stamp"}}))
        .unwrap(),
    )
    .unwrap();
    subc_transport::write_frame(&mut client, &query)
        .await
        .unwrap();
    let response = subc_transport::read_frame(&mut client)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(response.header.ty, subc_protocol::FrameType::Error,
        "a stopped configured id missing from approval cache cannot redeem its old nonce for activity");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&response.body).unwrap()["code"],
        "activity_read_forbidden"
    );
    let hello = subc_protocol::ModuleHelloBody {
        manifest: subc_protocol::manifest::ModuleManifest::builder("callosum", "1.0.0").build(),
        protocol_ver: subc_protocol::PROTOCOL_VERSION,
        control_ops: None,
        launch_nonce: Some("stale-before-stamp".into()),
    };
    let hello = subc_protocol::Frame::build(
        subc_protocol::FrameType::Hello,
        subc_protocol::Flags::new(false, subc_protocol::Priority::Interactive, false),
        0,
        0,
        2,
        serde_json::to_vec(&hello).unwrap(),
    )
    .unwrap();
    subc_transport::write_frame(&mut client, &hello)
        .await
        .unwrap();
    assert_eq!(
        subc_transport::read_frame(&mut client)
            .await
            .unwrap()
            .unwrap()
            .header
            .ty,
        subc_protocol::FrameType::Error,
        "uncached gated ids also refuse nonce HELLO"
    );
    server.abort();
}

async fn reported_pid(module: &crate::supervise::SupervisedModule) -> u32 {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(pid) = module.status().unwrap().pid {
                return pid;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the owned child must cross its exec acknowledgement before observation")
}

fn approve(_rig: &Rig, owner: &RuntimeOwner, kind: PlanKind) -> u64 {
    let plan = owner
        .finish_plan(owner.prepare(kind).unwrap(), daemon_review)
        .unwrap();
    owner
        .desktop_view_delivered(ConnectionId::new(7), &plan)
        .unwrap();
    let challenge = owner.begin_approval(&plan.digest, &Rig::facts()).unwrap();
    owner
        .finish_approval(
            challenge.attempt(),
            &Rig::facts(),
            ApprovalReply::Presence(super::super::super::PresenceDecision::InteractiveApproval),
            2,
        )
        .unwrap()
}

#[test]
fn unix_channel_gated_admission_withholds_legacy_nonce() {
    let rig = rig();
    let owner = rig.owner("channel-with-legacy-nonce");
    rig.stamp(&owner);
    let spec = spec(&rig);
    let admission = owner.prepare_spawn(&spec).unwrap();
    crate::supervise::unix_channel_placement_tests::assert_channel_only_admission(
        &spec, &admission,
    );
}

#[test]
fn config_admission_changed_binary_is_held_and_refused_once_per_identity() {
    let rig = rig();
    let owner = rig.owner("binary");
    rig.stamp(&owner);
    let spec = spec(&rig);
    assert!(owner.prepare_spawn(&spec).is_ok());
    std::fs::copy(&spec.program, rig.path.join("replacement")).unwrap();
    std::fs::rename(rig.path.join("replacement"), &spec.program).unwrap();
    assert_eq!(
        owner.prepare_spawn(&spec).err().unwrap().code,
        "program_refused"
    );
    for _ in 0..3 {
        assert_eq!(
            owner.prepare_spawn(&spec).err().unwrap().code,
            "config_gated"
        );
    }
    assert_eq!(
        owner
            .take_activities()
            .iter()
            .filter(|activity| activity.event == "config.refused")
            .count(),
        1
    );
    drop(owner);
    let owner = rig.owner("binary-restarted");
    assert_eq!(
        owner.prepare_spawn(&spec).err().unwrap().code,
        "config_gated"
    );
    assert_eq!(
        owner
            .take_activities()
            .iter()
            .filter(|activity| activity.event == "config.refused")
            .count(),
        0
    );
    assert_eq!(owner.status().unwrap().pending_proposals, 1);
    assert_eq!(owner.status().unwrap().gated_modules, ["a"]);
    approve(&rig, &owner, PlanKind::Change);
    assert!(owner
        .lock()
        .unwrap()
        .store
        .body()
        .unwrap()
        .program_refusals
        .is_empty());
    assert!(owner.prepare_spawn(&spec).is_ok());
    assert!(owner.status().unwrap().gated_modules.is_empty());
}

#[test]
fn config_admission_refusal_does_not_return_after_a_to_b_to_a_approval() {
    let rig = rig();
    let owner = rig.owner("refusal-a-b-a");
    rig.stamp(&owner);
    let path = rig.path.join("program");
    let original = std::fs::read(&path).unwrap();
    let original_spec = spec(&rig);
    std::fs::write(&path, b"unapproved content").unwrap();
    assert_eq!(
        owner.prepare_spawn(&original_spec).err().unwrap().code,
        "program_refused"
    );
    let mut changed = original.clone();
    changed.extend_from_slice(b"different program approval bytes");
    std::fs::write(&path, &changed).unwrap();
    approve(&rig, &owner, PlanKind::Change);
    assert!(
        owner.lock().unwrap().refused_generations.is_empty(),
        "approving B must retire A's volatile refusal"
    );
    std::fs::write(&path, &original).unwrap();
    approve(&rig, &owner, PlanKind::Change);
    assert!(
        owner.prepare_spawn(&original_spec).is_ok(),
        "approving the original A must not resurrect its old refusal"
    );
    approve(&rig, &owner, PlanKind::Unstamp);
    assert!(owner.lock().unwrap().refused_generations.is_empty());
}

#[test]
fn config_admission_transient_capture_failure_defers_without_a_durable_refusal() {
    let rig = rig();
    let owner = rig.owner("capture-unavailable");
    rig.stamp(&owner);
    let program = rig.path.join("program");
    let saved = rig.path.join("temporarily-unavailable");
    std::fs::rename(&program, &saved).unwrap();
    let result = owner.prepare_spawn(&spec(&rig));
    std::fs::rename(&saved, &program).unwrap();
    assert_eq!(result.err().unwrap().code, "config_delivery_deferred");
    assert!(owner
        .lock()
        .unwrap()
        .store
        .body()
        .unwrap()
        .program_refusals
        .is_empty());
    assert!(owner.prepare_spawn(&spec(&rig)).is_ok());
}

#[test]
fn config_admission_capture_classification_approved_symlink_is_unchanged() {
    let rig = rig();
    let link = rig.path.join("program-link");
    std::os::unix::fs::symlink(rig.path.join("program"), &link).unwrap();
    let (_, input) = crate::config_gate::capture::capture_program(&link).unwrap();
    let approved = std::collections::BTreeMap::from([(
        crate::config_gate::InputKey::Program {
            module: "a".into(),
            path: link.to_string_lossy().into_owned(),
        },
        input,
    )]);
    assert!(
        !crate::config_gate::program::definite_program_change(&approved),
        "a previously approved symlink is not a new mismatch when capture temporarily fails"
    );
}

#[test]
fn config_admission_capture_classification_over_limit_is_held() {
    let rig = rig();
    let owner = rig.owner("over-limit-program");
    rig.stamp(&owner);
    std::fs::OpenOptions::new()
        .write(true)
        .open(rig.path.join("program"))
        .unwrap()
        .set_len(512 * 1024 * 1024 + 1)
        .unwrap();
    assert_eq!(
        owner.prepare_spawn(&spec(&rig)).err().unwrap().code,
        "program_refused"
    );
    assert_eq!(
        owner.check_module_hold("a").unwrap_err().code,
        "config_gated"
    );
    assert_eq!(
        owner
            .lock()
            .unwrap()
            .store
            .body()
            .unwrap()
            .program_refusals
            .len(),
        1
    );
}

#[test]
#[cfg(target_os = "macos")]
fn config_admission_capture_classification_malformed_macho_is_held() {
    let rig = rig();
    let owner = rig.owner("malformed-program");
    rig.stamp(&owner);
    std::fs::write(rig.path.join("program"), [0xcf, 0xfa, 0xed, 0xfe]).unwrap();
    assert_eq!(
        owner.prepare_spawn(&spec(&rig)).err().unwrap().code,
        "program_refused"
    );
    assert_eq!(
        owner.check_module_hold("a").unwrap_err().code,
        "config_gated"
    );
    assert_eq!(
        owner
            .lock()
            .unwrap()
            .store
            .body()
            .unwrap()
            .program_refusals
            .len(),
        1
    );
}

#[test]
fn config_admission_refusal_from_an_old_base_cannot_hold_the_new_approval() {
    let rig = rig();
    let owner = rig.owner("stale-refusal");
    rig.stamp(&owner);
    let old_base = crate::config_gate::program::approved_base(
        &crate::config_gate::GateSnapshot::from_store(
            &owner.lock().unwrap().store.body().unwrap().approved,
        )
        .unwrap(),
        "a",
    );
    let path = rig.path.join("program");
    let mut changed = std::fs::read(&path).unwrap();
    changed.extend_from_slice(b"different program approval bytes");
    std::fs::write(path, changed).unwrap();
    approve(&rig, &owner, PlanKind::Change);
    owner
        .record_program_refusal(
            "a",
            &old_base,
            subc_config::raw_digest(b"old failing image"),
        )
        .unwrap();
    assert!(
        owner.prepare_spawn(&spec(&rig)).is_ok(),
        "a refusal checked under {old_base:?} must not hold a different approved base"
    );
    assert!(owner
        .lock()
        .unwrap()
        .store
        .body()
        .unwrap()
        .program_refusals
        .is_empty());
}

#[test]
fn config_admission_program_only_approval_of_a_held_module_needs_no_live_reviewer() {
    use std::io::Write;
    let rig = rig();
    let owner = rig.owner("held-program-review");
    rig.declare(&owner, 10);
    rig.stamp(&owner);
    std::fs::OpenOptions::new()
        .append(true)
        .open(rig.path.join("program"))
        .unwrap()
        .write_all(b"new approved executable content")
        .unwrap();
    assert_eq!(
        owner.prepare_spawn(&spec(&rig)).err().unwrap().code,
        "program_refused"
    );
    let prepared = owner.prepare(PlanKind::Change).unwrap();
    assert!(
        !prepared.review_modules.contains("a"),
        "a stopped program-only hold cannot answer its own review RPC"
    );
}

#[test]
fn config_admission_missing_approved_command_never_falls_back_to_the_live_spec() {
    let rig = rig();
    let owner = rig.owner("no-command");
    let generation = rig.stamp(&owner);
    owner
        .lock()
        .unwrap()
        .store
        .clear_delivery_snapshot_for_test(generation);
    let mut live = spec(&rig);
    live.args = vec!["unapproved live arguments".into()];
    assert_eq!(
        owner.prepare_spawn(&live).err().unwrap().code,
        "config_gated"
    );
}

#[tokio::test]
async fn config_admission_spawn_rehashes_scripts_and_covered_files_after_preparation() {
    use std::io::Write;
    for script_changed in [false, true] {
        let rig = rig();
        std::fs::write(
            rig.path.join("program"),
            "#!/bin/sh\nwhile :; do /bin/sleep 600; done\n",
        )
        .unwrap();
        let covered = rig.path.join("covered");
        std::fs::write(&covered, b"approved").unwrap();
        let mut doc: serde_json::Value =
            serde_json::from_slice(&std::fs::read(rig.path.join("subc.jsonc")).unwrap()).unwrap();
        doc["modules"]["a"]["program_files"] = json!([covered]);
        std::fs::write(
            rig.path.join("subc.jsonc"),
            serde_json::to_vec(&doc).unwrap(),
        )
        .unwrap();
        let owner = Arc::new(rig.owner("spawn-double-check"));
        rig.stamp(&owner);
        let path = if script_changed {
            rig.path.join("program")
        } else {
            covered
        };
        owner.set_spawn_preflight_hook_for_test(move || {
            std::fs::OpenOptions::new()
                .append(true)
                .open(path)
                .unwrap()
                .write_all(b"\n# changed after preparation\n")
                .unwrap()
        });
        let supervisor = Supervisor::new_for_test(
            Arc::new(crate::Registry::default()),
            RestartPolicy::default(),
        )
        .with_config_runtime(owner);
        let module = supervisor.spawn_async(spec(&rig)).await.unwrap();
        let status = module.status().unwrap();
        module.stop().await.unwrap();
        assert_eq!(status.state, ModuleState::Stopped);
        assert_eq!(status.spawn_generation, 0);
        assert_eq!(status.restart_count, 0);
    }
}

#[test]
fn config_admission_stamped_rescan_refuses_before_applying_a_live_file_spec() {
    let rig = rig();
    let owner = rig.owner("rescan-authority");
    rig.stamp(&owner);
    wire_fixture_document(&rig, json!(["unapproved arguments"]));
    assert_eq!(
        owner.checked_admission_hold(None).unwrap_err().code,
        "config_gated"
    );
    assert_eq!(owner.status().unwrap().pending_proposals, 1);
}

#[tokio::test]
async fn config_admission_update_configuration_cannot_install_an_unapproved_spec() {
    let rig = rig();
    let owner = Arc::new(rig.owner("update-authority"));
    rig.stamp(&owner);
    let original = spec(&rig);
    let supervisor = Supervisor::new_for_test(
        Arc::new(crate::Registry::default()),
        RestartPolicy::default(),
    )
    .with_config_runtime(owner);
    let module = supervisor.spawn_async(original.clone()).await.unwrap();
    let mut changed = original.clone();
    changed.args = vec!["unapproved arguments".into()];
    changed.reserved_prefixes = vec!["unapproved".into()];
    let result = module
        .update_configuration(changed, crate::HealthConfig::default(), None)
        .await;
    let after = module.configuration().unwrap().0;
    module.stop().await.unwrap();
    assert!(
        result.is_err(),
        "the internal reconfiguration seam must not bypass stamped authority"
    );
    assert_eq!(after, original);
}

#[test]
fn config_admission_recovery_and_a_missing_binding_cannot_complete_hello() {
    for missing in [false, true] {
        let rig = rig();
        let owner = Arc::new(rig.owner("hello-recovery"));
        rig.stamp(&owner);
        let store = rig.path.join("store").join(crate::config_store::STORE_FILE);
        if missing {
            std::fs::remove_file(&store).unwrap();
        } else {
            let mut bytes = std::fs::read(&store).unwrap();
            bytes.push(b' ');
            std::fs::write(&store, bytes).unwrap();
        }
        assert!(owner.checked_admission_hold(Some("a")).is_err());
        let registry = Arc::new(crate::Registry::default());
        let handle = channel_handle();
        handle.set_spawn_nonce("a", "known-but-unbound".into());
        handle.set_reserved_nonce("a", "known-but-unbound".into());
        let handler = crate::ControlHandler::new(registry.clone())
            .with_supervisor(handle)
            .with_config_runtime(owner);
        let hello = subc_protocol::ModuleHelloBody {
            manifest: subc_protocol::manifest::ModuleManifest::builder("a", "1.0.0").build(),
            protocol_ver: subc_protocol::PROTOCOL_VERSION,
            control_ops: None,
            launch_nonce: Some("known-but-unbound".into()),
        };
        let frame = subc_protocol::Frame::build(
            subc_protocol::FrameType::Hello,
            subc_protocol::Flags::new(false, subc_protocol::Priority::Passive, false),
            0,
            0,
            1,
            serde_json::to_vec(&hello).unwrap(),
        )
        .unwrap();
        let reply = handler
            .handle_control(ConnectionId::new(999), frame)
            .unwrap();
        assert_ne!(
            reply[0].header.ty,
            subc_protocol::FrameType::HelloAck,
            "recovery must not use unbound nonce authority"
        );
        assert!(registry.get_module("a").unwrap().is_none());
    }
}

#[test]
fn config_admission_declared_files_are_covered_but_argument_files_are_not() {
    let rig = rig();
    let declared = rig.path.join("code.cjs");
    let argument = rig.path.join("settings.jsonc");
    std::fs::write(&declared, "approved code").unwrap();
    std::fs::write(&argument, "old settings").unwrap();
    let mut doc: serde_json::Value =
        serde_json::from_slice(&std::fs::read(rig.path.join("subc.jsonc")).unwrap()).unwrap();
    doc["modules"]["a"]["program_files"] = json!([declared]);
    doc["modules"]["a"]["args"] = json!([argument]);
    std::fs::write(
        rig.path.join("subc.jsonc"),
        serde_json::to_vec(&doc).unwrap(),
    )
    .unwrap();
    let owner = rig.owner("files");
    rig.stamp(&owner);
    std::fs::write(argument, "edited settings").unwrap();
    assert!(owner.prepare_spawn(&spec(&rig)).is_ok());
    assert_eq!(owner.status().unwrap().pending_proposals, 0);
    std::fs::write(declared, "unapproved code").unwrap();
    assert_eq!(
        owner.prepare_spawn(&spec(&rig)).err().unwrap().code,
        "program_refused"
    );
}

#[test]
fn config_admission_shebang_forms_resolve_interpreter_once_and_reject_env_options() {
    let rig = rig();
    let interpreter = rig.path.join("node");
    std::fs::copy("/bin/sleep", &interpreter).unwrap();
    let script = rig.path.join("main.cjs");
    std::fs::write(&script, "#!/usr/bin/env node\ncode\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut spec = spec(&rig);
    spec.program = script.clone();
    spec.env
        .push(("PATH".into(), rig.path.to_string_lossy().into_owned()));
    spec.args = vec!["argument".into()];
    let launch = crate::config_gate::program::capture(&spec, &[]).unwrap();
    assert_eq!(launch.spec.program, interpreter);
    assert_eq!(
        launch.spec.args,
        [script.to_string_lossy().as_ref(), "argument"]
    );
    assert_eq!(launch.inputs.len(), 2);
    assert!(launch.inputs.values().any(|input| matches!(&input.value, subc_config::RecordedValue::Value { value } if value["coverage"] == "interpreter only covered")));
    for rejected in [
        "#!/usr/bin/env -S node --flag\n",
        "#!relative\n",
        "#!/usr/bin/env NAME=value\n",
    ] {
        std::fs::write(&script, rejected).unwrap();
        assert_eq!(
            crate::config_gate::program::capture(&spec, &[])
                .err()
                .unwrap()
                .code,
            "program_refused"
        );
    }
    std::fs::write(&script, "#!/bin/sleep 600\n").unwrap();
    let launch = crate::config_gate::program::capture(&spec, &[]).unwrap();
    assert_eq!(launch.spec.args[0], "600");
    assert_eq!(launch.inputs.len(), 2);
}

#[tokio::test(start_paused = true)]
async fn config_admission_program_hold_never_arms_retry_or_spends_restart_budget() {
    let rig = rig();
    let owner = Arc::new(rig.owner("held"));
    rig.stamp(&owner);
    let spec = spec(&rig);
    std::fs::write(&spec.program, "unapproved replacement").unwrap();
    let handle = channel_handle();
    let supervisor = Supervisor::new_for_test(
        Arc::new(crate::Registry::default()),
        RestartPolicy::default(),
    )
    .with_handle(handle)
    .with_config_runtime(owner);
    let module = supervisor
        .supervise_configured_requested_async(
            spec,
            true,
            Default::default(),
            None,
            RestartPolicy::default(),
            Duration::from_secs(5),
            None,
        )
        .await
        .unwrap();
    let before = module.status().unwrap();
    assert_eq!(before.state, ModuleState::Stopped);
    assert!(module
        .restart(None)
        .await
        .unwrap_err()
        .to_string()
        .contains("config_gated"));
    assert!(module
        .reload()
        .await
        .unwrap_err()
        .to_string()
        .contains("config_gated"));
    assert!(module
        .set_enabled(true)
        .await
        .unwrap_err()
        .to_string()
        .contains("config_gated"));
    assert!(module
        .swap(None)
        .await
        .unwrap_err()
        .to_string()
        .contains("config_gated"));
    tokio::time::advance(Duration::from_secs(7200)).await;
    let after = module.status().unwrap();
    assert_eq!(after.spawn_generation, before.spawn_generation);
    assert_eq!(after.restart_count, 0);
    assert!(module.next_retry_at_ms().unwrap().is_none());
}

#[tokio::test]
async fn config_admission_crash_during_approval_defers_without_parking_the_actor() {
    let rig = rig();
    let owner = Arc::new(rig.owner("publication"));
    rig.stamp(&owner);
    let supervisor = Supervisor::new_for_test(
        Arc::new(crate::Registry::default()),
        RestartPolicy::default(),
    )
    .with_handle(channel_handle())
    .with_config_runtime(owner.clone());
    let module = supervisor.spawn_async(spec(&rig)).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while module.status().unwrap().pid.is_none() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let before = module.status().unwrap();
    let mut doc: serde_json::Value =
        serde_json::from_slice(&std::fs::read(rig.path.join("subc.jsonc")).unwrap()).unwrap();
    doc["modules"]["a"]["args"] = json!(["601"]);
    std::fs::write(
        rig.path.join("subc.jsonc"),
        serde_json::to_vec(&doc).unwrap(),
    )
    .unwrap();
    let plan = owner
        .finish_plan(owner.prepare(PlanKind::Change).unwrap(), daemon_review)
        .unwrap();
    owner
        .desktop_view_delivered(ConnectionId::new(7), &plan)
        .unwrap();
    let challenge = owner.begin_approval(&plan.digest, &Rig::facts()).unwrap();
    let attempt = challenge.attempt();
    let gates = owner.approval_commit_gates(attempt).await.unwrap();
    assert!(!gates.is_empty());
    assert!(before.process_start_time.is_some());
    assert_eq!(
        crate::provenance::process_start_time(before.pid.unwrap()),
        before.process_start_time
    );
    subc_os::Process::open(before.pid.unwrap())
        .unwrap()
        .unwrap_or_else(|| {
            panic!(
                "child vanished: {:?}; stderr {:?}",
                module.status(),
                module.stderr_tail(None, None)
            )
        })
        .signal(subc_os::Signal::Kill)
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while module.status().unwrap().process_alive && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    let responsive = tokio::time::timeout(Duration::from_secs(1), module.set_enabled(true)).await;
    let during = module.status().unwrap();
    let published = owner
        .on_blocking(move |owner| {
            owner.finish_approval(
                attempt,
                &Rig::facts(),
                ApprovalReply::Presence(super::super::super::PresenceDecision::InteractiveApproval),
                2,
            )
        })
        .await;
    drop(gates);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while module.status().unwrap().spawn_generation == before.spawn_generation
        && tokio::time::Instant::now() < deadline
    {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let after = module.status().unwrap();
    module.stop().await.unwrap();
    assert!(published.is_ok(), "{published:?}");
    assert!(
        matches!(responsive, Ok(Ok(false))),
        "the actor must still answer commands while publication owns its gate: {responsive:?}"
    );
    assert_eq!(during.spawn_generation, before.spawn_generation);
    assert_eq!(after.spawn_generation, before.spawn_generation + 1);
    assert_eq!(
        after.restart_count, during.restart_count,
        "deferred admission cannot spend another restart-budget entry"
    );
}

#[tokio::test]
async fn config_admission_registered_actor_serves_commands_while_its_gate_is_held() {
    let rig = rig();
    let owner = Arc::new(rig.owner("registered-responsive"));
    rig.stamp(&owner);
    let registry = Arc::new(crate::Registry::default());
    let forwarding = Arc::new(crate::ForwardingTable::default());
    let mut launch = spec(&rig);
    launch.protocol = subc_control::ModuleProtocol::Subc;
    let supervisor = Supervisor::new_for_test(registry.clone(), RestartPolicy::default())
        .with_forwarding(forwarding.clone())
        .with_config_runtime(owner.clone());
    let module = supervisor.spawn_async(launch).await.unwrap();
    let gate = owner.module_gate("a").unwrap();
    let guard = gate.lock().await;
    let connection = ConnectionId::new(333);
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    forwarding
        .register_module_connection(
            connection,
            "a".into(),
            subc_protocol::PROTOCOL_VERSION,
            subc_protocol::manifest::Concurrency::ModuleManaged,
            crate::FrameSink::new(tx),
        )
        .unwrap();
    registry
        .register_with_control_ops(
            subc_protocol::manifest::ModuleManifest::builder("a", "1.0.0").build(),
            subc_protocol::PROTOCOL_VERSION,
            connection,
            vec!["config.review".into()],
        )
        .unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    let responsive = tokio::time::timeout(Duration::from_secs(1), module.set_enabled(true)).await;
    drop(guard);
    registry.deregister_connection(connection).unwrap();
    module.stop().await.unwrap();
    assert!(
        matches!(responsive, Ok(Ok(false))),
        "registered actor parked before its command select: {responsive:?}"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn config_admission_swap_cutover_does_not_park_stop_behind_owner_publication() {
    let rig = rig();
    wire_fixture_document(&rig, json!(["600"]));
    let path = rig.path.join("subc.jsonc");
    let mut document: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    document["modules"]["a"]["overlap"] = json!("safe");
    std::fs::write(path, serde_json::to_vec(&document).unwrap()).unwrap();
    let owner = Arc::new(rig.owner("swap-owner-busy"));
    rig.stamp(&owner);
    let registry = Arc::new(crate::Registry::default());
    let forwarding = Arc::new(crate::ForwardingTable::default());
    let handle = channel_handle();
    let supervisor = Supervisor::new_for_test(registry.clone(), RestartPolicy::default())
        .with_handle(handle.clone())
        .with_forwarding(forwarding.clone())
        .with_config_runtime(owner.clone())
        .with_drain_timeout(Duration::from_millis(20));
    let module = supervisor.spawn_async(spec(&rig)).await.unwrap();
    let mut manifest = subc_protocol::manifest::ModuleManifest::builder("a", "1.0.0").build();
    manifest.ready = Some(true);
    registry
        .register_with_control_ops(
            manifest.clone(),
            subc_protocol::PROTOCOL_VERSION,
            ConnectionId::new(770),
            vec![],
        )
        .unwrap();
    let (tx, _rx) = tokio::sync::mpsc::channel(16);
    forwarding
        .register_module_connection(
            ConnectionId::new(770),
            "a".into(),
            subc_protocol::PROTOCOL_VERSION,
            subc_protocol::manifest::Concurrency::ModuleManaged,
            crate::FrameSink::new(tx),
        )
        .unwrap();
    let swapping = {
        let module = module.clone();
        tokio::spawn(async move { module.swap(Some(Duration::from_secs(5))).await })
    };
    let candidate_nonce = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(nonce) = handle.candidate_registration_proof_for_test("a") {
                if handle.has_live_verified_program_binding("a", &nonce)
                    && handle.wait_program_binding("a", &nonce).await
                {
                    break nonce;
                }
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let guard = owner.lock().unwrap();
    registry
        .register_candidate_with_control_ops(
            manifest,
            subc_protocol::PROTOCOL_VERSION,
            ConnectionId::new(771),
            vec!["config.review".into(), "health.check".into()],
        )
        .unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    forwarding
        .register_candidate_module_connection(
            ConnectionId::new(771),
            "a".into(),
            subc_protocol::PROTOCOL_VERSION,
            subc_protocol::manifest::Concurrency::ModuleManaged,
            crate::FrameSink::new(tx),
        )
        .unwrap();
    let probe = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    forwarding
        .complete_module_control_rpc(
            ConnectionId::new(771),
            probe.header.corr,
            Some("health.check"),
            crate::forwarding::ModuleControlRpcOutcome::Response(
                subc_protocol::session::ModuleControlResponse::HealthCheck {
                    status: subc_protocol::session::HealthStatus::Ok,
                    detail: None,
                    metrics: None,
                },
            ),
        )
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while owner
            .swap_preflight_attempts
            .load(std::sync::atomic::Ordering::SeqCst)
            == 0
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    registry
        .deregister_connection(ConnectionId::new(770))
        .unwrap();
    registry
        .deregister_connection(ConnectionId::new(771))
        .unwrap();
    let stopped = tokio::time::timeout(Duration::from_millis(500), module.stop()).await;
    drop(guard);
    let _ = swapping.await.unwrap();
    if stopped.is_err() {
        let _ = module.stop().await;
    }
    assert!(
        stopped.is_ok(),
        "the module gate parked stop behind blocking owner publication"
    );
    assert!(
        handle.has_program_binding_record("a", &handle.registration_proof_for_test("a").unwrap())
    );
    assert!(!handle.has_live_verified_program_binding("a", &candidate_nonce));
    let handler = crate::ControlHandler::new(registry.clone())
        .with_supervisor(handle)
        .with_config_runtime(owner.clone());
    for recovery in [false, true] {
        if recovery {
            std::fs::write(
                rig.path.join("store").join(crate::config_store::STORE_FILE),
                b"tampered",
            )
            .unwrap();
            assert!(owner.check_recovery().unwrap());
        }
        candidate_nonce.bind(ConnectionId::new(772));
        let hello = subc_protocol::ModuleHelloBody {
            manifest: subc_protocol::manifest::ModuleManifest::builder("a", "1.0.0").build(),
            protocol_ver: subc_protocol::PROTOCOL_VERSION,
            control_ops: None,
            launch_nonce: candidate_nonce.nonce(),
        };
        let frame = subc_protocol::Frame::build(
            subc_protocol::FrameType::Hello,
            subc_protocol::Flags::new(false, subc_protocol::Priority::Passive, false),
            0,
            0,
            1,
            serde_json::to_vec(&hello).unwrap(),
        )
        .unwrap();
        let reply = handler
            .handle_control(ConnectionId::new(772), frame)
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&reply[0].body).unwrap()["code"],
            if recovery {
                "registration_channel_hello_repeated"
            } else {
                "registration_channel_closed"
            }
        );
        assert!(registry.get_module("a").unwrap().is_none());
    }
}

#[tokio::test]
async fn config_admission_exit_observation_does_not_park_commands_behind_the_owner_mutex() {
    let rig = rig();
    let owner = Arc::new(rig.owner("exit-responsive"));
    rig.stamp(&owner);
    let supervisor = Supervisor::new_for_test(
        Arc::new(crate::Registry::default()),
        RestartPolicy::default(),
    )
    .with_config_runtime(owner.clone());
    let module = supervisor.spawn_async(spec(&rig)).await.unwrap();
    let pid = reported_pid(&module).await;
    let (entered, ready) = tokio::sync::oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    let publication = owner.clone();
    let task = tokio::task::spawn_blocking(move || {
        let _guard = publication.lock().unwrap();
        entered.send(()).unwrap();
        wait.recv().unwrap();
    });
    ready.await.unwrap();
    subc_os::Process::open(pid)
        .unwrap()
        .unwrap()
        .signal(subc_os::Signal::Kill)
        .unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    let deferred_restart = tokio::time::timeout(Duration::from_secs(1), module.restart(None)).await;
    let responsive = tokio::time::timeout(Duration::from_secs(1), module.set_enabled(true)).await;
    release.send(()).unwrap();
    task.await.unwrap();
    module.stop().await.unwrap();
    assert!(
        matches!(
            deferred_restart,
            Ok(Err(
                crate::supervise::SuperviseError::ConfigDeliveryDeferred { .. }
            ))
        ),
        "a new spawn decision cannot precede the old child's recorded exit: {deferred_restart:?}"
    );
    assert!(
        matches!(responsive, Ok(Ok(false))),
        "exited actor parked inside the owner-lock wait: {responsive:?}"
    );
}

#[tokio::test]
async fn config_admission_rollback_executes_the_current_restored_path_and_rechecks_it() {
    let rig = rig();
    let owner = Arc::new(rig.owner("rollback-spawn"));
    rig.stamp(&owner);
    let restored = spec(&rig);
    let other = rig.path.join("new-program");
    std::fs::copy("/bin/sleep", &other).unwrap();
    let mut doc: serde_json::Value =
        serde_json::from_slice(&std::fs::read(rig.path.join("subc.jsonc")).unwrap()).unwrap();
    doc["modules"]["a"]["program"] = json!(other);
    std::fs::write(
        rig.path.join("subc.jsonc"),
        serde_json::to_vec(&doc).unwrap(),
    )
    .unwrap();
    approve(&rig, &owner, PlanKind::Change);
    std::fs::copy("/bin/sleep", rig.path.join("restored-replacement")).unwrap();
    std::fs::rename(rig.path.join("restored-replacement"), &restored.program).unwrap();
    let generation = approve(&rig, &owner, PlanKind::Rollback(1));
    let approved = owner.approved_rollback_commands(generation).unwrap();
    assert_eq!(
        approved.programs["a"].path,
        restored.program.to_string_lossy()
    );
    let supervisor = Supervisor::new_for_test(
        Arc::new(crate::Registry::default()),
        RestartPolicy::default(),
    )
    .with_config_runtime(owner.clone());
    let module = supervisor.spawn_async(spec(&rig)).await.unwrap();
    let status = module.status().unwrap();
    module.stop().await.unwrap();
    assert_eq!(status.spawned_from.as_ref(), Some(&restored.program));
    std::fs::write(&restored.program, "changed after rollback approval").unwrap();
    assert_eq!(
        owner.prepare_spawn(&spec(&rig)).err().unwrap().code,
        "program_refused"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn config_admission_post_exec_hashes_a_protocol_none_child() {
    let rig = rig();
    let owner = Arc::new(rig.owner("post-exec-none"));
    rig.stamp(&owner);
    let program = rig.path.join("program");
    let identity =
        subc_os::retained_file_identity(&std::fs::File::open(&program).unwrap()).unwrap();
    owner.set_spawn_exec_hook_for_test(move || {
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(&program)
            .unwrap()
            .write_all(b"unapproved same-inode bytes")
            .unwrap();
        assert_eq!(
            subc_os::retained_file_identity(&std::fs::File::open(&program).unwrap()).unwrap(),
            identity
        );
    });
    let supervisor = Supervisor::new_for_test(
        Arc::new(crate::Registry::default()),
        RestartPolicy::default(),
    )
    .with_config_runtime(owner.clone());
    let module = supervisor.spawn_async(spec(&rig)).await.unwrap();
    let held = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let status = module.status().unwrap();
            if owner.check_module_hold("a").is_err()
                && status.state == ModuleState::Stopped
                && !status.process_alive
            {
                break status;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    module.stop().await.unwrap();
    let status =
        held.expect("a protocol:none child with unapproved content was never checked after exec");
    assert_eq!(status.restart_count, 0);
    assert_eq!(
        owner
            .take_activities()
            .iter()
            .filter(|activity| activity.event == "config.refused")
            .count(),
        1
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn config_admission_unix_replacement_between_check_and_exec_is_killed_and_held() {
    let rig = rig();
    let owner = Arc::new(rig.owner("exec-race"));
    rig.stamp(&owner);
    let program = rig.path.join("program");
    let replacement = rig.path.join("racing-program");
    std::fs::copy(&program, &replacement).unwrap();
    owner.set_spawn_exec_hook_for_test(move || std::fs::rename(replacement, program).unwrap());
    let supervisor = Supervisor::new_for_test(
        Arc::new(crate::Registry::default()),
        RestartPolicy::default(),
    )
    .with_config_runtime(owner.clone());
    let module = supervisor.spawn_async(spec(&rig)).await.unwrap();
    let status = module.status().unwrap();
    assert_eq!(status.state, ModuleState::Stopped);
    assert!(!status.process_alive);
    assert_eq!(status.spawn_generation, 0);
    assert_eq!(status.restart_count, 0);
    assert_eq!(
        owner
            .take_activities()
            .iter()
            .filter(|activity| activity.event == "config.refused")
            .count(),
        1
    );
}

#[tokio::test]
async fn config_admission_tamper_recovery_preserves_running_children_but_refuses_new_use() {
    let rig = rig();
    let owner = Arc::new(rig.owner("tamper"));
    rig.stamp(&owner);
    let supervisor = Supervisor::new_for_test(
        Arc::new(crate::Registry::default()),
        RestartPolicy::default(),
    )
    .with_config_runtime(owner.clone());
    let module = supervisor.spawn_async(spec(&rig)).await.unwrap();
    let pid = Some(reported_pid(&module).await);
    let store = rig.path.join("store").join(crate::config_store::STORE_FILE);
    let mut edited = std::fs::read(&store).unwrap();
    edited.push(b' ');
    std::fs::write(store, edited).unwrap();
    assert_eq!(
        owner.checked_admission_hold(Some("a")).unwrap_err().code,
        "store_tampered"
    );
    assert!(module.status().unwrap().process_alive);
    assert_eq!(module.status().unwrap().pid, pid);
    assert!(module
        .restart(None)
        .await
        .unwrap_err()
        .to_string()
        .contains("config_gated"));
    assert!(module
        .reload()
        .await
        .unwrap_err()
        .to_string()
        .contains("config_gated"));
    assert!(module
        .swap(None)
        .await
        .unwrap_err()
        .to_string()
        .contains("config_gated"));
    assert_eq!(module.status().unwrap().pid, pid);
    module.stop().await.unwrap();
    let supervisor = Supervisor::new_for_test(
        Arc::new(crate::Registry::default()),
        RestartPolicy::default(),
    )
    .with_config_runtime(owner.clone());
    let held = supervisor.spawn_async(spec(&rig)).await.unwrap();
    assert_eq!(held.status().unwrap().state, ModuleState::Stopped);
    assert_eq!(held.status().unwrap().restart_count, 0);
    assert_eq!(held.status().unwrap().spawn_generation, 0);
    assert_eq!(
        owner
            .take_activities()
            .iter()
            .filter(|activity| activity.event == "config.tamper")
            .count(),
        1
    );
}

#[test]
fn config_admission_unstamp_clears_durable_refusals_and_restores_file_authority() {
    let rig = rig();
    let owner = rig.owner("unstamp");
    rig.stamp(&owner);
    std::fs::write(rig.path.join("program"), "different code").unwrap();
    assert_eq!(
        owner.prepare_spawn(&spec(&rig)).err().unwrap().code,
        "program_refused"
    );
    approve(&rig, &owner, PlanKind::Unstamp);
    assert!(owner.prepare_spawn(&spec(&rig)).unwrap().launch.is_none());
    assert!(owner
        .lock()
        .unwrap()
        .store
        .body()
        .unwrap()
        .program_refusals
        .is_empty());
    assert_eq!(owner.status().unwrap().state, "unstamped");
}

#[test]
fn config_admission_program_refusal_limit_never_releases_a_held_module() {
    let rig = rig();
    let path = rig.path.join("subc.jsonc");
    let mut document: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    document["modules"]["b"] = document["modules"]["a"].clone();
    std::fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
    let owner = rig.owner("refusal-cap");
    rig.stamp(&owner);
    let approved = owner.lock().unwrap().store.body().unwrap().approved.clone();
    let snapshot = crate::config_gate::GateSnapshot::from_store(&approved).unwrap();
    let base_a = crate::config_gate::program::approved_base(&snapshot, "a");
    let base_b = crate::config_gate::program::approved_base(&snapshot, "b");
    let limit = crate::config_store::PROGRAM_REFUSAL_LIMIT;
    owner
        .record_program_refusal(
            "a",
            &base_a,
            subc_config::raw_digest(b"first refused program"),
        )
        .unwrap();
    for identity in 0..limit {
        owner
            .record_program_refusal(
                "b",
                &base_b,
                subc_config::raw_digest(identity.to_string().as_bytes()),
            )
            .unwrap();
    }
    assert_eq!(
        owner
            .check_module_hold("a")
            .expect_err("capacity eviction released a held module")
            .code,
        "config_gated"
    );
    assert_eq!(
        owner
            .lock()
            .unwrap()
            .store
            .body()
            .unwrap()
            .program_refusals
            .len(),
        limit + 1
    );
    drop(owner);
    let restarted = rig.owner("refusal-cap-restarted");
    assert_eq!(
        restarted
            .check_module_hold("a")
            .expect_err("restart lost the first durable hold")
            .code,
        "config_gated"
    );
}

#[tokio::test]
async fn config_admission_approving_a_refused_program_starts_its_enabled_held_actor() {
    let rig = rig();
    let owner = Arc::new(rig.owner("release"));
    rig.stamp(&owner);
    std::fs::copy(rig.path.join("program"), rig.path.join("replacement")).unwrap();
    std::fs::rename(rig.path.join("replacement"), rig.path.join("program")).unwrap();
    let handle = channel_handle();
    let registry = Arc::new(crate::Registry::default());
    let supervisor = Supervisor::new_for_test(registry.clone(), RestartPolicy::default())
        .with_handle(handle.clone())
        .with_config_runtime(owner.clone());
    let module = supervisor.spawn_async(spec(&rig)).await.unwrap();
    assert_eq!(module.status().unwrap().state, ModuleState::Stopped);
    let handler = Arc::new(
        crate::ControlHandler::new(registry)
            .with_supervisor(handle)
            .with_config_runtime(owner.clone()),
    );
    let worker = handler.spawn_config_delivery_worker().unwrap();
    approve(&rig, &owner, PlanKind::Change);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while module.status().unwrap().pid.is_none() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let status = module.status().unwrap();
    worker.abort();
    module.stop().await.unwrap();
    assert!(status.process_alive);
    assert_eq!(status.spawn_generation, 1);
    assert_eq!(status.lifetime_restarts, 0);
    assert_eq!(status.restart_count, 0);
}

#[test]
fn config_admission_final_check_keeps_the_pending_exit_fence_then_holds_without_an_active_vector() {
    let rig = rig();
    let owner = rig.owner("pending-exit");
    rig.declare(&owner, 10);
    rig.stamp(&owner);
    let admission = owner.prepare_spawn(&spec(&rig)).unwrap();
    let spawn = owner
        .delivery_spawned(
            "a",
            tokio::time::Instant::now(),
            Duration::from_secs(10),
            Duration::from_secs(10),
        )
        .unwrap()
        .unwrap();
    owner.delivery_child_exited(&spawn).unwrap();
    assert_eq!(
        owner.check_spawn("a", &admission).unwrap_err().code,
        "config_delivery_deferred"
    );
    let exit = owner.delivery_exits().unwrap().pop().unwrap();
    owner.delivery_record_exit(&exit).unwrap();
    owner.delivery_release_exit(&exit).unwrap();
    assert_eq!(
        owner.prepare_spawn(&spec(&rig)).err().unwrap().code,
        "config_gated"
    );
}

#[test]
fn config_admission_daemon_restart_precedence_keeps_the_active_command_until_a_new_daemon() {
    let rig = rig();
    let owner = rig.owner("deferred-command");
    rig.declare(&owner, 10);
    rig.stamp(&owner);
    owner
        .lock()
        .unwrap()
        .store
        .record_delivery(
            &crate::config_store::Tuple {
                module: "a".into(),
                project: None,
            },
            1,
            true,
        )
        .unwrap();
    let mut doc: serde_json::Value =
        serde_json::from_slice(&std::fs::read(rig.path.join("subc.jsonc")).unwrap()).unwrap();
    doc["modules"]["a"]["args"] = json!(["601"]);
    std::fs::write(
        rig.path.join("subc.jsonc"),
        serde_json::to_vec(&doc).unwrap(),
    )
    .unwrap();
    let plan = owner
        .finish_plan(owner.prepare(PlanKind::Change).unwrap(), |old, new| {
            let mut changes = daemon_review(old, new)?;
            changes.fields.push(subc_config::FieldChange::new(
                "a",
                "/daemon_dependency",
                subc_config::RecordedValue::new(json!(1)),
                subc_config::RecordedValue::new(json!(2)),
                subc_config::ApplyKind::DaemonRestart,
            ));
            Ok(changes)
        })
        .unwrap();
    owner
        .desktop_view_delivered(ConnectionId::new(7), &plan)
        .unwrap();
    let challenge = owner.begin_approval(&plan.digest, &Rig::facts()).unwrap();
    owner
        .finish_approval(
            challenge.attempt(),
            &Rig::facts(),
            ApprovalReply::Presence(super::super::super::PresenceDecision::InteractiveApproval),
            2,
        )
        .unwrap();
    assert_eq!(
        owner
            .prepare_spawn(&spec(&rig))
            .unwrap()
            .launch
            .unwrap()
            .spec
            .args,
        ["600"]
    );
    drop(owner);
    let restarted = rig.owner("new-daemon");
    assert_eq!(
        restarted
            .prepare_spawn(&spec(&rig))
            .unwrap()
            .launch
            .unwrap()
            .spec
            .args,
        ["601"]
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn config_admission_exited_before_binding_keeps_its_real_exit_without_admitting_hello() {
    let rig = rig();
    std::fs::write(rig.path.join("program"), "#!/bin/sh\nexit 23\n").unwrap();
    let mut doc: serde_json::Value =
        serde_json::from_slice(&std::fs::read(rig.path.join("subc.jsonc")).unwrap()).unwrap();
    doc["modules"]["a"]["protocol"] = json!("subc");
    std::fs::write(
        rig.path.join("subc.jsonc"),
        serde_json::to_vec(&doc).unwrap(),
    )
    .unwrap();
    let owner = Arc::new(rig.owner("already-exited"));
    rig.stamp(&owner);
    let handle = channel_handle();
    let supervisor = Supervisor::new_for_test(
        Arc::new(crate::Registry::default()),
        RestartPolicy::new(0, Duration::ZERO),
    )
    .with_handle(handle.clone())
    .with_config_runtime(owner);
    supervisor.set_admission_hook_for_test(|_, stage, pid| {
        if stage == "before_image_binding" {
            let path = format!("/proc/{}/stat", pid.unwrap());
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while !std::fs::read_to_string(&path)
                .unwrap()
                .rsplit_once(") ")
                .unwrap()
                .1
                .starts_with("Z ")
            {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the fixture must exit before its image is sampled"
                );
                std::thread::yield_now();
            }
        }
        true
    });
    let module = supervisor.spawn_async(spec(&rig)).await.unwrap();
    let nonce = handle.registration_proof_for_test("a").unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while module.status().unwrap().last_exit.is_none() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let status = module.status().unwrap();
    assert_eq!(status.last_exit.unwrap().code, Some(23));
    assert_eq!(status.state, ModuleState::Failed);
    assert_eq!(status.spawn_generation, 1);
    assert!(!status.process_alive);
    assert!(!handle.has_live_verified_program_binding("a", &nonce));
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn config_admission_wrapper_exec_to_a_different_program_is_program_refused() {
    let rig = rig();
    std::fs::write(rig.path.join("program"), "#!/bin/sh\nexec /bin/sleep 600\n").unwrap();
    let owner = Arc::new(rig.owner("wrapper-exec"));
    rig.stamp(&owner);
    let supervisor = Supervisor::new_for_test(
        Arc::new(crate::Registry::default()),
        RestartPolicy::default(),
    )
    .with_config_runtime(owner.clone());
    let replacement_image = subc_os::file_identity(std::path::Path::new("/bin/sleep")).unwrap();
    supervisor.set_admission_hook_for_test(move |_, stage, pid| {
        if stage == "before_image_binding" {
            let process = subc_os::Process::open(pid.unwrap()).unwrap().unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while process.observe().and_then(|image| image.executable) != Some(replacement_image) {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the wrapper must exec the other program before binding is sampled"
                );
                std::thread::yield_now();
            }
        }
        true
    });
    let module = supervisor.spawn_async(spec(&rig)).await.unwrap();
    let status = module.status().unwrap();
    assert_eq!(status.state, ModuleState::Stopped);
    assert!(!status.process_alive);
    assert_eq!(status.spawn_generation, 0);
    assert_eq!(status.restart_count, 0);
    let refused: Vec<_> = owner
        .take_activities()
        .into_iter()
        .filter(|activity| activity.event == "config.refused")
        .collect();
    assert_eq!(refused.len(), 1);
    assert!(
        refused[0].detail.starts_with("a: program_refused;"),
        "{:?}",
        refused[0]
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn refused_content_hello(
    rig: &Rig,
    owner: Arc<RuntimeOwner>,
    handle: SupervisorHandle,
    registry: Arc<crate::Registry>,
    module: crate::supervise::SupervisedModule,
) {
    let nonce = handle.registration_proof_for_test("a").unwrap();
    let handler = crate::ControlHandler::new(registry.clone())
        .with_supervisor(handle)
        .with_config_runtime(owner.clone());
    let hello = subc_protocol::ModuleHelloBody {
        manifest: subc_protocol::manifest::ModuleManifest::builder("a", "1.0.0").build(),
        protocol_ver: subc_protocol::PROTOCOL_VERSION,
        control_ops: None,
        launch_nonce: nonce.nonce(),
    };
    let frame = subc_protocol::Frame::build(
        subc_protocol::FrameType::Hello,
        subc_protocol::Flags::new(false, subc_protocol::Priority::Passive, false),
        0,
        0,
        1,
        serde_json::to_vec(&hello).unwrap(),
    )
    .unwrap();
    let connection = ConnectionId::new(444);
    nonce.bind(connection);
    let reply = handler.handle_control(connection, frame).unwrap();
    let _ = registry.deregister_connection(connection);
    module.stop().await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&reply[0].body).unwrap();
    assert_eq!(
        body["code"], "program_refused",
        "unapproved bytes completed HELLO at {:?}: {body}",
        rig.path
    );
    assert_eq!(
        owner
            .take_activities()
            .iter()
            .filter(|activity| activity.event == "config.refused")
            .count(),
        1,
        "content refusal, not merely an unready latch, must reject HELLO"
    );
}

fn wire_fixture_document(rig: &Rig, args: serde_json::Value) {
    let mut doc: serde_json::Value =
        serde_json::from_slice(&std::fs::read(rig.path.join("subc.jsonc")).unwrap()).unwrap();
    doc["modules"]["a"]["protocol"] = json!("subc");
    doc["modules"]["a"]["args"] = args;
    std::fs::write(
        rig.path.join("subc.jsonc"),
        serde_json::to_vec(&doc).unwrap(),
    )
    .unwrap();
}

#[tokio::test]
async fn config_admission_hello_accepts_only_the_approved_current_native_image() {
    let rig = rig();
    wire_fixture_document(&rig, json!(["600"]));
    let owner = Arc::new(rig.owner("native-content"));
    rig.stamp(&owner);
    let handle = channel_handle();
    let registry = Arc::new(crate::Registry::default());
    let supervisor = Supervisor::new_for_test(registry.clone(), RestartPolicy::default())
        .with_handle(handle.clone())
        .with_config_runtime(owner.clone());
    let module = supervisor.spawn_async(spec(&rig)).await.unwrap();
    let nonce = handle.registration_proof_for_test("a").unwrap();
    assert!(handle.wait_program_binding("a", &nonce).await);
    // The channel service waits for launch confirmation before dispatching
    // HELLO. A direct-handler fixture must wait for the same transition.
    handle
        .registration_for_test("a")
        .unwrap()
        .wait_launch()
        .await;
    assert!(
        !module.configuration().unwrap().0.reserved,
        "the launch-local nonce gate must not rewrite approved role policy"
    );
    assert!(
        !handle.reserved_hello_authorized("a", None),
        "configuration gating refuses nonce admission without rewriting the unreserved role policy"
    );
    assert!(
        !handle.reserved_hello_authorized("a", Some(&nonce)),
        "a channel binding key is never a legacy nonce"
    );
    assert!(
        handle.reserved_hello_authorized("a.child", None),
        "no capability or child-id prefix was reserved by the launch-local flag"
    );
    let handler = crate::ControlHandler::new(registry.clone())
        .with_supervisor(handle)
        .with_config_runtime(owner);
    let hello = subc_protocol::ModuleHelloBody {
        manifest: subc_protocol::manifest::ModuleManifest::builder("a", "1.0.0").build(),
        protocol_ver: subc_protocol::PROTOCOL_VERSION,
        control_ops: None,
        launch_nonce: nonce.nonce(),
    };
    let frame = subc_protocol::Frame::build(
        subc_protocol::FrameType::Hello,
        subc_protocol::Flags::new(false, subc_protocol::Priority::Passive, false),
        0,
        0,
        1,
        serde_json::to_vec(&hello).unwrap(),
    )
    .unwrap();
    let impostor = handler
        .handle_control(
            ConnectionId::new(446),
            module_hello(None, ConnectionId::new(446)),
        )
        .unwrap();
    assert_ne!(
        impostor[0].header.ty,
        subc_protocol::FrameType::HelloAck,
        "gated identity checks remain mandatory without a synthetic legacy reservation"
    );
    assert!(registry.get_module("a").unwrap().is_none());
    let connection = ConnectionId::new(445);
    nonce.bind(connection);
    let reply = handler.handle_control(connection, frame).unwrap();
    registry.deregister_connection(connection).unwrap();
    module.stop().await.unwrap();
    assert_eq!(
        reply[0].header.ty,
        subc_protocol::FrameType::HelloAck,
        "{reply:?}"
    );
}

#[tokio::test]
async fn config_recovery_dead_child_retires_channel_without_legacy_fallback() {
    for mode in 0..3 {
        let recovery = mode == 1;
        let rig = rig();
        wire_fixture_document(&rig, json!(["600"]));
        let owner = Arc::new(rig.owner("dead-child-hello"));
        rig.stamp(&owner);
        let handle = channel_handle();
        let registry = Arc::new(crate::Registry::default());
        let supervisor = Supervisor::new_for_test(registry.clone(), RestartPolicy::default())
            .with_handle(handle.clone())
            .with_config_runtime(owner.clone());
        let module = supervisor.spawn_async(spec(&rig)).await.unwrap();
        let nonce = handle.registration_proof_for_test("a").unwrap();
        assert!(handle.wait_program_binding("a", &nonce).await);
        if recovery {
            std::fs::write(
                rig.path.join("store").join(crate::config_store::STORE_FILE),
                b"tampered",
            )
            .unwrap();
            assert!(owner.check_recovery().unwrap());
        }
        if mode == 2 {
            approve(&rig, &owner, PlanKind::Unstamp);
            assert!(!handle.has_program_binding_record("a", &nonce));
        }
        module.stop().await.unwrap();
        assert!(handle.spawn_launch_nonce_for("a").is_none());
        assert!(
            handle.has_program_binding_record("a", &nonce),
            "retirement must keep the legacy fallback closed"
        );
        assert!(!handle.has_live_verified_program_binding("a", &nonce));
        let handler = crate::ControlHandler::new(registry.clone())
            .with_supervisor(handle)
            .with_config_runtime(owner);
        let hello = subc_protocol::ModuleHelloBody {
            manifest: subc_protocol::manifest::ModuleManifest::builder("a", "1.0.0").build(),
            protocol_ver: subc_protocol::PROTOCOL_VERSION,
            control_ops: None,
            launch_nonce: nonce.nonce(),
        };
        let frame = subc_protocol::Frame::build(
            subc_protocol::FrameType::Hello,
            subc_protocol::Flags::new(false, subc_protocol::Priority::Passive, false),
            0,
            0,
            1,
            serde_json::to_vec(&hello).unwrap(),
        )
        .unwrap();
        nonce.bind(ConnectionId::new(447));
        let reply = handler
            .handle_control(ConnectionId::new(447), frame)
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&reply[0].body).unwrap()["code"],
            "registration_channel_closed"
        );
        assert!(registry.get_module("a").unwrap().is_none());
    }
}

#[tokio::test]
async fn config_recovery_direct_hello_rechecks_store_instead_of_trusting_cached_state() {
    let rig = rig();
    let owner = Arc::new(rig.owner("fresh-direct-hello"));
    rig.stamp(&owner);
    std::fs::write(
        rig.path.join("store").join(crate::config_store::STORE_FILE),
        b"tampered",
    )
    .unwrap();
    let registry = Arc::new(crate::Registry::default());
    let handler = crate::ControlHandler::new(registry.clone()).with_config_runtime(owner);
    let hello = subc_protocol::ModuleHelloBody {
        manifest: subc_protocol::manifest::ModuleManifest::builder("unconfigured", "1.0.0").build(),
        protocol_ver: subc_protocol::PROTOCOL_VERSION,
        control_ops: None,
        launch_nonce: None,
    };
    let frame = subc_protocol::Frame::build(
        subc_protocol::FrameType::Hello,
        subc_protocol::Flags::new(false, subc_protocol::Priority::Passive, false),
        0,
        0,
        1,
        serde_json::to_vec(&hello).unwrap(),
    )
    .unwrap();
    let reply = handler
        .handle_control(ConnectionId::new(446), frame)
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&reply[0].body).unwrap()["code"],
        "config_gated"
    );
    assert!(registry.get_module("unconfigured").unwrap().is_none());
}

#[tokio::test]
async fn config_recovery_hello_accepts_only_a_preexisting_bound_launch() {
    let rig = rig();
    wire_fixture_document(&rig, json!(["600"]));
    let owner = Arc::new(rig.owner("recovery-bound-hello"));
    rig.stamp(&owner);
    let handle = channel_handle();
    let registry = Arc::new(crate::Registry::default());
    let supervisor = Supervisor::new_for_test(registry.clone(), RestartPolicy::default())
        .with_handle(handle.clone())
        .with_config_runtime(owner.clone());
    let module = supervisor.spawn_async(spec(&rig)).await.unwrap();
    let nonce = handle.registration_proof_for_test("a").unwrap();
    assert!(handle.wait_program_binding("a", &nonce).await);
    std::fs::write(
        rig.path.join("store").join(crate::config_store::STORE_FILE),
        b"tampered",
    )
    .unwrap();
    assert!(owner.check_recovery().unwrap());
    let handler = crate::ControlHandler::new(registry.clone())
        .with_supervisor(handle)
        .with_config_runtime(owner);
    for presented in [None, Some("wrong".into()), Some(nonce)] {
        if let Some(proof) = &presented {
            proof.bind(ConnectionId::new(445));
        }
        let hello = subc_protocol::ModuleHelloBody {
            manifest: subc_protocol::manifest::ModuleManifest::builder("a", "1.0.0").build(),
            protocol_ver: subc_protocol::PROTOCOL_VERSION,
            control_ops: None,
            launch_nonce: presented.as_ref().and_then(|proof| proof.nonce()),
        };
        let frame = subc_protocol::Frame::build(
            subc_protocol::FrameType::Hello,
            subc_protocol::Flags::new(false, subc_protocol::Priority::Passive, false),
            0,
            0,
            1,
            serde_json::to_vec(&hello).unwrap(),
        )
        .unwrap();
        let reply = handler
            .handle_control(ConnectionId::new(445), frame)
            .unwrap();
        if presented.as_deref().is_some_and(|nonce| nonce != "wrong") {
            assert_eq!(reply[0].header.ty, subc_protocol::FrameType::HelloAck);
        } else {
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&reply[0].body).unwrap()["code"],
                "config_gated"
            );
            assert!(registry.get_module("a").unwrap().is_none());
        }
    }
    registry
        .deregister_connection(ConnectionId::new(445))
        .unwrap();
    module.stop().await.unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn config_admission_unsigned_macos_program_is_held_before_any_spawn() {
    let rig = rig();
    assert!(std::process::Command::new("/usr/bin/codesign")
        .arg("--remove-signature")
        .arg(rig.path.join("program"))
        .status()
        .unwrap()
        .success());
    let owner = Arc::new(rig.owner("unsigned-image"));
    rig.stamp(&owner);
    let supervisor = Supervisor::new_for_test(
        Arc::new(crate::Registry::default()),
        RestartPolicy::default(),
    )
    .with_config_runtime(owner.clone());
    let module = supervisor.spawn_async(spec(&rig)).await.unwrap();
    let status = module.status().unwrap();
    module.stop().await.unwrap();
    assert_eq!(status.state, ModuleState::Stopped);
    assert_eq!(status.spawn_generation, 0);
    assert_eq!(
        owner
            .take_activities()
            .iter()
            .filter(|activity| activity.event == "config.refused")
            .count(),
        1
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn config_admission_hello_cannot_publish_after_post_exec_same_inode_refusal() {
    use std::io::Write;
    let rig = rig();
    wire_fixture_document(&rig, json!(["600"]));
    let owner = Arc::new(rig.owner("image-content"));
    rig.stamp(&owner);
    let program = rig.path.join("program");
    owner.set_spawn_exec_hook_for_test(move || {
        std::fs::OpenOptions::new()
            .append(true)
            .open(&program)
            .unwrap()
            .write_all(b"unapproved image bytes")
            .unwrap()
    });
    let handle = channel_handle();
    let registry = Arc::new(crate::Registry::default());
    let supervisor = Supervisor::new_for_test(registry.clone(), RestartPolicy::default())
        .with_handle(handle.clone())
        .with_config_runtime(owner.clone());
    let module = supervisor.spawn_async(spec(&rig)).await.unwrap();
    let proof = handle.registration_proof_for_test("a");
    let handler = crate::ControlHandler::new(registry.clone())
        .with_supervisor(handle)
        .with_config_runtime(owner);
    let reply = handler
        .handle_control(
            ConnectionId::new(730),
            module_hello(proof, ConnectionId::new(730)),
        )
        .unwrap();
    module.stop().await.unwrap();
    assert_ne!(reply[0].header.ty, subc_protocol::FrameType::HelloAck);
    assert!(registry.get_module("a").unwrap().is_none());
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn config_admission_post_exec_checks_protocol_none_covered_files_after_privacy_ack() {
    let rig = rig();
    let covered = rig.path.join("covered.txt");
    std::fs::write(&covered, b"approved").unwrap();
    let mut document: serde_json::Value =
        serde_json::from_slice(&std::fs::read(rig.path.join("subc.jsonc")).unwrap()).unwrap();
    document["modules"]["a"]["program_files"] = json!([covered]);
    std::fs::write(
        rig.path.join("subc.jsonc"),
        serde_json::to_vec(&document).unwrap(),
    )
    .unwrap();
    let owner = Arc::new(rig.owner("post-privacy-content"));
    rig.stamp(&owner);
    owner.set_spawn_exec_hook_for_test(move || std::fs::write(covered, b"not approved").unwrap());
    let supervisor = Supervisor::new_for_test(
        Arc::new(crate::Registry::default()),
        RestartPolicy::default(),
    )
    .with_config_runtime(owner.clone());
    let module = supervisor.spawn_async(spec(&rig)).await.unwrap();
    let held = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let status = module.status().unwrap();
            if owner.check_module_hold("a").is_err()
                && status.state == ModuleState::Stopped
                && !status.process_alive
            {
                break status;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    module.stop().await.unwrap();
    let status =
        held.expect("a protocol:none child skipped its covered-file check after privacy exec");
    assert_eq!(status.restart_count, 0);
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn config_admission_hello_rechecks_script_and_declared_file_content() {
    use std::io::Write;
    for script_changed in [false, true] {
        let rig = rig();
        std::fs::write(
            rig.path.join("program"),
            "#!/bin/sh\nwhile :; do /bin/sleep 600; done\n",
        )
        .unwrap();
        wire_fixture_document(&rig, json!([]));
        let covered = rig.path.join("covered");
        std::fs::write(&covered, b"approved").unwrap();
        let mut doc: serde_json::Value =
            serde_json::from_slice(&std::fs::read(rig.path.join("subc.jsonc")).unwrap()).unwrap();
        doc["modules"]["a"]["program_files"] = json!([covered]);
        std::fs::write(
            rig.path.join("subc.jsonc"),
            serde_json::to_vec(&doc).unwrap(),
        )
        .unwrap();
        let owner = Arc::new(rig.owner("auxiliary-content"));
        rig.stamp(&owner);
        let handle = channel_handle();
        let registry = Arc::new(crate::Registry::default());
        let supervisor = Supervisor::new_for_test(registry.clone(), RestartPolicy::default())
            .with_handle(handle.clone())
            .with_config_runtime(owner.clone());
        let module = supervisor.spawn_async(spec(&rig)).await.unwrap();
        assert!(
            handle
                .wait_program_binding("a", &handle.registration_proof_for_test("a").unwrap())
                .await
        );
        let changed = if script_changed {
            rig.path.join("program")
        } else {
            covered
        };
        std::fs::OpenOptions::new()
            .append(true)
            .open(changed)
            .unwrap()
            .write_all(b"\n# unapproved bytes\n")
            .unwrap();
        refused_content_hello(&rig, owner, handle, registry, module).await;
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn config_admission_hello_rejects_a_wrapper_that_execs_after_the_spawn_sample() {
    let rig = rig();
    let marker = rig.path.join("release-wrapper");
    #[cfg(target_os = "linux")]
    std::fs::write(
        rig.path.join("program"),
        "#!/bin/sh\nwhile [ ! -f \"$1\" ]; do /bin/sleep 0.01; done\nexec /bin/sleep 600\n",
    )
    .unwrap();
    #[cfg(target_os = "macos")]
    {
        // Approved macOS programs must be signed images. A native wrapper
        // keeps the late exec under test without requiring script admission.
        let source = rig.path.join("late-exec.c");
        std::fs::write(&source, include_str!("../../tests/fixtures/late-exec.c")).unwrap();
        assert!(std::process::Command::new("/usr/bin/cc")
            .arg(&source)
            .arg("-o")
            .arg(rig.path.join("program"))
            .status()
            .unwrap()
            .success());
        assert!(std::process::Command::new("/usr/bin/codesign")
            .args(["--force", "--sign", "-"])
            .arg(rig.path.join("program"))
            .status()
            .unwrap()
            .success());
    }
    wire_fixture_document(&rig, json!([marker]));
    let owner = Arc::new(rig.owner("late-exec"));
    rig.stamp(&owner);
    let handle = channel_handle();
    let registry = Arc::new(crate::Registry::default());
    let supervisor = Supervisor::new_for_test(registry.clone(), RestartPolicy::default())
        .with_handle(handle.clone())
        .with_config_runtime(owner.clone());
    let module = supervisor.spawn_async(spec(&rig)).await.unwrap();
    let nonce = handle.registration_proof_for_test("a").unwrap_or_else(|| {
        panic!(
            "wrapper did not spawn: {:?}; {:?}",
            module.status(),
            owner.take_activities()
        )
    });
    assert!(handle.wait_program_binding("a", &nonce).await);
    // Binding publication can precede suspended exec confirmation on macOS.
    // Wait for a reportable child before releasing its late-exec barrier.
    let pid = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(pid) = module.status().unwrap().pid {
                break pid;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("wrapper must complete exec admission");
    let process = subc_os::Process::open(pid).unwrap().unwrap();
    std::fs::write(marker, "").unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while process.observe().and_then(|image| image.executable)
            != subc_os::file_identity(std::path::Path::new("/bin/sleep"))
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    refused_content_hello(&rig, owner, handle, registry, module).await;
}

#[tokio::test]
async fn config_admission_tamper_activity_is_emitted_only_after_the_module_gate_releases() {
    async fn logged(ledger: &crate::activity::ActivityLedger) -> usize {
        ledger.record("barrier".into(), serde_json::from_value(json!({"op":"activity.record","kind":"barrier.flushed","summary":"Queue flushed"})).unwrap(), subc_control::ActivityActor { principal:subc_protocol::Principal::Direct, scope_owner:None,agent_id:None,run_id:None,flow_id:None,project_root:None,workdir:None,project_id:None },1).await.unwrap();
        ledger
            .query(
                serde_json::from_value(
                    json!({"op":"activity.query","limit":1000,"kind":"config.tamper"}),
                )
                .unwrap(),
            )
            .await
            .unwrap()
            .entries
            .len()
    }
    let rig = rig();
    let owner = Arc::new(rig.owner("tamper-activity"));
    rig.stamp(&owner);
    let ledger = crate::activity::ActivityLedger::open(
        rig.path.join("activity.jsonl"),
        "tamper-activity".into(),
    );
    owner.set_activity(Some(ledger.clone())).unwrap();
    let gate = owner.module_gate("a").unwrap().lock_owned().await;
    let path = rig.path.join("store").join(crate::config_store::STORE_FILE);
    let mut bytes = std::fs::read(&path).unwrap();
    bytes.push(b' ');
    std::fs::write(path, bytes).unwrap();
    let result = owner
        .on_blocking(|owner| owner.checked_admission_hold(Some("a")))
        .await;
    assert_eq!(result.unwrap_err().code, "store_tampered");
    assert_eq!(logged(&ledger).await, 0);
    drop(gate);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while logged(&ledger).await == 0 && tokio::time::Instant::now() < deadline {
        tokio::task::yield_now().await;
    }
    assert_eq!(logged(&ledger).await, 1);
}

fn module_hello(
    proof: Option<crate::supervise::RegistrationProof>,
    connection: ConnectionId,
) -> subc_protocol::Frame {
    if let Some(proof) = &proof {
        proof.bind(connection);
    }
    let body = subc_protocol::ModuleHelloBody {
        manifest: subc_protocol::manifest::ModuleManifest::builder("a", "1.0.0").build(),
        protocol_ver: subc_protocol::PROTOCOL_VERSION,
        control_ops: None,
        launch_nonce: proof.and_then(|proof| proof.nonce()),
    };
    subc_protocol::Frame::build(
        subc_protocol::FrameType::Hello,
        subc_protocol::Flags::new(false, subc_protocol::Priority::Passive, false),
        0,
        0,
        1,
        serde_json::to_vec(&body).unwrap(),
    )
    .unwrap()
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn config_recovery_reaped_child_refuses_hello_before_exit_cleanup() {
    for case in ["plain", "reserved_unstamped", "swap_candidate"] {
        let rig = rig();
        wire_fixture_document(&rig, json!(["600"]));
        let path = rig.path.join("subc.jsonc");
        let mut doc: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        doc["modules"]["a"]["reserved"] = json!(case == "reserved_unstamped");
        doc["modules"]["a"]["overlap"] = json!("safe");
        std::fs::write(path, serde_json::to_vec(&doc).unwrap()).unwrap();
        let owner = Arc::new(rig.owner(case));
        rig.stamp(&owner);
        let registry = Arc::new(crate::Registry::default());
        let handle = channel_handle();
        let forwarding = Arc::new(crate::ForwardingTable::default());
        let supervisor = Supervisor::new_for_test(registry.clone(), RestartPolicy::default())
            .with_handle(handle.clone())
            .with_forwarding(forwarding)
            .with_config_runtime(owner.clone());
        let module = supervisor.spawn_async(spec(&rig)).await.unwrap();
        let pid = reported_pid(&module).await;
        let mut nonce = handle.registration_proof_for_test("a").unwrap();
        assert!(handle.wait_program_binding("a", &nonce).await);
        let mut swapping = None;
        if case == "swap_candidate" {
            let mut manifest =
                subc_protocol::manifest::ModuleManifest::builder("a", "1.0.0").build();
            manifest.ready = Some(true);
            registry
                .register_with_control_ops(
                    manifest,
                    subc_protocol::PROTOCOL_VERSION,
                    ConnectionId::new(787),
                    vec![],
                )
                .unwrap();
            swapping = Some(tokio::spawn({
                let module = module.clone();
                async move { module.swap(Some(Duration::from_secs(5))).await }
            }));
            nonce = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if let Some(nonce) = handle.candidate_registration_proof_for_test("a") {
                        if handle.has_live_verified_program_binding("a", &nonce) {
                            break nonce;
                        }
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
        }
        let (reaped, release) = handle.pause_program_binding_exit_for_test("a", &nonce);
        if case != "plain" {
            approve(&rig, &owner, PlanKind::Unstamp);
        }
        let stopping = if case == "swap_candidate" {
            Some(tokio::spawn({
                let module = module.clone();
                async move { module.stop().await }
            }))
        } else {
            subc_os::Process::open(pid)
                .unwrap()
                .unwrap()
                .signal(subc_os::Signal::Kill)
                .unwrap();
            None
        };
        tokio::time::timeout(Duration::from_secs(5), reaped)
            .await
            .unwrap()
            .unwrap();
        let retired = handle.has_program_binding_record("a", &nonce)
            && !handle.has_live_verified_program_binding("a", &nonce);
        let handler = crate::ControlHandler::new(registry.clone())
            .with_supervisor(handle)
            .with_config_runtime(owner);
        let reply = handler
            .handle_control(
                ConnectionId::new(788),
                module_hello(Some(nonce), ConnectionId::new(788)),
            )
            .unwrap();
        let _ = registry.deregister_connection(ConnectionId::new(788));
        let _ = registry.deregister_connection(ConnectionId::new(787));
        let _ = release.send(());
        if let Some(stopping) = stopping {
            stopping.await.unwrap().unwrap();
        }
        if let Some(swapping) = swapping {
            let _ = swapping.await.unwrap();
        }
        module.stop().await.unwrap();
        assert!(
            retired,
            "{case}: reap must retire authority before the paused cleanup await"
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&reply[0].body).unwrap()["code"],
            "registration_channel_closed",
            "{case}"
        );
    }
}

#[tokio::test]
async fn config_recovery_unstamped_respawn_admits_fresh_nonce_and_refuses_dead_nonce() {
    let rig = rig();
    wire_fixture_document(&rig, json!(["600"]));
    let path = rig.path.join("subc.jsonc");
    let mut doc: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    doc["modules"]["a"]["reserved"] = json!(true);
    std::fs::write(path, serde_json::to_vec(&doc).unwrap()).unwrap();
    let owner = Arc::new(rig.owner("unstamped-respawn"));
    rig.stamp(&owner);
    let registry = Arc::new(crate::Registry::default());
    let handle = channel_handle();
    let supervisor = Supervisor::new_for_test(registry.clone(), RestartPolicy::default())
        .with_handle(handle.clone())
        .with_config_runtime(owner.clone());
    let module = supervisor.spawn_async(spec(&rig)).await.unwrap();
    let pid = reported_pid(&module).await;
    let dead_nonce = handle.registration_proof_for_test("a").unwrap();
    assert!(handle.wait_program_binding("a", &dead_nonce).await);
    approve(&rig, &owner, PlanKind::Unstamp);
    #[cfg(target_os = "linux")]
    let (reaped, release) = handle.pause_program_binding_exit_for_test("a", &dead_nonce);
    subc_os::Process::open(pid)
        .unwrap()
        .unwrap()
        .signal(subc_os::Signal::Kill)
        .unwrap();
    #[cfg(target_os = "linux")]
    {
        tokio::time::timeout(Duration::from_secs(5), reaped)
            .await
            .unwrap()
            .unwrap();
        let handler = crate::ControlHandler::new(registry.clone())
            .with_supervisor(handle.clone())
            .with_config_runtime(owner.clone());
        let dead = handler
            .handle_control(
                ConnectionId::new(784),
                module_hello(Some(dead_nonce.clone()), ConnectionId::new(784)),
            )
            .unwrap();
        let _ = release.send(());
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&dead[0].body).unwrap()["code"],
            "registration_channel_closed"
        );
    }
    let fresh_nonce = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(nonce) = handle
                .spawn_launch_nonce_for("a")
                .map(crate::supervise::RegistrationProof::from)
            {
                if nonce != dead_nonce && module.status().unwrap().process_alive {
                    break nonce;
                }
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("unstamped crash must respawn with a fresh nonce");
    let handler = crate::ControlHandler::new(registry.clone())
        .with_supervisor(handle)
        .with_config_runtime(owner);
    let fresh = handler
        .handle_control(
            ConnectionId::new(785),
            module_hello(Some(fresh_nonce), ConnectionId::new(785)),
        )
        .unwrap();
    registry
        .deregister_connection(ConnectionId::new(785))
        .unwrap();
    let dead = handler
        .handle_control(
            ConnectionId::new(786),
            module_hello(Some(dead_nonce), ConnectionId::new(786)),
        )
        .unwrap();
    let _ = registry.deregister_connection(ConnectionId::new(786));
    module.stop().await.unwrap();
    assert_eq!(
        fresh[0].header.ty,
        subc_protocol::FrameType::HelloAck,
        "fresh unstamped respawn must register: {fresh:?}"
    );
    assert_eq!(dead[0].header.ty, subc_protocol::FrameType::Error);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&dead[0].body).unwrap()["code"],
        if cfg!(target_os = "linux") {
            "registration_channel_hello_repeated"
        } else {
            "registration_channel_closed"
        },
        "a retired channel cannot authorize the fresh unstamped launch"
    );
}

#[tokio::test]
async fn config_admission_hello_publication_rechecks_across_stamp_hold_and_new_spawn() {
    for transition in ["stamp", "hold", "new_spawn", "unstamped_exit"] {
        let rig = rig();
        wire_fixture_document(&rig, json!(["600"]));
        let owner = Arc::new(rig.owner(transition));
        let registry = Arc::new(crate::Registry::default());
        let handle = channel_handle();
        let supervisor = Supervisor::new_for_test(registry.clone(), RestartPolicy::default())
            .with_handle(handle.clone())
            .with_config_runtime(owner.clone());
        let module = if transition == "stamp" {
            None
        } else {
            rig.stamp(&owner);
            Some(supervisor.spawn_async(spec(&rig)).await.unwrap())
        };
        if let Some(module) = &module {
            reported_pid(module).await;
        }
        let nonce = if module.is_some() {
            Some(handle.registration_proof_for_test("a").unwrap())
        } else {
            None
        };
        if let Some(nonce) = &nonce {
            assert!(tokio::time::timeout(
                Duration::from_secs(5),
                handle.wait_program_binding("a", nonce)
            )
            .await
            .unwrap());
        }
        if transition == "unstamped_exit" {
            approve(&rig, &owner, PlanKind::Unstamp);
        }
        let hold = Arc::new(std::sync::Barrier::new(2));
        let handler = crate::ControlHandler::new(registry.clone())
            .with_supervisor(handle.clone())
            .with_config_runtime(owner.clone())
            .with_hello_before_publication_hold(hold.clone());
        let (sender, _frames) = tokio::sync::mpsc::channel(8);
        let ctx = crate::RouteCtx {
            connection_id: ConnectionId::new(760),
            egress: crate::FrameSink::new(sender),
        };
        let runtime = tokio::runtime::Handle::current();
        let pending = std::thread::spawn(move || {
            runtime
                .block_on(
                    handler.handle_control_frame(&ctx, module_hello(nonce, ctx.connection_id)),
                )
                .unwrap()
        });
        hold.wait();
        let gate = owner.module_gate("a").unwrap();
        let gate_is_held = gate.try_lock().is_err();
        let mut replacement = None;
        match transition {
            "stamp" => {
                rig.stamp(&owner);
            }
            "hold" => {
                let base = crate::config_gate::program::approved_base(
                    &crate::config_gate::GateSnapshot::from_store(
                        &owner.lock().unwrap().store.body().unwrap().approved,
                    )
                    .unwrap(),
                    "a",
                );
                owner
                    .record_program_refusal(
                        "a",
                        &base,
                        subc_config::raw_digest(b"held during hello"),
                    )
                    .unwrap();
            }
            _ => {
                module.as_ref().unwrap().stop().await.unwrap();
                // The paused HELLO owns the publication gate; a replacement
                // can be spawned only once that stale admission is resolved.
            }
        }
        hold.wait();
        let reply = pending.join().unwrap();
        if transition == "new_spawn" {
            replacement = Some(supervisor.spawn_async(spec(&rig)).await.unwrap());
        }
        assert!(
            gate_is_held,
            "production HELLO pause must own its module gate"
        );
        if let Some(module) = replacement {
            module.stop().await.unwrap();
        }
        if let Some(module) = module {
            module.stop().await.unwrap();
        }
        let admitted = registry.get_module("a").unwrap().is_some();
        registry
            .deregister_connection(ConnectionId::new(760))
            .unwrap();
        assert!(!admitted, "{transition} published the stale HELLO");
        assert_ne!(
            reply[0].header.ty,
            subc_protocol::FrameType::HelloAck,
            "a HELLO crossed {transition} without rechecking publication authority"
        );
    }
}

#[tokio::test]
async fn config_admission_stopped_gated_identity_refuses_impostors_without_a_live_spawn() {
    let rig = rig();
    wire_fixture_document(&rig, json!(["600"]));
    let owner = Arc::new(rig.owner("stopped-hello"));
    rig.stamp(&owner);
    let registry = Arc::new(crate::Registry::default());
    let handle = channel_handle();
    let handler = crate::ControlHandler::new(registry.clone())
        .with_supervisor(handle.clone())
        .with_config_runtime(owner.clone());
    for nonce in [None, Some("unminted".into())] {
        let reply = handler
            .handle_control(
                ConnectionId::new(701),
                module_hello(nonce, ConnectionId::new(701)),
            )
            .unwrap();
        assert_ne!(reply[0].header.ty, subc_protocol::FrameType::HelloAck);
    }
    let supervisor = Supervisor::new_for_test(registry.clone(), RestartPolicy::default())
        .with_handle(handle.clone())
        .with_config_runtime(owner.clone());
    let module = supervisor.spawn_async(spec(&rig)).await.unwrap();
    reported_pid(&module).await;
    let old = handle.registration_proof_for_test("a").unwrap();
    module.stop().await.unwrap();
    for nonce in [None, Some(old.clone())] {
        let reply = handler
            .handle_control(
                ConnectionId::new(702),
                module_hello(nonce, ConnectionId::new(702)),
            )
            .unwrap();
        assert_ne!(
            reply[0].header.ty,
            subc_protocol::FrameType::HelloAck,
            "a stopped child is not a live content witness"
        );
    }
    let mut changed = std::fs::read(rig.path.join("program")).unwrap();
    changed.extend_from_slice(b"changed while stopped");
    std::fs::write(rig.path.join("program"), changed).unwrap();
    assert_eq!(
        owner.prepare_spawn(&spec(&rig)).err().unwrap().code,
        "program_refused"
    );
    let held = handler
        .handle_control(
            ConnectionId::new(704),
            module_hello(Some(old), ConnectionId::new(704)),
        )
        .unwrap();
    assert_ne!(
        held[0].header.ty,
        subc_protocol::FrameType::HelloAck,
        "a held identity is protected without a spawn in flight"
    );
    assert!(registry.get_module("a").unwrap().is_none());
}

#[tokio::test]
async fn config_admission_unstamp_restores_unreserved_hello_without_a_synthetic_sticky_nonce() {
    let rig = rig();
    wire_fixture_document(&rig, json!(["600"]));
    let owner = Arc::new(rig.owner("unstamp-reservation"));
    rig.stamp(&owner);
    let registry = Arc::new(crate::Registry::default());
    let handle = channel_handle();
    let supervisor = Supervisor::new_for_test(registry.clone(), RestartPolicy::default())
        .with_handle(handle.clone())
        .with_config_runtime(owner.clone());
    let module = supervisor.spawn_async(spec(&rig)).await.unwrap();
    reported_pid(&module).await;
    let old_nonce = handle.registration_proof_for_test("a").unwrap();
    assert!(handle.has_program_binding_record("a", &old_nonce));
    module.stop().await.unwrap();
    approve(&rig, &owner, PlanKind::Unstamp);
    assert!(handle.has_program_binding_record("a", &old_nonce));
    assert!(
        !handle.has_live_verified_program_binding("a", &old_nonce),
        "unstamp retained a retired content witness"
    );
    let handler = crate::ControlHandler::new(registry.clone())
        .with_supervisor(handle)
        .with_config_runtime(owner);
    let reply = handler
        .handle_control(
            ConnectionId::new(703),
            module_hello(None, ConnectionId::new(703)),
        )
        .unwrap();
    let _ = registry.deregister_connection(ConnectionId::new(703));
    assert_eq!(
        reply[0].header.ty,
        subc_protocol::FrameType::HelloAck,
        "unstamp must restore the configured unreserved policy"
    );
    let refused = handler
        .handle_control(
            ConnectionId::new(704),
            module_hello(Some(old_nonce), ConnectionId::new(704)),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&refused[0].body).unwrap()["code"],
        "registration_channel_closed"
    );
}

#[tokio::test]
async fn config_admission_promoted_unreserved_child_restarts_with_a_fresh_channel() {
    let rig = rig();
    wire_fixture_document(&rig, json!(["600"]));
    let mut document: serde_json::Value =
        serde_json::from_slice(&std::fs::read(rig.path.join("subc.jsonc")).unwrap()).unwrap();
    document["modules"]["a"]["overlap"] = json!("safe");
    std::fs::write(
        rig.path.join("subc.jsonc"),
        serde_json::to_vec(&document).unwrap(),
    )
    .unwrap();
    let owner = Arc::new(rig.owner("promoted-reservation"));
    rig.stamp(&owner);
    let registry = Arc::new(crate::Registry::default());
    let forwarding = Arc::new(crate::ForwardingTable::default());
    let handle = channel_handle();
    let supervisor = Supervisor::new_for_test(registry.clone(), RestartPolicy::default())
        .with_handle(handle.clone())
        .with_forwarding(forwarding.clone())
        .with_config_runtime(owner.clone())
        .with_drain_timeout(Duration::from_millis(20));
    let module = supervisor.spawn_async(spec(&rig)).await.unwrap();
    reported_pid(&module).await;
    let original = handle.registration_proof_for_test("a").unwrap();
    let mut manifest = subc_protocol::manifest::ModuleManifest::builder("a", "1.0.0").build();
    manifest.ready = Some(true);
    let incumbent = ConnectionId::new(710);
    let candidate = ConnectionId::new(711);
    registry
        .register_with_control_ops(
            manifest.clone(),
            subc_protocol::PROTOCOL_VERSION,
            incumbent,
            vec![],
        )
        .unwrap();
    let (tx, _incumbent_frames) = tokio::sync::mpsc::channel(32);
    forwarding
        .register_module_connection(
            incumbent,
            "a".into(),
            subc_protocol::PROTOCOL_VERSION,
            subc_protocol::manifest::Concurrency::ModuleManaged,
            crate::FrameSink::new(tx),
        )
        .unwrap();
    let swapping = {
        let module = module.clone();
        tokio::spawn(async move { module.swap(Some(Duration::from_secs(5))).await })
    };
    // The swap is marked open before its replacement process has a registration
    // channel. Wait for that channel to exist before attempting candidate HELLO.
    let candidate_proof = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(proof) = handle.candidate_registration_proof_for_test("a") {
                break proof;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(handle.swap_open("a"));
    assert!(handle.wait_program_binding("a", &candidate_proof).await);
    candidate_proof.bind(candidate);
    let registering = crate::ControlHandler::new(registry.clone())
        .with_supervisor(handle.clone())
        .with_config_runtime(owner.clone());
    let hello = subc_protocol::ModuleHelloBody {
        manifest,
        protocol_ver: subc_protocol::PROTOCOL_VERSION,
        control_ops: None,
        launch_nonce: None,
    };
    let reply = registering
        .handle_control(
            candidate,
            subc_protocol::Frame::build(
                subc_protocol::FrameType::Hello,
                subc_protocol::Flags::new(false, subc_protocol::Priority::Passive, false),
                0,
                0,
                1,
                serde_json::to_vec(&hello).unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
    assert_eq!(reply[0].header.ty, subc_protocol::FrameType::HelloAck);
    let (tx, _candidate_frames) = tokio::sync::mpsc::channel(32);
    forwarding
        .register_candidate_module_connection(
            candidate,
            "a".into(),
            subc_protocol::PROTOCOL_VERSION,
            subc_protocol::manifest::Concurrency::ModuleManaged,
            crate::FrameSink::new(tx),
        )
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while registry
            .get_module("a")
            .unwrap()
            .is_none_or(|active| active.connection_id != candidate)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    registry.deregister_connection(incumbent).unwrap();
    let outcome = swapping.await.unwrap();
    assert!(outcome.is_ok(), "{outcome:?}");
    reported_pid(&module).await;
    let current = handle.registration_proof_for_test("a").unwrap();
    assert_ne!(original, current);
    assert!(handle.wait_program_binding("a", &current).await);
    tokio::time::timeout(Duration::from_secs(5), async {
        while handle.swap_open("a") {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let handler = crate::ControlHandler::with_forwarding(registry.clone(), forwarding)
        .with_supervisor(handle.clone())
        .with_config_runtime(owner);
    handler.cleanup_connection(candidate).unwrap();
    let reconnect = ConnectionId::new(712);
    // Child-bound registration: "A registration channel cannot be reconnected.
    // If it breaks, the module exits, and its restart gets a new channel."
    let reply = handler
        .handle_control(reconnect, module_hello(Some(current.clone()), reconnect))
        .unwrap();
    let _ = registry.deregister_connection(reconnect);
    assert_eq!(reply[0].header.ty, subc_protocol::FrameType::Error);
    let previous_generation = module.status().unwrap().spawn_generation;
    module.restart(None).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while module.status().unwrap().spawn_generation <= previous_generation {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let fresh = handle.registration_proof_for_test("a").unwrap();
    assert_ne!(fresh, current);
    assert!(handle.wait_program_binding("a", &fresh).await);
    let connection = ConnectionId::new(713);
    let reply = handler
        .handle_control(connection, module_hello(Some(fresh), connection))
        .unwrap();
    registry.deregister_connection(connection).unwrap();
    module.stop().await.unwrap();
    assert_eq!(
        reply[0].header.ty,
        subc_protocol::FrameType::HelloAck,
        "a restarted unreserved module must register over its fresh channel"
    );
}

#[tokio::test]
async fn config_admission_hold_after_final_hello_check_preserves_admission_but_refuses_next_spawn()
{
    let rig = rig();
    wire_fixture_document(&rig, json!(["600"]));
    let owner = Arc::new(rig.owner("post-check-hold"));
    rig.stamp(&owner);
    let registry = Arc::new(crate::Registry::default());
    let handle = channel_handle();
    let supervisor = Supervisor::new_for_test(registry.clone(), RestartPolicy::default())
        .with_handle(handle.clone())
        .with_config_runtime(owner.clone());
    let module = supervisor.spawn_async(spec(&rig)).await.unwrap();
    reported_pid(&module).await;
    let nonce = handle.registration_proof_for_test("a").unwrap();
    assert!(tokio::time::timeout(
        Duration::from_secs(5),
        handle.wait_program_binding("a", &nonce)
    )
    .await
    .unwrap());
    let hold = Arc::new(std::sync::Barrier::new(2));
    let handler = crate::ControlHandler::new(registry.clone())
        .with_supervisor(handle)
        .with_config_runtime(owner.clone())
        .with_hello_registry_to_forwarding_hold(hold.clone());
    let (sender, mut frames) = tokio::sync::mpsc::channel(8);
    let ctx = crate::RouteCtx {
        connection_id: ConnectionId::new(761),
        egress: crate::FrameSink::new(sender),
    };
    let runtime = tokio::runtime::Handle::current();
    let pending = std::thread::spawn(move || {
        runtime
            .block_on(
                handler.handle_control_frame(&ctx, module_hello(Some(nonce), ctx.connection_id)),
            )
            .unwrap()
    });
    hold.wait();
    let gate = owner.module_gate("a").unwrap();
    let gate_is_held = gate.try_lock().is_err();
    let base = crate::config_gate::program::approved_base(
        &crate::config_gate::GateSnapshot::from_store(
            &owner.lock().unwrap().store.body().unwrap().approved,
        )
        .unwrap(),
        "a",
    );
    owner
        .record_program_refusal(
            "a",
            &base,
            subc_config::raw_digest(b"refused after verified hello"),
        )
        .unwrap();
    let held_before_ack = owner.check_module_hold("a").is_err();
    hold.wait();
    let reply = pending.join().unwrap();
    let published = registry
        .published_registration(crate::registry::RegistrationSlot::Connection(
            ConnectionId::new(761),
        ))
        .unwrap()
        .is_some();
    let ack = frames.try_recv().unwrap().frame.header.ty;
    let held_after_ack = owner.check_module_hold("a").is_err();
    registry
        .deregister_connection(ConnectionId::new(761))
        .unwrap();
    let generation = module.status().unwrap().spawn_generation;
    let pid = module.status().unwrap().pid;
    let next = module.restart(None).await;
    let status = module.status().unwrap();
    module.stop().await.unwrap();
    assert!(gate_is_held && held_before_ack);
    assert!(reply.is_empty() && published);
    assert_eq!(ack, subc_protocol::FrameType::HelloAck);
    assert!(held_after_ack);
    assert!(
        matches!(next, Err(crate::SuperviseError::ConfigHeld { .. })),
        "next spawn must retain the hold: {next:?}"
    );
    assert!(status.process_alive);
    assert_eq!(status.pid, pid);
    assert_eq!(status.spawn_generation, generation);
    assert_eq!(status.restart_count, 0);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn config_recovery_late_swap_incumbent_cannot_erase_promoted_nonce_refusal() {
    for respawn in [false, true] {
        let rig = rig();
        wire_fixture_document(&rig, json!(["600"]));
        let path = rig.path.join("subc.jsonc");
        let mut doc: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        doc["modules"]["a"]["reserved"] = json!(true);
        doc["modules"]["a"]["overlap"] = json!("safe");
        std::fs::write(path, serde_json::to_vec(&doc).unwrap()).unwrap();
        let owner = Arc::new(rig.owner("late-incumbent"));
        rig.stamp(&owner);
        let registry = Arc::new(crate::Registry::default());
        let forwarding = Arc::new(crate::ForwardingTable::default());
        let handle = channel_handle();
        let supervisor = Supervisor::new_for_test(registry.clone(), RestartPolicy::default())
            .with_handle(handle.clone())
            .with_forwarding(forwarding.clone())
            .with_config_runtime(owner.clone())
            .with_drain_timeout(Duration::from_secs(30));
        let module = supervisor.spawn_async(spec(&rig)).await.unwrap();
        let incumbent_pid = reported_pid(&module).await;
        let incumbent_nonce = handle.registration_proof_for_test("a").unwrap();
        assert!(handle.wait_program_binding("a", &incumbent_nonce).await);
        let mut manifest = subc_protocol::manifest::ModuleManifest::builder("a", "1.0.0").build();
        manifest.ready = Some(true);
        let incumbent = ConnectionId::new(790);
        let candidate = ConnectionId::new(791);
        registry
            .register_with_control_ops(
                manifest.clone(),
                subc_protocol::PROTOCOL_VERSION,
                incumbent,
                vec![],
            )
            .unwrap();
        let (tx, _incumbent_frames) = tokio::sync::mpsc::channel(32);
        forwarding
            .register_module_connection(
                incumbent,
                "a".into(),
                subc_protocol::PROTOCOL_VERSION,
                subc_protocol::manifest::Concurrency::ModuleManaged,
                crate::FrameSink::new(tx),
            )
            .unwrap();
        let swapping = tokio::spawn({
            let module = module.clone();
            async move { module.swap(Some(Duration::from_secs(5))).await }
        });
        let candidate_nonce = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(nonce) = handle.candidate_registration_proof_for_test("a") {
                    if handle.has_live_verified_program_binding("a", &nonce) {
                        break nonce;
                    }
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        approve(&rig, &owner, PlanKind::Unstamp);
        registry
            .register_candidate_with_control_ops(
                manifest,
                subc_protocol::PROTOCOL_VERSION,
                candidate,
                vec![],
            )
            .unwrap();
        let (tx, _candidate_frames) = tokio::sync::mpsc::channel(32);
        forwarding
            .register_candidate_module_connection(
                candidate,
                "a".into(),
                subc_protocol::PROTOCOL_VERSION,
                subc_protocol::manifest::Concurrency::ModuleManaged,
                crate::FrameSink::new(tx),
            )
            .unwrap();
        assert!(swapping.await.unwrap().is_ok());
        assert_eq!(
            handle.registration_proof_for_test("a").as_deref(),
            Some(candidate_nonce.as_str())
        );
        registry.deregister_connection(candidate).unwrap();
        let fresh_nonce = if respawn {
            subc_os::Process::open(reported_pid(&module).await)
                .unwrap()
                .unwrap()
                .signal(subc_os::Signal::Kill)
                .unwrap();
            Some(
                tokio::time::timeout(Duration::from_secs(5), async {
                    loop {
                        if let Some(nonce) = handle.registration_proof_for_test("a") {
                            if nonce != candidate_nonce && module.status().unwrap().process_alive {
                                break nonce;
                            }
                        }
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("fresh plain respawn must run while the old incumbent drains"),
            )
        } else {
            module.stop().await.unwrap();
            None
        };
        assert!(handle.has_program_binding_record("a", &candidate_nonce));
        let handler = crate::ControlHandler::with_forwarding(registry.clone(), forwarding)
            .with_supervisor(handle.clone())
            .with_config_runtime(owner);
        if let Some(fresh) = &fresh_nonce {
            assert!(!handle.spawned_consumer_authorized("a", &incumbent_nonce));
            assert!(!handle.spawned_consumer_authorized("a", &candidate_nonce));
            assert!(!handle.spawned_consumer_authorized("a", fresh));
            let dead = handler
                .handle_control(
                    ConnectionId::new(792),
                    module_hello(Some(candidate_nonce.clone()), ConnectionId::new(792)),
                )
                .unwrap();
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&dead[0].body).unwrap()["code"],
                "registration_channel_closed"
            );
            let fresh_reply = handler
                .handle_control(
                    ConnectionId::new(793),
                    module_hello(Some(fresh.clone()), ConnectionId::new(793)),
                )
                .unwrap();
            assert_eq!(
                fresh_reply[0].header.ty,
                subc_protocol::FrameType::HelloAck,
                "a plain channel respawn registers while the old incumbent drains"
            );
            assert!(registry
                .get_module_by_connection(incumbent)
                .unwrap()
                .is_some());
        }
        // The old process drains in a detached task. Reap it only after the promoted
        // process has died, so its later retirement must not replace that refusal.
        registry.deregister_connection(incumbent).unwrap();
        subc_os::Process::open(incumbent_pid)
            .unwrap()
            .unwrap()
            .signal(subc_os::Signal::Kill)
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while handle.swap_open("a") {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        if fresh_nonce.is_some() {
            assert!(!handle.has_live_verified_program_binding("a", &candidate_nonce));
            assert!(!handle.has_live_verified_program_binding("a", &incumbent_nonce));
            assert_eq!(
                registry.get_module("a").unwrap().unwrap().connection_id,
                ConnectionId::new(793)
            );
            handler.cleanup_connection(ConnectionId::new(793)).unwrap();
            module.stop().await.unwrap();
        } else {
            assert!(!handle.reserved_hello_authorized("a", Some(&candidate_nonce)));
            assert!(!handle.spawned_consumer_authorized("a", &candidate_nonce));
            let reply = handler
                .handle_control(
                    ConnectionId::new(792),
                    module_hello(Some(candidate_nonce), ConnectionId::new(792)),
                )
                .unwrap();
            handler.cleanup_connection(ConnectionId::new(792)).unwrap();
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&reply[0].body).unwrap()["code"],
                "registration_channel_closed",
                "late incumbent retirement must preserve the promoted channel's refusal: {reply:?}"
            );
        }
    }
}

#[tokio::test]
async fn config_recovery_stamped_unapproved_actor_enable_is_refused() {
    let rig = rig();
    let owner = Arc::new(rig.owner("unapproved-enable"));
    rig.stamp(&owner);
    let mut unapproved = spec(&rig);
    unapproved.module_id = "not-approved".into();
    let supervisor = Supervisor::new_for_test(
        Arc::new(crate::Registry::default()),
        RestartPolicy::default(),
    )
    .with_config_runtime(owner);
    let module = supervisor
        .supervise_configured_requested_async(
            unapproved,
            false,
            crate::HealthConfig::default(),
            None,
            RestartPolicy::default(),
            Duration::from_secs(5),
            None,
        )
        .await
        .unwrap();
    let result = module.set_enabled(true).await;
    let state = module.status().unwrap();
    module.stop().await.unwrap();
    assert!(
        matches!(result, Err(crate::SuperviseError::ConfigHeld { ref code, .. }) if code == "config_gated"),
        "stamped-unapproved enable must refuse, not stage: {result:?}"
    );
    assert!(!state.enabled);
    assert!(!state.process_alive);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn config_recovery_carried_command_close_reap_retires_before_group_cleanup() {
    let rig = rig();
    wire_fixture_document(&rig, json!(["600"]));
    let path = rig.path.join("subc.jsonc");
    let mut doc: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    doc["modules"]["a"]["reserved"] = json!(true);
    doc["modules"]["a"]["carry_containment"] = json!(true);
    doc["modules"]["a"]["overlap"] = json!("exclusive");
    std::fs::write(path, serde_json::to_vec(&doc).unwrap()).unwrap();
    let owner = Arc::new(rig.owner("carried-command-close"));
    rig.stamp(&owner);
    let registry = Arc::new(crate::Registry::default());
    let handle = channel_handle();
    let supervisor = Supervisor::new_for_test(registry.clone(), RestartPolicy::default())
        .with_handle(handle.clone())
        .with_config_runtime(owner.clone());
    let mut module = supervisor.spawn_async(spec(&rig)).await.unwrap();
    reported_pid(&module).await;
    let nonce = handle.registration_proof_for_test("a").unwrap();
    assert!(handle.wait_program_binding("a", &nonce).await);
    let (reaped, release) = handle.pause_program_binding_exit_for_test("a", &nonce);
    approve(&rig, &owner, PlanKind::Unstamp);
    let closing = tokio::spawn({
        let handle = handle.clone();
        async move {
            handle.close_actor_commands_for_test(&mut module).await;
        }
    });
    tokio::time::timeout(Duration::from_secs(5), reaped)
        .await
        .unwrap()
        .unwrap();
    let handler = crate::ControlHandler::new(registry)
        .with_supervisor(handle)
        .with_config_runtime(owner);
    let reply = handler
        .handle_control(
            ConnectionId::new(794),
            module_hello(Some(nonce), ConnectionId::new(794)),
        )
        .unwrap();
    handler.cleanup_connection(ConnectionId::new(794)).unwrap();
    let _ = release.send(());
    tokio::time::timeout(Duration::from_secs(5), closing)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&reply[0].body).unwrap()["code"],
        "registration_channel_closed",
        "carried pin reap must retire before the paused recover_group await: {reply:?}"
    );
}
