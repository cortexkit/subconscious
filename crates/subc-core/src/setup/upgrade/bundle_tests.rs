use crate::setup::runtime::{CommandResult, CommandRunner, RuntimePlatform};

const TEST_APP: &str = "CortexKit Daemon.app";
const TEST_APP_ID: &str = "5R5846NBPW.io.cortexkit.daemon";

pub(in crate::setup) struct BundleRunner {
    signature_ok: bool,
    app_id: String,
}

impl Default for BundleRunner {
    fn default() -> Self {
        Self {
            signature_ok: true,
            app_id: TEST_APP_ID.into(),
        }
    }
}

impl CommandRunner for BundleRunner {
    fn run(&mut self, program: &str, args: &[String]) -> Result<CommandResult, String> {
        match program {
            "ditto" => crate::setup::runtime::SystemCommandRunner.run(program, args),
            "codesign" => {
                assert_eq!(&args[..2], ["--verify", "--strict"]);
                assert!(Path::new(args.last().unwrap())
                    .join("Contents/embedded.provisionprofile")
                    .is_file());
                Ok(CommandResult {
                    success: self.signature_ok,
                    stdout: String::new(),
                    stderr: "signature refused".into(),
                })
            }
            "security" => {
                assert_eq!(&args[..3], ["cms", "-D", "-i"]);
                Ok(CommandResult { success: true, stdout: format!("<?xml version=\"1.0\"?><plist version=\"1.0\"><dict><key>Entitlements</key><dict><key>com.apple.application-identifier</key><string>{}</string></dict></dict></plist>", self.app_id), stderr: String::new() })
            }
            "/usr/libexec/PlistBuddy" => Ok(CommandResult {
                success: true,
                stdout: self.app_id.clone(),
                stderr: String::new(),
            }),
            "launchctl" => Ok(CommandResult {
                success: true,
                stdout: "state = running".into(),
                stderr: String::new(),
            }),
            other => panic!("unexpected command {other}: {args:?}"),
        }
    }

    fn daemon_processes(
        &mut self,
        _daemon: &Path,
    ) -> Result<Vec<crate::setup::runtime::DaemonProcess>, String> {
        Ok(vec![])
    }
}

pub(in crate::setup) fn write_bundle(root: &Path, version: &str) -> PathBuf {
    let app = root.join(TEST_APP);
    fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
    write_executable(
        &app.join("Contents/MacOS/ck-subc"),
        format!("#!/bin/sh\necho ck-subc {version}\n").as_bytes(),
    );
    fs::write(
        app.join("Contents/Info.plist"),
        format!("metadata {version}"),
    )
    .unwrap();
    fs::write(
        app.join("Contents/embedded.provisionprofile"),
        "signed profile fixture",
    )
    .unwrap();
    app
}

fn bundle_backend(
    root: &ScratchDir,
    legacy: bool,
    runner: BundleRunner,
) -> (SystemUpgradeBackend, UpgradeTarget, PathBuf, PathBuf) {
    let target = upgrade_target("ck-subc");
    fs::create_dir_all(root.join("bin")).unwrap();
    let app = root.join("apps").join(TEST_APP);
    let destination = if legacy {
        let path = root.join("bin/ck-subc");
        write_executable(&path, b"#!/bin/sh\necho ck-subc 0.1.0\n");
        path
    } else {
        write_bundle(&root.join("apps"), "0.1.0").join("Contents/MacOS/ck-subc")
    };
    let mut backend = managed_backend(root, target, &destination);
    backend.platform = AlphaTarget::DarwinArm64;
    backend.runner = Box::new(runner);
    let mut paths = crate::setup::runtime::runtime_paths(
        RuntimePlatform::Macos,
        &root.join("bin"),
        root.path(),
    );
    paths.daemon = destination.clone();
    crate::setup::runtime::ensure(
        RuntimePlatform::Macos,
        &paths,
        crate::setup::runtime::RuntimeStatus::default(),
        &mut backend.runner,
        &mut backend.inventory,
    )
    .unwrap();
    backend.daemon_definition_snapshot = Some(
        crate::setup::runtime::DefinitionSnapshot::capture(RuntimePlatform::Macos, &paths).unwrap(),
    );
    let candidate =
        write_bundle(&root.join("workspace/extracted"), "0.2.0").join("Contents/MacOS/ck-subc");
    backend.prepared.insert(
        target.label().into(),
        PreparedUpgradeAsset::test_candidate(candidate, target),
    );
    (backend, target, destination, app)
}

#[test]
fn macos_bundle_upgrade_from_bare_keeps_legacy_until_verified() {
    let root = ScratchDir::new("bundle-from-bare");
    let (mut backend, target, bare, app) = bundle_backend(&root, true, BundleRunner::default());
    backend.create_rollback_copy(target).unwrap();
    backend.replace_destination(target).unwrap();
    assert!(app.join("Contents/embedded.provisionprofile").is_file());
    assert_eq!(
        fs::read_to_string(&bare).unwrap(),
        "#!/bin/sh\necho ck-subc 0.1.0\n"
    );
    assert_eq!(
        backend.target(target).unwrap().destination,
        app.join("Contents/MacOS/ck-subc")
    );
    backend.completed(target);
    assert!(!bare.exists());
    assert!(!backend.inventory.owns_path("managed-binary", &bare));
    assert!(backend
        .inventory
        .owns_path("managed-binary", &app.join("Contents/MacOS/ck-subc")));
}

#[test]
fn macos_bundle_upgrade_replaces_the_whole_app() {
    let root = ScratchDir::new("bundle-from-bundle");
    let (mut backend, target, _, app) = bundle_backend(&root, false, BundleRunner::default());
    fs::write(app.join("Contents/old-resource"), "old only").unwrap();
    backend.create_rollback_copy(target).unwrap();
    backend.replace_destination(target).unwrap();
    assert_eq!(
        fs::read_to_string(app.join("Contents/Info.plist")).unwrap(),
        "metadata 0.2.0"
    );
    assert!(!app.join("Contents/old-resource").exists());
    backend.completed(target);
    assert!(!app.with_extension("app.rollback").exists());
}

fn bundle_rollback_restores(root: &ScratchDir, legacy: bool) {
    let (mut backend, target, old, app) = bundle_backend(root, legacy, BundleRunner::default());
    let paths = crate::setup::runtime::runtime_paths(
        RuntimePlatform::Macos,
        &root.join("bin"),
        root.path(),
    );
    fs::create_dir_all(paths.definition.parent().unwrap()).unwrap();
    let old_definition = fs::read_to_string(&paths.definition).unwrap();
    backend.create_rollback_copy(target).unwrap();
    backend.replace_destination(target).unwrap();
    assert!(app.join("Contents/Info.plist").is_file());
    backend
        .restart_daemon_via_service_manager(Duration::from_secs(1))
        .unwrap();
    backend.rollback(target).unwrap();
    assert_eq!(
        fs::read_to_string(&paths.definition).unwrap(),
        old_definition
    );
    assert_eq!(
        fs::read_to_string(&old).unwrap(),
        "#!/bin/sh\necho ck-subc 0.1.0\n"
    );
    assert_eq!(app.exists(), !legacy);
    if !legacy {
        assert_eq!(
            fs::read_to_string(app.join("Contents/Info.plist")).unwrap(),
            "metadata 0.1.0"
        );
    }
    assert!(backend.inventory.owns_path("managed-binary", &old));
}

#[test]
fn macos_bundle_rollback_restores_pre_bundle_install() {
    bundle_rollback_restores(&ScratchDir::new("bundle-rollback-bare"), true);
}

#[test]
fn macos_bundle_rollback_restores_previous_bundle() {
    bundle_rollback_restores(&ScratchDir::new("bundle-rollback-app"), false);
}

fn bundle_refusal_leaves_install_unchanged(runner: BundleRunner, expected: &str) {
    let root = ScratchDir::new("bundle-refused");
    let (mut backend, target, old, app) = bundle_backend(&root, false, runner);
    let before = fs::read(&old).unwrap();
    let definition = backend
        .daemon_definition_snapshot
        .as_ref()
        .unwrap()
        .paths()
        .definition
        .clone();
    let before_definition = fs::read(&definition).unwrap();
    backend.create_rollback_copy(target).unwrap();
    let error = backend.replace_destination(target).unwrap_err();
    assert!(error.contains(expected), "{error}");
    assert_eq!(fs::read(&old).unwrap(), before);
    assert_eq!(fs::read(&definition).unwrap(), before_definition);
    assert_eq!(
        fs::read_to_string(app.join("Contents/Info.plist")).unwrap(),
        "metadata 0.1.0"
    );
    assert!(!app.with_extension("app.new").exists());
}

#[test]
fn macos_bundle_wrong_profile_is_refused_before_replacement() {
    bundle_refusal_leaves_install_unchanged(
        BundleRunner {
            app_id: "OTHER.io.cortexkit.daemon".into(),
            ..BundleRunner::default()
        },
        "application-identifier",
    );
}

#[test]
fn macos_bundle_bad_signature_is_refused_before_replacement() {
    bundle_refusal_leaves_install_unchanged(
        BundleRunner {
            signature_ok: false,
            ..BundleRunner::default()
        },
        "signature",
    );
}

struct MigratingProcessRunner {
    expected_image: PathBuf,
    polled: bool,
}

impl CommandRunner for MigratingProcessRunner {
    fn run(&mut self, program: &str, args: &[String]) -> Result<CommandResult, String> {
        if args.first().is_some_and(|arg| arg == "bootstrap") {
            assert!(
                self.polled,
                "the previous daemon must exit before its replacement starts"
            );
        }
        BundleRunner::default().run(program, args)
    }

    fn daemon_processes(
        &mut self,
        image: &Path,
    ) -> Result<Vec<crate::setup::runtime::DaemonProcess>, String> {
        assert_eq!(
            image, self.expected_image,
            "scan the image being stopped, not the image about to start"
        );
        Ok(vec![crate::setup::runtime::DaemonProcess {
            pid: 123,
            start_time: 1,
        }])
    }

    fn process_running(&mut self, _process: &crate::setup::runtime::DaemonProcess) -> bool {
        self.polled = true;
        false
    }
}

#[test]
fn macos_bundle_migration_waits_for_the_previous_executable_in_both_directions() {
    let root = ScratchDir::new("bundle-migration-process");
    let (mut backend, target, bare, app) = bundle_backend(&root, true, BundleRunner::default());
    let connection = root.join("connection.json");
    subc_transport::write_atomic(
        &connection,
        &subc_transport::ConnectionInfo {
            schema: subc_transport::SCHEMA_VERSION,
            wire_version: Some(subc_protocol::PROTOCOL_VERSION),
            endpoints: vec![subc_transport::Endpoint {
                host: "127.0.0.1".into(),
                port: 1,
            }],
            key: vec![7; 32],
            daemon_id: [8; 16],
            pid: 123,
            daemon_ver: "test".into(),
        },
    )
    .unwrap();
    backend.subc = Some(connection);
    backend.create_rollback_copy(target).unwrap();
    backend.replace_destination(target).unwrap();
    backend.runner = Box::new(MigratingProcessRunner {
        expected_image: bare,
        polled: false,
    });
    backend
        .restart_daemon_via_service_manager(Duration::from_secs(1))
        .unwrap();
    backend.runner = Box::new(MigratingProcessRunner {
        expected_image: app.join("Contents/MacOS/ck-subc"),
        polled: false,
    });
    backend.rollback(target).unwrap();
}

#[test]
fn macos_bundle_fresh_release_install_lands_bundle_and_definition() {
    use crate::setup::components::ArtifactSource;
    let root = ScratchDir::new("bundle-fresh-release");
    let app = write_bundle(&root.join("release"), "0.2.0");
    let zip = root.join("daemon.zip");
    assert!(Command::new("ditto")
        .args(["-c", "-k", "--keepParent"])
        .arg(&app)
        .arg(&zip)
        .status()
        .unwrap()
        .success());
    let index: ReleaseIndex = serde_json::from_value(serde_json::json!({
        "schema":1,"channel":"alpha","generated_at_ms":0,
        "components":{"core":{"release":"v0.2.0","version":"0.2.0","assets":{"darwin-arm64":{"ck-subc":{"url":format!("file://{}", zip.display()),"sha256":sha256_file(&zip).unwrap(),"reports":"0.2.0"}}}}}
    })).unwrap();
    let target = upgrade_target("ck-subc");
    let prepared = crate::setup::upgrade_assets::prepare_upgrade_asset_with_runner(
        &mut ReleaseUpgradeAssetFetcher::from_index(index.clone()),
        target,
        AlphaTarget::DarwinArm64,
        &mut BundleRunner::default(),
    )
    .unwrap();
    assert!(prepared.candidate.is_file());
    assert!(prepared
        .candidate
        .ancestors()
        .nth(3)
        .unwrap()
        .join("Contents/embedded.provisionprofile")
        .is_file());
    prepared.cleanup();
    let mut source = ReleaseArtifactSource::from_index(index, AlphaTarget::DarwinArm64);
    source.runner = Box::new(BundleRunner::default());
    let paths = crate::setup::runtime::runtime_paths(
        RuntimePlatform::Macos,
        &root.join("bin"),
        root.path(),
    );
    let mut inventory = Inventory::load(root.join("manifest.json"), "darwin-arm64").unwrap();
    let mut migration = crate::setup::daemon_bundle::Migration::capture(
        &paths,
        &root.join("bin/ck-subc"),
        &inventory,
    )
    .unwrap();
    source
        .install(Component::Core, "ck-subc", &paths.daemon)
        .unwrap();
    migration.attach(source.daemon_replacement.take().unwrap());
    assert!(root
        .join("apps/CortexKit Daemon.app/Contents/embedded.provisionprofile")
        .is_file());
    crate::setup::runtime::ensure(
        RuntimePlatform::Macos,
        &paths,
        crate::setup::runtime::RuntimeStatus::default(),
        &mut BundleRunner::default(),
        &mut inventory,
    )
    .unwrap();
    assert!(fs::read_to_string(&paths.definition)
        .unwrap()
        .contains("apps/CortexKit Daemon.app/Contents/MacOS/ck-subc"));
    assert!(!root.join("bin/ck-subc").exists());
    migration
        .rollback(&mut BundleRunner::default(), &mut inventory, None)
        .unwrap();
    assert!(!root.join("apps/CortexKit Daemon.app").exists());
    assert!(!paths.definition.exists());
}
