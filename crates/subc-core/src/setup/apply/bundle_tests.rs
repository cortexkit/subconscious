use crate::setup::upgrade::tests::{write_bundle, BundleRunner};

fn pre_bundle_setup(root: &ScratchDir, health_ok: bool) -> (SetupBackend, String) {
    use crate::setup::{model::AlphaTarget, test_exec::write_executable};
    let mut backend = backend_without_user_manager(root);
    backend.platform = RuntimePlatform::Macos;
    backend.paths.data_dir = root.path().to_path_buf();
    backend.paths.runtime_paths =
        runtime::runtime_paths(RuntimePlatform::Macos, &root.join("bin"), root.path());
    backend.runner = Box::new(BundleRunner::default());
    fs::create_dir_all(root.join("bin")).unwrap();
    for binary in components::component_binaries(Component::Core) {
        let path = root.join("bin").join(binary);
        write_executable(
            &path,
            format!("#!/bin/sh\necho {binary} 0.1.0\n").as_bytes(),
        );
        backend
            .inventory
            .record("managed-binary", &path, Map::new());
    }
    write_executable(&backend.executable, format!("#!/bin/sh\nif [ \"$1\" = health ]; then echo daemon-health-failed; exit {}; fi\nexit 0\n", if health_ok { 0 } else { 1 }).as_bytes());
    let mut old_paths = backend.paths.runtime_paths.clone();
    old_paths.daemon = root.join("bin/ck-subc");
    runtime::ensure(
        RuntimePlatform::Macos,
        &old_paths,
        RuntimeStatus::default(),
        &mut backend.runner,
        &mut backend.inventory,
    )
    .unwrap();
    let definition = fs::read_to_string(&old_paths.definition).unwrap();
    let app = write_bundle(&root.join("release"), "0.2.0");
    let zip = root.join("daemon.zip");
    assert!(Command::new("ditto")
        .args(["-c", "-k", "--keepParent"])
        .arg(app)
        .arg(&zip)
        .status()
        .unwrap()
        .success());
    let mut assets = serde_json::Map::new();
    for binary in components::component_binaries(Component::Core) {
        assets.insert((*binary).into(), serde_json::json!({"url":format!("file://{}", zip.display()),"sha256":components::digest_file(&zip).unwrap(),"reports":"0.2.0"}));
    }
    let index = serde_json::from_value(serde_json::json!({"schema":1,"channel":"alpha","generated_at_ms":0,"components":{"core":{"release":"v0.2.0","version":"0.2.0","assets":{"darwin-arm64":assets}}}})).unwrap();
    backend.artifacts = ReleaseArtifactSource::from_index(index, AlphaTarget::DarwinArm64);
    backend.artifacts.runner = Box::new(BundleRunner::default());
    (backend, definition)
}

fn run_setup_migration(backend: &mut SetupBackend) -> Result<(), String> {
    backend.apply(&SetupOperation::InstallComponent {
        component: Component::Core,
    })?;
    assert!(
        backend.paths.binary_home.join("ck-subc").is_file(),
        "legacy image remains until daemon health is verified"
    );
    backend.apply(&SetupOperation::UpdateRuntimeDefinition { restart: true })?;
    backend.apply_plan(
        &SetupPlan {
            operations: vec![],
            outcomes: vec![],
            permission_repair: false,
        },
        &SetupRequest::install(vec![]),
    )
}

#[test]
fn macos_bundle_setup_migrates_pre_bundle_core_after_health_verification() {
    let root = ScratchDir::new("bundle-setup-migrates");
    let (mut backend, _) = pre_bundle_setup(&root, true);
    assert!(components::component_is_owned(
        Component::Core,
        &root.join("bin"),
        &backend.inventory
    ));
    assert!(
        !components::is_installed(Component::Core, &root.join("bin"), &backend.inventory),
        "owned legacy Core still needs bundle placement"
    );
    run_setup_migration(&mut backend).unwrap();
    assert!(backend.paths.runtime_paths.daemon.is_file());
    assert!(!root.join("bin/ck-subc").exists());
    assert!(fs::read_to_string(&backend.paths.runtime_paths.definition)
        .unwrap()
        .contains("apps/Subconscious.app/Contents/MacOS/ck-subc"));
}

#[test]
fn macos_bundle_setup_failed_health_restores_bare_and_definition() {
    let root = ScratchDir::new("bundle-setup-health-rollback");
    let (mut backend, old_definition) = pre_bundle_setup(&root, false);
    let original = fs::read(root.join("bin/ck-subc")).unwrap();
    let error = run_setup_migration(&mut backend).unwrap_err();
    assert!(error.contains("daemon-health-failed"), "{error}");
    assert_eq!(fs::read(root.join("bin/ck-subc")).unwrap(), original);
    assert_eq!(
        fs::read_to_string(&backend.paths.runtime_paths.definition).unwrap(),
        old_definition
    );
    assert!(!root.join("apps/Subconscious.app").exists());
    assert!(backend
        .inventory
        .owns_path("managed-binary", &root.join("bin/ck-subc")));
}

fn unowned_bundle_refusal(missing_executable: bool) {
    let root = ScratchDir::new("bundle-unowned");
    let (mut backend, old_definition) = pre_bundle_setup(&root, true);
    let app = write_bundle(&root.join("apps"), "0.2.0");
    fs::write(app.join("Contents/foreign-resource"), "operator owned").unwrap();
    if missing_executable {
        fs::remove_file(app.join("Contents/MacOS/ck-subc")).unwrap();
    }
    let error = backend
        .apply(&SetupOperation::InstallComponent {
            component: Component::Core,
        })
        .unwrap_err();
    assert!(
        error.contains(&app.to_string_lossy().into_owned()),
        "{error}"
    );
    assert!(error.contains("move it aside"), "{error}");
    assert_eq!(
        fs::read_to_string(app.join("Contents/foreign-resource")).unwrap(),
        "operator owned"
    );
    assert_eq!(
        fs::read_to_string(&backend.paths.runtime_paths.definition).unwrap(),
        old_definition
    );
    assert!(!backend
        .inventory
        .owns_path("managed-binary", &app.join("Contents/MacOS/ck-subc")));
}

#[test]
fn macos_bundle_setup_refuses_unowned_app_with_matching_executable() {
    unowned_bundle_refusal(false);
}

#[test]
fn macos_bundle_setup_refuses_unowned_app_with_missing_executable() {
    unowned_bundle_refusal(true);
}
