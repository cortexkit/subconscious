use super::*;
use crate::setup::{
    install_journal::{bootstrap::FileRecovery, Journal},
    model::AlphaTarget,
};
use cortexkit_test_support::ScratchDir;
use std::{collections::VecDeque, time::Duration};

pub(super) const TARGET: &str = "darwin-arm64";

pub(super) fn inventory(root: &Path) -> Inventory {
    Inventory::load_locked(
        root.join("installer-manifest.json"),
        TARGET,
        "setup test",
        Duration::from_secs(2),
    )
    .unwrap()
}

pub(super) fn restart(root: &Path) -> Inventory {
    let inventory = inventory(root);
    if let Some(journal) = Journal::open(inventory.manifest()).unwrap() {
        journal.recover(&mut FileRecovery).unwrap();
    }
    inventory
}

#[cfg(feature = "test-support")]
pub(super) fn crash_at(root: &Path, test: &str, cut: &str, extra_env: &[(&str, &str)]) {
    let marker = root.join("crash-marker");
    let (_, test) = test.split_once("::").expect("crate-qualified test name");
    let mut child = Command::new(env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"])
        .env("CK_TEST_INSTALL_CRASH_POINT", cut)
        .env("CK_TEST_INSTALL_CRASH_MARKER", &marker)
        .env("CK_MODULE_STEP_TEST_ROOT", root)
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_RUNTIME_DIR", root.join("run"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .envs(extra_env.iter().copied())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while !marker.is_file() {
        assert!(
            child.try_wait().unwrap().is_none(),
            "child exited before reaching {cut}"
        );
        if std::time::Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("timed out before reaching {cut}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(fs::read_to_string(&marker).unwrap(), cut);
    child.kill().unwrap();
    assert!(!child.wait().unwrap().success());
    fs::remove_file(marker).unwrap();
}

#[derive(Default)]
struct Fake {
    calls: Vec<(String, Vec<String>)>,
    answers: VecDeque<(i32, &'static str)>,
}

impl CommandRunner for Fake {
    fn run(&mut self, program: &str, args: &[String]) -> Result<runtime::CommandResult, String> {
        self.calls.push((program.into(), args.to_vec()));
        let (exit, stdout) = self.answers.pop_front().expect("scripted command result");
        Ok(runtime::CommandResult {
            success: exit == 0,
            exit_code: Some(exit),
            stdout: stdout.into(),
            stderr: String::new(),
        })
    }
}

#[test]
fn fresh_module_grant_steps_use_only_the_exact_category_tuples() {
    let mut runner = Fake {
        answers: VecDeque::from(vec![(0, "already granted (no change)\n"); 10]),
        ..Default::default()
    };
    let mut auth = vault::Auth {
        program: Path::new("ck-auth"),
        runner: &mut runner,
        wait: |_| panic!("unexpected retry"),
    };
    for component in [
        Component::Astrocyte,
        Component::Striatum,
        Component::Broca,
        Component::Cerebellum,
        Component::Plexus,
        Component::Entorhinal,
    ] {
        assert!(!auth.ensure_component(component).unwrap());
    }
    let tuples = [
        ("reserved:astrocyte", "llm-provider", "list"),
        ("reserved:striatum", "llm-provider", "list"),
        ("reserved:striatum", "llm-gateway", "list"),
        ("reserved:striatum", "media-provider", "list"),
        ("reserved:broca", "llm-provider", "read"),
        ("reserved:broca", "llm-gateway", "read"),
        ("reserved:broca", "llm-gateway", "list"),
        ("reserved:cerebellum", "browser-session", "deposit"),
        ("reserved:plexus", "github-app-native", "read"),
        ("reserved:plexus", "web-search", "read"),
    ];
    assert_eq!(runner.calls.len(), tuples.len());
    for ((program, args), (principal, selector, op)) in runner.calls.iter().zip(tuples) {
        assert_eq!(program, "ck-auth");
        assert_eq!(
            *args,
            [
                "grant",
                "--principal",
                principal,
                "--selector-kind",
                "category",
                "--selector",
                selector,
                "--operation",
                op
            ]
        );
    }
}

#[test]
fn module_grant_retries_only_documented_codes_and_reports_uncertain_retry_as_ensured() {
    for (codes, success, last_exit) in [
        (vec![5, 0], true, ""),
        (vec![5, 5], false, "exit 5"),
        (vec![3; 11], false, "exit 3"),
        (vec![4], false, "exit 4"),
        (vec![5, 3], false, "exit 3"),
    ] {
        let mut runner = Fake {
            answers: codes
                .iter()
                .map(|&code| {
                    (
                        code,
                        if code == 0 {
                            "already granted (no change)"
                        } else {
                            ""
                        },
                    )
                })
                .collect(),
            ..Default::default()
        };
        let mut auth = vault::Auth {
            program: Path::new("ck-auth"),
            runner: &mut runner,
            wait: |delay| assert_eq!(delay, Duration::from_secs(2)),
        };
        let result = auth.ensure_component(Component::Astrocyte);
        if success {
            assert_eq!(vault::report(result.unwrap()), "ensured");
        } else {
            let error = result.unwrap_err();
            assert!(
                error.contains(last_exit)
                    && error.contains("reserved:astrocyte, category, llm-provider, list"),
                "{error}"
            );
        }
        assert_eq!(runner.calls.len(), codes.len());
        assert!(runner.calls.windows(2).all(|pair| pair[0] == pair[1]));
    }
}

#[test]
fn fusiform_writes_for_empty_descriptor_directory_but_not_for_store_and_keeps_receipt_on_repeat() {
    let root = ScratchDir::new("fusiform-descriptor");
    let config = root.join("subc.jsonc");
    let data = root.join("chosen-data-home");
    fs::write(
        &config,
        serde_json::json!({"version":1,"storage":{"backend":"sqlite","data_home":data}})
            .to_string(),
    )
    .unwrap();
    let folder = data.join("cortexkit/fusiform");
    fs::create_dir_all(&folder).unwrap();
    let settings_path = root.join("fusiform.jsonc");
    let mut inventory = inventory(root.path());
    assert!(matches!(
        settings::plan(&settings_path, &inventory, || {
            settings::fusiform_precondition(&config, &settings_path)
        })
        .unwrap(),
        settings::Plan::Write
    ));
    fs::write(folder.join("store.db"), b"already initialized").unwrap();
    let condition = settings::fusiform_precondition(&config, &settings_path)
        .unwrap()
        .unwrap();
    // The storage descriptor writes `<data_home>/cortexkit/<module>/store.db`
    // with forward slashes on every platform (daemon_config::descriptor_for),
    // so the note names the store that way, not with Windows separators.
    let store = format!("{}/cortexkit/fusiform/store.db", data.display());
    assert!(
        condition.contains("existing store.db") && condition.contains(&store),
        "{condition}"
    );
    fs::remove_file(folder.join("store.db")).unwrap();
    inventory.start_transaction().unwrap();
    assert!(
        settings::ensure_fusiform(&config, &settings_path, &mut inventory)
            .unwrap()
            .ends_with("ensured")
    );
    inventory.commit_transaction(&mut FileRecovery).unwrap();
    assert_eq!(
        fs::read_to_string(&settings_path).unwrap(),
        concat!(
            r#"{"source":{"mode":"store","url":"https://models.cortexkit.io","keys":[{"kid":"models-1","public_key":"YMEG61WYx6n19Xs44Q6FlEmsbEee_gvDnpl7GOjwz2U"}]}}"#,
            "\n"
        )
    );
    assert!(inventory.owns_path("settings-receipt", &settings_path));
    assert!(matches!(
        settings::plan(&settings_path, &inventory, || panic!(
            "receipt already recorded"
        ))
        .unwrap(),
        settings::Plan::Skip
    ));
}

#[test]
fn native_host_register_runs_on_macos_only() {
    let root = ScratchDir::new("native-host-macos-only");
    let mut backend = tests::backend_without_user_manager(&root);
    backend.runner = Box::new(Fake::default());
    for platform in [RuntimePlatform::Linux, RuntimePlatform::Windows] {
        backend.platform = platform;
        backend
            .apply_module_step(&SetupOperation::RegisterCerebellumNativeHost)
            .unwrap();
    }
    struct Register;
    impl CommandRunner for Register {
        fn run(
            &mut self,
            program: &str,
            args: &[String],
        ) -> Result<runtime::CommandResult, String> {
            assert!(program.ends_with("ck-cerebellum-native-host"));
            assert_eq!(args, ["--register"]);
            Ok(runtime::CommandResult {
                success: true,
                exit_code: Some(0),
                ..Default::default()
            })
        }
    }
    backend.runner = Box::new(Register);
    backend.platform = RuntimePlatform::Macos;
    backend
        .apply_module_step(&SetupOperation::RegisterCerebellumNativeHost)
        .unwrap();
}

#[test]
fn remedy_excludes_failed_component_and_dependents_and_keeps_optional_roots() {
    let plan = SetupPlan {
        operations: Vec::new(),
        outcomes: Vec::new(),
        validation: vec![
            Component::Core,
            Component::Claustrum,
            Component::Fusiform,
            Component::Striatum,
            Component::Broca,
            Component::Cingulate,
            Component::Entorhinal,
            Component::Cerebellum,
        ],
        permission_repair: false,
    };
    let error = module_steps::refusal_remedy(&plan, Component::Fusiform, "settings refused");
    assert!(error.ends_with("ck setup --with cerebellum"), "{error}");
    let plan = SetupPlan {
        validation: vec![Component::Core, Component::Fusiform],
        ..plan
    };
    let error = module_steps::refusal_remedy(&plan, Component::Fusiform, "settings refused");
    assert!(
        error.contains("rerun the same ck setup command") && !error.contains("--with"),
        "{error}"
    );
}

fn installed_observation() -> SetupObserved {
    let mut observed = SetupObserved::unconfigured_current_host();
    observed.platform = PlatformObservation::Supported(AlphaTarget::LinuxX64);
    observed.runtime = super::super::model::RuntimeState::Correct;
    observed
        .components
        .values_mut()
        .for_each(|state| *state = ComponentState::Correct);
    observed.detections.clear();
    observed
}

#[test]
fn new_steps_follow_component_order_and_defer_list_when_vault_bootstrap_is_pending() {
    let root = ScratchDir::new("setup-module-plan-order");
    let mut backend = tests::backend_without_user_manager(&root);
    fs::write(
        &backend.paths.config_path,
        serde_json::json!({"version":1,"storage":{"backend":"sqlite","data_home":root.path()}})
            .to_string(),
    )
    .unwrap();
    backend.runner = Box::new(Fake::default());
    let mut observed = installed_observation();
    for component in [Component::Broca, Component::Fusiform, Component::Cerebellum] {
        observed
            .components
            .insert(component, ComponentState::Missing);
    }
    observed
        .components
        .insert(Component::Claustrum, ComponentState::Configured);
    let request = SetupRequest::install(Vec::new());
    let index = backend.artifacts.cloned_index().unwrap();
    backend.observe_module_steps(&request, &index, &mut observed);
    let plan = crate::setup::plan_setup(&observed, &request);
    let operations = &plan.operations;
    let position =
        |predicate: fn(&SetupOperation) -> bool| operations.iter().position(predicate).unwrap();
    let write = position(|op| {
        matches!(
            op,
            SetupOperation::WriteModuleSettings {
                component: Component::Fusiform,
                ..
            }
        )
    });
    assert_eq!(
        operations[write + 1],
        SetupOperation::InstallComponent {
            component: Component::Fusiform
        }
    );
    let key = position(|op| matches!(op, SetupOperation::EnsureBrocaKey { .. }));
    assert_eq!(
        operations[key - 1],
        SetupOperation::ConfigureComponent {
            component: Component::Broca
        }
    );
    assert_eq!(
        operations[key + 1],
        SetupOperation::GrantComponentVault {
            component: Component::Broca
        }
    );
    assert!(operations[key]
        .to_string()
        .contains("reads the vault when this step runs"));
    assert!(key < position(|op| matches!(op, SetupOperation::EnableComponent { .. })));
    assert!(!operations.contains(&SetupOperation::RegisterCerebellumNativeHost));
}

#[cfg(unix)]
#[test]
fn failing_cerebellum_grant_undoes_its_component_but_keeps_committed_broca() {
    use crate::setup::{
        components::{component_binaries, host_alpha_target},
        release_index::{IndexAsset, IndexComponent, ReleaseIndex},
        test_exec::write_executable,
    };
    use sha2::{Digest, Sha256};
    use std::io::Write;
    fn asset(root: &ScratchDir, name: &str) -> IndexAsset {
        let path = root.join(format!("{name}.zip"));
        let mut zip = zip::ZipWriter::new(fs::File::create(&path).unwrap());
        zip.start_file(name, zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"#!/bin/sh\nprintf '1.0.0\\n'\n").unwrap();
        zip.finish().unwrap();
        let bytes = fs::read(&path).unwrap();
        IndexAsset {
            url: format!("file://{}", path.display()),
            sha256: format!("{:x}", Sha256::digest(&bytes)),
            bytes: bytes.len() as u64,
            reports: Some("1.0.0".into()),
        }
    }
    let root = ScratchDir::new("setup-component-local-refusal");
    let mut backend = tests::backend_without_user_manager(&root);
    backend.inventory = inventory(root.path());
    let broca_file = root.join("broca.jsonc");
    backend
        .inventory
        .persist_retained(
            "broca-key-intent",
            &broca_file,
            Map::from_iter([("deployment_key_id".into(), Value::String("42".into()))]),
        )
        .unwrap();
    fs::create_dir_all(&backend.paths.binary_home).unwrap();
    write_executable(&backend.executable, b"#!/bin/sh\nexit 0\n");
    let mut index = ReleaseIndex {
        schema: 1,
        channel: "alpha".into(),
        generated_at_ms: 0,
        components: BTreeMap::new(),
    };
    // Installing a component fetches the host's program set (Cerebellum on
    // macOS also ships its native-messaging host), so the index publishes
    // exactly that set for the host's target.
    let target = host_alpha_target();
    for component in [Component::Broca, Component::Cerebellum] {
        index.components.insert(
            component.label().into(),
            IndexComponent {
                release: "v1.0.0".into(),
                version: Some("1.0.0".into()),
                requires_core: None,
                assets: BTreeMap::from([(
                    target.label().into(),
                    component_binaries(component)
                        .iter()
                        .map(|name| (name.to_string(), asset(&root, name)))
                        .collect(),
                )]),
            },
        );
    }
    backend.artifacts = ReleaseArtifactSource::from_index(index, target);
    let mut operations = Vec::new();
    for component in [Component::Broca, Component::Cerebellum] {
        operations.extend([
            SetupOperation::InstallComponent { component },
            SetupOperation::ConfigureComponent { component },
        ]);
        if component == Component::Broca {
            operations.push(SetupOperation::EnsureBrocaKey {
                path: broca_file.clone(),
                observation: "adopts intent".into(),
            });
        }
        operations.push(SetupOperation::GrantComponentVault { component });
    }
    const KEY_LIST: &str =
        r#"{"v":1,"credentials":[{"id":"opaque:broca-deployment:42","state":"active"}]}"#;
    backend.runner = Box::new(Fake {
        answers: VecDeque::from([
            (0, KEY_LIST),
            (0, "already granted (no change)"),
            (0, "already granted (no change)"),
            (0, "already granted (no change)"),
            (0, "already granted (no change)"),
            (4, ""),
        ]),
        ..Default::default()
    });
    let plan = SetupPlan {
        operations,
        outcomes: Vec::new(),
        validation: vec![Component::Broca, Component::Cerebellum],
        permission_repair: false,
    };
    let error = backend.execute_transactions(&plan, false).unwrap_err();
    assert!(
        error.contains("reserved:cerebellum, category, browser-session, deposit"),
        "{error}"
    );
    assert!(error.ends_with("ck setup --with broca"), "{error}");
    assert!(!broca_file.exists());
    assert!(!backend.inventory.owns_path("settings-receipt", &broca_file));
    assert!(!backend.inventory.owns_path("broca-key-intent", &broca_file));
    let broca = backend.paths.binary_home.join("ck-broca");
    let cerebellum = backend.paths.binary_home.join("ck-cerebellum");
    assert!(broca.is_file());
    assert!(components::owned_binary_paths(&backend.inventory).contains(&broca));
    assert!(!cerebellum.exists());
    assert!(!components::owned_binary_paths(&backend.inventory).contains(&cerebellum));
    let config = fs::read_to_string(&backend.paths.config_path).unwrap();
    assert!(
        config.contains("ck-broca") && !config.contains("ck-cerebellum"),
        "{config}"
    );
}

#[cfg(unix)]
#[test]
fn second_bare_setup_plans_ensures_but_leaves_files_and_inventory_unchanged() {
    use crate::setup::test_exec::write_executable;
    struct UnchangedVault;
    impl CommandRunner for UnchangedVault {
        fn run(
            &mut self,
            program: &str,
            args: &[String],
        ) -> Result<runtime::CommandResult, String> {
            let stdout = match args[0].as_str() {
                "list" => {
                    assert!(program.ends_with("ck-auth"));
                    r#"{"v":1,"credentials":[{"id":"opaque:broca-deployment:1","state":"active"}]}"#
                }
                "grant" => {
                    assert!(program.ends_with("ck-auth"));
                    "already granted (no change)"
                }
                "--json" => {
                    assert!(program.ends_with("ck"));
                    r#"{"module":{"live":true}}"#
                }
                "--register" => {
                    assert!(program.ends_with("ck-cerebellum-native-host"));
                    ""
                }
                _ => panic!("unexpected command {program} {args:?}"),
            };
            Ok(runtime::CommandResult {
                success: true,
                exit_code: Some(0),
                stdout: stdout.into(),
                stderr: String::new(),
            })
        }
    }
    fn files(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        let mut snapshot = BTreeMap::new();
        for entry in fs::read_dir(root).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                snapshot.extend(files(&path));
            } else {
                snapshot.insert(path.clone(), fs::read(&path).unwrap());
            }
        }
        snapshot
    }
    let root = ScratchDir::new("setup-second-bare-no-disk-change");
    let mut backend = tests::backend_without_user_manager(&root);
    backend.inventory = inventory(root.path());
    backend.platform = RuntimePlatform::Macos;
    backend.runner = Box::new(UnchangedVault);
    fs::create_dir_all(&backend.paths.binary_home).unwrap();
    write_executable(&backend.executable, b"#!/bin/sh\nprintf '%s\\n' '{\"status\":\"ok\",\"metrics\":{\"connectorHome\":{\"state\":\"held\",\"lease\":{\"holding\":true}}}}'\n");
    write_executable(
        &backend.paths.binary_home.join("ck-plexus-admin"),
        b"#!/bin/sh\ncase \"$1 $2\" in\n'connector-home status') printf '%s\\n' '{\"result\":{\"connectorHome\":{\"admitted\":true}}}' ;;\n'agent-policy preview') printf '%s\\n' '{\"enabled\":true}' ;;\n*) exit 64 ;;\nesac\n",
    );
    fs::write(&backend.paths.config_path, "{\"version\":1}\n").unwrap();
    backend.inventory.start_transaction().unwrap();
    backend.apply(&SetupOperation::ConfigureMessageHub).unwrap();
    for name in ["fusiform.jsonc", "broca.jsonc", "cerebellum.jsonc"] {
        let file = root.join(name);
        assert!(settings::write(&file, b"{}\n", &mut backend.inventory).unwrap());
    }
    backend
        .inventory
        .commit_transaction(&mut FileRecovery)
        .unwrap();
    let before = files(root.path());
    let mut observed = installed_observation();
    observed.platform = PlatformObservation::Supported(AlphaTarget::DarwinArm64);
    let index = backend.artifacts.cloned_index().unwrap();
    let request = SetupRequest::install(Vec::new());
    backend.observe_module_steps(&request, &index, &mut observed);
    let plan = crate::setup::plan_setup(&observed, &request);
    assert!(plan.mutation_count() >= 6);
    let rendered = plan
        .operations
        .iter()
        .filter(|step| step.is_module_ensure())
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    assert_eq!(
        rendered,
        [
            "ensure astrocyte vault grants",
            "ensure striatum vault grants",
            "ensure Broca deployment key: found 1 active key; ensures Broca's read grant on each",
            "ensure broca vault grants",
            "ensure cerebellum vault grants",
            "ensure Chrome native messaging with ck-cerebellum-native-host --register",
            "ensure plexus vault grants",
        ]
    );
    assert!(!plan.operations.iter().any(|step| matches!(
        step,
        SetupOperation::InstallComponent { .. } | SetupOperation::ConfigureComponent { .. }
    )));
    backend.execute_transactions(&plan, false).unwrap();
    assert!(!backend.component_steps[&Component::Plexus].plexus_maintenance_changed);
    assert_eq!(files(root.path()), before);
}

#[test]
fn observed_broca_refusal_is_left_out_under_bare_setup_but_blocks_an_explicit_pick() {
    use crate::setup::model::PlanOutcome;
    for (listing, named) in [
        (r#"{"v":2,"credentials":[]}"#, "unsupported v=2"),
        (
            r#"{"v":1,"credentials":[{"id":"opaque:broca-deployment:0","state":"active"}]}"#,
            "opaque:broca-deployment:0",
        ),
    ] {
        let root = ScratchDir::new("setup-observable-broca-refusal");
        let mut backend = tests::backend_without_user_manager(&root);
        fs::write(
            &backend.paths.config_path,
            serde_json::json!({"version":1,"storage":{"backend":"sqlite","data_home":root.path()}})
                .to_string(),
        )
        .unwrap();
        let index = backend.artifacts.cloned_index().unwrap();
        backend.runner = Box::new(Fake {
            answers: VecDeque::from([(0, listing), (0, listing)]),
            ..Default::default()
        });
        for explicit in [false, true] {
            let request = SetupRequest::install(if explicit {
                vec![Component::Broca]
            } else {
                Vec::new()
            });
            let mut observed = installed_observation();
            backend.observe_module_steps(&request, &index, &mut observed);
            let plan = crate::setup::plan_setup(&observed, &request);
            assert!(!plan.validation.contains(&Component::Broca));
            assert_eq!(plan.is_authorized(), !explicit);
            assert!(
                plan.outcomes.iter().any(|outcome| match outcome {
                    PlanOutcome::Refusal { reason } => explicit && reason.contains(named),
                    PlanOutcome::LeftOut {
                        component: Component::Broca,
                        reason,
                    } => !explicit && reason.contains(named),
                    _ => false,
                }),
                "{plan:?}"
            );
        }
    }
}

#[test]
fn fresh_mac_extension_plan_precedes_enable_and_owned_old_program_gets_upgrade_hint() {
    use crate::setup::release_index::{IndexAsset, IndexComponent, ReleaseIndex};
    let root = ScratchDir::new("setup-extension-release-plan");
    let mut backend = tests::backend_without_user_manager(&root);
    backend.platform = RuntimePlatform::Macos;
    fs::write(root.join("cerebellum.jsonc"), "{}\n").unwrap();
    let component = Component::Cerebellum;
    let request = SetupRequest::install(vec![component]);
    let asset = |digit: char| IndexAsset {
        url: "https://example.invalid/release.zip".into(),
        sha256: digit.to_string().repeat(64),
        bytes: 1,
        reports: None,
    };
    let index = ReleaseIndex {
        schema: 1,
        channel: "alpha".into(),
        generated_at_ms: 0,
        components: BTreeMap::from([(
            component.label().into(),
            IndexComponent {
                release: "v1.0.0".into(),
                version: Some("1.0.0".into()),
                requires_core: None,
                assets: BTreeMap::from([(
                    "darwin-arm64".into(),
                    BTreeMap::from([
                        ("ck-cerebellum".into(), asset('1')),
                        (extension::ASSET.into(), asset('2')),
                    ]),
                )]),
            },
        )]),
    };
    let mut observed = installed_observation();
    observed.platform = PlatformObservation::Supported(AlphaTarget::DarwinArm64);
    observed
        .components
        .insert(component, ComponentState::Missing);
    backend.observe_module_steps(&request, &index, &mut observed);
    let plan = crate::setup::plan_setup(&observed, &request);
    let ext = plan
        .operations
        .iter()
        .position(|step| matches!(step, SetupOperation::EnsureCerebellumExtension { .. }))
        .unwrap();
    let enable = plan
        .operations
        .iter()
        .position(|step| step == &SetupOperation::EnableComponent { component })
        .unwrap();
    assert_eq!(
        plan.operations[ext - 3],
        SetupOperation::ConfigureComponent { component }
    );
    assert_eq!(
        plan.operations[ext - 2],
        SetupOperation::GrantComponentVault { component }
    );
    assert_eq!(
        plan.operations[ext - 1],
        SetupOperation::RegisterCerebellumNativeHost
    );
    assert!(ext < enable);
    assert!(plan.operations[ext]
        .to_string()
        .contains("chrome://extensions"));
    assert!(!plan
        .outcomes
        .iter()
        .any(|outcome| outcome.to_string().contains("refresh the Chrome extension")));
    assert!(!extension::Paths::new(&backend.paths.data_dir).live.exists());

    let mut observed = installed_observation();
    observed.platform = PlatformObservation::Supported(AlphaTarget::DarwinArm64);
    let program = backend.paths.binary_home.join("ck-cerebellum");
    fs::create_dir_all(&backend.paths.binary_home).unwrap();
    fs::write(&program, b"earlier release").unwrap();
    backend.inventory.record(
        "managed-binary",
        &program,
        Map::from_iter([("archive_sha256".into(), Value::String("3".repeat(64)))]),
    );
    backend.observe_module_steps(&request, &index, &mut observed);
    let plan = crate::setup::plan_setup(&observed, &request);
    assert!(!plan
        .operations
        .iter()
        .any(|step| matches!(step, SetupOperation::EnsureCerebellumExtension { .. })));
    assert!(plan.outcomes.iter().any(|outcome| outcome
        .to_string()
        .contains("run ck upgrade, then ck setup, to refresh the Chrome extension")));
}
