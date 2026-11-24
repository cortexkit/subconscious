use super::*;

#[test]
fn both_install_locks_serialize_competing_installers_and_recheck() {
    for tool in ["node", "pi"] {
        let paused = Arc::new(Pause::default());
        let waiting = Arc::new(Pause::default());
        let mut first = Fake::new();
        let mut second = Fake {
            root: first.root.clone(),
            waiting: Some(waiting.clone()),
            ..Fake::new()
        };
        if tool == "node" {
            first.download_pause = Some(paused.clone());
            seed_pi(&first.pi(), BIN, false);
        } else {
            let path = first.path_node("v22.19.0", true);
            first.npm_pause = Some(paused.clone());
            second.probe = Ok(format!("{}\nv22.19.0\n", path.display()));
        }
        let orphan = first.tools(tool).join(".staging-abandoned");
        fs::create_dir_all(&orphan).unwrap();
        fs::write(orphan.join("file"), b"abandoned").unwrap();
        std::thread::scope(|scope| {
            let a = scope.spawn(|| prepare_with(&first).unwrap().launch_with(&first).unwrap());
            paused.entered();
            let b = scope.spawn(|| prepare_with(&second).unwrap().launch_with(&second).unwrap());
            waiting.entered();
            waiting.resume();
            paused.resume();
            a.join().unwrap();
            b.join().unwrap();
        });
        let events = second.events.lock().unwrap();
        assert_eq!(events.iter().filter(|event| *event == "waiting").count(), 1);
        assert!(!events
            .iter()
            .any(|event| event.starts_with("download ") || event == "npm ci"));
        assert_eq!(events.last().unwrap(), "launch");
        no_staging(&second.tools(tool));
    }
}

#[test]
fn runs_share_use_locks_and_never_hold_an_install_lock_during_launch() {
    let paused = Arc::new(Pause::default());
    let first = Fake {
        launch_pause: Some(paused.clone()),
        ..Fake::new()
    };
    let second = Fake {
        root: first.root.clone(),
        ..Fake::new()
    };
    prepare_with(&first).unwrap();
    std::thread::scope(|scope| {
        let a = scope.spawn(|| prepare_with(&first).unwrap().launch_with(&first).unwrap());
        paused.entered();
        for (tool, version) in [("node", NODE_VERSION), ("pi", PI_VERSION)] {
            let root = first.open_managed(&["tools", tool], false);
            let install = root.open_lock(".lock").unwrap();
            install.try_lock().unwrap();
            let active = root.open_lock(&format!(".use-{version}")).unwrap();
            assert!(matches!(
                active.try_lock(),
                Err(fs::TryLockError::WouldBlock)
            ));
        }
        prepare_with(&second).unwrap().launch_with(&second).unwrap();
        paused.resume();
        a.join().unwrap();
    });
    let events = second.events.lock().unwrap();
    assert!(!events
        .iter()
        .any(|event| event == "waiting" || event.starts_with("download ") || event == "npm ci"));
    assert_eq!(events.last().unwrap(), "launch");
}

#[test]
fn retention_preserves_newer_nonversions_best_lower_and_in_use() {
    let fake = Fake::new();
    let versions = [
        "0.1.0",
        "0.2.0",
        "0.10.0",
        "0.9.0",
        "999.0.0",
        "v0.0.1",
        "0.0.1-rc1",
    ];
    for tool in ["node", "pi"] {
        for version in versions {
            fs::create_dir_all(fake.tools(tool).join(version)).unwrap();
        }
    }
    let node = fake.open_managed(&["tools", "node"], true);
    let pi = fake.open_managed(&["tools", "pi"], true);
    let node_reader = node.open_lock(".use-0.1.0").unwrap();
    let pi_reader = pi.open_lock(".use-0.1.0").unwrap();
    node_reader.lock_shared().unwrap();
    pi_reader.lock_shared().unwrap();
    prepare_with(&fake).unwrap();
    for tool in ["node", "pi"] {
        let folder = fake.tools(tool);
        for kept in ["0.1.0", "0.10.0", "999.0.0", "v0.0.1", "0.0.1-rc1"] {
            assert!(folder.join(kept).is_dir(), "{tool}: {kept}");
        }
        for pruned in ["0.2.0", "0.9.0"] {
            assert!(!folder.join(pruned).exists(), "{tool}: {pruned}");
            assert!(folder.join(format!(".use-{pruned}")).is_file());
        }
    }
    drop((node_reader, pi_reader));
    fs::remove_dir_all(fake.node_root()).unwrap();
    fs::remove_dir_all(fake.pi()).unwrap();
    prepare_with(&fake).unwrap();
    assert!(!fake.tools("node/0.1.0").exists());
    assert!(!fake.tools("pi/0.1.0").exists());
}

#[test]
fn failed_retention_delete_warns_without_failing_the_run() {
    let fake = Fake {
        fail_prune: true,
        ..Fake::new()
    };
    for version in ["0.1.0", "0.2.0"] {
        fs::create_dir_all(fake.tools("pi").join(version)).unwrap();
    }
    prepare_with(&fake).unwrap().launch_with(&fake).unwrap();
    assert!(fake.tools("pi/0.1.0").exists());
    assert!(fake
        .events
        .lock()
        .unwrap()
        .iter()
        .any(|event| event.starts_with("warning could not prune ")
            && event.ends_with("injected version deletion failure")));
}

#[test]
fn sessions_created_at_same_timestamp_are_exclusive() {
    let fake = Fake::new().seeded();
    let first = prepare_with(&fake).unwrap();
    let second = prepare_with(&fake).unwrap();
    assert_eq!(
        first.session_dir(),
        fake.sessions().join("20240229T000000Z")
    );
    assert_eq!(
        second.session_dir(),
        fake.sessions().join("20240229T000000Z-2")
    );
    first.write_session_file("report.json", b"first").unwrap();
    second.write_session_file("report.json", b"second").unwrap();
    assert_eq!(
        fs::read(first.session_dir().join("report.json")).unwrap(),
        b"first"
    );
    assert_eq!(
        fs::read(second.session_dir().join("report.json")).unwrap(),
        b"second"
    );
}

#[test]
fn forget_only_removes_credential_and_reports_absent_without_writing() {
    let fake = Fake::new().seeded();
    let run = prepare_with(&fake).unwrap();
    let profile = fake.open_managed(&["rescue", PROFILE], false);
    profile
        .new_file(AUTH)
        .unwrap()
        .write_all(b"credential")
        .unwrap();
    profile
        .new_file("transcript.json")
        .unwrap()
        .write_all(b"keep")
        .unwrap();
    fake.events.lock().unwrap().clear();
    let result = forget_with(&fake).unwrap();
    assert_eq!(result.path, fake.profile().join(AUTH));
    assert!(result.removed);
    assert!(!result.path.exists());
    assert!(!forget_with(&fake).unwrap().removed);
    assert_eq!(
        fs::read(fake.profile().join("transcript.json")).unwrap(),
        b"keep"
    );
    assert!(run.session_dir().is_dir());
    assert!(fake.events.lock().unwrap().is_empty());
}

#[test]
fn rescue_refuses_a_missing_data_home_without_creating_ancestors() {
    let mut fake = Fake::new();
    let missing = fake.root.with_file_name("missing-ancestor");
    let data_home = missing.join("data");
    fake.root = data_home.join(INSTALL_DIR);
    let error = fake.error();
    assert!(
        error.starts_with(&format!("{}:", data_home.display())),
        "{error}"
    );
    let error = forget_with(&fake).err().unwrap();
    assert!(
        error.starts_with(&format!("{}:", data_home.display())),
        "{error}"
    );
    assert!(!missing.exists());
    assert!(fake.events.lock().unwrap().is_empty());
}

#[cfg(unix)]
mod unix {
    use std::{
        os::unix::fs::{symlink, PermissionsExt},
        process::Command,
    };

    use super::*;

    fn mode(path: &Path) -> u32 {
        fs::symlink_metadata(path).unwrap().permissions().mode() & 0o7777
    }

    #[test]
    fn rescue_uses_a_data_home_beneath_a_symlinked_ancestor() {
        let mut fake = Fake::new();
        let real = fake.root.with_file_name("real");
        let link = fake.root.with_file_name("linked");
        fs::create_dir_all(real.join("data")).unwrap();
        symlink(&real, &link).unwrap();
        fake.root = link.join("data").join(INSTALL_DIR);

        prepare_with(&fake).unwrap();
        let auth = real.join("data/cortexkit/rescue/pi-profile/auth.json");
        fs::write(&auth, b"credential").unwrap();
        assert!(forget_with(&fake).unwrap().removed);
        assert!(!auth.exists());
        assert!(link.is_symlink());
    }

    #[test]
    fn rescue_refuses_a_symlink_at_the_install_root() {
        let fake = Fake::new();
        let outside = fake.root.with_file_name("outside");
        fs::create_dir(&outside).unwrap();
        symlink(&outside, &fake.root).unwrap();

        let error = fake.error();
        assert!(
            error.starts_with(&format!("{}:", fake.root.display())),
            "{error}"
        );
        assert!(forget_with(&fake).is_err());
        assert!(fake.root.is_symlink());
        assert_eq!(fs::read_dir(outside).unwrap().count(), 0);
    }

    #[test]
    fn rescue_preserves_permissions_of_an_existing_install_root() {
        let fake = Fake::new();
        fs::create_dir(&fake.root).unwrap();
        fs::set_permissions(&fake.root, fs::Permissions::from_mode(0o755)).unwrap();

        prepare_with(&fake).unwrap();
        assert_eq!(mode(&fake.root), 0o755);
        fs::write(fake.profile().join(AUTH), b"credential").unwrap();
        assert!(forget_with(&fake).unwrap().removed);
        assert_eq!(mode(&fake.root), 0o755);
    }

    #[test]
    fn real_tar_install_verifies_node_leaves_private_modes_and_cleans_staging() {
        let mut fake = Fake::new();
        let archive = pins::node_archive(fake.platform).unwrap();
        let payload = fake.root.with_file_name("payload with 'quote'");
        let inner = payload.join(archive.folder());
        let script = seed_node(&inner, fake.platform, true);
        fs::write(&script, format!("#!/bin/sh\nprintf 'v{NODE_VERSION}\\n'\n")).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let archive_path = fake.root.with_file_name("node.tar.gz");
        let output = Command::new("tar")
            .arg("-czf")
            .arg(&archive_path)
            .arg("-C")
            .arg(&payload)
            .arg(archive.folder())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        fake.archive_file = Some(archive_path);
        prepare_with(&fake).unwrap();
        assert!(fake.node_root().join("bin/node").is_file());
        assert!(fake
            .node_root()
            .join("lib/node_modules/npm/bin/npm-cli.js")
            .is_file());
        let mut pending = vec![fake.node_root()];
        while let Some(entry) = pending.pop() {
            assert_eq!(mode(&entry) & 0o077, 0, "{}", entry.display());
            if entry.is_dir() {
                pending.extend(
                    fs::read_dir(entry)
                        .unwrap()
                        .map(|entry| entry.unwrap().path()),
                );
            }
        }
        no_staging(&fake.tools("node"));
        assert!(!fake.tools("node").join(archive.filename()).exists());
    }

    #[test]
    fn lock_symlink_is_refused_on_install_but_valid_tools_skip_install_locks() {
        let fake = Fake::new();
        let target = fake.root.with_file_name("not-created");
        fs::create_dir_all(fake.tools("node")).unwrap();
        symlink(&target, fake.tools("node/.lock")).unwrap();
        assert!(fake.error().contains(".lock"));
        assert!(!target.exists());
        assert!(fake.tools("node/.lock").is_symlink());
        seed_node(&fake.node_root(), fake.platform, false);
        seed_pi(&fake.pi(), BIN, false);
        symlink(&target, fake.tools("pi/.lock")).unwrap();
        prepare_with(&fake).unwrap().launch_with(&fake).unwrap();
        assert!(!target.exists());
        assert!(!fake
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|event| event.starts_with("download ") || event == "npm ci"));
        fs::remove_file(fake.tools("node").join(format!(".use-{NODE_VERSION}"))).unwrap();
        symlink(
            &target,
            fake.tools("node").join(format!(".use-{NODE_VERSION}")),
        )
        .unwrap();
        assert!(fake.error().contains(".use-"));
        assert!(!target.exists());
    }

    #[test]
    fn a_refused_retention_lock_prevents_publish_and_pruning() {
        let fake = Fake::new();
        let target = fake.root.with_file_name("outside-lock");
        for version in ["0.1.0", "0.2.0", "0.3.0"] {
            fs::create_dir_all(fake.tools("node").join(version)).unwrap();
        }
        symlink(&target, fake.tools("node/.use-0.1.0")).unwrap();
        assert!(fake.error().contains(".use-0.1.0"));
        assert!(!fake.node_root().exists());
        assert!(fake.tools("node/0.1.0").is_dir());
        assert!(fake.tools("node/0.2.0").is_dir());
        assert!(!target.exists());
        no_staging(&fake.tools("node"));
    }

    #[test]
    fn cache_log_and_profile_links_are_refused_without_touching_target() {
        for name in [
            "tools",
            "tools/node",
            "tools/pi",
            "tools/npm-cache",
            "tools/npm-cache/_logs",
            "rescue",
            "rescue/pi-profile",
            "rescue/sessions",
        ] {
            let fake = Fake::new();
            let outside = fake.root.with_file_name("outside");
            fs::create_dir(&outside).unwrap();
            fs::set_permissions(&outside, fs::Permissions::from_mode(0o755)).unwrap();
            fs::write(outside.join("sentinel"), b"untouched").unwrap();
            let leaf = fake.root.join(name);
            let parent = leaf.parent().unwrap();
            fs::create_dir_all(parent).unwrap();
            if name == "rescue/pi-profile" {
                fs::set_permissions(parent, fs::Permissions::from_mode(0o700)).unwrap();
            }
            symlink(&outside, &leaf).unwrap();
            assert!(fake.error().contains(leaf.to_str().unwrap()));
            if name == "rescue" || name == "rescue/pi-profile" {
                let error = forget_with(&fake).err().unwrap();
                assert!(
                    error.starts_with(&format!("{}:", leaf.display())),
                    "{error}"
                );
            }
            assert!(leaf.is_symlink());
            assert_eq!(mode(&outside), 0o755);
            assert_eq!(fs::read(outside.join("sentinel")).unwrap(), b"untouched");
            assert_eq!(fs::read_dir(outside).unwrap().count(), 1);
        }
    }

    #[test]
    fn auth_rejects_non_regular_link_foreign_and_hard_link_before_pi() {
        let fake = Fake::new().seeded();
        prepare_with(&fake).unwrap();
        fake.children.lock().unwrap().clear();
        let target = fake.root.with_file_name("outside-auth");
        fs::write(&target, b"unchanged").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();
        let leaf = fake.profile().join(AUTH);
        symlink(&target, &leaf).unwrap();
        assert!(fake.error().contains("auth.json"));
        assert!(forget_with(&fake).is_err());
        assert!(leaf.is_symlink());
        fs::remove_file(&leaf).unwrap();
        fs::create_dir(&leaf).unwrap();
        assert!(fake.error().contains("not a regular file"));
        assert!(forget_with(&fake).is_err());
        assert!(leaf.is_dir());
        fs::remove_dir(&leaf).unwrap();
        fs::hard_link(&target, &leaf).unwrap();
        assert!(fake.error().contains("hard link"));
        assert_eq!(mode(&target), 0o644);
        fs::remove_file(&leaf).unwrap();
        fs::write(&leaf, b"owned").unwrap();
        fs::set_permissions(&leaf, fs::Permissions::from_mode(0o644)).unwrap();
        let profile = fake.open_managed(&["rescue", PROFILE], false);
        let different_uid = rustix::process::geteuid().as_raw() ^ 1;
        assert!(profile
            .check_auth_as(different_uid)
            .unwrap_err()
            .to_string()
            .contains("not owned"));
        assert!(profile
            .forget_as(different_uid)
            .unwrap_err()
            .to_string()
            .contains("not owned"));
        assert_eq!(mode(&leaf), 0o644);
        assert_eq!(fs::read(leaf).unwrap(), b"owned");
        assert!(fake.children.lock().unwrap().is_empty());
        assert_eq!(fs::read(target).unwrap(), b"unchanged");
    }

    #[test]
    fn owned_auth_is_tightened_and_managed_dirs_and_report_are_private() {
        let fake = Fake::new();
        fs::create_dir_all(fake.profile()).unwrap();
        fs::create_dir_all(fake.tools("npm-cache/_logs")).unwrap();
        let auth = fake.profile().join(AUTH);
        fs::write(&auth, b"credential").unwrap();
        fs::set_permissions(&auth, fs::Permissions::from_mode(0o644)).unwrap();
        let run = prepare_with(&fake).unwrap();
        for path in [
            fake.tools(""),
            fake.tools("node"),
            fake.tools("pi"),
            fake.tools("npm-cache"),
            fake.tools("npm-cache/_logs"),
            fake.root.join("rescue"),
            fake.profile(),
            fake.sessions(),
            run.session_dir().into(),
        ] {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(mode(&path), 0o700, "{}", path.display());
            assert_eq!(
                fs::metadata(path).unwrap().uid(),
                rustix::process::geteuid().as_raw()
            );
        }
        assert_eq!(mode(&auth), 0o600);
        run.write_session_file("report.json", b"report").unwrap();
        assert_eq!(mode(&run.session_dir().join("report.json")), 0o600);
        let tools = fake.open_managed(&["tools"], false);
        let cache = fake.open_managed(&["tools", "npm-cache"], false);
        let foreign = rustix::process::geteuid().as_raw() ^ 1;
        assert!(tools
            .verify_child_owner_as(OsStr::new("npm-cache"), foreign)
            .is_err());
        assert!(cache
            .verify_child_owner_as(OsStr::new("_logs"), foreign)
            .is_err());
    }

    #[test]
    fn package_bin_cannot_escape_pi_install_through_a_link() {
        let mut fake = Fake::new().seeded();
        fake.path_node("v22.19.0", true);
        let package = fake.pi().join("node_modules").join(PI_PACKAGE);
        let target = fake.root.with_file_name("outside-script");
        fs::write(&target, b"outside").unwrap();
        fs::remove_file(package.join(BIN)).unwrap();
        symlink(&target, package.join(BIN)).unwrap();
        prepare_with(&fake).unwrap();
        assert_eq!(fs::read(target).unwrap(), b"outside");
        assert!(!package.join(BIN).is_symlink());
        assert!(fake
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|event| event == "npm ci"));
    }

    #[tokio::test]
    async fn bounded_probe_works_from_an_async_caller_and_enforces_timeout() {
        let fake = Fake::new();
        let cwd = fake.root.parent().unwrap();
        let env = vec![("PATH".into(), "/usr/bin:/bin".into())];
        let shell = |body: &str, timeout| Child {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), body.into()],
            cwd: cwd.into(),
            env: env.clone(),
            timeout: Some(timeout),
            io: Io::Capture,
        };
        let good = System
            .run(shell("printf probe", Duration::from_secs(2)))
            .unwrap();
        assert_eq!(good.stdout, b"probe");
        let error = System
            .run(shell("exec sleep 10", Duration::from_millis(60)))
            .err()
            .unwrap();
        assert!(error.contains("timed out"));
    }
}
