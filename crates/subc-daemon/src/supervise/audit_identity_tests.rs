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
