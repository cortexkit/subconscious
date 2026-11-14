use super::*;
#[cfg(unix)]
use crate::FrameSink;
#[cfg(unix)]
use serde_json::json;

#[test]
fn config_admission_gate_capability_requires_explicit_store_activation() {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/scratch")
        .join(format!("config-advertisement-{}", std::process::id()));
    std::fs::create_dir_all(&path).unwrap();
    let owner = Arc::new(crate::config_gate::runtime::RuntimeOwner::new(
        "advertisement".into(),
        crate::config_store::ConfigStore::open_digest_only_for_tests(
            crate::config_store::StoreRoot::from_service_definition(path.clone()).unwrap(),
        ),
        crate::config_gate::capture::ConfigCapturer {
            daemon_path: path.join("subc.jsonc"),
            config_home: path.clone(),
            projects: Arc::new(crate::config_gate::capture::UnavailableProjectRegistry),
        },
    ));
    let plain = ControlHandler::new(Arc::new(Registry::default()));
    let frame = Frame::build(FrameType::Hello, control_flags(), 0, 0, 7, vec![]).unwrap();
    let original = plain
        .build_hello_ack(&frame, subc_protocol::PROTOCOL_VERSION, "probe")
        .unwrap();
    let embedded = plain.clone().with_config_runtime(owner.clone());
    assert_eq!(
        embedded
            .build_hello_ack(&frame, subc_protocol::PROTOCOL_VERSION, "probe")
            .unwrap()
            .body,
        original.body,
        "installing an embedded runtime alone must not advertise daemon gate activation"
    );
    let gated = plain
        .clone()
        .with_explicit_config_store(owner.clone())
        .with_explicit_config_store(owner);
    let activated = gated
        .build_hello_ack(&frame, subc_protocol::PROTOCOL_VERSION, "probe")
        .unwrap();
    let mut body: ModuleHelloAckBody = serde_json::from_slice(&activated.body).unwrap();
    assert_eq!(
        body.subc_capabilities
            .iter()
            .filter(|capability| capability.as_str() == "config.gate")
            .count(),
        1
    );
    body.subc_capabilities
        .retain(|capability| capability != "config.gate");
    assert_eq!(
        serde_json::to_vec(&body).unwrap(),
        original.body,
        "activation must add only config.gate, preserving every legacy ACK byte otherwise"
    );
    assert_eq!(
        plain
            .build_hello_ack(&frame, subc_protocol::PROTOCOL_VERSION, "probe")
            .unwrap()
            .body,
        original.body,
        "the no-runtime handler must remain byte-for-byte unchanged"
    );
    assert!(
        !serde_json::from_slice::<ModuleHelloAckBody>(&original.body)
            .unwrap()
            .subc_capabilities
            .iter()
            .any(|capability| capability == "config.gate")
    );
    std::fs::remove_dir_all(path).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn config_admission_production_reviewer_requires_the_active_live_child_nonce_and_image() {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/scratch")
        .join(format!("config-review-admission-{}", std::process::id()));
    std::fs::create_dir_all(path.join("store")).unwrap();
    let program = crate::config_gate::program::native_fixture_for_test();
    std::fs::write(
        path.join("subc.jsonc"),
        serde_json::to_vec(
            &json!({"version":1,"modules":{"a":{"program":program,"args":["sleep"],"reserved":true}}}),
        )
        .unwrap(),
    )
    .unwrap();
    let owner = Arc::new(crate::config_gate::runtime::RuntimeOwner::new(
        "review-admission".into(),
        crate::config_store::ConfigStore::open_digest_only_for_tests(
            crate::config_store::StoreRoot::from_service_definition(path.join("store")).unwrap(),
        ),
        crate::config_gate::capture::ConfigCapturer {
            daemon_path: path.join("subc.jsonc"),
            config_home: path.clone(),
            projects: Arc::new(crate::config_gate::capture::UnavailableProjectRegistry),
        },
    ));
    let registry = Arc::new(Registry::default());
    let handle = SupervisorHandle::new();
    let supervisor = crate::supervise::Supervisor::new_for_test(
        registry.clone(),
        crate::supervise::RestartPolicy::default(),
    )
    .with_handle(handle.clone())
    .with_config_runtime(owner.clone());
    let config = crate::daemon_config::parse_with_activity(
        &std::fs::read_to_string(path.join("subc.jsonc")).unwrap(),
        path.join("subc.jsonc"),
    )
    .unwrap()
    .0;
    let module = supervisor
        .spawn_async(config.modules[0].module_spec())
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while module.status().unwrap().pid.is_none() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let handler = ControlHandler::new(registry.clone())
        .with_config_runtime(owner)
        .with_supervisor(handle.clone());
    let nonce = handle.spawn_launch_nonce_for("a").unwrap();
    assert!(tokio::time::timeout(
        std::time::Duration::from_secs(5),
        handle.wait_program_binding_for_test("a", &nonce)
    )
    .await
    .unwrap());
    let hello = subc_protocol::ModuleHelloBody {
        manifest: ModuleManifest::builder("a", "0.1.0").build(),
        protocol_ver: subc_protocol::PROTOCOL_VERSION,
        control_ops: Some(vec!["config.review".into()]),
        launch_nonce: handle.spawn_launch_nonce_for("a"),
    };
    let frame = Frame::build(
        FrameType::Hello,
        control_flags(),
        0,
        0,
        1,
        serde_json::to_vec(&hello).unwrap(),
    )
    .unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    assert!(handler
        .handle_hello(ConnectionId::new(10), Some(FrameSink::new(tx)), frame)
        .unwrap()
        .is_empty());
    assert_eq!(
        rx.recv().await.unwrap().frame.header.ty,
        FrameType::HelloAck
    );
    let registration = registry.get_module("a").unwrap().unwrap();
    let admitted = handler.review_admission().check(&registration);
    let observed_status = module.status().unwrap();
    let observed_start = observed_status
        .pid
        .and_then(crate::provenance::process_start_time);
    let stderr = module.stderr_tail(None, None);
    let expected = crate::config_gate::program::capture(&config.modules[0].module_spec(), &[])
        .map(|launch| launch.image);
    let observed = observed_status
        .pid
        .and_then(|pid| subc_os::Process::open(pid).ok().flatten())
        .and_then(|process| process.observe());
    let mut forged = registration.clone();
    forged.connection_id = ConnectionId::new(11);
    assert_eq!(
        handler.review_admission().check(&forged).unwrap_err().code,
        "program_refused"
    );
    handler.handle_goodbye(ConnectionId::new(10)).unwrap();
    module.stop().await.unwrap();
    assert!(admitted.is_ok(), "{admitted:?}; status {observed_status:?}; observed_start {observed_start:?}; stderr {stderr:?}; expected {expected:?}; observed {observed:?}");
    assert_eq!(
        handler
            .review_admission()
            .check(&registration)
            .unwrap_err()
            .code,
        "program_refused"
    );
    std::fs::remove_dir_all(path).unwrap();
}
