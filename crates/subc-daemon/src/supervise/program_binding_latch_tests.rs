use super::*;

#[tokio::test]
async fn config_admission_post_exec_unattached_binding_never_publishes_ready() {
    let handle = SupervisorHandle::new();
    handle.set_spawn_nonce("binding-probe", "nonce".into());
    handle.set_reserved_nonce("binding-probe", "nonce".into());
    let publication = handle
        .begin_program_binding("binding-probe", SpawnRole::Plain)
        .unwrap();
    let registry = Arc::new(Registry::default());
    let handler = crate::ControlHandler::new(registry.clone()).with_supervisor(handle.clone());
    let hello = subc_protocol::ModuleHelloBody {
        manifest: subc_protocol::manifest::ModuleManifest::builder("binding-probe", "0.1.0")
            .build(),
        protocol_ver: subc_protocol::PROTOCOL_VERSION,
        control_ops: Some(vec!["config.review".into()]),
        launch_nonce: Some("nonce".into()),
    };
    let frame = subc_protocol::Frame::build(
        subc_protocol::FrameType::Hello,
        control_flags(),
        0,
        0,
        1,
        serde_json::to_vec(&hello).unwrap(),
    )
    .unwrap();
    let refused = handler
        .handle_control(crate::ConnectionId::new(1), frame.clone())
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&refused[0].body).unwrap()["code"],
        "program_refused"
    );
    assert!(registry.get_module("binding-probe").unwrap().is_none());
    let waiting = handle.wait_program_binding("binding-probe", "nonce");
    tokio::pin!(waiting);
    assert!(
        tokio::time::timeout(Duration::from_millis(10), &mut waiting)
            .await
            .is_err()
    );
    let _ = publication.verify();
    assert!(
        !waiting.await,
        "a missing live witness must never publish a verified binding"
    );
    let unbound = handler
        .handle_control(crate::ConnectionId::new(2), frame)
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&unbound[0].body).unwrap()["code"],
        "program_refused",
        "a ready latch without a current content witness must not authorize HELLO"
    );
    assert!(registry.get_module("binding-probe").unwrap().is_none());
    drop(publication);
    assert!(!handle.has_live_verified_program_binding("binding-probe", "nonce"));
}

#[test]
fn config_recovery_binding_tombstones_are_bounded_and_replaced_by_spawn() {
    let handle = SupervisorHandle::new();
    for index in 0..4 {
        let nonce = format!("retired-{index}");
        handle.set_spawn_nonce("bounded", nonce.clone());
        let publication = handle
            .begin_program_binding("bounded", SpawnRole::Plain)
            .unwrap();
        assert!(handle.program_bindings.lock().unwrap().retired.is_empty());
        drop(publication);
        let records = handle.program_bindings.lock().unwrap();
        assert!(records.active.is_empty());
        assert_eq!(records.retired.len(), 1);
        drop(records);
        assert!(handle.has_program_binding_record("bounded", &nonce));
        assert!(!handle.has_live_verified_program_binding("bounded", &nonce));
    }
    handle.retire("bounded");
    assert!(!handle.has_program_binding_record("bounded", "retired-3"));
}

#[test]
fn config_recovery_binding_retirement_is_nonce_specific_and_idempotent() {
    let handle = SupervisorHandle::new();
    handle.set_spawn_nonce("bounded", "old".into());
    let old = handle
        .begin_program_binding("bounded", SpawnRole::Plain)
        .unwrap();
    old.retire();
    handle.set_spawn_nonce("bounded", "fresh".into());
    assert!(!handle.has_program_binding_record("bounded", "old"));
    assert!(!handle.has_program_binding_record("bounded", "fresh"));
    let fresh = handle
        .begin_program_binding("bounded", SpawnRole::Plain)
        .unwrap();
    old.retire();
    assert!(handle.program_bindings.lock().unwrap().retired.is_empty());
    fresh.retire();
    drop(old);
    let records = handle.program_bindings.lock().unwrap();
    assert_eq!(records.retired.len(), 1);
    assert!(records
        .retired
        .contains_key(&("bounded".into(), "fresh".into())));
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn config_recovery_registration_deadline_reap_retires_before_probe_await() {
    let handle = SupervisorHandle::new();
    let spec = ModuleSpec {
        module_id: "deadline".into(),
        program: "/bin/sh".into(),
        args: vec!["-c".into(), "exit 0".into()],
        env: Vec::new(),
        reserved: false,
        reserved_prefixes: Vec::new(),
        protocol: ModuleProtocol::None,
        overlap: Default::default(),
        carry_containment: false,
    };
    let supervisor =
        Supervisor::new_for_test(Arc::new(Registry::default()), RestartPolicy::default());
    let mut child = spawn_and_mark_running(
        &spec,
        &supervisor.runtime_config(),
        &Arc::new(Mutex::new(SupervisorSnapshot::starting())),
    )
    .unwrap();
    handle.set_spawn_nonce("deadline", "dead".into());
    handle.set_reserved_nonce("deadline", "dead".into());
    child._program_binding = handle.begin_program_binding("deadline", SpawnRole::Plain);
    // Unstamp releases active witnesses; the still-owned child must recreate its
    // nonce-specific refusal as soon as the registration probe observes exit.
    handle.program_bindings.lock().unwrap().active.clear();
    tokio::time::timeout(Duration::from_secs(5), async {
        while subc_os::child_exit::observe(child.pid).unwrap().is_none() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(!registration_child_is_running(&mut child));
    let handler =
        crate::ControlHandler::new(Arc::new(Registry::default())).with_supervisor(handle.clone());
    let hello = subc_protocol::ModuleHelloBody {
        manifest: subc_protocol::manifest::ModuleManifest::builder("deadline", "1.0.0").build(),
        protocol_ver: subc_protocol::PROTOCOL_VERSION,
        control_ops: None,
        launch_nonce: Some("dead".into()),
    };
    let reply = handler
        .handle_control(
            crate::ConnectionId::new(97),
            subc_protocol::Frame::build(
                subc_protocol::FrameType::Hello,
                control_flags(),
                0,
                0,
                1,
                serde_json::to_vec(&hello).unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
    child.finish_exit(false).await.unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&reply[0].body).unwrap()["code"],
        "program_refused",
        "registration-deadline reap must revoke before health probing can await: {reply:?}"
    );
}

#[test]
fn config_recovery_retired_records_follow_all_accepted_swap_nonces_in_either_exit_order() {
    for candidate_exits_first in [false, true] {
        let handle = SupervisorHandle::new();
        handle.set_spawn_nonce("overlap", "a".into());
        handle.set_reserved_nonce("overlap", "a".into());
        let a = handle
            .begin_program_binding("overlap", SpawnRole::Plain)
            .unwrap();
        handle.open_swap("overlap", "b".into());
        let b = handle
            .begin_program_binding("overlap", SpawnRole::SwapCandidate)
            .unwrap();
        handle.promote_swap_nonce("overlap", true);
        handle.program_bindings.lock().unwrap().active.clear();
        if candidate_exits_first {
            b.retire();
            a.retire();
        } else {
            a.retire();
            b.retire();
        }
        assert!(handle.has_program_binding_record("overlap", "a"));
        assert!(handle.has_program_binding_record("overlap", "b"));
        assert_eq!(handle.program_bindings.lock().unwrap().retired.len(), 2);
        handle.set_spawn_nonce("overlap", "c".into());
        handle.set_reserved_nonce("overlap", "c".into());
        let c = handle
            .begin_program_binding("overlap", SpawnRole::Plain)
            .unwrap();
        c.retire();
        assert_eq!(handle.program_bindings.lock().unwrap().retired.len(), 3);
        handle.close_swap("overlap");
        a.retire();
        b.retire();
        assert!(!handle.has_program_binding_record("overlap", "a"));
        assert!(!handle.has_program_binding_record("overlap", "b"));
        assert!(handle.has_program_binding_record("overlap", "c"));
        assert_eq!(handle.program_bindings.lock().unwrap().retired.len(), 1);
        handle.set_spawn_nonce("overlap", "d".into());
        assert!(
            handle.has_program_binding_record("overlap", "c"),
            "reserved comparison still recognizes c until its own rotation"
        );
        handle.set_reserved_nonce("overlap", "d".into());
        assert!(handle.program_bindings.lock().unwrap().retired.is_empty());
    }
}

#[cfg(unix)]
#[test]
fn config_recovery_carried_retirement_hook_does_not_keep_publication_alive() {
    let handle = SupervisorHandle::new();
    handle.set_spawn_nonce("hook", "nonce".into());
    let publication = handle
        .begin_program_binding("hook", SpawnRole::Plain)
        .unwrap();
    let hook = publication.retirement_hook();
    drop(publication);
    assert!(hook.upgrade().is_none());
    assert!(handle.has_program_binding_record("hook", "nonce"));
    assert!(!handle.has_live_verified_program_binding("hook", "nonce"));
}
