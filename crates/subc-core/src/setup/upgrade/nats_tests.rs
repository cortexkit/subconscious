fn nats_upgrade_fixture(
    root: &cortexkit_test_support::ScratchDir,
    enabled: bool,
) -> (SystemUpgradeBackend, UpgradeTarget, Value) {
    let home = root.join("bin");
    fs::create_dir_all(&home).unwrap();
    for binary in ["ck-subc", "nats-server", "ck-nats-server", "ck-nats-engine"] {
        fs::write(
            home.join(format!(
                "{binary}{}",
                if cfg!(windows) { ".exe" } else { "" }
            )),
            binary,
        )
        .unwrap();
    }
    let target = UpgradeTarget {
        component: Component::Core,
        binary: "ck-subc",
        module_id: None,
    };
    let mut backend = managed_backend(
        root.path(),
        target,
        &home.join(format!(
            "ck-subc{}",
            if cfg!(windows) { ".exe" } else { "" }
        )),
    );
    backend.subc = Some(root.join("connection.json"));
    let config = root.join("subc.jsonc");
    backend
        .inventory
        .record("configuration", &config, Map::new());
    backend.inventory.record(
        "managed-binary",
        &home.join(format!(
            "nats-server{}",
            if cfg!(windows) { ".exe" } else { "" }
        )),
        Map::new(),
    );
    let entry = serde_json::json!({"program": home.join(format!("nats-server{}", if cfg!(windows) { ".exe" } else { "" })), "args": ["-c", root.join("nats/server.conf")], "protocol":"none", "enabled": enabled, "health":{"http":"http://127.0.0.1:18222/healthz"}, "env":{"KEEP":"yes"}});
    fs::write(
        config,
        serde_json::to_string_pretty(&serde_json::json!({"modules":{"nats-server":entry}}))
            .unwrap(),
    )
    .unwrap();
    stage_candidate(&mut backend, root.path(), target, b"new-daemon");
    (backend, target, entry)
}

#[test]
fn nats_upgrade_uses_injected_config_without_a_configuration_inventory_row() {
    for stale_inventory in [false, true] {
        let root = cortexkit_test_support::ScratchDir::new("nats-upgrade-explicit-config");
        let (mut backend, daemon, _) = nats_upgrade_fixture(&root, false);
        let config = root.join("subc.jsonc");
        backend
            .inventory
            .remove_owned_path("configuration", &config);
        let other_config = root.join("other.jsonc");
        let other_text = r#"{"modules":{"nats-server":{"program":"operator-server"}}}"#;
        fs::write(&other_config, other_text).unwrap();
        if stale_inventory {
            backend
                .inventory
                .record("configuration", &other_config, Map::new());
        }
        backend.capture_core_cut(daemon).unwrap();
        assert!(backend
            .core_cut
            .nats
            .as_ref()
            .is_some_and(|migration| migration.needs_rewrite()));
        backend.replace_destination(daemon).unwrap();
        assert_eq!(
            subc_jsonc::read_document(&config).unwrap().1["modules"]["nats-server"]["protocol"],
            "subc"
        );
        assert_eq!(fs::read_to_string(&other_config).unwrap(), other_text);
    }
}

#[test]
fn nats_upgrade_migrates_entry_and_retains_stock_until_verified() {
    let root = cortexkit_test_support::ScratchDir::new("nats-upgrade-entry");
    let (mut backend, daemon, _) = nats_upgrade_fixture(&root, false);
    backend.create_rollback_copy(daemon).unwrap();
    backend.replace_destination(daemon).unwrap();
    let entry = subc_jsonc::read_document(&root.join("subc.jsonc"))
        .unwrap()
        .1["modules"]["nats-server"]
        .clone();
    assert_eq!(
        PathBuf::from(entry["program"].as_str().unwrap()),
        root.join("bin").join(format!(
            "ck-nats-server{}",
            if cfg!(windows) { ".exe" } else { "" }
        ))
    );
    assert_eq!(entry["protocol"], "subc");
    assert_eq!(entry["enabled"], false);
    assert!(root
        .join(format!(
            "bin/nats-server{}",
            if cfg!(windows) { ".exe" } else { "" }
        ))
        .is_file());
    backend.completed(daemon);
    assert!(!root
        .join(format!(
            "bin/nats-server{}",
            if cfg!(windows) { ".exe" } else { "" }
        ))
        .exists());
}

#[test]
fn nats_upgrade_rollback_restores_entry_stock_and_daemon() {
    let root = cortexkit_test_support::ScratchDir::new("nats-upgrade-rollback");
    let (mut backend, daemon, original) = nats_upgrade_fixture(&root, true);
    backend.create_rollback_copy(daemon).unwrap();
    backend.replace_destination(daemon).unwrap();
    assert_eq!(
        subc_jsonc::read_document(&root.join("subc.jsonc"))
            .unwrap()
            .1["modules"]["nats-server"]["protocol"],
        "subc"
    );
    backend.rollback(daemon).unwrap();
    assert_eq!(
        subc_jsonc::read_document(&root.join("subc.jsonc"))
            .unwrap()
            .1["modules"]["nats-server"],
        original
    );
    assert_eq!(
        fs::read(root.join(format!(
            "bin/ck-subc{}",
            if cfg!(windows) { ".exe" } else { "" }
        )))
        .unwrap(),
        b"ck-subc"
    );
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
fn nats_upgrade_refuses_operator_program_without_touching_daemon() {
    let root = cortexkit_test_support::ScratchDir::new("nats-upgrade-operator");
    let (mut backend, daemon, _) = nats_upgrade_fixture(&root, true);
    let config = root.join("subc.jsonc");
    let (_, mut document) = subc_jsonc::read_document(&config).unwrap();
    document["modules"]["nats-server"]["program"] = serde_json::json!(root.join("bin").join(
        format!("operator-server{}", if cfg!(windows) { ".exe" } else { "" })
    ));
    let text = serde_json::to_string_pretty(&document).unwrap();
    fs::write(&config, &text).unwrap();
    let error = backend.create_rollback_copy(daemon).unwrap_err();
    assert!(error.contains("operator-server"), "{error}");
    assert_eq!(fs::read_to_string(config).unwrap(), text);
    assert_eq!(
        fs::read(root.join(format!(
            "bin/ck-subc{}",
            if cfg!(windows) { ".exe" } else { "" }
        )))
        .unwrap(),
        b"ck-subc"
    );
}

#[test]
fn nats_upgrade_failed_verification_rolls_back_the_whole_cut_automatically() {
    let root = cortexkit_test_support::ScratchDir::new("nats-upgrade-verify-failed");
    let (mut backend, target, original) = nats_upgrade_fixture(&root, true);
    let plan = super::super::planner::UpgradePlan {
        operations: vec![
            super::super::model::UpgradeOperation::CreateRollbackCopy { target },
            super::super::model::UpgradeOperation::ReplaceDestination { target },
            super::super::model::UpgradeOperation::PostVerify { target },
        ],
        outcomes: vec![],
    };
    let failure = super::super::upgrade_executor::execute_upgrade(&plan, &mut backend).unwrap_err();
    assert_eq!(failure.stage, "post-verification");
    assert!(
        failure
            .report
            .evidence
            .iter()
            .any(|e| e.stage == "rollback" && e.detail.contains("NATS entry")),
        "{failure:?}"
    );
    assert_eq!(
        subc_jsonc::read_document(&root.join("subc.jsonc"))
            .unwrap()
            .1["modules"]["nats-server"],
        original
    );
    assert_eq!(
        fs::read(root.join(format!(
            "bin/ck-subc{}",
            if cfg!(windows) { ".exe" } else { "" }
        )))
        .unwrap(),
        b"ck-subc"
    );
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
fn nats_upgrade_failed_cut_removes_new_pair_and_restores_inventory() {
    let root = cortexkit_test_support::ScratchDir::new("nats-upgrade-new-pair-rollback");
    let (mut backend, daemon, original) = nats_upgrade_fixture(&root, true);
    let mut operations = Vec::new();
    for name in ["ck-nats-engine", "ck-nats-server"] {
        let target = UpgradeTarget {
            component: Component::Core,
            binary: name,
            module_id: None,
        };
        let destination = root
            .join("bin")
            .join(format!("{name}{}", if cfg!(windows) { ".exe" } else { "" }));
        fs::remove_file(&destination).unwrap();
        backend.targets.insert(
            name.into(),
            ManagedUpgradeTarget {
                target,
                destination,
                installed_version: String::new(),
                installed_archive_sha256: None,
            },
        );
        backend.new_companions.insert(name.into());
        backend
            .expected_versions
            .insert(name.into(), "0.1.1".into());
        let staging = root.join(format!("{name}-staging"));
        fs::create_dir(&staging).unwrap();
        stage_candidate(&mut backend, &staging, target, name.as_bytes());
        operations.push(super::super::model::UpgradeOperation::CreateRollbackCopy { target });
        operations.push(super::super::model::UpgradeOperation::ReplaceDestination { target });
    }
    operations.extend([
        super::super::model::UpgradeOperation::CreateRollbackCopy { target: daemon },
        super::super::model::UpgradeOperation::ReplaceDestination { target: daemon },
        super::super::model::UpgradeOperation::PostVerify { target: daemon },
    ]);
    let plan = super::super::planner::UpgradePlan {
        operations,
        outcomes: vec![],
    };
    let failure = super::super::upgrade_executor::execute_upgrade(&plan, &mut backend).unwrap_err();
    assert_eq!(failure.stage, "post-verification");
    assert!(
        failure
            .report
            .evidence
            .iter()
            .any(|e| e.stage == "rollback" && e.detail.contains("NATS entry")),
        "{failure:?}"
    );
    for name in ["ck-nats-engine", "ck-nats-server"] {
        let destination = &backend.targets[name].destination;
        assert!(!destination.exists(), "{}", destination.display());
        assert!(!backend.inventory.owns_path("binary-placement", destination));
    }
    assert_eq!(
        subc_jsonc::read_document(&root.join("subc.jsonc"))
            .unwrap()
            .1["modules"]["nats-server"],
        original
    );
    assert_eq!(
        fs::read(&backend.targets[daemon.label()].destination).unwrap(),
        b"ck-subc"
    );
}
