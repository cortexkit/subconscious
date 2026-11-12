use super::*;

fn spec(root: &std::path::Path, protocol: ModuleProtocol, reserved: bool) -> ModuleSpec {
    ModuleSpec {
        module_id: "protocol-transition".into(),
        program: "/bin/sleep".into(),
        args: vec!["600".into()],
        env: [
            "HOME",
            "XDG_DATA_HOME",
            "XDG_CONFIG_HOME",
            "XDG_RUNTIME_DIR",
        ]
        .into_iter()
        .map(|name| (name.into(), root.to_string_lossy().into_owned()))
        .collect(),
        reserved,
        reserved_prefixes: vec![],
        protocol,
        overlap: Default::default(),
        carry_containment: false,
    }
}

fn supervisor(handle: &SupervisorHandle) -> Supervisor {
    handle.capture_registration_channels_for_test();
    Supervisor::new_for_test(Arc::new(Registry::default()), RestartPolicy::default())
        .with_handle(handle.clone())
}

#[tokio::test]
async fn audit2_subc_to_none_rescan_preserves_the_running_nonce() {
    let root = cortexkit_test_support::ScratchDir::new("subc-to-none-identity");
    let handle = SupervisorHandle::new();
    let supervisor = supervisor(&handle);
    let initial = spec(&root, ModuleProtocol::Subc, false);
    let module = supervisor.spawn_async(initial.clone()).await.unwrap();
    let nonce = handle.spawn_nonce(&initial.module_id).unwrap();
    let mut next = initial;
    next.protocol = ModuleProtocol::None;
    module
        .update_configuration(next.clone(), HealthConfig::default(), None)
        .await
        .unwrap();
    assert_eq!(module.declared_protocol().unwrap(), ModuleProtocol::Subc);
    let retained = handle.spawn_nonce(&next.module_id);
    let consumer_authorized = handle.spawned_consumer_authorized(&next.module_id, &nonce);
    module.stop().await.unwrap();
    assert_eq!(
        retained.as_deref(),
        Some(nonce.as_str()),
        "launch changes are staged, not immediate revocations"
    );
    assert!(
        consumer_authorized,
        "the still-running subc process keeps its consumer authority"
    );
}

#[tokio::test]
async fn audit2_none_to_subc_releases_only_the_implicit_reservation() {
    for reserved in [false, true] {
        let root = cortexkit_test_support::ScratchDir::new("none-to-subc-identity");
        let handle = SupervisorHandle::new();
        let supervisor = supervisor(&handle);
        let initial = spec(&root, ModuleProtocol::None, reserved);
        let module = supervisor.spawn_async(initial.clone()).await.unwrap();
        assert!(!handle.reserved_hello_authorized(&initial.module_id, None));
        let mut next = initial;
        next.protocol = ModuleProtocol::Subc;
        next.reserved = false;
        module
            .update_configuration(next.clone(), HealthConfig::default(), None)
            .await
            .unwrap();
        assert_eq!(module.declared_protocol().unwrap(), ModuleProtocol::None);
        assert!(
            !handle.reserved_hello_authorized(&next.module_id, None),
            "the protocol-none process is still running before replacement"
        );
        module.stop().await.unwrap();
        let replacement = supervisor.spawn_async(next.clone()).await.unwrap();
        let nonce = handle.spawn_nonce(&next.module_id).unwrap();
        let without_nonce = handle.reserved_hello_authorized(&next.module_id, None);
        let with_nonce = handle.reserved_hello_authorized(&next.module_id, Some(&nonce));
        replacement.stop().await.unwrap();
        assert_eq!(
            without_nonce, !reserved,
            "only an operator-configured reservation survives a protocol-none replacement"
        );
        if !reserved {
            assert!(with_nonce);
        }
    }
}

#[tokio::test]
async fn audit2_protocol_none_owner_cannot_authorize_prefix_children_with_swap_nonce() {
    let root = cortexkit_test_support::ScratchDir::new("none-swap-identity");
    let handle = SupervisorHandle::new();
    let supervisor = supervisor(&handle);
    let mut initial = spec(&root, ModuleProtocol::None, false);
    initial.reserved_prefixes = vec!["implicit:".into()];
    let module = supervisor.spawn_async(initial.clone()).await.unwrap();
    handle.open_swap(&initial.module_id, "candidate-token".into());
    let token_matches = handle.swap_nonce_matches(&initial.module_id, "candidate-token");
    let prefix_authorized =
        handle.reserved_hello_authorized("implicit:child", Some("candidate-token"));
    module.stop().await.unwrap();
    assert!(
        !token_matches,
        "protocol:none has no nonce authority, including swap tokens"
    );
    assert!(!prefix_authorized);
}

async fn restart_with_staged_protocol(module: &SupervisedModule, next: ModuleSpec) {
    let generation = module.status().unwrap().spawn_generation;
    module
        .update_configuration(next.clone(), HealthConfig::default(), None)
        .await
        .unwrap();
    module.restart(None).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let status = module.status().unwrap();
            if status.state == ModuleState::Running && status.spawn_generation > generation {
                assert_eq!(status.protocol, next.protocol);
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the same supervised module must finish its staged restart");
}

fn legacy_hello(module: &str, nonce: &str) -> subc_protocol::Frame {
    subc_protocol::Frame::build(
        subc_protocol::FrameType::Hello,
        subc_protocol::Flags::new(false, subc_protocol::Priority::Passive, false),
        0,
        0,
        1,
        serde_json::to_vec(&subc_protocol::ModuleHelloBody {
            manifest: subc_protocol::manifest::ModuleManifest::builder(module, "1.0.0").build(),
            protocol_ver: subc_protocol::PROTOCOL_VERSION,
            control_ops: None,
            launch_nonce: Some(nonce.into()),
        })
        .unwrap(),
    )
    .unwrap()
}

#[tokio::test]
async fn audit3_in_place_restart_to_none_retires_nonce_and_reserves_identity() {
    let root = cortexkit_test_support::ScratchDir::new("restart-subc-to-none");
    let handle = SupervisorHandle::new();
    let supervisor = supervisor(&handle);
    let initial = spec(&root, ModuleProtocol::Subc, false);
    let module = supervisor.spawn_async(initial.clone()).await.unwrap();
    let old_nonce = handle.spawn_nonce(&initial.module_id).unwrap();
    let mut next = initial;
    next.protocol = ModuleProtocol::None;
    restart_with_staged_protocol(&module, next.clone()).await;
    let handler =
        crate::ControlHandler::new(supervisor.registry.clone()).with_supervisor(handle.clone());
    let reply = handler
        .handle_control(
            ConnectionId::new(805),
            legacy_hello(&next.module_id, &old_nonce),
        )
        .unwrap();
    let nonce = handle.spawn_nonce(&next.module_id);
    let implicit = handle
        .implicit_protocol_none
        .lock()
        .unwrap()
        .contains(&next.module_id);
    module.stop().await.unwrap();
    assert!(
        nonce.is_none(),
        "in-place replacement must remove the old spawn nonce"
    );
    assert!(
        implicit,
        "the none replacement needs its implicit no-holder reservation"
    );
    assert_eq!(reply[0].header.ty, subc_protocol::FrameType::Error);
}

#[tokio::test]
async fn audit3_in_place_restart_to_unreserved_subc_releases_implicit_reservation() {
    let root = cortexkit_test_support::ScratchDir::new("restart-none-to-subc");
    let handle = SupervisorHandle::new();
    let supervisor = supervisor(&handle);
    let initial = spec(&root, ModuleProtocol::None, false);
    let module = supervisor.spawn_async(initial.clone()).await.unwrap();
    let mut next = initial;
    next.protocol = ModuleProtocol::Subc;
    restart_with_staged_protocol(&module, next.clone()).await;
    let nonce = handle
        .spawn_nonce(&next.module_id)
        .expect("the subc replacement gets a fresh legacy nonce");
    let unreserved = handle.reserved_hello_authorized(&next.module_id, None);
    let implicit = handle
        .implicit_protocol_none
        .lock()
        .unwrap()
        .contains(&next.module_id);
    let handler =
        crate::ControlHandler::new(supervisor.registry.clone()).with_supervisor(handle.clone());
    let reply = handler
        .handle_control(
            ConnectionId::new(806),
            legacy_hello(&next.module_id, &nonce),
        )
        .unwrap();
    handler.cleanup_connection(ConnectionId::new(806)).unwrap();
    module.stop().await.unwrap();
    assert!(
        unreserved,
        "the old implicit reservation is not an operator reservation"
    );
    assert!(!implicit);
    assert_eq!(reply[0].header.ty, subc_protocol::FrameType::HelloAck);
}

#[tokio::test]
async fn audit3_protocol_none_owner_refuses_a_stale_prefix_spawn_nonce() {
    let root = cortexkit_test_support::ScratchDir::new("none-prefix-nonce");
    let handle = SupervisorHandle::new();
    let supervisor = supervisor(&handle);
    let mut initial = spec(&root, ModuleProtocol::None, false);
    initial.reserved_prefixes = vec!["implicit:".into()];
    let module = supervisor.spawn_async(initial.clone()).await.unwrap();
    handle.set_spawn_nonce(&initial.module_id, "stale-owner-nonce".into());
    let authorized = handle.reserved_hello_authorized("implicit:child", Some("stale-owner-nonce"));
    module.stop().await.unwrap();
    assert!(
        !authorized,
        "a protocol-none owner cannot authorize a prefix child through a stale spawn nonce"
    );
}
