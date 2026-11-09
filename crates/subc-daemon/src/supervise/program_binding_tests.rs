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
    let refused = matches!(&prompt, Ok(Ok(Err(error))) if error.to_string().contains("regular"));
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
        refused,
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
    std::fs::create_dir(&early).unwrap();
    std::fs::create_dir(&later).unwrap();
    std::fs::write(
        early.join("command"),
        format!(
            "#!/bin/sh\nprintf wrong > '{}'\nexec /bin/sleep 600\n",
            marker.display()
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
        format!("printf right > '{}'; exec /bin/sleep 600", marker.display()),
    ];
    let supervisor = Supervisor::new_for_test(
        Arc::new(Registry::default()),
        RestartPolicy::new(0, Duration::ZERO),
    );
    let module = supervisor.spawn_async(spec).await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(6);
    while !marker.exists() && Instant::now() < deadline {
        if module.state().unwrap() == ModuleState::Failed {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let ran = std::fs::read_to_string(&marker).ok();
    module.stop().await.unwrap();
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
