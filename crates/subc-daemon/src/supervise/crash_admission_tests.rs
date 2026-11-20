use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn backoff_longer_than_the_window_does_not_age_the_replacement() {
    let policy =
        RestartPolicy::new(1, Duration::from_millis(1500)).with_window(Duration::from_secs(1));
    let supervisor = Supervisor::new_for_test(Arc::new(Registry::default()), policy);
    let spec = ModuleSpec {
        module_id: "crash-admission-window".into(),
        program: terminal_history_tests::fake_aft_stub_path(),
        env: vec![("FAKE_AFT_EXIT_CODE".into(), "23".into())],
        protocol: ModuleProtocol::Subc,
        args: vec![],
        reserved_prefixes: vec![],
        reserved: false,
        script: None,
        overlap: ModuleOverlap::Exclusive,
        carry_containment: false,
    };
    let module = supervisor.spawn(spec).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        let status = module.status().unwrap();
        if status.spawn_generation == 2
            && module.terminal_history().entries.len() >= 2
            && status.state != ModuleState::Running
        {
            break status;
        }
        assert!(Instant::now() < deadline, "the replacement never exited");
        sleep(Duration::from_millis(10)).await;
    };
    assert_eq!(
        status.state,
        ModuleState::Failed,
        "the replacement used the only slot in the current window"
    );
    assert_eq!(status.restart_count, 1);
    assert_eq!(status.lifetime_restarts, 1);
}
