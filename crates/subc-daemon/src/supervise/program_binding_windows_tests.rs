use super::*;

fn shell_spec() -> ModuleSpec {
    ModuleSpec {
        module_id: "windows-program-witness".into(),
        program: PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("System32/cmd.exe"),
        args: vec!["/C".into(), "exit 0".into()],
        env: vec![],
        reserved: false,
        reserved_prefixes: vec![],
        protocol: ModuleProtocol::None,
        overlap: Default::default(),
        carry_containment: false,
    }
}

#[test]
fn config_spawn_windows_program_witness_uses_retained_volume_and_file_id() {
    let binding = SpawnProgramBinding::capture(&shell_spec())
        .expect("a native executable has a retained handle identity");
    assert!(
        binding.current(),
        "the original Windows launch name must still name the retained executable"
    );
}

#[tokio::test]
async fn config_spawn_windows_async_launch_accepts_its_unchanged_program() {
    let supervisor = Supervisor::new_for_test(
        Arc::new(Registry::default()),
        RestartPolicy::new(0, Duration::ZERO),
    );
    let module = supervisor
        .spawn_async(shell_spec())
        .await
        .expect("an unchanged approved launch witness must not always fail plan_stale on Windows");
    module.stop().await.unwrap();
}
