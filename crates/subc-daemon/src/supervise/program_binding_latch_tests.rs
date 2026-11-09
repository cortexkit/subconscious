use super::*;

#[tokio::test]
async fn config_admission_hello_stays_unregistered_until_its_spawn_binding_passes() {
    let handle = SupervisorHandle::new();
    handle.set_spawn_nonce("binding-probe", "nonce".into());
    handle.set_reserved_nonce("binding-probe", "nonce".into());
    let mut publication = handle
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
    publication.verify();
    assert!(waiting.await);
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
    assert!(!handle.program_binding_verified("binding-probe", "nonce"));
}
