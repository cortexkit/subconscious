use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn activity_bootstrap_installs_the_run_directory_ledger() {
    let scratch = cortexkit_test_support::ScratchDir::new("activity-bootstrap");
    let connection_path = scratch.path().join("connection.json");
    let config = BootstrapConfig::new(&connection_path, 0)
        .with_terminal_journal_path(scratch.path().join("terminals.jsonl"));
    let daemon = tokio::spawn(run_with_config(config));
    tokio::time::timeout(Duration::from_secs(5), async {
        while !connection_path.exists() {
            assert!(
                !daemon.is_finished(),
                "daemon must publish its connection file"
            );
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let info = subc_transport::connection_file::read(&connection_path).unwrap();
    let mut module = connect_from_info(&info).await.unwrap();
    subc_transport::authenticate_client(&mut module, &info, Duration::from_secs(5))
        .await
        .unwrap();
    let hello = subc_protocol::ModuleHelloBody {
        manifest: subc_protocol::manifest::ModuleManifest::builder("worker", "0.1.0")
            .protocol_ver(PROTOCOL_VERSION)
            .build(),
        protocol_ver: PROTOCOL_VERSION,
        control_ops: None,
        launch_nonce: None,
    };
    let frame = |ty, corr, body| {
        crate::Frame::build(
            ty,
            subc_protocol::Flags::new(false, subc_protocol::Priority::Interactive, false),
            0,
            0,
            corr,
            body,
        )
        .unwrap()
    };
    crate::write_frame(
        &mut module,
        &frame(
            subc_protocol::FrameType::Hello,
            1,
            serde_json::to_vec(&hello).unwrap(),
        ),
    )
    .await
    .unwrap();
    assert_eq!(
        crate::read_frame(&mut module)
            .await
            .unwrap()
            .unwrap()
            .header
            .ty,
        subc_protocol::FrameType::HelloAck
    );
    crate::write_frame(&mut module, &frame(subc_protocol::FrameType::Request, 2, br#"{"op":"activity.record","kind":"worker.settled","summary":"Worker settled","subject":{"type":"run","id":"run-1"}}"#.to_vec())).await.unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(5), crate::read_frame(&mut module))
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        reply.header.ty,
        subc_protocol::FrameType::Response,
        "{}",
        String::from_utf8_lossy(&reply.body)
    );
    let stored: subc_control::ActivityEntry = serde_json::from_str(
        std::fs::read_to_string(scratch.path().join("activity.jsonl"))
            .unwrap()
            .trim(),
    )
    .unwrap();
    assert_eq!(stored.seq, 1);
    assert_eq!(
        stored.daemon_incarnation,
        info.daemon_id
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    );
    assert_eq!(stored.subject.unwrap().id, "run-1");
    drop(module);
    daemon.abort();
    assert!(daemon.await.unwrap_err().is_cancelled());
}
