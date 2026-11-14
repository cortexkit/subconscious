use super::*;

struct Scratch(PathBuf);
impl Scratch {
    fn new(name: &str) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/scratch")
            .join(format!(
                "{name}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}
impl std::ops::Deref for Scratch {
    type Target = std::path::Path;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn spec(program: PathBuf, env: Vec<(String, String)>) -> ModuleSpec {
    ModuleSpec {
        module_id: "binding-probe".into(),
        program,
        args: vec![],
        env,
        reserved: false,
        reserved_prefixes: vec![],
        protocol: ModuleProtocol::None,
        overlap: Default::default(),
        carry_containment: false,
    }
}

#[test]
fn config_spawn_bare_name_missing_from_path_never_falls_back_to_working_directory() {
    let root = Scratch::new("binding-path-absent");
    assert!(std::path::Path::new("Cargo.toml").is_file());
    let result = SpawnProgramBinding::capture(&spec(
        "Cargo.toml".into(),
        vec![("PATH".into(), root.to_string_lossy().into_owned())],
    ));
    assert!(
        matches!(result, Err(SuperviseError::Spawn { source, .. }) if source.kind() == io::ErrorKind::NotFound),
        "a bare name absent from PATH must not capture an unrelated cwd file"
    );
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn config_spawn_macos_fifo_refuses_without_stranding_the_module_gate() {
    use std::io::Write;
    let (_rig, owner, supervisor, _handler, spec) =
        crate::config_gate::runtime::RuntimeOwner::unactivated_supervisor_fixture_for_test();
    let roster = supervisor.child_roster.clone();
    let program = spec.program.clone();
    let gate = owner.module_gate(&spec.module_id).unwrap();
    std::fs::remove_file(&program).unwrap();
    assert!(std::process::Command::new("mkfifo")
        .arg(&program)
        .status()
        .unwrap()
        .success());
    let mut launch = tokio::spawn(async move { supervisor.spawn_async(spec).await });
    let prompt = tokio::time::timeout(Duration::from_millis(250), &mut launch).await;
    let refused = matches!(&prompt, Ok(Ok(Ok(module))) if module.status().is_ok_and(|status| !status.process_alive && status.state == ModuleState::Stopped));
    if prompt.is_err() {
        launch.abort();
        let _ = launch.await;
        // Unblock both the old FIFO open and its two-byte shebang read so a red
        // test cannot leave the blocking worker alive during runtime teardown.
        let mut rescue = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&program)
            .unwrap();
        rescue.write_all(b"xx").unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(5), gate.lock())
            .await
            .unwrap();
    }
    assert_eq!(roster.live_count_for_test(), 0);
    assert_eq!(owner.unregistered_spawn_count_for_test(), 0);
    assert!(gate.try_lock().is_ok());
    assert!(
        refused && owner.module_is_gated("a"),
        "macOS must reject a FIFO before shebang inspection"
    );
}

#[cfg(target_os = "macos")]
async fn path_launch(last_override: bool) {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let root = Scratch::new("macos-binding-path");
    let early = root.join("early");
    let later = root.join("later");
    let marker = root.join("executed");
    let fixture = crate::config_gate::program::native_fixture_for_test();
    std::fs::create_dir(&early).unwrap();
    std::fs::create_dir(&later).unwrap();
    std::fs::write(
        early.join("command"),
        format!(
            "#!/bin/sh\nprintf wrong > '{}'\n'{}' 600\n:\n",
            marker.display(),
            fixture.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(
        early.join("command"),
        std::fs::Permissions::from_mode(if last_override { 0o700 } else { 0o600 }),
    )
    .unwrap();
    symlink("/bin/sh", later.join("command")).unwrap();
    let path = |dirs: &[&PathBuf]| std::env::join_paths(dirs).unwrap().into_string().unwrap();
    let env = if last_override {
        vec![
            ("PATH".into(), path(&[&early])),
            ("PATH".into(), path(&[&later])),
        ]
    } else {
        vec![("PATH".into(), path(&[&early, &later]))]
    };
    let mut spec = spec("command".into(), env);
    spec.args = vec![
        "-c".into(),
        "printf right > \"$1\"; \"$2\" 600; :".into(),
        "binding-path".into(),
        marker.display().to_string(),
        fixture.display().to_string(),
    ];
    let supervisor = Supervisor::new_for_test(
        Arc::new(Registry::default()),
        RestartPolicy::new(0, Duration::ZERO),
    );
    let module = supervisor.spawn_async(spec).await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(6);
    while (!marker.exists() || module.status().unwrap().pid.is_none()) && Instant::now() < deadline
    {
        if module.state().unwrap() == ModuleState::Failed {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let ran = std::fs::read_to_string(&marker).ok();
    let admitted = module.status().unwrap().pid.is_some();
    module.stop().await.unwrap();
    // Printing before a refused launch is killed is not successful admission.
    assert!(
        admitted,
        "the selected PATH program must finish privacy exec confirmation"
    );
    assert_eq!(
        ran.as_deref(),
        Some("right"),
        "the privacy trampoline must execute the executable selected by the retained binding"
    );
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn config_spawn_macos_path_skips_non_executable_regular_files() {
    path_launch(false).await;
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn config_spawn_macos_path_last_override_matches_the_executed_program() {
    path_launch(true).await;
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn config_spawn_macos_unset_path_capture_matches_default_path_execution() {
    const CHILD: &str = "SUBC_TEST_UNSET_PATH_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let root = Scratch::new("macos-binding-unset-path");
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "supervise::program_binding_tests::config_spawn_macos_unset_path_capture_matches_default_path_execution", "--nocapture"])
            .env(CHILD, "1")
            .env_remove("PATH")
            .current_dir(&*root)
            .output().unwrap();
        assert!(
            output.status.success(),
            "PATH-unset subprocess failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("1 passed"),
            "the subprocess must run the named test"
        );
        return;
    }

    use std::os::unix::fs::PermissionsExt;
    assert!(std::env::var_os("PATH").is_none());
    let root = std::env::current_dir().unwrap();
    let decoy = root.join("sleep");
    std::fs::write(&decoy, "#!/bin/sh\nexit 17\n").unwrap();
    std::fs::set_permissions(&decoy, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut spec = spec("sleep".into(), vec![]);
    spec.args = vec!["600".into()];
    let binding = SpawnProgramBinding::capture(&spec).unwrap();
    let supervisor = Supervisor::new_for_test(
        Arc::new(Registry::default()),
        RestartPolicy::new(0, Duration::ZERO),
    );
    let module = supervisor.spawn_async(spec).await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(6);
    while Instant::now() < deadline {
        if module.status().unwrap().pid.is_some() || module.state().unwrap() == ModuleState::Failed
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let observed = module
        .status()
        .unwrap()
        .pid
        .and_then(observe_spawned_image)
        .and_then(|image| image.executable);
    module.stop().await.unwrap();
    let system_program = subc_os::file_identity(std::path::Path::new("/bin/sleep")).unwrap();
    assert_eq!(
        observed,
        Some(system_program),
        "the actual exec must use Darwin's default search path"
    );
    assert_eq!(binding.identity, system_program, "capture must use the same default search path as exec when PATH is unset, not bind the cwd decoy");
}
