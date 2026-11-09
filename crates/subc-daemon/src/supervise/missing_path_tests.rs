use super::*;

#[test]
fn config_spawn_unset_path_refuses_bare_program_before_capture_or_spawn() {
    const CHILD: &str = "SUBC_TEST_MISSING_PATH_CHILD";
    const NAME: &str = "supervise::missing_path_tests::config_spawn_unset_path_refuses_bare_program_before_capture_or_spawn";
    if std::env::var_os(CHILD).is_none() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/scratch")
            .join(format!("binding-no-path-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let executable = std::env::current_exe().unwrap();
        std::fs::copy(&executable, root.join("decoy.exe")).unwrap();
        let output = std::process::Command::new(executable)
            .args(["--exact", NAME, "--nocapture"])
            .env(CHILD, "1")
            .env_remove("PATH")
            .current_dir(&root)
            .output()
            .unwrap();
        std::fs::remove_dir_all(root).unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
        return;
    }

    assert!(std::env::var_os("PATH").is_none());
    let spec = ModuleSpec {
        module_id: "missing-path".into(),
        program: "decoy.exe".into(),
        args: vec![],
        env: vec![],
        reserved: false,
        reserved_prefixes: vec![],
        protocol: ModuleProtocol::None,
        overlap: Default::default(),
        carry_containment: false,
    };
    assert!(
        matches!(SpawnProgramBinding::capture(&spec), Err(SuperviseError::Spawn { source, .. }) if source.kind() == io::ErrorKind::InvalidInput),
        "an absent PATH must not bind a cwd decoy while exec uses a platform default"
    );
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let supervisor = Supervisor::new_for_test(
            Arc::new(Registry::default()),
            RestartPolicy::new(0, Duration::ZERO),
        );
        assert!(matches!(supervisor.spawn_async(spec).await, Err(SuperviseError::Spawn { source, .. }) if source.kind() == io::ErrorKind::InvalidInput), "the common launch path must reject the same ambiguous bare name");
    });
}
