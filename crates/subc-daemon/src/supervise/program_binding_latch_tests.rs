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
    assert!(handle.has_program_binding_record("bounded", "old"));
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
    assert_eq!(records.retired["bounded"].nonce, "fresh");
}
