use super::*;
use subc_protocol::{Frame, FrameType};

include!("registration_drain_tests.rs");

fn hello(module: &str) -> Frame {
    let hello = subc_protocol::ModuleHelloBody {
        manifest: subc_protocol::manifest::ModuleManifest::builder(module, "1.0.0").build(),
        protocol_ver: subc_protocol::PROTOCOL_VERSION,
        control_ops: None,
        launch_nonce: None,
    };
    Frame::build(
        FrameType::Hello,
        super::super::control_flags(),
        0,
        0,
        1,
        serde_json::to_vec(&hello).unwrap(),
    )
    .unwrap()
}

fn auth() -> (crate::server::ServerAuth, subc_transport::ConnectionInfo) {
    let info = subc_transport::ConnectionInfo {
        schema: subc_transport::SCHEMA_VERSION,
        wire_version: None,
        endpoints: Vec::new(),
        key: vec![0x42; 32],
        daemon_id: [0x24; 16],
        pid: std::process::id(),
        daemon_ver: "test".into(),
    };
    (
        crate::server::ServerAuth::new(info.key.clone(), info.daemon_id, "test"),
        info,
    )
}

fn serve(
    handle: &SupervisorHandle,
    spawn: Arc<RegistrationSpawn>,
    auth: crate::server::ServerAuth,
) -> (
    Arc<crate::Router>,
    tokio::net::UnixStream,
    std::os::unix::net::UnixStream,
) {
    let control =
        crate::ControlHandler::new(Arc::new(Registry::default())).with_supervisor(handle.clone());
    serve_control(handle, spawn, auth, control)
}

fn serve_control(
    handle: &SupervisorHandle,
    spawn: Arc<RegistrationSpawn>,
    auth: crate::server::ServerAuth,
    control: crate::ControlHandler,
) -> (
    Arc<crate::Router>,
    tokio::net::UnixStream,
    std::os::unix::net::UnixStream,
) {
    let router = Arc::new(crate::Router::with_control_handler(Arc::new(control)));
    router.install_registration_service(auth);
    let (parent, child) = std::os::unix::net::UnixStream::pair().unwrap();
    let retained = parent.try_clone().unwrap();
    child.set_nonblocking(true).unwrap();
    let child = tokio::net::UnixStream::from_std(child).unwrap();
    handle.serve_registration(
        parent,
        spawn,
        std::process::id(),
        Instant::now() + Duration::from_secs(5),
    );
    (router, child, retained)
}

#[tokio::test]
async fn registration_channel_retirement_wins_during_hello_publication() {
    let handle = SupervisorHandle::new();
    let registry = Arc::new(Registry::default());
    let hold = Arc::new(std::sync::Barrier::new(2));
    let control = crate::ControlHandler::new(registry.clone())
        .with_supervisor(handle.clone())
        .with_hello_after_launch_check_hold(hold.clone());
    let spawn = RegistrationSpawn::new(&handle, "channel-module", SpawnRole::Plain, None);
    spawn.ready();
    let (auth, info) = auth();
    let (_router, mut child, _copy) = serve_control(&handle, spawn.clone(), auth, control);
    subc_transport::authenticate_client(&mut child, &info, Duration::from_secs(2))
        .await
        .unwrap();
    subc_transport::write_frame(&mut child, &hello("channel-module"))
        .await
        .unwrap();
    let barrier = hold.clone();
    tokio::task::spawn_blocking(move || {
        barrier.wait();
    })
    .await
    .unwrap();
    spawn.retire();
    let barrier = hold.clone();
    tokio::task::spawn_blocking(move || {
        barrier.wait();
        barrier.wait();
    })
    .await
    .unwrap();
    assert_eq!(
        registry.active_registration_count().unwrap(),
        0,
        "HELLO cannot publish after retirement won the launch transition"
    );
    assert_eq!(
        registry.generation().unwrap(),
        0,
        "a retired launch cannot briefly publish and then clean up"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn registration_channel_publication_lock_excludes_retirement_until_cleanup() {
    let handle = SupervisorHandle::new();
    let registry = Arc::new(Registry::default());
    let hold = Arc::new(std::sync::Barrier::new(2));
    let control = crate::ControlHandler::new(registry.clone())
        .with_supervisor(handle.clone())
        .with_hello_inside_publication_hold(hold.clone());
    let spawn = RegistrationSpawn::new(&handle, "channel-module", SpawnRole::Plain, None);
    spawn.ready();
    let (auth, info) = auth();
    let (_router, mut child, _copy) = serve_control(&handle, spawn.clone(), auth, control);
    subc_transport::authenticate_client(&mut child, &info, Duration::from_secs(2))
        .await
        .unwrap();
    subc_transport::write_frame(&mut child, &hello("channel-module"))
        .await
        .unwrap();
    let barrier = hold.clone();
    tokio::task::spawn_blocking(move || {
        barrier.wait();
    })
    .await
    .unwrap();
    assert_eq!(registry.active_registration_count().unwrap(), 1);
    let (started, entered) = tokio::sync::oneshot::channel();
    let mut retirement = tokio::task::spawn_blocking(move || {
        started.send(()).unwrap();
        spawn.retire();
    });
    entered.await.unwrap();
    let completed_early = tokio::time::timeout(Duration::from_millis(100), &mut retirement)
        .await
        .is_ok();
    let barrier = hold.clone();
    tokio::task::spawn_blocking(move || {
        barrier.wait();
    })
    .await
    .unwrap();
    if !completed_early {
        retirement.await.unwrap();
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        while registry.active_registration_count().unwrap() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("retirement must withdraw the published registration");
    assert!(
        !completed_early,
        "retirement must wait while publication owns the transition lock"
    );
    assert!(
        registry.generation().unwrap() >= 2,
        "the test must cover publication and its removal"
    );
}

#[tokio::test]
async fn registration_channel_binding_retirement_shuts_down_retained_endpoint() {
    let handle = SupervisorHandle::new();
    handle.set_spawn_nonce("channel-module", "binding-key".into());
    let publication = handle
        .begin_program_binding("channel-module", SpawnRole::Plain)
        .unwrap();
    let spawn = RegistrationSpawn::new(
        &handle,
        "channel-module",
        SpawnRole::Plain,
        Some(publication.clone()),
    );
    spawn.ready();
    let (auth, info) = auth();
    let (_router, mut child, _inherited_daemon_copy) = serve(&handle, spawn.clone(), auth);
    subc_transport::authenticate_client(&mut child, &info, Duration::from_secs(2))
        .await
        .unwrap();
    publication.retire();
    assert!(tokio::time::timeout(
        Duration::from_secs(1),
        subc_transport::read_frame(&mut child)
    )
    .await
    .expect("retirement must shut down even a retained daemon endpoint")
    .unwrap()
    .is_none());
    tokio::time::timeout(Duration::from_secs(1), spawn.wait_closed())
        .await
        .unwrap();
    assert!(!spawn.launch_ready());
}

#[tokio::test]
async fn registration_channel_hello_at_three_seconds_uses_five_second_spawn_budget() {
    let handle = SupervisorHandle::new();
    let spawn = RegistrationSpawn::new(&handle, "channel-module", SpawnRole::Plain, None);
    spawn.ready();
    let (auth, info) = auth();
    let (_router, mut child, _copy) = serve(&handle, spawn.clone(), auth);
    tokio::time::sleep(Duration::from_secs(3)).await;
    subc_transport::authenticate_client(&mut child, &info, Duration::from_secs(2))
        .await
        .expect("the registration budget, not the TCP pre-auth budget, allows slow module startup");
    subc_transport::write_frame(&mut child, &hello("channel-module"))
        .await
        .unwrap();
    let reply = subc_transport::read_frame(&mut child)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reply.header.ty, FrameType::HelloAck);
    spawn.retire();
}

#[tokio::test]
async fn registration_channel_pre_hello_ping_closes_endpoint() {
    let handle = SupervisorHandle::new();
    let spawn = RegistrationSpawn::new(&handle, "channel-module", SpawnRole::Plain, None);
    spawn.ready();
    let (auth, info) = auth();
    let (_router, mut child, _copy) = serve(&handle, spawn.clone(), auth);
    subc_transport::authenticate_client(&mut child, &info, Duration::from_secs(2))
        .await
        .unwrap();
    let ping = Frame::build(
        FrameType::Ping,
        super::super::control_flags(),
        0,
        0,
        1,
        vec![],
    )
    .unwrap();
    subc_transport::write_frame(&mut child, &ping)
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(
            Duration::from_secs(1),
            subc_transport::read_frame(&mut child)
        )
        .await
        .unwrap()
        .unwrap()
        .is_none(),
        "a pre-HELLO channel has no client or module operation surface"
    );
    assert!(!spawn.launch_ready());
}

#[tokio::test]
async fn registration_channel_refused_hello_closes_endpoint() {
    let handle = SupervisorHandle::new();
    let spawn = RegistrationSpawn::new(&handle, "channel-module", SpawnRole::Plain, None);
    spawn.ready();
    let (auth, info) = auth();
    let (_router, mut child, _copy) = serve(&handle, spawn.clone(), auth);
    subc_transport::authenticate_client(&mut child, &info, Duration::from_secs(2))
        .await
        .unwrap();
    subc_transport::write_frame(&mut child, &hello("wrong-module"))
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(
            Duration::from_secs(1),
            subc_transport::read_frame(&mut child)
        )
        .await
        .unwrap()
        .unwrap()
        .is_none(),
        "a refused one-shot HELLO ends the registration channel"
    );
    assert!(!spawn.launch_ready());
}

#[tokio::test]
async fn registration_channel_own_id_still_obeys_reserved_prefix_owner() {
    let handle = SupervisorHandle::new();
    handle.set_reserved_prefixes("prefix-owner", &["channel:".into()]);
    let spawn = RegistrationSpawn::new(&handle, "channel:module", SpawnRole::Plain, None);
    spawn.ready();
    let (auth, info) = auth();
    let (_router, mut child, _copy) = serve(&handle, spawn.clone(), auth);
    subc_transport::authenticate_client(&mut child, &info, Duration::from_secs(2))
        .await
        .unwrap();
    subc_transport::write_frame(&mut child, &hello("channel:module"))
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(
            Duration::from_secs(1),
            subc_transport::read_frame(&mut child)
        )
        .await
        .unwrap()
        .unwrap()
        .is_none(),
        "a spawn-bound module id does not own another module's reserved prefix"
    );
    assert!(!spawn.launch_ready());
}

#[tokio::test]
async fn registration_channel_candidate_cannot_register_after_swap_closes() {
    let handle = SupervisorHandle::new();
    handle.open_swap("channel-module", "candidate".into());
    let registry = Arc::new(Registry::default());
    let control = crate::ControlHandler::new(registry.clone()).with_supervisor(handle.clone());
    let spawn = RegistrationSpawn::new(&handle, "channel-module", SpawnRole::SwapCandidate, None);
    spawn.ready();
    let (auth, info) = auth();
    let (_router, mut child, _copy) = serve_control(&handle, spawn, auth, control);
    subc_transport::authenticate_client(&mut child, &info, Duration::from_secs(2))
        .await
        .unwrap();
    handle.close_swap("channel-module");
    subc_transport::write_frame(&mut child, &hello("channel-module"))
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(
            Duration::from_secs(1),
            subc_transport::read_frame(&mut child)
        )
        .await
        .unwrap()
        .unwrap()
        .is_none(),
        "a closed swap cannot admit a late candidate HELLO"
    );
    assert!(registry.get_candidate("channel-module").unwrap().is_none());
}

#[tokio::test]
async fn registration_channel_gated_without_binding_never_admits_hello() {
    let handle = SupervisorHandle::new();
    let spawn = RegistrationSpawn::new_with_binding_requirement(
        &handle,
        "channel-module",
        SpawnRole::Plain,
        None,
        true,
    );
    spawn.ready();
    let (auth, info) = auth();
    let (_router, mut child, _copy) = serve(&handle, spawn.clone(), auth);
    subc_transport::authenticate_client(&mut child, &info, Duration::from_secs(2))
        .await
        .unwrap();
    subc_transport::write_frame(&mut child, &hello("channel-module"))
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(
            Duration::from_secs(1),
            subc_transport::read_frame(&mut child)
        )
        .await
        .unwrap()
        .unwrap()
        .is_none(),
        "launch readiness alone cannot replace a gated program binding"
    );
    assert!(!spawn.launch_ready());
}

#[tokio::test]
async fn registration_channel_nonce_registration_retires_unused_endpoint() {
    tcp_registration_retires_unused_endpoint(true).await;
}

#[tokio::test]
async fn registration_channel_nonce_less_registration_retires_unused_endpoint() {
    tcp_registration_retires_unused_endpoint(false).await;
}

async fn tcp_registration_retires_unused_endpoint(present_nonce: bool) {
    let handle = SupervisorHandle::new();
    handle.set_spawn_nonce("channel-module", "legacy-test-nonce".into());
    if present_nonce {
        handle.set_reserved_nonce("channel-module", "legacy-test-nonce".into());
    }
    let spawn = RegistrationSpawn::new(&handle, "channel-module", SpawnRole::Plain, None);
    spawn.ready();
    let (auth, info) = auth();
    let (router, mut channel, _copy) = serve(&handle, spawn.clone(), auth.clone());
    subc_transport::authenticate_client(&mut channel, &info, Duration::from_secs(2))
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(crate::server::serve_listener(listener, router, auth));
    let mut nonce_connection = tokio::net::TcpStream::connect(address).await.unwrap();
    subc_transport::authenticate_client(&mut nonce_connection, &info, Duration::from_secs(2))
        .await
        .unwrap();
    let mut nonce_hello = hello("channel-module");
    let mut body: subc_protocol::ModuleHelloBody =
        serde_json::from_slice(&nonce_hello.body).unwrap();
    body.launch_nonce = present_nonce.then(|| "legacy-test-nonce".into());
    nonce_hello = Frame::build(
        FrameType::Hello,
        super::super::control_flags(),
        0,
        0,
        1,
        serde_json::to_vec(&body).unwrap(),
    )
    .unwrap();
    subc_transport::write_frame(&mut nonce_connection, &nonce_hello)
        .await
        .unwrap();
    assert_eq!(
        subc_transport::read_frame(&mut nonce_connection)
            .await
            .unwrap()
            .unwrap()
            .header
            .ty,
        FrameType::HelloAck
    );
    assert!(tokio::time::timeout(
        Duration::from_secs(1),
        subc_transport::read_frame(&mut channel)
    )
    .await
    .expect("nonce registration must close its unused channel immediately")
    .unwrap()
    .is_none());
    let _ = subc_transport::write_frame(&mut channel, &hello("channel-module")).await;
    assert!(subc_transport::read_frame(&mut channel)
        .await
        .unwrap()
        .is_none());
    assert!(!spawn.launch_ready());
    server.abort();
}

#[tokio::test]
async fn registration_channel_second_hello_finds_closed_endpoint() {
    let handle = SupervisorHandle::new();
    let spawn = RegistrationSpawn::new(&handle, "channel-module", SpawnRole::Plain, None);
    spawn.ready();
    let (auth, info) = auth();
    let (_router, mut channel, _copy) = serve(&handle, spawn.clone(), auth);
    subc_transport::authenticate_client(&mut channel, &info, Duration::from_secs(2))
        .await
        .unwrap();
    subc_transport::write_frame(&mut channel, &hello("channel-module"))
        .await
        .unwrap();
    assert_eq!(
        subc_transport::read_frame(&mut channel)
            .await
            .unwrap()
            .unwrap()
            .header
            .ty,
        FrameType::HelloAck
    );
    subc_transport::write_frame(&mut channel, &hello("channel-module"))
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(
            Duration::from_secs(1),
            subc_transport::read_frame(&mut channel)
        )
        .await
        .unwrap()
        .unwrap()
        .is_none(),
        "a second HELLO cannot leave an accepted channel usable"
    );
    assert!(!spawn.launch_ready());
}

async fn nonce_hello_over_tcp(handle: SupervisorHandle, module: &str, nonce: &str) -> Frame {
    let control = crate::ControlHandler::new(Arc::new(Registry::default())).with_supervisor(handle);
    let router = Arc::new(crate::Router::with_control_handler(Arc::new(control)));
    let (auth, info) = auth();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(crate::server::serve_listener(listener, router, auth));
    let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
    subc_transport::authenticate_client(&mut client, &info, Duration::from_secs(2))
        .await
        .unwrap();
    let mut body: subc_protocol::ModuleHelloBody =
        serde_json::from_slice(&hello(module).body).unwrap();
    body.launch_nonce = Some(nonce.into());
    let frame = Frame::build(
        FrameType::Hello,
        super::super::control_flags(),
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
async fn registration_nonce_policy_empty_swap_token_never_authorizes_prefix() {
    let handle = SupervisorHandle::new();
    handle.set_reserved_prefixes("prefix-owner", &["prefix:".into()]);
    handle.open_swap("prefix-owner", String::new());
    let reply = nonce_hello_over_tcp(handle, "prefix:child", "").await;
    assert_eq!(
        reply.header.ty,
        FrameType::Error,
        "an empty candidate token is not a prefix credential"
    );
}

#[tokio::test]
async fn registration_nonce_policy_empty_current_token_never_authorizes_prefix() {
    let handle = SupervisorHandle::new();
    handle.set_reserved_prefixes("prefix-owner", &["prefix:".into()]);
    handle.set_spawn_nonce("prefix-owner", String::new());
    let reply = nonce_hello_over_tcp(handle, "prefix:child", "").await;
    assert_eq!(reply.header.ty, FrameType::Error);
}

#[tokio::test]
async fn registration_nonce_policy_empty_exact_token_never_authorizes_hello() {
    let handle = SupervisorHandle::new();
    handle.set_reserved_nonce("channel-module", String::new());
    let reply = nonce_hello_over_tcp(handle, "channel-module", "").await;
    assert_eq!(reply.header.ty, FrameType::Error);
}

#[tokio::test]
async fn registration_nonce_policy_empty_candidate_token_never_authorizes_hello() {
    let handle = SupervisorHandle::new();
    handle.open_swap("channel-module", String::new());
    let reply = nonce_hello_over_tcp(handle, "channel-module", "").await;
    assert_eq!(reply.header.ty, FrameType::Error);
}

async fn activity_identity_over_tcp(handle: SupervisorHandle, token: &str) -> serde_json::Value {
    let control = crate::ControlHandler::new(Arc::new(Registry::default())).with_supervisor(handle);
    let router = Arc::new(crate::Router::with_control_handler(Arc::new(control)));
    let (auth, info) = auth();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(crate::server::serve_listener(listener, router, auth));
    let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
    subc_transport::authenticate_client(&mut client, &info, Duration::from_secs(2))
        .await
        .unwrap();
    let request = Frame::build(
        FrameType::Request,
        super::super::control_flags(),
        0,
        0,
        1,
        serde_json::to_vec(&serde_json::json!({"op":"activity.query", "limit":1,
            "consumer_identity":{"module_id":"callosum", "launch_nonce":token}}))
        .unwrap(),
    )
    .unwrap();
    subc_transport::write_frame(&mut client, &request)
        .await
        .unwrap();
    let reply = subc_transport::read_frame(&mut client)
        .await
        .unwrap()
        .unwrap();
    server.abort();
    serde_json::from_slice(&reply.body).unwrap()
}

#[tokio::test]
async fn registration_nonce_policy_empty_current_token_never_attests_consumer() {
    let handle = SupervisorHandle::new();
    handle.set_spawn_nonce("callosum", String::new());
    assert_eq!(
        activity_identity_over_tcp(handle, "").await["code"],
        "activity_read_forbidden"
    );
}

#[tokio::test]
async fn registration_nonce_policy_empty_promotion_clears_spawn_attestation() {
    let handle = SupervisorHandle::new();
    handle.set_spawn_nonce("callosum", "incumbent".into());
    handle.open_swap("callosum", String::new());
    handle.promote_swap_nonce("callosum", false);
    handle.close_swap("callosum");
    assert_eq!(
        activity_identity_over_tcp(handle, "incumbent").await["code"],
        "activity_read_forbidden"
    );
}

#[tokio::test]
async fn registration_nonce_policy_empty_promotion_clears_incumbent_swap_attestation() {
    let handle = SupervisorHandle::new();
    handle.set_spawn_nonce("callosum", "incumbent".into());
    handle.open_swap("callosum", String::new());
    handle.promote_swap_nonce("callosum", false);
    assert_eq!(
        activity_identity_over_tcp(handle, "incumbent").await["code"],
        "activity_read_forbidden"
    );
}

#[tokio::test]
async fn registration_nonce_policy_empty_promotion_clears_incumbent_authority() {
    let handle = SupervisorHandle::new();
    handle.set_spawn_nonce("channel-module", "old-incumbent".into());
    handle.set_reserved_nonce("channel-module", "old-incumbent".into());
    handle.open_swap("channel-module", String::new());
    handle.promote_swap_nonce("channel-module", true);
    handle.close_swap("channel-module");
    let reply = nonce_hello_over_tcp(handle, "channel-module", "old-incumbent").await;
    assert_eq!(
        reply.header.ty,
        FrameType::Error,
        "gated promotion must not retain an incumbent nonce"
    );
}

#[tokio::test]
async fn registration_channel_failed_ungated_service_keeps_nonce_launch_alive() {
    let handle = SupervisorHandle::new();
    let registry = Arc::new(Registry::default());
    let supervisor = Supervisor::new_for_test(registry, RestartPolicy::new(0, Duration::ZERO))
        .with_handle(handle.clone());
    let module = supervisor
        .spawn(ModuleSpec {
            module_id: "legacy-channel-failure".into(),
            program: "/bin/sleep".into(),
            args: vec!["600".into()],
            env: vec![],
            reserved: true,
            reserved_prefixes: vec![],
            protocol: ModuleProtocol::Subc,
            overlap: Default::default(),
            carry_containment: false,
        })
        .unwrap();
    let nonce = handle.spawn_launch_nonce_for(module.module_id()).unwrap();
    let spawn = handle
        .registration_launches
        .lock()
        .unwrap()
        .last()
        .unwrap()
        .upgrade()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), spawn.wait_closed())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_secs(3)).await;
    let alive = module.status().unwrap().process_alive;
    let reply = nonce_hello_over_tcp(handle, module.module_id(), &nonce).await;
    module.stop().await.unwrap();
    assert!(
        alive,
        "an ungated child must survive failure of its unused channel service"
    );
    assert_eq!(
        reply.header.ty,
        FrameType::HelloAck,
        "the older SDK still registers over its nonce"
    );
}

#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn registration_channel_macos_direct_exec_reap_never_becomes_ready() {
    let handle = SupervisorHandle::new();
    let (auth, info) = auth();
    let scratch = cortexkit_test_support::ScratchDir::new("direct-exec-reap");
    let spec = ModuleSpec {
        module_id: "channel-module".into(),
        program: "/bin/sleep".into(),
        args: vec!["2".into()],
        env: [
            "HOME",
            "XDG_DATA_HOME",
            "XDG_RUNTIME_DIR",
            "XDG_CONFIG_HOME",
        ]
        .into_iter()
        .map(|key| (key.into(), scratch.display().to_string()))
        .collect(),
        reserved: false,
        reserved_prefixes: vec![],
        protocol: ModuleProtocol::None,
        overlap: Default::default(),
        carry_containment: false,
    };
    let runtime = Supervisor::new_for_test(Arc::new(Registry::default()), RestartPolicy::default())
        .runtime_config();
    runtime
        .child_roster
        .set_privacy_trampoline(Ok(test_privacy_trampoline()));
    let mut child = spawn_child_in_slot_async(&spec, &runtime, SpawnRole::Plain, false)
        .await
        .unwrap();
    child.privacy_exec.as_mut().unwrap().image_probe = Some(|pid| {
        assert!(
            observe_spawned_image(pid).is_some(),
            "probe must run before the child exits"
        );
        None
    });
    let spawn = RegistrationSpawn::new(&handle, "channel-module", SpawnRole::Plain, None);
    child.registration_spawn = Some(spawn.clone());
    let (_router, mut channel, _copy) = serve(&handle, spawn.clone(), auth);
    subc_transport::authenticate_client(&mut channel, &info, Duration::from_secs(2))
        .await
        .unwrap();
    subc_transport::write_frame(&mut channel, &hello("channel-module"))
        .await
        .unwrap();
    child.confirm_privacy_exec().await;
    assert!(
        !spawn.launch_ready(),
        "observed reap must be terminal before launch confirmation finishes"
    );
    assert!(tokio::time::timeout(
        Duration::from_secs(1),
        subc_transport::read_frame(&mut channel)
    )
    .await
    .unwrap()
    .unwrap()
    .is_none());
    child.finish_exit(false).await.unwrap();
}
