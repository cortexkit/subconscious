fn nats_setup_fixture(root: &ScratchDir, enabled: bool) -> (SetupBackend, String) {
    let mut backend = backend_without_user_manager(root);
    fs::create_dir_all(root.join("bin")).unwrap();
    for name in ["nats-server", "ck-nats-server", "ck-nats-engine"] {
        fs::write(
            root.join("bin")
                .join(format!("{name}{}", if cfg!(windows) { ".exe" } else { "" })),
            name,
        )
        .unwrap();
    }
    backend.inventory.record(
        "managed-binary",
        &root.join(format!(
            "bin/nats-server{}",
            if cfg!(windows) { ".exe" } else { "" }
        )),
        Map::new(),
    );
    let text = serde_json::to_string_pretty(&serde_json::json!({
        "modules": {"nats-server": {
            "program": root.join(format!("bin/nats-server{}", if cfg!(windows) { ".exe" } else { "" })),
            "args": ["-c", root.join("nats/server.conf")],
            "protocol": "none", "enabled": enabled,
            "health": {"http": "http://127.0.0.1:18222/healthz"},
            "restart": {"max_attempts": 3}, "env": {"KEEP": "yes"}
        }}
    })).unwrap();
    fs::write(&backend.paths.config_path, &text).unwrap();
    (backend, text)
}

fn nats_entry(backend: &SetupBackend) -> Value {
    subc_jsonc::read_document(&backend.paths.config_path)
        .unwrap()
        .1["modules"]["nats-server"]
        .clone()
}

#[test]
fn nats_setup_uses_injected_config_without_a_configuration_inventory_row() {
    let root = ScratchDir::new("nats-setup-explicit-config");
    let (mut backend, _) = nats_setup_fixture(&root, false);
    assert!(backend.inventory.paths_for_kind("configuration").is_empty());
    let previous_path = backend.paths.config_path.clone();
    let config = root.join("installation/daemon.jsonc");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::rename(&previous_path, &config).unwrap();
    let decoy = r#"{"modules":{"nats-server":{"program":"operator-server"}}}"#;
    fs::write(&previous_path, decoy).unwrap();
    backend.paths.config_path = config;
    backend
        .apply(&SetupOperation::ConfigureComponent {
            component: Component::Core,
        })
        .unwrap();
    assert_eq!(nats_entry(&backend)["protocol"], "subc");
    assert_eq!(fs::read_to_string(previous_path).unwrap(), decoy);
}

#[test]
fn nats_setup_rewrites_existing_entry_without_moving_config_or_store() {
    let root = ScratchDir::new("nats-setup-entry");
    let (mut backend, _) = nats_setup_fixture(&root, true);
    backend
        .apply(&SetupOperation::ConfigureComponent {
            component: Component::Core,
        })
        .unwrap();
    let entry = nats_entry(&backend);
    assert_eq!(
        PathBuf::from(entry["program"].as_str().unwrap()),
        root.join("bin").join(format!(
            "ck-nats-server{}",
            if cfg!(windows) { ".exe" } else { "" }
        ))
    );
    assert_eq!(entry["protocol"], "subc");
    assert_eq!(entry["registration_channel"], true);
    assert_eq!(entry["reserved"], true);
    assert_eq!(
        entry["args"],
        serde_json::json!([
            "-c",
            root.join("nats/server.conf"),
            "--subc",
            subc_daemon::bootstrap::connection_file_path()
        ])
    );
    assert!(entry.get("health").is_none());
    assert_eq!(entry["env"]["KEEP"], "yes");
    assert_eq!(entry["restart"]["max_attempts"], 3);
    assert!(
        root.join(format!(
            "bin/nats-server{}",
            if cfg!(windows) { ".exe" } else { "" }
        ))
        .exists(),
        "stock server stays until verification"
    );
}

#[test]
fn nats_setup_disabled_entry_is_rewritten_without_health_wait() {
    let root = ScratchDir::new("nats-setup-disabled");
    let (mut backend, _) = nats_setup_fixture(&root, false);
    backend
        .apply(&SetupOperation::ConfigureComponent {
            component: Component::Core,
        })
        .unwrap();
    assert_eq!(nats_entry(&backend)["protocol"], "subc");
    assert_eq!(nats_entry(&backend)["enabled"], false);
    backend
        .component_steps
        .get_mut(&Component::Core)
        .unwrap()
        .nats_migration
        .as_mut()
        .unwrap()
        .complete(&mut backend.inventory)
        .unwrap();
    assert!(!root
        .join(format!(
            "bin/nats-server{}",
            if cfg!(windows) { ".exe" } else { "" }
        ))
        .exists());
}

#[test]
fn core_repair_leaves_a_stopped_managed_nats_wrapper_to_the_bus_step() {
    use std::{cell::Cell, rc::Rc, time::{Duration, Instant}};
    use serde_json::json;

    struct StoppedNats {
        restarted: Rc<Cell<bool>>,
        config: PathBuf,
    }

    impl bus::BusControl for StoppedNats {
        fn port_is_free(&mut self, _port: u16) -> bool {
            panic!("the managed bus does not probe ports");
        }

        fn status(&mut self, module: &str) -> Result<Option<bus::BusStatus>, String> {
            let live = module == "ckbus" || self.restarted.get();
            Ok(Some(bus::BusStatus { live, healthy: live }))
        }

        fn provenance(&mut self, module: &str) -> Result<Option<u32>, String> {
            assert_eq!(module, "nats-server");
            Ok(self.restarted.get().then_some(42))
        }

        fn rescan(&mut self) -> Result<bus::BusRescan, String> {
            panic!("settled bus entries do not need a rescan");
        }

        fn restart(&mut self, module: &str, _drain: Duration) -> Result<(), String> {
            assert_eq!(module, "nats-server");
            assert!(!self.restarted.replace(true));
            let (_, document) = subc_jsonc::read_document(&self.config).unwrap();
            assert!(document["modules"]["subc-mcp"].is_object(), "Core must repair its gateway first");
            Ok(())
        }

        fn now(&mut self) -> Instant {
            Instant::now()
        }

        fn sleep(&mut self, _duration: Duration) {
            panic!("the restarted wrapper is immediately ready");
        }
    }

    struct BusRunner(Value);
    impl CommandRunner for BusRunner {
        fn run(&mut self, program: &str, args: &[String]) -> Result<runtime::CommandResult, String> {
            let stdout = match args[0].as_str() {
                "--json" => json!({"module":{"live":true}}).to_string(),
                "grant" => "already granted (no change)".into(),
                "list" => json!({"v":1,"credentials":[
                    {"id":"signing:ck-bus-operator-signer:1","state":"active"},
                    {"id":"signing:ck-bus-sysaccount:1","state":"active"},
                    {"id":"signing:ck-bus-account:1","state":"active"}
                ]}).to_string(),
                "public-key" => format!("public_key_hex {}\n", "12".repeat(32)),
                "install-plan" => json!({"status":"no ceremony needed"}).to_string(),
                "install-apply" => json!({"status":"applied", "env":self.0}).to_string(),
                _ => panic!("unexpected command {program} {args:?}; legacy verification must not run"),
            };
            Ok(runtime::CommandResult { success: true, exit_code: Some(0), stdout, stderr: String::new() })
        }
    }

    let root = ScratchDir::new("setup-repair-with-stopped-managed-nats");
    let mut backend = backend_without_user_manager(&root);
    backend.inventory = Inventory::load_locked(
        backend.inventory.manifest().to_owned(), "linux-x64", "setup test", Duration::ZERO,
    ).unwrap();
    let nats = backend.paths.data_dir.join("nats");
    let bin = &backend.paths.binary_home;
    fs::create_dir_all(bin).unwrap();
    let ckbus = bin.join(components::platform_binary("ck-bus"));
    let wrapper = bin.join(components::platform_binary("ck-nats-server"));
    let engine = bin.join(components::platform_binary("ck-nats-engine"));
    for path in [&ckbus, &wrapper, &engine, &backend.auth_path()] {
        fs::write(path, b"fixture binary").unwrap();
        backend.inventory.record("managed-binary", path, Map::new());
    }
    let record = json!({"state":"applied", "root_pub":"34".repeat(32),
        "signer_pub":"12".repeat(32), "sysaccount_pub":"12".repeat(32), "activation_pending":false});
    backend.inventory.save().unwrap();
    backend.inventory.persist_retained(bus::BUS_TRUST, &nats, record.as_object().unwrap().clone()).unwrap();
    let env = json!({"CKBUS_NATS_URL":"nats://127.0.0.1:14222",
        "CKBUS_OPERATOR_JWT":nats.join("operator.jwt"), "CKBUS_SYSTEM_ACCOUNT":"fixture-account"});
    let config = json!({"modules":{
        "nats-server":{"program":wrapper,
            "args":["-c",nats.join("server.conf"),"--subc",subc_daemon::bootstrap::connection_file_path()],
            "protocol":"subc", "registration_channel":true, "reserved":true, "enabled":true},
        "ckbus":{"program":ckbus, "args":[], "env":env, "registration_channel":true,
            "reserved":true, "enabled":true},
    }});
    fs::write(&backend.paths.config_path, config.to_string()).unwrap();
    let restarted = Rc::new(Cell::new(false));
    backend.bus_control = Box::new(StoppedNats { restarted: restarted.clone(), config: backend.paths.config_path.clone() });
    backend.runner = Box::new(BusRunner(env));

    backend.execute_transactions(&SetupPlan {
        operations: vec![SetupOperation::ConfigureComponent { component: Component::Core },
            SetupOperation::GrantMessageKeys, SetupOperation::ConfigureMessageBus],
        outcomes: vec![], validation: vec![], permission_repair: false,
    }, false).unwrap();

    assert!(restarted.get());
    assert!(backend.component_steps[&Component::Core].message_bus_changed);
    let row = backend.inventory.entry_for_path(bus::BUS_TRUST, &nats).unwrap();
    assert_eq!(row["activation_pending"], false);
}

#[test]
fn nats_setup_verification_failure_restores_entry_and_stock_binary() {
    let root = ScratchDir::new("nats-setup-rollback");
    let (mut backend, _) = nats_setup_fixture(&root, true);
    let original = nats_entry(&backend);
    backend.inventory.start_transaction().unwrap();
    backend
        .apply(&SetupOperation::ConfigureComponent {
            component: Component::Core,
        })
        .unwrap();
    backend.apply(&SetupOperation::ConfigureMessageHub).unwrap();
    assert_eq!(nats_entry(&backend)["protocol"], "subc");
    backend
        .inventory
        .undo_transaction(&mut crate::setup::install_journal::bootstrap::FileRecovery)
        .unwrap();
    assert_eq!(nats_entry(&backend), original);
    assert!(subc_jsonc::read_document(&backend.paths.config_path)
        .unwrap()
        .1["modules"]
        .get("nats-hub")
        .is_none());
    assert_eq!(
        fs::read(root.join(format!(
            "bin/nats-server{}",
            if cfg!(windows) { ".exe" } else { "" }
        )))
        .unwrap(),
        b"nats-server"
    );
}

#[test]
fn message_plane_setup_hub_repeat_and_rollback_keep_existing_entries() {
    for existing in [false, true] {
        let root = ScratchDir::new("message-setup-hub");
        let (mut backend, _) = nats_setup_fixture(&root, false);
        let path = backend.paths.config_path.clone();
        if existing {
            let (mut text, _) = subc_jsonc::read_document(&path).unwrap();
            subc_jsonc::set_member(
                &mut text,
                &["modules", "nats-hub"],
                &serde_json::json!({"enabled":false,"program":"operator-hub"}),
            )
            .unwrap();
            fs::write(&path, text).unwrap();
        }
        let before = fs::read(&path).unwrap();
        backend.inventory.start_transaction().unwrap();
        backend.apply(&SetupOperation::ConfigureMessageHub).unwrap();
        let after = fs::read(&path).unwrap();
        if !existing {
            let doc = subc_jsonc::read_document(&path).unwrap().1;
            assert_eq!(
                doc["modules"]["nats-hub"]["args"][1],
                root.join("data")
                    .join("nats-hub")
                    .join("server.conf")
                    .to_string_lossy()
                    .as_ref()
            );
        }
        backend.apply(&SetupOperation::ConfigureMessageHub).unwrap();
        assert_eq!(fs::read(&path).unwrap(), after);
        backend
            .inventory
            .undo_transaction(&mut crate::setup::install_journal::bootstrap::FileRecovery)
            .unwrap();
        assert_eq!(fs::read(&path).unwrap(), before);
    }
}

#[test]
fn message_plane_setup_uses_the_installed_vault_cli_after_it_is_live() {
    struct VaultRunner {
        auth: PathBuf,
        calls: std::sync::Arc<std::sync::Mutex<usize>>,
    }
    impl CommandRunner for VaultRunner {
        fn run(
            &mut self,
            program: &str,
            args: &[String],
        ) -> Result<runtime::CommandResult, String> {
            let mut count = self.calls.lock().unwrap();
            let stdout = if *count == 0 {
                assert_eq!(args, ["--json", "module", "status", "claustrum"]);
                serde_json::json!({"module":{"live":true}}).to_string()
            } else {
                assert_eq!(Path::new(program), self.auth);
                assert!(Path::new(program).is_absolute());
                assert_eq!(args[0], "grant");
                "already granted (no change)".into()
            };
            *count += 1;
            Ok(runtime::CommandResult {
                success: true,
                exit_code: Some(0),
                stdout,
                stderr: String::new(),
            })
        }
    }
    let root = ScratchDir::new("message-setup-vault-cli");
    let mut backend = backend_without_user_manager(&root);
    let auth = backend.auth_path();
    backend.inventory.record("managed-binary", &auth, Map::new());
    let calls = std::sync::Arc::new(std::sync::Mutex::new(0));
    backend.runner = Box::new(VaultRunner {
        auth: root.join("bin").join(if cfg!(windows) {
            "ck-auth.exe"
        } else {
            "ck-auth"
        }),
        calls: calls.clone(),
    });
    backend.apply(&SetupOperation::GrantMessageKeys).unwrap();
    // One vault status read, then one grant per line: five signing keys, each
    // for read and sign; the hub token audience; ck-bus's open on the bus seal
    // key and sign on the message-signing key; and Callosum's read on the three
    // public keys it announces to paired computers; and Entorhinal's read and
    // sign on the fleet-record key.
    assert_eq!(*calls.lock().unwrap(), 19);
    assert!(!backend.component_steps[&Component::Claustrum].message_grants_changed);
}

#[test]
fn message_grants_refuse_an_unowned_vault_cli_before_running_it() {
    struct MustNotRun;
    impl CommandRunner for MustNotRun {
        fn run(&mut self, program: &str, args: &[String]) -> Result<runtime::CommandResult, String> {
            panic!("unowned vault must refuse before running any command: {program} {args:?}");
        }
    }
    let root = ScratchDir::new("message-setup-unowned-vault-cli");
    let mut backend = backend_without_user_manager(&root);
    let auth = backend.auth_path();
    fs::create_dir_all(auth.parent().unwrap()).unwrap();
    fs::write(&auth, b"unowned vault").unwrap();
    backend.runner = Box::new(MustNotRun);

    assert_eq!(
        backend.apply(&SetupOperation::GrantMessageKeys).unwrap_err(),
        super::super::text::unowned_binary_refusal(&auth),
    );
}

#[test]
fn nats_setup_refuses_an_operator_program_by_name() {
    let root = ScratchDir::new("nats-setup-operator");
    let (mut backend, text) = nats_setup_fixture(&root, true);
    let mut document: Value = serde_json::from_str(&text).unwrap();
    document["modules"]["nats-server"]["program"] = serde_json::json!(root.join("bin").join(
        format!("operator-server{}", if cfg!(windows) { ".exe" } else { "" })
    ));
    let text = serde_json::to_string_pretty(&document).unwrap();
    fs::write(&backend.paths.config_path, &text).unwrap();
    let error = backend
        .apply(&SetupOperation::ConfigureComponent {
            component: Component::Core,
        })
        .unwrap_err();
    assert!(error.contains("operator-server"), "{error}");
    assert_eq!(
        fs::read_to_string(&backend.paths.config_path).unwrap(),
        text
    );
}

#[test]
fn nats_setup_fresh_core_has_pair_but_does_not_create_a_bus() {
    let root = ScratchDir::new("nats-setup-fresh");
    let mut backend = backend_without_user_manager(&root);
    backend
        .apply(&SetupOperation::ConfigureComponent {
            component: Component::Core,
        })
        .unwrap();
    assert!(nats_entry(&backend).is_null());
    for binary in ["ck-nats-server", "ck-nats-engine"] {
        assert!(
            components::component_binaries(Component::Core).contains(&binary),
            "missing {binary}"
        );
    }
}
