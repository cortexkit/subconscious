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
